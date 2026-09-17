use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
};

use tracing::{instrument, warn};
use super::SubscriptionId;
use crate::util::id_set::IdSet;
use crate::util::path_util::{head, segments, PathExt};
use crate::util::{minislab, multi_map::MultiMap, names::Names};

minislab::key!(pub RecordId);

#[derive(Debug)]
pub struct WatchesTree<V> {
    // Stored separately because the "root" has no value, and storing an Option would inflate the size of a record.
    prefixes: Names<RecordId>,
    records: minislab::Slab<RecordId, Record<V>>,
    // RecordId itself cannot serve as an identity of the watch:
    // - the record is shared between several clients, and each of them unsubscribes independently. Ref counting is an option, but it is considerably harder to debug
    // - they might be reused and their lifetime is controlled by the event loop, so it is not safe to let clients refer to them
    subscriptions: MultiMap<SubscriptionId, RecordId>,
}

#[derive(Debug)]
struct Record<V> {
    value: V,
    // minislab keys are NonZeroU32 so Option<RecordId> is the same size as RecordId
    parent: Option<RecordId>,
    subscriptions: IdSet<SubscriptionId>,
    children: Names<RecordId>,
}

#[derive(Debug)]
pub enum IllegalArgument {
    EmptyPath,
}

impl<V> WatchesTree<V> {
    pub fn empty() -> Self {
        WatchesTree {
            subscriptions: MultiMap::<SubscriptionId, RecordId>::default(),
            prefixes: Names::default(),
            records: minislab::Slab::default(),
        }
    }

