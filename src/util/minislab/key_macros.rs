/// Declares a newtype key for a [`super::Slab`].
///
/// Accepts any visibility: `key!(RecordId)`, `key!(pub RecordId)`, `key!(pub(super) RecordId)`.
macro_rules! key {
    ($vis:vis $key_type:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis struct $key_type($crate::util::minislab::SlabKey);

        impl $crate::util::minislab::SlabKeyType for $key_type {
            fn into(self) -> $crate::util::minislab::SlabKey {
                self.0
            }

            fn from(key: $crate::util::minislab::SlabKey) -> Self {
                Self(key)
            }
        }
    };
}

pub(crate) use key;

#[allow(dead_code)]
#[cfg(test)]
mod test {
    // test macro expansion

    key!(PrivateKey);

    key!(pub PublicKey);

    key!(pub(crate) PubCrateKey);

    key!(pub(super) PubSuperKey);
}
