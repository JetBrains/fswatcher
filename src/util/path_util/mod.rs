use std::path::{Component, Path, PathBuf};

mod head;
pub mod path_stack;
mod prefixes;
mod segments;
#[cfg(target_os = "windows")]
mod verbatim;

pub use head::*;
pub use prefixes::*;
pub use segments::*;

pub trait PathExt {
    #[cfg(target_os = "windows")]
    fn to_verbatim(&self) -> PathBuf;
    /// standard library plans to add is_empty on Path so this name is reserved
    fn is_empty_path(&self) -> bool;
    fn prefixes(&self) -> Prefixes<'_>;

    /// Performs a simple path normalization without resolving symlinks or doing any stat calls.
    ///
    /// If the path starts with a relative component, (e.g., `../file.txt`) that component is preserved as is.
    fn normalise(&self) -> PathBuf;
}

impl<T: AsRef<Path>> PathExt for T {
    #[cfg(target_os = "windows")]
    fn to_verbatim(&self) -> PathBuf {
        verbatim::to_verbatim(self)
    }

    fn is_empty_path(&self) -> bool {
        self.as_ref().as_os_str().is_empty()
    }

    fn prefixes(&self) -> Prefixes<'_> {
        prefixes(self.as_ref())
    }

    fn normalise(&self) -> PathBuf {
        let mut normalised = Vec::new();
        let mut skip_counter = 0;
        for component in self.as_ref().components().rev() {
            match component {
                Component::CurDir => (),
                Component::ParentDir => skip_counter += 1,
                _ => {
                    if skip_counter > 0 {
                        skip_counter -= 1;
                    } else {
                        normalised.push(component);
                    }
                }
            }
        }
        while skip_counter > 0 {
            normalised.push(Component::ParentDir);
            skip_counter -= 1;
        }
        PathBuf::from_iter(normalised.into_iter().rev())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalise() {
        let p = Path::new("/home/user1/../user2/dev/src/../../../user3/././file.txt");
        let expected = Path::new("/home/user3/file.txt");

        assert_eq!(expected, p.normalise());

        let p = Path::new("../user2/dev/../images");
        let expected = Path::new("../user2/images");

        assert_eq!(expected, p.normalise());

        let p = Path::new("../../../user2/./dev/../images/../././");
        let expected = Path::new("../../../user2");

        assert_eq!(expected, p.normalise());

        let p = PathBuf::new();

        assert!(p.normalise().is_empty_path());

        let p = Path::new("./");

        assert!(p.normalise().is_empty_path());

        let p = Path::new("/");

        assert_eq!(p, p.normalise());
    }
}
