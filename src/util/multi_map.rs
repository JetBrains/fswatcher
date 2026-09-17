use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

/// This data structure is optimized for the common case where most keys have only a single value.
/// To minimize memory overhead, keys with a single value are stored in a `HashMap<K, V>`, while keys
/// with multiple values are stored in a separate `HashMap<K, HashSet<V>>`.
#[derive(Debug)]
pub struct MultiMap<K, V> {
    singles: HashMap<K, V>,
    multis: HashMap<K, HashSet<V>>,
}

impl<K, V> MultiMap<K, V>
where
    K: Eq + Hash,
    V: Eq + Hash,
{
    pub fn new() -> Self {
        Self {
            singles: HashMap::new(),
            multis: HashMap::new(),
        }
    }

    /// Returns an iterator over all values associated with the key.
    #[cfg(any(test, target_os = "linux"))]
    pub fn get(&self, key: &K) -> Values<'_, V> {
        if let Some(value) = self.singles.get(key) {
            Values::Single(Some(value).into_iter())
        } else if let Some(set) = self.multis.get(key) {
            Values::Multi(set.iter())
        } else {
            Values::Empty
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        if let Some(existing_value) = self.singles.remove(&key) {
            if existing_value != value {
                let mut set = HashSet::new();
                set.insert(existing_value);
                set.insert(value);
                self.multis.insert(key, set);
            } else {
                self.singles.insert(key, existing_value);
            }
        } else if let Some(set) = self.multis.get_mut(&key) {
            set.insert(value);
        } else {
            self.singles.insert(key, value);
        }
    }

    /// Removes a specific key-value pair from the map.
    ///
    /// Returns `true` if this was the last value for the key and the key has been removed entirely.
    /// Returns `false` if the value was removed but the key still has other values, or if the
    /// value wasn't found.
    pub fn remove(&mut self, key: &K, value: &V) -> bool {
        if let Some((single_key, single_value)) = self.singles.remove_entry(key) {
            if single_value.eq(value) {
                true
            } else {
                self.singles.insert(single_key, single_value);
                false
            }
        } else if let Some(set) = self.multis.get_mut(key) {
            if set.remove(value) {
                if set.is_empty() {
                    self.multis.remove(key);
                    true
                } else if set.len() == 1 {
                    let (multi_key, values) = self.multis.remove_entry(key).expect("we know it is there");
                    let remaining_value = values.into_iter().next().expect("we know it is there");
                    self.singles.insert(multi_key, remaining_value);
                    false
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    }

    pub fn remove_all(&mut self, key: &K) -> IntoValues<V> {
        if let Some(value) = self.singles.remove(key) {
            IntoValues::Single(Some(value).into_iter())
        } else if let Some(values) = self.multis.remove(key) {
            IntoValues::Multi(values.into_iter())
        } else {
            IntoValues::Empty
        }
    }

    #[cfg(target_os = "linux")]
    pub fn clear(&mut self) {
        self.singles.clear();
        self.multis.clear();
    }

    #[cfg(test)]
    pub fn key_count(&self) -> usize {
        self.singles.len() + self.multis.len()
    }

    #[cfg(any(test, target_os = "windows"))]
    pub fn is_empty(&self) -> bool {
        self.singles.is_empty() && self.multis.is_empty()
    }
}

impl<K, V> Default for MultiMap<K, V>
where
    K: Eq + Hash,
    V: Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Iterator over values associated with a key in a `MultiMap`.
#[cfg(any(test, target_os = "linux"))]
pub enum Values<'a, V> {
    Single(std::option::IntoIter<&'a V>),
    Multi(std::collections::hash_set::Iter<'a, V>),
    Empty,
}

#[cfg(any(test, target_os = "linux"))]
impl<'a, V> Iterator for Values<'a, V> {
    type Item = &'a V;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Values::Single(iter) => iter.next(),
            Values::Multi(iter) => iter.next(),
            Values::Empty => None,
        }
    }
}

/// Owning iterator over values removed from a `MultiMap`.
pub enum IntoValues<V> {
    Single(std::option::IntoIter<V>),
    Multi(std::collections::hash_set::IntoIter<V>),
    Empty,
}

impl<V> Iterator for IntoValues<V> {
    type Item = V;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            IntoValues::Single(iter) => iter.next(),
            IntoValues::Multi(iter) => iter.next(),
            IntoValues::Empty => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_single_value() {
        let mut map = MultiMap::new();
        map.insert(1, "a");

        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values, vec![&"a"]);

        assert_eq!(map.singles.len(), 1);
        assert_eq!(map.multis.len(), 0);
    }

    #[test]
    fn test_insert_same_value_twice() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        map.insert(1, "a");

        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values, vec![&"a"]);
        assert_eq!(map.key_count(), 1);

        assert_eq!(map.singles.len(), 1);
        assert_eq!(map.multis.len(), 0);
    }

    #[test]
    fn test_insert_two_different_values() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        map.insert(1, "b");

        let mut values: Vec<_> = map.get(&1).collect();
        values.sort();
        assert_eq!(values, vec![&"a", &"b"]);
        assert_eq!(map.key_count(), 1);

        assert_eq!(map.singles.len(), 0);
        assert_eq!(map.multis.len(), 1);
        assert_eq!(map.multis.get(&1).unwrap().len(), 2);
    }

    #[test]
    fn test_multiple_keys_single_values() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        map.insert(2, "b");
        map.insert(3, "c");

        let values1: Vec<_> = map.get(&1).collect();
        let values2: Vec<_> = map.get(&2).collect();
        let values3: Vec<_> = map.get(&3).collect();
        assert_eq!(values1, vec![&"a"]);
        assert_eq!(values2, vec![&"b"]);
        assert_eq!(values3, vec![&"c"]);
        assert_eq!(map.key_count(), 3);

        assert_eq!(map.singles.len(), 3);
        assert_eq!(map.multis.len(), 0);
    }

    #[test]
    fn test_remove_only_value() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        let removed = map.remove(&1, &"a");

        assert!(removed, "Should return true when removing the last value");
        assert_eq!(map.get(&1).count(), 0);
        assert_eq!(map.key_count(), 0);
        assert!(map.is_empty());
    }

    #[test]
    fn test_remove_nonexistent_value() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        let removed = map.remove(&1, &"b");

        assert!(!removed, "Should return false when value doesn't exist");
        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values, vec![&"a"]);
        assert_eq!(map.key_count(), 1);
    }

    #[test]
    fn test_remove_nonexistent_key() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        let removed = map.remove(&2, &"a");

        assert!(!removed, "Should return false when key doesn't exist");
        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values, vec![&"a"]);
        assert_eq!(map.key_count(), 1);
    }

    #[test]
    fn test_remove_one_of_two_values() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        map.insert(1, "b");
        let removed = map.remove(&1, &"a");

        assert!(!removed, "Should return false when key still has other values");
        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values, vec![&"b"]);
        assert_eq!(map.key_count(), 1);

        assert_eq!(map.singles.len(), 1);
        assert_eq!(map.multis.len(), 0);
    }

    #[test]
    fn test_remove_one_of_three_values() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        map.insert(1, "b");
        map.insert(1, "c");
        let removed = map.remove(&1, &"b");

        assert!(!removed, "Should return false when key still has other values");
        assert_eq!(map.key_count(), 1);

        assert_eq!(map.singles.len(), 0);
        assert_eq!(map.multis.len(), 1);
        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values.len(), 2);
        assert!(values.contains(&&"a"));
        assert!(values.contains(&&"c"));
    }

    #[test]
    fn test_promotion_and_demotion_cycle() {
        let mut map = MultiMap::new();

        map.insert(1, "a");
        assert_eq!(map.singles.len(), 1);
        assert_eq!(map.multis.len(), 0);

        map.insert(1, "b");
        assert_eq!(map.singles.len(), 0);
        assert_eq!(map.multis.len(), 1);

        let removed = map.remove(&1, &"b");
        assert!(!removed, "Should return false - key still has value 'a'");
        assert_eq!(map.singles.len(), 1);
        assert_eq!(map.multis.len(), 0);
        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values, vec![&"a"]);

        map.insert(1, "c");
        assert_eq!(map.singles.len(), 0);
        assert_eq!(map.multis.len(), 1);

        let removed = map.remove(&1, &"a");
        assert!(!removed, "Should return false - key still has value 'c'");
        assert_eq!(map.singles.len(), 1);
        assert_eq!(map.multis.len(), 0);
        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values, vec![&"c"]);
    }

    #[test]
    fn test_remove_returns_true_only_when_key_removed() {
        let mut map = MultiMap::new();

        map.insert(1, "a");
        assert!(map.remove(&1, &"a"), "Should return true - key removed");
        assert!(map.is_empty());

        map.insert(2, "x");
        map.insert(2, "y");
        map.insert(2, "z");
        assert!(!map.remove(&2, &"x"), "Should return false - key still has values");
        assert_eq!(map.key_count(), 1);

        assert!(!map.remove(&2, &"y"), "Should return false - key still has values");
        assert_eq!(map.key_count(), 1);

        assert!(map.remove(&2, &"z"), "Should return true - key removed");
        assert!(map.is_empty());
    }

    #[test]
    fn test_get_returns_empty_iterator_for_nonexistent_key() {
        let mut map = MultiMap::new();
        map.insert(1, "a");

        assert_eq!(map.get(&2).count(), 0);
        assert_eq!(map.get(&0).count(), 0);
    }

    #[test]
    fn test_get_returns_all_values() {
        let mut map = MultiMap::new();

        map.insert(1, "a");
        let values: Vec<_> = map.get(&1).collect();
        assert_eq!(values, vec![&"a"]);

        map.insert(2, "x");
        map.insert(2, "y");
        map.insert(2, "z");
        let mut values: Vec<_> = map.get(&2).collect();
        values.sort();
        assert_eq!(values, vec![&"x", &"y", &"z"]);
    }

    #[test]
    fn test_remove_all_for_single_value() {
        let mut map = MultiMap::new();
        map.insert(1, "a");

        let values: Vec<_> = map.remove_all(&1).collect();
        assert_eq!(values, vec!["a"]);
        assert!(map.is_empty());
    }

    #[test]
    fn test_remove_all_for_multiple_values() {
        let mut map = MultiMap::new();
        map.insert(1, "a");
        map.insert(1, "b");

        let mut values: Vec<_> = map.remove_all(&1).collect();
        values.sort();
        assert_eq!(values, vec!["a", "b"]);
        assert!(map.is_empty());
    }
}
