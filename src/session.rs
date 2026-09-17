use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    num::NonZeroU32,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tracing::{instrument, trace, trace_span, warn, Instrument};

use futures::StreamExt;

use crate::{
    backend::{Audience, BackendError, BackendEvent, BackendEventContext, ParentPolicy, Scope, SubscriptionId, WatcherBackend},
    canonicalization::{
        self, read_link, CanonicalizationError, CanonicalizationIO, CanonicalizationUpdate, MissingPolicy, ReadLink, Registered,
        SymbolicKey, WatchHandle,
    },
    symbolic_tree::SymbolicTree,
    util::{
        path_util::PathExt,
        result_util::ResultExt,
        resync_channel::{self, resync_channel},
    },
    Event, WatchError, Watcher,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub(super) NonZeroU32);

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ClientOverflow;

pub struct WatchSession<'a> {
    pub(crate) tx: WatchSessionTx<'a>,
    pub(crate) rx: WatchSessionRx<'a>,
}

pub struct WatchSessionTx<'a> {
    // used by event_stream
    pub(crate) inner: Arc<InnerSession<'a>>,
}

pub struct WatchSessionRx<'a> {
    events: resync_channel::Receive<SessionEvent>,
    pending: VecDeque<Event>,
    inner: Arc<InnerSession<'a>>,
}

#[derive(Debug)]
struct SessionEvent {
    audience: Audience,
    event: BackendEvent,
    event_path: PathBuf,
}

pub(crate) fn session<'a>(watcher: impl Deref<Target = Watcher> + Send + Sync + 'a) -> WatchSession<'a> {
    let (mut events_tx, events_rx) = resync_channel::<SessionEvent>(watcher.client_buffer_size);
    let canonicalization_subscription_id = SubscriptionId::from_non_zero_u32(watcher.id_source.next());
    let session_id = watcher.register_session({
        Box::new(move |ctx: BackendEventContext<'_>| {
            let session_event = SessionEvent {
                audience: ctx.audience.clone(),
                event: ctx.event,
                event_path: ctx.event_path.to_path_buf(),
            };
            let _ = events_tx.send(session_event);
        })
    });
    trace!("registered new session {session_id:?} with canonicalization id {canonicalization_subscription_id:?}");
    watcher.register_subscription(session_id, canonicalization_subscription_id);
    let inner = Arc::new(InnerSession {
        session_id,
        watcher: Box::new(watcher),
        state: Mutex::new(SessionState::new(canonicalization_subscription_id)),
    });
    let tx = WatchSessionTx { inner: inner.clone() };
    let rx = WatchSessionRx {
        events: events_rx,
        pending: VecDeque::new(),
        inner,
    };
    WatchSession { tx, rx }
}

impl<'a> Drop for WatchSessionTx<'a> {
    fn drop(&mut self) {
        // Destroying the session removes its event handler from dispatch, which drops the channel
        // sender (events_tx) and closes the underlying channel, causing WatchSessionRx::next_event
        // to return None.
        let mut dispatch = self.inner.watcher.dispatch.lock().expect("mutex is poisoned");
        dispatch.destroy_session(self.inner.session_id);
    }
}

impl<'a> WatchSessionTx<'a> {
    /// See [`WatchSession::watch_root`].
    #[instrument(level = "trace", skip_all, fields(path = %path.as_ref().display(), scope))]
    pub fn watch_root(&self, path: impl AsRef<Path>, scope: Scope) {
        trace!("watch_root");
        let path = path.as_ref();
        validate_path(path);
        let mut state = self.inner.state.lock().expect("mutex is poisoned");
        let subscription_id = SubscriptionId::from_non_zero_u32(self.inner.watcher.id_source.next());
        let Registered {
            key: root_key,
            canonical_path,
        } = {
            let SessionState {
                canonicalization_subscription_id,
                tracker,
                ..
            } = &mut *state;
            tracker
                .register(
                    path.to_path_buf(),
                    MissingPolicy::Track,
                    &mut real_io(self.inner.watcher.backend.as_ref(), *canonicalization_subscription_id),
                )
                .expect("watch_root tracks missing paths and always registers")
        };
        state.symbolic_paths.insert(root_key, (subscription_id, scope));
        state.symbolic_tree.insert(path, root_key);
        state.root_keys.insert(root_key);
        drop(state);

        self.inner.watcher.register_subscription(self.inner.session_id, subscription_id);

        // Subscribe to the canonical target automatically.
        if let Some(canonical) = canonical_path {
            self.inner
                .watcher
                .backend
                .add_watch(&canonical, subscription_id, scope, ParentPolicy::Unchecked)
                .ok()
                .or_else(|| {
                    // If the target is a file rather than a directory, watch the parent instead.
                    canonical.parent().and_then(|parent| {
                        self.inner
                            .watcher
                            .backend
                            .add_watch(parent, subscription_id, Scope::DirectChildren, ParentPolicy::Unchecked)
                            .ok()
                    })
                });
        }
    }

