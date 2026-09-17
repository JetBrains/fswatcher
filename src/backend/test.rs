use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use crate::test_helpers::*;
use tracing::{debug, trace, warn};

use crate::{
    options::UlimitStrategy,
    util::{id_source::IdSource, Debug},
};

use super::{fake::timeout_iterator::timeout_iterator, *};

static ID_SOURCE: IdSource = IdSource::new();

fn subscription_id() -> SubscriptionId {
    SubscriptionId::from_non_zero_u32(ID_SOURCE.next())
}

const RECV_TIMEOUT: Duration = Duration::from_secs(5000);
const RETRY_COUNT: usize = 5;

#[derive(Debug)]
enum TestErr {
    Overflow,
}

fn test(body: impl FnOnce(&dyn WatcherBackend, SubscriptionId)) -> Result<Vec<(PathBuf, BackendEvent)>, TestErr> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<(PathBuf, BackendEvent)>(0);
    let mut options = BackendOptions::default();
    options.macos.fs_event_stream_latency = Duration::ZERO;
    options.linux.ulimit_strategy = Debug::wrap(UlimitStrategy::panic());
    let test_subscription = subscription_id();
    let had_overflow = Arc::new(AtomicBool::new(false));
    let backend = Box::new(
        default_backend(
            options,
            Box::new({
                let had_overflow = had_overflow.clone();
                let mut tx = Some(tx);
                move |ctx| {
                    if matches!(ctx.event, BackendEvent::Overflow) {
                        had_overflow.store(true, std::sync::atomic::Ordering::Relaxed);
                        warn!(event = ?ctx.event, path = ?ctx.event_path, "Got Overflow while waiting for test events");
                        drop(tx.take())
                    }

                    if ctx.audience.contains(test_subscription) {
                        if let Some(tx) = tx.as_ref() {
                            debug!(event_path = ?ctx.event_path, event = ?ctx.event, "sending event");
                            // rx is converted into an iterator and dropped before the backend is shut down
                            let _ignore = tx.send((ctx.event_path.to_path_buf(), ctx.event));
                        }
                    } else {
                        trace!(event_path = ?ctx.event_path, event = ?ctx.event, "ignoring irrelevan event");
                    }
                }
            }),
        )
        .unwrap(),
    );
    let vault = files!({});
    let before_latch_path = vault.path().join("latch_before");
    let after_latch_path = vault.path().join("latch_after");
    backend.add_watch(vault.path(), test_subscription, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
    create(&before_latch_path);
    body(backend.as_ref(), test_subscription);
    create(&after_latch_path);

    let events = timeout_iterator(&rx, RECV_TIMEOUT)
        .skip_while(|(path, evt)| {
            let latch_event = path.eq(&before_latch_path) && matches!(evt, BackendEvent::RecentlyCreated { .. });
            !latch_event
        })
        .skip(1)
        .take_while(|(path, evt)| {
            let latch_event = path.eq(&after_latch_path) && matches!(evt, BackendEvent::RecentlyCreated { .. });
            !latch_event
        })
        .collect();
    backend.shutdown_and_join().expect("backend failed to shutdown");
    if had_overflow.load(std::sync::atomic::Ordering::Relaxed) {
        Err(TestErr::Overflow)
    } else {
        Ok(events)
    }
}

fn retry(mut body: impl FnMut() -> Result<(), TestErr>) {
    let mut n = RETRY_COUNT;
    while n > 0 {
        match body() {
            Ok(v) => return v,
            Err(err) => {
                n -= 1;
                warn!(?err, "attempt failed");
            }
        }
    }
    panic!("Failed after {} retries, see logs", RETRY_COUNT);
}

#[test]
fn create_file() {
    enable_logging();

    retry(|| {
        let dir = files!({});
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            create(dir.path().join("file"));
        })?;
        assert_eq!(
            vec![(
                dir.path().join("file"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Regular
                }
            )],
            events
        );
        Ok(())
    });
}

