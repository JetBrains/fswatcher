use std::{
    panic::{self, catch_unwind, UnwindSafe},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{future::BoxFuture, stream::StreamExt, FutureExt};
use tracing::info;

use crate::test_helpers::test_with_custom_timeout;
use watch::*;

#[path = "../src/test_helpers/mod.rs"]
mod test_helpers;

/// These tests are not wrong and might still have scenarios that are not covered otherwise,
/// but testing file system events end-to-end is inherently fragile.
///
/// On the way to the client, a file system event passes through several buffers, each with its own conflation strategy.
/// As a result, writing expectations for more elaborate scenarios turns into a nightmare.
/// This is especially true considering the differences in the behaviour of operating systems.
///
/// The way forward is to keep a small number of end-to-end tests and cover the rest in unit tests and/or platform-specific backend tests.

/// Wall-clock budget for a single end-to-end test, enforced from outside the runtime
/// under test. It is a guard against a hang, not an assertion: the real deadlines are
/// the per-event timeouts inside the test bodies.
///
/// Windows gets a much larger budget. Its immediate watcher polls file metadata every
/// `POLL_INTERVAL` (see `src/backend/windows/immediate.rs`) instead of being woken by
/// the OS, and the whole suite runs about four times slower there than on Linux or
/// macOS, so the tests that wait for several events in a row need the headroom.
const WATCH_TEST_TIMEOUT: Duration = if cfg!(target_os = "windows") {
    Duration::from_secs(20)
} else {
    Duration::from_secs(5)
};

fn test_watcher() -> Watcher {
    let watch_options = watch::options::Builder::new().macos_latency(Duration::from_millis(0));
    watch::Watcher::create(watch_options).expect("failed to create watcher")
}

// TODO this is extremely fragile, perhaps the approach with a latch file will work better
const SETTLE_DOWN_DELAY: Duration = if cfg!(target_os = "windows") {
    Duration::from_millis(1000)
} else {
    Duration::from_millis(250)
};

static GENERAL_TEST_MU: Mutex<()> = Mutex::new(());

fn single_thread(body: impl FnOnce() + Send + UnwindSafe + 'static) {
    let guard = GENERAL_TEST_MU.lock().unwrap();
    let test_result = catch_unwind(body);
    drop(guard);
    if let Err(panic) = test_result {
        panic::resume_unwind(panic);
    }
}

fn with_relative_paths(base: impl AsRef<Path>, events: Vec<Event>) -> Vec<Event> {
    let base = base.as_ref();
    events
        .into_iter()
        .map(|it| match it {
            Event::Removed { path } => Event::Removed {
                path: path.strip_prefix(base).unwrap().to_path_buf(),
            },
            Event::Dirty { path, file_type } => Event::Dirty {
                path: path.strip_prefix(base).unwrap().to_path_buf(),
                file_type,
            },
            // Event2::Rename { old_name, new_name } => {
            //     let old_name = old_name.strip_prefix(base).unwrap().to_path_buf();
            //     let new_name = new_name.strip_prefix(base).unwrap().to_path_buf();
            //     Event2::Rename { old_name, new_name }
            // }
            Event::Rescan { path } => {
                let path = path.strip_prefix(base).unwrap().to_path_buf();
                Event::Rescan { path }
            }
        })
        .collect()
}

fn with_immediate_watch_subscription<F>(path: impl Into<PathBuf>, body: F)
where
    F: 'static + Send + FnOnce(watch::EventStream<'_>) -> BoxFuture<'_, ()>,
{
    use tokio::time;

    let path = path.into();
    test_with_custom_timeout(WATCH_TEST_TIMEOUT, move |_rt| async move {
        let watcher = test_watcher();

        info!("created a watcher");
        time::sleep(SETTLE_DOWN_DELAY).await;
        info!(
            "{:?} passed, let's hope we will not receive events from initial setup",
            SETTLE_DOWN_DELAY,
        );
        let subscription = watcher.watch_immediate(&path);

        info!(path = %path.display(), "subscribed");
        body(subscription).await;
        info!("executed the body");
        watcher.shutdown_and_join().unwrap();
    });
}

#[derive(Debug)]
enum WatchMode {
    Recursive,
    NonRecursive,
}

// [duration] = how long to wait for all events to arrive after [body] is executed
fn collect_events(path: impl Into<PathBuf>, watch_mode: WatchMode, duration: Duration, body: impl FnOnce() + Send + 'static) -> Vec<Event> {
    let path = path.into();
    test_with_custom_timeout(
        Duration::max(WATCH_TEST_TIMEOUT, SETTLE_DOWN_DELAY + duration * 2),
        move |_rt| async move {
            let watcher = Arc::new(test_watcher());
            info!("Created a watcher");
            tokio::time::sleep(SETTLE_DOWN_DELAY).await;
            info!(
                "{:?} passed, let's hope we will not receive events from initial setup",
                SETTLE_DOWN_DELAY
            );
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let subscription = match watch_mode {
                WatchMode::Recursive => watcher.watch_recursively(&path),
                WatchMode::NonRecursive => watcher.watch_one(&path),
            };
            // The stream is cold, but some tests rely on the events being buffered, so it must be actively collected.
            let abort_handle = tokio::task::spawn(async move {
                subscription
                    .for_each(move |evt| {
                        tx.send(evt).unwrap();
                        futures::future::ready(())
                    })
                    .await;
            })
            .abort_handle();
            tokio::task::yield_now().await;

            info!("Subscribed to {}", path.display());
            body();
            info!("Executed the body, will wait {:?} to collect events", duration);
            tokio::time::sleep(duration).await;
            abort_handle.abort();
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        },
    )
}

fn eq_ignoring_created(one: &Event, another: &Event) -> bool {
    match (one, another) {
        (Event::Rescan { path: path1 }, Event::Rescan { path: path2 }) => path1.eq(path2),
        (
            Event::Dirty {
                path: path1,
                file_type: file_type1,
            },
            Event::Dirty {
                path: path2,
                file_type: file_type2,
            },
        ) => path1.eq(path2) && file_type1 == file_type2,
        (Event::Removed { path: path1 }, Event::Removed { path: path2 }) => path1.eq(path2),
        _ => false,
    }
}

fn vec_eq_ignoring_created(one: impl IntoIterator<Item = impl AsRef<Event>>, another: impl IntoIterator<Item = impl AsRef<Event>>) -> bool {
    let mut one_iter = one.into_iter();
    let mut another_iter = another.into_iter();
    loop {
        match (one_iter.next(), another_iter.next()) {
            (Some(one), Some(another)) => {
                if !eq_ignoring_created(one.as_ref(), another.as_ref()) {
                    break false;
                }
            }
            (None, None) => break true,
            _ => break false,
        }
    }
}

fn assert_eq_events(expected: impl IntoIterator<Item = Event>, actual: impl IntoIterator<Item = Event>) {
    let expected: Vec<Event> = expected.into_iter().collect();
    let actual: Vec<Event> = actual.into_iter().collect();
    /*
      The `created` flag is a direct translation of OS-specific flags (kFSEventStreamEventFlagItemCreated for fsevents, CREATE for inotify, FILE_ACTION_ADDED for Windows).
      Given the differences between these APIs, non-trivial cases should be treated as undefined behaviour.

      Also, macOS sets `kFSEventStreamEventFlagItemCreated` for each event in the first several seconds after the file is created.
    */

    let eq = vec_eq_ignoring_created(&expected, &actual);
    assert!(eq, "expected: {:#?}, actual: {:#?}", expected, actual);
}

mod watch_single_file {
    use super::*;
    use std::fs::rename;
    use crate::test_helpers::*;
    use watch::FileType;