    pub fn entry<'t, 'p: 't>(&'t mut self, canonical_path: &'p Path) -> Result<Entry<'t, 'p, V>, IllegalArgument> {
        fn recur<'t, 'p: 't, V>(
            tree: &'t mut WatchesTree<V>,
            original: &'p Path,
            current: RecordId,
            depth: usize,
            suffix: &'p Path,
        ) -> Result<Entry<'t, 'p, V>, IllegalArgument> {
            if let Some((head, tail)) = head(suffix) {
                if let Some(next) = tree.records[current].children.get(&head).cloned() {
                    recur(tree, original, next, depth + 1, tail)
                } else {
                    Ok(Entry::Absent(AbsentEntry {
                        present_node: Some(current),
                        full_path: original,
                        present_depth: depth,
                        parent: if tail.is_empty_path() { Some(current) } else { None },
                        tree,
                    }))
                }
            } else {
                Ok(Entry::Present(PresentEntry { tree, record_id: current }))
            }
        }

        if let Some((head, tail)) = head(canonical_path) {
            if let Some(first) = self.prefixes.get(&head).cloned() {
                recur(self, canonical_path, first, 1, tail)
            } else {
                Ok(Entry::Absent(AbsentEntry {
                    tree: self,
                    present_node: None,
                    parent: None,
                    full_path: canonical_path,
                    present_depth: 0,
                }))
            }
        } else {
            Err(IllegalArgument::EmptyPath)
        }
    }

    /// Slab ids are just indices in a vector, and on top of that they are reusable.
    /// It is possible to use a key after it was removed, to confuse one record with another, or to hit the vector bounds.
    #[allow(unused)] // It is used by inotify backend.
    pub fn unsafe_entry(&mut self, record_id: RecordId) -> PresentEntry<'_, V> {
        PresentEntry { tree: self, record_id }
    }

    #[instrument(level = "trace", skip_all, fields(?subscription))]
    pub fn destroy_subscription(&mut self, subscription: SubscriptionId, mut destructor: impl FnMut(RecordId, V)) {
        for record_id in self.subscriptions.remove_all(&subscription) {
            // it is retained by the subscription, so it will not be removed early
            let record = &mut self.records[record_id];
            record.subscriptions.remove(subscription);
            if record.subscriptions.is_empty() && record.children.is_empty() {
                self.remove_up(record_id, &mut destructor);
            }
        }
    }

    #[instrument(level = "trace", skip_all, fields(?canonical_path))]
    pub fn remove_path(&mut self, canonical_path: &Path, destructor: impl FnMut(RecordId, V)) {
        if canonical_path.is_empty_path() {
            self.clear(destructor)
        } else if let Some(present_entry) = self.entry(canonical_path).ok().and_then(|e| e.present()) {
            present_entry.remove_recursively(destructor);
        }
    }

    #[instrument(level = "trace", skip_all, fields(?canonical_path, subscription))]
    pub fn remove_subscriber(
        &mut self,
        canonical_path: &Path,
        subscription: SubscriptionId,
        mut destructor: impl FnMut(RecordId, V),
    ) -> Result<(), IllegalArgument> {
        if let Some(present_entry) = self.entry(canonical_path)?.present() {
            let record_id = present_entry.record_id;
            self.subscriptions.remove(&subscription, &record_id);
            let r = &mut self.records[record_id];
            r.subscriptions.remove(subscription);
            if r.subscriptions.is_empty() && r.children.is_empty() {
                self.remove_recursively(record_id, &mut destructor);
            }
        }
        Ok(())
    }

    #[instrument(level = "trace", skip_all, fields(?canonical_path, subscription))]
    pub fn remove_subscriber_recursively(
        &mut self,
        canonical_path: &Path,
        subscription: SubscriptionId,
        mut destructor: impl FnMut(RecordId, V),
    ) -> Result<(), IllegalArgument> {
        if let Some(present_entry) = self.entry(canonical_path)?.present() {
            let record_id = present_entry.record_id;
            self.unsubscibe_recursively(record_id, subscription, &mut destructor);
        }
        Ok(())
    }

    #[instrument(level = "trace", skip_all)]
    pub fn clear(&mut self, mut destructor: impl FnMut(RecordId, V)) {
        let first_level_records = {
            let mut set = HashSet::<RecordId>::new();
            self.prefixes.for_each(|_, record_id| {
                set.insert(*record_id);
            });
            set
        };
        for record_id in first_level_records {
            self.remove_recursively(record_id, &mut destructor);
        }
    }

    #[instrument(level = "trace", skip_all, fields(?record_id))]
    fn remove_recursively(&mut self, record_id: RecordId, destructor: &mut dyn FnMut(RecordId, V)) {
        fn recur<V>(tree: &mut WatchesTree<V>, record_id: RecordId, destructor: &mut dyn FnMut(RecordId, V)) -> Option<RecordId> {
            let record = tree.records.remove(record_id);
            record.subscriptions.for_each(|subscription_id| {
                tree.subscriptions.remove(&subscription_id, &record_id);
            });
            record.children.for_each(|_, child| {
                recur(tree, *child, destructor);
            });
            destructor(record_id, record.value);
            record.parent
        }
        if let Some(parent_id) = recur(self, record_id, destructor) {
            let parent_record = &mut self.records[parent_id];
            parent_record.children.remove_by_value(&record_id);
            if parent_record.subscriptions.is_empty() && parent_record.children.is_empty() {
                self.remove_up(parent_id, destructor);
            }
        } else {
            self.prefixes.remove_by_value(&record_id);
        }
    }

    fn unsubscibe_recursively(&mut self, record_id: RecordId, subscription_id: SubscriptionId, destructor: &mut dyn FnMut(RecordId, V)) {
        enum Step {
            Push(RecordId),
            Pop(RecordId),
        }
        let parent_id = self.records[record_id].parent;
        let mut stack = vec![Step::Pop(record_id), Step::Push(record_id)];
        while let Some(step) = stack.pop() {
            match step {
                Step::Push(record_id) => {
                    self.records[record_id].children.for_each(|_, child_id| {
                        stack.push(Step::Pop(*child_id));
                        stack.push(Step::Push(*child_id));
                    });
                }
                Step::Pop(record_id) => {
                    let record = &mut self.records[record_id];
                    if record.subscriptions.remove(subscription_id) {
                        self.subscriptions.remove(&subscription_id, &record_id);
                        if record.subscriptions.is_empty() && record.children.is_empty() {
                            let record = self.records.remove(record_id);
                            destructor(record_id, record.value);
                            if let Some(parent_id) = record.parent {
                                self.records[parent_id].children.remove_by_value(&record_id);
                            } else {
                                self.prefixes.remove_by_value(&record_id);
                            }
                        }
                    }
                }
            }
        }
        if let Some(parent_id) = parent_id {
            let parent = &self.records[parent_id];
            if parent.subscriptions.is_empty() && parent.children.is_empty() {
                self.remove_up(parent_id, destructor);
            }
        }
    }

    /// Deletes the record and all its parents that have no retainers of their own.
    fn remove_up(&mut self, record_id: RecordId, destructor: &mut dyn FnMut(RecordId, V)) {
        let record = self.records.remove(record_id);
        destructor(record_id, record.value);
        if let Some(parent_id) = record.parent {
            let parent = &mut self.records[parent_id];
            parent.children.remove_by_value(&record_id);
            if parent.subscriptions.is_empty() && parent.children.is_empty() {
                self.remove_up(parent_id, destructor);
            }
        } else {
            self.prefixes.remove_by_value(&record_id);
        }
    }

    #[cfg(any(test, target_os = "windows"))]
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty() && self.records.is_empty() && self.prefixes.is_empty()
    }

    /// Shrink internal data structures if necessary.
    ///
    /// Calling this method invalidates all previously issued RecordIds.
    /// [map] is invoked for each moved entry with (&V, old_key, new_key)
    #[allow(unused)]
    fn compact(&mut self, mut rekey: impl FnMut(&V, RecordId, RecordId)) {
        let mut mapping = HashMap::<RecordId, RecordId>::new();
        self.records.compact(|v, old, new| {
            mapping.insert(old, new);
            rekey(&v.value, old, new)
        });
        // self.prefixes.map_values(|old| mapping[old]);
        // self.subscriptions.map_values(|old| mapping[old]);

        todo!("prefixes and subscriptions");
    }
}

