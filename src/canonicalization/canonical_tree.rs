use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use super::{cycles::CycleDetection, CanonicalizationIO, ReadLink};
use crate::{
    backend::BackendError,
    util::{
        id_set::IdSet,
        minislab,
        names::Names,
        path_util::{head, path_stack::PathStack, PathExt},
    },
};
use tracing::{instrument, trace, warn};

minislab::key!(pub(super) CanonicalKey);

/// No special handle is needed to destroy a watch, but we don't want to forget which nodes own one.
#[derive(Debug)]
#[must_use]
pub struct WatchHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolicKey(pub(super) NonZeroU32);

/// Outcome of expanding a symbolic path into the canonical tree.
#[derive(Debug, Clone)]
pub enum ChainOutcome {
    /// The path resolved to an existing canonical target.
    Resolved { canonical_path: PathBuf },
    /// The chain traverses at least one symlink but does not resolve right now (a dangling or
    /// cyclic symlink). It stays tracked so the client is notified if it later resolves.
    DanglingSymlink,
    /// The path is not a symlink and does not currently exist; there is nothing to follow.
    Missing,
}

impl ChainOutcome {
    pub fn resolved(&self) -> bool {
        matches!(self, ChainOutcome::Resolved { .. })
    }

    pub fn into_canonical_path(self) -> Option<PathBuf> {
        match self {
            ChainOutcome::Resolved { canonical_path } => Some(canonical_path),
            ChainOutcome::DanglingSymlink | ChainOutcome::Missing => None,
        }
    }
}

/// The tree keeps all records that are of interest to currently registered paths.
/// If there is no record, it is irrelevant. If a path is removed, the corresponding records are removed.
#[derive(Debug)]
pub struct CanonicalTree {
    records: minislab::Slab<CanonicalKey, CanonicalNode>,
    root: CanonicalKey,
}

#[derive(Debug)]
enum CanonicalNode {
    Root {
        children: Names<CanonicalKey>,
    },
    /// No one told us that this is not a directory, but it could still be a file.
    ///
    /// We rely on the backend to do the check, and only Linux can do that without additional IO.
    Regular {
        watch_handle: WatchHandle,
        /// Paths that are canonically terminated on this node
        aliases: IdSet<SymbolicKey>,
        children: Names<CanonicalKey>,
    },
    Symlink {
        /// Set of paths that are canonicalized via this symlink in any way.
        /// Symlinks should be aware of all of them so that we can propagate the change backwards.
        /// Unlike terminal nodes like Regular or Missing, one path can visit the same symlink twice; see the `same_symlink_encountered_twice` test.
        continuations: Continuations,
        /// Absolute path; not canonical
        target: PathBuf,
    },
    File {
        /// Paths that expected this node to be a directory
        blocked_paths: IdSet<SymbolicKey>,
        /// Paths that are canonically terminated on this node
        aliases: IdSet<SymbolicKey>,
    },
    /// Failed to watch this component, either because of an IO error or because of an internal backend error. It makes no difference.
    Missing {
        /// All paths that couldn't be canonicalized because this node is missing
        blocked_paths: IdSet<SymbolicKey>,
    },
}

impl CanonicalNode {
    fn missing() -> Self {
        Self::Missing {
            blocked_paths: Default::default(),
        }
    }

    fn regular(watch_handle: WatchHandle) -> Self {
        Self::Regular {
            watch_handle,
            aliases: Default::default(),
            children: Default::default(),
        }
    }

    fn file() -> Self {
        CanonicalNode::File {
            blocked_paths: Default::default(),
            aliases: Default::default(),
        }
    }

    fn symlink(target: impl Into<PathBuf>) -> Self {
        CanonicalNode::Symlink {
            continuations: Continuations::default(),
            target: target.into(),
        }
    }
}

impl CanonicalTree {
    pub fn new() -> Self {
        let mut canonical = minislab::Slab::new();
        let canonical_root = canonical.insert(CanonicalNode::Root {
            children: Names::default(),
        });

        Self {
            records: canonical,
            root: canonical_root,
        }
    }