#[test]
fn create_directory() {
    enable_logging();
    retry(|| {
        let dir = files!({});
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            create_dir(dir.path().join("dir"));
        })?;
        assert_eq!(
            vec![(
                dir.path().join("dir"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory
                }
            )],
            events
        );
        Ok(())
    });
}

#[test]
fn create_symlink() {
    enable_logging();
    retry(|| {
        let dir = files!({ "original" => {} });
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            symlink(dir.path().join("original"), dir.path().join("symlink"));
        })?;
        let expected = if cfg!(target_os = "windows") {
            vec![
                (
                    dir.path().join("symlink"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Directory,
                    },
                ),
                (
                    dir.path().join("symlink"),
                    BackendEvent::Changed {
                        file_type: FileType::Symlink,
                    },
                ),
            ]
        } else {
            vec![(
                dir.path().join("symlink"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Symlink,
                },
            )]
        };
        assert_eq_pretty!(expected, events);
        Ok(())
    });
}

#[test]
fn remove() {
    // There is no difference between the removal of a file and a directory.
    enable_logging();
    retry(|| {
        let dir = files!({ "file" => "content" });
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            delete(dir.path().join("file"));
        })?;
        assert_eq_pretty!(vec![(dir.path().join("file"), BackendEvent::Removed)], events);
        Ok(())
    });
}

#[test]
fn remove_direct_watch() {
    enable_logging();
    retry(|| {
        let dir = files!({ "target" => {} });
        let events = test(|backend, s| {
            let target = dir.path().join("target");
            backend.add_watch(&target, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            backend.remove_watch(&target, s, Scope::DirectChildren);
            create(target.join("file"));
        })?;
        assert_eq_pretty!(Vec::<(PathBuf, BackendEvent)>::new(), events);
        Ok(())
    });
}

#[test]
fn remove_recursive_watch() {
    enable_logging();
    retry(|| {
        let dir = files!({ "target" => { "deep" => {} } });
        let events = test(|backend, s| {
            let target = dir.path().join("target");
            backend.add_watch(&target, s, Scope::Recursive, ParentPolicy::Unchecked).unwrap();
            backend.remove_watch(&target, s, Scope::Recursive);
            create(target.join("deep/file"));
        })?;
        assert_eq_pretty!(Vec::<(PathBuf, BackendEvent)>::new(), events);
        Ok(())
    });
}

#[test]
fn change_file() {
    enable_logging();
    retry(|| {
        let dir = files!({ "file" => "content" });
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            write_all(dir.path().join("file"), "new-content");
        })?;
        if cfg!(target_os = "macos") {
            // FSEventStream has a race
            let expected1 = vec![(
                dir.path().join("file"),
                BackendEvent::Changed {
                    file_type: FileType::Regular,
                },
            )];
            let expected2 = vec![(
                dir.path().join("file"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Regular,
                },
            )];
            let expected3 = vec![
                (
                    dir.path().join("file"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Regular,
                    },
                ),
                (
                    dir.path().join("file"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Regular,
                    },
                ),
            ];
            let is_expected = events.eq(&expected1) || events.eq(&expected2) || events.eq(&expected3);
            assert!(
                is_expected,
                "expected {:#?} or {:#?} or {:#?}, got {:#?}",
                expected1, expected2, expected3, events
            );
        } else if cfg!(target_os = "windows") || cfg!(target_os = "linux") {
            let expected = (
                dir.path().join("file"),
                BackendEvent::Changed {
                    file_type: FileType::Regular,
                },
            );
            let eq = events.iter().all(|evt| evt.eq(&expected));
            assert!(eq, "expected any number of {:#?}, got {:#?}", expected, events);
        } else {
            unreachable!()
        }
        Ok(())
    });
}