    #[test]
    fn create_file() {
        single_thread(|| {
            let dir = files!({});
            let file_path = dir.path().join("target");
            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    create(file_path);
                }
            });
            let expected = vec![Event::Rescan { path: file_path }];

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn change_file() {
        single_thread(|| {
            let dir = files!({"target" => "old content"});
            let file_path = dir.path().join("target");

            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    write_all(file_path, "new content");
                }
            });
            let expected = Event::Dirty {
                path: file_path,
                file_type: FileType::Regular,
            };
            let eq = actual.iter().all(|evt| eq_ignoring_created(evt, &expected));
            assert!(eq, "expected any number of {:#?}, got {:#?}", expected, actual);
        });
    }

    #[test]
    fn change_file_and_immediately_delete_it() {
        single_thread(|| {
            let dir = files!({"target" => "old content"});
            let file_path = dir.path().join("target");

            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    write_all(&file_path, "new content");
                    delete(&file_path);
                }
            });
            if cfg!(target_os = "macos") {
                let expected1 = vec![
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                    Event::Removed { path: file_path.clone() },
                ];
                let expected2 = vec![Event::Removed { path: file_path.clone() }];
                // I am not crazy. This test failed with:
                // [2022-05-23T16:16:44.708034000Z TRACE watch::platform::fsevents] Raw event 183416084 /private/var/folders/ws/f6mpq4pj06zb0tb1p44szbbw0000kt/T/testing-dir.17fCr7SyGoh5/files/target kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemRemoved | kFSEventStreamEventFlagItemInodeMetaMod | kFSEventStreamEventFlagItemModified | kFSEventStreamEventFlagItemIsFile
                // [2022-05-23T16:16:44.908063000Z TRACE watch::platform::fsevents] Raw event 183416090 /private/var/folders/ws/f6mpq4pj06zb0tb1p44szbbw0000kt/T/testing-dir.17fCr7SyGoh5/files/target kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemRemoved | kFSEventStreamEventFlagItemInodeMetaMod | kFSEventStreamEventFlagItemModified | kFSEventStreamEventFlagItemIsFile
                let expected3 = vec![Event::Removed { path: file_path.clone() }, Event::Removed { path: file_path }];
                let eq = vec_eq_ignoring_created(&actual, &expected1)
                    || vec_eq_ignoring_created(&actual, &expected2)
                    || vec_eq_ignoring_created(&actual, &expected3);
                assert!(
                    eq,
                    "expected: {:#?} or {:#?} or {:#?}, actual: {:#?}",
                    expected1, expected2, expected3, actual
                );
            } else {
                let expected1 = vec![
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                    Event::Removed { path: file_path.clone() },
                ];
                let expected2 = vec![
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                    Event::Removed { path: file_path.clone() },
                ];
                let expected3 = vec![Event::Removed { path: file_path }];
                let eq = vec_eq_ignoring_created(&actual, &expected1)
                    || vec_eq_ignoring_created(&actual, &expected2)
                    || vec_eq_ignoring_created(&actual, &expected3);
                assert!(
                    eq,
                    "expected: {:#?} or {:#?} or {:#?}, actual: {:#?}",
                    expected1, expected2, expected3, actual
                );
            }
        });
    }

    #[test]
    fn touch_file() {
        single_thread(|| {
            let dir = files!({"target" => "content"});
            let file_path = dir.path().join("target");

            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    touch(file_path);
                }
            });
            let expected = vec![Event::Dirty {
                path: file_path,
                file_type: FileType::Regular,
            }];
            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn delete_file() {
        single_thread(|| {
            let dir = files!({"target" => "content"});
            let file_path = dir.path().join("target");

            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    delete(file_path);
                }
            });
            let expected = vec![Event::Removed { path: file_path }];
            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn delete_file_and_immediately_create_another() {
        single_thread(|| {
            enable_logging();

            let dir = files!({"target" => "content"});
            let file_path = dir.path().join("target");

            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    delete(&file_path);
                    create(&file_path);
                }
            });
            if cfg!(target_os = "macos") {
                let expected1 = vec![
                    Event::Removed { path: file_path.clone() },
                    Event::Rescan { path: file_path.clone() },
                ];

                let expected2 = vec![
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                ];
                let eq = vec_eq_ignoring_created(&actual, &expected1) || vec_eq_ignoring_created(&actual, &expected2);
                assert!(eq, "expected: {:#?} or {:#?}, actual: {:#?}", expected1, expected2, actual);
            } else if cfg!(target_os = "linux") || cfg!(target_os = "windows") {
                let expected = vec![Event::Removed { path: file_path.clone() }, Event::Rescan { path: file_path }];
                assert_eq_events(expected, actual);
            } else {
                unreachable!()
            };
        });
    }

    #[test]
    fn overwrite_with_another_file() {
        single_thread(|| {
            let dir = files!({"target" => "old content", "another" => "new content"});
            let file_path = dir.path().join("target");
            let another_file_path = dir.path().join("another");

            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    rename(&another_file_path, &file_path).unwrap();
                }
            });
            if cfg!(target_os = "windows") {
                let expected = vec![Event::Removed { path: file_path.clone() }, Event::Rescan { path: file_path }];
                assert_eq_events(expected, actual);
            } else {
                let expected = Event::Dirty {
                    path: file_path,
                    file_type: FileType::Regular,
                };
                let eq = actual.iter().all(|evt| eq_ignoring_created(evt, &expected));
                assert!(eq, "expected any number of {:#?}, got {:#?}", expected, actual);
            };
        });
    }

    #[test]
    fn rename_file() {
        single_thread(|| {
            let dir = files!({"dir" => {"file" => ""}, "another_dir" => {}});
            let file_path = dir.path().join("dir/file");
            let new_file_path = dir.path().join("another_dir/file");
            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    rename(&file_path, &new_file_path).unwrap();
                    touch(&new_file_path);
                    rename(&new_file_path, &file_path).unwrap();
                }
            });
            let expected1 = vec![
                Event::Removed { path: file_path.clone() },
                Event::Rescan { path: file_path.clone() },
            ];

            #[cfg(not(target_os = "macos"))]
            assert_eq_events(expected1, actual);

            // FSEvents coalesces both renames of the watched path into one event. The backend stats the path,
            // finds a regular file, and reports it as changed. The client re-reads the file either way.
            // [
            //     watch::backend::macos::fs_event_stream::FSEventStreamEvent {
            //         id: 1340049679,
            //         path: "/private/var/folders/9d/dtyhgw_502n72wxldn41g6zm0000gn/T/testing-dirhg712I/files/another_dir/file",
            //         flags: "kFSEventStreamEventFlagItemInodeMetaMod | kFSEventStreamEventFlagItemRenamed | kFSEventStreamEventFlagItemIsFile",
            //     },
            //     watch::backend::macos::fs_event_stream::FSEventStreamEvent {
            //         id: 1340049680,
            //         path: "/private/var/folders/9d/dtyhgw_502n72wxldn41g6zm0000gn/T/testing-dirhg712I/files/dir/file",
            //         flags: "kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemRenamed | kFSEventStreamEventFlagItemIsFile",
            //     },
            // ]
            #[cfg(target_os = "macos")]
            {
                let expected2 = vec![Event::Dirty {
                    path: file_path,
                    file_type: FileType::Regular,
                }];
                let is_expected = vec_eq_ignoring_created(&expected1, &actual) || vec_eq_ignoring_created(&expected2, &actual);
                assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, actual);
            }
        });
    }

    #[test]
    #[cfg(unix)]
    fn change_file_permissions() {
        let _mu = GENERAL_TEST_MU.lock().unwrap();

        let dir = files!({"target" => ""});
        let file_path = dir.path().join("target");

        let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
            let file_path = file_path.clone();
            // mark executable
            move || {
                set_permissions(file_path, 777);
            }
        });
        let expected = vec![Event::Dirty {
            path: file_path,
            file_type: FileType::Regular,
        }];
        assert_eq_events(expected, actual);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn change_uf_hidden_flag() {
        let _mu = GENERAL_TEST_MU.lock().unwrap();

        use std::ffi::OsStr;

        let dir = files!({"target" => ""});
        let file_path = dir.path().join("target");
        let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
            let file_path = file_path.clone();
            move || {
                run("chflags", vec![OsStr::new("hidden"), file_path.as_os_str()]);
            }
        });
        let expected = vec![Event::Dirty {
            path: file_path,
            file_type: FileType::Regular,
        }];
        assert_eq_events(expected, actual);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn change_file_attribute_hidden() {
        single_thread(|| {
            let dir = files!({"target" => ""});
            let file_path = dir.path().join("target");
            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    use std::iter::once;
                    use std::os::windows::ffi::OsStrExt;
                    use windows::core::PCWSTR;
                    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};

                    let lpfilename = file_path.as_path().as_os_str().encode_wide().chain(once(0)).collect::<Vec<u16>>();
                    unsafe {
                        let success = SetFileAttributesW(PCWSTR(lpfilename.as_ptr()), FILE_ATTRIBUTE_HIDDEN);
                        success.ok().unwrap();
                    }
                }
            });
            let expected = vec![Event::Dirty {
                path: file_path,
                file_type: FileType::Regular,
            }];
            assert_eq_events(expected, actual);
        });
    }
}

