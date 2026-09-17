use crate::util::path_util::PathExt;
use futures::FutureExt;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use crate::test_helpers::*;

use super::{test, test_options};
use crate::{backend::*, canonicalization::CanonicalizationError, session::ClientOverflow, *};

macro_rules! expect_event {
    ($session:expr) => {{
        let s = $session;
        s.next_event().now_or_never().map(|r| r.expect("overflow")).expect("no event")
    }};
}

macro_rules! expect_nothing {
    ($session:expr) => {
        let s = $session;
        let r = s.next_event().now_or_never();
        assert_eq_pretty!(None, r, "expected no events");
    };
}

macro_rules! expect_changed_file {
    ($session:expr, $path:expr) => {{
        let s = $session;
        let path = $path.to_path_buf();
        let evt = s.next_event().now_or_never().map(|r| r.expect("overflow")).expect("no event");
        assert_eq_pretty!(
            Event::Dirty {
                path: path,
                file_type: FileType::Regular
            },
            evt,
            "Expected change event for file"
        );
    }};
}

macro_rules! expect_changed_dir {
    ($session:expr, $path:expr) => {
        let s = $session;
        let path = $path.to_path_buf();
        let evt = s.next_event().now_or_never().map(|r| r.expect("overflow")).expect("no event");
        assert_eq_pretty!(
            Event::Dirty {
                path: path,
                file_type: FileType::Directory
            },
            evt,
            "Expected change event for directory"
        );
    };
}

macro_rules! expect_changed_symlink {
    ($session:expr, $path:expr) => {{
        let s = $session;
        let path = $path.to_path_buf();
        let evt = s.next_event().now_or_never().map(|r| r.expect("overflow")).expect("no event");
        assert_eq_pretty!(
            Event::Dirty {
                path: path,
                file_type: FileType::Symlink
            },
            evt,
            "Expected change event for file"
        );
    }};
}

macro_rules! expect_rescan {
    ($session:expr, $path:expr) => {{
        let s = $session;
        let path = $path.to_path_buf();
        let evt = s.next_event().now_or_never().map(|r| r.expect("overflow")).expect("no event");
        assert_eq_pretty!(Event::Rescan { path }, evt, "Expected rescan event for directory");
    }};
}

macro_rules! expect_removed {
    ($session:expr, $path:expr) => {
        let s = $session;
        let path = $path.to_path_buf();
        let evt = s.next_event().now_or_never().map(|r| r.expect("overflow")).expect("no event");
        assert_eq_pretty!(Event::Removed { path }, evt, "Expected change event for directory");
    };
}

#[test]
fn add_root_on_existing_directory() {
    let dir = files!({"child" => "content"});
    let root_path = dir.path();
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(root_path, Scope::DirectChildren);
        fake.emulate_dir_changed(root_path);
        fake.emulate_file_changed(root_path.join("child"));
        expect_changed_dir!(&mut session, root_path);
        expect_changed_file!(&mut session, root_path.join("child"));
        expect_nothing!(&mut session);
    })
}

#[test]
fn add_nested_watch() {
    let dir = files!({
        "dir1" => {
            "file" => "content",
            "dir2" => { "file" => "content"},
            "ignored_dir" => { "file" => "content" }
        },
        "ignored_dir" => { "file" => "content" },
    });
    let root_path = dir.path();
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(root_path, Scope::DirectChildren);
        session.watch_directory(root_path.join("dir1"), Scope::DirectChildren).unwrap();
        session.watch_directory(root_path.join("dir1/dir2"), Scope::DirectChildren).unwrap();

        fake.emulate_file_changed(root_path.join("dir1/file"));
        fake.emulate_file_changed(root_path.join("ignored_dir/file"));
        fake.emulate_file_changed(root_path.join("dir1/dir2/file"));
        fake.emulate_file_changed(root_path.join("dir1/ignored_dir/file"));

        expect_changed_file!(&mut session, root_path.join("dir1/file"));
        expect_changed_file!(&mut session, root_path.join("dir1/dir2/file"));
        expect_nothing!(&mut session);
    })
}

#[test]
fn watch_directory_without_root() {
    // A canonical path can be watched purely with `watch_directory`, segment by segment, without any prior `watch_root`.
    let dir = files!({ "a" => { "b" => { "c" => { "file" => "content" } } } });
    let canonical = dir.path().join("a/b/c").canonicalize().expect("canonicalize temp dir");
    test(|fake, watcher| {
        let mut session = watcher.session();
        for prefix in canonical.prefixes() {
            session
                .watch_directory(prefix, Scope::DirectChildren)
                .unwrap_or_else(|e| panic!("watch_directory({prefix:?}) failed: {e:?}"));
        }

        // a change to a direct child of the deepest watched directory is delivered under its full path
        let file = canonical.join("file");
        fake.emulate_file_changed(&file);
        expect_changed_file!(&mut session, file);
        expect_nothing!(&mut session);
    })
}

