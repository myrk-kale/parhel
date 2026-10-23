# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/) (0.x: the CLI may still change).

## [Unreleased]

### Changed

- **Breaking:** the error code and the `doctor` finding `not-apfs` are now
  `not-cow`, and their messages no longer mention APFS ("copy-on-write is not
  available on <fs> at `<path>`"). Scripts that match `not-apfs` must match
  `not-cow`. Whether a volume supports copy-on-write is now decided by the
  filesystem adapter, not by comparing the filesystem's name. The refusal now
  names `--copy` and `--fallback` as the ways out.

### Added

- Linux support. On Btrfs, XFS (with reflink), bcachefs and ZFS 2.2+, `add`
  clones each file with `ioctl(FICLONE)` and publishes with
  `renameat2(RENAME_NOREPLACE)`. Whether a directory can reflink is found by a
  probe with two anonymous temporary files, not by the filesystem's name. Linux
  binaries are not released yet; install with `cargo install parhel`.
- `--copy`: copy every file in full instead of cloning it. It works on any
  filesystem and across volumes (ext4, tmpfs, NFS, another disk), goes through
  the same staging, journal, rollback and path rewriting as a clone, and is
  reported as a `full-copy` worktree. It cannot be combined with `--fallback`.
- An agent skill (`npx skills add myrk-kale/parhel`) that teaches coding agents
  when to use parhel and the workflow that keeps copies away from shared
  services and ignored data from being lost.

## [0.2.2] - 2026-10-22

### Fixed

- `doctor --fix` could delete a legitimate worktree whose directory name only
  looked like parhel's staging (`.parhel-staging-feature`), uncommitted work
  included. Only `<staging dir>/<name>` counts as parhel's now, and without a
  journal a registered worktree's files are never deleted: the registration is
  dropped only once its directory is gone.
- Relocating into a path with spaces broke scripts whose shebang named the
  interpreter (for example a virtualenv's `pip`): the kernel splits a shebang
  at whitespace. Such shebangs are now written as `#!/usr/bin/env -S "<path>"`
  with the path escaped.

## [0.2.1] - 2026-10-22

### Added

- Release archives carry a signed build provenance attestation
  (`gh attestation verify`).
- CI runs the whole suite against git 2.29 and 2.39 built from source.
- README "Security and privacy" section.

### Fixed

- The declared minimum git version (2.29) was not true: parhel used
  `rev-parse --path-format=absolute` (git 2.31) and `rev-parse
  --end-of-options` (git 2.30). It no longer does.
- `doctor --fix` trusted any journal it found, so a forged one could make it
  delete an arbitrary directory or branch. Journals are now validated against
  the staging folder they sit in, the repository and the disk.
- Branches are deleted with a compare-and-delete (`git update-ref -d`), which
  also stops git 2.39 from creating `.git/packed-refs` during a rollback.
- Relocation no longer rewrites references into paths the copy does not contain
  (the source's `.git`, other worktrees, submodules, `..`), which broke for
  example a nested `git clone --shared`.
- A copied gitfile pointing at a git dir copied along (`--separate-git-dir`)
  now follows the copy, instead of letting the copy move the source's nested
  repository.
- A destination whose name starts with `-` is refused when git would use it as a
  branch name.
- A branch created by another process between parhel's check and git's is never
  deleted by the rollback.
- Submodule reinit never writes the repository's shared configuration.

## [0.2.0] - 2026-10-22

### Added

- Copied untracked files that recorded the source's absolute path are relocated
  to the new worktree: whole-path occurrences in text files up to 1 MiB and
  absolute symlinks into the source. A copied Python venv used to run the
  source's pip and install into the source's environment. Binaries are reported,
  never edited. `--no-relocate` turns this off.
- `.parhelignore` (gitignore syntax) keeps untracked paths out of the copy.


- Visual identity in ASCII ("sun dogs": the source worktree in the middle, its
  copies on each side). README hero, a banner at the top of `git parhel --help`
  (not `-h`), the `o(O)o` mark in release titles and a social preview image.
  Sources live in `assets/`; the README shows the logo as an image with light and
  dark variants.

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
- parhel no longer runs `git worktree repair`, which also rewrote the `.git`
  files of other worktrees with broken links. It updates only its own
  registration.
- Every git command runs with `GIT_OPTIONAL_LOCKS=0`: a refused `add` used to
  let `git status` rewrite the source's index.
- `doctor --fix` deletes a crashed run's branch only if it still points at the
  commit it was created on, and never deletes a `.parhel-staging-*` folder that
  holds files other than parhel's journal.
- Submodule reinit no longer rewrites `.git/config` with values it already has.
- A `git worktree add -b` that failed after git had already created the branch
  (racing another `worktree add`) left that branch behind: the branch is now
  journaled before git is asked to create it.

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

[Unreleased]: https://github.com/myrk-kale/parhel/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/myrk-kale/parhel/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/myrk-kale/parhel/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/myrk-kale/parhel/releases/tag/v0.1.0