mod watch_directory {
    use super::*;
    use std::fs::rename;
    use crate::test_helpers::*;
    use watch::FileType;

    // -- change directory permissions
    // -- change child permissions
    // -- create symlink
    // -- change file in symlinked directory

    #[test]
    fn create_file() {
        single_thread(|| {
            let dir = files!({});
            let file_path = dir.path().join("file");

            let actual = collect_events(dir.path(), WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                create(file_path);
            });
            let expected = vec![Event::Dirty {
                path: PathBuf::from("file"),
                file_type: FileType::Regular,
            }];

            assert_eq_events(expected, with_relative_paths(dir.path(), actual));
        });
    }

    #[test]
    fn change_file() {
        single_thread(|| {
            let dir = files!({"file" => "old content"});
            let file_path = dir.path().join("file");

            let actual = collect_events(dir.path(), WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                write_all(file_path, "new content");
            });
            let actual = with_relative_paths(dir.path(), actual);
            let expected = Event::Dirty {
                path: PathBuf::from("file"),
                file_type: FileType::Regular,
            };
            let eq = actual.iter().all(|evt| eq_ignoring_created(evt, &expected));
            assert!(eq, "expected any number of {:#?}, got {:#?}", expected, actual);
        });
    }

    #[test]
    fn touch_file() {
        single_thread(|| {
            let dir = files!({"file" => "old content"});
            let file_path = dir.path().join("file");

            let actual = collect_events(dir.path(), WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                touch(file_path);
            });
            let actual = with_relative_paths(dir.path(), actual);
            let expected = Event::Dirty {
                path: PathBuf::from("file"),
                file_type: FileType::Regular,
            };
            let eq = actual.iter().all(|evt| eq_ignoring_created(evt, &expected));
            assert!(eq, "expected any number of {:#?}, got {:#?}", expected, actual);
        });
    }

    #[test]
    fn delete_file() {
        single_thread(|| {
            let dir = files!({"file" => "content"});
            let file_path = dir.path().join("file");

            let actual = collect_events(dir.path(), WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                delete(file_path);
            });
            let expected = vec![Event::Removed {
                path: PathBuf::from("file"),
            }];

            assert_eq_events(expected, with_relative_paths(dir.path(), actual));
        });
    }

    #[test]
    fn rename_child() {
        single_thread(|| {
            let dir = files!({"file" => "content"});
            let file_path = dir.path().join("file");
            let new_file_path = dir.path().join("new_file");
            let actual = collect_events(dir.path(), WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                rename(&file_path, &new_file_path).unwrap();
            });
            // TODO expect Event::Rename instead?
            let expected = vec![
                Event::Removed {
                    path: PathBuf::from("file"),
                },
                Event::Dirty {
                    path: PathBuf::from("new_file"),
                    file_type: FileType::Regular,
                },
            ];
            assert_eq_events(expected, with_relative_paths(dir.path(), actual));
        });
    }

    #[test]
    fn rename_child_directory() {
        single_thread(|| {
            let dir = files!({"child_dir" => {}});
            let child_dir_path = dir.path().join("child_dir");
            let new_child_dir_path = dir.path().join("new_child_dir");
            let actual = collect_events(dir.path(), WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                rename(&child_dir_path, &new_child_dir_path).unwrap();
            });

            let expected = if cfg!(target_os = "macos") {
                vec![
                    Event::Removed {
                        path: PathBuf::from("child_dir"),
                    },
                    Event::Rescan {
                        path: PathBuf::from("new_child_dir"),
                    },
                ]
            } else {
                vec![
                    Event::Removed {
                        path: PathBuf::from("child_dir"),
                    },
                    Event::Dirty {
                        path: PathBuf::from("new_child_dir"),
                        file_type: FileType::Directory,
                    },
                ]
            };
            assert_eq_events(expected, with_relative_paths(dir.path(), actual));
        });
    }

    #[test]
    fn move_to_another_directory_move_from_another_directory() {
        single_thread(|| {
            let dir = files!({"dir" => {"file" => "content"}, "another_dir" => {}});
            let dir_path = dir.path().join("dir");
            let file_path = dir.path().join("dir/file");
            let new_file_path = dir.path().join("another_dir/file");
            let actual = collect_events(&dir_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                rename(&file_path, &new_file_path).unwrap();
                rename(&new_file_path, &file_path).unwrap();
            });
            let actual = with_relative_paths(&dir_path, actual);
            if cfg!(target_os = "macos") {
                let expected1 = vec![Event::Dirty {
                    path: PathBuf::from("file"),
                    file_type: FileType::Regular,
                }];
                let expected2 = vec![
                    Event::Dirty {
                        path: PathBuf::from("file"),
                        file_type: FileType::Regular,
                    },
                    Event::Dirty {
                        path: PathBuf::from("file"),
                        file_type: FileType::Regular,
                    },
                ];
                let eq = vec_eq_ignoring_created(&actual, &expected1) || vec_eq_ignoring_created(&actual, &expected2);
                assert!(eq, "expected: {:#?} or {:#?}, actual: {:#?}", expected1, expected2, actual);
            } else {
                let expected = vec![
                    Event::Removed {
                        path: PathBuf::from("file"),
                    },
                    Event::Dirty {
                        path: PathBuf::from("file"),
                        file_type: FileType::Regular,
                    },
                ];
                assert_eq_events(expected, actual);
            }
        });
    }

    #[ignore = "Change events with is_dir=true are dropped"]
    #[test]
    #[cfg(target_os = "windows")]
    fn change_file_attribute_hidden() {
        single_thread(|| {
            let dir = files!({});
            let actual = collect_events(dir.path(), WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let dir_path = dir.path().to_path_buf();
                move || {
                    use std::iter::once;
                    use std::os::windows::ffi::OsStrExt;
                    use windows::core::PCWSTR;
                    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};

                    let lpfilename = dir_path.as_path().as_os_str().encode_wide().chain(once(0)).collect::<Vec<u16>>();
                    unsafe {
                        let success = SetFileAttributesW(PCWSTR(lpfilename.as_ptr()), FILE_ATTRIBUTE_HIDDEN);
                        success.ok().unwrap();
                    }
                }
            });
            let expected = vec![Event::Dirty {
                path: dir.path().to_path_buf(),
                file_type: FileType::Directory,
            }];
            assert_eq_events(expected, actual);
        });
    }
}

mod watch_directory_recursively {
    use super::*;
    use std::thread;
    use crate::test_helpers::*;
    use watch::FileType;

    // -- create a distant child
    // -- delete a distant child
    // -- change a distant child
    // -- move a distant child to another directory
    // -- move a distant child between nested directories

    #[test]
    fn create() {
        single_thread(|| {
            let dir = files!({"dir" => {}});
            let actual = collect_events(dir.path(), WatchMode::Recursive, SETTLE_DOWN_DELAY, {
                let dir_path = dir.path().to_path_buf();
                move || {
                    touch(dir_path.join("file"));
                    touch(dir_path.join("dir/file"));
                    create_dir(dir_path.join("new_dir"));
                    if cfg!(target_os = "linux") {
                        // It can take some time for the event loop to notice new_dir and call add_watch for it. We might miss an event if new_dir/file is created immediately.
                        // This is not a bug, but it makes the test flaky.
                        thread::sleep(SETTLE_DOWN_DELAY);
                    }
                    touch(dir_path.join("new_dir/file"));
                }
            });
            let expected = vec![
                Event::Dirty {
                    path: PathBuf::from("file"),
                    file_type: FileType::Regular,
                },
                Event::Dirty {
                    path: PathBuf::from("dir/file"),
                    file_type: FileType::Regular,
                },
                Event::Dirty {
                    path: PathBuf::from("new_dir"),
                    file_type: FileType::Directory,
                },
                Event::Dirty {
                    path: PathBuf::from("new_dir/file"),
                    file_type: FileType::Regular,
                },
            ];

            assert_eq_events(expected, with_relative_paths(dir.path(), actual));
        });
    }
}

mod changes_in_parent_directories {
    use super::*;
    use std::fs::{remove_dir_all, rename};
    use std::thread;
    use crate::test_helpers::*;
    use watch::FileType;

