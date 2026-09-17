use std::collections::HashSet;
use std::ffi::OsStr;
use std::hash::Hash;
use std::path::Path;

use crate::util::{minislab, names::Names, path_util::head};

minislab::key!(pub RecordId);

/// Stores all registered symbolic paths in their original form.
///
/// Strictly speaking, canonical_tree already contains this information,
/// but navigating it is trickier, especially the pruning part.
pub(crate) struct SymbolicTree<V> {
    root: RecordId,
    records: minislab::Slab<RecordId, SymbolicRecord<V>>,
}

#[derive(Debug)]
struct SymbolicRecord<V> {
    value: Option<V>,
    children: Names<RecordId>,
}

impl<V> SymbolicTree<V> {
    pub fn new() -> Self {
        let mut records = minislab::Slab::new();
        let root = records.insert(SymbolicRecord {
            value: None,
            children: Names::new(),
        });
        SymbolicTree { root, records }
    }

    pub fn get_exact(&self, symbolic_path: &Path) -> Option<&V> {
        let mut current_id = self.root;
        let mut current_tail = symbolic_path;
        while let Some((head, tail)) = head(current_tail) {
            let record = &self.records[current_id];
            if let Some(next_id) = record.children.get(&head) {
                current_tail = tail;
                current_id = *next_id;
            } else {
                return None;
            }
        }
        self.records[current_id].value.as_ref()
    }

    pub fn insert(&mut self, symbolic_path: &Path, value: V) -> Option<V> {
        let mut current_id = self.root;
        let mut current_tail = symbolic_path;
        while let Some((head, tail)) = head(current_tail) {
            let record = &self.records[current_id];
            if let Some(next_id) = record.children.get(&head) {
                current_id = *next_id;
            } else {
                let child = self.records.insert(SymbolicRecord {
                    value: None,
                    children: Names::new(),
                });
                self.records[current_id].children.insert(head.to_os_string(), child);
                current_id = child;
            }
            current_tail = tail;
        }
        self.records[current_id].value.replace(value)
    }

    /// Returns the value stored at the closest ancestor (or exact match) of `symbolic_path`.
    ///
    /// Walks the tree as deep as the path components match, and returns the last value encountered.
    pub fn get_closest(&self, symbolic_path: &Path) -> Option<&V> {
        let mut current_id = self.root;
        let mut last_value_id: Option<RecordId> = None;
        let mut current_tail = symbolic_path;
        while let Some((name, tail)) = head(current_tail) {
            match self.records[current_id].children.get(&name) {
                Some(&next_id) => {
                    current_id = next_id;
                    if self.records[current_id].value.is_some() {
                        last_value_id = Some(current_id);
                    }
                    current_tail = tail;
                }
                None => break,
            }
        }
        last_value_id.and_then(|id| self.records[id].value.as_ref())
    }

    /// Prunes the subtree at `symbolic_path`, feeding every key under it to `f` and removing it when
    /// `f` returns true.
    ///
    /// While descending from the root to `symbolic_path`, `descend` is consulted for each *strict
    /// ancestor* value. If it returns false, the descent is aborted and the tree is left untouched.
    pub fn prune(
        &mut self,
        symbolic_path: &Path,
        mut descend: impl FnMut(&V) -> bool,
        mut f: impl FnMut(&V) -> bool,
    ) -> Option<HashSet<V>>
    where
        V: Eq + Hash,
    {
        // Walk to the node at symbolic_path
        let mut current = self.root;
        let mut ancestors: Vec<(RecordId, Box<OsStr>)> = Vec::new();
        let mut current_tail = symbolic_path;
        while let Some((name, tail)) = head(current_tail) {
            // `current` is a strict ancestor of the target node here
            if let Some(v) = &self.records[current].value {
                if !descend(v) {
                    return None;
                }
            }
            let record = &self.records[current];
            match record.children.get(&name) {
                Some(&child) => {
                    ancestors.push((current, name.into()));
                    current = child;
                }
                None => return Some(HashSet::new()),
            }
            current_tail = tail;
        }

        let mut result = HashSet::new();
        self.prune_subtree(current, &mut f, &mut result);

        // Clean up empty ancestors bottom-up
        for (parent, name) in ancestors.into_iter().rev() {
            let child_id = match self.records[parent].children.get(&name) {
                Some(&id) => id,
                None => continue,
            };
            let child = &self.records[child_id];
            if child.value.is_none() && child.children.is_empty() {
                self.records[parent].children.remove(&name);
                self.records.remove(child_id);
            }
        }

        Some(result)
    }

