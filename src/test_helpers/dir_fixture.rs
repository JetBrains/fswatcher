use super::tree_like::TreeLike;
use anyhow::Context;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct DirFixture {
    #[allow(dead_code)]
    temp_dir: TempDir,
    canonical_path: PathBuf,
}

impl DirFixture {
    pub fn path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn create_dir(&self, path: impl AsRef<Path>) {
        super::create_dir(self.path().join(reconstruct(path)))
    }

    pub fn create(&self, path: impl AsRef<Path>) {
        super::create(self.path().join(reconstruct(path)))
    }

    pub fn touch(&self, path: impl AsRef<Path>) {
        super::touch(self.path().join(reconstruct(path)))
    }

    pub fn delete(&self, path: impl AsRef<Path>) {
        super::delete(self.path().join(reconstruct(path)))
    }

    pub fn write_all(&self, path: impl AsRef<Path>, new_content: impl Into<Vec<u8>>) {
        super::write_all(self.path().join(reconstruct(path)), new_content)
    }
}

fn reconstruct(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let mut r = PathBuf::new();
    for c in path.components() {
        r.push(c);
    }
    r
}

pub fn create_temp_dir(structure: TreeLike<String, Vec<u8>>) -> DirFixture {
    let temp_dir = TempDir::with_prefix("testing-dir").expect("failed to create temp directory");
    let canonical_path = temp_dir.path().canonicalize().unwrap();
    fn mk_recursive(path: &Path, structure: TreeLike<String, Vec<u8>>) {
        match structure {
            TreeLike::Leaf(content) => {
                let mut f = std::fs::OpenOptions::new()
                    .create_new(true)
                    .truncate(true)
                    .write(true)
                    .open(path)
                    .with_context(|| path.display().to_string())
                    .expect("failed to create file");
                f.write_all(content.as_slice())
                    .with_context(|| path.display().to_string())
                    .expect("failed to write file");
            }
            TreeLike::Node { children } => {
                std::fs::create_dir(path).unwrap();
                for (name, node) in children {
                    mk_recursive(&path.join(name), node);
                }
            }
        }
    }
    let target_path = canonical_path.join("files");
    mk_recursive(&target_path, structure);
    DirFixture {
        temp_dir,
        canonical_path: target_path,
    }
}

/**
/// files!({ <name> => <content | { child folder }>, ...})
```
    let tmp = files!({
        "src" => { "lib.rs" => "pub fn main() -> () {}" },
        "Cargo.toml" => "[package] name = \"sample\"",
    });
```
 **/
macro_rules! files {
    ( $structure:tt ) => {
        $crate::test_helpers::dir_fixture::create_temp_dir($crate::test_helpers::tree_like!($structure))
    };
}

pub(crate) use files;