impl<V> Default for WatchesTree<V> {
    fn default() -> Self {
        Self::empty()
    }
}

pub enum Entry<'t, 'k, V> {
    Present(PresentEntry<'t, V>),
    Absent(AbsentEntry<'t, 'k, V>),
}

impl<'t, 'k, V> Entry<'t, 'k, V> {
    pub fn or_insert_all_with<E>(
        self,
        value_fn: impl FnMut(&Path) -> Result<V, E>,
        key_fn: impl FnMut(RecordId, &V),
    ) -> Result<PresentEntry<'t, V>, E> {
        match self {
            Entry::Present(present_entry) => Ok(present_entry),
            Entry::Absent(absent_entry) => absent_entry.insert_all_with(value_fn, key_fn),
        }
    }

    pub fn present(self) -> Option<PresentEntry<'t, V>> {
        match self {
            Entry::Present(present_entry) => Some(present_entry),
            Entry::Absent(_) => None,
        }
    }

    pub fn self_and_parent_audience(self, mut f: impl FnMut(SubscriptionId)) {
        if let Entry::Present(present) = &self {
            present.tree.records[present.record_id].subscriptions.for_each(&mut f);
        }
        let parent_id = match &self {
            Entry::Present(present) => present.parent_id(),
            Entry::Absent(absent) => absent.parent_id(),
        };
        let tree = match &self {
            Entry::Present(present_entry) => &present_entry.tree,
            Entry::Absent(absent_entry) => &absent_entry.tree,
        };
        if let Some(parent_id) = parent_id {
            tree.records[parent_id].subscriptions.for_each(&mut f);
        }
    }

    pub fn ancestors_audience(self, mut f: impl FnMut(SubscriptionId)) {
        self.on_each_ancestor(|tree, id| {
            tree.records[id].subscriptions.for_each(|s| {
                f(s);
            });
        });
    }

    fn on_each_ancestor(mut self, mut f: impl FnMut(&mut WatchesTree<V>, RecordId)) -> Entry<'t, 'k, V> {
        let mut present_id = match &mut self {
            Entry::Present(present_entry) => Some(present_entry.record_id),
            Entry::Absent(absent_entry) => absent_entry.present_node,
        };
        let tree = match &mut self {
            Entry::Present(present_entry) => &mut present_entry.tree,
            Entry::Absent(absent_entry) => &mut absent_entry.tree,
        };
        while let Some(id) = present_id {
            f(tree, id);
            present_id = tree.records[id].parent;
        }
        self
    }
}

impl<'t, 'k, V: Default> Entry<'t, 'k, V> {
    pub fn or_insert_default(self) -> PresentEntry<'t, V> {
        self.or_insert_all_with::<()>(|_| Ok(Default::default()), |_, _| {}).unwrap()
    }
}

pub struct AbsentEntry<'t, 'p, V> {
    tree: &'t mut WatchesTree<V>,
    full_path: &'p Path,
    present_depth: usize,
    parent: Option<RecordId>,
    present_node: Option<RecordId>,
}

