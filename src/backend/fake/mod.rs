pub mod timeout_iterator;

use std::{path::Path, sync::Mutex};

use tracing::{debug, instrument};

use super::{
    registry::{SubscriptionId, Watches},
    Audience, BackendError, BackendEvent, BackendEventContext, BackendEventHandler, FileType, ParentPolicy, Scope, WatcherBackend,
};
use crate::util::path_util::PathExt;

pub struct FakeWatcherBackend {
    callback: Mutex<Option<BackendEventHandler>>,
    watches: Mutex<Watches<(), ()>>,
}

impl FakeWatcherBackend {
    pub fn new() -> Self {
        FakeWatcherBackend {
            callback: Mutex::new(None),
            watches: Mutex::new(Watches::default()),
        }
    }

    pub fn inspect_tree(&self, f: impl FnOnce(&mut Watches<(), ()>)) {
        f(&mut self.watches.lock().expect("mutex is poisoned"))
    }

    pub fn set_handler(&self, handler: BackendEventHandler) {
        self.callback
            .lock()
            .expect("mutex is poisoned")
            .replace(handler)
            .inspect(|_| panic!("replaced existing handler"));
    }

    #[instrument(skip_all, fields(path = ?path.as_ref(), ?event))]
    pub fn emulate_event(&self, path: impl AsRef<Path>, event: BackendEvent) {
        let path = path.as_ref();
        assert!(!path.components().is_empty_path(), "path is empty: {path:?}");

        // Real watchers operate with canonical paths, so all events are emitted in canonical form.
        let canonical_path = path
            .parent()
            .map(|prefix| {
                prefix
                    .canonicalize()
                    .unwrap_or_else(|_| panic!("failed to canonicalize file {prefix:?}"))
            })
            .map(|canonical_prefix| canonical_prefix.join(path.file_name().expect("path has no name of its own")))
            .unwrap_or_else(|| path.to_path_buf());
        let subscriptions = self
            .watches
            .lock()
            .expect("mutex is poisoned")
            .query(&canonical_path)
            .expect("illegal argument");
        // same behaviour as in real implementations
        if matches!(event, BackendEvent::Removed | BackendEvent::Overflow | BackendEvent::Ambiguous) {
            self.watches.lock().expect("mutex is poisoned").remove_path(&canonical_path);
        }
        let audience = Audience::Some(subscriptions);
        debug!(?canonical_path, ?event, ?audience, "emulate_event");
        // should not hold any locks while invoking the handler
        let ctx = BackendEventContext {
            event,
            event_path: canonical_path.as_path(),
            backend: self,
            audience: &audience,
        };
        let mut cb = self.callback.lock().expect("mutex is poisoned");
        cb.as_mut().unwrap()(ctx)
    }

    pub fn emulate_dir_created(&self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        self.emulate_event(
            path,
            BackendEvent::RecentlyCreated {
                file_type: FileType::Directory,
            },
        );
    }

    pub fn emulate_dir_changed(&self, path: impl AsRef<Path>) {
        self.emulate_event(
            path.as_ref(),
            BackendEvent::Changed {
                file_type: FileType::Directory,
            },
        );
    }

    pub fn emulate_removed(&self, path: impl AsRef<Path>) {
        self.emulate_event(path.as_ref(), BackendEvent::Removed);
    }

    pub fn emulate_file_changed(&self, path: impl AsRef<Path>) {
        self.emulate_event(
            path.as_ref(),
            BackendEvent::Changed {
                file_type: FileType::Regular,
            },
        );
    }

    pub fn emulate_symlink_changed(&self, path: impl AsRef<Path>) {
        self.emulate_event(
            path.as_ref(),
            BackendEvent::Changed {
                file_type: FileType::Symlink,
            },
        );
    }

    pub fn emulate_overflow(&self, path: impl AsRef<Path>) {
        self.emulate_event(path.as_ref(), BackendEvent::Overflow);
    }
}

impl<T> WatcherBackend for T
where
    T: AsRef<FakeWatcherBackend>,
{
    fn add_watch(
        &self,
        canonical_path: &Path,
        subscription: SubscriptionId,
        scope: Scope,
        parent_policy: ParentPolicy,
    ) -> Result<(), BackendError> {
        debug!(?canonical_path, ?subscription, ?parent_policy, "add_watch");
        assert!(canonical_path.is_absolute(), "path is not absolute: {canonical_path:?}");

        let mut w = self.as_ref().watches.lock().expect("mutex is poisoned");
        // Enforce the parent policy first exactly as the real backends do.
        w.check_parent_policy(canonical_path, subscription, parent_policy)?;

        // verify that the file exists
        let true_canonical = canonical_path.canonicalize()?;
        // A well-behaved client should never arrive here with a symbolic path.
        // It is unsafe to enforce this in production because file systems are inherently racy, but the tests are deterministic.
        assert!(true_canonical.eq(&canonical_path), "path is not canonical: {canonical_path:?}");

        if !true_canonical.is_dir() {
            return Err(BackendError::IO(std::io::Error::from(std::io::ErrorKind::NotADirectory)));
        }

        let tree = match scope {
            Scope::DirectChildren => &mut w.direct,
            Scope::Recursive => &mut w.recursive,
        };
        let mut entry = tree.entry(&true_canonical).expect("illegal argument").or_insert_default();
        entry.subscribe(subscription);
        Ok(())
    }

    fn remove_watch(&self, canonical_path: &Path, subscription: SubscriptionId, scope: Scope) {
        debug!(?canonical_path, ?subscription, "remove_watch");
        let mut w = self.as_ref().watches.lock().expect("mutex is poisoned");
        let recursive = match scope {
            Scope::DirectChildren => false,
            Scope::Recursive => true,
        };
        let _ = w.remove_subscriber(canonical_path, subscription, recursive, |_, _| {}, |_, _| {});
    }

    fn destroy_subscription(&self, subscription: SubscriptionId) {
        let mut w = self.as_ref().watches.lock().unwrap();
        w.destroy_subscription(subscription);
    }

    fn shutdown_and_join(self: Box<Self>) -> std::thread::Result<()> {
        Ok(())
    }
}

impl AsRef<FakeWatcherBackend> for FakeWatcherBackend {
    fn as_ref(&self) -> &FakeWatcherBackend {
        self
    }
}
