pub mod fs_event_stream;
pub mod immediate;
mod kqueue;

use std::{
    panic,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use futures::{channel::mpsc, StreamExt};
use tracing::{debug, instrument, trace, trace_span, warn};

use crate::{
    options::MacosOptions,
    util::{result_util::ResultExt, Debug},
};

use super::{
    registry::Watches, Audience, BackendError, BackendEvent, BackendEventContext, BackendEventHandler, ParentPolicy, Scope,
    SubscriptionId, WatcherBackend,
};
use fs_event_stream::{fs_event_stream, FSEventStream, FSEventStreamEvent, FsEventStreamError, FsEventStreamFlags};

pub struct FSEventStreamBackend {
    thread_handle: thread::JoinHandle<()>,
    shared: Arc<Shared>,
    fs_event_stream: FSEventStream,
}

impl FSEventStreamBackend {
    pub fn create(options: MacosOptions, callback: BackendEventHandler) -> Result<Self, FsEventStreamError> {
        let (events_tx, events_rx) = mpsc::unbounded::<thread::Result<Vec<FSEventStreamEvent>>>();
        let MacosOptions {
            fs_event_stream_latency,
            report_rescan,
            muted_paths,
        } = options;
        let fs_event_stream = fs_event_stream(
            &[PathBuf::from("/")],
            &[],
            fs_event_stream_latency,
            FsEventStreamFlags::default(),
            move |events| {
                let _ = events_tx.unbounded_send(events);
            },
        )?;
        let shared_impl = Arc::new(Shared {
            watches: Mutex::new(Watches::default()),
        });
        let thread_handle = {
            let shared_impl = shared_impl.clone();
            thread::Builder::new()
                .name(String::from("fsevents-event-loop"))
                .spawn(|| {
                    futures::executor::block_on(event_loop(
                        callback,
                        report_rescan.map(Debug::into_inner),
                        muted_paths,
                        shared_impl,
                        events_rx,
                    ))
                })
                .expect("failed to spawn a thread")
        };

        Ok(FSEventStreamBackend {
            thread_handle,
            shared: shared_impl,
            fs_event_stream,
        })
    }
}

struct Shared {
    watches: Mutex<Watches<(), ()>>,
}

impl WatcherBackend for Shared {
    #[instrument(level = "trace", skip(self))]
    fn add_watch(
        &self,
        canonical_path: &Path,
        subscription: SubscriptionId,
        scope: Scope,
        parent_policy: ParentPolicy,
    ) -> Result<(), BackendError> {
        let mut w = self.watches.lock().expect("mutex is poisoned");
        w.check_parent_policy(canonical_path, subscription, parent_policy)?;
        let tree = match scope {
            Scope::DirectChildren => &mut w.direct,
            Scope::Recursive => &mut w.recursive,
        };
        let mut entry = tree.entry(canonical_path).expect("illegal argument").or_insert_default();
        entry.subscribe(subscription);
        trace!("watch_added");

        Ok(())
    }

    #[instrument(level = "trace", skip(self))]
    fn remove_watch(&self, canonical_path: &Path, subscription: SubscriptionId, scope: Scope) {
        if let Ok(mut watches) = self.watches.lock() {
            let recursive = match scope {
                Scope::DirectChildren => false,
                Scope::Recursive => true,
            };
            let _ = watches.remove_subscriber(canonical_path, subscription, recursive, |_, _| {}, |_, _| {});
        }
    }

    #[instrument(level = "trace", skip(self))]
    fn destroy_subscription(&self, subscription: SubscriptionId) {
        if let Ok(mut guard) = self.watches.lock() {
            guard.destroy_subscription(subscription);
        }
    }

    fn shutdown_and_join(self: Box<Self>) -> std::thread::Result<()> {
        unreachable!()
    }
}

impl WatcherBackend for FSEventStreamBackend {
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

    fn shutdown_and_join(self: Box<Self>) -> std::thread::Result<()> {
        drop(self.fs_event_stream);
        self.thread_handle.join()
    }
}

async fn event_loop(
    mut callback: BackendEventHandler,
    mut report_rescan: Option<Box<dyn FnMut() + Send>>,
    muted_paths: Vec<PathBuf>,
    shared_impl: Arc<Shared>,
    mut fs_event_stream_events: mpsc::UnboundedReceiver<thread::Result<Vec<FSEventStreamEvent>>>,
) {
    while let Some(events) = fs_event_stream_events.next().await {
        match events {
            Ok(events) => {
                trace!("received events batch, events: {events:#?}");
                // TODO conflate

                for event in events {
                    let _span = trace_span!("event_loop", event = ?event).entered();
                    let mut watches = shared_impl.watches.lock().expect("mutex is poisoned");

                    let subscriptions = if muted_paths.iter().any(|ignored| event.path.starts_with(ignored)) {
                        None
                    } else {
                        watches.query(&event.path).log(|err| warn!(?err, "query has failed")).ok()
                    }
                    .unwrap_or_default();

                    if let Some(translated) = fs_event_stream::event(&event.path, event.flags, subscriptions.is_empty()) {
                        if matches!(translated, BackendEvent::Overflow) {
                            if let Some(f) = report_rescan.as_mut() {
                                f()
                            }
                        }
                        if matches!(translated, BackendEvent::Removed | BackendEvent::Overflow | BackendEvent::Ambiguous) {
                            watches.remove_path(&event.path);
                        }
                        if !subscriptions.is_empty() {
                            // don't forget to release the lock
                            drop(watches);
                            trace!(?translated, "delievering translated event");
                            let audience = Audience::Some(subscriptions);
                            let ctx = BackendEventContext {
                                audience: &audience,
                                event: translated,
                                event_path: event.path.as_path(),
                                backend: shared_impl.as_ref(),
                            };
                            callback(ctx)
                        }
                    }
                }
            }
            Err(panic) => panic::resume_unwind(panic),
        }
    }
    debug!("FsEventStream is shut down, terminating event loop");
}