impl<'t, 'p, V> AbsentEntry<'t, 'p, V> {
    pub fn insert_all_with<E>(
        self,
        mut value_fn: impl FnMut(&Path) -> Result<V, E>,
        mut key_fn: impl FnMut(RecordId, &V),
    ) -> Result<PresentEntry<'t, V>, E> {
        let mut current = self.present_node;
        for missing in segments(self.full_path).skip(self.present_depth) {
            let value = value_fn(missing.prefix_inclusive)?;
            let new_id = self.tree.records.insert(Record {
                parent: current,
                value,
                subscriptions: Default::default(),
                children: Default::default(),
            });
            key_fn(new_id, &self.tree.records[new_id].value);
            let name = missing.as_os_str().to_os_string();
            match current {
                Some(parent_id) => {
                    let existing = self.tree.records[parent_id].children.insert(name, new_id);
                    if existing.is_some() {
                        panic!("overwritten existing record at {missing:?}");
                    }
                }
                None => {
                    let existing = self.tree.prefixes.insert(name, new_id);
                    if existing.is_some() {
                        panic!("overwritten existing record {missing:?}");
                    }
                }
            }
            current = Some(new_id);
        }
        Ok(PresentEntry {
            tree: self.tree,
            record_id: current.unwrap_or_else(|| {
                // It should be unreachable unless the path is empty, and this is validated earlier.
                panic!("no nodes were insterd, path: {:?}, depth: {}", self.full_path, self.present_depth)
            }),
        })
    }

    fn parent_id(&self) -> Option<RecordId> {
        self.parent
    }
}

pub struct PresentEntry<'t, V> {
    tree: &'t mut WatchesTree<V>,
    record_id: RecordId,
}

impl<'t, V> PresentEntry<'t, V> {
    pub fn subscribe(&mut self, subscription: SubscriptionId) {
        self.tree.subscriptions.insert(subscription, self.record_id);
        self.tree.records[self.record_id].subscriptions.insert(subscription);
    }

    pub fn remove_recursively(self, mut destructor: impl FnMut(RecordId, V)) {
        self.tree.remove_recursively(self.record_id, &mut destructor);
    }

    /// Navigates up from the current node and returns its canonical path.
    #[cfg_attr(any(target_os = "windows", target_os = "macos"), allow(unused))]
    pub fn collect_path(&self) -> PathBuf {
        let mut components = Vec::<&OsStr>::with_capacity(8);
        let mut current_id = self.record_id;
        loop {
            match self.tree.records[current_id].parent {
                Some(parent_id) => {
                    components.push(
                        self.tree.records[parent_id]
                            .children
                            .name_by_value(&current_id)
                            .expect("parent doesn't know about its child"),
                    );
                    current_id = parent_id;
                }
                None => {
                    let prefix = self.tree.prefixes.name_by_value(&current_id).expect("node is detached from root");
                    components.push(prefix);
                    break components.into_iter().rev().collect();
                }
            }
        }
    }

    #[cfg(test)]
    pub fn collect_subtree(&self, sink: &mut Vec<PathBuf>) {
        fn recur<V>(tree: &WatchesTree<V>, record_id: RecordId, current_path: PathBuf, sink: &mut Vec<PathBuf>) {
            sink.push(current_path.clone());
            tree.records[record_id].children.for_each(|name, child_id| {
                let child_path = current_path.join(name);
                recur(tree, *child_id, child_path, sink);
            });
        }
        recur(self.tree, self.record_id, self.collect_path(), sink);
    }

    fn parent_id(&self) -> Option<RecordId> {
        self.tree.records[self.record_id].parent
    }

    #[cfg_attr(any(target_os = "windows", target_os = "macos"), allow(unused))]
    pub fn value(&self) -> &V {
        &self.tree.records[self.record_id].value
    }
}

#[cfg(test)]
mod test {
    use crate::test_helpers::enable_logging;

    use crate::util::id_source::IdSource;

    use super::*;

    static ID_SOURCE: IdSource = IdSource::new();

    fn subscription_id() -> SubscriptionId {
        SubscriptionId::from_non_zero_u32(ID_SOURCE.next())
    }

    fn insert<'t, 'p: 't>(t: &'t mut WatchesTree<()>, path: &'p Path) -> PresentEntry<'t, ()> {
        t.entry(path).expect("illegal argument").or_insert_default()
    }

    #[test]
    fn insert_root() {
        enable_logging();
        let mut t = WatchesTree::<()>::empty();
        insert(&mut t, Path::new("/"));
        assert!(t.entry(Path::new("/")).unwrap().present().is_some());
    }

