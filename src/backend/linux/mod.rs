use std::{
    collections::HashSet,
    fs, io,
    ops::DerefMut,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use ::inotify::{EventMask, EventOwned, Inotify, WatchDescriptor, WatchMask};
use futures::StreamExt;
use tracing::{debug, info, instrument, trace, trace_span, warn};

use super::{
    registry::{RecordId, SubscriptionId, Watches},
    Audience, BackendError, BackendEvent, BackendEventContext, BackendEventHandler, FileType, ParentPolicy, Scope, WatcherBackend,
};
use crate::{
    options::{LinuxOptions, UlimitStrategy},
    util::{
        lifetimes::{Lifetime, LifetimeDefinition},
        result_util::ResultExt,
    },
};
use crate::util::multi_map::MultiMap;

/// See "Reading events from an inotify file descriptor" at
/// https://man7.org/linux/man-pages/man7/inotify.7.html
const SIZE_OF_STRUCT: usize = std::mem::size_of::<std::os::raw::c_int>() + std::mem::size_of::<u32>() * 3;

// TODO do not hardcode, query pathconf instead?
const NAME_MAX: usize = 255;

const INOTIFY_EVENT_SIZE: usize = SIZE_OF_STRUCT + NAME_MAX + 1;

const MASK: WatchMask = WatchMask::ATTRIB
    .union(WatchMask::CREATE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MODIFY)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::EXCL_UNLINK)
    .union(WatchMask::ONLYDIR)
    .union(WatchMask::DONT_FOLLOW);

pub struct INotifyBackend {
    shared: Arc<Shared>,
    thread_handle: thread::JoinHandle<()>,
    lifetime_def: LifetimeDefinition,
}

impl INotifyBackend {
    pub fn create(options: LinuxOptions, callback_fn: BackendEventHandler) -> io::Result<Self> {
        let LinuxOptions {
            ulimit_strategy,
            muted_paths: ignored_paths,
        } = options;
        let lifetime_def = LifetimeDefinition::new();
        let inotify = Inotify::init()?;
        let watches = inotify.watches();
        let descriptors = Descriptors::new(watches);
        let shared = Arc::new(Shared {
            descriptors: Mutex::new(descriptors),
        });
        let thread_handle = {
            let lifetime = lifetime_def.lifetime();
            let shared = shared.clone();
            thread::Builder::new()
                .name(String::from("inotify-event-loop"))
                .spawn(|| {
                    // Inotify::into_event_stream requires a tokio runtime
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to instantiate tokio runtime");
                    rt.block_on(event_loop(
                        inotify,
                        callback_fn,
                        shared,
                        lifetime,
                        ignored_paths,
                        ulimit_strategy.into_inner(),
                    ))
                })
                .expect("failed to spawn a thread")
        };

        Ok(Self {
            shared,
            thread_handle,
            lifetime_def,
        })
    }
}

impl WatcherBackend for INotifyBackend {
    fn add_watch(
        &self,
        canonical_path: &Path,
        subscription: SubscriptionId,
        scope: Scope,
        parent_policy: ParentPolicy,
    ) -> Result<(), BackendError> {
        self.shared.add_watch(canonical_path, subscription, scope, parent_policy)
    }

    fn remove_watch(&self, canonical_path: &Path, subscription: SubscriptionId, scope: Scope) {
        self.shared.remove_watch(canonical_path, subscription, scope);
    }

    fn destroy_subscription(&self, subscription: SubscriptionId) {
        self.shared.destroy_subscription(subscription);
    }

    fn shutdown_and_join(self: Box<Self>) -> thread::Result<()> {
        self.lifetime_def.terminate();
        self.thread_handle.join()
    }
}

struct Shared {
    descriptors: Mutex<Descriptors>,
}

