pub trait ResultExt {
    type Error;

    // identical to unstable inspect_err
    fn log(self, f: impl FnOnce(&Self::Error)) -> Self;
}

impl<T, E> ResultExt for Result<T, E> {
    type Error = E;

    fn log(self, f: impl FnOnce(&Self::Error)) -> Self {
        if let Err(err) = &self {
            f(err);
        }
        self
    }
}
