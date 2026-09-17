use super::PathExt;
use anyhow::anyhow;
use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

/// A path buffer with a symmetrical push and pop. `PathBuf::pop` returns `false` on a root, this type pops it.
/// It merges Prefix and Root on Windows to be consistent with [super::head::head].
#[derive(Debug, Clone)]
pub struct PathStack {
    buf: PathBuf,
}

impl PathStack {
    pub fn new() -> Self {
        Self { buf: PathBuf::new() }
    }

    pub fn push(&mut self, segment: &OsStr) -> anyhow::Result<()> {
        let path = Path::new(segment);
        let mut components = path.components();
        let first = components.next();
        let second = components.next();
        let third = components.next();

        match (first, second, third) {
            (None, _, _) => return Err(anyhow!("empty segment")),
            // Merged Windows root: "C:\\" or "\\\\?\\C:\\" (Prefix + RootDir as one unit)
            (Some(Component::Prefix(_)), Some(Component::RootDir), None) => {
                if !self.buf.is_empty_path() {
                    return Err(anyhow!("shouldn't push root segment to a non-empty buffer, segment: {:?}", segment));
                }
                self.buf.push(segment);
            }
            (Some(Component::Prefix(_)), None, None) => {
                return Err(anyhow!(
                    "on Windows push prefix as the merged form (e.g. \"C:\\\\\"), separate prefix is not allowed, segment: {:?}",
                    segment
                ));
            }
            // Root component: "/" on Unix only.
            // On Windows the root must always be the merged "C:\\" form (Prefix + RootDir);
            // accepting a bare RootDir would break pop symmetry because pop_root clears the
            // entire buffer (wiping any preceding Prefix) on a single pop.
            (Some(Component::RootDir), None, None) => {
                if cfg!(windows) {
                    return Err(anyhow!(
                        "on Windows push root as the merged form (e.g. \"C:\\\\\"), separate root is not allowed, segment: {:?}",
                        segment
                    ));
                }
                if !self.buf.is_empty_path() {
                    return Err(anyhow!("shouldn't push root to a non-empty buffer, segment: {:?}", segment));
                }
                self.buf.push(segment);
            }
            // Single normal/relative component
            (Some(Component::Normal(_) | Component::CurDir | Component::ParentDir), None, None) => {
                self.buf.push(segment);
            }
            _ => return Err(anyhow!("more than one component: {:?}", segment)),
        }
        Ok(())
    }

    #[inline]
    pub fn as_path(&self) -> &Path {
        self.buf.as_path()
    }

    #[cfg(test)]
    pub fn parent(&self) -> Option<&Path> {
        parent(self.buf.as_path())
    }

    #[inline]
    pub fn pop(&mut self) -> bool {
        self.buf.pop() || pop_root(&mut self.buf)
    }
}

/// Returns the parent path for a PathStack buffer.
///
/// Treats the merged Prefix+RootDir ("C:\\") as a single unit, so parent("C:\\") returns Some("").
#[cfg(test)]
fn parent(path: &Path) -> Option<&Path> {
    path.parent().or_else(|| {
        let mut components = path.components();
        if let Some(last) = components.next_back() {
            match &last {
                Component::Prefix(_) => Some(Path::new("")),
                Component::RootDir => {
                    // PathStack merges Prefix+RootDir into one unit,
                    // so parent of both "/" and "C:\\" is always empty
                    Some(Path::new(""))
                }
                Component::Normal(_) | Component::ParentDir | Component::CurDir => {
                    panic!("unexpected component: {last:?}");
                }
            }
        } else {
            None
        }
    })
}

