//! Test helpers shared by the unit tests and the integration tests.
//!
//! The integration test includes this module by path, so each compilation unit uses a different subset.
#![allow(dead_code, unused_imports, unused_macros)]

mod assert_pretty;
pub mod dir_fixture;
mod path_macro;
pub mod tree_like;

pub(crate) use crate::{path, root};
pub(crate) use assert_pretty::{assert_eq_pretty, assert_matches};
pub(crate) use dir_fixture::files;
pub(crate) use tree_like::tree_like;

use anyhow::Context;
use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Once;
use std::time::Duration;
use tokio::runtime;
use tracing::{debug, Instrument};

pub const DEFAULT_TEST_TIMEOUT: Duration = Duration::from_secs(3);

pub struct TestOptions {
    timeout: Duration,
    rt: tokio::runtime::Builder,
}

impl TestOptions {
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = duration;
        self
    }
}

impl Default for TestOptions {
    fn default() -> Self {
        let mut rt_builder = runtime::Builder::new_multi_thread();
        rt_builder.enable_all();
        TestOptions {
            rt: rt_builder,
            timeout: DEFAULT_TEST_TIMEOUT,
        }
    }
}

pub fn test<T, Fn, Fut>(options: TestOptions, f: Fn) -> T
where
    Fn: FnOnce(runtime::Handle) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let TestOptions { timeout, mut rt } = options;
    enable_logging();

    let test_name = std::thread::current().name().unwrap_or("test").to_owned();
    let span = tracing::info_span!("test_with_custom_timeout", name = test_name);

    let rt = rt.build().expect("Failed to create Tokio runtime");
    let t = std::time::Instant::now();
    println!("TEST START {test_name}");

    // The timeout is enforced from this thread, outside of the runtime under test: a test that
    // blocks every worker thread also stops the tokio time driver, so a `tokio::time::timeout`
    // inside `block_on` would never fire. `recv_timeout` relies on an OS deadline instead.
    let (tx, rx) = mpsc::sync_channel(1);
    let rt_thread = std::thread::Builder::new()
        .name(format!("{test_name}-rt"))
        .spawn(move || {
            let result = rt.block_on(
                async move {
                    tokio::task::spawn(
                        async move {
                            let fut = f(runtime::Handle::current());
                            fut.await
                        }
                        .in_current_span(),
                    )
                    .await
                }
                .instrument(span),
            );
            // Hand the result over before tearing the runtime down, so that shutdown is not
            // charged against the test timeout. Send fails only if we already timed out.
            let _ = tx.send(result);
            rt.shutdown_timeout(Duration::from_millis(100));
        })
        .expect("Failed to spawn the test runtime thread");

    let result = rx.recv_timeout(timeout);
    match result.as_ref() {
        Ok(Ok(_)) => {
            println!("TEST END {test_name}: SUCCESS");
        }
        Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            println!("TEST END {test_name}: FAILURE");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            println!("TEST END {test_name}: TIMEOUT");
        }
    }

    match result {
        Ok(outcome) => {
            // The runtime is done, wait for its shutdown so the next test starts from a clean state.
            rt_thread.join().expect("Test runtime thread panicked");
            match outcome {
                Ok(v) => v,
                Err(panic) => std::panic::resume_unwind(panic.into_panic()),
            }
        }
        // The runtime thread is deliberately left running: it is stuck in the test body and cannot
        // be joined or dropped. It dies with the process.
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("Test timed out after {:?} ({:?} total)", timeout, t.elapsed());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("Test runtime thread terminated without producing a result");
        }
    }
}

pub fn test_with_custom_timeout<T, Fn, Fut>(timeout: Duration, f: Fn) -> T
where
    Fn: FnOnce(runtime::Handle) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    test(TestOptions::default().timeout(timeout), f)
}

pub fn test_with_timeout<T, Fn, Fut>(f: Fn) -> T
where
    Fn: FnOnce(runtime::Handle) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    test_with_custom_timeout(DEFAULT_TEST_TIMEOUT, f)
}

pub fn enable_logging() {
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .pretty()
            .init();
    });
}

pub fn touch(path: impl AsRef<Path>) {
    let path = path.as_ref();
    debug!(path = %path.display(), "touch");
    if path.exists() {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()))
            .with_context(|| format!("path: {}", path.display()))
            .expect("failed to touch");
    } else {
        create(path)
    }
}

