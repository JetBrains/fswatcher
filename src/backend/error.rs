use std::io;

/// Errors that a backend may surface to its caller.
///
/// These are reserved for failures outside of the caller's control: IO and
/// system call failures while installing or removing a watch. Invalid arguments
/// (e.g. a non-canonical or empty path) are caller bugs and panic instead.
#[derive(Debug)]
pub enum BackendError {
    /// An IO error has occurred while adding the watch. Maybe the requested directory does not exist, or it is not a directory.
    IO(io::Error),
    /// The watch was requested with [`super::ParentPolicy::RequireWatchedParent`], but the parent of
    /// the canonical path is not watched by the same subscription. The chain leading from a watched
    /// root down to this path is not contiguous, and the derived canonical path is untrustworthy.
    DetachedParent,
}

impl From<io::Error> for BackendError {
    fn from(value: io::Error) -> Self {
        BackendError::IO(value)
    }
}
