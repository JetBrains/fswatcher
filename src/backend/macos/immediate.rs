use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures::{stream::BoxStream, StreamExt};

use nix::errno::Errno;

use super::kqueue::{Flags, KEventId, KQueueWorker};
use crate::backend::immediate::{Changed, ImmediateError, ImmediateWatcher};

pub struct KQueueImmediateWatcher {
    worker: KQueueWorker,
    watches: Arc<Mutex<HashMap<KEventId, WatchContext>>>,
}

struct WatchContext {
    callback: Box<dyn FnMut(Flags) + Send>,
}

impl KQueueImmediateWatcher {
    pub fn create() -> anyhow::Result<Self> {
        let watches = Arc::new(Mutex::new(HashMap::<KEventId, WatchContext>::new()));
        let worker = KQueueWorker::create({
            let watches = watches.clone();
            move |event| {
                // If the ID doesn't exist, it means the watch has already been dropped.
                if let Some(context) = watches.lock().expect("mutex is poisoned").get_mut(&event.id) {
                    (context.callback)(event.flags)
                }
            }
        })?;
        Ok(Self { worker, watches })
    }
}

impl ImmediateWatcher for KQueueImmediateWatcher {
    fn watch_immediate_changes(&self, canonical_path: PathBuf) -> Result<BoxStream<'static, Changed>, ImmediateError> {
        let mut guard = self.watches.lock().expect("mutex is poisoned");
        let watch = self.worker.add_kernel_queue_watch(&canonical_path).map_err(|errno| match errno {
            // The worker rejects non-regular files with ENOTSUP; surface it as the dedicated variant.
            Errno::ENOTSUP => ImmediateError::NotAFile,
            other => ImmediateError::IOError(std::io::Error::from_raw_os_error(other as i32)),
        })?;
        let (tx, mut rx) = futures::channel::mpsc::channel::<Changed>(1);
        guard.insert(
            watch.id(),
            WatchContext {
                callback: Box::new({
                    let mut tx = Some(tx);
                    move |note| {
                        if note.contains(Flags::NOTE_EXTEND) || note.contains(Flags::NOTE_WRITE) {
                            if let Some(tx) = tx.as_mut() {
                                let _ = tx.try_send(Changed);
                            }
                        } else if note.contains(Flags::NOTE_RENAME)
                            || note.contains(Flags::NOTE_DELETE)
                            || note.contains(Flags::NOTE_REVOKE)
                        {
                            // If the file was renamed, we treat this as if the file under the original name was removed (this is the same
                            // behaviour as with the FSEventStream).

                            // The NOTE_REVOKE flag is set if the `revoke` syscall was called on the file or if the filesystem was
                            // unmounted, which is effectively a removal of the file.
                            drop(tx.take());
                        } else {
                            // TODO warn, we didn't order it
                        }
                    }
                }),
            },
        );
        let stream = async_stream::stream! {
            while let Some(evt) = rx.next().await {
                yield evt;
            }
            drop(watch);
        };
        Ok(stream.boxed())
    }

    fn shutdown_and_join(self: Box<Self>) -> std::thread::Result<()> {
        self.worker.terminate_and_wait()
    }
}

#[cfg(test)]
mod test {
    use crate::test_helpers::{enable_logging, files};

    use super::*;

    #[test]
    fn watching_unopenable_path_returns_error_instead_of_panicking() {
        enable_logging();

        let dir = files!({ "present" => "content" });
        let watcher = KQueueImmediateWatcher::create().unwrap();

        let missing = dir.path().join("does-not-exist");
        let result = watcher.watch_immediate_changes(missing).map(|_| ());
        assert!(
            matches!(result, Err(ImmediateError::IOError(_))),
            "expected an IO error, got {result:?}",
        );

        Box::new(watcher).shutdown_and_join().unwrap();
    }

    #[test]
    fn watching_directory_reports_not_a_file() {
        enable_logging();

        let dir = files!({ "child" => "content" });
        let watcher = KQueueImmediateWatcher::create().unwrap();

        let result = watcher.watch_immediate_changes(dir.path().to_path_buf()).map(|_| ());
        assert!(
            matches!(result, Err(ImmediateError::NotAFile)),
            "expected NotAFile, got {result:?}",
        );

        Box::new(watcher).shutdown_and_join().unwrap();
    }
}
