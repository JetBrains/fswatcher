# watch

[![CI](https://github.com/JetBrains/watch/actions/workflows/ci.yml/badge.svg)](https://github.com/JetBrains/watch/actions/workflows/ci.yml)

A cross-platform file system watcher (FSEvents/kqueue on macOS, inotify on Linux,
`ReadDirectoryChangesW` on Windows) with symlink canonicalization.

See [doc/Overview.md](doc/Overview.md) for the design, and the `doc/` directory for
notes on each platform backend.