impl Shared {
    #[instrument(level = "trace", skip_all, fields(?canonical_path, ?subscriptions))]
    fn watch_recursively_impl(
        &self,
        canonical_path: &Path,
        subscriptions: &[SubscriptionId],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), BackendError> {
        fn recur(this: &Shared, canonical_path: &Path, subscriptions: &[SubscriptionId], cancelled: &dyn Fn() -> bool) -> Result<(), BackendError> {
            if cancelled() {
                return Ok(());
            }
            {
                let mut descriptors = this.descriptors.lock().expect("mutex is poisoned");
                descriptors.watch_one_impl(canonical_path, subscriptions)?;
            }
            for child in fs::read_dir(canonical_path).and_then(|iter| iter.collect::<io::Result<Vec<_>>>())? {
                let child_path = child.path();

                // This never fails and doesn't do syscalls on supported OSes.
                if !child
                    .file_type()
                    .expect("child entry had to perform IO to fetch file type and it failed")
                    .is_dir()
                {
                    // On other platforms, we do not set up redirects or watches for symlinks under the given directory,
                    // so we do not do this here either.
                    continue;
                }
                recur(this, &child_path, subscriptions, cancelled)?;
            }
            Ok(())
        }
        recur(self, canonical_path, subscriptions, cancelled)
    }

    fn destroy_subscription(&self, subscription: SubscriptionId) {
        let mut descriptors = self.descriptors.lock().expect("mutex is poisoned");
        let Descriptors {
            descriptors,
            watches,
            inotify,
        } = descriptors.deref_mut();
        watches.recursive.destroy_subscription(subscription, |_, _| {});
        watches.direct.destroy_subscription(subscription, |record_id, wd| {
            if descriptors.remove(&wd, &record_id) {
                let _ = inotify.remove(wd).log(|err| warn!(?err, "error while removing watch"));
            }
        });
    }
}

impl WatcherBackend for Shared {
    fn add_watch(
        &self,
        canonical_path: &Path,
        subscription: SubscriptionId,
        scope: Scope,
        parent_policy: ParentPolicy,
    ) -> Result<(), BackendError> {
        match scope {
            Scope::DirectChildren => {
                let mut descriptors = self.descriptors.lock().expect("mutex is poisoned");
                descriptors
                    .watches
                    .check_parent_policy(canonical_path, subscription, parent_policy)?;
                descriptors.watch_one_impl(canonical_path, &[subscription])
            }
            Scope::Recursive => {
                let mut descriptors = self.descriptors.lock().expect("mutex is poisoned");
                descriptors
                    .watches
                    .check_parent_policy(canonical_path, subscription, parent_policy)?;
                descriptors
                    .watches
                    .recursive
                    .entry(canonical_path)
                    .unwrap() // TODO propagate
                    .or_insert_default()
                    .subscribe(subscription);
                drop(descriptors);
                // TODO no cancellation on recursive watches
                self.watch_recursively_impl(canonical_path, &[subscription], &|| false)
            }
        }
    }

    fn remove_watch(&self, canonical_path: &Path, subscription: SubscriptionId, scope: Scope) {
        let mut descriptors = self.descriptors.lock().expect("mutex is poisoned");
        let Descriptors {
            descriptors: watched_descriptors,
            watches,
            inotify,
        } = descriptors.deref_mut();
        let recursive = match scope {
            Scope::DirectChildren => false,
            Scope::Recursive => true,
        };
        let _ = watches.remove_subscriber(
            canonical_path,
            subscription,
            recursive,
            |record_id, wd| {
                if watched_descriptors.remove(&wd, &record_id) {
                    let _ = inotify.remove(wd).log(|err| warn!(?err, "error while removing watch"));
                }
            },
            |_, _| {},
        );
    }

    fn destroy_subscription(&self, subscription: SubscriptionId) {
        self.destroy_subscription(subscription);
    }

    fn shutdown_and_join(self: Box<Self>) -> thread::Result<()> {
        unreachable!()
    }
}