#[test]
fn retarget_symlink() {
    enable_logging();
    retry(|| {
        let dir = files!({ "root" => { "original" => {}, "new_target" => {} }});
        symlink(dir.path().join("root/original"), dir.path().join("root/symlink"));
        let events = test(|backend, s| {
            backend.add_watch(&dir.path().join("root"), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            // The underlying libc call to `symlink` *will not* overwrite an existing file, so retargeting is a two-step process.
            symlink(dir.path().join("root/new_target"), dir.path().join("new_symlink"));
            rename(dir.path().join("new_symlink"), dir.path().join("root/symlink"));
        })?;
        let expected = if cfg!(target_os = "macos") {
            // macos produces two events:
            // kFSEventStreamEventFlagItemRenamed | kFSEventStreamEventFlagItemIsSymlink
            // kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemRenamed | kFSEventStreamEventFlagItemXattrMod | kFSEventStreamEventFlagItemIsSymlink
            vec![
                (
                    dir.path().join("root/symlink"),
                    BackendEvent::Changed {
                        file_type: FileType::Symlink,
                    },
                ),
                (
                    dir.path().join("root/symlink"),
                    BackendEvent::Changed {
                        file_type: FileType::Symlink,
                    },
                ),
            ]
        } else if cfg!(target_os = "windows") {
            vec![
                (dir.path().join("root/symlink"), BackendEvent::Removed),
                (
                    dir.path().join("root/symlink"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Symlink,
                    },
                ),
            ]
        } else if cfg!(target_os = "linux") {
            // MOVED_TO is converted into RecentlyCreated
            vec![(
                dir.path().join("root/symlink"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Symlink,
                },
            )]
        } else {
            unreachable!()
        };
        assert_eq_pretty!(expected, events);
        Ok(())
    });
}

#[test]
fn replace_file_with_symlink() {
    enable_logging();
    retry(|| {
        let dir = files!({ "root" => { "original" => {}, "watch_target" => "it is a file" }});
        let events = test(|backend, s| {
            backend.add_watch(&dir.path().join("root"), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            // The underlying libc call to `symlink` *will not* overwrite an existing file.
            symlink(dir.path().join("root/original"), dir.path().join("new_symlink"));
            rename(dir.path().join("new_symlink"), dir.path().join("root/watch_target"));
        })?;
        let expected = if cfg!(target_os = "macos") {
            // macos produces two events:
            // kFSEventStreamEventFlagItemRenamed | kFSEventStreamEventFlagItemIsSymlink
            // kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemRenamed | kFSEventStreamEventFlagItemXattrMod | kFSEventStreamEventFlagItemIsSymlink
            vec![
                (
                    dir.path().join("root/watch_target"),
                    BackendEvent::Changed {
                        file_type: FileType::Symlink,
                    },
                ),
                (
                    dir.path().join("root/watch_target"),
                    BackendEvent::Changed {
                        file_type: FileType::Symlink,
                    },
                ),
            ]
        } else if cfg!(target_os = "windows") {
            // One would think rename should be an atomic operation.
            vec![
                (dir.path().join("root/watch_target"), BackendEvent::Removed),
                (
                    dir.path().join("root/watch_target"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Symlink,
                    },
                ),
            ]
        } else if cfg!(target_os = "linux") {
            // MOVED_TO is converted into RecentlyCreated
            vec![(
                dir.path().join("root/watch_target"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Symlink,
                },
            )]
        } else {
            unreachable!()
        };
        assert_eq_pretty!(expected, events);
        Ok(())
    });
}

/// It is impossible to replace a file with a directory in one operation (and vice versa, mostly; see [sanity_check]),
/// but events about the transformation might be incorrectly coalesced along the way.
/// We want to assert that the client is able to understand what has happened.
#[test]
fn replace_directory_with_file() {
    enable_logging();
    retry(|| {
        let dir = files!({ "target" => {} });
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            dir.delete("target");
            dir.create("target");
        })?;
        if cfg!(target_os = "macos") {
            let expected1 = vec![
                (dir.path().join("target"), BackendEvent::Removed),
                (
                    dir.path().join("target"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Regular,
                    },
                ),
            ];
            let expected2 = vec![
                (dir.path().join("target"), BackendEvent::Ambiguous),
                (
                    dir.path().join("target"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Regular,
                    },
                ),
            ];
            let is_expected = events.eq(&expected1) || events.eq(&expected2);
            assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, events);
        } else {
            let expected = vec![
                (dir.path().join("target"), BackendEvent::Removed),
                (
                    dir.path().join("target"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Regular,
                    },
                ),
            ];
            assert_eq_pretty!(expected, events);
        };
        Ok(())
    });
}

