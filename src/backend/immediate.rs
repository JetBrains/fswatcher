use std::{path::PathBuf, thread};

use futures::stream::BoxStream;

#[derive(Debug)]
pub struct Changed;

#[derive(Debug, thiserror::Error)]
pub enum ImmediateError {
    #[error("Requested path is not a regular file")]
    NotAFile,
    #[error("Couldn't watch because of IO error: {0}")]
    IOError(std::io::Error),
}

/// Windows and macOS don't send notifications about content changes while the file descriptor is open.
/// Usually this doesn't matter, but the watched file might be something like a log that never closes while its writer is running.
/// This is fine for indexing scenarios, but in interactive mode (for example, when the file is open in an editor) we want to be notified immediately.
///
/// Only regular files can be watched in this mode.
pub trait ImmediateWatcher {
    /// The stream is closed if the file is renamed or deleted.
    fn watch_immediate_changes(&self, canonical_path: PathBuf) -> Result<BoxStream<'static, Changed>, ImmediateError>;
    fn shutdown_and_join(self: Box<Self>) -> thread::Result<()>;
}

#[allow(unreachable_code)]
pub fn immediate_watcher_impl() -> anyhow::Result<Option<Box<dyn ImmediateWatcher + Send + Sync>>> {
    #[cfg(target_os = "macos")]
    {
        use super::macos::immediate::KQueueImmediateWatcher;

        return Ok(Some(Box::new(KQueueImmediateWatcher::create()?)));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(None);
    }
    #[cfg(target_os = "windows")]
    {
        use super::windows::immediate::TimerImmediateWatcher;

        return Ok(Some(Box::new(TimerImmediateWatcher)));
    }
    unreachable!("only macos, linux and windows targets are supported")
}

impl From<std::io::Error> for ImmediateError {
    fn from(value: std::io::Error) -> Self {
        Self::IOError(value)
    }
}
