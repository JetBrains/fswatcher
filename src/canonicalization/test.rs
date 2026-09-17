#[cfg(unix)]
use std::path::Path;
use std::{collections::HashMap, path::PathBuf};

use crate::test_helpers::{enable_logging, path, root};
use tracing::debug;

use super::{mock::*, *};
use crate::{backend::BackendEvent, FileType};

#[test]
fn register_path() {
    enable_logging();

    let mut t = Tracker::new();
    expect_commands(
        |io| t.register(path!("foo"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("foo"), Directory)],
    );

    assert_eq!(path!("foo"), t.canonicalization(path!("foo").as_path()).unwrap());
    assert_eq!(vec![path!("foo").as_path()], t.aliases(path!("foo").as_path()));
}

#[test]
fn suffix_of_registered_path_should_subscribe_to_missing_segments() {
    enable_logging();

    let mut t = Tracker::new();
    expect_commands(
        |io| t.register(path!("foo"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("foo"), Directory)],
    );

    expect_commands(
        |w| t.register(path!("foo", "bar"), MissingPolicy::Track, w).unwrap().key,
        vec![Read(path!("foo", "bar"), Directory)],
    );
}

#[test]
fn stop_on_first_io_error() {
    enable_logging();

    let mut t = Tracker::new();
    expect_commands(
        |io| t.register(path!("1", "2", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "2"), Fail),
        ],
    );
}

#[test]
fn parent_moved_in_with_target_file_already_inside() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1", "2", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "2"), NotFound),
        ],
    );
    let changes = expect_commands(
        |io| {
            t.handle_event(
                path!("1", "2").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![Read(path!("1", "2"), Directory), Read(path!("1", "2", "3"), Directory)],
    );

    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(changes));
}