/// see [replace_directory_with_file] above
#[cfg(unix)]
#[test]
fn replace_file_with_directory() {
    enable_logging();
    retry(|| {
        let dir = files!({ "target" => "" });
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            dir.delete("target");
            dir.create_dir("target");
        })?;
        if cfg!(target_os = "macos") {
            let expected1 = vec![
                (dir.path().join("target"), BackendEvent::Removed),
                (
                    dir.path().join("target"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Directory,
                    },
                ),
            ];
            let expected2 = vec![
                (dir.path().join("target"), BackendEvent::Ambiguous),
                (
                    dir.path().join("target"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Directory,
                    },
                ),
            ];
            let is_expected = events.eq(&expected1) || events.eq(&expected2);
            assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, events);
        } else {
            let expected = vec![
                (dir.path().join("target"), BackendEvent::Removed),
                (
                    dir.path().join("target"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Directory,
                    },
                ),
            ];
            assert_eq_pretty!(expected, events);
        };
        Ok(())
    });
}

#[test]
#[cfg(windows)]
fn replace_file_with_directory() {
    enable_logging();
    retry(|| {
        let dir = files!({ "target" => { "file" => "content" }, "dir" => {} });
        let events = test(|backend, s| {
            backend.add_watch(&dir.path().join("target"), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            rename(dir.path().join("dir"), dir.path().join("target\\file"));
        })?;
        assert_eq_pretty!(
            vec![
                (dir.path().join("target\\file"), BackendEvent::Removed),
                (
                    dir.path().join("target\\file"),
                    BackendEvent::RecentlyCreated {
                        file_type: FileType::Directory
                    }
                )
            ],
            events
        );
        Ok(())
    })
}

#[test]
fn change_file_metadata() {
    enable_logging();
    retry(|| {
        let dir = files!({ "file" => "content" });
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            touch(dir.path().join("file"));
        })?;
        if cfg!(target_os = "macos") {
            let expected1 = vec![(
                dir.path().join("file"),
                BackendEvent::Changed {
                    file_type: FileType::Regular,
                },
            )];
            // FSEventStream has a race, sometimes the event will come with kFSEventStreamEventFlagItemCreated
            let expected2 = vec![(
                dir.path().join("file"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Regular,
                },
            )];
            let is_expected = events.eq(&expected1) || events.eq(&expected2);
            assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, events);
        } else {
            assert_eq_pretty!(
                vec![(
                    dir.path().join("file"),
                    BackendEvent::Changed {
                        file_type: FileType::Regular
                    }
                )],
                events
            )
        }
        Ok(())
    });
}

#[test]
#[cfg(unix)]
fn change_directory_permissions() {
    enable_logging();
    retry(|| {
        let dir = files!({ "directory" => {} });
        let dir_path = dir.path().join("directory");
        let events = test(|backend, s| {
            backend.add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            set_permissions(&dir_path, 666);
        })?;
        if cfg!(target_os = "macos") {
            let expected1 = vec![(
                dir_path.to_path_buf(),
                BackendEvent::Changed {
                    file_type: FileType::Directory,
                },
            )];
            // FSEventStream has a race, sometimes the event will come with kFSEventStreamEventFlagItemCreated
            let expected2 = vec![(
                dir_path.to_path_buf(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
            )];
            let is_expected = events.eq(&expected1) || events.eq(&expected2);
            assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, events);
        } else {
            assert_eq_pretty!(
                vec![(
                    dir_path.to_path_buf(),
                    BackendEvent::Changed {
                        file_type: FileType::Directory
                    }
                )],
                events
            )
        }
        Ok(())
    });
}