#[test]
fn watch_directory_requires_watched_parent() {
    // watch_directory trusts the client to have watched every segment leading up to the path. The
    // backend enforces this: skipping an intermediate directory is refused, because a gap in the
    // watched chain could hide an unregistered symlink and yield a bogus canonical path.
    let dir = files!({ "root" => { "a" => { "b" => { "file" => "content" } } } });
    let root_path = dir.path().join("root");
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&root_path, Scope::DirectChildren);

        // Jumping straight to root/a/b without watching root/a first is rejected...
        let skipped = session.watch_directory(root_path.join("a/b"), Scope::DirectChildren);
        assert!(
            matches!(skipped, Err(WatchError::Backend(BackendError::DetachedParent))),
            "expected the detached watch to be refused, got {skipped:?}"
        );
        // ...and nothing is left behind, so no event leaks through the rejected watch.
        fake.emulate_file_changed(root_path.join("a/b/file"));
        expect_nothing!(&mut session);

        // Watching each segment in order re-establishes a contiguous chain and succeeds.
        session.watch_directory(root_path.join("a"), Scope::DirectChildren).unwrap();
        session.watch_directory(root_path.join("a/b"), Scope::DirectChildren).unwrap();

        fake.emulate_file_changed(root_path.join("a/b/file"));
        expect_changed_file!(&mut session, root_path.join("a/b/file"));
        expect_nothing!(&mut session);
    })
}

#[test]
fn session_receives_events_from_all_added_roots() {
    let dir = files!({
        "a" => { },
        "b" => { },
    });
    test(|fake, watcher| {
        let mut session = watcher.session();
        let root_path1 = dir.path().join("a");
        let root_path2 = dir.path().join("b");
        session.watch_root(&root_path1, Scope::DirectChildren);
        session.watch_root(&root_path2, Scope::DirectChildren);

        fake.emulate_dir_changed(&root_path1);
        fake.emulate_dir_changed(&root_path2);

        expect_changed_dir!(&mut session, root_path1);
        expect_changed_dir!(&mut session, root_path2);
        expect_nothing!(&mut session);
    })
}

#[test]
fn session_wont_receive_events_from_watches_added_to_other_sessions() {
    let dir = files!({
        "a" => {},
        "b" => {},
    });
    test(|fake, watcher| {
        let mut session1 = watcher.session();
        let mut session2 = watcher.session();
        let root_path1 = dir.path().join("a");
        let root_path2 = dir.path().join("b");
        session1.watch_root(&root_path1, Scope::DirectChildren);
        session2.watch_root(&root_path2, Scope::DirectChildren);

        fake.emulate_dir_changed(&root_path2);

        expect_nothing!(&mut session1);

        expect_changed_dir!(&mut session2, root_path2);
        expect_nothing!(&mut session2);
    })
}

#[test]
fn session_wont_receive_events_from_nested_watches_of_the_same_root_added_to_other_sessions() {
    // two sessions for the same root, but one ignores a sub-directory
    let dir = files!({
        "a" => {},
        "b" => { "foo" => "" },
    });
    test(|fake, watcher| {
        let mut session1 = watcher.session();
        let mut session2 = watcher.session();
        let root_path = dir.path();
        session1.watch_root(root_path, Scope::DirectChildren);
        session1.watch_directory(root_path.join("a"), Scope::DirectChildren).unwrap();
        session1.watch_directory(root_path.join("b"), Scope::DirectChildren).unwrap();
        session2.watch_root(&root_path, Scope::DirectChildren);
        session2.watch_directory(root_path.join("a"), Scope::DirectChildren).unwrap();

        fake.emulate_file_changed(root_path.join("b/foo"));

        expect_changed_file!(&mut session1, root_path.join("b/foo"));
        expect_nothing!(&mut session2);
    })
}

#[test]
fn add_root_on_non_existing_path() {
    let dir = files!({});
    let target_path = dir.path().join("dir/another-dir/target");
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&target_path, Scope::DirectChildren);
        dir.create_dir("dir");
        fake.emulate_dir_created(dir.path().join("dir"));

        dir.create_dir("dir/another-dir");
        fake.emulate_dir_created(dir.path().join("dir/another-dir"));

        dir.create_dir("dir/another-dir/unrelated");
        fake.emulate_dir_created(dir.path().join("dir/another-dir/unrelated"));

        expect_nothing!(&mut session);

        dir.create_dir("dir/another-dir/target");
        fake.emulate_dir_created(dir.path().join("dir/another-dir/target"));
        expect_rescan!(&mut session, target_path);
        expect_nothing!(&mut session);
    })
}