    pub fn resolve_symbolic(&mut self, symbolic_path: &Path) -> Symbolic<'_> {
        assert!(!symbolic_path.is_empty_path());

        advance_symbolic(self, PathBuf::new(), self.root, symbolic_path)
    }

    pub fn resolve_canonical(&mut self, canonical_path: &Path) -> Option<Canonical<'_>> {
        assert!(!canonical_path.is_empty_path());

        resolve_canonical(self, PathBuf::new(), self.root, canonical_path)
    }

    /// Routes a canonical path to registered symbolic paths.
    ///
    /// [consumer] will be invoked for each PathKey that is canonically terminated on a prefix of [canonical_path].
    /// Arguments are (prefix, key, suffix).
    #[instrument(level = "trace", skip_all, fields(?canonical_path))]
    pub fn collect_aliases(&self, canonical_path: &Path, consumer: &mut dyn FnMut(SymbolicKey, &Path)) {
        fn recur(tree: &CanonicalTree, current_key: CanonicalKey, suffix: &Path, consumer: &mut dyn FnMut(SymbolicKey, &Path)) {
            if let CanonicalNode::Regular { aliases, .. } | CanonicalNode::File { aliases, .. } = &tree.records[current_key] {
                aliases.for_each(|path_key| {
                    trace!(?path_key, ?suffix, "dispatching symbolic path");
                    consumer(path_key, suffix)
                });
            }
            if let Some((head, tail)) = head(suffix) {
                if let CanonicalNode::Root { children } | CanonicalNode::Regular { children, .. } = &tree.records[current_key] {
                    if let Some(next_key) = children.get(&head).cloned() {
                        recur(tree, next_key, tail, consumer)
                    }
                }
            }
        }
        recur(self, self.root, canonical_path, consumer)
    }

    /// The function will attempt to establish a watch on every directory component of the given path.
    /// If part of the path doesn't exist yet, we should be able to know when more components become available.
    ///
    /// If a path component is a symbolic link, the link will be dereferenced and the new path, formed by this dereference,
    /// will be used instead. The dereference happens one step at a time, recursively. Every dereference is tracked
    pub fn expand_symlink_chain(
        self: &mut CanonicalTree,
        path_key: SymbolicKey,
        symbolic_path: &Path,
        io: &mut dyn CanonicalizationIO,
    ) -> ChainOutcome {
        let mut current_entry = self.resolve_symbolic(symbolic_path);
        let mut cycle = CycleDetection::new();
        let mut followed_symlink = false;
        loop {
            current_entry = match current_entry {
                Symbolic::Resolved(mut entry) => {
                    entry.add_alias(path_key);
                    break ChainOutcome::Resolved {
                        canonical_path: entry.canonical_prefix,
                    };
                }
                Symbolic::Unresolved(mut entry) => {
                    entry.add_blocked(path_key);
                    break if followed_symlink {
                        ChainOutcome::DanglingSymlink
                    } else {
                        ChainOutcome::Missing
                    };
                }
                Symbolic::Symlink(mut entry) => {
                    // Even a symlink whose target is missing counts as "a symlink exists here".
                    followed_symlink = true;
                    if cycle.record_visit(entry.key, &entry.suffix) {
                        entry.record_observer(path_key);
                        entry.resolve()
                    } else {
                        break ChainOutcome::DanglingSymlink;
                    }
                }
                Symbolic::Absent(absent_entry) => absent_entry.fetch_next(io),
            }
        }
    }

    /// The path_key is no longer of interest. Remove all associated information.
    ///
    /// [path] is the symbolic path that is represented by [path_key]
    pub fn remove_key(&mut self, path_key: SymbolicKey, path: &Path, io: &mut dyn CanonicalizationIO) {
        remove_key_from_path(self, path_key, path, None, io);
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        let has_only_root = self.records.len() == 1 && self.records.contains(self.root);
        has_only_root
            && match &self.records[self.root] {
                CanonicalNode::Root { children } => children.is_empty(),
                unexpected => unreachable!("expected CanonicalNode::Root, got {:?}", unexpected),
            }
    }
}

