use std::ops::{Deref, DerefMut};

pub mod id_set;
pub mod id_source;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod lifetimes;
pub mod minislab;
pub mod multi_map;
pub mod names;
#[cfg(target_os = "windows")]
pub mod option_util;
pub mod path_util;
pub mod result_util;
pub mod resync_channel;
#[cfg(target_os = "windows")]
pub mod tokio_util;

#[cfg(target_os = "windows")]
pub type BoxedStream<T> = futures::stream::BoxStream<'static, T>;

/// Simple wrapper to help derive `Debug` for types that contain non-`Debug` fields.
pub struct Debug<T>(T);

impl<T> Debug<T> {
    pub fn wrap(t: T) -> Self {
        Debug(t)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for Debug<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Debug<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> std::fmt::Debug for Debug<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(std::any::type_name::<T>())
    }
}
