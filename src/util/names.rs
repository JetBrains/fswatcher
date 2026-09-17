use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::hash::Hash;

/// A bidirectional map optimized for small collections with memory-efficient storage.
///
/// It uses progressively larger storage strategies as the collection grows, starting with inline storage
/// for small collections and transitioning to a HashMap for larger ones.
///
/// # Important Assumption: Unique Values
///
/// **This structure assumes that all values are unique.** Operations like [`name_by_value`] and
/// [`remove_by_value`] rely on this assumption. If duplicate values exist, these operations will
/// only work with the first occurrence found, leading to potentially confusing behavior.
///
/// This assumption is appropriate for this structure's primary use case: representing a file system
/// tree where values are typically unique record IDs or handles.
///
/// [`name_by_value`]: Names::name_by_value
/// [`remove_by_value`]: Names::remove_by_value
pub struct Names<V> {
    storage: Storage<V>,
}

enum Storage<V> {
    Empty,
    One(Box<(OsString, V)>),
    Array4(Box<[Option<(OsString, V)>; 4]>),
    Array8(Box<[Option<(OsString, V)>; 8]>),
    Array16(Box<[Option<(OsString, V)>; 16]>),
    Map(Box<Maps<V>>),
}

struct Maps<V> {
    forward: HashMap<OsString, V>,
    backward: HashMap<V, OsString>,
}

impl<V> Default for Names<V>
where
    V: Clone + Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<V> Names<V>