impl Default for CanonicalTree {
    fn default() -> Self {
        Self::new()
    }
}

#[instrument(level = "trace", skip(io))]
fn fetch(canonical_path: &Path, io: &mut dyn CanonicalizationIO) -> CanonicalNode {
    match io.read_link(canonical_path) {
        Ok(ReadLink::Symlink(target)) => {
            trace!(?canonical_path, ?target, "path is a symlink");
            CanonicalNode::symlink(target)
        }
        Ok(ReadLink::NotALink) => {
            trace!(?canonical_path, "path exists, adding a watch");
            // TODO do not add_watch if tail is empty. how to store it then?
            match io.add_watch(canonical_path) {
                Ok(watch_handle) => {
                    trace!(?canonical_path, "watch added");
                    CanonicalNode::regular(watch_handle)
                }
                Err(BackendError::IO(io_err)) => match io_err.kind() {
                    std::io::ErrorKind::NotADirectory => {
                        trace!("it is not a directory");
                        CanonicalNode::file()
                    }
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound => {
                        trace!("failed to add a watch, store missing node");
                        CanonicalNode::missing()
                    }
                    error => {
                        warn!(?canonical_path, ?error, "io error on adding a watch");
                        CanonicalNode::missing()
                    }
                },
                Err(BackendError::DetachedParent) => {
                    // Canonicalization probes with `ParentPolicy::Unchecked`, so a detached-parent rejection cannot originate here
                    panic!("unexpected DetachedParent while probing canonicalization");
                }
            }
        }
        Err(_io_error) => CanonicalNode::missing(),
    }
}

fn replace_node(
    tree: &mut CanonicalTree,
    key: CanonicalKey,
    replacement: CanonicalNode,
    io: &mut dyn CanonicalizationIO,
) -> InvalidatedPaths {
    replace_node_with(tree, key, io, |_| replacement)
}

pub struct InvalidatedPaths {
    pub broken: HashSet<SymbolicKey>,
    pub all: HashSet<SymbolicKey>,
}

impl InvalidatedPaths {
    pub fn empty() -> Self {
        InvalidatedPaths {
            broken: HashSet::new(),
            all: HashSet::new(),
        }
    }
}

