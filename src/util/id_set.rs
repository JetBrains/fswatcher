use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;

/// A space-efficient set for compact collections of IDs.
///
/// Uses a progressive storage strategy to minimize memory overhead:
/// - up to 3 elements: stored inline with no heap allocation
/// - up to 12 elements: stored in a single boxed array
/// - more than 12 elements: stored in a boxed [`HashSet`]
///
/// The boxed [`HashSet`] keeps the struct size at 16 bytes regardless of capacity,
/// at the cost of an extra indirection for large sets (which are not on the critical path).
///
/// # Type parameter
///
/// `T` is intended to be a newtype wrapper over [`std::num::NonZeroU32`], for example:
///
/// ```rust,ignore
/// #[derive(Clone, Copy, PartialEq, Eq, Hash)]
/// struct MyId(std::num::NonZeroU32);
/// ```
///
/// With such a type, `Option<T>` has the same size as `NonZeroU32` (4 bytes) due to
/// the niche optimization, so each inline or slice slot costs only 4 bytes.
/// Using a type without this niche (e.g. a plain `u32`) doubles per-element cost
/// and reduces the efficiency of the inline tiers.
pub struct IdSet<T> {
    storage: SetStorage<T>,
}

enum SetStorage<T> {
    Empty,
    Inline([Option<T>; 3]),
    Slice(Box<[Option<T>; 12]>),
    // While this introduces an extra allocation and indirection, it is not on the critical path.
    // Most of the records won't have that many subscriptions.
    // Using HashSet directly would increase the record size from 16 to 56 bytes.
    #[allow(clippy::box_collection)]
    Set(Box<HashSet<T>>),
}

impl<T: Copy + Eq + Hash> IdSet<T> {
    pub fn new() -> Self {
        Self { storage: SetStorage::Empty }
    }

    pub fn insert(&mut self, item: T) {
        match &mut self.storage {
            SetStorage::Empty => {
                self.storage = SetStorage::Inline([Some(item), None, None]);
            }
            SetStorage::Inline(arr) => {
                for slot in arr.iter() {
                    if *slot == Some(item) {
                        return;
                    }
                }
                for slot in arr.iter_mut() {
                    if slot.is_none() {
                        *slot = Some(item);
                        return;
                    }
                }
                // No empty slot, upgrade to Slice
                let mut slice = Box::new([None; 12]);
                slice[0..3].copy_from_slice(arr);
                slice[3] = Some(item);
                self.storage = SetStorage::Slice(slice);
            }
            SetStorage::Slice(arr) => {
                for slot in arr.iter() {
                    if *slot == Some(item) {
                        return;
                    }
                }
                for slot in arr.iter_mut() {
                    if slot.is_none() {
                        *slot = Some(item);
                        return;
                    }
                }
                // No empty slot, upgrade to Set
                let mut set = HashSet::new();
                for id in arr.iter().flatten() {
                    set.insert(*id);
                }
                set.insert(item);
                self.storage = SetStorage::Set(Box::new(set));
            }
            SetStorage::Set(set) => {
                set.insert(item);
            }
        }
    }