fn pop_root(buf: &mut PathBuf) -> bool {
    let mut components = buf.components();
    if let Some(last) = components.next_back() {
        match last {
            Component::Prefix(_) => {
                buf.clear();
            }
            Component::RootDir => {
                // PathStack treats Prefix+RootDir as one unit, so clear both at once
                buf.clear();
            }
            last @ Component::Normal(_) | last @ Component::ParentDir | last @ Component::CurDir => {
                panic!("unexpected component: {last:?}");
            }
        }
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn check_symmetrical(path: impl AsRef<Path>) {
        let path = path.as_ref();
        let mut stack = PathStack::new();
        let mut current = path;
        let mut depth = 0;
        while let Some((seg, tail)) = crate::util::path_util::head(current) {
            stack.push(&seg).unwrap();
            depth += 1;
            current = tail;
        }
        while depth != 0 {
            let expected_parent = stack.parent().map(|p| p.to_path_buf());
            assert!(stack.pop(), "pop should succeed at depth {depth}");
            let after_pop = stack.as_path().to_path_buf();
            match expected_parent {
                Some(p) => assert_eq!(p, after_pop, "parent mismatch at depth {depth}"),
                None => assert!(after_pop.is_empty_path(), "expected empty at depth {depth}, got {:?}", after_pop),
            }
            depth -= 1;
        }
        assert!(!stack.pop(), "pop on empty stack should return false");
        assert!(stack.as_path().is_empty_path());
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::ffi::OsStr;

        #[test]
        fn root_only() {
            check_symmetrical("/");
        }

        #[test]
        fn absolute_path() {
            check_symmetrical("/Users/user/directory");
        }

        #[test]
        fn relative_path() {
            check_symmetrical("Users/user/directory");
        }

        #[test]
        fn single_segment() {
            check_symmetrical("file.txt");
        }

        #[test]
        fn error_empty_segment() {
            let mut s = PathStack::new();
            assert!(s.push(OsStr::new("")).is_err());
        }

        #[test]
        fn error_root_pushed_twice() {
            let mut s = PathStack::new();
            assert!(s.push(OsStr::new("/")).is_ok());
            assert!(s.push(OsStr::new("/")).is_err());
        }

        #[test]
        fn error_multi_component() {
            let mut s = PathStack::new();
            assert!(s.push(OsStr::new("a/b")).is_err());
        }

        #[test]
        fn parent_of_root_is_empty() {
            let mut s = PathStack::new();
            s.push(OsStr::new("/")).unwrap();
            assert!(s.parent().unwrap().is_empty_path());
        }

        #[test]
        fn parent_of_empty_is_none() {
            let s = PathStack::new();
            assert!(s.parent().is_none());
        }
    }

    #[cfg(target_os = "windows")]
    mod windows {
        use super::PathExt;
        use super::*;
        use std::ffi::OsStr;

        #[test]
        fn drive_root() {
            check_symmetrical("C:\\");
        }

        #[test]
        fn absolute_path() {
            check_symmetrical("C:\\Users\\user\\directory");
        }

        #[test]
        fn relative_path() {
            check_symmetrical("Users\\user\\directory");
        }

        #[test]
        fn verbatim_path() {
            check_symmetrical(Path::new("C:\\Users\\user\\directory").to_verbatim());
        }

        #[test]
        fn error_empty_segment() {
            let mut s = PathStack::new();
            assert!(s.push(OsStr::new("")).is_err());
        }

        #[test]
        fn error_drive_root_pushed_twice() {
            let mut s = PathStack::new();
            assert!(s.push(OsStr::new("C:\\")).is_ok());
            assert!(s.push(OsStr::new("C:\\")).is_err());
        }

        #[test]
        fn error_multi_component() {
            let mut s = PathStack::new();
            assert!(s.push(OsStr::new("a\\b")).is_err());
        }

        #[test]
        fn error_bare_prefix() {
            let mut s = PathStack::new();
            assert!(s.push(OsStr::new("C:")).is_err());
        }

        #[test]
        fn error_bare_root() {
            let mut s = PathStack::new();
            assert!(s.push(OsStr::new("\\")).is_err());
        }

        #[test]
        fn parent_of_drive_root_is_empty() {
            // PathStack merges Prefix+RootDir, so parent("C:\\") == ""
            // (unlike ComponentsStack where parent("C:\\") == "C:")
            let mut s = PathStack::new();
            s.push(OsStr::new("C:\\")).unwrap();
            assert!(s.parent().unwrap().is_empty_path());
        }

        #[test]
        fn parent_of_absolute_path_is_drive_root() {
            let mut s = PathStack::new();
            s.push(OsStr::new("C:\\")).unwrap();
            s.push(OsStr::new("Users")).unwrap();
            assert_eq!(s.parent().unwrap(), Path::new("C:\\"));
        }

        #[test]
        fn parent_of_empty_is_none() {
            let s = PathStack::new();
            assert!(s.parent().is_none());
        }
    }
}