#[cfg(unix)]
pub fn set_permissions(path: impl AsRef<Path>, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    let path = path.as_ref();
    debug!(path = %path.display(), %mode, "set_permissions");
    let f = fs::File::open(path).with_context(|| format!("path: {}", path.display())).unwrap();
    let metadata = f.metadata().with_context(|| format!("path: {}", path.display())).unwrap();
    let mut permissions = metadata.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("path: {}", path.display()))
        .unwrap();
}

pub fn create(path: impl AsRef<Path>) {
    let path = path.as_ref();
    debug!(path = %path.display(), "create_file");
    fs::create_dir_all(path.parent().unwrap())
        .with_context(|| format!("path: {}", path.display()))
        .unwrap();
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("path: {}", path.display()))
        .unwrap();
}

pub fn delete(path: impl AsRef<Path>) {
    let path = path.as_ref();
    debug!(path = %path.display(), "delete");
    let file_type = fs::symlink_metadata(path)
        .with_context(|| format!("failed to delete path: {}", path.display()))
        .unwrap()
        .file_type();
    if file_type.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to delete path: {}", path.display()))
            .unwrap();
    } else {
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileTypeExt;
            if file_type.is_symlink_dir() {
                fs::remove_dir(path)
                    .with_context(|| format!("failed to delete path: {}", path.display()))
                    .unwrap();
                return;
            }
        }
        fs::remove_file(path)
            .with_context(|| format!("failed to delete path: {}", path.display()))
            .unwrap();
    }
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) {
    let from = from.as_ref();
    let to = to.as_ref();
    debug!(?from, ?to, "rename");
    fs::rename(from, to).unwrap()
}

pub fn write_all(path: impl AsRef<Path>, new_content: impl Into<Vec<u8>>) {
    let path = path.as_ref();
    debug!(path = %path.display(), "write");
    OpenOptions::new()
        .create_new(false)
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("path: {}", path.display()))
        .unwrap()
        .write_all(new_content.into().as_slice())
        .unwrap();
}

pub fn read(path: impl AsRef<Path>) -> Vec<u8> {
    let path = path.as_ref();
    fs::read(path).with_context(|| format!("path: {}", path.display())).unwrap()
}

pub fn create_dir(path: impl AsRef<Path>) {
    let path = path.as_ref();
    debug!(?path, "create_dir");
    fs::create_dir_all(path)
        .with_context(|| format!("path: {}", path.display()))
        .unwrap()
}

pub fn run(program: impl AsRef<OsStr>, args: Vec<impl AsRef<OsStr>>) {
    let program = program.as_ref();
    let args: Vec<&OsStr> = args.iter().map(|i| i.as_ref()).collect();
    let output = std::process::Command::new(program)
        .args(&args)
        .output()
        .with_context(|| format!("Failed to execute command {:?} {:?}", program, args))
        .unwrap();
    let exit_code = output.status.code();
    let output = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        exit_code,
        Some(0),
        "{:?} {:?} exited with status code {:?}: {}",
        program,
        args,
        exit_code,
        output
    );
}

#[cfg(unix)]
pub fn symlink(original: impl AsRef<Path>, link: impl AsRef<Path>) {
    debug!(original = %original.as_ref().display(), link = %link.as_ref().display(), "symlink");
    std::os::unix::fs::symlink(original, link).expect("failed to create a symlink");
}

#[cfg(windows)]
pub fn symlink(original: impl AsRef<Path>, link: impl AsRef<Path>) {
    // Forward slashes cause errors on subsequent reads of such symlinks, reconstruct the path to
    // make sure it has platform-native delimiters.
    let original = original.as_ref().components().collect::<PathBuf>();
    let source_is_dir = original.is_dir();

    if source_is_dir {
        debug!(original = %original.display(), link = %link.as_ref().display(), source_is_dir, "symlink");
        std::os::windows::fs::symlink_dir(original, link).expect("failed to create a symlink");
    } else {
        debug!(original = %original.display(), link = %link.as_ref().display(), source_is_dir, "symlink");
        std::os::windows::fs::symlink_file(original, link).expect("failed to create a symlink");
    }
}
