# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/) (0.x: the CLI may still change).

## [Unreleased]

### Added

- Copied untracked files that recorded the source's absolute path are relocated
  to the new worktree: whole-path occurrences in text files up to 1 MiB and
  absolute symlinks into the source. A copied Python venv used to run the
  source's pip and install into the source's environment. Binaries are reported,
  never edited. `--no-relocate` turns this off.
- `.parhelignore` (gitignore syntax) keeps untracked paths out of the copy.

### Fixed

- Rollback and `doctor --fix` no longer run `git worktree prune`, which also
  removed the registrations of unrelated worktrees whose directories were
  unreachable (for example on an unmounted disk). Only the run's own
  registration is removed.
- `doctor --fix` detects a worktree that was already published even when the
  crashed run's journal lacks the destination or was lost, and repairs it instead
  of removing its registration and branch.
- The journal is written with `F_FULLFSYNC`, so it reaches the disk before the
  step it describes.
- `doctor --fix` finds the staging dir of a run that crashed after publishing to
  a destination in another folder, instead of removing that worktree's
  registration.
- Copied `.git` files that linked a folder to another repository's git data are
  removed from the copy, so the copy cannot change that repository's state.
- Directories parhel creates (the worktree root, ancestors of excluded paths)
  take the source's permissions instead of the defaults.
- `--allow-dirty --submodules=reinit` refuses dirty submodules
  (`dirty-submodule`) instead of silently dropping their changes.
- `git worktree add` is retried on the transient error git reports when another
  `worktree add` registers at the same moment.

### Added

- Visual identity in ASCII ("sun dogs": the source worktree in the middle, its
  copies on each side). README hero, a banner at the top of `git parhel --help`
  (not `-h`), the `o(O)o` mark in release titles and a social preview image.
  Sources live in `assets/`; the README shows the logo as an image with light and
  dark variants.

## [0.1.1] - 2026-10-22

### Changed

- Descriptions (README, `--help`, man pages, crate and formula metadata) describe
  filesystem copy-on-write in general; APFS is the first supported backend.
  A new "Platform support" section in the README states that only macOS/APFS
  works today, with Linux and Windows planned.

### Added

- Published on crates.io: `cargo install parhel`.

### Fixed

- Homebrew formula passes `brew audit --strict` (the 0.1.0 tap update failed on it).

## [0.1.0] - 2026-10-22

First public release.

### Added

- `git parhel add`: create a worktree by cloning the source with APFS
  copy-on-write (`clonefile`), ignored build output included. Supports
  `--new-branch`, `--branch`, `--detach`, a `[ref]` different from the source
  HEAD (two-way checkout of the differing files only), `--from`, `--allow-dirty`,
  `--submodules=reinit` and `--fallback`.
- `git parhel doctor [--fix]`: detect non-APFS sources, leftovers of crashed runs,
  live runs and foreign `.git` files; repair or undo crashed runs from their journal.
- Transactional `add`: refusals before any mutation, staging directory,
  write-ahead journal, exclusive atomic publish, rollback on error, panic and
  SIGINT/SIGTERM/SIGHUP.
- Man pages (`git parhel --help`), Homebrew tap and install script.

### Fixed (compared to git-worktree-apfs)

- Concurrent runs can no longer delete an existing worktree at the destination.
- Submodules no longer keep pointing at the source worktree.
- Failed runs no longer leave registrations that keep branches "in use".

[Unreleased]: https://github.com/myrk-kale/parhel/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/myrk-kale/parhel/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/myrk-kale/parhel/releases/tag/v0.1.0
