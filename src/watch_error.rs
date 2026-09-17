use super::{backend::BackendError, canonicalization::CanonicalizationError};

/// Errors propagated to clients of the watch session.
///
/// All variants represent situations outside of the caller's control: backend
/// IO failures, the live filesystem state (canonicalization), and natural races.
/// Invalid arguments (empty or relative paths, etc.) are caller bugs and panic.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("backend has returned an error: {0:?}")]
    Backend(BackendError),
    #[error("canonicalization of the requestsed path is not (yet) available: {0:?}")]
    CanonicalizationError(CanonicalizationError),
}

impl From<BackendError> for WatchError {
    fn from(value: BackendError) -> Self {
        WatchError::Backend(value)
    }
}

impl From<CanonicalizationError> for WatchError {
    fn from(value: CanonicalizationError) -> Self {
        WatchError::CanonicalizationError(value)
    }
}