    fn prune_subtree(&mut self, node: RecordId, f: &mut impl FnMut(&V) -> bool, result: &mut HashSet<V>)
    where
        V: Eq + Hash,
    {
        // Collect child ids first to avoid borrow issues
        let mut child_ids = Vec::new();
        self.records[node].children.for_each(|name, &id| {
            child_ids.push((name.to_os_string(), id));
        });

        // Recurse into children
        for (_, child_id) in &child_ids {
            self.prune_subtree(*child_id, f, result);
        }

        // Remove empty children
        for (name, child_id) in &child_ids {
            let child = &self.records[*child_id];
            if child.value.is_none() && child.children.is_empty() {
                self.records[node].children.remove(name.as_os_str());
                self.records.remove(*child_id);
            }
        }

        // Process this node's value
        if let Some(ref v) = self.records[node].value {
            if f(v) {
                result.insert(self.records[node].value.take().unwrap());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use std::collections::HashSet;

    use super::SymbolicTree;

    #[test]
    fn get_from_empty_tree() {
        let tree: SymbolicTree<i32> = SymbolicTree::new();
        assert_eq!(tree.get_exact(Path::new("/a/b")), None);
    }

    #[test]
    fn insert_and_get() {
        let mut tree = SymbolicTree::new();
        assert_eq!(tree.insert(Path::new("/a/b"), 1), None);
        assert_eq!(tree.get_exact(Path::new("/a/b")), Some(&1));
    }

    #[test]
    fn insert_overwrite_returns_previous() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a/b"), 1);
        assert_eq!(tree.insert(Path::new("/a/b"), 2), Some(1));
        assert_eq!(tree.get_exact(Path::new("/a/b")), Some(&2));
    }

    #[test]
    fn get_nonexistent_path() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a/b"), 1);
        assert_eq!(tree.get_exact(Path::new("/a/c")), None);
        assert_eq!(tree.get_exact(Path::new("/a")), None);
        assert_eq!(tree.get_exact(Path::new("/a/b/c")), None);
    }

    #[test]
    fn sibling_paths() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a/b"), 1);
        tree.insert(Path::new("/a/c"), 2);
        assert_eq!(tree.get_exact(Path::new("/a/b")), Some(&1));
        assert_eq!(tree.get_exact(Path::new("/a/c")), Some(&2));
    }

    #[test]
    fn nested_paths_parent_and_child_both_have_values() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a"), 1);
        tree.insert(Path::new("/a/b"), 2);
        assert_eq!(tree.get_exact(Path::new("/a")), Some(&1));
        assert_eq!(tree.get_exact(Path::new("/a/b")), Some(&2));
    }

    #[test]
    fn prune_removes_subtree() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a/b"), 1);
        tree.insert(Path::new("/a/b/c"), 2);
        tree.insert(Path::new("/a/b/d"), 3);
        tree.insert(Path::new("/a/e"), 4);

        let pruned = tree.prune(Path::new("/a/b"), |_| true, |_| true).unwrap();
        assert_eq!(pruned, HashSet::from([1, 2, 3]));
        // sibling is untouched
        assert_eq!(tree.get_exact(Path::new("/a/e")), Some(&4));
        // pruned entries are gone
        assert_eq!(tree.get_exact(Path::new("/a/b")), None);
        assert_eq!(tree.get_exact(Path::new("/a/b/c")), None);
    }

    #[test]
    fn prune_with_filter() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a/b"), 1);
        tree.insert(Path::new("/a/c"), 2);

        // only prune even values
        let pruned = tree.prune(Path::new("/a"), |_| true, |v| v % 2 == 0).unwrap();
        assert_eq!(pruned, HashSet::from([2]));
        // odd value survives
        assert_eq!(tree.get_exact(Path::new("/a/b")), Some(&1));
        assert_eq!(tree.get_exact(Path::new("/a/c")), None);
    }

    #[test]
    fn prune_nonexistent_path_returns_empty() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a/b"), 1);
        let pruned = tree.prune(Path::new("/x/y"), |_| true, |_| true).unwrap();
        assert!(pruned.is_empty());
        // original data untouched
        assert_eq!(tree.get_exact(Path::new("/a/b")), Some(&1));
    }

    #[test]
    fn prune_cleans_up_empty_ancestors() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a/b/c"), 1);

        tree.prune(Path::new("/a/b/c"), |_| true, |_| true).unwrap();
        // intermediate nodes should be cleaned up, so inserting a new
        // value at a sibling path should work and the old path is gone
        assert_eq!(tree.get_exact(Path::new("/a/b/c")), None);
        assert_eq!(tree.get_exact(Path::new("/a/b")), None);
        assert_eq!(tree.get_exact(Path::new("/a")), None);
    }

    #[test]
    fn prune_preserves_ancestor_with_value() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a"), 1);
        tree.insert(Path::new("/a/b"), 2);

        let pruned = tree.prune(Path::new("/a/b"), |_| true, |_| true).unwrap();
        assert_eq!(pruned, HashSet::from([2]));
        // ancestor with its own value is preserved
        assert_eq!(tree.get_exact(Path::new("/a")), Some(&1));
    }

    #[test]
    fn prune_at_root_removes_everything() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a"), 1);
        tree.insert(Path::new("/b"), 2);
        tree.insert(Path::new("/a/c"), 3);

        let pruned = tree.prune(Path::new("/"), |_| true, |_| true).unwrap();
        assert_eq!(pruned, HashSet::from([1, 2, 3]));
        assert_eq!(tree.get_exact(Path::new("/a")), None);
        assert_eq!(tree.get_exact(Path::new("/b")), None);
    }

    #[test]
    fn multiple_inserts_at_different_depths() {
        let mut tree = SymbolicTree::new();
        tree.insert(Path::new("/a"), 1);
        tree.insert(Path::new("/a/b/c/d"), 2);
        tree.insert(Path::new("/a/b"), 3);

        assert_eq!(tree.get_exact(Path::new("/a")), Some(&1));
        assert_eq!(tree.get_exact(Path::new("/a/b")), Some(&3));
        assert_eq!(tree.get_exact(Path::new("/a/b/c")), None);
        assert_eq!(tree.get_exact(Path::new("/a/b/c/d")), Some(&2));
    }
}
