//! A `slab::Slab` wrapper with typed `NonZeroU32` keys.
#![allow(dead_code)]

mod key_macros;

pub(crate) use key_macros::key;

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::ops::{Index, IndexMut};

/// Wrapper around slab::Slab with NonZeroU32 keys
pub struct Slab<K: SlabKeyType, T> {
    slab: slab::Slab<T>,
    _phantom: PhantomData<K>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlabKey(NonZeroU32);

impl fmt::Debug for SlabKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SlabKey({})", self.0.get() - 1)
    }
}

impl SlabKey {
    #[inline]
    pub fn from_usize(idx: usize) -> SlabKey {
        SlabKey(NonZeroU32::new(idx as u32 + 1).expect("got zero key"))
    }

    #[inline]
    pub fn into_usize(self) -> usize {
        let u = self.0.get();
        debug_assert!(u > 0);
        (u - 1) as usize
    }

    #[inline]
    pub fn into_u32(self) -> u32 {
        self.into_usize() as u32
    }
}

pub trait SlabKeyType {
    fn into(self) -> SlabKey;
    fn from(key: SlabKey) -> Self;
}

impl SlabKeyType for SlabKey {
    fn into(self) -> SlabKey {
        self
    }

    fn from(key: SlabKey) -> Self {
        key
    }
}

pub struct VacantEntry<'a, K: SlabKeyType, T> {
    entry: slab::VacantEntry<'a, T>,
    _phantom: PhantomData<K>,
}

impl<'a, K: SlabKeyType, T> fmt::Debug for VacantEntry<'a, K, T>
where
    K: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VacantEntry").field("key", &self.key()).finish()
    }
}

impl<'a, K: SlabKeyType, T> VacantEntry<'a, K, T> {
    pub fn key(&self) -> K {
        K::from(SlabKey::from_usize(self.entry.key()))
    }

    pub fn insert(self, val: T) -> &'a mut T {
        self.entry.insert(val)
    }
}

pub struct Iter<'a, K: SlabKeyType, T> {
    inner: slab::Iter<'a, T>,
    _phantom: PhantomData<K>,
}

impl<'a, K: SlabKeyType, T> Iterator for Iter<'a, K, T> {
    type Item = (K, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(key, val)| (K::from(SlabKey::from_usize(key)), val))
    }
}

pub struct IterMut<'a, K: SlabKeyType, T> {
    inner: slab::IterMut<'a, T>,
    _phantom: PhantomData<K>,
}

impl<'a, K: SlabKeyType, T> Iterator for IterMut<'a, K, T> {
    type Item = (K, &'a mut T);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(key, val)| (K::from(SlabKey::from_usize(key)), val))
    }
}

pub struct Drain<'a, T> {
    inner: slab::Drain<'a, T>,
}

impl<'a, T> Iterator for Drain<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl<K: SlabKeyType, T> Slab<K, T> {
    pub fn new() -> Self {
        Self {
            slab: slab::Slab::new(),
            _phantom: PhantomData,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            slab: slab::Slab::with_capacity(capacity),
            _phantom: PhantomData,
        }
    }

    pub fn capacity(&self) -> usize {
        self.slab.capacity()
    }

    pub fn reserve(&mut self, additional: usize) {
        self.slab.reserve(additional);
    }

    pub fn reserve_exact(&mut self, additional: usize) {
        self.slab.reserve_exact(additional);
    }

    pub fn shrink_to_fit(&mut self) {
        self.slab.shrink_to_fit();
    }

    pub fn clear(&mut self) {
        self.slab.clear();
    }

    pub fn len(&self) -> usize {
        self.slab.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slab.is_empty()
    }

    pub fn insert(&mut self, val: T) -> K {
        assert!((self.slab.len() as u32) < u32::MAX);
        K::from(SlabKey::from_usize(self.slab.insert(val)))
    }

    pub fn vacant_entry(&mut self) -> VacantEntry<'_, K, T> {
        assert!((self.slab.len() as u32) < u32::MAX);
        VacantEntry {
            entry: self.slab.vacant_entry(),
            _phantom: PhantomData,
        }
    }

    pub fn try_remove(&mut self, key: K) -> Option<T> {
        let slab_key = key.into();
        self.slab.try_remove(slab_key.into_usize())
    }

    pub fn remove(&mut self, key: K) -> T {
        let slab_key = key.into();
        self.slab.remove(slab_key.into_usize())
    }

    pub fn contains(&self, key: K) -> bool {
        let slab_key = key.into();
        self.slab.contains(slab_key.into_usize())
    }

    pub fn get(&self, key: K) -> Option<&T> {
        let slab_key = key.into();
        self.slab.get(slab_key.into_usize())
    }

    pub fn get_mut(&mut self, key: K) -> Option<&mut T> {
        let slab_key = key.into();
        self.slab.get_mut(slab_key.into_usize())
    }

    pub fn iter(&self) -> Iter<'_, K, T> {
        Iter {
            inner: self.slab.iter(),
            _phantom: PhantomData,
        }
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, K, T> {
        IterMut {
            inner: self.slab.iter_mut(),
            _phantom: PhantomData,
        }
    }

    pub fn drain(&mut self) -> Drain<'_, T> {
        Drain { inner: self.slab.drain() }
    }

    pub fn compact(&mut self, mut rekey: impl FnMut(&mut T, K, K)) {
        self.slab.compact(|value, from, to| {
            rekey(value, K::from(SlabKey::from_usize(from)), K::from(SlabKey::from_usize(to)));
            true
        });
    }
}

impl<K: SlabKeyType, T> Default for Slab<K, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: SlabKeyType, T> fmt::Debug for Slab<K, T>
where
    K: fmt::Debug,
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug_struct = f.debug_struct("Slab");
        debug_struct.field("len", &self.len());
        debug_struct.field("capacity", &self.capacity());

        let entries: Vec<_> = self.iter().collect();
        if !entries.is_empty() {
            debug_struct.field("entries", &entries);
        }

        debug_struct.finish()
    }
}

impl<K: SlabKeyType, T> Index<K> for Slab<K, T> {
    type Output = T;

    fn index(&self, key: K) -> &T {
        let slab_key = key.into();
        &self.slab[slab_key.into_usize()]
    }
}

impl<K: SlabKeyType, T> IndexMut<K> for Slab<K, T> {
    fn index_mut(&mut self, key: K) -> &mut T {
        let slab_key = key.into();
        &mut self.slab[slab_key.into_usize()]
    }
}