    /// See [`WatchSession::watch_directory`].
    #[instrument(level = "trace", skip_all, fields(path = %path.as_ref().display(), scope))]
    pub fn watch_directory(&self, path: impl AsRef<Path>, scope: Scope) -> Result<(), WatchError> {
        let path = path.as_ref();
        let _span = trace_span!("watch_directory", ?path).entered();
        validate_path(path);
        // An anchor speculatively created for a path that has no registered ancestor. Rolled back if
        // the backend rejects the watch, so a refused `watch_directory` leaves no trace.
        let mut new_anchor: Option<(SymbolicKey, SubscriptionId, PathBuf)> = None;
        let (subscription_id, canonical_path) = {
            let mut state = self.inner.state.lock().expect("mutex is poisoned");
            let subscription_id = match state.symbolic_tree.get_closest(path).copied() {
                Some(closest_key) => state
                    .symbolic_paths
                    .get(&closest_key)
                    .map(|&(id, _)| id)
                    .expect("inconsistent symbolic tree"),
                None => {
                    let prefix = path.prefixes().next().expect("absolute path has a root prefix");
                    let sub = SubscriptionId::from_non_zero_u32(self.inner.watcher.id_source.next());
                    let root_key = {
                        let SessionState {
                            canonicalization_subscription_id,
                            tracker,
                            ..
                        } = &mut *state;
                        tracker
                            .register(
                                prefix.to_path_buf(),
                                MissingPolicy::Track,
                                &mut real_io(self.inner.watcher.backend.as_ref(), *canonicalization_subscription_id),
                            )
                            .expect("watch_directory anchor is a path prefix and always registers")
                            .key
                    };
                    state.symbolic_paths.insert(root_key, (sub, scope));
                    state.symbolic_tree.insert(prefix, root_key);
                    new_anchor = Some((root_key, sub, prefix.to_path_buf()));
                    sub
                }
            };
            (subscription_id, state.tracker.canonicalization(path)?)
        };
        if let Some((_, sub, _)) = &new_anchor {
            self.inner.watcher.register_subscription(self.inner.session_id, *sub);
        }
        trace!(?canonical_path, "path canonicalized");
        match self
            .inner
            .watcher
            .backend
            .add_watch(&canonical_path, subscription_id, scope, ParentPolicy::RequireWatchedParent)
        {
            Ok(()) => Ok(()),
            Err(err) => {
                // The watch was refused (e.g. the parent is not part of the watched chain). Undo the
                // anchor we just created so nothing dangles disconnected from a real root.
                if let Some((anchor_key, sub, prefix)) = new_anchor {
                    self.rollback_anchor(anchor_key, sub, &prefix);
                }
                Err(err.into())
            }
        }
    }

    /// Removes an anchor speculatively registered by [`Self::watch_directory`] once the backend has
    /// refused the corresponding watch.
    fn rollback_anchor(&self, anchor_key: SymbolicKey, subscription_id: SubscriptionId, prefix: &Path) {
        {
            let mut state = self.inner.state.lock().expect("mutex is poisoned");
            state.symbolic_paths.remove(&anchor_key);
            state.root_keys.remove(&anchor_key);
            // Drop only the anchor's own value; siblings sharing the prefix node stay intact.
            state.symbolic_tree.prune(prefix, |_| true, |key| *key == anchor_key);
            let SessionState {
                canonicalization_subscription_id,
                tracker,
                ..
            } = &mut *state;
            tracker.unregister(
                anchor_key,
                &mut real_io(self.inner.watcher.backend.as_ref(), *canonicalization_subscription_id),
            );
        }
        let mut dispatch = self.inner.watcher.dispatch.lock().expect("mutex is poisoned");
        dispatch.unregister_subscription(subscription_id);
        drop(dispatch);
        self.inner.watcher.backend.destroy_subscription(subscription_id);
    }

