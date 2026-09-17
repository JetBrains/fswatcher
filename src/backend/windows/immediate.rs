use std::{os::windows::fs::MetadataExt, time::Duration};

use futures::StreamExt;
use tracing::trace;

use crate::backend::immediate::{Changed, ImmediateError, ImmediateWatcher};

/// Each watch will poll the current metadata of the file with this interval.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct TimerImmediateWatcher;

impl ImmediateWatcher for TimerImmediateWatcher {
    fn watch_immediate_changes(
        &self,
        canonical_path: std::path::PathBuf,
    ) -> Result<futures::stream::BoxStream<'static, Changed>, ImmediateError> {
        let meta = std::fs::symlink_metadata(&canonical_path)?;
        // The documentation asserts that these two are mutually exclusive anyway, so the "!meta.is_symlink()" check is redundant.
        // It is kept here because there appears to be some kind of misalignment of the actual behavior with the documentation,
        // as it was observed on several occasions that on Windows a file type could be both a symlink and a directory (using this Rust API).
        // Possibly, some kinds of symlinks aren't properly handled (Windows has several kinds of soft links).
        //
        // Whenever it is proven that this doesn't happen anymore, the extra check can be removed.
        if meta.is_file() && !meta.is_symlink() {
            trace!(?canonical_path, ?meta, "obtained initial metadata");
            let stream = async_stream::stream! {
                let mut prev_meta = meta;
                loop {
                    tokio::time::sleep(POLL_INTERVAL).await;
                    let meta = tokio::fs::symlink_metadata(&canonical_path).await;
                    trace!(?canonical_path, ?prev_meta, ?meta, "obtained file metadata");
                    match meta {
                        Ok(meta) if meta.is_file() => {
                            // `modified` never fails on Windows.
                            let different_mtime = prev_meta.modified().expect("never fails on Windows") != meta.modified().expect("never fails on Windows");
                            let different_file_size = prev_meta.file_size() != meta.file_size();
                            prev_meta = meta;
                            if different_mtime || different_file_size {
                                trace!(?canonical_path, "file has changed");

                                yield Changed;
                            } else {
                                trace!(?canonical_path, "metadata is the same");
                            }
                        }

                        // The file is not a regular file anymore.
                        Ok(_) => break,

                        // The file isn't accessible anymore. The Removed event is delivered by the primary event tracking loop.
                        Err(_) => break,
                    }
                }
            };
            Ok(stream.boxed())
        } else {
            Err(ImmediateError::NotAFile)
        }
    }

    fn shutdown_and_join(self: Box<Self>) -> std::thread::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::{fs::OpenOptions, io::Write};

    use futures::Stream;
    use crate::test_helpers::{assert_matches, delete, enable_logging, files};

    use super::*;

    const RECV_TIMEOUT: Duration = Duration::from_secs(3);

    async fn next<S: Stream + Unpin>(s: &mut S) -> Option<S::Item> {
        tokio::time::timeout(RECV_TIMEOUT, s.next()).await.expect("recv_timeout")
    }

    #[tokio::test]
    async fn write() {
        enable_logging();
        let w = TimerImmediateWatcher;

        let dir = files!({ "file" => "content" });
        let file_path = dir.path().join("file");
        let mut file_handle = OpenOptions::new().create_new(false).write(true).open(&file_path).unwrap();

        let mut subscription = w.watch_immediate_changes(file_path.to_path_buf()).unwrap();

        file_handle.write_all("new content".as_bytes()).unwrap();

        let evt = next(&mut subscription).await;
        assert_matches!(evt, Some(Changed));
    }

    #[tokio::test]
    async fn deletion() {
        enable_logging();
        let w = TimerImmediateWatcher;

        let dir = files!({ "file" => "content" });
        let file_path = dir.path().join("file");
        let mut subscription = w.watch_immediate_changes(file_path.to_path_buf()).unwrap();

        delete(file_path);

        let evt = next(&mut subscription).await;
        assert_matches!(evt, None);
    }
}
