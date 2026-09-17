use std::path::Path;

use super::PathExt;

/// Iterator of all prefixes of the path including the path itself.
///```
///use std::path::Path;
///use crate::util::path_util::prefixes;
///assert_eq!(
///    prefixes(Path::new("/Users/user/Documents/document")).collect::<Vec<_>>(),
///    vec![
///        Path::new("/"),
///        Path::new("/Users"),
///        Path::new("/Users/user"),
///        Path::new("/Users/user/Documents"),
///        Path::new("/Users/user/Documents/document"),
///    ],
///)
///```
pub fn prefixes(path: &Path) -> Prefixes<'_> {
    let yield_empty = path.is_empty_path();
    #[cfg(windows)]
    {
        let vec = path.ancestors().collect::<Vec<_>>();
        let iter = vec.into_iter().rev();

        Prefixes { iter, yield_empty }
    }
    #[cfg(unix)]
    {
        Prefixes {
            original: path,
            yield_empty,
            components: path.components(),
        }
    }
}

#[cfg(windows)]
pub struct Prefixes<'a> {
    yield_empty: bool,
    iter: std::iter::Rev<std::vec::IntoIter<&'a Path>>,
}

#[cfg(windows)]
impl<'a> Iterator for Prefixes<'a> {
    type Item = &'a Path;

    fn next(&mut self) -> Option<Self::Item> {
        if self.yield_empty {
            self.yield_empty = false;
            Some(Path::new(""))
        } else {
            // on relative paths `ancestors` will end the iteration with empty path
            self.iter.next().take_if(|it| !it.is_empty_path()).or_else(|| self.iter.next())
        }
    }
}

#[cfg(unix)]
pub struct Prefixes<'a> {
    original: &'a Path,
    components: std::path::Components<'a>,
    yield_empty: bool,
}

#[cfg(unix)]
impl<'a> Iterator for Prefixes<'a> {
    type Item = &'a Path;

    fn next(&mut self) -> Option<Self::Item> {
        use std::os::unix::ffi::OsStrExt;

        if self.yield_empty {
            self.yield_empty = false;
            Some(Path::new(""))
        } else if self.components.next().is_some() {
            use std::ffi::OsStr;

            let remaining_bytes = self.components.as_path().as_os_str().len();
            let total_len = self.original.as_os_str().len();
            let bytes = &self.original.as_os_str().as_bytes()[0..total_len - remaining_bytes];
            Some(Path::new::<OsStr>(OsStrExt::from_bytes(bytes)))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::Path;

    #[test]
    fn empty_path() {
        let empty_path = Path::new("");
        let actual = prefixes(empty_path).collect::<Vec<_>>();
        let expected = vec![Path::new("")];
        assert_eq!(expected, actual);
    }

    #[test]
    fn absolute_path() {
        if cfg!(target_os = "windows") {
            let absolute = Path::new("C:\\segment\\another segment\\document");
            let actual = prefixes(absolute).collect::<Vec<_>>();
            let expected = vec![
                Path::new("C:\\"),
                Path::new("C:\\segment"),
                Path::new("C:\\segment\\another segment"),
                Path::new("C:\\segment\\another segment\\document"),
            ];
            assert_eq!(expected, actual);
        } else {
            let absolute = Path::new("/Users/user/Documents/document");
            let actual = prefixes(absolute).collect::<Vec<_>>();
            let expected = vec![
                Path::new("/"),
                Path::new("/Users"),
                Path::new("/Users/user"),
                Path::new("/Users/user/Documents"),
                Path::new("/Users/user/Documents/document"),
            ];
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn relative_path() {
        if cfg!(target_os = "windows") {
            let relative = Path::new("segment\\another segment\\document");
            let actual = prefixes(relative).collect::<Vec<_>>();
            let expected = vec![
                Path::new("segment"),
                Path::new("segment\\another segment"),
                Path::new("segment\\another segment\\document"),
            ];
            assert_eq!(expected, actual);
        } else {
            let relative = Path::new("Documents/Another documents/document");
            let actual = prefixes(relative).collect::<Vec<_>>();
            let expected = vec![
                Path::new("Documents"),
                Path::new("Documents/Another documents"),
                Path::new("Documents/Another documents/document"),
            ];
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn unc_path() {
        if cfg!(target_os = "windows") {
            let unc = Path::new("\\\\?\\UNC\\VBOXSVR\\fsd\\doc\\doc");
            let actual = prefixes(unc).collect::<Vec<_>>();
            let expected = vec![
                Path::new("\\\\?\\UNC\\VBOXSVR\\fsd\\"),
                Path::new("\\\\?\\UNC\\VBOXSVR\\fsd\\doc"),
                Path::new("\\\\?\\UNC\\VBOXSVR\\fsd\\doc\\doc"),
            ];
            assert_eq!(expected, actual);

            let unc_drive = Path::new("\\\\?\\C:\\doc\\doc");
            let actual = prefixes(unc_drive).collect::<Vec<_>>();
            let expected = vec![
                Path::new("\\\\?\\C:\\"),
                Path::new("\\\\?\\C:\\doc"),
                Path::new("\\\\?\\C:\\doc\\doc"),
            ];
            assert_eq!(expected, actual);
        }
    }
}
