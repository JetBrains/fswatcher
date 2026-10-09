# fswatcher

[![internal JetBrains project](https://jb.gg/badges/internal-plastic.svg)](https://confluence.jetbrains.com/display/ALL/JetBrains+on+GitHub) [![CI](https://github.com/JetBrains/fswatcher/actions/workflows/ci.yml/badge.svg)](https://github.com/JetBrains/fswatcher/actions/workflows/ci.yml) 

A cross-platform file system watcher (FSEvents/kqueue on macOS, inotify on Linux,
`ReadDirectoryChangesW` on Windows) with symlink canonicalization.

See [doc/Overview.md](doc/Overview.md) for the design, and the `doc/` directory for
notes on each platform backend.

## Usage

```toml
[dependencies]
jetbrains-fswatcher = "1.0"
futures = "0.3"
```

```rust
use futures::StreamExt;
use jetbrains_fswatcher::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let watcher = Watcher::create_default()?;
    let mut events = watcher.watch_recursively("/some/directory");

    while let Some(event) = events.next().await {
        match event {
            Event::Dirty { path, file_type } => println!("changed: {} ({file_type:?})", path.display()),
            Event::Removed { path } => println!("gone: {}", path.display()),
            Event::Rescan { path } => println!("rescan: {}", path.display()),
        }
    }

    drop(events);
    watcher.shutdown_and_join()
}
```

One `Watcher` holds the OS resources and can be shared; each subscription is a stream
of events for one path, registered as soon as it is created:

- `watch_one(path)` — the path and, for a directory, its direct children.
- `watch_recursively(path)` — the whole subtree, not following nested symlinks.
- `watch_immediate(path)` — one regular file, seeing writes while the writer keeps it
  open (macOS and Windows report none until the handle is closed).

The path need not exist yet. `Rescan` means the change could not be expressed as events
(OS buffer overflow, a slow client, a retargeted symlink) — re-read that subtree.

For finer control use `watcher.session()`; see `examples/recursive_walk.rs`.

## Releasing

Push a semver tag from a commit on `main` (or run the *Publish release* workflow
manually with the tag name):

```sh
git tag 1.0.1 && git push origin 1.0.1
```

The workflow runs CI, publishes the crate to crates.io with the tag as its version
(the `version` in `Cargo.toml` is only a placeholder), then publishes the GitHub release.

## License
```
   Copyright 2026 JetBrains s.r.o.

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
```
