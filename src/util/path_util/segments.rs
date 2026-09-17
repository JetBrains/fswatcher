use std::{borrow::Cow, ffi::OsStr, path::Path};

use super::{head, prefixes, Prefixes};

pub fn segments(path: &Path) -> PathSegments<'_> {
    let prefixes = prefixes(path);
    PathSegments {
        previous_prefix: Path::new(""),
        prefixes,
        suffix: path,
    }
}

pub struct PathSegments<'a> {
    previous_prefix: &'a Path,
    prefixes: Prefixes<'a>,
    suffix: &'a Path,
}

/// prefix + component + suffix constitute the original path
pub struct PathSegment<'a> {
    pub prefix: &'a Path,
    component: Cow<'a, OsStr>,
    pub suffix: &'a Path,

    /// Same as prefix + component
    pub prefix_inclusive: &'a Path,
}

impl<'a> std::fmt::Debug for PathSegment<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathSegment")
            .field("prefix", &self.prefix)
            .field("component", &self.component)
            .field("suffix", &self.suffix)
            .finish()
    }
}

impl<'a> PathSegment<'a> {
    pub fn as_os_str(&self) -> &OsStr {
        self.component.as_ref()
    }
}

impl<'a> Iterator for PathSegments<'a> {
    type Item = PathSegment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((head, tail)) = head(self.suffix) {
            let prefix = self.previous_prefix;
            let prefix_inclusive = self.prefixes.next().expect("prefixes iterator is shorter than components iterator");
            let component = head;
            let suffix = tail;
            self.previous_prefix = prefix_inclusive;
            self.suffix = tail;
            Some(PathSegment {
                prefix,
                component,
                suffix,
                prefix_inclusive,
            })
        } else {
            None
        }
    }
}
