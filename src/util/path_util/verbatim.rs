use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf, Prefix},
};

use super::PathExt;

const VERBATIM_PREFIX: &str = "\\\\?\\";
const VERBATIM_UNC_PREFIX: &str = "\\\\?\\UNC\\";

/*
    https://docs.microsoft.com/en-us/windows/win32/fileio/maximum-file-path-limitation
*/
// TODO maybe try to avoid allocations when conversion is not needed, Cow?
pub fn to_verbatim(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();

    if !cfg!(target_os = "windows") {
        return path.to_path_buf();
    }

    if path.is_empty_path() {
        return PathBuf::new();
    }

    if path.is_relative() {
        panic!("UNC paths can't be relative: [{path:?}]");
    }

    let mut components = path.components();
    if let Component::Prefix(prefix) = components.next().expect("path is relative or empty") {
        if prefix.kind().is_verbatim() {
            path.to_path_buf()
        } else if let Prefix::UNC(server, share) = prefix.kind() {
            let prefix_os = OsStr::new(VERBATIM_UNC_PREFIX);
            let remaining_os = components.as_path().as_os_str();
            let mut os_string = OsString::with_capacity(prefix_os.len() + server.len() + share.len() + 2 + remaining_os.len());
            os_string.push(prefix_os);
            os_string.push(server);
            os_string.push("\\");
            os_string.push(share);
            os_string.push("\\");
            let mut r = PathBuf::from(os_string);
            for c in components {
                r.push(c)
            }
            r
        } else if matches!(prefix.kind(), Prefix::DeviceNS(_)) {
            // `\\.\device\...` already bypasses the limit this conversion exists for, and callers dispatch on the
            // literal prefix (e.g. `\\.\pipe\`), so it is left as is. Prepending would yield `\\?\\\.\device\...`.
            path.to_path_buf()
        } else {
            let verbatim_os = OsStr::new(VERBATIM_PREFIX);
            let path_os = path.as_os_str();
            let mut os_string = OsString::with_capacity(path_os.len() + verbatim_os.len());
            os_string.push(verbatim_os);
            os_string.push(prefix.as_os_str());
            let mut r = PathBuf::from(os_string);
            for c in components {
                r.push(c);
            }
            r
        }
    } else {
        panic!("Path should have a prefix: {path:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unc_prefix() {
        if cfg!(target_os = "windows") {
            assert_eq!(PathBuf::from("\\\\?\\C:\\sample\\path"), to_verbatim(Path::new("C:\\sample\\path")));
            assert_eq!(
                PathBuf::from("\\\\?\\C:\\sample\\path"),
                to_verbatim(Path::new("\\\\?\\C:\\sample\\path"))
            );

            assert_eq!(
                PathBuf::from("\\\\?\\UNC\\VBOXSVR\\fsd\\doc"),
                to_verbatim(Path::new("\\\\VBOXSVR\\fsd\\doc"))
            );
            assert_eq!(
                PathBuf::from("\\\\?\\C:\\sample\\path\\component"),
                to_verbatim(Path::new("C:\\sample/path/component"))
            );
            assert_eq!(
                PathBuf::from("\\\\?\\UNC\\VBOXSVR\\fsd\\doc\\doc"),
                to_verbatim(Path::new("\\\\VBOXSVR\\fsd/doc/doc"))
            );

            // Device namespace paths are passed through untouched.
            assert_eq!(
                PathBuf::from("\\\\.\\pipe\\openssh-ssh-agent"),
                to_verbatim(Path::new("\\\\.\\pipe\\openssh-ssh-agent"))
            );
        } else {
            assert_eq!(PathBuf::from("/Users/user/path"), to_verbatim(Path::new("/Users/user/path")))
        }
    }
}