    #[test]
    fn watch_non_existing_path() {
        single_thread(|| {
            let dir = files!({});
            let file_path = dir.path().join("directory/another/file");

            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                move || {
                    create(file_path);
                }
            });
            let expected = vec![Event::Rescan { path: file_path }];
            assert_eq_pretty!(expected, actual);
        });
    }

    #[test]
    fn rename_parent_dir() {
        single_thread(|| {
            let dir = files!({"dir" => {"file" => ""}});
            let dir_path = dir.path().join("dir");
            let new_dir_path = dir.path().join("another_dir");
            let file_path = dir_path.join("file");
            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                move || {
                    rename(&dir_path, &new_dir_path).unwrap();
                    // this action should not emit an event
                    touch(new_dir_path.join("file"));
                    rename(&new_dir_path, &dir_path).unwrap();
                    thread::sleep(SETTLE_DOWN_DELAY);
                    touch(dir_path.join("file"));
                }
            });
            if cfg!(target_os = "macos") {
                let expected1 = vec![
                    Event::Rescan { path: file_path.clone() },
                    Event::Rescan { path: file_path.clone() },
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                ];
                let expected2 = vec![
                    Event::Rescan { path: file_path.clone() },
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                ];
                let expected3 = vec![Event::Rescan { path: file_path }];
                let eq = vec_eq_ignoring_created(&actual, &expected1)
                    || vec_eq_ignoring_created(&actual, &expected2)
                    || vec_eq_ignoring_created(&actual, &expected3);
                assert!(
                    eq,
                    "expected {:#?} or {:#?} or {:#?}, actual: {:?}",
                    expected1, expected2, expected3, actual
                );
            } else {
                let expected1 = vec![
                    Event::Removed { path: file_path.clone() },
                    Event::Rescan { path: file_path.clone() },
                    Event::Dirty {
                        path: file_path.clone(),
                        file_type: FileType::Regular,
                    },
                ];
                let expected2 = vec![
                    Event::Removed { path: file_path.clone() },
                    Event::Rescan { path: file_path.clone() },
                ];
                let eq = vec_eq_ignoring_created(&actual, &expected1) || vec_eq_ignoring_created(&actual, &expected2);
                assert!(eq, "expected {:#?} or {:#?}, actual: {:?}", expected1, expected2, actual);
            }
        });
    }

    #[test]
    fn remove_parent_recursively() {
        single_thread(|| {
            let dir = files!({ "parent" => { "file" => "" }});
            let parent_path = dir.path().join("parent");
            let file_path = parent_path.join("file");
            let actual = collect_events(&file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                move || {
                    remove_dir_all(parent_path).unwrap();
                }
            });
            let expected1 = vec![Event::Removed { path: file_path.clone() }];
            let expected2 = vec![Event::Removed { path: file_path.clone() }, Event::Rescan { path: file_path }];
            let eq = vec_eq_ignoring_created(&actual, &expected1) || vec_eq_ignoring_created(&actual, &expected2);
            assert!(eq, "expected: {:#?} or {:#?}, actual: {:#?}", expected1, expected2, actual);
        });
    }
}

mod symlinks {
    use super::*;
    use std::fs;
    use crate::test_helpers::*;
    use watch::FileType;