    #[test]
    fn insert_relative_path() {
        enable_logging();
        let mut t = WatchesTree::<()>::empty();
        insert(&mut t, Path::new("a/b"));

        assert!(t.entry(Path::new("a/b")).unwrap().present().is_some());
    }

    #[test]
    fn remove_root_path() {
        enable_logging();

        let mut t = WatchesTree::<()>::empty();
        let subscription_id = subscription_id();
        insert(&mut t, Path::new("/a/b/c")).subscribe(subscription_id);
        insert(&mut t, Path::new("/a/b/d")).subscribe(subscription_id);
        insert(&mut t, Path::new("/a/b/e")).subscribe(subscription_id);
        insert(&mut t, Path::new("/a")).subscribe(subscription_id);
        insert(&mut t, Path::new("/b/c/d/e")).subscribe(subscription_id);
        t.remove_path(Path::new("/"), |_, _| {});
        assert!(t.is_empty(), "tree is not empty {t:?}");
    }

    #[test]
    fn remove_last_retainer_of_a_path() {
        enable_logging();
        let mut t = WatchesTree::<()>::empty();
        let subscription_id = subscription_id();
        insert(&mut t, Path::new("/a/b/c")).subscribe(subscription_id);
        t.remove_path(Path::new("/a/b/c"), |_, _| {});
        assert!(t.is_empty(), "tree is not empty {t:#?}");
    }

    #[test]
    fn remove_subscriber_removes_last_retainer_and_prunes_ancestors() {
        enable_logging();

        let mut t = WatchesTree::<()>::empty();
        let subscription_id = subscription_id();
        insert(&mut t, Path::new("/a/b/c")).subscribe(subscription_id);

        let mut removed = Vec::new();
        t.remove_subscriber(Path::new("/a/b/c"), subscription_id, |record_id, _| {
            removed.push(record_id);
        }).unwrap();

        assert!(t.is_empty(), "tree is not empty {t:#?}");
        assert_eq!(
            removed.len(),
            4,
            "expected removed leaf, empty ancestors, and empty absolute-path prefix to be destroyed"
        );
    }

    #[test]
    fn remove_subscriber_stops_pruning_at_first_ancestor_retained_by_different_subscription() {
        enable_logging();

        let mut t = WatchesTree::<()>::empty();
        let removed_subscription = subscription_id();
        let retained_subscription = subscription_id();
        insert(&mut t, Path::new("/a")).subscribe(retained_subscription);
        insert(&mut t, Path::new("/a/b/c")).subscribe(removed_subscription);

        let mut removed = Vec::new();
        t.remove_subscriber(Path::new("/a/b/c"), removed_subscription, |record_id, _| {
            removed.push(record_id);
        }).unwrap();

        assert_eq!(removed.len(), 2, "expected only the leaf and first empty ancestor to be destroyed");
        assert!(
            t.entry(Path::new("/a")).unwrap().present().is_some(),
            "pruning should stop at retained ancestor"
        );
        assert!(
            t.entry(Path::new("/a/b")).unwrap().present().is_none(),
            "unretained intermediate branch should be removed"
        );
        assert!(
            t.entry(Path::new("/a/b/c")).unwrap().present().is_none(),
            "removed leaf should be absent"
        );
    }

    #[test]
    fn remove_subscriber_keeps_node_retained_by_other_subscriber() {
        enable_logging();

        let mut t = WatchesTree::<()>::empty();
        let first_subscription = subscription_id();
        let second_subscription = subscription_id();
        insert(&mut t, Path::new("/a/b/c")).subscribe(first_subscription);
        insert(&mut t, Path::new("/a/b/c")).subscribe(second_subscription);

        let mut removed = Vec::new();
        t.remove_subscriber(Path::new("/a/b/c"), first_subscription, |record_id, _| {
            removed.push(record_id);
        }).unwrap();

        assert!(removed.is_empty(), "retained node should not be destroyed");
        assert!(t.entry(Path::new("/a/b/c")).unwrap().present().is_some());
    }