#[test]
fn root_is_deleted_and_created_again() {
    let dir = files!({"root" => { "foo" => {} }});
    let root_path = dir.path().join("root");
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&root_path, Scope::DirectChildren);

        dir.delete("root/foo");
        fake.emulate_removed(root_path.join("foo"));
        dir.delete("root");
        fake.emulate_removed(&root_path);

        expect_removed!(&mut session, root_path.join("foo"));

        expect_removed!(&mut session, root_path);
        expect_nothing!(&mut session);

        dir.create_dir("root");
        fake.emulate_dir_created(&root_path);
        expect_rescan!(&mut session, root_path);

        dir.create_dir("root/foo");
        fake.emulate_dir_created(root_path.join("foo"));
        expect_changed_dir!(&mut session, root_path.join("foo"));
        expect_nothing!(&mut session);
    })
}

#[test]
fn add_root_with_symlink_segment() {
    let dir = files!({
        "1" => { "real_dir" => { "target" => { "file" => "content"} } },
        "2" => { "real_dir" => { } } ,
    });
    let symlink_path = dir.path().join("symlink");
    symlink(Path::new("1/real_dir"), &symlink_path);
    let symbolic_root = symlink_path.join("target");
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&symbolic_root, Scope::DirectChildren);
        // emulate a change on the canonical path and expect an event for the symbolic one
        fake.emulate_file_changed(dir.path().join("1/real_dir/target/file"));
        expect_changed_file!(&mut session, symbolic_root.join("file"));
        expect_nothing!(&mut session);

        // retarget the symlink to a directory that has no `target` inside
        delete(&symlink_path);
        symlink(Path::new("2/real_dir"), &symlink_path);
        fake.emulate_symlink_changed(&symlink_path);
        expect_removed!(&mut session, symbolic_root);
        expect_nothing!(&mut session);
        // watches on the old canonical path should be destroyed
        fake.inspect_tree(|t| assert_eq!(HashSet::new(), t.query(&dir.path().join("1/real_dir")).unwrap()));
        fake.inspect_tree(|t| assert_eq!(HashSet::new(), t.query(&dir.path().join("1/real_dir/target")).unwrap()));
        fake.emulate_file_changed(dir.path().join("1/real_dir/target/file"));
        expect_nothing!(&mut session);

        // now that the target is created, we should get an event
        create_dir(dir.path().join("2/real_dir/target"));
        fake.emulate_dir_created(&dir.path().join("2/real_dir/target"));
        expect_rescan!(&mut session, symbolic_root);
        expect_nothing!(&mut session);
    })
}

#[test]
fn event_is_delivered_to_all_registered_aliases() {
    let dir = files!({ "canonical" => { "dir" => { "file" => "content" } }});
    // two symlinks that point to the same directory
    symlink(dir.path().join("canonical"), dir.path().join("s1"));
    symlink(dir.path().join("canonical"), dir.path().join("s2"));

    test(|fake, watcher| {
        let mut session = watcher.session();
        // the session observes two paths that point to the same directory via different symlinks
        let root_path1 = dir.path().join("s1/dir");
        let root_path2 = dir.path().join("s2/dir");
        session.watch_root(&root_path1, Scope::DirectChildren);
        session.watch_root(&root_path2, Scope::DirectChildren);

        fake.emulate_file_changed(&dir.path().join("canonical/dir/file"));
        let mut actual = HashSet::<Event>::new();
        actual.insert(expect_event!(&mut session));
        actual.insert(expect_event!(&mut session));
        expect_nothing!(&mut session);
        let expected = vec![
            Event::Dirty {
                path: root_path1.join("file"),
                file_type: FileType::Regular,
            },
            Event::Dirty {
                path: root_path2.join("file"),
                file_type: FileType::Regular,
            },
        ]
        .into_iter()
        .collect::<HashSet<_>>();
        assert_eq_pretty!(expected, actual);
    })
}