where
    V: Clone + Eq + Hash,
{
    pub fn new() -> Self {
        Names { storage: Storage::Empty }
    }

    pub fn insert(&mut self, k: OsString, v: V) -> Option<V> {
        match &mut self.storage {
            Storage::Empty => {
                self.storage = Storage::One(Box::new((k, v)));
                None
            }
            Storage::One(pair) => {
                if pair.0 == k {
                    let old = std::mem::replace(&mut pair.1, v);
                    Some(old)
                } else {
                    let mut arr = Box::new([None, None, None, None]);
                    let old = std::mem::replace(&mut self.storage, Storage::Empty);
                    if let Storage::One(old_pair) = old {
                        arr[0] = Some(*old_pair);
                        arr[1] = Some((k, v));
                    }
                    self.storage = Storage::Array4(arr);
                    None
                }
            }
            Storage::Array4(arr) => {
                if let Some(Some((_, val))) = arr.iter_mut().find(|e| e.as_ref().is_some_and(|(key, _)| key == &k)) {
                    let old = std::mem::replace(val, v);
                    return Some(old);
                }
                if let Some(empty_slot) = arr.iter_mut().find(|e| e.is_none()) {
                    *empty_slot = Some((k, v));
                    None
                } else {
                    self.upgrade_to_array8(k, v);
                    None
                }
            }
            Storage::Array8(arr) => {
                if let Some(Some((_, val))) = arr.iter_mut().find(|e| e.as_ref().is_some_and(|(key, _)| key == &k)) {
                    let old = std::mem::replace(val, v);
                    return Some(old);
                }
                if let Some(empty_slot) = arr.iter_mut().find(|e| e.is_none()) {
                    *empty_slot = Some((k, v));
                    None
                } else {
                    self.upgrade_to_array16(k, v);
                    None
                }
            }
            Storage::Array16(arr) => {
                if let Some(Some((_, val))) = arr.iter_mut().find(|e| e.as_ref().is_some_and(|(key, _)| key == &k)) {
                    let old = std::mem::replace(val, v);
                    return Some(old);
                }
                if let Some(empty_slot) = arr.iter_mut().find(|e| e.is_none()) {
                    *empty_slot = Some((k, v));
                    None
                } else {
                    self.upgrade_to_map(k, v);
                    None
                }
            }
            Storage::Map(maps) => {
                let old_v = maps.forward.insert(k.clone(), v.clone());
                if let Some(ref old) = old_v {
                    maps.backward.remove(old);
                }
                maps.backward.insert(v, k);
                old_v
            }
        }
    }

    #[allow(unused)] // it is indeed not used, but it does make sense to have it
    pub fn remove(&mut self, k: &OsStr) -> Option<V> {
        // First, try to remove the item and check if downgrade is needed
        let (result, needs_downgrade) = match &mut self.storage {
            Storage::Empty => (None, false),
            Storage::One(pair) => {
                if pair.0.as_os_str() == k {
                    let old = std::mem::replace(&mut self.storage, Storage::Empty);
                    if let Storage::One(pair) = old {
                        return Some(pair.1);
                    }
                }
                (None, false)
            }
            Storage::Array4(arr) => {
                let mut found = None;
                for (i, slot) in arr.iter_mut().enumerate() {
                    if let Some((key, _)) = slot {
                        if key.as_os_str() == k {
                            found = Some(i);
                            break;
                        }
                    }
                }
                if let Some(index) = found {
                    let result = arr[index].take().map(|(_, v)| v);
                    (result, true)
                } else {
                    (None, false)
                }
            }
            Storage::Array8(arr) => {
                let mut found = None;
                for (i, slot) in arr.iter_mut().enumerate() {
                    if let Some((key, _)) = slot {
                        if key.as_os_str() == k {
                            found = Some(i);
                            break;
                        }
                    }
                }
                if let Some(index) = found {
                    let result = arr[index].take().map(|(_, v)| v);
                    (result, true)
                } else {
                    (None, false)
                }
            }
            Storage::Array16(arr) => {
                let mut found = None;
                for (i, slot) in arr.iter_mut().enumerate() {
                    if let Some((key, _)) = slot {
                        if key.as_os_str() == k {
                            found = Some(i);
                            break;
                        }
                    }
                }
                if let Some(index) = found {
                    let result = arr[index].take().map(|(_, v)| v);
                    (result, true)
                } else {
                    (None, false)
                }
            }
            Storage::Map(maps) => {
                if let Some(v) = maps.forward.remove(k) {
                    maps.backward.remove(&v);
                    if maps.forward.is_empty() {
                        self.storage = Storage::Empty;
                        return Some(v);
                    }
                    (Some(v), false)
                } else {
                    (None, false)
                }
            }
        };

        // Perform downgrade if needed
        if needs_downgrade {
            self.try_downgrade();
        }

        result
    }

    pub fn get(&self, k: &OsStr) -> Option<&V> {
        match &self.storage {
            Storage::Empty => None,
            Storage::One(pair) => {
                if pair.0.as_os_str() == k {
                    Some(&pair.1)
                } else {
                    None
                }
            }
            Storage::Array4(arr) => {
                for (key, val) in arr.iter().flatten() {
                    if key.as_os_str() == k {
                        return Some(val);
                    }
                }
                None
            }
            Storage::Array8(arr) => {
                for (key, val) in arr.iter().flatten() {
                    if key.as_os_str() == k {
                        return Some(val);
                    }
                }
                None
            }
            Storage::Array16(arr) => {
                for (key, val) in arr.iter().flatten() {
                    if key.as_os_str() == k {
                        return Some(val);
                    }
                }
                None
            }
            Storage::Map(maps) => maps.forward.get(k),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self.storage, Storage::Empty)
    }

    pub fn remove_by_value(&mut self, v: &V) {
        let needs_downgrade = match &mut self.storage {
            Storage::Empty => false,
            Storage::One(pair) => {
                if pair.1 == *v {
                    self.storage = Storage::Empty;
                }
                false
            }
            Storage::Array4(arr) => {
                for slot in arr.iter_mut() {
                    if let Some((_, val)) = slot {
                        if val == v {
                            *slot = None;
                            return self.try_downgrade();
                        }
                    }
                }
                false
            }
            Storage::Array8(arr) => {
                for slot in arr.iter_mut() {
                    if let Some((_, val)) = slot {
                        if val == v {
                            *slot = None;
                            return self.try_downgrade();
                        }
                    }
                }
                false
            }
            Storage::Array16(arr) => {
                for slot in arr.iter_mut() {
                    if let Some((_, val)) = slot {
                        if val == v {
                            *slot = None;
                            return self.try_downgrade();
                        }
                    }
                }
                false
            }
            Storage::Map(maps) => {
                if let Some(k) = maps.backward.remove(v) {
                    maps.forward.remove(&k);
                    if maps.forward.is_empty() {
                        self.storage = Storage::Empty;
                    }
                }
                false
            }
        };

        if needs_downgrade {
            self.try_downgrade();
        }
    }

    pub fn name_by_value(&self, v: &V) -> Option<&OsStr> {
        match &self.storage {
            Storage::Empty => None,
            Storage::One(pair) => {
                if pair.1 == *v {
                    Some(pair.0.as_os_str())
                } else {
                    None
                }
            }
            Storage::Array4(arr) => {
                for (key, val) in arr.iter().flatten() {
                    if val == v {
                        return Some(key.as_os_str());
                    }
                }
                None
            }
            Storage::Array8(arr) => {
                for (key, val) in arr.iter().flatten() {
                    if val == v {
                        return Some(key.as_os_str());
                    }
                }
                None
            }
            Storage::Array16(arr) => {
                for (key, val) in arr.iter().flatten() {
                    if val == v {
                        return Some(key.as_os_str());
                    }
                }
                None
            }
            Storage::Map(maps) => maps.backward.get(v).map(|k| k.as_os_str()),
        }
    }

    fn try_downgrade(&mut self) {
        let new_storage = match &self.storage {
            Storage::Array4(arr) => {
                let mut count = 0;
                let mut last_item = None;
                for item in arr.iter().flatten() {
                    count += 1;
                    last_item = Some(item.clone());
                }

                match count {
                    0 => Some(Storage::Empty),
                    1 => last_item.map(|item| Storage::One(Box::new(item))),
                    _ => None, // Keep as Array4
                }
            }
            Storage::Array8(arr) => {
                let mut items = Vec::new();
                for item in arr.iter().flatten() {
                    items.push(item.clone());
                }

                match items.len() {
                    0 => Some(Storage::Empty),
                    1 => Some(Storage::One(Box::new(items[0].clone()))),
                    2..=4 => {
                        let mut new_arr = Box::new([None, None, None, None]);
                        for (i, item) in items.into_iter().enumerate() {
                            new_arr[i] = Some(item);
                        }
                        Some(Storage::Array4(new_arr))
                    }
                    _ => None, // Keep as Array8
                }
            }
            Storage::Array16(arr) => {
                let mut items = Vec::new();
                for item in arr.iter().flatten() {
                    items.push(item.clone());
                }

                match items.len() {
                    0 => Some(Storage::Empty),
                    1 => Some(Storage::One(Box::new(items[0].clone()))),
                    2..=4 => {
                        let mut new_arr = Box::new([None, None, None, None]);
                        for (i, item) in items.into_iter().enumerate() {
                            new_arr[i] = Some(item);
                        }
                        Some(Storage::Array4(new_arr))
                    }
                    5..=8 => {
                        let mut new_arr = Box::new([None, None, None, None, None, None, None, None]);
                        for (i, item) in items.into_iter().enumerate() {
                            new_arr[i] = Some(item);
                        }
                        Some(Storage::Array8(new_arr))
                    }
                    _ => None, // Keep as Array16
                }
            }
            _ => None, // Only arrays need downgrading
        };

        if let Some(new_storage) = new_storage {
            self.storage = new_storage;
        }
    }

    fn upgrade_to_array8(&mut self, k: OsString, v: V) {
        let mut new_arr = Box::new([None, None, None, None, None, None, None, None]);
        let old = std::mem::replace(&mut self.storage, Storage::Empty);
        if let Storage::Array4(old_arr) = old {
            for (i, slot) in old_arr.into_iter().enumerate() {
                new_arr[i] = slot.clone();
            }
            new_arr[4] = Some((k, v));
        }
        self.storage = Storage::Array8(new_arr);
    }

    fn upgrade_to_array16(&mut self, k: OsString, v: V) {
        let mut new_arr = Box::new([
            None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None,
        ]);
        let old = std::mem::replace(&mut self.storage, Storage::Empty);
        if let Storage::Array8(old_arr) = old {
            for (i, slot) in old_arr.into_iter().enumerate() {
                new_arr[i] = slot.clone();
            }
            new_arr[8] = Some((k, v));
        }
        self.storage = Storage::Array16(new_arr);
    }

    fn upgrade_to_map(&mut self, k: OsString, v: V) {
        let mut forward = HashMap::new();
        let mut backward = HashMap::new();

        let old = std::mem::replace(&mut self.storage, Storage::Empty);
        if let Storage::Array16(old_arr) = old {
            for slot in old_arr.into_iter() {
                if let Some((key, val)) = slot.clone() {
                    forward.insert(key.clone(), val.clone());
                    backward.insert(val, key);
                }
            }
        }

        forward.insert(k.clone(), v.clone());
        backward.insert(v, k);

        self.storage = Storage::Map(Box::new(Maps { forward, backward }));
    }

    pub fn for_each(&self, mut f: impl FnMut(&OsStr, &V)) {
        match &self.storage {
            Storage::Empty => {}
            Storage::One(pair) => {
                f(pair.0.as_os_str(), &pair.1);
            }
            Storage::Array4(arr) => {
                for (k, v) in arr.iter().flatten() {
                    f(k.as_os_str(), v);
                }
            }
            Storage::Array8(arr) => {
                for (k, v) in arr.iter().flatten() {
                    f(k.as_os_str(), v);
                }
            }
            Storage::Array16(arr) => {
                for (k, v) in arr.iter().flatten() {
                    f(k.as_os_str(), v);
                }
            }
            Storage::Map(maps) => {
                for (k, v) in &maps.forward {
                    f(k.as_os_str(), v);
                }
            }
        }
    }
}

