use std::{
    borrow::Cow,
    ffi::OsStr,
    path::{Component, Path},
};

/// Returns the first component of the path and its suffix if the path is not empty.
///
/// Important distinction from [`std::path::Components`]: on Windows it will combine Prefix and Root into one string.
/// ```rust,ignore-linux,ignore-darwin
/// use std::path::Path;
/// let (head, tail) = crate::util::path_util::head(Path::new("C:\\some")).unwrap();
/// assert_eq!(std::ffi::OsStr::new("C:\\"), head);
/// assert_eq!(Path::new("some"), tail);
/// ```
pub fn head(path: &Path) -> Option<(Cow<'_, OsStr>, &Path)> {
    let mut components = path.components();
    let first = components.next();
    let first_tail = components.as_path();
    let second = components.next();
    match (first, second) {
        (Some(Component::Prefix(prefix)), Some(Component::RootDir)) => {
            let mut prefix = prefix.as_os_str().to_os_string();
            prefix.push(Component::RootDir.as_os_str());
            Some((Cow::Owned(prefix), components.as_path()))
        }
        (Some(first), _) => Some((Cow::Borrowed(first.as_os_str()), first_tail)),
        (None, _) => None,
    }
}

#[cfg(test)]
mod test {
    use crate::test_helpers::assert_eq_pretty;

    use super::*;

    #[test]
    fn test_head() {
        assert_eq!(head(Path::new("")), None);
        if cfg!(target_os = "windows") {
            {
                let (head, tail) = head(Path::new("C:\\")).unwrap();
                assert_eq!(head, OsStr::new("C:\\"));
                assert_eq!(tail, Path::new(""));
            }
            {
                let (head, tail) = head(Path::new("C:\\segment\\another")).unwrap();
                assert_eq!(head, OsStr::new("C:\\"));
                assert_eq!(tail, Path::new("segment\\another"));
            }
            {
                let (head, tail) = head(Path::new("segment\\another")).unwrap();
                assert_eq!(head, OsStr::new("segment"));
                assert_eq!(tail, Path::new("another"));
            }
            {
                let (head, tail) = head(Path::new("another")).unwrap();
                assert_eq!(head, OsStr::new("another"));
                assert_eq!(tail, Path::new(""));
            }
            {
                let (head, tail) = head(Path::new("\\\\?\\C:\\component")).unwrap();
                assert_eq!(head, OsStr::new("\\\\?\\C:\\"));
                assert_eq!(tail, Path::new("component"));
            }
            {
                let (head, tail) = head(Path::new("\\\\?\\C:\\")).unwrap();
                assert_eq!(head, OsStr::new("\\\\?\\C:\\"));
                assert_eq!(tail, Path::new(""));
            }
        } else {
            {
                let (head, tail) = head(Path::new("/")).unwrap();
                assert_eq!(head, OsStr::new("/"));
                assert_eq!(tail, Path::new(""));
            }
            {
                let (head, tail) = head(Path::new("/segment/another")).unwrap();
                assert_eq!(head, OsStr::new("/"));
                assert_eq!(tail, Path::new("segment/another"));
            }
            {
                let (head, tail) = head(Path::new("segment/another")).unwrap();
                assert_eq!(head, OsStr::new("segment"));
                assert_eq!(tail, Path::new("another"));
            }
            {
                let (head, tail) = head(Path::new("another")).unwrap();
                assert_eq!(head, OsStr::new("another"));
                assert_eq!(tail, Path::new(""));
            }
            let components = Path::new("/segment/another/segment")
                .components()
                .map(|it| it.as_os_str())
                .collect::<Vec<&OsStr>>();
            let head_components = {
                let mut v = Vec::new();
                let mut path = Path::new("/segment/another/segment");
                while let Some((head, tail)) = head(path) {
                    v.push(head);
                    path = tail;
                }
                v
            };
            assert_eq_pretty!(components, head_components);
            {
                let (head, tail) = head(Path::new("segment/another/")).unwrap();
                assert_eq!(head, OsStr::new("segment"));
                assert_eq!(tail, Path::new("another"));
            }
        }
    }
}