#[test]
fn sibling_symlink_roots_to_same_canonical_both_removed() {
    // Two sibling symlinks point at the same canonical directory, and each is watched as a root.
    // Both roots alias the *same* canonical node, yet their symbolic paths are siblings
    // (neither is an ancestor of the other), so a canonicalization break must be delivered to
    // BOTH roots independently.
    let dir = files!({ "canonical" => { "sub" => {} } });
    symlink(dir.path().join("canonical"), dir.path().join("s1"));
    symlink(dir.path().join("canonical"), dir.path().join("s2"));

    let root1 = dir.path().join("s1/sub");
    let root2 = dir.path().join("s2/sub");
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&root1, Scope::DirectChildren);
        session.watch_root(&root2, Scope::DirectChildren);

        // delete the shared canonical directory behind both symlinks
        dir.delete("canonical/sub");
        fake.emulate_removed(dir.path().join("canonical/sub"));

        let mut actual = HashSet::<Event>::new();
        actual.insert(expect_event!(&mut session));
        actual.insert(expect_event!(&mut session));
        expect_nothing!(&mut session);
        let expected = [Event::Removed { path: root1.clone() }, Event::Removed { path: root2.clone() }]
            .into_iter()
            .collect::<HashSet<_>>();
        assert_eq_pretty!(expected, actual);
    })
}

#[test]
fn retarget_notifies_root_not_its_traversal_symlinks() {
    let dir = files!({
        "before" => { "sub" => {} },
        "after"  => { },
    });
    let link = dir.path().join("link");
    symlink(dir.path().join("before"), &link); // link -> before
    let root = link.clone(); // /base/link, watched as a root
    let traversal = link.join("sub"); // /base/link/sub, registered for traversal only
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&root, Scope::DirectChildren);
        session.watch_symlink(&traversal, Scope::DirectChildren).unwrap();

        // retarget the symlink to a directory that has no `sub`
        delete(&link);
        symlink(dir.path().join("after"), &link);
        fake.emulate_symlink_changed(&link);

        // only the root is notified; the traversal symlink under it produces no separate event
        expect_rescan!(&mut session, root);
        expect_nothing!(&mut session);
    })
}

#[test]
fn session_with_nested_roots_should_not_duplicate_events() {
    let dir = files!({ "root1" => { "root2" => { "file" => "content" } } });
    let root_path1 = dir.path().join("root1");
    let root_path2 = dir.path().join("root1/root2");
    let file_path = dir.path().join("root1/root2/file");

    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&root_path1, Scope::DirectChildren);
        session.watch_root(root_path2, Scope::DirectChildren);

        fake.emulate_file_changed(&file_path);
        expect_changed_file!(&mut session, file_path);
        expect_nothing!(&mut session);
    })
}

#[test]
fn add_watch_is_idempotent() {
    let dir = files!({ "dir" => { "file" => "content" } });
    let root_path = dir.path();
    let file_path = root_path.join("dir/file");
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&root_path, Scope::DirectChildren);
        // add several watches for the same path
        session.watch_directory(&root_path.join("dir"), Scope::DirectChildren).unwrap();
        session.watch_directory(&root_path.join("dir"), Scope::DirectChildren).unwrap();
        session.watch_directory(&root_path.join("dir"), Scope::DirectChildren).unwrap();

        fake.emulate_file_changed(&file_path);

        // assert there is only one event
        expect_changed_file!(&mut session, file_path);
        expect_nothing!(&mut session);
    })
}

#[test]
fn client_overflow() {
    let dir = files!({ "a" => "a", "b" => "b", "c" => "c" });
    let root_path = dir.path();
    test_options(1, |fake, watcher| {
        // configure the session to use the smallest buffer possible
        let mut session = watcher.session();
        session.watch_root(root_path, Scope::DirectChildren);

        // overflow it
        fake.emulate_dir_changed(root_path.join("a"));
        fake.emulate_dir_changed(root_path.join("b"));
        fake.emulate_dir_changed(root_path.join("c"));

        assert_eq_pretty!(Err(ClientOverflow), session.next_event().now_or_never().unwrap());
        expect_nothing!(&mut session);
    })
}

#[test]
fn backend_overflow() {
    let dir = files!({ "subdir" => { "file" => "content" } });
    let root_path = dir.path();
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(root_path, Scope::DirectChildren);
        session.watch_directory(root_path.join("subdir"), Scope::DirectChildren).unwrap();

        let root_prefix = root_path.prefixes().next().unwrap();
        fake.emulate_overflow(root_prefix);
        expect_rescan!(&mut session, root_path);
        expect_nothing!(&mut session);
    })
}

#[test]
fn dropping_last_session_clears_everything() {
    let dir = files!({ "subdir" => {} });
    let root_path = dir.path();
    test(|fake, watcher| {
        let session = watcher.session();
        session.watch_root(root_path, Scope::DirectChildren);
        session.watch_directory(root_path.join("subdir"), Scope::DirectChildren).unwrap();
        drop(session);

        let d = watcher.debug();
        assert!(d.dispatch.is_empty(), "state should be empty: {d:?}");
        fake.inspect_tree(|t| assert!(t.is_empty(), "watches tree should be empty: {t:?}"));
    })
}

