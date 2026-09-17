mod watches_tree;

use std::{collections::HashSet, num::NonZeroU32, path::Path};

use super::{BackendError, ParentPolicy};

pub use watches_tree::{IllegalArgument, RecordId, WatchesTree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriptionId(NonZeroU32);

impl SubscriptionId {
    pub fn from_u32(u: u32) -> SubscriptionId {
        SubscriptionId(NonZeroU32::new(u + 1).expect("invalid input for SubscriptionId"))
    }

    pub fn from_non_zero_u32(u: NonZeroU32) -> SubscriptionId {
        SubscriptionId(u)
    }
}

#[derive(Debug)]
pub struct Watches<D, R> {
    pub direct: WatchesTree<D>,
    pub recursive: WatchesTree<R>,
}

impl<D, R> Watches<D, R> {
    fn retained_recursively(&mut self, canonical_path: &Path, subscription_id: SubscriptionId) -> Result<bool, IllegalArgument> {
        let mut found = false;
        self.recursive.entry(canonical_path)?.ancestors_audience(|s| {
            if s == subscription_id {
                found = true;
            }
        });
        Ok(found)
    }

    #[cfg(test)]
    pub fn records(&mut self, canonical_path: &Path) -> Vec<std::path::PathBuf> {
        let mut result = Vec::new();
        if let Some(present) = self.direct.entry(canonical_path).expect("illegal argument").present() {
            present.collect_subtree(&mut result)
        }
        if let Some(present) = self.recursive.entry(canonical_path).expect("illegal argument").present() {
            present.collect_subtree(&mut result)
        }
        result
    }

    /// Enforces `parent_policy` for a watch about to be attached at `canonical_path`.
    ///
    /// [`ParentPolicy::Unchecked`] always passes. [`ParentPolicy::RequireWatchedParent`] passes only
    /// when events about `canonical_path` already reach `subscription_id` through an existing watch
    /// (a direct watch on the parent, or a recursive watch on the path or an ancestor). A root prefix
    /// (a path without a parent) has nothing to anchor to and is always accepted.
    ///
    /// Must be called *before* the new watch is registered, otherwise the path would trivially
    /// satisfy the check through the watch being added.
    pub fn check_parent_policy(
        &mut self,
        canonical_path: &Path,
        subscription_id: SubscriptionId,
        parent_policy: ParentPolicy,
    ) -> Result<(), BackendError> {
        match parent_policy {
            ParentPolicy::Unchecked => Ok(()),
            ParentPolicy::RequireWatchedParent if canonical_path.parent().is_none() => Ok(()),
            ParentPolicy::RequireWatchedParent => {
                if self.query(canonical_path).expect("illegal argument").contains(&subscription_id) {
                    Ok(())
                } else {
                    Err(BackendError::DetachedParent)
                }
            }
        }
    }

    pub fn query(&mut self, canonical_path: &Path) -> Result<HashSet<SubscriptionId>, IllegalArgument> {
        let mut set = HashSet::new();
        self.direct.entry(canonical_path)?.self_and_parent_audience(|s| {
            set.insert(s);
        });
        self.recursive.entry(canonical_path)?.ancestors_audience(|s| {
            set.insert(s);
        });

        Ok(set)
    }

    /// Removes a subscriber from one of the watch trees and tears down direct watches only when
    /// the same subscriber has no recursive watch on the same path or any ancestor.
    pub fn remove_subscriber(
        &mut self,
        canonical_path: &Path,
        subscription_id: SubscriptionId,
        recursive: bool,
        mut direct_destructor: impl FnMut(RecordId, D),
        mut recursive_destructor: impl FnMut(RecordId, R),
    ) -> Result<(), IllegalArgument> {
        if recursive {
            self.recursive
                .remove_subscriber_recursively(canonical_path, subscription_id, &mut recursive_destructor)?;
            if !self.retained_recursively(canonical_path, subscription_id)? {
                self.direct
                    .remove_subscriber_recursively(canonical_path, subscription_id, &mut direct_destructor)?;
            }
        } else if !self.retained_recursively(canonical_path, subscription_id)? {
            self.direct.remove_subscriber(canonical_path, subscription_id, &mut direct_destructor)?;
        }

        Ok(())
    }

    #[cfg(any(test, target_os = "windows"))]
    pub fn is_empty(&self) -> bool {
        self.direct.is_empty() && self.recursive.is_empty()
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", test))]
impl Watches<(), ()> {
    pub fn destroy_subscription(&mut self, subscription: SubscriptionId) {
        self.direct.destroy_subscription(subscription, |_, _| {});
        self.recursive.destroy_subscription(subscription, |_, _| {});
    }

    pub fn remove_path(&mut self, canonical_path: &Path) {
        self.direct.remove_path(canonical_path, |_, _| {});
        self.recursive.remove_path(canonical_path, |_, _| {});
    }
}

impl<D, R> Default for Watches<D, R> {
    fn default() -> Self {
        Self {
            direct: Default::default(),
            recursive: Default::default(),
        }
    }
}

#[cfg(test)]
mod test {
    use crate::test_helpers::enable_logging;
    use watches_tree::PresentEntry;

    use crate::util::id_source::IdSource;

    use super::*;

    static ID_SOURCE: IdSource = IdSource::new();

    fn subscription_id() -> SubscriptionId {
        SubscriptionId::from_non_zero_u32(ID_SOURCE.next())
    }

    fn insert<'t, 'p: 't>(t: &'t mut WatchesTree<()>, path: &'p Path) -> PresentEntry<'t, ()> {
        t.entry(path).expect("illegal argument").or_insert_default()
    }

    fn query_vec(t: &mut Watches<(), ()>, path: &Path) -> Vec<SubscriptionId> {
        let mut vec = t.query(path).expect("illegal argument").into_iter().collect::<Vec<_>>();
        vec.sort_by(|s1, s2| s1.0.cmp(&s2.0));
        vec
    }

    fn present<V>(tree: &mut WatchesTree<V>, path: &Path) -> bool {
        tree.entry(path).expect("illegal argument").present().is_some()
    }

    #[test]
    fn insert_root() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let single_id = subscription_id();
        insert(&mut t.direct, Path::new("/")).subscribe(single_id);

        let ss = query_vec(&mut t, Path::new("/"));
        assert_eq!(vec![single_id], ss);

        let ss = query_vec(&mut t, Path::new("/1"));
        assert_eq!(vec![single_id], ss);

        let ss = query_vec(&mut t, Path::new("/1/2"));
        assert_eq!(Vec::<SubscriptionId>::new(), ss);
    }

    #[test]
    fn query_direct_children() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let s1 = subscription_id();
        let s2 = subscription_id();
        insert(&mut t.direct, Path::new("/1")).subscribe(s1);
        insert(&mut t.direct, Path::new("/1/2/3")).subscribe(s2);

        let ss = query_vec(&mut t, Path::new("/"));
        assert_eq!(Vec::<SubscriptionId>::new(), ss);

        let ss = query_vec(&mut t, Path::new("/1"));
        assert_eq!(vec![s1], ss);

        let ss = query_vec(&mut t, Path::new("/1/2"));
        assert_eq!(vec![s1], ss);

        let ss = query_vec(&mut t, Path::new("/1/2/3"));
        assert_eq!(vec![s2], ss);
    }

    #[test]
    fn query_direct_children_on_leaf_node() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription_id = subscription_id();
        insert(&mut t.direct, Path::new("/1")).subscribe(subscription_id);

        let ss = query_vec(&mut t, Path::new("/"));
        assert_eq!(Vec::<SubscriptionId>::new(), ss);

        let ss = query_vec(&mut t, Path::new("/1"));
        assert_eq!(vec![subscription_id], ss);

        let ss = query_vec(&mut t, Path::new("/1/2"));
        assert_eq!(vec![subscription_id], ss);

        let ss = query_vec(&mut t, Path::new("/1/2/3"));
        assert_eq!(Vec::<SubscriptionId>::new(), ss);
    }

    #[test]
    fn query_recursive() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription_id = subscription_id();
        insert(&mut t.recursive, Path::new("/1")).subscribe(subscription_id);

        let ss = query_vec(&mut t, Path::new("/"));
        assert_eq!(Vec::<SubscriptionId>::new(), ss);

        let ss = query_vec(&mut t, Path::new("/1"));
        assert_eq!(vec![subscription_id], ss);

        let ss = query_vec(&mut t, Path::new("/1/2"));
        assert_eq!(vec![subscription_id], ss);

        let ss = query_vec(&mut t, Path::new("/1/2/3"));
        assert_eq!(vec![subscription_id], ss);
    }

    #[test]
    fn clear_last_subscription() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription_id = subscription_id();

        insert(&mut t.direct, Path::new("/1/2/3/4")).subscribe(subscription_id);
        insert(&mut t.recursive, Path::new("/1/2/3/4/5")).subscribe(subscription_id);
        insert(&mut t.recursive, Path::new("/1/2/3/4/5'")).subscribe(subscription_id);
        insert(&mut t.recursive, Path::new("/1/2'/3/4/5")).subscribe(subscription_id);

        t.destroy_subscription(subscription_id);

        assert!(t.is_empty(), "tree is not empty: {t:?}");
    }

    #[test]
    fn clear_subscription_when_there_are_several() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription1 = subscription_id();
        let subscription2 = subscription_id();
        let subscription3 = subscription_id();

        // Set up a tree with multiple subscriptions watching different nodes
        // subscription1 watches /a/b/c recursively
        insert(&mut t.recursive, Path::new("/a/b/c")).subscribe(subscription1);
        // subscription2 watches /a/b (shared parent with subscription1)
        insert(&mut t.direct, Path::new("/a/b")).subscribe(subscription2);
        // subscription3 watches /a/b/c/d (child of subscription1's node)
        insert(&mut t.direct, Path::new("/a/b/c/d")).subscribe(subscription3);

        // Verify initial state - all subscriptions should be returned for their respective paths
        assert_eq!(vec![subscription1, subscription2], query_vec(&mut t, Path::new("/a/b/c")));
        assert_eq!(vec![subscription1, subscription3], query_vec(&mut t, Path::new("/a/b/c/d")),);
        assert_eq!(vec![subscription2], query_vec(&mut t, Path::new("/a/b")));

        // Clear subscription1 - nodes should remain because subscription2 and subscription3 still watch them
        t.destroy_subscription(subscription1);

        // Verify subscription1 is gone but others remain
        assert_eq!(vec![subscription2], query_vec(&mut t, Path::new("/a/b/c")));
        assert_eq!(vec![subscription3], query_vec(&mut t, Path::new("/a/b/c/d")));
        assert_eq!(vec![subscription2], query_vec(&mut t, Path::new("/a/b")));

        // Clear subscription3 - /a/b/c/d should be removed since no one else watches it
        t.destroy_subscription(subscription3);

        // Verify subscription3 is gone and its exclusive node is removed
        assert_eq!(Vec::<SubscriptionId>::new(), query_vec(&mut t, Path::new("/a/b/c/d")));
        assert_eq!(vec![subscription2], query_vec(&mut t, Path::new("/a/b/c")),);

        // Clear subscription2 - now everything should be cleaned up
        t.destroy_subscription(subscription2);

        // Tree should be completely empty now
        assert!(t.is_empty(), "tree is not empty after clearing all subscriptions: {t:#?}");
    }

    #[test]
    fn remove_by_path() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription_id = subscription_id();
        insert(&mut t.recursive, Path::new("/a/b/c")).subscribe(subscription_id);
        insert(&mut t.recursive, Path::new("/a/b/c/d")).subscribe(subscription_id);
        insert(&mut t.recursive, Path::new("/a/b/e")).subscribe(subscription_id);
        t.remove_path(Path::new("/a/b"));
        assert!(
            t.is_empty(),
            "remaining nodes are not retained by any subscribers, should be empty: {:#?}",
            t
        );
    }

    #[test]
    fn remove_recursive_subscriber_without_recursive_parent_removes_matching_direct_watches() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription = subscription_id();
        insert(&mut t.recursive, Path::new("/a/b")).subscribe(subscription);
        insert(&mut t.direct, Path::new("/a/b")).subscribe(subscription);
        insert(&mut t.direct, Path::new("/a/b/c")).subscribe(subscription);
        insert(&mut t.direct, Path::new("/a/b/d")).subscribe(subscription);

        let mut direct_removed = Vec::new();
        let mut recursive_removed = Vec::new();
        t.remove_subscriber(
            Path::new("/a/b"),
            subscription,
            true,
            |record_id, _| direct_removed.push(record_id),
            |record_id, _| recursive_removed.push(record_id),
        )
        .expect("illegal argument");

        assert_eq!(5, direct_removed.len(), "direct subtree should be destroyed");
        assert_eq!(3, recursive_removed.len(), "recursive watch branch should be destroyed");
        assert!(t.is_empty(), "all matching watches should be removed: {t:#?}");
    }

    #[test]
    fn remove_recursive_subscriber_with_recursive_parent_keeps_direct_watches() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription = subscription_id();
        insert(&mut t.recursive, Path::new("/a")).subscribe(subscription);
        insert(&mut t.recursive, Path::new("/a/b")).subscribe(subscription);
        insert(&mut t.direct, Path::new("/a")).subscribe(subscription);
        insert(&mut t.direct, Path::new("/a/b")).subscribe(subscription);
        insert(&mut t.direct, Path::new("/a/b/c")).subscribe(subscription);

        let mut direct_removed = Vec::new();
        let mut recursive_removed = Vec::new();
        t.remove_subscriber(
            Path::new("/a/b"),
            subscription,
            true,
            |record_id, _| direct_removed.push(record_id),
            |record_id, _| recursive_removed.push(record_id),
        )
        .expect("illegal argument");

        assert!(direct_removed.is_empty(), "parent recursive watch should retain direct subtree");
        assert_eq!(1, recursive_removed.len(), "only the removed recursive node should be destroyed");
        assert!(present(&mut t.recursive, Path::new("/a")), "parent recursive watch should remain");
        assert!(present(&mut t.direct, Path::new("/a/b/c")), "direct subtree should stay intact");
        assert_eq!(vec![subscription], query_vec(&mut t, Path::new("/a/b/c")));
    }

    #[test]
    fn remove_direct_subscriber_with_same_path_recursive_watch_keeps_direct_nodes() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription = subscription_id();
        insert(&mut t.recursive, Path::new("/a/b")).subscribe(subscription);
        insert(&mut t.direct, Path::new("/a/b")).subscribe(subscription);

        let mut direct_removed = Vec::new();
        let mut recursive_removed = Vec::new();
        t.remove_subscriber(
            Path::new("/a/b"),
            subscription,
            false,
            |record_id, _| direct_removed.push(record_id),
            |record_id, _| recursive_removed.push(record_id),
        )
        .expect("illegal argument");

        assert!(direct_removed.is_empty(), "recursive watch should retain the direct node");
        assert!(recursive_removed.is_empty(), "recursive tree should stay untouched");
        assert!(present(&mut t.direct, Path::new("/a/b")), "direct node should remain");
        assert_eq!(vec![subscription], query_vec(&mut t, Path::new("/a/b")));
    }

    #[test]
    fn remove_direct_subscriber_without_recursive_watch_destroys_only_that_direct_watch() {
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let subscription = subscription_id();
        insert(&mut t.direct, Path::new("/a")).subscribe(subscription);
        insert(&mut t.direct, Path::new("/a/b")).subscribe(subscription);

        let mut direct_removed = Vec::new();
        let mut recursive_removed = Vec::new();
        t.remove_subscriber(
            Path::new("/a/b"),
            subscription,
            false,
            |record_id, _| direct_removed.push(record_id),
            |record_id, _| recursive_removed.push(record_id),
        )
        .expect("illegal argument");

        assert_eq!(1, direct_removed.len(), "only the removed direct watch should be destroyed");
        assert!(recursive_removed.is_empty(), "recursive tree should stay untouched");
        assert!(
            present(&mut t.direct, Path::new("/a")),
            "retained parent direct watch should remain"
        );
        assert!(!present(&mut t.direct, Path::new("/a/b")), "removed direct watch should disappear");
        assert_eq!(vec![subscription], query_vec(&mut t, Path::new("/a")));
    }

    #[test]
    fn check_parent_policy_enforces_watched_parent() {
        use crate::backend::{BackendError, ParentPolicy};
        enable_logging();

        let mut t = Watches::<(), ()>::default();
        let sub = subscription_id();
        let other = subscription_id();

        // `Unchecked` always passes, even with nothing watched.
        t.check_parent_policy(Path::new("/a/b"), sub, ParentPolicy::Unchecked)
            .expect("Unchecked must always pass");

        // A root prefix has no parent to anchor to and is always accepted.
        t.check_parent_policy(Path::new("/"), sub, ParentPolicy::RequireWatchedParent)
            .expect("a root prefix must be exempt");

        // With nothing watching the parent, `RequireWatchedParent` is rejected.
        assert!(matches!(
            t.check_parent_policy(Path::new("/a/b"), sub, ParentPolicy::RequireWatchedParent),
            Err(BackendError::DetachedParent)
        ));

        // A parent watched by a *different* subscription does not satisfy the check.
        insert(&mut t.direct, Path::new("/a")).subscribe(other);
        assert!(matches!(
            t.check_parent_policy(Path::new("/a/b"), sub, ParentPolicy::RequireWatchedParent),
            Err(BackendError::DetachedParent)
        ));

        // A direct watch on the parent by the same subscription satisfies the check.
        insert(&mut t.direct, Path::new("/a")).subscribe(sub);
        t.check_parent_policy(Path::new("/a/b"), sub, ParentPolicy::RequireWatchedParent)
            .expect("a directly watched parent must satisfy the check");

        // A recursive watch on an ancestor covers deep descendants.
        insert(&mut t.recursive, Path::new("/x")).subscribe(sub);
        t.check_parent_policy(Path::new("/x/y/z"), sub, ParentPolicy::RequireWatchedParent)
            .expect("a recursive ancestor must satisfy the check");

        // ...but a *direct* watch on an ancestor only covers its immediate children, not grandchildren.
        insert(&mut t.direct, Path::new("/p")).subscribe(sub);
        assert!(matches!(
            t.check_parent_policy(Path::new("/p/q/r"), sub, ParentPolicy::RequireWatchedParent),
            Err(BackendError::DetachedParent)
        ));
    }
}