/// Enforces the same behaviour on all platforms.
///
/// The baseline is inotify, where watching a directory requires an open file descriptor,
/// which is invalidated when the directory is deleted.
#[test]
fn removing_directory_destroys_all_watches_inside() {
    enable_logging();
    retry(|| {
        let dir = files!({ "dir" => { "deeply" => { "nested" => { "path" => {} }} }});
        let events = test(|backend, s| {
            backend.add_watch(&dir.path().join("dir"), s, Scope::Recursive, ParentPolicy::Unchecked).unwrap();
            backend
                .add_watch(&dir.path().join("dir/deeply/nested/path"), s, Scope::Recursive, ParentPolicy::Unchecked)
                .unwrap();

            rename(dir.path().join("dir"), dir.path().join("irrelevant"));
            create(dir.path().join("dir/deeply/nested/path/again"));
        })?;
        let expected1 = vec![(dir.path().join("dir"), BackendEvent::Removed)];
        let expected2 = vec![((dir.path().join("dir"), BackendEvent::Ambiguous))];
        let is_expected = events.eq(&expected1) || events.eq(&expected2);
        assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, events);
        Ok(())
    });
}

#[test]
fn moving_directory_destroys_watches_inside() {
    enable_logging();

    retry(|| {
        let dir = files!({ "dir" => { "deeply" => { "nested" => { "path" => {} }} }});
        let events = test(|backend, s| {
            backend.add_watch(&dir.path().join("dir"), s, Scope::Recursive, ParentPolicy::Unchecked).unwrap();
            backend
                .add_watch(&dir.path().join("dir/deeply/nested/path"), s, Scope::Recursive, ParentPolicy::Unchecked)
                .unwrap();

            rename(dir.path().join("dir"), dir.path().join("irrelevant"));
            create(dir.path().join("dir/deeply/nested/path/again"));
        })?;
        let expected1 = vec![(dir.path().join("dir"), BackendEvent::Removed)];
        let expected2 = vec![((dir.path().join("dir"), BackendEvent::Ambiguous))];
        let is_expected = events.eq(&expected1) || events.eq(&expected2);
        assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, events);
        // TODO check that backend.registry is empty now
        Ok(())
    });
}