    /// See [`WatchSession::watch_symlink`].
    pub fn watch_symlink(&self, symlink_path: impl Into<PathBuf>, scope: Scope) -> Result<(), WatchError> {
        let symlink_path = symlink_path.into();
        let _span = trace_span!("watch_symlink", ?symlink_path, ?scope).entered();
        validate_path(&symlink_path);
        let mut state = self.inner.state.lock().expect("mutex is poisoned");

        // Idempotency check: if we already track this exact path, skip
        if state.symbolic_tree.get_exact(&symlink_path).is_some() {
            return Ok(());
        }

        let registration = {
            let SessionState {
                canonicalization_subscription_id,
                tracker,
                ..
            } = &mut *state;
            tracker.register(
                symlink_path.clone(),
                MissingPolicy::Reject,
                &mut real_io(self.inner.watcher.backend.as_ref(), *canonicalization_subscription_id),
            )
        };
        let Some(Registered {
            key: sym_key,
            canonical_path,
        }) = registration
        else {
            // The path is not a symlink and does not exist. Unlike watch_root, watch_symlink does
            // not keep a pending subscription waiting for a non-existing link to appear.
            return Err(CanonicalizationError::PathDoesNotExist.into());
        };
        let subscription_id = SubscriptionId::from_non_zero_u32(self.inner.watcher.id_source.next());
        state.symbolic_paths.insert(sym_key, (subscription_id, scope));

        state.symbolic_tree.insert(&symlink_path, sym_key);
        drop(state);

        self.inner.watcher.register_subscription(self.inner.session_id, subscription_id);

        // Subscribe to the canonical target automatically.
        if let Some(canonical) = canonical_path {
            self.inner
                .watcher
                .backend
                .add_watch(&canonical, subscription_id, scope, ParentPolicy::Unchecked)
                .ok()
                .or_else(|| {
                    // If the target is a file rather than a directory, watch the parent instead.
                    canonical.parent().and_then(|parent| {
                        self.inner
                            .watcher
                            .backend
                            .add_watch(parent, subscription_id, Scope::DirectChildren, ParentPolicy::Unchecked)
                            .ok()
                    })
                });
        }

        Ok(())
    }

    /// See [`WatchSession::remove_watches`].
    #[instrument(level = "trace", skip_all, fields(path = %path.as_ref().display()))]
    pub fn remove_watches(&self, path: impl AsRef<Path>) {
        trace!("remove_watches");
        let path = path.as_ref();
        validate_path(path);

        let mut state = self.inner.state.lock().expect("mutex is poisoned");

        // Find the closest registered ancestor (or exact match) of `path`
        let effective_key = state.symbolic_tree.get_closest(path).copied();

        // Remove all entries at or under `path` from the symbolic tree
        let pruned_set = state
            .symbolic_tree
            .prune(path, |_| true, |_| true)
            .expect("descent is never stopped");

        // Collect subscriptions and clean up state for each pruned key
        let mut subscriptions_to_destroy = Vec::new();
        for &pruned_key in &pruned_set {
            state.root_keys.remove(&pruned_key);
            if let Some((subscription_id, _)) = state.symbolic_paths.remove(&pruned_key) {
                subscriptions_to_destroy.push(subscription_id);
            }
        }
        for &pruned_key in &pruned_set {
            let SessionState {
                canonicalization_subscription_id,
                tracker,
                ..
            } = &mut *state;
            tracker.unregister(
                pruned_key,
                &mut real_io(self.inner.watcher.backend.as_ref(), *canonicalization_subscription_id),
            );
        }

        // If the effective key survived pruning, explicitly remove backend watches under `path`
        if let Some(eff_key) = effective_key {
            if !pruned_set.contains(&eff_key) {
                let &(subscription_id, _) = state.symbolic_paths.get(&eff_key).expect("inconsistent state");
                if let Some(canonical_path) = state.tracker.canonicalization(path).ok() {
                    self.inner
                        .watcher
                        .backend
                        .remove_watch(&canonical_path, subscription_id, Scope::Recursive);
                }
            }
        }

        drop(state);

        let mut dispatch = self.inner.watcher.dispatch.lock().expect("mutex is poisoned");
        for &subscription_id in &subscriptions_to_destroy {
            dispatch.unregister_subscription(subscription_id);
        }
        drop(dispatch);

        for subscription_id in subscriptions_to_destroy {
            self.inner.watcher.backend.destroy_subscription(subscription_id);
        }
    }
}

