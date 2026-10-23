# Contributing

Thanks for helping! parhel is small and test-driven; changes follow the same path.

## Setup

macOS on APFS or Linux, Rust (the version in `rust-toolchain.toml` is installed
automatically), git 2.29+.

```sh
cargo test --features failpoints   # everything; takes a few minutes
cargo test --lib                   # fast unit tests only
```

Integration tests create throwaway disk images with `hdiutil` (no root) and run
real git in hermetic sandboxes (their own `HOME` and global config).

To run the suite on Linux from a Mac, use `scripts/linux-test.sh` (Docker, with
btrfs, XFS and ext4 loop mounts; extra arguments go to `cargo test`).

## Workflow

1. Open an issue first for behavior changes, so we can agree on the expected outcome.
2. Write the test first and watch it fail for the right reason. Every behavior
   has an ID in [docs/test-catalog.md](docs/test-catalog.md); add yours there.
3. Implement until the suite is green, then run:
   ```sh
   cargo fmt
   cargo clippy --all-targets --all-features -- -D warnings
   ```
4. If you changed the CLI, regenerate the man pages:
   `PARHEL_UPDATE_MAN=1 cargo test --test man`.
5. Add a line under `[Unreleased]` in `CHANGELOG.md`.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/).

## Layout

- `src/domain`: pure rules (paths, exclusions, clone plan, errors)
- `src/ports.rs`: traits for side effects; `src/app/fakes.rs` implements them for unit tests
- `src/git`, `src/fs`: adapters (the `git` binary; APFS and Linux reflink syscalls)
- `src/app`: the `add` transaction, rollback and `doctor`
- `tests/`: integration tests; `tests/common` is the harness

## Releasing (maintainers)

Bump `version` in `Cargo.toml`, move `[Unreleased]` to the new version in
`CHANGELOG.md`, commit, then `git tag vX.Y.Z && git push --tags`. The release
workflow builds the universal binary, publishes the GitHub release and updates
the Homebrew tap.