    pub fn remove(&mut self, item: T) -> bool {
        match &mut self.storage {
            SetStorage::Empty => false,
            SetStorage::Inline(arr) => {
                for slot in arr.iter_mut() {
                    if *slot == Some(item) {
                        *slot = None;
                        if arr.iter().all(|x| x.is_none()) {
                            self.storage = SetStorage::Empty;
                        }
                        return true;
                    }
                }
                false
            }
            SetStorage::Slice(arr) => {
                for slot in arr.iter_mut() {
                    if *slot == Some(item) {
                        *slot = None;
                        let count = arr.iter().filter(|x| x.is_some()).count();
                        if count == 0 {
                            self.storage = SetStorage::Empty;
                        } else if count <= 3 {
                            let mut inline = [None; 3];
                            let mut idx = 0;
                            for id in arr.iter().flatten() {
                                inline[idx] = Some(*id);
                                idx += 1;
                                if idx == 3 {
                                    break;
                                }
                            }
                            self.storage = SetStorage::Inline(inline);
                        }
                        return true;
                    }
                }
                false
            }
            SetStorage::Set(set) => {
                let removed = set.remove(&item);
                if set.len() <= 12 {
                    let mut slice = Box::new([None; 12]);
                    for (idx, id) in set.iter().enumerate() {
                        slice[idx] = Some(*id);
                    }
                    self.storage = SetStorage::Slice(slice);
                }
                removed
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(&self.storage, SetStorage::Empty)
    }

    pub fn for_each(&self, mut f: impl FnMut(T)) {
        match &self.storage {
            SetStorage::Empty => {}
            SetStorage::Inline(arr) => {
                for id in arr.iter().flatten() {
                    f(*id);
                }
            }
            SetStorage::Slice(arr) => {
                for id in arr.iter().flatten() {
                    f(*id);
                }
            }
            SetStorage::Set(set) => {
                for id in set.iter() {
                    f(*id);
                }
            }
        }
    }
}

impl<T: Copy + Eq + Hash> From<&HashSet<T>> for IdSet<T> {
    fn from(set: &HashSet<T>) -> Self {
        let mut id_set = IdSet::new();
        for item in set {
            id_set.insert(*item);
        }
        id_set
    }
}

impl<T: Copy + Eq + Hash> Default for IdSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Eq + Hash + fmt::Debug> fmt::Debug for IdSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.storage {
            SetStorage::Empty => write!(f, "IdSet::Empty"),
            SetStorage::Inline(arr) => {
                let items: Vec<_> = arr.iter().filter_map(|slot| slot.as_ref()).collect();
                write!(f, "IdSet::Inline[{}/3](", items.len())?;
                for (i, id) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?}", id)?;
                }
                write!(f, ")")
            }
            SetStorage::Slice(arr) => {
                let items: Vec<_> = arr.iter().filter_map(|slot| slot.as_ref()).collect();
                write!(f, "IdSet::Slice[{}/12](", items.len())?;
                for (i, id) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?}", id)?;
                }
                write!(f, ")")
            }
            SetStorage::Set(set) => {
                write!(f, "IdSet::Set[{}](", set.len())?;
                let mut first = true;
                for id in set.iter() {
                    if !first {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?}", id)?;
                    first = false;
                }
                write!(f, ")")
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::num::NonZeroU32;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct TestId(NonZeroU32);

    fn id(n: u32) -> TestId {
        TestId(NonZeroU32::new(n).unwrap())
    }

    #[test]
    fn test_new_is_empty() {
        let set = IdSet::<TestId>::new();
        assert!(set.is_empty());
    }

    #[test]
    fn test_insert_single() {
        let mut set = IdSet::new();
        set.insert(id(1));
        assert!(!set.is_empty());

        let mut count = 0;
        set.for_each(|_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn test_insert_duplicate() {
        let mut set = IdSet::new();
        set.insert(id(1));
        set.insert(id(1));

        let mut count = 0;
        set.for_each(|_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn test_insert_upgrades_empty_to_inline() {
        let mut set = IdSet::new();
        set.insert(id(1));

        assert!(matches!(set.storage, SetStorage::Inline(_)), "expected inline, actual: {:?}", set);
    }

    #[test]
    fn test_insert_upgrades_inline_to_slice() {
        let mut set = IdSet::new();
        for i in 1..=4 {
            set.insert(id(i));
        }

        assert!(matches!(set.storage, SetStorage::Slice(_)), "expected Slice, actual: {:?}", set);

        let mut count = 0;
        set.for_each(|_| count += 1);
        assert_eq!(count, 4);
    }

    #[test]
    fn test_insert_upgrades_slice_to_set() {
        let mut set = IdSet::new();
        for i in 1..=13 {
            set.insert(id(i));
        }

        assert!(matches!(set.storage, SetStorage::Set(_)), "expected Set, actual: {:?}", set);

        let mut count = 0;
        set.for_each(|_| count += 1);
        assert_eq!(count, 13);
    }

    #[test]
    fn test_remove_from_empty() {
        let mut set = IdSet::new();
        assert!(!set.remove(id(1)));
        assert!(set.is_empty());
    }

    #[test]
    fn test_remove_single_downgrades_to_empty() {
        let mut set = IdSet::new();
        set.insert(id(1));
        assert!(set.remove(id(1)));

        assert!(set.is_empty());
        assert!(matches!(set.storage, SetStorage::Empty), "expected Empty, actual: {:?}", set);
    }

    #[test]
    fn test_remove_downgrades_slice_to_inline() {
        let mut set = IdSet::new();
        for i in 1..=4 {
            set.insert(id(i));
        }

        assert!(set.remove(id(1)));

        assert!(matches!(set.storage, SetStorage::Inline(_)), "expected Inline, actual: {:?}", set);

        let mut count = 0;
        set.for_each(|_| count += 1);
        assert_eq!(count, 3);
    }

    #[test]
    fn test_remove_downgrades_set_to_slice() {
        let mut set = IdSet::new();
        for i in 1..=13 {
            set.insert(id(i));
        }

        assert!(set.remove(id(1)));

        assert!(matches!(set.storage, SetStorage::Slice(_)), "expected Slice, actual: {:?}", set);

        let mut count = 0;
        set.for_each(|_| count += 1);
        assert_eq!(count, 12);
    }

    #[test]
    fn test_remove_nonexistent() {
        let mut set = IdSet::new();
        set.insert(id(1));
        assert!(!set.remove(id(999)));

        let mut count = 0;
        set.for_each(|_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn test_for_each_empty() {
        let set = IdSet::<TestId>::new();
        let mut count = 0;
        set.for_each(|_| count += 1);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_for_each_inline() {
        let mut set = IdSet::new();
        set.insert(id(1));
        set.insert(id(2));
        set.insert(id(3));

        let mut collected = Vec::new();
        set.for_each(|x| collected.push(x.0.get()));
        collected.sort();

        assert_eq!(collected, vec![1, 2, 3]);
    }

    #[test]
    fn test_for_each_slice() {
        let mut set = IdSet::new();
        for i in 1..=10 {
            set.insert(id(i));
        }

        let mut collected = Vec::new();
        set.for_each(|x| collected.push(x.0.get()));
        collected.sort();

        assert_eq!(collected, (1..=10).collect::<Vec<_>>());
    }

    #[test]
    fn test_for_each_set() {
        let mut set = IdSet::new();
        for i in 1..=20 {
            set.insert(id(i));
        }

        let mut collected = Vec::new();
        set.for_each(|x| collected.push(x.0.get()));
        collected.sort();

        assert_eq!(collected, (1..=20).collect::<Vec<_>>());
    }

    #[test]
    fn size_is_16() {
        assert_eq!(16, std::mem::size_of::<IdSet<TestId>>());
    }
}