impl<'a> WatchSessionRx<'a> {
    pub async fn next_event(&mut self) -> Option<Result<Event, ClientOverflow>> {
        let span = trace_span!("next_event", ?self.inner.session_id);
        async {
            loop {
                if let Some(next) = self.pending.pop_front() {
                    break Some(Ok(next));
                } else {
                    let session_event = self.events.next().await?;
                    match session_event.log(|_| warn!("Too slow, the channel is overflown")) {
                        Err(_) => break Some(Err(ClientOverflow)),
                        Ok(event) => {
                            let mut state = self.inner.state.lock().expect("mutex is poisoned");
                            self.pending = VecDeque::from(state.handle_event(event, &self.inner.watcher));
                        }
                    }
                }
            }
        }
        .instrument(span)
        .await
    }

    pub fn next_event_blocking(&mut self) -> Option<Result<Event, ClientOverflow>> {
        futures::executor::block_on(self.next_event())
    }
}

impl<'a> WatchSession<'a> {
    /// Subscribes to every parent of [path], including any encountered symlink chains.
    /// Succeeds even if the path does not exist. The subscriber gets a notification if it appears.
    ///
    /// If the target path can no longer be resolved to a file system entry, Event::Removed will be sent to the client.
    /// If canonicalization of the target path changes in any way (e.g. due to changes in symlinks), Event::Rescan will be sent to the client.
    pub fn watch_root(&self, path: impl AsRef<Path>, scope: Scope) {
        self.tx.watch_root(path, scope)
    }

    /// The client is responsible for ensuring that every path segment leading up to `path` is already watched.
    pub fn watch_directory(&self, path: impl AsRef<Path>, scope: Scope) -> Result<(), WatchError> {
        self.tx.watch_directory(path, scope)
    }

    /// Follows a symlink chain and subscribes to changes in all intermediate links.
    ///
    /// The client is responsible for ensuring that every path segment leading up to `symlink_path` is already watched.
    pub fn watch_symlink(&self, symlink_path: impl Into<PathBuf>, scope: Scope) -> Result<(), WatchError> {
        self.tx.watch_symlink(symlink_path, scope)
    }

    /// Removes all watches (including roots and symlinks) registered at or under `path`.
    ///
    /// No-op if nothing is registered at or under `path`.
    pub fn remove_watches(&self, path: impl AsRef<Path>) {
        self.tx.remove_watches(path)
    }

    pub async fn next_event(&mut self) -> Result<Event, ClientOverflow> {
        self.rx.next_event().await.expect("unexpected end of session stream")
    }

    pub fn next_event_blocking(&mut self) -> Result<Event, ClientOverflow> {
        self.rx.next_event_blocking().expect("unexpected end of session stream")
    }

    pub(crate) fn canonicalization(&self, path: &Path) -> Result<PathBuf, CanonicalizationError> {
        self.tx
            .inner
            .state
            .lock()
            .expect("mutex is poisoned")
            .tracker
            .canonicalization(path)
    }

    pub fn split(self) -> (WatchSessionTx<'a>, WatchSessionRx<'a>) {
        (self.tx, self.rx)
    }
}

pub(crate) struct InnerSession<'a> {
    state: Mutex<SessionState>,
    // used from event_stream
    pub(crate) watcher: Box<dyn Deref<Target = Watcher> + Send + Sync + 'a>,
    session_id: SessionId,
}

