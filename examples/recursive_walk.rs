use std::{
    env,
    path::{Path, PathBuf},
};

use tracing::{debug, info};
use watch::*;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .pretty()
        .init();

    let target_paths = env::args().skip(1).map(PathBuf::from).collect::<Vec<_>>();

    println!("Watching {:?}", &target_paths);

    let watcher = Watcher::create_default().expect("failed to create watcher");
    let mut session = watcher.session();

    let mut on_file = |file_path: &Path| debug!("file: {:?}", file_path);
    let on_removed = |path: &Path| debug!("reset: {:?}", path);

    let roots: Vec<PathBuf> = target_paths
        .into_iter()
        .map(|path| {
            session.watch_root(&path, Scope::DirectChildren);
            let t0 = std::time::Instant::now();
            walk_recursively(&mut session, &path, &mut on_file);
            println!("walk {:?}, elapsed {:?}", path, t0.elapsed());
            path
        })
        .collect();

    loop {
        let event = session.next_event_blocking();
        info!("event: {:#?}", event);
        match event {
            Ok(Event::Dirty { path, file_type, .. }) => {
                if file_type.is_symlink() {
                    session.watch_symlink(&path, Scope::DirectChildren).expect("illegal argument");
                    walk_recursively(&mut session, &path, &mut on_file);
                } else if file_type.is_directory() {
                    walk_recursively(&mut session, &path, &mut on_file);
                } else if file_type.is_regular() {
                    on_file(&path)
                }
            }
            Ok(Event::Removed { path, .. }) => on_removed(&path),
            Ok(Event::Rescan { path }) => {
                on_removed(&path);
                walk_recursively(&mut session, &path, &mut on_file);
            }
            Err(_overflow) => {
                for path in roots.iter() {
                    on_removed(path);
                    walk_recursively(&mut session, path, &mut on_file);
                }
            }
        }
    }
}

fn walk_recursively(s: &mut WatchSession, path: &Path, on_file: &mut dyn FnMut(&Path)) {
    if s.watch_directory(path, Scope::DirectChildren).is_ok() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let file_type = entry.file_type().unwrap();
                    let path = entry.path();
                    if file_type.is_symlink() {
                        if s.watch_symlink(&path, Scope::DirectChildren).is_ok() {
                            walk_recursively(s, &path, on_file);
                        }
                    } else if file_type.is_dir() {
                        if s.watch_directory(&path, Scope::DirectChildren).is_ok() {
                            walk_recursively(s, &path, on_file)
                        }
                    } else if file_type.is_file() {
                        on_file(&path);
                    }
                }
            }
        }
    }
}