/// The subtree is removed and [f] is invoked with a set of keys found inside (or reachable via a symlink that was inside).
///
/// It is assumed that all watches in the subtree are already destroyed by the backend.
#[instrument(level = "trace", skip_all, fields(?target))]
fn replace_node_with(
    tree: &mut CanonicalTree,
    target: CanonicalKey,
    io: &mut dyn CanonicalizationIO,
    f: impl FnOnce(&HashSet<SymbolicKey>) -> CanonicalNode,
) -> InvalidatedPaths {
    let mut all_affected = HashSet::<SymbolicKey>::new();
    let mut broken_aliases = HashSet::<SymbolicKey>::new();

    // collect all affected keys before replacing the node to give the set to [f]
    fn before(record: &CanonicalNode, all_affected: &mut HashSet<SymbolicKey>) {
        if let CanonicalNode::Regular { aliases, .. } | CanonicalNode::File { aliases, .. } = record {
            aliases.for_each(|alias| {
                all_affected.insert(alias);
            });
        }
        if let CanonicalNode::File { blocked_paths, .. } | CanonicalNode::Missing { blocked_paths } = record {
            blocked_paths.for_each(|alias| {
                all_affected.insert(alias);
            });
        }
        if let CanonicalNode::Symlink { continuations, .. } = record {
            all_affected.extend(continuations.keys());
        }
    }

    // We cannot borrow the tree while holding a &CanonicalNode, so we have to take ownership.
    fn after(
        tree: &mut CanonicalTree,
        record: CanonicalNode,
        broken_aliases: &mut HashSet<SymbolicKey>,
        current_key: Option<CanonicalKey>,
        io: &mut dyn CanonicalizationIO,
    ) {
        match record {
            CanonicalNode::Regular { aliases, .. } | CanonicalNode::File { aliases, .. } => {
                aliases.for_each(|alias| {
                    broken_aliases.insert(alias);
                });
            }
            CanonicalNode::Symlink { continuations, target } => {
                for (path_key, suffixes) in continuations.map.into_iter() {
                    for suffix in suffixes {
                        if remove_key_from_path(tree, path_key, &target.join(suffix), current_key, io) {
                            broken_aliases.insert(path_key);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    assert_ne!(target, tree.root, "trying to replace root");
    dfs_before_mut(tree, target, |tree, key| {
        // we will reuse the target record to store the replacement
        if key != target {
            before(&tree.records[key], &mut all_affected);
            // The node is removed while its parent still thinks it has it as a child, which makes the tree inconsistent.
            // The subtree will be replaced entirely, so it makes little sense to waste time updating the relation.
            // However, it requires extra attention in the functions that are invoked here.
            let removed = tree.records.remove(key);
            after(tree, removed, &mut broken_aliases, None, io);
        }
    });

    before(&tree.records[target], &mut all_affected);
    let removed = std::mem::replace(&mut tree.records[target], f(&all_affected));
    // Do not remove the key from the current node itself.
    // We could reach it accidentally in the case of cyclic symlinks.
    // This is the only case where we care, because there are no more terminal nodes for this key and symlinks are capable of distinguishing different continuations.
    let current_key = Some(target);
    after(tree, removed, &mut broken_aliases, current_key, io);

    InvalidatedPaths {
        all: all_affected,
        broken: broken_aliases,
    }
}

#[derive(Debug)]
pub enum Symbolic<'t> {
    /// The path is successfully resolved to a canonical node.
    Resolved(SymbolicResolved<'t>),
    /// The path cannot be canonicalized because one of its components is missing or is a file.
    Unresolved(SymbolicUnresolved<'t>),
    /// Encountered a symlink while resolving the path.
    Symlink(SymlinkEntry<'t>),
    /// Information about the next segment of the path is not present in the tree
    Absent(AbsentEntry<'t>),
}

#[derive(Debug)]
pub struct SymlinkEntry<'t> {
    tree: &'t mut CanonicalTree,
    /// CanonicalNode::Symlink
    key: CanonicalKey,
    suffix: PathBuf,
}

impl<'t> SymlinkEntry<'t> {
    pub fn key(&self) -> CanonicalKey {
        self.key
    }

    pub fn suffix(&self) -> &Path {
        &self.suffix
    }

    pub fn resolve(self) -> Symbolic<'t> {
        match &self.tree.records[self.key] {
            CanonicalNode::Symlink { target, .. } => {
                let full_target = target.join(&self.suffix);
                advance_symbolic(self.tree, PathBuf::new(), self.tree.root, &full_target)
            }
            unexpected => {
                unreachable!("expected CanonicalNode::Symlink, got {:?}", unexpected);
            }
        }
    }

    pub fn record_observer(&mut self, path_key: SymbolicKey) {
        match &mut self.tree.records[self.key] {
            CanonicalNode::Symlink { continuations, .. } => {
                continuations.insert(path_key, self.suffix.to_path_buf());
            }
            unexpected => {
                unreachable!("expected CanonicalNode::Symlink, got {:?}", unexpected);
            }
        }
    }
}

#[derive(Debug)]
pub struct SymbolicUnresolved<'t> {
    tree: &'t mut CanonicalTree,
    /// CanonicalNode::File | CanonicalNode::Missing
    key: CanonicalKey,
}

impl<'t> SymbolicUnresolved<'t> {
    pub fn add_blocked(&mut self, path_key: SymbolicKey) {
        match &mut self.tree.records[self.key] {
            CanonicalNode::File { blocked_paths, .. } | CanonicalNode::Missing { blocked_paths } => {
                blocked_paths.insert(path_key);
            }
            unexpected => {
                unreachable!("expected CanonicalNode::File | CanonicalNode::Missing, got {:?}", unexpected);
            }
        }
    }
}

#[derive(Debug)]
pub struct SymbolicResolved<'t> {
    pub canonical_prefix: PathBuf,
    tree: &'t mut CanonicalTree,
    /// CanonicalNode::Regular | CanonicalNode::File
    key: CanonicalKey,
}

impl<'t> SymbolicResolved<'t> {
    pub fn add_alias(&mut self, alias: SymbolicKey) {
        match &mut self.tree.records[self.key] {
            CanonicalNode::Regular { aliases, .. } | CanonicalNode::File { aliases, .. } => {
                aliases.insert(alias);
            }
            unexpected => {
                unreachable!("expected CanonicalNode::Regular | CanonicalNode::File, got {:?}", unexpected);
            }
        }
    }
}

#[derive(Debug)]
pub struct AbsentEntry<'t> {
    /// CanonicalNode::Root | CanonicalNode::Regular
    parent_key: CanonicalKey,
    pub present_canonical_path: PathBuf,
    pub symbolic_suffix: PathBuf,
    tree: &'t mut CanonicalTree,
}

impl<'t> AbsentEntry<'t> {
    pub fn fetch_next(self, io: &mut dyn CanonicalizationIO) -> Symbolic<'t> {
        let AbsentEntry {
            parent_key: parent,
            present_canonical_path: mut canonical_prefix,
            symbolic_suffix,
            tree,
        } = self;
        let (head, tail) = head(&symbolic_suffix).expect("entry thinks it is absent, but the suffix is empty");
        canonical_prefix.push(&head);
        let node = fetch(&canonical_prefix, io);
        let new_key = tree.records.insert(node);
        match &mut tree.records[parent] {
            CanonicalNode::Root { children } | CanonicalNode::Regular { children, .. } => {
                children.insert(head.to_os_string(), new_key);
            }
            unexpected => {
                unreachable!("expected CanonicalNode::Root | CanonicalNode::Regular, got {:?}", unexpected);
            }
        }
        // It has nowhere to go, but it constructs an Entry the usual way.
        advance_symbolic(tree, canonical_prefix, new_key, tail)
    }
}

/// Navigates further along [symbolic_suffix] and stops on the first encountered node that is not Regular.
fn advance_symbolic<'r>(
    tree: &'r mut CanonicalTree,
    mut canonical_prefix: PathBuf,
    current_key: CanonicalKey,
    symbolic_suffix: &Path,
) -> Symbolic<'r> {
    let record = &tree.records[current_key];
    match record {
        CanonicalNode::Regular { children, .. } | CanonicalNode::Root { children } => {
            match head(symbolic_suffix) {
                Some((head, tail)) => match children.get(head.as_ref()) {
                    Some(next_key) => {
                        canonical_prefix.push(head);
                        advance_symbolic(tree, canonical_prefix, *next_key, tail)
                    }
                    None => {
                        Symbolic::Absent(AbsentEntry {
                            tree,
                            parent_key: current_key,
                            present_canonical_path: canonical_prefix,
                            symbolic_suffix: symbolic_suffix.to_path_buf(), // TODO allocation
                        })
                    }
                },
                None => Symbolic::Resolved(SymbolicResolved {
                    tree,
                    key: current_key,
                    canonical_prefix,
                }),
            }
        }
        CanonicalNode::Symlink { .. } => {
            Symbolic::Symlink(SymlinkEntry {
                key: current_key,
                tree,
                suffix: symbolic_suffix.to_path_buf(), // TODO allocation
            })
        }
        CanonicalNode::Missing { .. } => Symbolic::Unresolved(SymbolicUnresolved { tree, key: current_key }),
        CanonicalNode::File { .. } => {
            if symbolic_suffix.is_empty_path() {
                Symbolic::Resolved(SymbolicResolved {
                    tree,
                    key: current_key,
                    canonical_prefix,
                })
            } else {
                Symbolic::Unresolved(SymbolicUnresolved { tree, key: current_key })
            }
        }
    }
}

#[derive(Debug)]
pub enum Canonical<'t> {
    /// The path was fully resolved
    Resolved(CanonicalEntry<'t>),
    /// Resolution couldn't continue because one of the components is not a directory
    Unresolved(CanonicalEntry<'t>),
}

pub struct CanonicalEntry<'t> {
    tree: &'t mut CanonicalTree,
    key: CanonicalKey,
    canonical_path: PathBuf,
}

impl<'t> std::fmt::Debug for CanonicalEntry<'t> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanonicalEntry")
            .field("key", &self.key)
            .field("canonical_path", &self.canonical_path)
            .field("record", &self.tree.records.get(self.key))
            .finish()
    }
}