impl<'a> Drop for InnerSession<'a> {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("mutex is poisoned");
        let mut dispatch = self.watcher.dispatch.lock().expect("mutex is poisoned");
        for (_, (subscription_id, _)) in state.symbolic_paths.drain() {
            self.watcher.backend.destroy_subscription(subscription_id);
            dispatch.unregister_subscription(subscription_id);
        }
        self.watcher.backend.destroy_subscription(state.canonicalization_subscription_id);
        dispatch.unregister_subscription(state.canonicalization_subscription_id);
    }
}

pub(crate) struct SessionState {
    /// Each symbolic key gets its own `SubscriptionId` because multiple symbolic paths can
    /// resolve to the same canonical path. Each must hold an independent subscription so
    /// that removing one does not cancel the others.
    symbolic_paths: HashMap<SymbolicKey, (SubscriptionId, Scope)>,
    /// Same rationale as `symbolic_paths`: a symlink chain watched by
    /// the canonicalization tracker might lead to the same canonical path
    /// as some other symbolic paths and it must use a dedicated subscription ID,
    /// independent from the per-path ones.
    canonicalization_subscription_id: SubscriptionId,
    tracker: canonicalization::Tracker,
    symbolic_tree: SymbolicTree<SymbolicKey>,
    /// These keys are not invalidated if their canonicalization is broken.
    root_keys: HashSet<SymbolicKey>,
}

impl SessionState {
    fn new(canonicalization_subscription_id: SubscriptionId) -> Self {
        Self {
            symbolic_paths: HashMap::default(),
            canonicalization_subscription_id,
            tracker: canonicalization::Tracker::new(),
            symbolic_tree: SymbolicTree::new(),
            root_keys: HashSet::default(),
        }
    }

    fn handle_event<'a>(&mut self, event: SessionEvent, watcher: &Watcher) -> Vec<Event> {
        let mut events = Vec::<Event>::new();
        let canonicalization = match &event.audience {
            Audience::All => true,
            Audience::Some(subscriptions) => subscriptions.contains(&self.canonicalization_subscription_id),
        };
        let canonicalization_updates = if canonicalization {
            self.tracker.handle_event(
                &event.event_path,
                event.event,
                &mut real_io(watcher.backend.as_ref(), self.canonicalization_subscription_id),
            )
        } else {
            HashMap::new()
        };

        self.tracker.collect_aliases(&event.event_path, |symbolic_prefix, sym_key, suffix| {
            if let Some(&(subscription_id, _)) = self.symbolic_paths.get(&sym_key) {
                let canonicalization_changed = canonicalization_updates.contains_key(&sym_key);
                trace!(?canonicalization_changed, ?subscription_id, "root considers event");
                if !canonicalization_changed && event.audience.contains(subscription_id) {
                    let symbolic_path = symbolic_prefix.join(suffix);
                    let event = match &event.event {
                        BackendEvent::RecentlyCreated { file_type } | BackendEvent::Changed { file_type } => Event::Dirty {
                            file_type: *file_type,
                            path: symbolic_path,
                        },
                        BackendEvent::Removed => Event::Removed { path: symbolic_path },
                        BackendEvent::Overflow | BackendEvent::Ambiguous => Event::Rescan { path: symbolic_path },
                    };
                    events.push(event);
                }
            } else {
                warn!(?symbolic_prefix, ?sym_key, ?suffix, "unknown key");
            }
        });

        trace!(?canonicalization_updates, "delivering canonicalization updates");

        // Snapshot (key, path) up front to avoid borrowing `self.tracker` while mutating the tree.
        let affected_paths: Vec<(SymbolicKey, PathBuf)> = canonicalization_updates
            .keys()
            .filter_map(|&sk| self.tracker.remind(sk).map(|p| (sk, p.to_path_buf())))
            .collect();

