# Upgrading the dependencies

## cargo upgrade
`cargo-edit` comes with `cargo upgrade` command that simplifies the update process.
```
cargo install cargo-edit --locked
```
### --dry-run
`--dry-run` prints the table of what would change and writes nothing:
```
# from the repository root
cargo upgrade --dry-run --incompatible
```

```
name    old req compatible latest new req
nix     0.29    0.29.0     0.31.0 0.31
tokio   1.37    1.53.1     1.53.1 1.53
windows 0.58.0  0.58.0     0.62.2 0.62.2
```
### `--incompatible`

By default `cargo upgrade` only raises a requirement to the newest release that is semver-compatible with it.

`--incompatible` also proposes the requirements whose newest release crosses a semver boundary, which by default are left alone.

So run it with `--incompatible` to see the whole picture, but treat the two halves differently. The compatible half is usually a single commit. Each entry of the incompatible half is an API break that can need source changes, and belongs in a commit of its own:

```
cargo upgrade                          # the compatible half
cargo upgrade --incompatible -p nix    # one incompatible entry
```

The target-specific dependencies (`inotify`, `windows`, `nix`, `core-foundation`, ...) are listed
by `cargo upgrade` like any other, whichever platform it runs on.
## build and test
```
cargo build --locked --all-targets
cargo test --locked
```

That covers only the backend of the machine it runs on. Each backend is a separate implementation
behind a `cfg(target_os)` gate, with its own target-specific dependencies: an upgrade of `windows`
can break the Windows backend while the build and the tests on macOS stay green. Type-check the
other platforms too:

```
rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-msvc
cargo check --locked --all-targets --target x86_64-unknown-linux-gnu
cargo check --locked --all-targets --target x86_64-pc-windows-msvc
```

`cargo check` does not link, so it needs no cross toolchain. It does not run anything either: the
tests of the other platforms run in CI, which builds and tests on Linux, macOS and Windows.
## `cargo update`

Run `cargo update` after `cargo upgrade`, as a commit of its own. `cargo upgrade` writes
`Cargo.lock` too, but only as far as its own edits reach: it re-locks the crates whose requirement
it just raised, and leaves the rest of the lock alone.