#[instrument(level = "trace", skip_all)]
async fn event_loop(
    inotify: Inotify,
    mut callback: BackendEventHandler,
    shared: Arc<Shared>,
    lifetime: Lifetime,
    ignored_paths: Vec<PathBuf>,
    _ulimit_handler: UlimitStrategy,
) {
    enum LoopEvent {
        Events(Vec<EventOwned>),
        Shutdown,
    }
    const CHUNK_SIZE: usize = 4096;

    let mut buffer = vec![0u8; (INOTIFY_EVENT_SIZE) * CHUNK_SIZE].into_boxed_slice();
    debug!(buffer.len = buffer.len(), "allocated buffer for inotify");
    let mut events = inotify
        .into_event_stream(&mut buffer)
        .expect("failed to get event_stream from inotify")
        .ready_chunks(CHUNK_SIZE)
        .fuse();

    loop {
        let next = tokio::select! {
            _ = lifetime.terminated() => LoopEvent::Shutdown,
            events = events.next() => {
                // `into_event_stream` claims it is "an infinite source of events."
                let evts: Vec<io::Result<EventOwned>> = events.expect("inotify stream suddenly closed");
                // The man page states that read returns only EINTR or EINVAL.
                // EINVAL should never happen if the buffer is large enough,
                // and EINTR should never happen without signal handlers.
                LoopEvent::Events(evts.into_iter().map(|evt| evt.expect("failed to read event from inotify")).collect())
            }
        };
        match next {
            LoopEvent::Events(raw_events) => {
                trace!("Received raw events batch: {raw_events:#?}");
                for raw in raw_events {
                    if raw.mask.contains(EventMask::Q_OVERFLOW) {
                        let mut descriptors = shared.descriptors.lock().expect("mutex is poisoned");
                        warn!("Received Q_OVERFLOW, will remove all watches and broadcast BackendEvent::Overflow");
                        descriptors.clear();
                        drop(descriptors);
                        let audience = Audience::All;
                        let event_path = PathBuf::from("/");
                        let ctx = BackendEventContext {
                            audience: &audience,
                            event: BackendEvent::Overflow,
                            event_path: event_path.as_path(),
                            backend: shared.as_ref(),
                        };
                        callback(ctx);
                        break; // no reason to process subsequent events
                    }
                    let event_paths = {
                        let mut descriptors = shared.descriptors.lock().expect("mutex is poisoned");
                        descriptors.restore_path(&raw.wd)
                    };
                    if event_paths.is_empty() {
                        trace!(watch_descriptor = ?raw.wd, "Unknown watch descriptor");
                    }
                    for mut event_path in event_paths {
                        if let Some(name) = raw.name.as_ref() {
                            event_path.push(name);
                        }

                        let _span = trace_span!("event", ?event_path, ?raw).entered();
                        if ignored_paths.iter().any(|ignored| event_path.starts_with(ignored)) {
                            trace!("skip ignored path");
                            continue;
                        }
                        let is_dir = raw.mask.contains(EventMask::ISDIR);
                        let created = raw.mask.contains(EventMask::CREATE) || raw.mask.contains(EventMask::MOVED_TO);
                        let modified = raw.mask.contains(EventMask::MODIFY) || raw.mask.contains(EventMask::ATTRIB);
                        let deleted = raw.mask.contains(EventMask::DELETE)
                            || raw.mask.contains(EventMask::MOVED_FROM)
                            || raw.mask.contains(EventMask::IGNORED);
                        let ambigious = modified && deleted || created && deleted || created && modified;
                        if ambigious {
                            warn!(?event_path, ?raw.mask, "event is ambigious")
                        }

                        let subscriptions = {
                            let mut descriptors = shared.descriptors.lock().expect("mutex is poisoned");
                            let subscriptions = descriptors
                                .watches
                                .query(&event_path)
                                .log(|err| warn!(?err, "query has failed"))
                                .unwrap_or_default();
                            if is_dir && deleted {
                                descriptors.remove_recursively(&event_path);
                            }
                            if is_dir && created {
                                let mut recursive_audience = HashSet::new();
                                descriptors
                                    .watches
                                    .recursive
                                    .entry(&event_path)
                                    .expect("illegal argument")
                                    .ancestors_audience(|s| {
                                        recursive_audience.insert(s);
                                    });
                                drop(descriptors);
                                if !recursive_audience.is_empty() {
                                    let audience_vec = recursive_audience.into_iter().collect::<Vec<_>>();
                                    trace!(?event_path, ?audience_vec, "watching recursively");
                                    let _ = shared.watch_recursively_impl(&event_path, &audience_vec, &|| lifetime.is_terminated());
                                }
                                {
                                    trace!(?event_path, "path doesn't have recursive subscritions");
                                }
                            }
                            subscriptions
                        };

                        let event = if modified {
                            Some(BackendEvent::Changed {
                                file_type: file_type(&event_path, is_dir),
                            })
                        } else if created {
                            Some(BackendEvent::RecentlyCreated {
                                file_type: file_type(&event_path, is_dir),
                            })
                        } else if deleted {
                            Some(BackendEvent::Removed)
                        } else {
                            warn!(?raw.mask, ?event_path, "don't know how to handle event mask");
                            None
                        };
                        if let Some(event) = event {
                            let audience = Audience::Some(subscriptions);
                            let ctx = BackendEventContext {
                                audience: &audience,
                                event,
                                event_path: event_path.as_path(),
                                backend: shared.as_ref(),
                            };
                            callback(ctx);
                        }
                    }
                }
            }
            LoopEvent::Shutdown => {
                info!("Received shutdown request, breaking event loop");
                break;
            }
        }
    }
}

