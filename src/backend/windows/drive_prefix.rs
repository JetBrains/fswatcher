use std::{
    ffi::{OsStr, OsString},
    ops::Deref,
    path::{Path, PathBuf},
};

use crate::util::path_util::head;

#[derive(Debug, Clone, Eq, PartialEq, std::hash::Hash)]
pub struct DrivePrefix(OsString);

impl DrivePrefix {
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(self.as_os_str())
    }

    pub fn from_os_string(string: OsString) -> Self {
        DrivePrefix(string)
    }
}

impl Deref for DrivePrefix {
    type Target = OsString;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<OsStr> for DrivePrefix {
    fn as_ref(&self) -> &OsStr {
        self.0.as_ref()
    }
}

impl AsRef<Path> for DrivePrefix {
    fn as_ref(&self) -> &Path {
        Path::new(self.0.as_os_str())
    }
}

#[derive(Debug)]
pub struct NoPrefix;

pub fn extract_prefix(path: &Path) -> Result<(DrivePrefix, &Path), NoPrefix> {
    match head(path) {
        Some((head, tail)) => Ok((DrivePrefix(head.to_os_string()), tail)),
        None => Err(NoPrefix),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_extract_prefix() {
        let (prefix, relative) = extract_prefix(Path::new("C:\\")).unwrap();
        assert_eq!(prefix.as_os_str(), OsString::from("C:\\"));
        assert_eq!(relative, Path::new(""));

        let (prefix, relative) = extract_prefix(Path::new("C:\\some\\path")).unwrap();
        assert_eq!(prefix.as_os_str(), OsString::from("C:\\"));
        assert_eq!(relative, Path::new("some\\path"));

        let (prefix, relative) = extract_prefix(Path::new("\\\\?\\C:\\cygwin64\\tmp\\testing-dir.AgBTd7JCXxKe")).unwrap();
        assert_eq!(prefix.as_os_str(), OsString::from("\\\\?\\C:\\"));
        assert_eq!(relative, Path::new("cygwin64\\tmp\\testing-dir.AgBTd7JCXxKe"));

        let (prefix, relative) = extract_prefix(Path::new("\\\\VBOXSVR\\fsd\\doc")).unwrap();
        assert_eq!(prefix.as_os_str(), OsString::from("\\\\VBOXSVR\\fsd\\"));
        assert_eq!(relative, Path::new("doc"));

        let (prefix, relative) = extract_prefix(Path::new("\\\\?\\UNC\\VBOXSVR\\fsd\\doc")).unwrap();
        assert_eq!(prefix.as_os_str(), OsString::from("\\\\?\\UNC\\VBOXSVR\\fsd\\"));
        assert_eq!(relative, Path::new("doc"));
    }
}