    #[test]
    #[cfg_attr(
        any(target_os = "windows", target_os = "linux"),
        ignore = "hard links are not supported on linux and windows"
    )]
    fn hardlink_file_events_delivered() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "a.txt" => "",
            });

            let target = dir.path().join("a.txt");
            let hardlink = dir.path().join("b.txt");

            fs::hard_link(&target, &hardlink).expect("failed to create a hardlink");
            let mut actual = collect_events(&hardlink, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                write_all(target, "Hello, world!");
            });
            let expected = vec![Event::Dirty {
                path: hardlink,
                file_type: FileType::Regular,
            }];

            // Write can produce several events (because of truncate=true?).
            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn finite_symlink_cycle_events_delivered_at_watched_path() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => {
                    /* here will be a symlink to here over parent (../src) */
                    "main.c" => "",
                },
            });

            let watched_path = dir.path().join("src/up/up/up/up/main.c");
            let symlink_path = dir.path().join("src/up");
            let target_path = dir.path().join("src/main.c");
            symlink("../src", symlink_path);

            let mut actual = collect_events(&watched_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                write_all(target_path, "Hello, world!");
            });
            let expected = vec![Event::Dirty {
                path: watched_path,
                file_type: FileType::Regular,
            }];

            // Write can produce several events (because of truncate=true?).
            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn malformed_recursive_symlinks_do_not_crash_the_event_loop() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => { },
                "real_src" => { "Main.java" => "" },
            });

            let watched_path = dir.path().join("src/dir-or-symlink/Main.java");
            let symlink_path = dir.path().join("src/dir-or-symlink");

            let mut actual = collect_events(&watched_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                // This should result in an error.
                symlink("../src/dir-or-symlink", &symlink_path);

                // Let the watcher pick up the change.
                std::thread::sleep(Duration::from_millis(200));

                std::fs::remove_file(&symlink_path).unwrap();
                create_dir(&symlink_path);
                create(symlink_path.join("Main.java"));
            });

            actual.dedup();

            let expected1 = vec![
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Dirty {
                    path: watched_path.clone(),
                    file_type: FileType::Regular,
                },
            ];
            let expected2 = vec![Event::Rescan {
                path: watched_path.clone(),
            }];
            let expected3 = vec![
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Removed {
                    path: watched_path.clone(),
                },
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Dirty {
                    path: watched_path,
                    file_type: FileType::Regular,
                },
            ];

            let is_expected = vec_eq_ignoring_created(&expected1, &actual)
                || vec_eq_ignoring_created(&expected2, &actual)
                || vec_eq_ignoring_created(&expected3, &actual);
            assert!(
                is_expected,
                "expected {:#?} or {:#?} or {:#?}, got {:#?}",
                expected1, expected2, expected3, actual
            );
        });
    }

    #[test]
    fn dangling_symlink_target_created() {
        single_thread(|| {
            enable_logging();

            let dir = files!({});

            let symlink_file = dir.path().join("b.txt");
            let target_file = dir.path().join("a.txt");
            symlink("a.txt", &symlink_file);

            let actual = collect_events(&symlink_file, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                create(target_file)
            });
            let expected = vec![Event::Rescan { path: symlink_file }];

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn symlink_target_modified() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "a.txt" => ""
            });

            let symlink_file = dir.path().join("b.txt");
            let target_file = dir.path().join("a.txt");
            symlink("a.txt", &symlink_file);

            let mut actual = collect_events(&symlink_file, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                write_all(target_file, "Hello, world!");
            });
            let expected = vec![Event::Dirty {
                path: symlink_file,
                file_type: FileType::Regular,
            }];

            // Write can produce several events (because of truncate=true?).
            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn symlink_to_dangling_symlink_target_created() {
        single_thread(|| {
            enable_logging();

            let dir = files!({});

            let symlink_to_symlink_file = dir.path().join("c.txt");
            let symlink_file = dir.path().join("b.txt");
            let target_file = dir.path().join("a.txt");

            symlink("b.txt", &symlink_to_symlink_file);
            symlink("a.txt", symlink_file);

            let actual = collect_events(&symlink_to_symlink_file, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                create(target_file);
            });
            let expected = vec![Event::Rescan {
                path: symlink_to_symlink_file,
            }];

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn symlink_to_symlink_target_modified() {
        single_thread(|| {
            enable_logging();

            let dir = files!({"a.txt" => ""});

            let symlink_to_symlink_file = dir.path().join("c.txt");
            let symlink_file = dir.path().join("b.txt");
            let target_file = dir.path().join("a.txt");

            symlink("b.txt", &symlink_to_symlink_file);
            symlink("a.txt", symlink_file);

            let mut actual = collect_events(&symlink_to_symlink_file, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                write_all(target_file, "Hello, world!");
            });
            let expected = vec![Event::Dirty {
                path: symlink_to_symlink_file,
                file_type: FileType::Regular,
            }];

            // Write can produce several events (because of truncate=true?).
            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn symlinked_directory_in_path_target_created_correctly_delivered() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => { "java" => { } }
            });

            let symlink_to_dir = dir.path().join("java_src");
            let expected_target_event_path = dir.path().join("java_src/Main.java");
            let target_file_canonical = dir.path().join("src/java").join("Main.java");

            symlink("src/java", symlink_to_dir);

            let actual = collect_events(&expected_target_event_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                create(target_file_canonical);
            });
            let expected = vec![Event::Rescan {
                path: expected_target_event_path,
            }];

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn symlinked_directory_on_the_same_level_target_modified_correctly_delivered() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => { "java" => { "Main.java" => "" } }
                /* here will be a symlink to src */
            });

            let symlink_to_dir = dir.path().join("Sources");
            let expected_target_event_path = dir.path().join("Sources/java/Main.java");
            let target_file_canonical = dir.path().join("src/java/Main.java");

            symlink("src", symlink_to_dir);

            let mut actual = collect_events(&expected_target_event_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                write_all(target_file_canonical, "class Main {}");
            });
            let expected = vec![Event::Dirty {
                path: expected_target_event_path,
                file_type: FileType::Regular,
            }];

            // Write can produce several events (because of truncate=true?).
            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn symlinked_directory_over_symlink_target_created_correctly_delivered() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => { "java" => {  } }
            });

            let symlink_to_dir = dir.path().join("java_src");
            let symlink_to_dir_over_symlink = dir.path().join("src_java");
            let expected_target_event_path = dir.path().join("src_java/Main.java");
            let target_file_canonical = dir.path().join("src/java/Main.java");

            symlink("src/java", symlink_to_dir);
            symlink("java_src", symlink_to_dir_over_symlink);

            let actual = collect_events(&expected_target_event_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                create(target_file_canonical);
            });
            let expected = vec![Event::Rescan {
                path: expected_target_event_path,
            }];

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn multiple_symlinks_in_path_target_modified_correctly_delivered() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "home" => {
                    "me" => { /* here we will have a symlink to /media/flash/android */ }
                },
                "media" => {
                    "flash" => {
                        "android" => {
                            "file.txt" => "",
                            /* here will be a symlink pointing to ../documentation */
                        },
                        "documentation" => {
                            "README.txt" => "",
                        }
                    }
                }
            });

            let watched_file = dir.path().join("home/me/repo/docs/README.txt");
            let repo_target = dir.path().join("media/flash/android");
            let repo_symlink = dir.path().join("home/me/repo");
            let docs_symlink = dir.path().join("home/me/repo/docs");

            symlink(repo_target, repo_symlink);
            symlink("../documentation", docs_symlink);

            let target = watched_file.clone();
            let mut actual = collect_events(&watched_file, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                write_all(target, "Hello, world!");
            });
            let expected = vec![Event::Dirty {
                path: watched_file,
                file_type: FileType::Regular,
            }];

            // Write can produce several events (because of truncate=true?).
            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn watching_recursively_a_directory_under_symlink_events_delivered_correctly() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "home" => {
                    "me" => { /* A symlink to media/devel */ }
                },
                "media" => {
                    "devel" => {
                        "project" => {
                            "src" => {
                                "source.c" => "",
                            },
                            "Makefile" => "",
                        }
                    }
                }
            });

            let watched_dir = dir.path().join("home/me/devel/project");
            let devel_target = dir.path().join("media/devel");
            let devel_symlink = dir.path().join("home/me/devel");
            let source_c = dir.path().join("home/me/devel/project/src/source.c");
            let makefile = dir.path().join("home/me/devel/project/Makefile");

            symlink(devel_target, devel_symlink);

            let mut actual = collect_events(&watched_dir, WatchMode::Recursive, SETTLE_DOWN_DELAY, move || {
                write_all(source_c, "Hello, world!");
                write_all(makefile, ".PHONY: all");
            });
            let expected = vec![
                Event::Dirty {
                    path: watched_dir.join("src/source.c"),
                    file_type: FileType::Regular,
                },
                Event::Dirty {
                    path: watched_dir.join("Makefile"),
                    file_type: FileType::Regular,
                },
            ];

            // Write can produce several events (because of truncate=true?).
            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn watch_symlink_to_symlink_intermediate_symlink_removed_event_delivered() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => {
                    "Main.java" => "",
                    /* here we'll have a symlink chain pointing to Main.java */
                },
            });

            let watched_path = dir.path().join("src/MainOrder3.java");
            let intermediate_link = dir.path().join("src/MainOrder2.java");

            symlink("Main.java", dir.path().join("src/MainOrder1.java"));
            symlink("MainOrder1.java", &intermediate_link);
            symlink("MainOrder2.java", &watched_path);

            let mut actual = collect_events(&watched_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                delete(intermediate_link);
            });
            let expected = vec![Event::Removed { path: watched_path }];

            // Write can produce several events (because of truncate=true?).
            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn watch_symlink_delete_target_file_removed_with_indirect() {
        single_thread(|| {
            let dir = files!({
                "src" => {
                    "Main.java" => "",
                },
                /* here will be a symlink to src/Main.java */
            });

            let watched_path = dir.path().join("MainSymlink.java");
            let target_path = dir.path().join("src/Main.java");

            symlink("src/Main.java", &watched_path);

            let actual = collect_events(&watched_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                delete(target_path);
            });

            let expected = vec![Event::Removed { path: watched_path }];

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn watch_symlink_a_link_in_chain_removed_and_created_with_different_content() {
        // One more case of a symlink creation being reported without the symlink flag:
        // [ExtendedInformation { file_name: "...\\testing-dirqm9kyC\\files\\MainSym1.java", action: FILE_ACTION_ADDED, is_dir: true, is_symlink: false }]
        // though it is later reported properly:
        // [ExtendedInformation { file_name: "...\\testing-dirqm9kyC\\files\\MainSym1.java", action: FILE_ACTION_MODIFIED, is_dir: true, is_symlink: true }
        // We emit an extra (incorrect) event.

        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => {
                    "Main.java" => "",
                },
                /* here will be a symlink chain to Main.java */
            });

            let watched_path = dir.path().join("MainSym2.java");
            let intermediate_link = dir.path().join("MainSym1.java");
            let target_path = dir.path().join("src/Main.java");

            symlink(target_path, &intermediate_link);
            symlink(&intermediate_link, &watched_path);

            let actual = collect_events(&watched_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                delete(&intermediate_link);
                symlink("src", intermediate_link);
            });

            // Depending on how the events get batched, the removal of the intermediate link may be reported as
            // `Removed` or `Dirty` for the watched path (or conflated away entirely), followed by any number of
            // `Rescan` events for the same path.
            assert!(!actual.is_empty(), "expected at least one event, got none");

            let rescans = match actual.first() {
                Some(Event::Removed { path }) | Some(Event::Dirty { path, .. }) if *path == watched_path => &actual[1..],
                _ => &actual[..],
            };
            let rest_are_rescans = rescans
                .iter()
                .all(|it| matches!(it, Event::Rescan { path } if *path == watched_path));

            assert!(
                rest_are_rescans,
                "expected an optional `Removed` or `Dirty` for {:?} followed by any number of `Rescan` events for the same path, got {:#?}",
                watched_path, actual
            );
        });
    }

    #[test]
    fn watch_path_contains_symlink_component_symlink_removed_rescan_received() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => {
                    "java" => {
                        "Main.java" => "",
                    },
                },

                /* here will be a symlink to src */
            });

            let watched_path = dir.path().join("Sources/java/Main.java");
            let symlink_path = dir.path().join("Sources");

            symlink("src", &symlink_path);

            let mut actual = collect_events(&watched_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                delete(symlink_path);
            });

            let expected = vec![Event::Removed { path: watched_path }];

            actual.dedup();

            assert_eq_events(expected, actual);
        });
    }

    #[test]
    fn watch_file_under_symlink_that_was_retargeted_events_for_old_path_do_not_get_delivered() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src1" => {
                    "java" => {
                        "Main.java" => "",
                    }
                },
                "src2" => {
                    "java" => {
                        "Main.java" => "",
                    }
                }
                /* here will be a symlink to src1 and then to src2 */
            });

            let watched_path = dir.path().join("Sources/java/Main.java");
            let src1_watched_file = dir.path().join("src1/java/Main.java");
            let src2_watched_file = dir.path().join("src2/java/Main.java");
            let symlink_path = dir.path().join("Sources");

            let src1_path = dir.path().join("src1");
            let src2_path = dir.path().join("src2");

            symlink(src1_path, &symlink_path);

            let mut actual = collect_events(&watched_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                // Retarget the symlink to a different directory with the same structure.
                delete(&symlink_path);
                symlink(src2_path, &symlink_path);

                // This event shouldn't be delivered, since it is an event from the old directory.
                write_all(src1_watched_file, "package main;");

                // Sleep on Linux because with inotify we have to establish a watch on entries individually.
                // After symlink retargeting, there is a race between setting up a watch on the new target of
                // the symlink and stuff happening to the target. Without the sleep, we may miss
                // the `Event::Removed` event. Maybe in the bright future we can switch to the fanotify API.
                // UPD: now this is true for every other platform. Rescan implies that some events are missing, so this doesn't violate the contract.
                std::thread::sleep(Duration::from_millis(200));

                // This event should be delivered, since it is an event from the new directory.
                delete(src2_watched_file);
            });

            let expected1 = vec![
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Removed {
                    path: watched_path.clone(),
                },
                // NOTE: no Event::Changed event that would happen from the write to the old file.
            ];
            let expected2 = vec![
                Event::Removed {
                    path: watched_path.clone(),
                },
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Removed {
                    path: watched_path.clone(),
                },
                // NOTE: no Event::Changed event that would happen from the write to the old file.
            ];

            // On Windows, I've observed these spurious rescan events occasionally. The weird thing is, they go
            // away if tracing is enabled. I assume this is some sort of a race that is happening somewhere
            // and events don't get properly deduplicated. It looks harmless overall, but it may not be great
            // for performance. This would need further investigation.
            // TODO what about dedup in the next line? it shouldn't be possible anymore
            let expected3 = vec![
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Removed { path: watched_path },
            ];

            // Since we are doing two operations (delete and symlink again), we may get either one or two rescan
            // events, depending on whether the two events got into a single batch and were conflated.
            actual.dedup();

            let is_expected = vec_eq_ignoring_created(&expected1, &actual)
                || vec_eq_ignoring_created(&expected2, &actual)
                || vec_eq_ignoring_created(&expected3, &actual);

            assert!(
                is_expected,
                "expected {:#?} or  {:#?} or {:#?}, got {:#?}",
                expected1, expected2, expected3, actual
            );
        });
    }

    #[test]
    fn watch_symlink_chain_intermediate_was_retargeted_events_from_new_target_delivered() {
        single_thread(|| {
            enable_logging();

            let dir = files!({
                "src" => {
                    "Main1.java" => "",
                    "Main2.java" => "",
                },
                /* here will be a symlink chain to Main1.java and later to Main2.java */
            });

            let watched_path = dir.path().join("Main.java");
            let target1_path = dir.path().join("src/Main1.java");
            let target2_path = dir.path().join("src/Main2.java");
            let intermediate_link = dir.path().join("MainInt.java");

            symlink(&target1_path, &intermediate_link);
            symlink(&intermediate_link, &watched_path);

            let actual = collect_events(&watched_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, move || {
                delete(&intermediate_link);
                symlink(&target2_path, &intermediate_link);

                std::thread::sleep(Duration::from_millis(100));
                // This event shouldn't be delivered, since it is an event from the old directory.
                write_all(target1_path, "package main;");
                // This event should be delivered, since it is an event from the new directory.
                delete(target2_path);
            });

            let expected1 = vec![
                Event::Removed {
                    path: watched_path.clone(),
                },
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Removed {
                    path: watched_path.clone(),
                },
            ];

            // If the two events (delete and create) got conflated into a single Event::Changed, we do not receive the first
            // Event::Removed.
            let expected2 = vec![
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Removed {
                    path: watched_path.clone(),
                },
            ];

            // On Windows, a symlink creation comprises two events: (1) create a regular file, (2) modify
            // this file with a symlink attribute. This generates an extra Event::Changed before we get
            // an Event::Rescan.
            #[cfg(windows)]
            let expected3 = vec![
                Event::Removed {
                    path: watched_path.clone(),
                },
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Removed { path: watched_path },
            ];

            // FsEventStream might get confused, and we have no choice but to emit Rescan:
            // [
            //     watch::new::backend::macos::fs_event_stream::FSEventStreamEvent {
            //         id: 3220716909,
            //         path: "/private/var/folders/xn/jmbk25y95jq15794293mbjcc0000gp/T/testing-dir2iAnlr/files/MainInt.java",
            //         flags: "kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemRemoved | kFSEventStreamEventFlagItemXattrMod | kFSEventStreamEventFlagItemIsSymlink",
            //     },
            //     watch::new::backend::macos::fs_event_stream::FSEventStreamEvent {
            //         id: 3220716915,
            //         path: "/private/var/folders/xn/jmbk25y95jq15794293mbjcc0000gp/T/testing-dir2iAnlr/files/MainInt.java",
            //         flags: "kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemXattrMod | kFSEventStreamEventFlagItemIsSymlink",
            //     },
            // ]
            #[cfg(target_os = "macos")]
            let expected4 = vec![
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Rescan {
                    path: watched_path.clone(),
                },
                Event::Removed {
                    path: watched_path.clone(),
                },
            ];

            let is_expected = vec_eq_ignoring_created(&expected1, &actual) || vec_eq_ignoring_created(&expected2, &actual);

            #[cfg(windows)]
            {
                let is_expected = is_expected || vec_eq_ignoring_created(&expected3, &actual);
                assert!(
                    is_expected,
                    "expected {:#?} or {:#?}, or {:#?}, got {:#?}",
                    expected1, expected2, expected3, actual
                );
            }

            #[cfg(target_os = "linux")]
            assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, actual);

            #[cfg(target_os = "macos")]
            {
                let is_expected = is_expected || vec_eq_ignoring_created(&expected4, &actual);
                assert!(is_expected, "expected {:#?} or {:#?}, got {:#?}", expected1, expected2, actual);
            }
        });
    }
}

