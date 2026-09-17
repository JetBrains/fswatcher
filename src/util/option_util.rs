pub trait OptionExt {
    /// Calls a function if [`None`].
    ///
    /// Returns the original option.
    fn inspect_none<F: FnOnce()>(self, f: F) -> Self;
}

impl<T> OptionExt for Option<T> {
    #[inline]
    fn inspect_none<F: FnOnce()>(self, f: F) -> Self {
        if self.is_none() {
            f();
        }
        self
    }
}