        for (sym_key, sym_path) in affected_paths {
            // Prune top-level keys; dominated descendants are covered by their ancestor.
            let top_level_prune = self.symbolic_tree.prune(
                &sym_path,
                |v| !canonicalization_updates.contains_key(v),
                |v| !self.root_keys.contains(v) && !v.eq(&sym_key),
            );
            let top_level = top_level_prune.is_some();
            if let Some(pruned) = top_level_prune {
                for pruned_key in &pruned {
                    if *pruned_key == sym_key {
                        continue;
                    }
                    if let Some((subscription_id, _)) = self.symbolic_paths.remove(pruned_key) {
                        watcher.destroy_subscription(subscription_id);
                    }
                    self.tracker.unregister(
                        *pruned_key,
                        &mut real_io(watcher.backend.as_ref(), self.canonicalization_subscription_id),
                    );
                }
            }
            if top_level {
                // TODO see rescan_delievered_on_each_registered_root
                // || self.root_keys.contains(&sym_key) {
                // Send event for this key itself
                if let Some(&(subscription_id, scope)) = self.symbolic_paths.get(&sym_key) {
                    // Whether it is resolved now or not, all the previous canonical subscriptions are obsolete.
                    watcher.backend.destroy_subscription(subscription_id);
                    match &canonicalization_updates[&sym_key] {
                        CanonicalizationUpdate::Resolved => {
                            // If the triggering event resolved the canonical target, try to re-subscribe
                            match self.tracker.canonicalization(&sym_path) {
                                Ok(canonical) => {
                                    let direct = watcher
                                        .backend
                                        .as_ref()
                                        .add_watch(&canonical, subscription_id, scope, ParentPolicy::Unchecked)
                                        .is_ok();
                                    if direct {
                                        // A directory target appeared and we re-subscribed. Send Rescan so the client can traverse it.
                                    } else {
                                        // Not a directory. Try watching the parent (the file symlink case).
                                        if let Some(canonical_parent) = canonical.parent() {
                                            let _ = watcher.backend.add_watch(
                                                canonical_parent,
                                                subscription_id,
                                                Scope::DirectChildren,
                                                ParentPolicy::Unchecked,
                                            );
                                        }
                                    }
                                    trace!(?sym_path, "target appeared, send rescan");
                                    events.push(Event::Rescan { path: sym_path.clone() });
                                }
                                Err(err) => {
                                    // Retargeted and unresolvable. Send Rescan just in case.
                                    warn!(
                                        ?sym_key,
                                        ?err,
                                        "received CanonicalizationUpdate::Resolved for path, but canonicalization failed"
                                    );
                                    events.push(Event::Rescan { path: sym_path.clone() });
                                }
                            }
                        }
                        CanonicalizationUpdate::Broken => {
                            events.push(Event::Removed { path: sym_path.clone() });
                        }
                    }
                };
            }
        }
        events
    }
}

fn validate_path(path: &Path) {
    assert!(!path.is_empty_path(), "path argument is empty");
    assert!(path.is_absolute(), "only absolute paths are allowed: {}", path.display());
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("symbolic_paths", &self.symbolic_paths)
            .field("canonicalization_subscription_id", &self.canonicalization_subscription_id)
            .field("tracker", &self.tracker)
            .finish()
    }
}

struct RealIO<'a> {
    backend: &'a dyn WatcherBackend,
    canonicalization_subscription_id: SubscriptionId,
}

impl CanonicalizationIO for RealIO<'_> {
    fn read_link(&mut self, canonical_path: &Path) -> Result<ReadLink, io::Error> {
        read_link(canonical_path)
    }

    fn add_watch(&mut self, canonical_path: &Path) -> Result<WatchHandle, BackendError> {
        // Canonicalization probes directories to detect symlinks; it establishes the first watch of
        // a chain and therefore cannot require a watched parent.
        self.backend.add_watch(
            canonical_path,
            self.canonicalization_subscription_id,
            Scope::DirectChildren,
            ParentPolicy::Unchecked,
        )?;
        Ok(WatchHandle)
    }

    fn destroy_watch(&mut self, _handle: WatchHandle, canonical_path: &Path) {
        self.backend
            .remove_watch(canonical_path, self.canonicalization_subscription_id, Scope::DirectChildren);
    }
}

fn real_io<'a>(backend: &'a dyn WatcherBackend, canonicalization_subscription_id: SubscriptionId) -> impl CanonicalizationIO + 'a {
    RealIO {
        backend,
        canonicalization_subscription_id,
    }
}
