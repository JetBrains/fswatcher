use std::{
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

/// Shared source of NonZeroU32 ids.
///
/// NonZero is convenient because Option<NonZero<T>> is the same size as NonZero<T>.
/// u32 ought to be enough for anybody.
pub struct IdSource(AtomicU32);

impl IdSource {
    pub const fn new() -> Self {
        IdSource::starting_with(unsafe { NonZeroU32::new_unchecked(1) })
    }

    pub const fn starting_with(first: NonZeroU32) -> Self {
        IdSource(AtomicU32::new(first.get()))
    }

    #[inline]
    pub fn next(&self) -> NonZeroU32 {
        NonZeroU32::new(self.0.fetch_add(1, Ordering::Relaxed)).expect("id is zero")
    }
}

impl Default for IdSource {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for IdSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("IdSource").field(&self.0.load(Ordering::Relaxed)).finish()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // This contract might be important for some clients because they might want to reserve some ids
    #[test]
    fn first() {
        let id_source = IdSource::starting_with(NonZeroU32::new(1).unwrap());
        assert_eq!(id_source.next().get(), 1);
        assert_eq!(id_source.next().get(), 2);

        let id_source = IdSource::starting_with(NonZeroU32::new(2).unwrap());
        assert_eq!(id_source.next().get(), 2);
        assert_eq!(id_source.next().get(), 3);
    }
}
