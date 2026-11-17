use std::{
    path::{Path, PathBuf},
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

fn default_backend_factory(options: BackendOptions, callback: BackendEventHandler, _vault: &Path) -> Box<dyn WatcherBackend> {
    Box::new(default_backend(options, callback).unwrap())
}

/// Runs `body` against the backend made by `create_backend` and returns the events of its subscription.
///
/// The vault, passed to both closures, is a watched directory that produces the latch events delimiting the body.
/// It must stay covered by the backend, see [`WatcherBackend::set_event_source_paths`].
fn test(
    create_backend: impl FnOnce(BackendOptions, BackendEventHandler, &Path) -> Box<dyn WatcherBackend>,
    body: impl FnOnce(&dyn WatcherBackend, SubscriptionId, &Path),
) -> Result<Vec<(PathBuf, BackendEvent)>, TestErr> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<(PathBuf, BackendEvent)>(0);
    let mut options = BackendOptions::default();
    options.macos.fs_event_stream_latency = Duration::ZERO;
    options.linux.ulimit_strategy = Debug::wrap(UlimitStrategy::panic());
    let test_subscription = subscription_id();
    let had_overflow = Arc::new(AtomicBool::new(false));
    let vault = files!({});
    let backend = create_backend(
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
                        // Fails once rx is dropped, right before the backend is shut down.
                        let _ignore = tx.send((ctx.event_path.to_path_buf(), ctx.event));
                    }
                } else {
                    trace!(event_path = ?ctx.event_path, event = ?ctx.event, "ignoring irrelevan event");
                }
            }
        }),
        vault.path(),
    );
    let before_latch_path = vault.path().join("latch_before");
    let after_latch_path = vault.path().join("latch_after");
    backend
        .add_watch(vault.path(), test_subscription, Scope::DirectChildren, ParentPolicy::Unchecked)
        .unwrap();
    let mut received = timeout_iterator(&rx, RECV_TIMEOUT);
    create(&before_latch_path);
    // Wait for the latch before running the body: a body that changes the event source (see
    // [`WatcherBackend::set_event_source_paths`]) may discard what has not been delivered yet, the latch included.
    received
        .by_ref()
        .find(|(path, evt)| path.eq(&before_latch_path) && matches!(evt, BackendEvent::RecentlyCreated { .. }));
    body(backend.as_ref(), test_subscription, vault.path());
    create(&after_latch_path);

    let events = received
        .take_while(|(path, evt)| {
            let latch_event = path.eq(&after_latch_path) && matches!(evt, BackendEvent::RecentlyCreated { .. });
            !latch_event
        })
        .collect();
    // The channel is a rendezvous: an event arriving after the latch would block the event loop in `send`, and
    // `shutdown_and_join` would never return. Disconnecting makes `send` fail instead.
    drop(rx);
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            let target = dir.path().join("target");
            backend
                .add_watch(&target, s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(&dir.path().join("root"), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(&dir.path().join("root"), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(&dir.path().join("target"), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(&dir.path().join("dir"), s, Scope::Recursive, ParentPolicy::Unchecked)
                .unwrap();
            backend
                .add_watch(
                    &dir.path().join("dir/deeply/nested/path"),
                    s,
                    Scope::Recursive,
                    ParentPolicy::Unchecked,
                )
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(&dir.path().join("dir"), s, Scope::Recursive, ParentPolicy::Unchecked)
                .unwrap();
            backend
                .add_watch(
                    &dir.path().join("dir/deeply/nested/path"),
                    s,
                    Scope::Recursive,
                    ParentPolicy::Unchecked,
                )
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
        let events = test(default_backend_factory, |backend, s, _| {
            backend
                .add_watch(&dir.path(), s, Scope::DirectChildren, ParentPolicy::Unchecked)
                .unwrap();
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
    test(default_backend_factory, |backend, s, _| {
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
        backend
            .add_watch(&rec_ancestor, s, Scope::Recursive, ParentPolicy::Unchecked)
            .unwrap();
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
    test(default_backend_factory, |backend, s, _| {
        let root = if cfg!(windows) { Path::new("C:\\") } else { Path::new("/") };
        backend.add_watch(root, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
        // It would be a bad idea to modify the root contents, and we probably have no rights anyway.
        // All we can do is check that there is no immediate error.
    })
    .unwrap();
}

mod sanity_check {
    use crate::test_helpers::files;
    use std::io::ErrorKind;

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

/// [`WatcherBackend::set_event_source_paths`] is only meaningful on macOS, where a single `FSEventStream` is the source of all events.
///
/// Every set installed by a body includes the vault, otherwise the latches of [`test`] never arrive.
/// A switch replays the events under the new set since the last delivered one, so a body never puts an "ignored"
/// change under a path that a later set adds back.
#[cfg(target_os = "macos")]
mod event_source_paths {
    use std::slice::from_ref;

    use super::*;

    #[test]
    fn narrows_the_stream() {
        enable_logging();
        retry(|| {
            // Two sibling directories, both watched: the watches stay in place, the stream decides which of them is reported.
            let dir = files!({ "a" => {}, "b" => {} });
            let (a, b) = (dir.path().join("a"), dir.path().join("b"));
            let events = test(default_backend_factory, |backend, s, vault| {
                backend.add_watch(&a, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
                backend.add_watch(&b, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();

                // The default stream covers "/"; narrow it down to `a`.
                backend.set_event_source_paths(&[vault.to_path_buf(), a.clone()]).unwrap();

                // `b` is still watched, but it is outside the stream, so its change must not be reported.
                create(b.join("ignored"));
                // `a` is inside, so its change proves that the rebuilt stream is alive.
                create(a.join("file"));
            })?;
            // FSEventStream has a race: a creation may be reported twice (see `change_file`), so any number of the same event is fine.
            let expected = (
                a.join("file"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Regular,
                },
            );
            let eq = !events.is_empty() && events.iter().all(|evt| evt.eq(&expected));
            assert!(eq, "expected one or more {:#?}, got {:#?}", expected, events);
            Ok(())
        });
    }

    #[test]
    fn initial_paths_are_honoured() {
        enable_logging();
        retry(|| {
            let dir = files!({ "a" => {}, "b" => {} });
            let (a, b) = (dir.path().join("a"), dir.path().join("b"));
            let events = test(
                // The stream starts narrow, instead of the default "/": the vault, so that the latches arrive, and `a`.
                |options, callback, vault| {
                    let paths = [vault.to_path_buf(), a.clone()];
                    Box::new(
                        macos::FSEventStreamBackend::create(&paths, macos::FsEventStreamFlags::default(), options.macos, callback).unwrap(),
                    )
                },
                |backend, s, _| {
                    backend.add_watch(&a, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();
                    backend.add_watch(&b, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();

                    // Same as in `narrows_the_stream`, but without any switch: only the initial set decides.
                    create(b.join("ignored"));
                    create(a.join("file"));
                },
            )?;
            // FSEventStream has a race: a creation may be reported twice (see `change_file`), so any number of the same event is fine.
            let expected = (
                a.join("file"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Regular,
                },
            );
            let eq = !events.is_empty() && events.iter().all(|evt| evt.eq(&expected));
            assert!(eq, "expected one or more {:#?}, got {:#?}", expected, events);
            Ok(())
        });
    }

    /// The rebuilt stream resumes from the last event id delivered by the old one, so a change under a newly
    /// added path that happened before the switch is not lost.
    #[test]
    fn events_before_the_switch_are_replayed_for_added_paths() {
        enable_logging();
        retry(|| {
            let dir = files!({ "a" => {} });
            let a = dir.path().join("a");
            let events = test(default_backend_factory, |backend, s, vault| {
                backend.add_watch(&a, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();

                // Leave `a` out of the stream, then change it: the event is recorded by fseventsd but not delivered.
                backend.set_event_source_paths(from_ref(&vault.to_path_buf())).unwrap();
                create(a.join("missed"));

                // Adding `a` back resumes from the last delivered event (the before-latch, or whatever preceded it),
                // which happened before the change, so the change is replayed.
                backend.set_event_source_paths(&[vault.to_path_buf(), a.clone()]).unwrap();
            })?;
            // FSEventStream has a race: a creation may be reported twice (see `change_file`), so any number of the same event is fine.
            let expected = (
                a.join("missed"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Regular,
                },
            );
            let eq = !events.is_empty() && events.iter().all(|evt| evt.eq(&expected));
            assert!(eq, "expected one or more {:#?}, got {:#?}", expected, events);
            Ok(())
        });
    }

    /// The contract is "always accept the full set", and the empty set is a legal one: a client that has no roots left must
    /// not be forced to keep "/" or invent a placeholder. It is an outlier for FSEvents, which refuses to create a stream
    /// without paths, so the wrapper must handle it instead of handing FSEvents an empty array.
    #[test]
    fn an_empty_set_is_accepted() {
        enable_logging();
        retry(|| {
            let dir = files!({ "a" => {} });
            let a = dir.path().join("a");
            let events = test(default_backend_factory, |backend, s, vault| {
                backend.add_watch(&a, s, Scope::DirectChildren, ParentPolicy::Unchecked).unwrap();

                // The call must succeed; there is no stream at all afterwards.
                backend.set_event_source_paths(&[]).unwrap();
                // A set of one path ignores everything else; the empty set ignores everything, this change included.
                create(a.join("ignored"));

                // The next set must work as usual, which the after-latch proves, and it starts from "now":
                // the change made during the empty period is not replayed even though `a` is in the set.
                backend.set_event_source_paths(&[vault.to_path_buf(), a.clone()]).unwrap();
            })?;
            assert_eq_pretty!(Vec::<(PathBuf, BackendEvent)>::new(), events);
            Ok(())
        });
    }
}

/// [`macos::FsEventStreamFlags::WATCH_ROOT`] reports a change along the path to a watched path as a rescan of that path,
/// delivered as [`BackendEvent::Ambiguous`] (see `fs_event_stream::event`). Its watches are torn down like for any other
/// removal, so the body sees nothing else for that path afterwards.
///
/// The stream is narrowed to the vault and the watched path instead of the default "/": "/" has no parents that could
/// change, and a stream over the paths of interest is the configuration the flag exists for.
#[cfg(target_os = "macos")]
mod watch_root {
    use super::*;

    /// a factory that builds a backend over the vault and `watched`; [`macos::FsEventStreamFlags::WATCH_ROOT`] is part of the default flags
    fn watch_root_backend_factory(watched: PathBuf) -> impl FnOnce(BackendOptions, BackendEventHandler, &Path) -> Box<dyn WatcherBackend> {
        move |options, callback, vault| {
            let paths = [vault.to_path_buf(), watched];
            let flags = macos::FsEventStreamFlags::default();
            assert!(
                flags.contains(macos::FsEventStreamFlags::WATCH_ROOT),
                "the flag under test is expected to be on by default"
            );
            Box::new(macos::FSEventStreamBackend::create(&paths, flags, options.macos, callback).unwrap())
        }
    }

    #[test]
    fn deleting_the_parent_rescans_the_watched_path() {
        enable_logging();
        retry(|| {
            // `watched` is empty on purpose: the removal of anything inside it would race with the root change.
            let dir = files!({ "parent" => { "watched" => {} } });
            let watched = dir.path().join("parent/watched");
            let events = test(watch_root_backend_factory(watched.clone()), |backend, s, _| {
                backend
                    .add_watch(&watched, s, Scope::DirectChildren, ParentPolicy::Unchecked)
                    .unwrap();
                delete(dir.path().join("parent"));
            })?;
            assert_eq_pretty!(vec![(watched.clone(), BackendEvent::Ambiguous)], events);
            Ok(())
        });
    }

    #[test]
    fn renaming_the_parent_rescans_the_watched_path() {
        enable_logging();
        retry(|| {
            let dir = files!({ "parent" => { "watched" => { "file" => "content" } } });
            let watched = dir.path().join("parent/watched");
            let events = test(watch_root_backend_factory(watched.clone()), |backend, s, _| {
                backend
                    .add_watch(&watched, s, Scope::DirectChildren, ParentPolicy::Unchecked)
                    .unwrap();
                rename(dir.path().join("parent"), dir.path().join("renamed"));
            })?;
            assert_eq_pretty!(vec![(watched.clone(), BackendEvent::Ambiguous)], events);
            Ok(())
        });
    }

    #[test]
    fn moving_the_parent_rescans_the_watched_path() {
        enable_logging();
        retry(|| {
            let dir = files!({ "parent" => { "watched" => { "file" => "content" } }, "elsewhere" => {} });
            let watched = dir.path().join("parent/watched");
            let events = test(watch_root_backend_factory(watched.clone()), |backend, s, _| {
                backend
                    .add_watch(&watched, s, Scope::DirectChildren, ParentPolicy::Unchecked)
                    .unwrap();
                rename(dir.path().join("parent"), dir.path().join("elsewhere/parent"));
            })?;
            assert_eq_pretty!(vec![(watched.clone(), BackendEvent::Ambiguous)], events);
            Ok(())
        });
    }

    /// The case the flag is for: the watched path stays the same, but what is behind it is a different directory with a
    /// different content. Nothing under the path has changed, so without the flag there is nothing to report.
    #[test]
    fn replacing_the_parent_rescans_the_watched_path() {
        enable_logging();
        retry(|| {
            let dir = files!({
                "parent" => { "watched" => { "file" => "content" } },
                "other" => { "watched" => { "another" => "content" } },
            });
            let watched = dir.path().join("parent/watched");
            let events = test(watch_root_backend_factory(watched.clone()), |backend, s, _| {
                backend
                    .add_watch(&watched, s, Scope::DirectChildren, ParentPolicy::Unchecked)
                    .unwrap();
                // `rename` refuses to replace a non-empty directory, so the swap takes two steps.
                rename(dir.path().join("parent"), dir.path().join("gone"));
                rename(dir.path().join("other"), dir.path().join("parent"));
            })?;
            // One rescan for the disappearance, and FSEvents may notice the reappearance in time to report a second one.
            let expected = (watched.clone(), BackendEvent::Ambiguous);
            let eq = !events.is_empty() && events.iter().all(|evt| evt.eq(&expected));
            assert!(eq, "expected one or more {:#?}, got {:#?}", expected, events);
            Ok(())
        });
    }
}