pub enum RecordKind {
    Regular,
    Symlink,
    Missing,
}

impl<'t> CanonicalEntry<'t> {
    #[instrument(level = "trace", skip_all)]
    pub fn replace_with_missing(self, io: &mut dyn CanonicalizationIO) -> InvalidatedPaths {
        let Self { tree, key, .. } = self;
        replace_node_with(tree, key, io, |keys| CanonicalNode::Missing {
            blocked_paths: IdSet::from(keys),
        })
    }

    pub fn kind(&self) -> RecordKind {
        match &self.tree.records[self.key] {
            CanonicalNode::Root { .. } => unreachable!(),
            CanonicalNode::Missing { .. } => RecordKind::Missing,
            CanonicalNode::Regular { .. } | CanonicalNode::File { .. } => RecordKind::Regular,
            CanonicalNode::Symlink { .. } => RecordKind::Symlink,
        }
    }

    #[instrument(level = "trace", skip_all)]
    pub fn refetch(self, io: &mut dyn CanonicalizationIO) -> InvalidatedPaths {
        let Self { canonical_path, key, tree } = self;
        let replacement = fetch(&canonical_path, io);
        replace_node(tree, key, replacement, io)
    }
}

fn resolve_canonical<'t>(tree: &'t mut CanonicalTree, mut prefix: PathBuf, key: CanonicalKey, suffix: &Path) -> Option<Canonical<'t>> {
    if let Some((head, tail)) = head(suffix) {
        match &tree.records[key] {
            CanonicalNode::Root { children } | CanonicalNode::Regular { children, .. } => {
                if let Some(next) = children.get(&head) {
                    prefix.push(head);
                    resolve_canonical(tree, prefix, *next, tail)
                } else {
                    None
                }
            }
            CanonicalNode::Missing { .. } | CanonicalNode::File { .. } | CanonicalNode::Symlink { .. } => {
                Some(Canonical::Unresolved(CanonicalEntry {
                    canonical_path: prefix,
                    tree,
                    key,
                }))
            }
        }
    } else {
        assert_ne!(tree.root, key);
        let entry = CanonicalEntry {
            canonical_path: prefix,
            tree,
            key,
        };
        Some(Canonical::Resolved(entry))
    }
}

