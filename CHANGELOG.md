# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/) (0.x: the CLI may still change).

## [Unreleased]

### Added

- Linux packages: a `.deb` (`parhel_<version>-1_amd64.deb`, `parhel_<version>-1_arm64.deb`)
  and an `.rpm` (`parhel-<version>-1.x86_64.rpm`, `parhel-<version>-1.aarch64.rpm`)
  for the static musl builds. They install `/usr/bin/git-parhel` and the man
  pages, require `git` and recommend `btrfs-progs`. Each has a `.sha256` file and a
  build provenance attestation, like the archives. CI installs them in Ubuntu,
  Debian and Fedora containers, runs the smoke test and lints the `.deb` with
  lintian. They are not GPG-signed, and there is no apt or dnf repository yet.
- **The store: copy-on-write worktrees for sources without reflink (Linux).** When
  `add` finds `not-cow` or `cross-volume` and the user has a store (a Btrfs
  image mounted at `/parhel/<user>`, or the directory named by `PARHEL_STORE`),
  it mirrors the source into a per-repository baseline inside the store (only what
  changed, with modes, extended attributes, POSIX ACLs and nanosecond times),
  clones the worktree from it with reflinks and leaves a symlink at the path you
  asked for. git registers the worktree at its real path in the store, and so does
  relocation. No flag: it is automatic; `--copy` and `--fallback` bypass it. A
  configured store that cannot be used is the new error `store-unavailable`, never
  a silent fallback; where there is no store, `not-cow` also suggests
  `sudo git parhel setup`. Two runs on one repository take turns on the baseline,
  a first sync over 1 GiB prints a notice, and a baseline whose sync died without
  finishing is mirrored again instead of trusted. `git worktree remove <symlink>`
  removes the real worktree and leaves the symlink dangling.
- `doctor` checks the store: `store-unavailable`, `store-image-deleted`,
  `store-low-space`, `store-orphan`, `store-dangling-link` and
  `store-stale-baseline`; `--fix` removes dangling symlinks and stale baselines
  and never a worktree. A source on ext4 is no longer reported as `not-cow` while
  the store can serve it, and the journals of crashed store runs are repaired or
  rolled back, trusting only paths inside the store.
- **`git parhel setup` (Linux, run with sudo) creates the store.** One command makes
  everything the store needs: `/var/lib/parhel` (root, 0700), a sparse
  `/var/lib/parhel/<user>.img` (`--size`, default 100G, minimum 256M; `--preallocate`
  reserves it) formatted as Btrfs, a systemd mount unit (`parhel-<user>.mount`,
  `loop,discard,noatime`, wanted by `local-fs.target`, so the store is back after a
  reboot) mounted at `/parhel/<user>`, and inside it the user's directories and the
  `.parhel-store` marker. The store belongs to `$SUDO_USER` or to `--user`; root is
  never inferred. Re-running is safe: a healthy store is reported, a stopped one is
  started, a lost protection is added back, an image that holds a filesystem is
  never formatted, and a unit, marker or image that is not this store's is refused
  untouched.
- **The images are protected from deletion.** `setup` sets `chattr +i` on
  `/var/lib/parhel` (the directory, so the image stays writable for the loop
  device) and checks it: removing and renaming the image fail with `EPERM` even for
  root. When `chattr` is missing or the filesystem does not honor the attribute, the
  store still works and the command fails naming the image that is not protected.
- **`git parhel setup --remove`** refuses with `store-in-use` (listing them) while
  worktrees live in the store, and otherwise stops the unit, deletes it, the image
  and the mount point, and protects the directory again if other users' images
  remain. Nothing is removed that is not parhel's.
- New error codes `store-unavailable`, `store-in-use`, `not-root`, `missing-tool`
  and `unsupported-platform` (`setup` off Linux, or on a Linux without systemd or
  btrfs).
- The agent skill tells the agent to stop on `not-cow` and `store-unavailable`, ask
  the user to run the one-time `sudo git parhel setup`, and never run it or `sudo`
  itself; `--copy` and `--fallback` only when the user chooses them after hearing
  about it. It also teaches the store route: the symlink and
  `git worktree remove` followed by `git parhel doctor --fix`.

### Changed

- `remove_dir_all` (rollback, `doctor`) now also removes a tree inside a
  read-only parent directory, which a mirror of a source with read-only
  directories needs.

## [0.3.0] - 2026-10-23

### Changed

- **Breaking:** the error code and the `doctor` finding `not-apfs` are now
  `not-cow`, and their messages no longer mention APFS ("copy-on-write is not
  available on <fs> at `<path>`"). Scripts that match `not-apfs` must match
  `not-cow`. Whether a volume supports copy-on-write is now decided by the
  filesystem adapter, not by comparing the filesystem's name. The refusal now
  names `--copy` and `--fallback` as the ways out.

### Fixed

- Relocation failed with `permission-denied` on a symlink to rewrite or a foreign
  `.git` file to detach inside a read-only directory. The directory is now
  unlocked around the change and gets its mode back.
- A read-only source directory (for example mode 0555) that contained a nested
  worktree, a submodule or a `.parhelignore`d path made `add` fail with
  `Permission denied` while its other entries were cloned or copied: parhel
  applied the directory's mode before filling it. The modes of directories parhel
  creates are now applied after their contents, deepest first.

### Added

- Linux support. On Btrfs and XFS (with reflink), `add` clones each file with
  `ioctl(FICLONE)` and publishes with `renameat2(RENAME_NOREPLACE)`. Whether a
  directory can reflink is found by a probe with two anonymous temporary files,
  not by the filesystem's name, in the destination's directory when both are on
  one filesystem. bcachefs and ZFS 2.2+ should work and are not tested. Btrfs
  subvolumes of one filesystem are not treated as different volumes, and a NOCOW
  file (`chattr +C`) fails with a message that names the file and offers `--copy`
  or `.parhelignore`.
- ACLs travel with the files: POSIX ACLs on Linux, extended ACLs (`chmod +a`) on
  macOS, for `--copy` and for the entries a macOS clone makes itself
  (`clonefile(2)` drops the ACLs inside the directories it clones). Entries do
  not keep the ACLs they inherit from the destination directory, which could have
  given a named user access the source never granted.
- The walk over the source cannot be turned against the copy while it runs: it
  descends through directory descriptors without following symlinks, refuses a
  file swapped for a FIFO instead of blocking, and Ctrl-C stops a long copy at the
  next entry and rolls back.
- Linux release binaries: static musl builds for x86_64 and aarch64
  (`parhel-v<version>-<target>.tar.gz` with a `.sha256` file and a build
  provenance attestation, like the macOS archive). The install script picks the
  archive for the system, and the Homebrew formula installs on Linux too.
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