#[test]
#[cfg(target_os = "linux")]
fn write_to_a_file_without_closing_it() {
    enable_logging();
    retry(|| {
        // Repro for FL-10616. It doesn't produce any events on Windows and macOS.
        use std::{fs::OpenOptions, io::Write};

        let dir = files!({"target" => ""});
        let file_path = dir.path().join("target");
        let mut file_handle = OpenOptions::new().create_new(false).write(true).open(&file_path).unwrap();
        let events = test(|backend, s| {
            backend.add_watch(&dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
            file_handle.write_all("new content".as_bytes()).unwrap();
        })?;
        assert_eq_pretty!(
            vec![(
                file_path.to_path_buf(),
                BackendEvent::Changed {
                    file_type: FileType::Regular
                }
            )],
            events
        );
        let actual_content = String::from_utf8(read(&file_path)).unwrap();
        assert_eq!("new content", actual_content.as_str());
        drop(file_handle);
        Ok(())
    });
}

#[test]
fn require_watched_parent_enforces_contiguous_chain() {
    enable_logging();
    // Two independent hierarchies so the recursive case can't accidentally reuse a directory that
    // the direct case already watched.
    let dir = files!({
        "direct" => { "a" => { "b" => {} } },
        "recursive" => { "a" => { "b" => { "c" => {} } } },
    });
    test(|backend, s| {
        let a = dir.path().join("direct/a");
        let b = dir.path().join("direct/a/b");

        // The parent of `b` (namely `a`) is not watched yet, so a RequireWatchedParent watch is refused.
        assert!(
            matches!(
                backend.add_watch(&b, s, Scope::DirectChildren, ParentPolicy::RequireWatchedParent),
                Err(BackendError::DetachedParent)
            ),
            "attaching under an unwatched parent must be rejected"
        );

        // Once the parent is watched, the child is accepted (the chain is contiguous).
        backend.add_watch(&a, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
        backend
            .add_watch(&b, s, Scope::DirectChildren, ParentPolicy::RequireWatchedParent)
            .expect("a watched parent must satisfy the check");

        // In a fresh hierarchy: a recursive watch on an ancestor covers the whole subtree, so a deep
        // descendant is accepted even though its own parent (`recursive/a/b`) is never watched.
        let rec_ancestor = dir.path().join("recursive/a");
        let rec_descendant = dir.path().join("recursive/a/b/c");
        backend.add_watch(&rec_ancestor, s, Scope::Recursive, ParentPolicy::Unchecked).unwrap();
        backend
            .add_watch(&rec_descendant, s, Scope::DirectChildren, ParentPolicy::RequireWatchedParent)
            .expect("recursive ancestor coverage must satisfy the check");
    })
    .unwrap();
}

#[test]
fn watch_root() {
    enable_logging();
    // To track the canonicalization of a path, we should watch all its parents, including the root.
    test(|backend, s| {
        let root = if cfg!(windows) { Path::new("C:\\") } else { Path::new("/") };
        backend.add_watch(root, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
        // It would be a bad idea to modify the root contents, and we probably have no rights anyway.
        // All we can do is check that there is no immediate error.
    })
    .unwrap();
}

mod sanity_check {
    use std::io::ErrorKind;
    use crate::test_helpers::files;

    // macos and linux
    // file -> dir: Err(Os { code: 21, kind: IsADirectory, message: "Is a directory" })
    // dir -> file: Err(Os { code: 20, kind: NotADirectory, message: "Not a directory" })
    // windows
    // file -> dir: Err(Os { code: 5, kind: PermissionDenied, message: "Access is denied." })
    // dir -> file: Ok(())

    #[test]
    #[cfg(unix)]
    fn file_cannot_replace_directory() {
        let dir = files!({ "dir" => {}, "file" => "content" });
        let file_path = dir.path().join("file");
        let dir_path = dir.path().join("dir");

        let result = std::fs::rename(&file_path, &dir_path).map_err(|err| err.kind());
        assert_eq!(result, Err(ErrorKind::IsADirectory));
    }

    #[test]
    #[cfg(windows)]
    fn file_cannot_replace_directory() {
        let dir = files!({ "dir" => {}, "file" => "content" });
        let file_path = dir.path().join("file");
        let dir_path = dir.path().join("dir");

        let result = std::fs::rename(&file_path, &dir_path).map_err(|err| err.kind());
        assert_eq!(result, Err(ErrorKind::PermissionDenied));
    }

    #[test]
    #[cfg(unix)]
    fn directory_cannot_replace_file() {
        let dir = files!({ "dir" => {}, "file" => "content" });
        let file_path = dir.path().join("file");
        let dir_path = dir.path().join("dir");

        let result = std::fs::rename(&dir_path, &file_path).map_err(|err| err.kind());
        assert_eq!(result, Err(ErrorKind::NotADirectory));
    }

    #[test]
    #[cfg(windows)]
    fn directory_can_replace_file() {
        let dir = files!({ "dir" => {}, "file" => "content" });
        let file_path = dir.path().join("file");
        let dir_path = dir.path().join("dir");

        let result = std::fs::rename(&dir_path, &file_path).map_err(|err| err.kind());
        assert_eq!(result, Ok(()));
    }
}