mod immediate_change_tracking {
    use super::*;

    use std::{
        error::Error,
        fs::{self, File, OpenOptions},
        io::Write,
        time::Duration,
    };

    use anyhow::Context;
    use tokio::time;
    use tracing::{info_span, instrument, trace, warn, Instrument};

    use crate::test_helpers::*;
    use watch::FileType;

    #[instrument(skip_all, fields(expectation = ?expectation.as_ref()))]
    async fn act_expect_event_retrying<'a, T, F, E>(
        subscription: &'a mut watch::EventStream<'_>,
        mut idempotent_action: F,
        expectation: impl AsRef<Event>,
    ) -> anyhow::Result<()>
    where
        F: 'a + FnMut() -> Result<T, E>,
        E: 'static + Error + Send + Sync,
    {
        trace!("act_expect_event_retrying");

        let timeout = Duration::from_secs(1);
        let expectation = expectation.as_ref();
        let mut counter = 0;

        loop {
            idempotent_action().context("action has failed")?;
            let event = time::timeout(timeout, subscription.next()).await;
            trace!(?event, "received an event");

            match &event {
                Ok(Some(event)) if event == expectation => return Ok(()),
                Ok(Some(event @ Event::Rescan { .. })) => {
                    warn!(?event, "unexpected rescan event");
                    if counter >= 5 {
                        return Err(anyhow::anyhow!("expected {:#?} got {} rescans instead", expectation, counter));
                    } else {
                        counter += 1;
                        // Give the OS time to settle.
                        time::sleep(Duration::from_millis(50)).await;
                    }
                }
                Ok(Some(event)) => return Err(anyhow::anyhow!("expected {:#?}, received {:#?}", expectation, event)),
                Ok(None) => {
                    return Err(anyhow::anyhow!(
                        "unexpected end of event stream while waiting for {:?} event",
                        expectation
                    ));
                }
                Err(_elapsed) => return Err(anyhow::anyhow!("didn't receive any events within the {:?} timeout", timeout)),
            };
        }
    }

    #[instrument(skip_all, fields(expectation = ?expectation.as_ref()), ret(level = "trace"))]
    async fn expect_event(subscription: &mut watch::EventStream<'_>, expectation: impl AsRef<Event>) -> anyhow::Result<bool> {
        trace!("expect_event");

        let timeout = Duration::from_secs(1);
        let expectation = expectation.as_ref();
        let event = time::timeout(timeout, subscription.next()).await;

        trace!(?event, "received an event");
        match &event {
            Ok(Some(event)) if event == expectation => Ok(true),
            Ok(Some(Event::Rescan { .. })) => Ok(false),
            Ok(Some(event)) => Err(anyhow::anyhow!("expected {:#?} or rescan, received: {:#?}", expectation, event)),
            Ok(None) => Err(anyhow::anyhow!(
                "unexpected end of event stream while waiting for {:#?}",
                expectation
            )),
            Err(_elapsed) => Err(anyhow::anyhow!("didn't receive any events within the {:?} timeout", timeout)),
        }
    }

    #[test]
    fn several_writes_without_closing_are_seen_as_separate_events() {
        single_thread(|| {
            let dir = files!({
                "src" => {
                    "main.c" => "",
                }
            });

            let watched_path = dir.path().join("src/main.c");
            with_immediate_watch_subscription(watched_path.clone(), |mut subscription| {
                async move {
                    let mut file = OpenOptions::new().write(true).open(&watched_path).unwrap();
                    let expected_event = Event::Dirty {
                        path: watched_path,
                        file_type: FileType::Regular,
                    };

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"#include <stdio.h>\n"), &expected_event)
                        .await
                        .unwrap();

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"int main(void) {\n"), &expected_event)
                        .await
                        .unwrap();

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"return 0;\n"), &expected_event)
                        .await
                        .unwrap();

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"}"), &expected_event)
                        .await
                        .unwrap();
                }
                .boxed()
            });
        });
    }

    #[test]
    fn watching_through_a_symlink() {
        single_thread(|| {
            let dir = files!({
                "src" => {
                    "main.c" => "",
                }
            });

            let symlink_path = dir.path().join("mainfile.c");
            symlink("src/main.c", &symlink_path);

            let watched_path = symlink_path;
            with_immediate_watch_subscription(watched_path.clone(), |mut subscription| {
                async move {
                    let mut file = OpenOptions::new().write(true).open(&watched_path).unwrap();
                    let expected_event = Event::Dirty {
                        path: watched_path,
                        file_type: FileType::Regular,
                    };

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"#include <stdio.h>\n"), &expected_event)
                        .await
                        .unwrap();

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"int main(void) {\n"), &expected_event)
                        .await
                        .unwrap();

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"return 0;\n"), &expected_event)
                        .await
                        .unwrap();

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"}"), &expected_event)
                        .await
                        .unwrap();
                }
                .boxed()
            });
        });
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore = "see comment")]
    fn watching_through_a_symlink_chain_middle_link_removed_and_restored_watch_is_not_broken() {
        // Fails on Windows because a symlink creation is _sometimes_ reported as
        // [ExtendedInformation { file_name: "....\\testing-dirDmBgA1\\files\\mainfile.c", action: FILE_ACTION_ADDED, is_dir: false, is_symlink: false }]
        // so after we delete and create it again, we get Changed instead of Rescan.
        // We should either do additional IO or learn to emit Rescans without relying on the knowledge of the file type.

        single_thread(|| {
            let dir = files!({
                "src" => {
                    "main.c" => "",
                }
            });

            let middle_symlink_path = dir.path().join("mainfile.c");
            symlink("src/main.c", &middle_symlink_path);

            let outer_symlink_path = dir.path().join("megafile.c");
            symlink("mainfile.c", &outer_symlink_path);

            let watched_path = outer_symlink_path;
            with_immediate_watch_subscription(watched_path.clone(), |mut subscription| {
                async move {
                    let mut file = OpenOptions::new().write(true).append(true).open(&watched_path).unwrap();

                    act_expect_event_retrying(
                        &mut subscription,
                        || file.write_all(b"#include <stdio.h>"),
                        Event::Dirty {
                            path: watched_path.clone(),
                            file_type: FileType::Regular,
                        },
                    )
                    .await
                    .unwrap();

                    fs::remove_file(&middle_symlink_path).unwrap();
                    expect_event(
                        &mut subscription,
                        Event::Removed {
                            path: watched_path.clone(),
                        },
                    )
                    .await
                    .unwrap();

                    symlink("src/main.c", &middle_symlink_path);
                    expect_event(
                        &mut subscription,
                        Event::Rescan {
                            path: watched_path.clone(),
                        },
                    )
                    .await
                    .unwrap();

                    act_expect_event_retrying(
                        &mut subscription,
                        || file.write_all(b"int main(void) {\n"),
                        Event::Dirty {
                            path: watched_path.clone(),
                            file_type: FileType::Regular,
                        },
                    )
                    .await
                    .unwrap();

                    act_expect_event_retrying(
                        &mut subscription,
                        || file.write_all(b"  return 0;\n"),
                        Event::Dirty {
                            path: watched_path.clone(),
                            file_type: FileType::Regular,
                        },
                    )
                    .await
                    .unwrap();
                }
                .instrument(info_span!("test_body"))
                .boxed()
            });
        });
    }

    #[test]
    fn deleting_and_recreating_a_file_does_not_break_immediate_watch() {
        single_thread(|| {
            let dir = files!({
                "src" => {
                    "main.c" => "",
                }
            });

            let watched_path = dir.path().join("src/main.c");
            with_immediate_watch_subscription(watched_path.clone(), |mut subscription| {
                async move {
                    let mut file = OpenOptions::new().write(true).append(true).open(&watched_path).unwrap();

                    let changed_event = Event::Dirty {
                        path: watched_path.clone(),
                        file_type: FileType::Regular,
                    };
                    let created_event = Event::Dirty {
                        path: watched_path.clone(),
                        file_type: FileType::Regular,
                    };

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"#include <stdio.h>"), &changed_event)
                        .await
                        .unwrap();

                    fs::remove_file(&watched_path).unwrap();
                    expect_event(
                        &mut subscription,
                        Event::Removed {
                            path: watched_path.clone(),
                        },
                    )
                    .await
                    .unwrap();

                    let mut file = File::create(&watched_path).unwrap();
                    expect_event(&mut subscription, &created_event).await.unwrap();

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"int main(void) {"), &changed_event)
                        .await
                        .unwrap();

                    act_expect_event_retrying(&mut subscription, || file.write_all(b"int main(void) {"), &changed_event)
                        .await
                        .unwrap();
                }
                .instrument(info_span!("test_body"))
                .boxed()
            });
        });
    }
}