#[test]
fn watch_nested_symlink() {
    let dir = files!({
        "target" => { /* symlink to "symlinked" here */ },
        "symlinked" => { "dir" => { "file" => "content" } },
        "symlinked_after" => { "dir" => { "file" => "content" } }
    });
    symlink(Path::new("../symlinked"), &dir.path().join("target").join("symlink"));
    test(|fake, watcher| {
        let mut session = watcher.session();
        // target
        session.watch_root(dir.path().join("target"), Scope::DirectChildren);
        // target/symlink
        session
            .watch_symlink(dir.path().join("target/symlink"), Scope::DirectChildren)
            .unwrap();
        // it is idempotent
        session
            .watch_symlink(dir.path().join("target/symlink"), Scope::DirectChildren)
            .unwrap();
        // target/symlink/dir
        session
            .watch_directory(dir.path().join("target/symlink/dir"), Scope::DirectChildren)
            .unwrap();

        // the event should be delivered to the symlinked path
        fake.emulate_file_changed(dir.path().join("symlinked/dir/file"));
        expect_changed_file!(&mut session, dir.path().join("target/symlink/dir/file"));

        // change the target of the nested symlink
        delete(dir.path().join("target").join("symlink"));
        symlink(Path::new("../symlinked_after"), &dir.path().join("target").join("symlink"));
        fake.emulate_symlink_changed(dir.path().join("target/symlink"));
        // the client should be notified
        // TODO what am I supposed to do with this event
        expect_changed_symlink!(&mut session, dir.path().join("target/symlink"));
        expect_rescan!(&mut session, dir.path().join("target/symlink"));
        // the nested watch should be destroyed
        fake.inspect_tree(|w| assert!(w.records(&dir.path().join("symlinked/dir")).is_empty()));
        // now it can establish a new watch
        session
            .watch_directory(dir.path().join("target/symlink/dir"), Scope::DirectChildren)
            .unwrap();
        // events from the new target should be delivered
        fake.emulate_file_changed(dir.path().join("symlinked_after/dir/file"));
        expect_changed_file!(&mut session, dir.path().join("target/symlink/dir/file"));
    })
}

#[test]
fn outer_symlink_change_destroys_inner_symlink_watch() {
    let dir = files!({
        "outer_target" => { /* symlink to "inner_target" here */ },
        "outer_target_after" => { /* symlink to "inner_target_after" here */ },
        "inner_target" => { "dir" => { "file" => "content" } },
    });
    symlink(Path::new("outer_target"), &dir.path().join("outer"));
    symlink(Path::new("../inner_target"), &dir.path().join("outer_target").join("inner"));
    symlink(Path::new("../inner_target"), &dir.path().join("outer_target_after").join("inner"));

    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(dir.path().join("outer"), Scope::DirectChildren);
        session
            .watch_symlink(dir.path().join("outer/inner"), Scope::DirectChildren)
            .unwrap();
        session.watch_directory(dir.path().join("outer/inner"), Scope::Recursive).unwrap();

        fake.emulate_file_changed(dir.path().join("inner_target/dir/file"));
        expect_changed_file!(&mut session, dir.path().join("outer/inner/dir/file"));

        delete(dir.path().join("outer"));
        symlink(Path::new("outer_target_after"), &dir.path().join("outer"));
        fake.emulate_symlink_changed(dir.path().join("outer"));

        expect_rescan!(&mut session, dir.path().join("outer"));
        expect_nothing!(&mut session);

        fake.inspect_tree(|w| assert_eq!(HashSet::new(), w.query(&dir.path().join("outer_target/inner")).unwrap()));
        // the nested watches should be destroyed
        fake.inspect_tree(|w| assert_eq!(HashSet::new(), w.query(&dir.path().join("inner_target/dir")).unwrap()));

        fake.emulate_file_changed(dir.path().join("inner_target/dir/file"));
        expect_nothing!(&mut session);
    });
}

