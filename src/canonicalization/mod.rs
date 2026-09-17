mod canonical_tree;
mod cycles;
mod read_link;
#[cfg(test)]
mod test;

#[cfg(test)]
mod mock;

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};
use tracing::{instrument, trace, trace_span};

use super::backend::{BackendError, BackendEvent};
use crate::util::id_source::IdSource;

pub use canonical_tree::{SymbolicKey, WatchHandle};
use canonical_tree::*;
use cycles::CycleDetection;
pub use read_link::{read_link, ReadLink};

/// Maintains records of all registered symbolic paths and monitors their canonicalization.
///
/// Allows dispatching a canonical event to all symbolic aliases and mapping a symbolic path to its canonical form.
/// In both situations there is an assumption that the path containing a symlink is registered in advance.
#[derive(Debug)]
pub struct Tracker {
    key_source: IdSource,
    registered_paths: HashMap<SymbolicKey, PathBuf>,
    tree: CanonicalTree,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            tree: CanonicalTree::default(),
            registered_paths: HashMap::default(),
            key_source: IdSource::new(),
        }
    }

    pub fn remind(&self, path_key: SymbolicKey) -> Option<&Path> {
        self.registered_paths.get(&path_key).map(|pb| pb.as_path())
    }

    /// Registers a symbolic path and starts monitoring its canonicalization.
    ///
    /// [`missing_policy`] controls what happens when the path cannot be canonicalized right now:
    /// [`MissingPolicy::Track`] keeps the path (so the client is notified once it appears), while
    /// [`MissingPolicy::Reject`] refuses paths that are neither resolvable nor a symlink chain to
    /// follow, leaving the tracker untouched and returning `None`.
    pub fn register(
        &mut self,
        symbolic_path: PathBuf,
        missing_policy: MissingPolicy,
        io: &mut dyn CanonicalizationIO,
    ) -> Option<Registered> {
        let path_key = SymbolicKey(self.key_source.next());
        let _span = trace_span!("register", ?symbolic_path, ?path_key, ?missing_policy).entered();
        let outcome = self.tree.expand_symlink_chain(path_key, &symbolic_path, io);
        // A caller that rejects missing paths (watch_symlink) has nothing to wait for when the path
        // is not a symlink and does not exist. A dangling symlink (link present, target missing)
        // still counts as a symlink chain and is tracked as usual.
        if missing_policy == MissingPolicy::Reject && matches!(outcome, ChainOutcome::Missing) {
            self.tree.remove_key(path_key, &symbolic_path, io);
            return None;
        }
        self.registered_paths.insert(path_key, symbolic_path);
        Some(Registered {
            key: path_key,
            canonical_path: outcome.into_canonical_path(),
        })
    }

    pub fn unregister(&mut self, path_key: SymbolicKey, io: &mut dyn CanonicalizationIO) {
        let symbolic_path = self.registered_paths.remove(&path_key);
        if let Some(symbolic_path) = symbolic_path {
            let _span = trace_span!("unregister", ?path_key, ?symbolic_path).entered();
            self.tree.remove_key(path_key, &symbolic_path, io);
        }
    }

    /// Applies all known redirects to the path.
    /// Relies on all symlinks in the path being registered in advance.
    #[instrument(level = "trace", skip_all, fields(?symbolic_path))]
    // TODO accept path_key instead? cache it?
    // TODO mut ref
    pub fn canonicalization(&mut self, symbolic_path: &Path) -> Result<PathBuf, CanonicalizationError> {
        let mut current_entry = self.tree.resolve_symbolic(symbolic_path);
        let mut cycle = CycleDetection::new();
        loop {
            match current_entry {
                Symbolic::Resolved(resolved_entry) => break Ok(resolved_entry.canonical_prefix),
                Symbolic::Symlink(symlink_entry) => {
                    if cycle.record_visit(symlink_entry.key(), symlink_entry.suffix()) {
                        current_entry = symlink_entry.resolve();
                    } else {
                        break Err(CanonicalizationError::SymlinkCycle);
                    }
                }
                Symbolic::Absent(absent_entry) => {
                    // Either the prefix is not registered or we have reached the end of it. We cannot distinguish these two cases.
                    let mut canonical_path = absent_entry.present_canonical_path;
                    canonical_path.extend(&absent_entry.symbolic_suffix);
                    break Ok(canonical_path);
                }
                Symbolic::Unresolved(_) => {
                    break Err(CanonicalizationError::PathDoesNotExist);
                }
            }
        }
    }

    /// Routes a canonical path to registered symbolic paths.
    /// Arguments are (prefix, key, suffix).
    #[instrument(level = "trace", skip_all, fields(?canonical_path))]
    pub fn collect_aliases(&self, canonical_path: &Path, mut consumer: impl FnMut(&Path, SymbolicKey, &Path)) {
        trace!("collect_aliases");
        self.tree.collect_aliases(canonical_path, &mut |path_key, suffix| {
            consumer(self.registered_paths.get(&path_key).expect("unknown path_key"), path_key, suffix)
        });
    }

    #[instrument(level = "trace", skip_all, fields(?canonical_path, ?event))]
    pub fn handle_event(
        &mut self,
        canonical_path: &Path,
        event: BackendEvent,
        io: &mut dyn CanonicalizationIO,
    ) -> HashMap<SymbolicKey, CanonicalizationUpdate> {
        let file_type = match &event {
            BackendEvent::RecentlyCreated { file_type } | BackendEvent::Changed { file_type } => Some(*file_type),
            BackendEvent::Overflow | BackendEvent::Ambiguous | BackendEvent::Removed => None,
        };
        trace!(?event, ?canonical_path, "handle_event");
        let entry = self.tree.resolve_canonical(canonical_path);
        trace!(?entry, "resolution result");
        match event {
            BackendEvent::Removed => match entry {
                Some(Canonical::Resolved(entry) | Canonical::Unresolved(entry)) => entry
                    .replace_with_missing(io)
                    .broken
                    .into_iter()
                    .map(|key| (key, CanonicalizationUpdate::Broken))
                    .collect(),
                None => HashMap::new(),
            },
            BackendEvent::Ambiguous | BackendEvent::Overflow | BackendEvent::Changed { .. } | BackendEvent::RecentlyCreated { .. } => {
                let invalidated_paths = match entry {
                    Some(Canonical::Resolved(entry)) => {
                        // A file cannot replace a directory and vice versa.
                        // Even on Windows, where rename can replace a file with a directory, we get two separate events about removal and creation.
                        let should_refetch = file_type.is_none_or(|file_type| match entry.kind() {
                            RecordKind::Regular => file_type.is_symlink(),
                            RecordKind::Symlink => true,
                            RecordKind::Missing => true,
                        });
                        if should_refetch {
                            entry.refetch(io)
                        } else {
                            InvalidatedPaths::empty()
                        }
                    }
                    Some(Canonical::Unresolved(entry)) => {
                        // The tree is under the impression that this canonical path cannot be canonicalized (strange).
                        // We know that it exists, which should imply that all its parents exist as well.
                        entry.refetch(io)
                    }
                    None => {
                        // We can get an event about a path that is sibling to one of our targets.
                        // If it is not present in the tree, there is nothing that needs to be done.
                        InvalidatedPaths::empty()
                    }
                };
                let InvalidatedPaths { broken, all } = invalidated_paths;
                let mut updates: HashMap<SymbolicKey, CanonicalizationUpdate> =
                    broken.into_iter().map(|key| (key, CanonicalizationUpdate::Broken)).collect();
                for path_key in all.into_iter() {
                    if self.tree.expand_symlink_chain(path_key, &self.registered_paths[&path_key], io).resolved() {
                        updates.insert(path_key, CanonicalizationUpdate::Resolved);
                    }
                }
                updates
            }
        }
    }

    #[cfg(test)]
    pub fn aliases(&self, canonical_path: &Path) -> Vec<PathBuf> {
        let mut v = Vec::new();
        self.collect_aliases(canonical_path, |prefix, _key, suffix| v.push(prefix.join(suffix)));
        v
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty() && self.registered_paths.is_empty()
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum CanonicalizationUpdate {
    Resolved, // maybe to a different target
    Broken,
}

/// Result of a successful [`Tracker::register`].
#[derive(Debug)]
pub struct Registered {
    /// Key identifying the freshly registered symbolic path.
    pub key: SymbolicKey,
    /// The canonical target if the path resolves right now; `None` while it is unresolved
    /// (a tracked missing root or a dangling symlink) but still registered.
    pub canonical_path: Option<PathBuf>,
}

/// Controls how [`Tracker::register`] reacts when a path cannot be canonicalized yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingPolicy {
    /// Register the path even if it currently resolves to a missing component, so the client is notified once the path appears.
    Track,
    /// Refuse to register a path that is not a symlink chain to follow.
    Reject,
}

/// Exists only to be mocked in tests
pub trait CanonicalizationIO {
    fn read_link(&mut self, canonical_path: &Path) -> Result<ReadLink, io::Error>;
    fn add_watch(&mut self, canonical_path: &Path) -> Result<WatchHandle, BackendError>;
    fn destroy_watch(&mut self, handle: WatchHandle, canonical_path: &Path);
}

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum CanonicalizationError {
    PathDoesNotExist,
    SymlinkCycle,
}
