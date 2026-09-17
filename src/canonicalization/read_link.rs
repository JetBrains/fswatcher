use std::{
    io,
    path::{Path, PathBuf},
};

use crate::util::path_util::PathExt;

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum ReadLink {
    Symlink(PathBuf),
    NotALink,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn read_link(path: &Path) -> Result<ReadLink, io::Error> {
    use std::{fs, io};
    use tracing::trace;

    match fs::read_link(path) {
        Ok(target) => {
            trace!(?path, ?target, "path is a symlink");
            let target = if target.is_absolute() {
                target
            } else {
                path.parent().expect("unexpected root").join(target).normalise()
            };
            Ok(ReadLink::Symlink(target))
        }
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
            trace!(?path, "path is not a symlink");
            Ok(ReadLink::NotALink)
        }
        Err(err) => {
            trace!(?err, ?path, "failed to access the file");
            Err(err)
        }
    }
}

#[cfg(target_os = "windows")]
pub fn read_link(path: &Path) -> Result<ReadLink, io::Error> {
    use std::fs;
    use tracing::trace;
    use windows::Win32::Foundation::ERROR_NOT_A_REPARSE_POINT;

    match fs::read_link(&path) {
        Ok(target) => {
            let target = if target.is_absolute() {
                target
            } else {
                path.parent().expect("unexpected root").join(target).normalise()
            };
            Ok(ReadLink::Symlink(target.to_verbatim()))
        }
        Err(e) if e.raw_os_error() == Some(ERROR_NOT_A_REPARSE_POINT.0 as i32) => Ok(ReadLink::NotALink),
        Err(err) => {
            trace!(?err, path_prefix = %path.display(), "failed to access the file");
            Err(err)
        }
    }
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use crate::test_helpers::{files, symlink};

    use super::{read_link, ReadLink};

    #[test]
    fn absolute_symlink() {
        let dir = files!({ "dir" => {} });
        let symlink_path = dir.path().join("dir/symlink");
        let symlink_target_path = dir.path().join("symlink_target");
        symlink(&symlink_target_path, &symlink_path);

        assert_eq!(ReadLink::Symlink(symlink_target_path), read_link(&symlink_path).unwrap());
    }

    #[test]
    fn relative_link_is_normalized() {
        let dir = files!({ "dir" => {} });
        let symlink_path = dir.path().join("dir/symlink");
        symlink(Path::new("../symlink_target"), &symlink_path);

        assert_eq!(
            ReadLink::Symlink(dir.path().join("symlink_target")),
            read_link(&symlink_path).unwrap()
        );
    }

    #[test]
    fn on_directory() {
        let dir = files!({ "dir" => {} });
        assert_eq!(ReadLink::NotALink, read_link(&dir.path().join("dir")).unwrap());
    }

    #[test]
    fn on_regular_file() {
        let dir = files!({ "file" => "content" });
        assert_eq!(ReadLink::NotALink, read_link(&dir.path().join("file")).unwrap());
    }

    #[test]
    fn on_non_existing_file() {
        let dir = files!({});

        assert!(read_link(&dir.path().join("does-not-exist")).is_err());
    }
}
