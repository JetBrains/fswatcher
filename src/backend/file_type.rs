use std::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileType {
    Directory,
    Regular,
    Symlink,
    Other,
}

impl FileType {
    pub fn from_metadata(metadata: &fs::Metadata) -> Self {
        match metadata.file_type() {
            // NOTE: the ordering here may matter. On Windows, a file can be both a symlink and a directory simultaneously.
            // On other platforms these flags are exclusive. To preserve the overall semantics, the is_symlink check
            // must go first.
            ft if ft.is_symlink() => FileType::Symlink,
            ft if ft.is_dir() => FileType::Directory,
            ft if ft.is_file() => FileType::Regular,
            _ => FileType::Other,
        }
    }

    pub fn is_symlink(self) -> bool {
        self == FileType::Symlink
    }

    pub fn is_regular(self) -> bool {
        self == FileType::Regular
    }

    pub fn is_directory(self) -> bool {
        self == FileType::Directory
    }

    pub fn is_other(self) -> bool {
        self == FileType::Other
    }
}
