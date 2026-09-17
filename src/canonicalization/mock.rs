use std::{collections::HashMap, path::PathBuf};

use super::*;

use crate::util::path_util::PathExt;
pub use MockExpectation::{Destroy, Read};
pub use ReadResult::{Directory, Fail, File, NotFound, Symlink};

#[derive(Debug, Clone)]
pub enum ReadResult {
    Directory,
    Symlink(PathBuf),
    File,
    NotFound,
    Fail,
}

#[derive(Debug, Clone)]
pub enum MockExpectation {
    Read(PathBuf, ReadResult),
    Destroy(PathBuf),
}

pub fn expect_commands<T>(body: impl FnOnce(&mut dyn CanonicalizationIO) -> T, expectations: Vec<MockExpectation>) -> T {
    let mut method_expectations_fifo = expectations
        .into_iter()
        .flat_map(|e| match e {
            Read(expected_path, read_result) => match read_result {
                Directory => vec![
                    MethodExpectation::ReadLink(expected_path.to_path_buf(), Ok(ReadLink::NotALink)),
                    MethodExpectation::AddWatch(expected_path, Ok(())),
                ],
                Symlink(target) => {
                    let target = if target.is_absolute() {
                        target.to_path_buf()
                    } else {
                        expected_path.parent().unwrap().join(target).normalise()
                    };
                    vec![MethodExpectation::ReadLink(expected_path, Ok(ReadLink::Symlink(target)))]
                }
                NotFound => vec![MethodExpectation::ReadLink(
                    expected_path.to_path_buf(),
                    Err(io::Error::from(io::ErrorKind::NotFound)),
                )],
                Fail => vec![MethodExpectation::ReadLink(
                    expected_path.to_path_buf(),
                    Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                )],
                File => vec![
                    MethodExpectation::ReadLink(expected_path.to_path_buf(), Ok(ReadLink::NotALink)),
                    MethodExpectation::AddWatch(
                        expected_path.to_path_buf(),
                        Err(BackendError::IO(std::io::Error::from(std::io::ErrorKind::NotADirectory))),
                    ),
                ],
            },
            Destroy(path) => vec![MethodExpectation::Destroy(path)],
        })
        .rev()
        .collect::<Vec<_>>();
    let mut mock_watcher = MockIO {
        expectations: &mut method_expectations_fifo,
    };
    let t = body(&mut mock_watcher);
    let unmet_expectations_natural_order = method_expectations_fifo.into_iter().rev().collect::<Vec<_>>();
    assert!(
        unmet_expectations_natural_order.is_empty(),
        "there are expectations that weren't invoked: {unmet_expectations_natural_order:#?}"
    );
    t
}

#[derive(Debug)]
enum MethodExpectation {
    AddWatch(PathBuf, Result<(), BackendError>),
    Destroy(PathBuf),
    ReadLink(PathBuf, Result<ReadLink, io::Error>),
}

struct MockIO<'e> {
    expectations: &'e mut Vec<MethodExpectation>,
}

impl<'e> MockIO<'e> {
    fn next_expectation(&mut self, actual: &str) -> MethodExpectation {
        match self.expectations.pop() {
            Some(e) => e,
            None => panic!("didn't expect any more invocations, but got {actual:?}"),
        }
    }
}

impl<'e> CanonicalizationIO for MockIO<'e> {
    fn read_link(&mut self, canonical_path: &Path) -> Result<ReadLink, io::Error> {
        let actual_invocation = format!("read_link({canonical_path:?})");
        match self.next_expectation(&actual_invocation) {
            MethodExpectation::ReadLink(path_buf, result) => {
                if path_buf.eq(canonical_path) {
                    result
                } else {
                    let exp = MethodExpectation::ReadLink(path_buf, result);
                    panic!("next expected invocation is {exp:?}, but got {actual_invocation}")
                }
            }
            expectation => panic!("next expected invocation is {expectation:?}, but got {actual_invocation}"),
        }
    }

    fn add_watch(&mut self, canonical_path: &Path) -> Result<WatchHandle, BackendError> {
        let actual_invocation = format!("add_watch({canonical_path:?})");
        let expectation = self.next_expectation(&actual_invocation);
        match expectation {
            MethodExpectation::AddWatch(path_buf, result) => {
                if path_buf.eq(canonical_path) {
                    result.map(|()| WatchHandle)
                } else {
                    let exp = MethodExpectation::AddWatch(path_buf, result);
                    panic!("next expected invocation is {exp:?}, but got {actual_invocation}")
                }
            }
            expectation => {
                panic!("next expected invocation is {expectation:?}, but got {actual_invocation}")
            }
        }
    }

    fn destroy_watch(&mut self, _handle: WatchHandle, canonical_path: &Path) {
        let actual_invocation = format!("destroy_watch({canonical_path:?})");
        let expectation = self.next_expectation(&actual_invocation);
        match expectation {
            MethodExpectation::Destroy(expected_path) => {
                if canonical_path != expected_path {
                    panic!("expected destroy_watch({expected_path:?}), but got {actual_invocation}")
                }
            }
            expectation => panic!("next expected invocation is {expectation:?}, but got {actual_invocation} instead"),
        }
    }
}

pub fn sorted_updates(changes: HashMap<SymbolicKey, CanonicalizationUpdate>) -> Vec<(SymbolicKey, CanonicalizationUpdate)> {
    let mut vec = changes.into_iter().collect::<Vec<_>>();
    vec.sort_by(|a, b| (a.0).cmp(&b.0));
    vec
}