#[test]
fn segments_appear_one_by_one() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1", "2", "3", "4"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "2"), NotFound),
        ],
    );

    let changes = expect_commands(
        |io| {
            t.handle_event(
                path!("1", "2").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![Read(path!("1", "2"), Directory), Read(path!("1", "2", "3"), NotFound)],
    );
    assert_eq!(HashMap::<SymbolicKey, CanonicalizationUpdate>::new(), changes);

    let changes = expect_commands(
        |io| {
            t.handle_event(
                path!("1", "2", "3").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![Read(path!("1", "2", "3"), Directory), Read(path!("1", "2", "3", "4"), NotFound)],
    );
    assert_eq!(HashMap::<SymbolicKey, CanonicalizationUpdate>::new(), changes);

    let changes = expect_commands(
        |io| {
            t.handle_event(
                path!("1", "2", "3", "4").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![Read(path!("1", "2", "3", "4"), Directory)],
    );

    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(changes));
}

#[test]
fn ancestor_moved_out() {
    enable_logging();
    let mut t = Tracker::new();

    expect_commands(
        |io| t.register(path!("1"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("1"), Directory)],
    );

    let key2 = expect_commands(|w| t.register(path!("1", "2"), MissingPolicy::Track, w).unwrap().key, vec![Read(path!("1", "2"), Directory)]);

    let key4 = expect_commands(
        |io| t.register(path!("1", "2", "3", "4"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
        ],
    );

    let changes = expect_commands(|io| t.handle_event(path!("1", "2").as_path(), BackendEvent::Removed, io), vec![]);
    assert_eq!(
        vec![(key2, CanonicalizationUpdate::Broken), (key4, CanonicalizationUpdate::Broken),],
        sorted_updates(changes)
    );
}

#[test]
fn removed_directory_is_added_back() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1", "2", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
        ],
    );
    let changes = expect_commands(|io| t.handle_event(path!("1", "2").as_path(), BackendEvent::Removed, io), vec![]);
    assert_eq!(vec![(key, CanonicalizationUpdate::Broken)], sorted_updates(changes));
    let changes = expect_commands(
        |io| {
            t.handle_event(
                path!("1", "2").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![Read(path!("1", "2"), Directory), Read(path!("1", "2", "3"), Directory)],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(changes));
}

#[test]
fn handle_notifications_about_sibling_paths() {
    enable_logging();
    let mut t = Tracker::new();

    expect_commands(
        |io| t.register(path!("1"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("1"), Directory)],
    );
    // We are subscribing to a directory, so we get notifications for all its children
    let changes = expect_commands(
        |io| {
            t.handle_event(
                path!("2").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![],
    );
    assert_eq!(HashMap::<SymbolicKey, CanonicalizationUpdate>::new(), changes);
    let changes = expect_commands(|io| t.handle_event(path!("3").as_path(), BackendEvent::Removed, io), vec![]);
    assert_eq!(HashMap::<SymbolicKey, CanonicalizationUpdate>::new(), changes);
    // and about direct children
    let changes = expect_commands(|io| t.handle_event(path!("1", "4").as_path(), BackendEvent::Removed, io), vec![]);
    assert_eq!(HashMap::<SymbolicKey, CanonicalizationUpdate>::new(), changes);
}

#[test]
fn handle_overflow() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1", "2", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
        ],
    );
    debug!("emulate buffer overflow at some prefix, expect watches to be recreated");
    let changes = expect_commands(
        |io| t.handle_event(path!("1", "2").as_path(), BackendEvent::Overflow, io),
        vec![Read(path!("1", "2"), Directory), Read(path!("1", "2", "3"), Directory)],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(changes));
}

#[test]
#[cfg(unix)]
fn overflow_on_root_rescans_everything() {
    enable_logging();

    let mut t = Tracker::new();
    let expectations = vec![
        Read(root!(), Directory),
        Read(path!("1"), Directory),
        Read(path!("1", "2"), Directory),
        Read(path!("1", "2", "3"), Directory),
    ];
    let key = expect_commands(|io| t.register(path!("1", "2", "3"), MissingPolicy::Track, io).unwrap().key, expectations.clone());

    let updates = expect_commands(|io| t.handle_event(Path::new("/"), BackendEvent::Overflow, io), expectations);
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
}

#[test]
fn symlink_component_basic() {
    enable_logging();

    let mut t = Tracker::new();
    let target_path = path!("1", "symlink", "4", "5");
    let symlink_path = path!("1", "symlink");
    let key = expect_commands(
        |io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(symlink_path.clone(), Symlink(path!("1", "2", "3"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
            Read(path!("1", "2", "3", "4", "5"), Directory),
        ],
    );
    debug!("verify that the path is canonicalized and dispatched properly");
    assert_eq!(
        path!("1", "2", "3", "4", "5"),
        t.canonicalization(path!("1", "symlink", "4", "5").as_path()).unwrap()
    );
    assert_eq!(
        vec![path!("1", "symlink", "4", "5").as_path()],
        t.aliases(path!("1", "2", "3", "4", "5").as_path())
    );

    debug!("verify that descendants of the path are canonicalized as well");
    assert_eq!(
        path!("1", "2", "3", "4", "5", "6"),
        t.canonicalization(path!("1", "symlink", "4", "5", "6").as_path()).unwrap()
    );
    assert_eq!(
        vec![path!("1", "symlink", "4", "5", "6").as_path()],
        t.aliases(path!("1", "2", "3", "4", "5", "6").as_path())
    );

    debug!("unregistering the path should destroy the linked directories");
    expect_commands(
        |io| t.unregister(key, io),
        vec![
            Destroy(path!("1", "2", "3", "4", "5")),
            Destroy(path!("1", "2", "3", "4")),
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );

    assert!(t.is_empty(), "supposed to be empty: {t:#?}");
}

#[test]
fn symlink_component_removal_and_recreation() {
    enable_logging();

    let mut t = Tracker::new();
    let target_path = path!("1", "symlink", "4", "5");
    let symlink_path = path!("1", "symlink");
    let key = expect_commands(
        |io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(symlink_path.clone(), Symlink(path!("1", "2", "3"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
            Read(path!("1", "2", "3", "4", "5"), Directory),
        ],
    );

    debug!("should destroy all symlinked directories when the symlink is removed");
    let updates = expect_commands(
        |io| t.handle_event(symlink_path.as_path(), BackendEvent::Removed, io),
        vec![
            Destroy(path!("1", "2", "3", "4", "5")),
            Destroy(path!("1", "2", "3", "4")),
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Broken)], sorted_updates(updates));

    debug!("resubscribe when the symlink is created again");
    let updates = expect_commands(
        |io| {
            t.handle_event(
                symlink_path.as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![
            Read(symlink_path.clone(), Symlink(path!("1'", "2", "3"))),
            Read(path!("1'"), Directory),
            Read(path!("1'", "2"), Directory),
            Read(path!("1'", "2", "3"), Directory),
            Read(path!("1'", "2", "3", "4"), Directory),
            Read(path!("1'", "2", "3", "4", "5"), Directory),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));

    debug!("verify the path is now resolved through the new symlink target");
    assert_eq!(
        path!("1'", "2", "3", "4", "5"),
        t.canonicalization(path!("1", "symlink", "4", "5").as_path()).unwrap()
    );

    debug!("unregistering the path should destroy the linked directories");
    expect_commands(
        |io| t.unregister(key, io),
        vec![
            Destroy(path!("1'", "2", "3", "4", "5")),
            Destroy(path!("1'", "2", "3", "4")),
            Destroy(path!("1'", "2", "3")),
            Destroy(path!("1'", "2")),
            Destroy(path!("1'")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );

    assert!(t.is_empty(), "supposed to be empty: {t:#?}");
}

#[test]
fn symlink_component_target_change() {
    enable_logging();

    let mut t = Tracker::new();
    let target_path = path!("1", "symlink", "4", "5");
    let symlink_path = path!("1", "symlink");
    let key = expect_commands(
        |io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(symlink_path.clone(), Symlink(path!("1", "2", "3"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
            Read(path!("1", "2", "3", "4", "5"), Directory),
        ],
    );

    debug!("change symlink target");
    let updates = expect_commands(
        |io| {
            t.handle_event(
                symlink_path.as_path(),
                BackendEvent::Changed {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![
            Read(symlink_path.clone(), Symlink(path!("1''", "2", "3"))),
            Destroy(path!("1", "2", "3", "4", "5")),
            Destroy(path!("1", "2", "3", "4")),
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
            Read(path!("1''"), Directory),
            Read(path!("1''", "2"), Directory),
            Read(path!("1''", "2", "3"), Directory),
            Read(path!("1''", "2", "3", "4"), Directory),
            Read(path!("1''", "2", "3", "4", "5"), Directory),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));

    debug!("verify the path is now resolved through the changed symlink target");
    assert_eq!(
        path!("1''", "2", "3", "4", "5"),
        t.canonicalization(path!("1", "symlink", "4", "5").as_path()).unwrap()
    );

    debug!("unregistering the path should destroy the linked directories");
    expect_commands(
        |io| t.unregister(key, io),
        vec![
            Destroy(path!("1''", "2", "3", "4", "5")),
            Destroy(path!("1''", "2", "3", "4")),
            Destroy(path!("1''", "2", "3")),
            Destroy(path!("1''", "2")),
            Destroy(path!("1''")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );

    assert!(t.is_empty(), "supposed to be empty: {t:#?}");
}

#[test]
fn symlink_chain_basic() {
    enable_logging();

    debug!("Test basic scenario with a link that is referenced by another symlink");
    let mut t = Tracker::new();
    let target_path = path!("1", "symlink1", "4");
    // {
    //     1 => {
    //         2 => { 3 => { 4 => {} } },
    //         symlink1 => "symlink2/3"
    //     }
    //     "symlink2" => "1/2"
    // }
    let key = expect_commands(
        |io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "symlink1"), Symlink(path!("symlink2", "3"))),
            Read(path!("symlink2"), Symlink(path!("1", "2"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
        ],
    );

    debug!("verify that the path is canonicalized and dispatched properly");
    assert_eq!(
        path!("1", "2", "3", "4"),
        t.canonicalization(path!("1", "symlink1", "4").as_path()).unwrap()
    );
    assert_eq!(
        vec![path!("1", "symlink1", "4").as_path()],
        t.aliases(path!("1", "2", "3", "4").as_path())
    );

    debug!("unregistering the path should destroy the linked directories");
    expect_commands(
        |io| t.unregister(key, io),
        vec![
            Destroy(path!("1", "2", "3", "4")),
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );

    assert!(t.is_empty(), "supposed to be empty {t:#?}");
}

#[test]
fn symlink_chain_referenced_link_removal_and_recreation() {
    enable_logging();

    debug!("Test removing and recreating a referenced link in a symlink chain");
    let mut t = Tracker::new();
    let target_path = path!("1", "symlink1", "4");
    let key = expect_commands(
        |io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "symlink1"), Symlink(path!("symlink2", "3"))),
            Read(path!("symlink2"), Symlink(path!("1", "2"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
        ],
    );

    debug!("intermediate link deleted, designated directories are not needed anymore");
    let updates = expect_commands(
        |io| t.handle_event(path!("symlink2").as_path(), BackendEvent::Removed, io),
        vec![
            Destroy(path!("1", "2", "3", "4")),
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Broken)], sorted_updates(updates));

    debug!("create the intermediate link again");
    let updates = expect_commands(
        |io| {
            t.handle_event(
                path!("symlink2").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![
            Read(path!("symlink2"), Symlink(path!("1'", "2"))),
            Read(path!("1'"), Directory),
            Read(path!("1'", "2"), Directory),
            Read(path!("1'", "2", "3"), Directory),
            Read(path!("1'", "2", "3", "4"), Directory),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(
        path!("1'", "2", "3", "4"),
        t.canonicalization(path!("1", "symlink1", "4").as_path()).unwrap()
    );

    debug!("unregistering the path should destroy the linked directories");
    expect_commands(
        |io| t.unregister(key, io),
        vec![
            Destroy(path!("1'", "2", "3", "4")),
            Destroy(path!("1'", "2", "3")),
            Destroy(path!("1'", "2")),
            Destroy(path!("1'")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );

    assert!(t.is_empty(), "supposed to be empty {t:#?}");
}

#[test]
fn symlink_chain_referenced_link_target_change() {
    enable_logging();
    debug!("Test changing the target of a referenced link in a symlink chain");

    let mut t = Tracker::new();
    let target_path = path!("1", "symlink1", "4");

    let key = expect_commands(
        |io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "symlink1"), Symlink(path!("symlink2", "3"))),
            Read(path!("symlink2"), Symlink(path!("1", "2"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
        ],
    );

    debug!("change the intermediate link");
    let updates = expect_commands(
        |io| {
            t.handle_event(
                path!("symlink2").as_path(),
                BackendEvent::Changed {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![
            Read(path!("symlink2"), Symlink(path!("1''", "2"))),
            Destroy(path!("1", "2", "3", "4")),
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
            Read(path!("1''"), Directory),
            Read(path!("1''", "2"), Directory),
            Read(path!("1''", "2", "3"), Directory),
            Read(path!("1''", "2", "3", "4"), Directory),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(
        path!("1''", "2", "3", "4"),
        t.canonicalization(path!("1", "symlink1", "4").as_path()).unwrap()
    );

    debug!("unregistering the path should destroy the linked directories");
    expect_commands(
        |io| t.unregister(key, io),
        vec![
            Destroy(path!("1''", "2", "3", "4")),
            Destroy(path!("1''", "2", "3")),
            Destroy(path!("1''", "2")),
            Destroy(path!("1''")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );

    assert!(t.is_empty(), "supposed to be empty {t:#?}");
}

#[test]
fn symlink_chain_referencing_link_removal_and_recreation() {
    enable_logging();

    let mut t = Tracker::new();
    let target_path = path!("1", "symlink1", "4");
    debug!("Test removing and recreating a referencing link in a symlink chain");
    let key = expect_commands(
        |io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "symlink1"), Symlink(path!("symlink2", "3"))),
            Read(path!("symlink2"), Symlink(path!("1", "2"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
        ],
    );
    debug!("delete the intermediate link");
    let updates = expect_commands(
        |io| t.handle_event(path!("1", "symlink1").as_path(), BackendEvent::Removed, io),
        vec![
            Destroy(path!("1", "2", "3", "4")),
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Broken)], sorted_updates(updates));
    assert_eq!(None, t.canonicalization(path!("1", "symlink1", "4").as_path()).ok());

    let updates = expect_commands(
        |io| {
            t.handle_event(
                path!("1", "symlink1").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![
            Read(path!("1", "symlink1"), Symlink(path!("symlink3", "3"))),
            Read(path!("symlink3"), Symlink(path!("1'", "2"))),
            Read(path!("1'"), Directory),
            Read(path!("1'", "2"), Directory),
            Read(path!("1'", "2", "3"), Directory),
            Read(path!("1'", "2", "3", "4"), Directory),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(
        path!("1'", "2", "3", "4"),
        t.canonicalization(path!("1", "symlink1", "4").as_path()).unwrap()
    );

    debug!("unregistering the path should destroy the linked directories");
    expect_commands(
        |io| t.unregister(key, io),
        vec![
            Destroy(path!("1'", "2", "3", "4")),
            Destroy(path!("1'", "2", "3")),
            Destroy(path!("1'", "2")),
            Destroy(path!("1'")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );
    assert!(t.is_empty(), "supposed to be empty {t:#?}");
}

#[test]
fn symlink_chain_referencing_link_target_change() {
    enable_logging();

    let mut t = Tracker::new();
    let target_path = path!("1", "symlink1", "4");
    debug!("Test removing and recreating a referencing link in a symlink chain");
    let key = expect_commands(
        |io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "symlink1"), Symlink(path!("symlink2", "3"))),
            Read(path!("symlink2"), Symlink(path!("1", "2"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
            Read(path!("1", "2", "3", "4"), Directory),
        ],
    );
    debug!("change the intermediate link");
    let updates = expect_commands(
        |io| {
            t.handle_event(
                path!("1", "symlink1").as_path(),
                BackendEvent::Changed {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![
            Read(path!("1", "symlink1"), Symlink(path!("symlink3", "3"))),
            Destroy(path!("1", "2", "3", "4")),
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
            Read(path!("symlink3"), Symlink(path!("1'", "2"))),
            Read(path!("1'"), Directory),
            Read(path!("1'", "2"), Directory),
            Read(path!("1'", "2", "3"), Directory),
            Read(path!("1'", "2", "3", "4"), Directory),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(
        path!("1'", "2", "3", "4"),
        t.canonicalization(path!("1", "symlink1", "4").as_path()).unwrap()
    );

    debug!("unregistering the path should destroy the linked directories");
    expect_commands(
        |io| t.unregister(key, io),
        vec![
            Destroy(path!("1'", "2", "3", "4")),
            Destroy(path!("1'", "2", "3")),
            Destroy(path!("1'", "2")),
            Destroy(path!("1'")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );
    assert!(t.is_empty(), "supposed to be empty {t:#?}");
}

#[test]
fn same_symlink_encountered_twice() {
    enable_logging();

    debug!("register a path that loops through the same symlink several times");
    let mut t = Tracker::new();
    expect_commands(
        |io| t.register(path!("s1", "s2", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("s1"), Symlink(path!("1"))),
            Read(path!("1"), Directory),
            Read(path!("1", "s2"), Symlink(path!("s1", "2"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
        ],
    );
    assert_eq!(path!("1", "2", "3"), t.canonicalization(path!("s1", "s2", "3").as_path()).unwrap());

    expect_commands(
        |io| t.handle_event(path!("1", "s2").as_path(), BackendEvent::Removed, io),
        vec![Destroy(path!("1", "2", "3")), Destroy(path!("1", "2"))],
    );
    expect_commands(
        |io| {
            t.handle_event(
                path!("1", "s2").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![
            Read(path!("1", "s2"), Symlink(path!("s1", "2"))),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
        ],
    );
    assert_eq!(path!("1", "2", "3"), t.canonicalization(path!("s1", "s2", "3").as_path()).unwrap());
}

#[test]
fn unregistering_path_should_destroy_all_unused_watches() {
    enable_logging();

    let mut t = Tracker::new();
    let key1 = expect_commands(
        |io| t.register(path!("1"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("1"), Directory)],
    );
    let key2 = expect_commands(|i| t.register(path!("1", "2"), MissingPolicy::Track, i).unwrap().key, vec![Read(path!("1", "2"), Directory)]);
    let key3 = expect_commands(
        |io| t.register(path!("1", "2", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(path!("1", "2", "3"), Directory)],
    );
    let key4 = expect_commands(
        |io| t.register(path!("1", "2", "4"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(path!("1", "2", "4"), Directory)],
    );

    debug!("unregistering a root that is shared with others should not destroy its watches");
    expect_commands(
        |io| {
            t.unregister(key2, io);
        },
        vec![],
    );

    debug!("unregistering a path should destroy its exclusive watches");
    expect_commands(
        |io| {
            t.unregister(key4, io);
        },
        vec![Destroy(path!("1", "2", "4"))],
    );

    debug!("unregistering the last child should clear the path");
    expect_commands(
        |io| {
            t.unregister(key3, io);
        },
        vec![Destroy(path!("1", "2", "3")), Destroy(path!("1", "2"))],
    );

    debug!("removing the last key should clear the tree");
    expect_commands(
        |io| {
            t.unregister(key1, io);
        },
        vec![Destroy(path!("1")), Destroy(root!())],
    );
    assert!(t.is_empty(), "supposed to be empty {t:#?}");
}

#[test]
fn dangling_symlink() {
    enable_logging();

    let mut t = Tracker::new();
    debug!("symlink points to a path that does not exist");
    let key = expect_commands(
        |io| t.register(path!("symlink", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("symlink"), Symlink(path!("1", "2"))),
            Read(path!("1"), NotFound),
        ],
    );
    assert_eq!(None, t.canonicalization(path!("symlink", "3").as_path()).ok());

    debug!("notify the directory was created");
    let updates = expect_commands(
        |io| {
            t.handle_event(
                path!("1").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![
            Read(path!("1"), Directory),
            Read(path!("1", "2"), Directory),
            Read(path!("1", "2", "3"), Directory),
        ],
    );

    debug!("should be resolved now");
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(Ok(path!("1", "2", "3")), t.canonicalization(path!("symlink", "3").as_path()));
    assert_eq!(vec![path!("symlink", "3").as_path()], t.aliases(path!("1", "2", "3").as_path()));

    debug!("unregister");
    expect_commands(
        |i| t.unregister(key, i),
        vec![
            Destroy(path!("1", "2", "3")),
            Destroy(path!("1", "2")),
            Destroy(path!("1")),
            Destroy(root!()),
        ],
    );
    assert!(t.is_empty(), "supposed to be empty: {t:#?}");
}

#[test]
fn path_is_a_regular_file() {
    enable_logging();

    let mut t = Tracker::new();
    expect_commands(
        |io| t.register(path!("1"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("1"), File)],
    );

    debug!("the path should be canonicalized properly");
    assert_eq!(path!("1"), t.canonicalization(path!("1").as_path()).unwrap());
    assert_eq!(vec![path!("1").as_path()], t.aliases(path!("1").as_path()));
}

#[test]
fn replace_file_with_symlink() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("1"), File)],
    );
    let updates = expect_commands(
        |io| {
            t.handle_event(
                &path!("1"),
                BackendEvent::Changed {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![Read(path!("1"), Symlink(path!("2"))), Read(path!("2"), Directory)],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(path!("2"), t.canonicalization(path!("1").as_path()).unwrap());
}

#[test]
fn replace_symlink_with_file() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Symlink(path!("2"))),
            Read(path!("2"), Directory),
        ],
    );
    let updates = expect_commands(
        |io| {
            t.handle_event(
                &path!("1"),
                BackendEvent::Changed {
                    file_type: FileType::Regular,
                },
                io,
            )
        },
        vec![Read(path!("1"), File), Destroy(path!("2"))],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(path!("1"), t.canonicalization(path!("1").as_path()).unwrap());
}

#[test]
fn replace_file_with_directory() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1", "2"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("1"), File)],
    );
    assert!(t.canonicalization(path!("1", "2").as_path()).is_err());
    expect_commands(|io| t.handle_event(&path!("1"), BackendEvent::Removed, io), vec![]);
    let updates = expect_commands(
        |io| {
            t.handle_event(
                &path!("1"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![Read(path!("1"), Directory), Read(path!("1", "2"), Directory)],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(path!("1", "2"), t.canonicalization(path!("1", "2").as_path()).unwrap());
}

#[test]
fn finite_symlink_cycle() {
    enable_logging();

    debug!("register a path that loops through the same symlink several times");
    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1", "s", "s", "s", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "s"), Symlink(path!("1"))),
            Read(path!("1", "3"), Directory),
        ],
    );

    debug!("the path should be canonicalized properly");
    assert_eq!(
        path!("1", "3"),
        t.canonicalization(path!("1", "s", "s", "s", "3").as_path()).unwrap()
    );
    assert_eq!(vec![path!("1", "s", "s", "s", "3").as_path()], t.aliases(path!("1", "3").as_path()));

    debug!("delete the symlink and expect a notification about broken path");
    let updates = expect_commands(
        |io| t.handle_event(path!("1", "s").as_path(), BackendEvent::Removed, io),
        vec![Destroy(path!("1", "3"))],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Broken)], sorted_updates(updates));

    debug!("create the symlink again, the path should be resolved");
    let updates = expect_commands(
        |io| {
            t.handle_event(
                path!("1", "s").as_path(),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Symlink,
                },
                io,
            )
        },
        vec![
            Read(path!("1", "s"), Symlink(PathBuf::from("../1"))),
            Read(path!("1", "3"), Directory),
        ],
    );
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
    assert_eq!(
        path!("1", "3"),
        t.canonicalization(path!("1", "s", "s", "s", "3").as_path()).unwrap()
    );

    debug!("unregister the path");
    expect_commands(
        |i| t.unregister(key, i),
        vec![Destroy(path!("1", "3")), Destroy(path!("1")), Destroy(root!())],
    );
    assert!(t.is_empty(), "supposed to be empty: {t:#?}");
}

#[test]
fn symlink_cycle() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1", "s", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "s"), Symlink(PathBuf::from("../1/s"))),
        ],
    );

    assert!(t.canonicalization(path!("1", "s", "3").as_path()).is_err());

    expect_commands(|i| t.unregister(key, i), vec![Destroy(path!("1")), Destroy(root!())]);
    assert!(t.is_empty(), "supposed to be empty: {t:#?}");
}

#[test]
fn cyclic_symlink_can_be_repaired() {
    enable_logging();
    let mut t = Tracker::new();
    expect_commands(
        |io| t.register(path!("1", "s", "3").to_path_buf(), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "s"), Symlink(path!("1", "s"))),
        ],
    );
    // Removing first, to test that we do not forget about the path.
    // Because the symlink is resolved to itself, deletion could accidentally remove all mentions of the path.
    expect_commands(|io| t.handle_event(&path!("1", "s"), BackendEvent::Removed, io), vec![]);
    expect_commands(
        |io| {
            t.handle_event(
                &path!("1", "s"),
                BackendEvent::RecentlyCreated {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![Read(path!("1", "s"), Directory), Read(path!("1", "s", "3"), Directory)],
    );
    assert_eq!(path!("1", "s", "3"), t.canonicalization(&path!("1", "s", "3")).unwrap())
}

#[test]
fn cyclic_symlink_chain() {
    enable_logging();

    let mut t = Tracker::new();
    let key = expect_commands(
        |io| t.register(path!("1", "s1", "3"), MissingPolicy::Track, io).unwrap().key,
        vec![
            Read(root!(), Directory),
            Read(path!("1"), Directory),
            Read(path!("1", "s1"), Symlink(path!("2", "s2"))),
            Read(path!("2"), Directory),
            Read(path!("2", "s2"), Symlink(path!("1", "s1"))),
        ],
    );

    assert!(t.canonicalization(path!("1", "s1", "3").as_path()).is_err());

    expect_commands(
        |io| t.unregister(key, io),
        vec![Destroy(path!("1")), Destroy(path!("2")), Destroy(root!())],
    );
    assert!(t.is_empty(), "supposed to be empty: {t:#?}");
}

#[test]
fn change_to_known_directory_doesnt_cause_io() {
    enable_logging();

    let mut t = Tracker::new();
    expect_commands(
        |io| t.register(path!("1"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("1"), Directory)],
    );

    let changes = expect_commands(
        |io| {
            t.handle_event(
                path!("1").as_path(),
                BackendEvent::Changed {
                    file_type: FileType::Directory,
                },
                io,
            )
        },
        vec![],
    );

    assert!(changes.is_empty())
}

#[test]
fn change_to_known_file_doesnt_cause_io() {
    enable_logging();

    let mut t = Tracker::new();
    expect_commands(
        |io| t.register(path!("1"), MissingPolicy::Track, io).unwrap().key,
        vec![Read(root!(), Directory), Read(path!("1"), File)],
    );

    let changes = expect_commands(
        |io| {
            t.handle_event(
                path!("1").as_path(),
                BackendEvent::Changed {
                    file_type: FileType::Regular,
                },
                io,
            )
        },
        vec![],
    );

    assert!(changes.is_empty())
}

#[test]
fn overflow_with_symlink() {
    enable_logging();
    // This is a special case because the removal of a subtree with a symlink inside triggers recursion on the symlinked path.
    // Overflow is the easiest way to trigger it in the wild.

    let mut t = Tracker::new();
    let target_path = path!("1", "symlink", "3");
    let expectations = vec![
        Read(root!(), Directory),
        Read(path!("1"), Directory),
        Read(path!("1", "symlink"), Symlink(path!("2"))),
        Read(path!("2"), Directory),
        Read(path!("2", "3"), Directory),
    ];
    let key = expect_commands(|io| t.register(target_path.clone(), MissingPolicy::Track, io).unwrap().key, expectations.clone());
    debug!("emulate overflow on root");
    let updates = expect_commands(|io| t.handle_event(&root!(), BackendEvent::Overflow, io), expectations);
    assert_eq!(vec![(key, CanonicalizationUpdate::Resolved)], sorted_updates(updates));
}
