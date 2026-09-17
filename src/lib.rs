pub mod backend;
mod canonicalization;
mod dispatch;
mod event;
mod event_stream;
pub mod options;
mod session;
mod symbolic_tree;
mod util;
mod watch_error;

#[cfg(test)]
mod test_helpers;
#[cfg(test)]
mod tests;

use anyhow::Result;
use futures::{stream::BoxStream, StreamExt};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing::{info, trace};

use crate::options::WatcherOptions;

use self::{
    backend::immediate::{immediate_watcher_impl, ImmediateWatcher},
    backend::{BackendEventHandler, SubscriptionId, WatcherBackend},
    dispatch::Dispatch,
    event_stream::{event_stream, immediate_stream},
    session::{session, SessionId},
    util::id_source::IdSource,
};

pub use {
    backend::{FileType, Scope},
    event::Event,
    event_stream::EventStream,
    session::{ClientOverflow, WatchSession, WatchSessionRx, WatchSessionTx},
    watch_error::WatchError,
};

pub mod prelude {
    pub use super::{Event, Watcher, WatcherApi};
}

/// Holds all the resources shared across watch sessions.
pub struct Watcher {
    client_buffer_size: usize,
    id_source: IdSource,
    dispatch: Arc<Mutex<Dispatch>>,
    backend: Box<dyn WatcherBackend + Sync + Send>,
    immediate: Option<Box<dyn ImmediateWatcher + Sync + Send>>,
}

pub trait WatcherApi<'a> {
    /// For a client that needs precise control over the set of watched files.
    fn session(self) -> WatchSession<'a>;

    /// Shortcut for a client that is interested in a single file system entry, be it a file or a directory.
    ///
    /// Reports all changes in ancestors including changes in symlinks.
    /// Will succeed even if the path does not exist.
    fn watch_one(self, path: impl Into<PathBuf>) -> EventStream<'a>;

    /// Same as [WatcherApi::watch_one] but subscribes to all children recursively.
    ///
    /// Won't follow nested symlinks. If this is a requirement, one has to use [WatcherApi::session].
    fn watch_recursively(self, path: impl Into<PathBuf>) -> EventStream<'a>;

    /// See [ImmediateWatcher].
    fn watch_immediate(self, path: impl Into<PathBuf>) -> EventStream<'a>;
}

impl<'a> WatcherApi<'a> for &'a Watcher {
    fn session(self) -> WatchSession<'a> {
        session(self)
    }

    fn watch_one(self, path: impl Into<PathBuf>) -> EventStream<'a> {
        event_stream(self, path.into(), Scope::DirectChildren)
    }

    fn watch_recursively(self, path: impl Into<PathBuf>) -> EventStream<'a> {
        event_stream(self, path.into(), Scope::Recursive)
    }

    fn watch_immediate(self, path: impl Into<PathBuf>) -> EventStream<'a> {
        immediate_stream(self, path.into())
    }
}

impl WatcherApi<'static> for Arc<Watcher> {
    fn session(self) -> WatchSession<'static> {
        session(self)
    }

    fn watch_one(self, path: impl Into<PathBuf>) -> EventStream<'static> {
        event_stream(self, path.into(), Scope::DirectChildren)
    }

    fn watch_recursively(self, path: impl Into<PathBuf>) -> EventStream<'static> {
        event_stream(self, path.into(), Scope::Recursive)
    }

    fn watch_immediate(self, path: impl Into<PathBuf>) -> EventStream<'static> {
        immediate_stream(self, path.into())
    }
}

impl Watcher {
    pub fn create_default() -> anyhow::Result<Self> {
        Watcher::create(WatcherOptions::default())
    }

    pub fn create(options: impl Into<WatcherOptions>) -> anyhow::Result<Watcher> {
        let options = options.into();
        Watcher::create_by(options.client_buffer_size, |handler| {
            backend::default_backend(options.backend, handler)
        })
    }

    pub fn create_by<B>(
        client_buffer_size: usize,
        backend_factory: impl FnOnce(BackendEventHandler) -> anyhow::Result<B>,
    ) -> anyhow::Result<Self>
    where
        B: WatcherBackend + Sync + Send + 'static,
    {
        let t0 = std::time::Instant::now();
        let dispatch = Arc::new(Mutex::new(Dispatch::default()));
        let backend = backend_factory({
            let dispatch = dispatch.clone();
            let event_id_source = IdSource::new();
            Box::new(move |ctx| {
                let event_id = event_id_source.next();
                trace!(?event_id,event = ?ctx.event, event_path = ?ctx.event_path, audience = ?ctx.audience, "dispatching event");
                dispatch.lock().expect("mutex is poisoned").dispatch_event(ctx);
            })
        })?;
        let immediate = immediate_watcher_impl()?;
        let id_source = IdSource::new();
        info!(elapsed = ?t0.elapsed(), "Watcher instantiated");
        Ok(Watcher {
            immediate,
            backend: Box::new(backend),
            id_source,
            dispatch,
            client_buffer_size,
        })
    }

    fn register_session(&self, handler: dispatch::SessionHandler) -> SessionId {
        let session_id = SessionId(self.id_source.next());
        self.dispatch
            .lock()
            .expect("mutex is poisoned")
            .register_session(session_id, handler);
        session_id
    }

    fn register_subscription(&self, session_id: SessionId, subscription_id: SubscriptionId) {
        self.dispatch
            .lock()
            .expect("mutex is poisoned")
            .register_subscription(subscription_id, session_id);
    }

    fn destroy_subscription(&self, subscription_id: SubscriptionId) {
        trace!("destroy subscription {subscription_id:?}");
        self.backend.destroy_subscription(subscription_id);
        self.dispatch
            .lock()
            .expect("mutex is poisoned")
            .unregister_subscription(subscription_id);
    }

    pub fn shutdown_and_join(self) -> anyhow::Result<()> {
        fn panic_anyhow(err: Box<dyn std::any::Any + Send + 'static>) -> anyhow::Error {
            let error = if let Some(error) = err.downcast_ref::<String>() {
                error.clone()
            } else if let Some(error) = err.downcast_ref::<&'static str>() {
                error.to_string()
            } else {
                format!("{err:?}")
            };
            anyhow::anyhow!("backend event loop joined with an error: {error:?}")
        }

        let backend_result = self.backend.shutdown_and_join().map_err(panic_anyhow);
        let immediate_result = self
            .immediate
            .map(|immediate| immediate.shutdown_and_join().map_err(panic_anyhow))
            .unwrap_or(Ok(()));

        backend_result.and(immediate_result)
    }

    #[cfg(test)]
    pub fn debug(&self) -> WatcherDebug {
        WatcherDebug {
            dispatch: self.dispatch.lock().expect("mutex is poisoned").debug(),
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub struct WatcherDebug {
    pub dispatch: dispatch::DispatchDebug,
}

pub async fn watch_all(
    watcher: &Arc<Watcher>,
    paths: impl IntoIterator<Item = impl Into<PathBuf>>,
) -> Result<BoxStream<'static, Event>, WatchError> {
    let subscriptions = paths.into_iter().map(|path| watcher.clone().watch_one(path)).collect::<Vec<_>>();
    Ok(futures::stream::select_all(subscriptions).boxed())
}