    #[test]
    fn remove_subscriber_recursively_removes_subtree_and_prunes_hanging_branches() {
        enable_logging();

        let mut t = WatchesTree::<()>::empty();
        let removed_subscription = subscription_id();
        let retained_subscription = subscription_id();
        insert(&mut t, Path::new("/a/b/c")).subscribe(removed_subscription);
        insert(&mut t, Path::new("/a/b/c/d")).subscribe(removed_subscription);
        insert(&mut t, Path::new("/a/b/c/e")).subscribe(removed_subscription);
        insert(&mut t, Path::new("/a/b/c/e/f")).subscribe(removed_subscription);
        insert(&mut t, Path::new("/x/y")).subscribe(retained_subscription);

        let mut removed = Vec::new();
        t.remove_subscriber_recursively(Path::new("/a/b/c"), removed_subscription, |record_id, _| {
            removed.push(record_id);
        }).unwrap();

        assert_eq!(
            removed.len(),
            6,
            "expected recursive subtree removal plus pruning of now-empty ancestors"
        );
        assert!(
            t.entry(Path::new("/a")).unwrap().present().is_none(),
            "empty ancestor branch should be pruned"
        );
        assert!(
            t.entry(Path::new("/a/b")).unwrap().present().is_none(),
            "empty ancestor branch should be pruned"
        );
        assert!(
            t.entry(Path::new("/a/b/c")).unwrap().present().is_none(),
            "removed subtree should be absent"
        );
        assert!(
            t.entry(Path::new("/a/b/c/d")).unwrap().present().is_none(),
            "descendant should be removed recursively"
        );
        assert!(
            t.entry(Path::new("/a/b/c/e")).unwrap().present().is_none(),
            "intermediate descendant branch should be removed recursively"
        );
        assert!(
            t.entry(Path::new("/a/b/c/e/f")).unwrap().present().is_none(),
            "deep descendant should be removed recursively"
        );
        assert!(
            t.entry(Path::new("/x/y")).unwrap().present().is_some(),
            "unrelated branch should stay intact"
        );
    }

    #[test]
    fn remove_subscriber_recursively_removes_subtree_and_root_when_nothing_else_is_retained() {
        enable_logging();

        let mut t = WatchesTree::<()>::empty();
        let removed_subscription = subscription_id();
        insert(&mut t, Path::new("/a/b/c")).subscribe(removed_subscription);
        insert(&mut t, Path::new("/a/b/c/d")).subscribe(removed_subscription);
        insert(&mut t, Path::new("/a/b/c/e")).subscribe(removed_subscription);
        insert(&mut t, Path::new("/a/b/c/e/f")).subscribe(removed_subscription);

        let mut removed = Vec::new();
        t.remove_subscriber_recursively(Path::new("/a/b/c"), removed_subscription, |record_id, _| {
            removed.push(record_id);
        }).unwrap();

        assert_eq!(
            removed.len(),
            7,
            "expected recursive subtree removal plus pruning of all now-empty ancestors including root"
        );
        assert!(
            t.is_empty(),
            "tree should be empty after removing the only retained root branch: {t:#?}"
        );
    }

    #[test]
    fn remove_subscriber_recursively_keeps_nodes_retained_by_other_subscriber() {
        enable_logging();

        let mut t = WatchesTree::<()>::empty();
        let first_subscription = subscription_id();
        let second_subscription = subscription_id();
        insert(&mut t, Path::new("/a/b")).subscribe(first_subscription);
        insert(&mut t, Path::new("/a/b/c")).subscribe(first_subscription);
        insert(&mut t, Path::new("/a/b/c")).subscribe(second_subscription);

        let mut removed = Vec::new();
        t.remove_subscriber_recursively(Path::new("/a/b"), first_subscription, |record_id, _| {
            removed.push(record_id);
        }).unwrap();

        assert!(removed.is_empty(), "retained subtree should not be destroyed");
        assert!(t.entry(Path::new("/a/b")).unwrap().present().is_some());
        assert!(t.entry(Path::new("/a/b/c")).unwrap().present().is_some());
    }

    #[test]
    fn collect_path() {
        enable_logging();

        let mut t = WatchesTree::<()>::empty();

        // Insert a simple path
        let entry = insert(&mut t, Path::new("/a/b/c"));
        let collected = entry.collect_path();
        assert_eq!(collected, Path::new("/a/b/c"), "simple path should match");

        // Get existing intermediate node
        let entry = t.entry(Path::new("/a/b")).unwrap().present().unwrap();
        let collected = entry.collect_path();
        assert_eq!(collected, Path::new("/a/b"), "intermediate path should match");
    }
}