mod subscriptions {
    use super::*;
    use futures::StreamExt;
    use crate::test_helpers::*;
    use tokio::time::sleep;
    use watch::{Event, FileType};

    #[test]
    fn nested_recursive_subscriptions_outer_survives() {
        single_thread(|| {
            let dir = files!({ "a" => { "b" => { "c" => "" }}});
            test_with_custom_timeout(WATCH_TEST_TIMEOUT, |_rt| async move {
                let watcher = test_watcher();
                sleep(SETTLE_DOWN_DELAY).await;
                let suba = watcher.watch_recursively(dir.path().join("a"));
                let subb = watcher.watch_recursively(dir.path().join("a/b"));
                dir.touch("a/b/file1");
                sleep(SETTLE_DOWN_DELAY).await;
                drop(subb);
                sleep(SETTLE_DOWN_DELAY).await;
                dir.touch("a/b/file2");
                let events = suba.take_until(sleep(SETTLE_DOWN_DELAY)).collect::<Vec<_>>().await;
                let expected_file1 = Event::Dirty {
                    path: dir.path().join("a/b/file1"),
                    file_type: FileType::Regular,
                };
                let expected_file2 = Event::Dirty {
                    path: dir.path().join("a/b/file2"),
                    file_type: FileType::Regular,
                };

                let contains = events.iter().any(|e| eq_ignoring_created(e, &expected_file1))
                    && events.iter().any(|e| eq_ignoring_created(e, &expected_file2));
                assert!(
                    contains,
                    "expected at least one of each {:#?} and {:#?}, but got {:#?}",
                    expected_file1, expected_file2, events
                );
            });
        });
    }

    #[test]
    fn nested_recursive_subscriptions_inner_survives() {
        single_thread(|| {
            enable_logging();

            let dir = files!({ "a" => { "b" => { "c" => "" }}});
            test_with_custom_timeout(WATCH_TEST_TIMEOUT, |_rt| async move {
                let watcher = test_watcher();
                sleep(SETTLE_DOWN_DELAY).await;
                let suba = watcher.watch_recursively(dir.path().join("a"));
                let subb = watcher.watch_recursively(dir.path().join("a/b"));
                dir.touch("a/b/file1");
                sleep(SETTLE_DOWN_DELAY).await;
                drop(suba);
                sleep(SETTLE_DOWN_DELAY).await;
                dir.touch("a/b/file2");
                let events = subb.take_until(sleep(SETTLE_DOWN_DELAY)).collect::<Vec<_>>().await;
                let expected_file1 = Event::Dirty {
                    path: dir.path().join("a/b/file1"),
                    file_type: FileType::Regular,
                };
                let expected_file2 = Event::Dirty {
                    path: dir.path().join("a/b/file2"),
                    file_type: FileType::Regular,
                };

                let contains = events.iter().any(|e| eq_ignoring_created(e, &expected_file1))
                    && events.iter().any(|e| eq_ignoring_created(e, &expected_file2));
                assert!(
                    contains,
                    "expected at least one of each {:#?} and {:#?}, but got {:#?}",
                    expected_file1, expected_file2, events
                );
            });
        });
    }