/// [f] is invoked before going deeper
fn dfs_before_mut(tree: &mut CanonicalTree, starting_node: CanonicalKey, mut f: impl FnMut(&mut CanonicalTree, CanonicalKey)) {
    let mut current_node = starting_node;
    let mut stack = Vec::<CanonicalKey>::new();
    loop {
        if let CanonicalNode::Root { children } | CanonicalNode::Regular { children, .. } = &mut tree.records[current_node] {
            children.for_each(|_, child| {
                stack.push(*child);
            })
        }
        f(tree, current_node);
        if let Some(next) = stack.pop() {
            current_node = next;
        } else {
            break;
        }
    }
}

/// Invoked when [path_key] is no longer canonicalized via [intermediate_path].
///
/// Recurses the same way canonicalization does, clears all mentions of [path_key] up to [intermediate_path],
/// and destroys the updated nodes if [path] was the last observer.
///
/// [intermediate_path] is still not fully canonicalized and might contain symlink segments.
///
/// Returns `true` if an alias is removed.
#[instrument(level = "trace", skip_all, fields(?path_key, ?intermediate_path))]
fn remove_key_from_path(
    tree: &mut CanonicalTree,
    path_key: SymbolicKey,
    intermediate_path: &Path,
    exception: Option<CanonicalKey>,
    io: &mut dyn CanonicalizationIO,
) -> bool {
    #[derive(Clone, Copy)]
    struct Recur {
        record_removed: bool,
        alias_broken: bool,
    }

    #[instrument(level = "trace", name = "remove_key_recur", skip_all, fields(?suffix, ?current_key))]
    fn recur(
        this: &mut CanonicalTree,
        canonical_prefix: &mut PathStack,
        current_key: CanonicalKey,
        suffix: &Path,
        path_key: SymbolicKey,
        exception: Option<CanonicalKey>,
        io: &mut dyn CanonicalizationIO,
    ) -> Recur {
        // It is invoked from `replace_node_with`, which breaks the consistency invariants.
        if !this.records.contains(current_key) {
            return Recur {
                record_removed: false,
                alias_broken: false,
            };
        }
        fn recur_on_children(
            this: &mut CanonicalTree,
            canonical_prefix: &mut PathStack,
            current_key: CanonicalKey,
            suffix: &Path,
            path_key: SymbolicKey,
            exception: Option<CanonicalKey>,
            io: &mut dyn CanonicalizationIO,
        ) -> bool {
            if let CanonicalNode::Root { children } | CanonicalNode::Regular { children, .. } = &mut this.records[current_key] {
                if let Some((head, child, tail)) =
                    head(suffix).and_then(|(head, tail)| children.get(&head).cloned().map(|c| (head, c, tail)))
                {
                    let r = {
                        canonical_prefix.push(head.as_ref()).expect("illegal argument");
                        let r = recur(this, canonical_prefix, child, tail, path_key, exception, io);
                        let popped = canonical_prefix.pop();
                        assert!(popped);
                        r
                    };
                    if r.record_removed {
                        match &mut this.records[current_key] {
                            CanonicalNode::Regular { children, .. } | CanonicalNode::Root { children } => {
                                children.remove_by_value(&child);
                            }
                            unexpected => unreachable!("expected CanonicalNode::Regular | CanonicalNode::Root, got {:?}", unexpected),
                        }
                    }
                    r.alias_broken
                } else {
                    false
                }
            } else {
                false
            }
        }

        // remove the key
        let alias_broken = exception.is_none_or(|ex| ex != current_key)
            && match &mut this.records[current_key] {
                CanonicalNode::Root { .. } => recur_on_children(this, canonical_prefix, current_key, suffix, path_key, exception, io),
                CanonicalNode::File { aliases, blocked_paths } => {
                    blocked_paths.remove(path_key);
                    aliases.remove(path_key)
                }
                CanonicalNode::Regular { aliases, .. } => {
                    aliases.remove(path_key) || recur_on_children(this, canonical_prefix, current_key, suffix, path_key, exception, io)
                }
                CanonicalNode::Missing { blocked_paths } => {
                    blocked_paths.remove(path_key);
                    false
                }
                CanonicalNode::Symlink { continuations, target, .. } => {
                    // Even if the symlink is cyclic, we won't fall into an infinite cycle because the continuation is removed here.
                    if continuations.remove(path_key, suffix) {
                        let full_target = target.join(suffix);
                        let mut prefix = PathStack::new();
                        let r = recur(this, &mut prefix, this.root, &full_target, path_key, exception, io);
                        assert!(!r.record_removed, "root should not be removed");
                        r.alias_broken
                    } else {
                        false
                    }
                }
            };

        // The entry could be deleted by the recursion on a cyclic symlink.
        if !this.records.contains(current_key) {
            Recur {
                record_removed: false,
                alias_broken: false, // if it is a cycle, there is no alias
            }
        } else {
            // remove the node if necessary
            let to_be_removed = match &this.records[current_key] {
                CanonicalNode::Root { .. } => false,
                CanonicalNode::File { aliases, blocked_paths } => aliases.is_empty() && blocked_paths.is_empty(),
                CanonicalNode::Regular { aliases, children, .. } => aliases.is_empty() && children.is_empty(),
                CanonicalNode::Missing { blocked_paths } => blocked_paths.is_empty(),
                CanonicalNode::Symlink { continuations, .. } => continuations.is_empty(),
            };
            if to_be_removed {
                let record = this.records.remove(current_key);
                if let CanonicalNode::Regular { watch_handle, .. } = record {
                    io.destroy_watch(watch_handle, canonical_prefix.as_path());
                }
            }
            Recur {
                record_removed: to_be_removed,
                alias_broken,
            }
        }
    }
    let mut prefix = PathStack::new();
    let Recur {
        record_removed,
        alias_broken,
    } = recur(tree, &mut prefix, tree.root, intermediate_path, path_key, exception, io);
    assert_eq!(false, record_removed, "root should not be removed");
    alias_broken
}