#[test]
fn remove_path_under_root_destroys_watches() {
    let dir = files!({
        "root" => {
            "dir1" => {
                "dir2" => { "file" => "content" }
            }
        }
    });
    let root_path = dir.path().join("root");
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(&root_path, Scope::DirectChildren);
        session.watch_directory(root_path.join("dir1"), Scope::DirectChildren).unwrap();
        session.watch_directory(root_path.join("dir1/dir2"), Scope::DirectChildren).unwrap();

        session.remove_watches(root_path.join("dir1"));

        // Events from the removed sub-tree should no longer reach the session
        fake.emulate_dir_changed(root_path.join("dir1/dir2"));
        fake.emulate_file_changed(root_path.join("dir1/dir2/file"));
        // All nested watches are removed
        fake.inspect_tree(|t| assert_eq!(Vec::<PathBuf>::new(), t.records(&root_path.join("dir1"))));
        expect_nothing!(&mut session);
    })
}

#[test]
fn remove_path_containing_root_destroys_root() {
    let dir = files!({
        "parent" => {
            "root" => { "dir" => {} }
        }
    });
    let parent_path = dir.path().join("parent");
    let root_path = parent_path.join("root");
    test(|fake, watcher| {
        let session = watcher.session();
        session.watch_root(&root_path, Scope::DirectChildren);
        session.watch_directory(&root_path.join("dir"), Scope::DirectChildren).unwrap();

        session.remove_watches(&parent_path);

        let debug_state = watcher.debug();
        // Only the session's own canonicalization subscription remains
        let almost_empty = debug_state.dispatch.subscriptions.len() == 1 && debug_state.dispatch.sessions.len() == 1;
        assert!(almost_empty, "root should be fully cleaned up: {debug_state:?}");
        fake.inspect_tree(|t| assert!(t.is_empty(), "all watches should be removed: {t:?}"));
    })
}

#[test]
fn remove_path_with_symlink_destroys_symlink_and_watches() {
    let dir = files!({
        "root" => {},
        "target" => { "subdir" => {} }
    });
    let root_path = dir.path().join("root");
    let target_path = dir.path().join("target");
    let symlink_path = root_path.join("symlink");
    symlink(&target_path, &symlink_path);
    test(|fake, watcher| {
        let session = watcher.session();
        session.watch_root(&root_path, Scope::DirectChildren);
        session.watch_symlink(&symlink_path, Scope::DirectChildren).unwrap();
        session.watch_directory(symlink_path.join("subdir"), Scope::DirectChildren).unwrap();

        session.remove_watches(&symlink_path);

        // The canonical watch for symlink/subdir should be removed
        fake.inspect_tree(|t| assert_eq!(Vec::<PathBuf>::new(), t.records(&target_path), "symlink watch should be removed"));
    })
}

#[test]
fn watch_symlinked_directory() {
    let dir = files!({
        "root" => { /* symlink to dir here */ },
        "dir" => {
            "file" => "",
        },
        "dir2" => {
            "file" => "",
        },
    });
    // establish symlink: root/symlink -> ../dir
    let symlink_path = dir.path().join("root/symlink");
    symlink(Path::new("../dir"), &symlink_path);
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(dir.path().join("root"), Scope::DirectChildren);
        session.watch_symlink(&symlink_path, Scope::DirectChildren).unwrap();

        // emulate a change on dir/file (the canonical path behind the symlink)
        fake.emulate_file_changed(dir.path().join("dir/file"));
        // the event should be delivered under the symlink path
        expect_changed_file!(&mut session, symlink_path.join("file"));
        expect_nothing!(&mut session);

        // After the symlink is retargeted to a different directory,
        // events from the NEW target should be delivered under the symbolic path,
        // without the client re-subscribing to the symlink itself.
        delete(&symlink_path);
        symlink(dir.path().join("dir2"), &symlink_path);
        fake.emulate_symlink_changed(&symlink_path);
        expect_changed_symlink!(&mut session, &symlink_path);
        expect_rescan!(&mut session, &symlink_path);

        // events from the NEW target should now be delivered under the symbolic path
        fake.emulate_file_changed(dir.path().join("dir2/file"));
        expect_changed_file!(&mut session, symlink_path.join("file"));
        expect_nothing!(&mut session);
    })
}

#[test]
fn watch_symlinked_directory_recursively() {
    let dir = files!({
        "root" => { /* symlink to dir here */ },
        "dir" => {
            "subdir" => {
                "file" => "",
            },
        },
    });
    // establish symlink: root/symlink -> ../dir
    let symlink_path = dir.path().join("root/symlink");
    symlink(Path::new("../dir"), &symlink_path);
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(dir.path().join("root"), Scope::DirectChildren);
        session.watch_symlink(&symlink_path, Scope::Recursive).unwrap();

        // emulate a change on dir/subdir/file (the canonical path behind the symlink)
        fake.emulate_file_changed(dir.path().join("dir/subdir/file"));
        // the event should be delivered under the symlink path, including the nested subdir
        expect_changed_file!(&mut session, symlink_path.join("subdir/file"));
        expect_nothing!(&mut session);
    })
}

