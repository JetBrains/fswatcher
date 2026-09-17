use std::path::PathBuf;

use super::FileType;

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Hash))]
pub enum Event {
    /// Indicates that a file or a directory exists in a new state.
    ///
    /// In the case of files and symlinks, it is emitted whenever
    /// - the entry is created
    /// - the content is changed
    /// - the metadata is changed
    /// - it is replaced with another file
    ///
    /// For directories:
    /// - a new directory is created
    /// - the directory metadata is changed.
    ///
    /// Do not assume a directory is created empty. If it was moved in from somewhere else, it might already have children.
    Dirty { path: PathBuf, file_type: FileType },
    /// Indicates that there is no more entry at the given symbolic path.
    /// Same for all file types:
    /// - the entry itself is removed
    /// - the entry is renamed
    /// - one of the ancestors is renamed
    /// - one of the ancestors is a symlink and it cannot be resolved anymore
    Removed { path: PathBuf },
    /// Covers situations where the filesystem state cannot be deduced from events alone.
    /// - the operating system has reported an internal buffer overflow
    /// - the client was too slow to consume events and ran out of buffer space
    /// - the path could be targeting a different file system entry because of changes in symlinks
    /// - the event reported by the operating system doesn't have enough information to understand what has happened
    Rescan { path: PathBuf },
}

impl AsRef<Event> for Event {
    fn as_ref(&self) -> &Event {
        self
    }
}
