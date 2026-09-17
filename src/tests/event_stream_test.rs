use super::test;
use futures::{FutureExt, StreamExt};
use crate::test_helpers::*;
use tracing::debug;

use crate::{backend::*, *};

fn get_stream_event(stream: &mut crate::EventStream<'_>) -> Option<Event> {
    stream.next().now_or_never().map(|it| it.unwrap())
}

#[test]
fn watch_one_directory() {
    let dir = files!({ "root" => {} });
    let root_path = dir.path().join("root");
    test(|fake, watcher| {
        let mut stream = watcher.watch_one(&root_path);
        debug!("create a child");
        dir.create("root/child");
        fake.emulate_event(
            &root_path.join("child"),
            BackendEvent::RecentlyCreated {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Dirty {
                path: root_path.join("child"),
                file_type: FileType::Regular,
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("emulate root removed");
        dir.delete("root");
        fake.emulate_event(&root_path, BackendEvent::Removed);
        assert_eq_pretty!(
            Event::Removed {
                path: root_path.to_path_buf()
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("emulate it was created again, expect a rescan");
        dir.create_dir("root");
        fake.emulate_event(
            &root_path,
            BackendEvent::RecentlyCreated {
                file_type: FileType::Directory,
            },
        );
        assert_eq_pretty!(
            Event::Rescan {
                path: root_path.to_path_buf()
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("we should still be able to receive events about children");
        dir.create("root/child");
        fake.emulate_event(
            &root_path.join("child"),
            BackendEvent::RecentlyCreated {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Dirty {
                path: root_path.join("child"),
                file_type: FileType::Regular,
            },
            get_stream_event(&mut stream).unwrap()
        );
        assert_eq_pretty!(None, get_stream_event(&mut stream))
    });
}

#[test]
fn watch_one_file() {
    let dir = files!({ "file" => "content" });
    let file_path = dir.path().join("file");
    test(|fake, watcher| {
        let mut stream = watcher.watch_one(&file_path);
        debug!("emulate file changed");
        fake.emulate_event(
            &file_path,
            BackendEvent::Changed {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Dirty {
                file_type: FileType::Regular,
                path: file_path.to_path_buf(),
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("emulate file removed");
        delete(&file_path);
        fake.emulate_event(&file_path, BackendEvent::Removed);
        assert_eq_pretty!(
            Event::Removed {
                path: file_path.to_path_buf()
            },
            get_stream_event(&mut stream).unwrap()
        );

        debug!("emulate it was created again, expect a rescan");
        create(&file_path);
        fake.emulate_event(
            &file_path,
            BackendEvent::RecentlyCreated {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Rescan {
                path: file_path.to_path_buf()
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("we should still be able to receive events");
        fake.emulate_event(
            &file_path,
            BackendEvent::Changed {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Dirty {
                path: file_path,
                file_type: FileType::Regular,
            },
            get_stream_event(&mut stream).unwrap()
        );
        assert_eq_pretty!(None, get_stream_event(&mut stream))
    });
}

#[test]
fn watch_one_symlink() {
    let dir = files!({ "dir" => { "file" => "content" } });
    let symlinked_file_path = dir.path().join("symlink");
    let file_path = dir.path().join("dir/file");
    symlink(&file_path, &symlinked_file_path);
    test(|fake, watcher| {
        let mut stream = watcher.watch_one(&symlinked_file_path);
        debug!("emulate file changed");
        fake.emulate_event(
            &file_path,
            BackendEvent::Changed {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Dirty {
                file_type: FileType::Regular,
                path: symlinked_file_path.to_path_buf(),
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("emulate file removed");
        delete(&file_path);
        fake.emulate_event(&file_path, BackendEvent::Removed);
        assert_eq_pretty!(
            Event::Removed {
                path: symlinked_file_path.to_path_buf()
            },
            get_stream_event(&mut stream).unwrap()
        );

        debug!("emulate it was created again, expect a rescan");
        create(&file_path);
        fake.emulate_event(
            &file_path,
            BackendEvent::RecentlyCreated {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Rescan {
                path: symlinked_file_path.to_path_buf()
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("we should still be able to receive events");
        fake.emulate_event(
            &file_path,
            BackendEvent::Changed {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Dirty {
                path: symlinked_file_path,
                file_type: FileType::Regular,
            },
            get_stream_event(&mut stream).unwrap()
        );
        assert_eq_pretty!(None, get_stream_event(&mut stream))
    });
}

#[test]
fn watch_recursively() {
    let dir = files!({ "root" => { "dir" => {}} });
    let root_path = dir.path().join("root");
    test(|fake, watcher| {
        let mut stream = watcher.watch_recursively(&root_path);
        debug!("create a descendant");
        dir.create("root/dir/child");
        fake.emulate_event(
            &root_path.join("dir/child"),
            BackendEvent::RecentlyCreated {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Dirty {
                path: root_path.join("dir/child"),
                file_type: FileType::Regular,
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("emulate root removed");
        dir.delete("root");
        fake.emulate_event(&root_path, BackendEvent::Removed);
        assert_eq_pretty!(
            Event::Removed {
                path: root_path.to_path_buf()
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("emulate it was created again, expect a rescan");
        dir.create("root/dir/child");
        fake.emulate_event(
            &root_path,
            BackendEvent::Changed {
                file_type: FileType::Directory,
            },
        );
        assert_eq_pretty!(
            Event::Rescan {
                path: root_path.to_path_buf()
            },
            get_stream_event(&mut stream).unwrap()
        );
        debug!("we should still be able to receive events about children");
        fake.emulate_event(
            &root_path.join("dir/child"),
            BackendEvent::Changed {
                file_type: FileType::Regular,
            },
        );
        assert_eq_pretty!(
            Event::Dirty {
                path: root_path.join("dir/child"),
                file_type: FileType::Regular,
            },
            get_stream_event(&mut stream).unwrap()
        );
        assert_eq_pretty!(None, get_stream_event(&mut stream))
    });
}