struct Descriptors {
    // Multiple paths may refer to the same inode, but this is a rare occasion.
    // Such descriptors are stored in a separate map to avoid storing a hash set for each entry.
    descriptors: MultiMap<WatchDescriptor, RecordId>,
    watches: Watches<WatchDescriptor, ()>,
    inotify: ::inotify::Watches,
}

impl Descriptors {
    pub fn new(inotify_watches: ::inotify::Watches) -> Self {
        Descriptors {
            inotify: inotify_watches,
            descriptors: MultiMap::default(),
            watches: Watches::default(),
        }
    }

    fn watch_one_impl(&mut self, canonical_path: &Path, subscriptions: &[SubscriptionId]) -> Result<(), BackendError> {
        trace!(?canonical_path, ?subscriptions, "watch_one_impl");

        assert!(!subscriptions.is_empty());

        let mut entry = self
            .watches
            .direct
            .entry(canonical_path)
            .unwrap() // todo
            .or_insert_all_with::<BackendError>(
                |path| Ok(self.inotify.add(path, MASK)?),
                |record_id, wd| {
                    self.descriptors.insert(wd.clone(), record_id);
                },
            )?;
        for subscription in subscriptions {
            entry.subscribe(*subscription);
        }
        Ok(())
    }

    // TODO mut self because entries on watch_tree are mutable
    pub fn restore_path<'d>(&'d mut self, wd: &WatchDescriptor) -> Vec<PathBuf> {
        let Self { descriptors, watches, .. } = self;
        descriptors
            .get(wd)
            .map(|record_id| {
                let entry = watches.direct.unsafe_entry(*record_id);
                assert!(entry.value().eq(wd));
                entry.collect_path()
            })
            .collect()
    }

    #[instrument(skip(self))]
    pub fn remove_recursively(&mut self, path: &Path) {
        self.watches.recursive.remove_path(path, |_, _| {});
        self.watches.direct.remove_path(path, |record_id, wd| {
            if self.descriptors.remove(&wd, &record_id) {
                let _ = self.inotify.remove(wd).log(|err| warn!(?err, "error on inotify.remove"));
            }
        });
    }

    pub fn clear(&mut self) {
        self.descriptors.clear();
        self.watches.recursive.clear(|_, _| {});
        self.watches.direct.clear(|_, wd| {
            let _ = self.inotify.remove(wd).log(|err| warn!(?err, "error on inotify.remove"));
        });
    }
}

fn file_type(path: &Path, is_dir: bool) -> FileType {
    // inotify's event mask doesn't contain the information about whether the event target is a symlink,
    // so we have to request this explicitly. If an error during lstat occurs or if the file type isn't
    // a symlink, a directory or a regular file, we treat it as regular (not sure it's the proper thing to do).
    if is_dir {
        FileType::Directory
    } else {
        // There is a race here. If we process an event that happened to a file that was at this path before,
        // but a new file is now located at this position, we get a divergence in the results.
        match fs::symlink_metadata(path) {
            Ok(m) if m.is_symlink() => FileType::Symlink,
            Ok(m) if m.is_file() => FileType::Regular,
            // This check is here for completeness, but we already know that for this event it wasn't a directory.
            Ok(m) if m.is_dir() => FileType::Regular,
            Ok(m) => {
                trace!(metadata = ?m, path = %path.display(), "unsupported file type");
                FileType::Regular
            }
            Err(e) => {
                // This most commonly happens if the file was deleted.
                trace!(error = ?e, path = %path.display(), "failed to get the event path metadata");
                FileType::Regular
            }
        }
    }
}