    #[test]
    fn watch_directory_recursively_when_there_are_nonrecursive_watches_already() {
        single_thread(|| {
            enable_logging();

            let dir = files!({ "a" => { "b" => { "c" => "" }}});
            test_with_custom_timeout(WATCH_TEST_TIMEOUT, |_rt| async move {
                let watcher = test_watcher();
                sleep(SETTLE_DOWN_DELAY).await;
                let subabc = watcher.watch_recursively(dir.path().join("a/b/c"));
                let suba = watcher.watch_recursively(dir.path().join("a"));
                dir.touch("a/b/file");
                dir.create_dir("a/b/dir");
                if cfg!(target_os = "linux") {
                    // a race with the inotify loop
                    sleep(SETTLE_DOWN_DELAY).await;
                }
                dir.touch("a/b/dir/file");
                let aevents = suba.take_until(sleep(SETTLE_DOWN_DELAY)).collect::<Vec<_>>().await;
                let expecteda = vec![
                    Event::Dirty {
                        path: dir.path().join("a/b/file"),
                        file_type: FileType::Regular,
                    },
                    Event::Dirty {
                        path: dir.path().join("a/b/dir"),
                        file_type: FileType::Directory,
                    },
                    Event::Dirty {
                        path: dir.path().join("a/b/dir/file"),
                        file_type: FileType::Regular,
                    },
                ];
                assert_eq_events(expecteda, aevents);

                // Now that suba is dropped, let's check that subabc is still functional.
                dir.touch("a/b/c");
                let cevents = subabc.take_until(sleep(SETTLE_DOWN_DELAY)).collect::<Vec<_>>().await;
                let expectedc = vec![Event::Dirty {
                    path: dir.path().join("a/b/c"),
                    file_type: FileType::Regular,
                }];
                assert_eq_events(expectedc, cevents);
            });
        });
    }
}

#[cfg(target_os = "linux")]
mod inotify {
    use super::*;
    use std::ffi::OsStr;
    use std::fs::rename;
    use std::os::unix::fs::symlink;
    use crate::test_helpers::*;
    use tracing::info;
    use watch::{options::UlimitStrategy, FileType};

    #[test]
    fn directory_aliased_by_symlink() {
        let _mu = GENERAL_TEST_MU.lock().unwrap();

        // Adding a watch for a symlink to a file and for the file itself gives the same result if inotify follows symlinks,
        // which might lead to an error.

        // This aliasing happens only with symlinks, because we do not watch files and you cannot hardlink a directory.

        let dir = files!({ "venv" => { "lib" => {} } });

        symlink(Path::new("lib"), dir.path().join("venv/lib64")).unwrap();

        let actual = collect_events(dir.path(), WatchMode::Recursive, SETTLE_DOWN_DELAY, {
            let dir_path = dir.path().to_path_buf();
            move || {
                rename(dir_path.join("venv"), dir_path.join("venv2")).unwrap();
                rename(dir_path.join("venv2"), dir_path.join("venv")).unwrap();
            }
        });

        // We aren't interested in the events, only that there is no panic.
        assert!(!actual.is_empty());
    }

    // The test uses `mount` to reproduce the problem.
    // It should be run as root.
    #[test]
    #[ignore]
    fn cycle_on_inodes() {
        let _mu = GENERAL_TEST_MU.lock().unwrap();

        enable_logging();

        let dir = files!({ "alias" => {}, "dir" => { "deep_file" => "content" }, "file" => "content" });

        let source = dir.path();
        let target = dir.path().join("alias");
        run("mount", vec![OsStr::new("--bind"), source.as_os_str(), target.as_os_str()]);

        let actual = collect_events(dir.path(), WatchMode::Recursive, SETTLE_DOWN_DELAY, {
            let dir_path = dir.path().to_path_buf();
            move || {
                touch(dir_path.join("alias/file"));
                touch(dir_path.join("alias/dir/deep_file"));
            }
        });

        let expected: Vec<Event> = vec![
            Event::Dirty {
                path: dir.path().join("file"),
                file_type: FileType::Regular,
            },
            Event::Dirty {
                path: dir.path().join("alias/file"),
                file_type: FileType::Regular,
            },
            Event::Dirty {
                path: dir.path().join("dir/deep_file"),
                file_type: FileType::Regular,
            },
            Event::Dirty {
                path: dir.path().join("alias/dir/deep_file"),
                file_type: FileType::Regular,
            },
        ];

        let eq = vec_eq_ignoring_created(&actual, &expected);
        assert!(eq, "expected: {:#?}, actual: {:#?}", expected, actual);
    }

    #[test]
    fn watcher_remains_responsive_after_hitting_inotify_limit() {
        let _mu = GENERAL_TEST_MU.lock().unwrap();

        // On some machines, this test might be useless. E.g. my Linux installation has only 328913 directories,
        // but /proc/sys/fs/inotify/max_user_watches is 524288.
        enable_logging();

        test_with_custom_timeout(Duration::from_secs(60), move |_rt| async move {
            let watcher = Arc::new({
                let options = watch::options::Builder::new().linux_on_ulimit(UlimitStrategy::report_and_carry_on(|| {
                    info!("as expected we get an error from inotify");
                }));
                Watcher::create(options).unwrap()
            });

            // Would produce a deaf subscription because of the inotify limit.
            let _root_subscription = watcher.clone().watch_recursively(Path::new("/"));
            info!("received root_subscription");
            // Any subsequent subscriptions are deaf as well.
            let _other_subscription = watcher.clone().watch_one(Path::new("/"));
            // But watch().await should complete either way.
            info!("received other_subscription");
        })
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod case_sensitivity {
    use super::*;
    use std::fs::rename;

    use crate::test_helpers::*;
    use watch::{Event, FileType};

    use crate::{assert_eq_events, collect_events, SETTLE_DOWN_DELAY};

    #[test]
    #[ignore = "fails, a bug"]
    fn watching_a_path_with_non_canonical_capitalization() {
        single_thread(|| {
            enable_logging();

            let dir = files!({"Target" => { "File" => "" }});
            let lowercase_dir_path = dir.path().join("target");
            let lowercase_file_path = dir.path().join("target").join("file");

            let actual = collect_events(&lowercase_dir_path, WatchMode::Recursive, SETTLE_DOWN_DELAY, {
                let file_path = lowercase_file_path.clone();
                move || touch(file_path)
            });
            let expected = vec![Event::Dirty {
                path: lowercase_dir_path.join("File"),
                file_type: FileType::Regular,
            }];
            assert_eq_pretty!(expected, actual);

            let actual = collect_events(&lowercase_file_path, WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = lowercase_file_path.clone();
                move || touch(file_path)
            });
            let expected = vec![Event::Dirty {
                path: lowercase_file_path,
                file_type: FileType::Regular,
            }];

            assert_eq_pretty!(expected, actual);
        });
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "macos produces two rename events which are impossible to match currently"
    )]
    fn changing_case_of_a_child_file() {
        single_thread(|| {
            // TODO macOS produces two rename events, which are impossible to match currently,
            // so there is no way to produce anything sensible.
            // Extended data allows querying the inode of the event:
            // https://github.com/fsevents/fsevents/pull/360
            // Using this, we can think of expressing this as a Rename event.
            enable_logging();

            let dir = files!({"file" => ""});
            let file_path = dir.path().join("file");
            let different_case_path = dir.path().join("File");

            let actual = collect_events(dir.path(), WatchMode::NonRecursive, SETTLE_DOWN_DELAY, {
                let file_path = file_path.clone();
                let different_case_path = different_case_path.clone();
                move || {
                    rename(&file_path, &different_case_path).unwrap();
                }
            });
            let expected = if cfg!(target_os = "windows") {
                // For some inexplicable reason, Windows sends both FILE_ACTION_REMOVED and FILE_ACTION_RENAMED_OLD_NAME, which are converted into two Removed events.
                // Strictly speaking, it does not violate the contract, so...
                vec![
                    Event::Removed { path: file_path.clone() },
                    Event::Removed { path: file_path },
                    Event::Dirty {
                        path: different_case_path,
                        file_type: FileType::Regular,
                    },
                ]
            } else {
                vec![
                    Event::Removed { path: file_path },
                    Event::Dirty {
                        path: different_case_path,
                        file_type: FileType::Regular,
                    },
                ]
            };

            assert_eq_events(expected, actual);
        });
    }
}
