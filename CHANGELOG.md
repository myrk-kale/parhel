# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/) (0.x: the CLI may still change).

## [Unreleased]

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