#[derive(Debug, Default)]
struct Continuations {
    /// Relative suffix paths pending resolution, keyed by the symbolic link that owns them.
    ///
    /// Boxed to keep `CanonicalNode` small: an inline `HashMap` is 48 bytes, a `Box` is 8.
    /// Symlinks with many continuations are uncommon, so the extra indirection is acceptable.
    map: Box<HashMap<SymbolicKey, HashSet<PathBuf>>>,
}

impl Continuations {
    pub fn insert(&mut self, path_key: SymbolicKey, suffix: PathBuf) {
        self.map.entry(path_key).or_default().insert(suffix);
    }

    pub fn remove(&mut self, path_key: SymbolicKey, suffix: &Path) -> bool {
        use std::collections::hash_map::Entry;

        match self.map.entry(path_key) {
            Entry::Occupied(mut occupied_entry) => {
                let removed = occupied_entry.get_mut().remove(suffix);
                if removed && occupied_entry.get().is_empty() {
                    occupied_entry.remove_entry();
                }
                removed
            }
            Entry::Vacant(_) => false,
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = SymbolicKey> + '_ {
        self.map.keys().cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod test {
    use crate::canonicalization::mock::*;

    use crate::test_helpers::{assert_matches, enable_logging, path, root};

    use super::*;

    fn fetch_absent_root(entry: Symbolic<'_>) -> AbsentEntry<'_> {
        if let Symbolic::Absent(absent) = entry {
            let entry = expect_commands(|io| absent.fetch_next(io), vec![Read(root!(), Directory)]);
            if let Symbolic::Absent(absent) = entry {
                absent
            } else {
                unreachable!("expected Entry::Absent got {:?}", entry)
            }
        } else {
            unreachable!("expected Entry::Absent got {:?}", entry)
        }
    }

    #[test]
    fn fetch_regular() {
        enable_logging();

        let mut sym = CanonicalTree::new();
        let absent = fetch_absent_root(sym.resolve_symbolic(&path!("1")));
        let entry = expect_commands(|io| absent.fetch_next(io), vec![Read(path!("1"), Directory)]);
        assert_matches!(entry, Symbolic::Resolved(_));
    }

    #[test]
    fn fetch_symlink() {
        enable_logging();

        let mut sym = CanonicalTree::new();
        let absent = fetch_absent_root(sym.resolve_symbolic(&path!("1")));
        let entry = expect_commands(
            |io| absent.fetch_next(io),
            vec![Read(path!("1"), Symlink(PathBuf::from(path!("2"))))],
        );
        assert_matches!(entry, Symbolic::Symlink { .. });
    }

    #[test]
    fn fetch_file() {
        enable_logging();

        let mut sym = CanonicalTree::new();
        let absent = fetch_absent_root(sym.resolve_symbolic(&path!("1")));
        let entry = expect_commands(|io| absent.fetch_next(io), vec![Read(path!("1"), File)]);
        assert_matches!(entry, Symbolic::Resolved(_));
    }

    #[test]
    fn fetch_missing() {
        enable_logging();

        let mut sym = CanonicalTree::new();
        let absent = fetch_absent_root(sym.resolve_symbolic(&path!("1")));
        let entry = expect_commands(|io| absent.fetch_next(io), vec![Read(path!("1"), NotFound)]);
        assert_matches!(entry, Symbolic::Unresolved(_));
    }

    #[test]
    fn size_is_40_bytes() {
        assert_eq!(40, size_of::<CanonicalNode>())
    }
}
