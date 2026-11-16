//! Watches a single path and logs every event it produces.
//!
//! ```sh
//! cargo run --example watch_path -- [--recursive] <path>
//! ```
//!
//! Regular events are logged at `info`; `Rescan` events, which tell the client that the
//! subtree has to be re-read, are logged at `warn`. Set `RUST_LOG` to adjust verbosity.

use std::{env, path::PathBuf, process};

use futures::StreamExt;
use jetbrains_fswatcher::prelude::*;
use tracing::{info, warn};

const USAGE: &str = "usage: watch_path [--recursive] <path>";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let (path, recursive) = parse_args(env::args().skip(1)).unwrap_or_else(|message| {
        eprintln!("{message}\n{USAGE}");
        process::exit(2);
    });

    // The watcher accepts only absolute paths. The path does not have to exist yet.
    let path = std::path::absolute(&path)?;

    let watcher = Watcher::create_default()?;
    let mut events = if recursive {
        watcher.watch_recursively(&path)
    } else {
        watcher.watch_one(&path)
    };
    info!(path = %path.display(), recursive, "watching");

    while let Some(event) = events.next().await {
        match event {
            Event::Dirty { path, file_type } => info!(path = %path.display(), ?file_type, "dirty"),
            Event::Removed { path } => info!(path = %path.display(), "removed"),
            Event::Rescan { path } => warn!(path = %path.display(), "rescan"),
        }
    }

    drop(events);
    watcher.shutdown_and_join()
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<(PathBuf, bool), String> {
    let mut recursive = false;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--recursive" => recursive = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag: {flag}")),
            _ if path.is_some() => return Err("expected exactly one path".to_string()),
            _ => path = Some(PathBuf::from(arg)),
        }
    }
    path.map(|path| (path, recursive)).ok_or_else(|| "missing path".to_string())
}