#[test]
fn watch_symlinked_file() {
    let dir = files!({
        "root" => { /* symlink to dir/file here */ },
        "dir" => {
            "file" => "",
            "sibling" => "",
        },
    });
    // establish symlink: root/symlink -> ../dir/file (a regular file, not a directory)
    let symlink_path = dir.path().join("root/symlink");
    symlink(dir.path().join("dir/file"), &symlink_path);
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(dir.path().join("root"), Scope::DirectChildren);
        // watch the symlink
        session.watch_symlink(&symlink_path, Scope::DirectChildren).unwrap();

        // emulate a change on dir/file (the canonical target of the symlink)
        fake.emulate_file_changed(dir.path().join("dir/file"));
        // emulate a change on dir/sibling, a file that is of no interest
        fake.emulate_file_changed(dir.path().join("dir/sibling"));
        // the event should be delivered as a change on root/symlink
        expect_changed_file!(&mut session, &symlink_path);
        expect_nothing!(&mut session);
    })
}

#[test]
fn watch_dangling_file_symlink() {
    let dir = files!({
        "root" => { /* symlink to dir/file here */ },
        "dir" => { },
    });
    // establish symlink: root/symlink -> ../dir/file (dangling, file does not exist)
    let symlink_path = dir.path().join("root/symlink");
    symlink(dir.path().join("dir/file"), &symlink_path);
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(dir.path().join("root"), Scope::DirectChildren);
        session.watch_symlink(&symlink_path, Scope::DirectChildren).unwrap();

        // create the file
        dir.create("dir/file");
        fake.emulate_file_changed(dir.path().join("dir/file"));
        expect_rescan!(&mut session, &symlink_path);
        expect_nothing!(&mut session);
        // change the file
        fake.emulate_file_changed(dir.path().join("dir/file"));
        expect_changed_file!(&mut session, &symlink_path);
        expect_nothing!(&mut session);

        // delete the file
        dir.delete("dir/file");
        fake.emulate_removed(dir.path().join("dir/file"));
        expect_removed!(&mut session, &symlink_path);
        expect_nothing!(&mut session);

        // create the file again
        dir.create("dir/file");
        fake.emulate_file_changed(dir.path().join("dir/file"));
        expect_rescan!(&mut session, &symlink_path);
        expect_nothing!(&mut session);
    })
}

#[test]
fn watch_dangling_symlink_to_directory() {
    let dir = files!({
        "root" => { /* symlink to dir here */ },
        // "dir" does not exist initially
    });
    // establish symlink: root/symlink -> ../dir (dangling, dir does not exist)
    let symlink_path = dir.path().join("root/symlink");
    symlink(dir.path().join("dir"), &symlink_path);
    test(|fake, watcher| {
        let mut session = watcher.session();
        session.watch_root(dir.path().join("root"), Scope::DirectChildren);
        session.watch_symlink(&symlink_path, Scope::DirectChildren).unwrap();

        // create the directory, expect rescan
        dir.create_dir("dir");
        fake.emulate_dir_created(dir.path().join("dir"));
        expect_rescan!(&mut session, &symlink_path);
        expect_nothing!(&mut session);

        // check that file events inside are delivered
        fake.emulate_file_changed(dir.path().join("dir/file"));
        expect_changed_file!(&mut session, symlink_path.join("file"));
        expect_nothing!(&mut session);

        // delete the directory, expect removed
        dir.delete("dir");
        fake.emulate_removed(dir.path().join("dir"));
        expect_removed!(&mut session, &symlink_path);
        expect_nothing!(&mut session);

        // create the directory again, expect rescan
        dir.create_dir("dir");
        fake.emulate_dir_created(dir.path().join("dir"));
        expect_rescan!(&mut session, &symlink_path);
        expect_nothing!(&mut session);

        // check that file events inside are delivered
        fake.emulate_file_changed(dir.path().join("dir/file"));
        expect_changed_file!(&mut session, symlink_path.join("file"));
        expect_nothing!(&mut session);
    })
}