impl<V> fmt::Debug for Names<V>
where
    V: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.storage {
            Storage::Empty => write!(f, "Names::Empty"),
            Storage::One(pair) => {
                write!(f, "Names::One({:?} => {:?})", pair.0, pair.1)
            }
            Storage::Array4(arr) => {
                let items: Vec<_> = arr.iter().filter_map(|slot| slot.as_ref()).collect();
                write!(f, "Names::Array4[{}/4](", items.len())?;
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?} => {:?}", k, v)?;
                }
                write!(f, ")")
            }
            Storage::Array8(arr) => {
                let items: Vec<_> = arr.iter().filter_map(|slot| slot.as_ref()).collect();
                write!(f, "Names::Array8[{}/8](", items.len())?;
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?} => {:?}", k, v)?;
                }
                write!(f, ")")
            }
            Storage::Array16(arr) => {
                let items: Vec<_> = arr.iter().filter_map(|slot| slot.as_ref()).collect();
                write!(f, "Names::Array16[{}/16](", items.len())?;
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?} => {:?}", k, v)?;
                }
                write!(f, ")")
            }
            Storage::Map(maps) => {
                write!(f, "Names::Map[{}](", maps.forward.len())?;
                let mut first = true;
                for (k, v) in maps.forward.iter() {
                    if !first {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?} => {:?}", k, v)?;
                    first = false;
                }
                write!(f, ")")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    #[test]
    fn test_growing_to_17_elements() {
        let mut names = Names::new();

        // Empty state
        assert!(matches!(names.storage, Storage::Empty));

        // Add 1st element -> One state
        names.insert(OsString::from("1"), "one");
        assert!(matches!(names.storage, Storage::One(_)));
        assert_eq!(names.get(OsStr::new("1")), Some(&"one"));

        // Add 2nd element -> Array4 state
        names.insert(OsString::from("2"), "two");
        assert!(matches!(names.storage, Storage::Array4(_)));
        assert_eq!(names.get(OsStr::new("1")), Some(&"one"));
        assert_eq!(names.get(OsStr::new("2")), Some(&"two"));

        // Add 3rd and 4th elements -> still Array4
        names.insert(OsString::from("3"), "three");
        names.insert(OsString::from("4"), "four");
        assert!(matches!(names.storage, Storage::Array4(_)));
        assert_eq!(names.get(OsStr::new("3")), Some(&"three"));
        assert_eq!(names.get(OsStr::new("4")), Some(&"four"));

        // Add 5th element -> Array8 state
        names.insert(OsString::from("5"), "five");
        assert!(matches!(names.storage, Storage::Array8(_)));
        assert_eq!(names.get(OsStr::new("5")), Some(&"five"));

        // Add elements 6-8 -> still Array8
        for i in 6..=8 {
            names.insert(OsString::from(i.to_string()), Box::leak(format!("{}", i).into_boxed_str()));
        }
        assert!(matches!(names.storage, Storage::Array8(_)));

        // Add 9th element -> Array16 state
        names.insert(OsString::from("9"), "nine");
        assert!(matches!(names.storage, Storage::Array16(_)));

        // Add elements 10-16 -> still Array16
        for i in 10..=16 {
            names.insert(OsString::from(i.to_string()), Box::leak(format!("{}", i).into_boxed_str()));
        }
        assert!(matches!(names.storage, Storage::Array16(_)));

        // Add 17th element -> Map state
        names.insert(OsString::from("17"), "seventeen");
        assert!(matches!(names.storage, Storage::Map(_)));

        // Verify all elements are still accessible
        for i in 1..=17 {
            assert!(names.get(OsStr::new(&i.to_string())).is_some());
        }
    }

    #[test]
    fn test_gradual_truncation_to_empty() {
        let mut names = Names::new();

        // Build up to Map state
        for i in 1..=17 {
            names.insert(OsString::from(i.to_string()), i * 100);
        }
        assert!(matches!(names.storage, Storage::Map(_)));

        // Remove elements one by one and verify downgrade happens
        for i in (1..=17).rev() {
            assert_eq!(names.remove(OsStr::new(&i.to_string())), Some(i * 100));
        }

        // After removing all from Map, it should downgrade to Empty
        assert!(matches!(names.storage, Storage::Empty));

        // Test downgrade from Array16 to Array8
        let mut names = Names::new();
        for i in 1..=9 {
            names.insert(OsString::from(i.to_string()), i * 10);
        }
        assert!(matches!(names.storage, Storage::Array16(_)));

        // Remove one element, should downgrade to Array8
        names.remove(OsStr::new("9"));
        assert!(matches!(names.storage, Storage::Array8(_)));

        // Remove 4 more, should still be Array8 (with 4 elements)
        for i in 5..=8 {
            names.remove(OsStr::new(&i.to_string()));
        }
        assert!(matches!(names.storage, Storage::Array4(_)));

        // Remove 3 more, should downgrade to One
        for i in 2..=4 {
            names.remove(OsStr::new(&i.to_string()));
        }
        assert!(matches!(names.storage, Storage::One(_)));

        // Remove last one, should be Empty
        names.remove(OsStr::new("1"));
        assert!(matches!(names.storage, Storage::Empty));

        // Test Array4 downgrade
        let mut names = Names::new();
        for i in 1..=4 {
            names.insert(OsString::from(i.to_string()), i);
        }
        assert!(matches!(names.storage, Storage::Array4(_)));

        // Remove 3 elements
        for i in 2..=4 {
            names.remove(OsStr::new(&i.to_string()));
        }
        assert!(matches!(names.storage, Storage::One(_)));

        // Remove last element
        names.remove(OsStr::new("1"));
        assert!(matches!(names.storage, Storage::Empty));
    }

    #[test]
    fn test_downgrade_with_remove_by_value() {
        let mut names = Names::new();

        // Test Map downgrade
        for i in 1..=17 {
            names.insert(OsString::from(i.to_string()), format!("val{}", i));
        }
        assert!(matches!(names.storage, Storage::Map(_)));

        for i in 1..=17 {
            names.remove_by_value(&format!("val{}", i));
        }
        assert!(matches!(names.storage, Storage::Empty));

        // Test Array16 downgrade via remove_by_value
        let mut names = Names::new();
        for i in 1..=10 {
            names.insert(OsString::from(i.to_string()), i * 100);
        }
        assert!(matches!(names.storage, Storage::Array16(_)));

        // Remove values to trigger downgrade
        for i in 3..=10 {
            names.remove_by_value(&(i * 100));
        }
        assert!(matches!(names.storage, Storage::Array4(_))); // 2 elements left

        names.remove_by_value(&200);
        assert!(matches!(names.storage, Storage::One(_))); // 1 element left

        names.remove_by_value(&100);
        assert!(matches!(names.storage, Storage::Empty));
    }

    #[test]
    fn test_get_at_each_state() {
        let mut names = Names::new();

        // Test at 0 elements (Empty)
        assert_eq!(names.get(OsStr::new("1")), None);

        // Test at 1 element (One)
        names.insert(OsString::from("1"), "first");
        assert_eq!(names.get(OsStr::new("1")), Some(&"first"));
        assert_eq!(names.get(OsStr::new("2")), None);

        // Test at 4 elements (Array4)
        names.insert(OsString::from("2"), "second");
        names.insert(OsString::from("3"), "third");
        names.insert(OsString::from("4"), "fourth");
        assert_eq!(names.get(OsStr::new("1")), Some(&"first"));
        assert_eq!(names.get(OsStr::new("2")), Some(&"second"));
        assert_eq!(names.get(OsStr::new("3")), Some(&"third"));
        assert_eq!(names.get(OsStr::new("4")), Some(&"fourth"));
        assert_eq!(names.get(OsStr::new("5")), None);

        // Test at 8 elements (Array8)
        names.insert(OsString::from("5"), "fifth");
        names.insert(OsString::from("6"), "sixth");
        names.insert(OsString::from("7"), "seventh");
        names.insert(OsString::from("8"), "eighth");
        for i in 1..=8 {
            assert!(names.get(OsStr::new(&i.to_string())).is_some());
        }
        assert_eq!(names.get(OsStr::new("9")), None);

        // Test at 16 elements (Array16)
        for i in 9..=16 {
            names.insert(OsString::from(i.to_string()), Box::leak(format!("value{}", i).into_boxed_str()));
        }
        for i in 1..=16 {
            assert!(names.get(OsStr::new(&i.to_string())).is_some());
        }
        assert_eq!(names.get(OsStr::new("17")), None);

        // Test at 17 elements (Map)
        names.insert(OsString::from("17"), "seventeenth");
        for i in 1..=17 {
            assert!(names.get(OsStr::new(&i.to_string())).is_some());
        }
        assert_eq!(names.get(OsStr::new("18")), None);
    }

    #[test]
    fn test_remove_by_value_at_each_state() {
        // Test Empty state
        let mut names: Names<&str> = Names::new();
        names.remove_by_value(&"nonexistent"); // Should not panic

        // Test One state
        let mut names = Names::new();
        names.insert(OsString::from("1"), "one");
        names.remove_by_value(&"one");
        assert!(matches!(names.storage, Storage::Empty));
        assert_eq!(names.get(OsStr::new("1")), None);

        // Test Array4 state
        let mut names = Names::new();
        names.insert(OsString::from("1"), "a");
        names.insert(OsString::from("2"), "b");
        names.insert(OsString::from("3"), "c");
        names.insert(OsString::from("4"), "d");
        assert!(matches!(names.storage, Storage::Array4(_)));

        names.remove_by_value(&"b");
        assert_eq!(names.get(OsStr::new("2")), None);
        assert_eq!(names.get(OsStr::new("1")), Some(&"a"));
        assert_eq!(names.get(OsStr::new("3")), Some(&"c"));
        assert_eq!(names.get(OsStr::new("4")), Some(&"d"));

        // Test Array8 state
        let mut names = Names::new();
        for i in 1..=8 {
            names.insert(OsString::from(i.to_string()), i * 10);
        }
        assert!(matches!(names.storage, Storage::Array8(_)));

        names.remove_by_value(&30);
        assert_eq!(names.get(OsStr::new("3")), None);
        assert_eq!(names.get(OsStr::new("1")), Some(&10));
        assert_eq!(names.get(OsStr::new("5")), Some(&50));

        // Test Array16 state
        let mut names = Names::new();
        for i in 1..=16 {
            names.insert(OsString::from(i.to_string()), i * 100);
        }
        assert!(matches!(names.storage, Storage::Array16(_)));

        names.remove_by_value(&1000);
        assert_eq!(names.get(OsStr::new("10")), None);
        assert_eq!(names.get(OsStr::new("11")), Some(&1100));

        // Test Map state with bidirectional lookup
        let mut names = Names::new();
        for i in 1..=17 {
            names.insert(OsString::from(i.to_string()), i * 1000);
        }
        assert!(matches!(names.storage, Storage::Map(_)));

        names.remove_by_value(&5000);
        assert_eq!(names.get(OsStr::new("5")), None);
        assert_eq!(names.get(OsStr::new("6")), Some(&6000));

        // Verify bidirectional consistency
        names.remove_by_value(&17000);
        assert_eq!(names.get(OsStr::new("17")), None);
    }

    #[test]
    fn test_update_existing_keys() {
        let mut names = Names::new();

        // Test update in One state
        assert_eq!(names.insert(OsString::from("1"), "old"), None);
        assert_eq!(names.insert(OsString::from("1"), "new"), Some("old"));
        assert_eq!(names.get(OsStr::new("1")), Some(&"new"));
        assert!(matches!(names.storage, Storage::One(_)));

        // Test update in Array4 state
        assert_eq!(names.insert(OsString::from("2"), "two"), None);
        assert_eq!(names.insert(OsString::from("1"), "updated"), Some("new"));
        assert_eq!(names.get(OsStr::new("1")), Some(&"updated"));
        assert!(matches!(names.storage, Storage::Array4(_)));

        // Test update in Array8 state
        for i in 3..=5 {
            names.insert(OsString::from(i.to_string()), Box::leak(format!("val{}", i).into_boxed_str()));
        }
        assert!(matches!(names.storage, Storage::Array8(_)));
        assert_eq!(names.insert(OsString::from("3"), "replaced3"), Some("val3"));
        assert_eq!(names.get(OsStr::new("3")), Some(&"replaced3"));

        // Test update in Array16 state
        for i in 6..=9 {
            names.insert(OsString::from(i.to_string()), Box::leak(format!("val{}", i).into_boxed_str()));
        }
        assert!(matches!(names.storage, Storage::Array16(_)));
        assert_eq!(names.insert(OsString::from("7"), "replaced7"), Some("val7"));
        assert_eq!(names.get(OsStr::new("7")), Some(&"replaced7"));

        // Test update in Map state (important for bidirectional map)
        for i in 10..=17 {
            names.insert(OsString::from(i.to_string()), Box::leak(format!("val{}", i).into_boxed_str()));
        }
        assert!(matches!(names.storage, Storage::Map(_)));

        assert_eq!(names.insert(OsString::from("10"), "new_value_10"), Some("val10"));
        assert_eq!(names.get(OsStr::new("10")), Some(&"new_value_10"));

        // Ensure backward map is updated correctly
        names.remove_by_value(&"new_value_10");
        assert_eq!(names.get(OsStr::new("10")), None);
    }

    #[test]
    fn test_name_by_value() {
        // Test Empty state
        let names: Names<&str> = Names::new();
        assert_eq!(names.name_by_value(&"nonexistent"), None);

        // Test One state
        let mut names = Names::new();
        names.insert(OsString::from("key1"), "value1");
        assert_eq!(names.name_by_value(&"value1"), Some(OsStr::new("key1")));
        assert_eq!(names.name_by_value(&"nonexistent"), None);

        // Test Array4 state
        let mut names = Names::new();
        names.insert(OsString::from("a"), "val_a");
        names.insert(OsString::from("b"), "val_b");
        names.insert(OsString::from("c"), "val_c");
        names.insert(OsString::from("d"), "val_d");
        assert_eq!(names.name_by_value(&"val_b"), Some(OsStr::new("b")));
        assert_eq!(names.name_by_value(&"val_d"), Some(OsStr::new("d")));
        assert_eq!(names.name_by_value(&"missing"), None);

        // Test Array8 state
        let mut names = Names::new();
        for i in 1..=8 {
            names.insert(OsString::from(format!("key{}", i)), i * 10);
        }
        assert_eq!(names.name_by_value(&30), Some(OsStr::new("key3")));
        assert_eq!(names.name_by_value(&70), Some(OsStr::new("key7")));
        assert_eq!(names.name_by_value(&999), None);

        // Test Array16 state
        let mut names = Names::new();
        for i in 1..=16 {
            names.insert(OsString::from(format!("k{}", i)), i * 100);
        }
        assert_eq!(names.name_by_value(&500), Some(OsStr::new("k5")));
        assert_eq!(names.name_by_value(&1600), Some(OsStr::new("k16")));
        assert_eq!(names.name_by_value(&9999), None);

        // Test Map state with bidirectional lookup
        let mut names = Names::new();
        for i in 1..=17 {
            names.insert(OsString::from(format!("name{}", i)), i * 1000);
        }
        assert_eq!(names.name_by_value(&5000), Some(OsStr::new("name5")));
        assert_eq!(names.name_by_value(&17000), Some(OsStr::new("name17")));
        assert_eq!(names.name_by_value(&99999), None);
    }

    #[test]
    fn size_is_16bytes() {
        assert_eq!(16, std::mem::size_of::<Names<u32>>());
        assert_eq!(16, std::mem::size_of::<Names<NonZeroU32>>());
    }
}