#[test]
fn watch_symlink_on_non_existing_link_returns_error() {
    let dir = files!({});
    // nothing exists at this path; it is not a symlink at all
    let missing_link = dir.path().join("missing");
    test(|_fake, watcher| {
        let session = watcher.session();
        session.watch_root(dir.path(), Scope::DirectChildren);

        let subscriptions_before = watcher.debug().dispatch.subscriptions.len();

        let result = session.watch_symlink(&missing_link, Scope::DirectChildren);
        assert!(
            matches!(result, Err(WatchError::CanonicalizationError(CanonicalizationError::PathDoesNotExist))),
            "watch_symlink on a non-existing link should fail with PathDoesNotExist, got: {result:?}"
        );

        // No symbolic key must be registered for a link that does not exist.
        let debug_state = watcher.debug();
        assert_eq!(
            subscriptions_before,
            debug_state.dispatch.subscriptions.len(),
            "no subscription should be registered for a non-existing link: {debug_state:?}"
        );
    })
}

#[test]
fn dropping_tx_causes_rx_to_return_none() {
    let dir = files!({ "file" => "content" });
    let root_path = dir.path();
    let file_path = dir.path().join("file");
    test(|fake, watcher| {
        let session = watcher.session();
        session.watch_root(root_path, Scope::DirectChildren);
        let (tx, mut rx) = session.split();
        fake.emulate_file_changed(&file_path);
        let evt = rx.next_event().now_or_never().flatten().map(|r| r.expect("overflow"));
        assert_eq_pretty!(
            Some(Event::Dirty {
                path: file_path.to_path_buf(),
                file_type: FileType::Regular
            }),
            evt
        );

        drop(tx);
        fake.emulate_file_changed(&file_path);
        let evt = rx
            .next_event()
            .now_or_never()
            .expect("future should be completed")
            .map(|r| r.expect("overflow"));
        assert_eq_pretty!(None, evt, "expected None after dropping WatchSessionTx");
    })
}

#[test]
fn concurrent_traverse_and_retarget_does_not_leak_stale_watch() {
    let dir = files!({
        "root" => { /* symlink "link" -> ../a here */ },
        "a" => { "target" => { "sub" => {} } },
    });
    let base = dir.path().join("root");
    let link = base.join("link");
    symlink(dir.path().join("a"), &link); // root/link -> a
    let sym_target = link.join("target"); // symbolic dir reached through the symlink

    test(|fake, watcher| {
        let session = watcher.session();
        let (tx, mut rx) = session.split();

        tx.watch_root(&base, Scope::DirectChildren);
        tx.watch_symlink(&link, Scope::DirectChildren).unwrap();

        fake.emulate_overflow(&base);

        // rx processes the event: the platform rebuilds/cleans and returns a Rescan for the root.
        let evt = rx.next_event().now_or_never().flatten().map(|r| r.expect("overflow"));
        assert_eq_pretty!(Some(Event::Rescan { path: base.to_path_buf() }), evt);

        assert!(
            matches!(
                tx.watch_directory(&sym_target, Scope::DirectChildren),
                Err(WatchError::Backend(BackendError::DetachedParent))
            ),
            "a watch resolved through the stale symlink must be refused"
        );
    })
}

#[test]
fn detached_watch_directory_is_rejected() {
    let dir = files!({
        "a" => { "b" => { } },
    });
    let detached = dir.path().join("a/b");
    test(|_fake, watcher| {
        let session = watcher.session();
        session.watch_root(dir.path(), Scope::DirectChildren);

        let result = session.watch_directory(&detached, Scope::DirectChildren);
        assert!(
            matches!(result, Err(WatchError::Backend(BackendError::DetachedParent))),
            "a directory watch detached from every root must be refused, got {result:?}"
        );
    })
}

#[allow(dead_code)]
fn rescan_delievered_on_each_registered_root() {
    // enable when session::handle_event is fixed
    let dir = files!({
        "dir1" => {},
        "dir2" => {
            "dir3" => {},
        },
    });
    let dir_path = dir.path();
    test(|fake, watcher| {
        let mut session = watcher.session();
        let root1 = dir_path.join("dir1");
        let root2 = dir_path.join("dir2");
        let root3 = dir_path.join("dir2/dir3");
        session.watch_root(&root1, Scope::DirectChildren);
        session.watch_root(&root2, Scope::DirectChildren);
        session.watch_root(&root3, Scope::DirectChildren);

        let root = dir_path.prefixes().next().unwrap();
        fake.emulate_event(root, BackendEvent::Overflow);
        let evt1 = expect_event!(&mut session);
        let evt2 = expect_event!(&mut session);
        let evt3 = expect_event!(&mut session);

        let expected = vec![
            Event::Rescan { path: root1 },
            Event::Rescan { path: root2 },
            Event::Rescan { path: root3 },
        ]
        .into_iter()
        .collect::<HashSet<_>>();
        let actual = vec![evt1, evt2, evt3].into_iter().collect::<HashSet<_>>();
        assert_eq_pretty!(expected, actual);
    })
}
