# Test catalog

Every behavior `git parhel` must have, as an ID, mapped to the tests that pin it down.
Unit test IDs appear as `// U-XXX-NN` comments above the test; integration test
functions are prefixed with their ID (`i31_...`).

Development was test-first: the suite was written against stubs (`todo!()` bodies and a
binary that answers `error[not-implemented]`) and every product test failed; only the
harness self-tests (`tests/harness.rs`, `failpoint::tests`) passed. The implementation
now passes the whole suite (I-124 is ignored unless `git-lfs` is installed).

The Linux backend (btrfs/XFS reflink) and `--copy` were developed the same way: the
tests (U-TXN-80…90, U-LFS-*, `add_copy.rs`, `add_linux.rs`) were committed first, against a
`LinuxFs` skeleton and a `copy_entry` stub that fail with `not-implemented`.

The store (phase 2: worktrees of sources without copy-on-write, `git parhel setup`) is being
built the same way, and the section [Store](#store--worktrees-without-copy-on-write) lists
its tests. Until it is implemented they are **red on purpose**: they fail against stubs that
answer `not-implemented` (or `todo!()`), or because `add` still answers `not-cow`; the
parts that are pure data (the journal's link field, size parsing, the command line, the
platform guard of `setup`) are real and their tests pass.

Spec changes made while implementing (each one a wrong assumption in the test, not a
weakened guarantee):
- I-10c: carried-over ignored files show as untracked when the target branch does not
  ignore them — the same thing `git switch` shows in the source.
- I-131: copy-on-write needs almost no space, so on a full volume a complete, clean
  worktree is as valid as a clean failure.
- I-23e, I-31/I-32, I-119: fixture/expectation bugs (missing parent dir, snapshot taken
  while the run's own registration existed, comparing HEAD text the test itself moved).

## How to run

```sh
cargo test --features failpoints      # full suite (failpoint tests need the feature)
cargo test --features failpoints -- --ignored   # tests needing optional tools (git-lfs)
cargo test                            # failpoint-driven tests show as `ignored`
cargo test --test harness             # harness self-tests only
```

Requirements, macOS: `$TMPDIR` on APFS, `git` ≥ 2.29, `hdiutil` (disk-image tests,
no root needed), `chflags`, `mkfifo`. Optional: `git-lfs` (I-124). Tests that mount images are serialized with `#[serial(hdiutil)]`.

Requirements, Linux: `git` ≥ 2.29, `python3` with `venv`, `mkfifo`, `$TMPDIR` on a reflink
filesystem (btrfs or XFS with `reflink=1`) and three mounts announced by
`PARHEL_TEST_BTRFS`, `PARHEL_TEST_XFS` and `PARHEL_TEST_EXT4`, writable by the (non-root)
user running the tests. `scripts/linux-test.sh` builds all of that in a privileged Docker
container (`scripts/mount-test-filesystems.sh` is the part CI shares); extra arguments go to
`cargo test`. A test that needs a mount whose variable is unset prints `SKIPPED: ...` and
passes, unless `CI` or `PARHEL_REQUIRE_FS=1` is set: then it fails, so a misconfigured
runner cannot turn the filesystem tests into silent passes. A variable that points at the
wrong filesystem always fails. Tests that only exist on one platform are gated with
`#[cfg(target_os = ...)]` (hdiutil images, `chflags`, `F_LOG2PHYS_EXT` on macOS; everything
in `add_linux.rs` and the FIEMAP proof on Linux).

## Known bugs of the original (`dannolan/git-worktree-apfs`) and the tests that cover them

| Bug | Fix in parhel | Tests |
|---|---|---|
| 1. A concurrent run deletes the winner's worktree (`RemoveAll(destination)` after losing) | Assemble in `.parhel-staging-*`, publish with `renamex_np(RENAME_EXCL)`, never delete the destination | U-TXN-43, U-FS-07/08, I-30, I-31, I-31b, I-32, I-32b, I-33 |
| 2. Submodule gitfiles still point at the source | Refuse by default; `--submodules=reinit` excludes submodule dirs and re-inits them with their own git dirs | U-EXCL-06, U-TXN-32, I-17…I-22 (I-19 reproduces the reported case) |
| 3. A failed run leaves its registration in `.git/worktrees`, so the branch stays "in use" | Write-ahead journal + rollback (worktree remove, branch delete only if created here, staging removal); `doctor --fix` after a crash | U-RB-*, U-TXN-40…48, U-DOC-03…05, I-40…I-45 |

## Unit tests (`src/**`, run with fakes or pure inputs)

| ID | Area | File |
|---|---|---|
| U-CLI-01…19 | Flag parsing, conflicts, defaults, help/version, non-UTF-8 paths, option-like names; U-CLI-17…19: `--copy` (off by default, conflicts with `--fallback`, help says it uses disk space and works across volumes) | `src/cli.rs` |
| U-ARGS-01…03 | `git worktree add` argv: every mode, values always after `--end-of-options` | `src/git/cli.rs` |
| U-PATH-01…19, P1…P3 | Lexical destination resolution, placement (`dest-is-source`, `dest-inside-git-dir`, nested), staging naming | `src/domain/paths.rs` |
| U-EXCL-01…10, P1…P2 | Exclusion set: root `.git`, nested staging, nested worktrees, submodules, component-wise matching | `src/domain/exclusions.rs` |
| U-GIT-01…03 | Re-pointing touches only its own registration; branch deletion is compare-and-delete (`update-ref -d`); option-like revisions never reach git | `src/git/cli.rs` |
| U-REL-01…08, P1 | Relocation of the source path: recorded forms (shebang, JSON, `file://`), boundaries (`/w/src2`, `/w/src.bak`, `/other/w/src`), `/private` alias, binaries reported, symlinks | `src/domain/relocate.rs` |
| U-PLAN-01…07, P1 | Minimal clone plan (descend only into ancestors of exclusions); property: every non-excluded leaf covered exactly once | `src/domain/clone_plan.rs` |
| U-PARSE-01…20, P1…P4 | `git --version`, `worktree list -z`, dirty status, gitlinks, object ids, bool config, `ls-files -v` flags, in-progress operation markers; properties incl. "never panics" | `src/git/parse.rs` |
| U-ENV-01…05 | Child `git` env sanitization, locale, cwd/args, error mapping, `GIT_OPTIONAL_LOCKS=0` | `src/git/runner.rs` |
| U-FS-01…23 | `clonefile`/`renamex_np`/`statfs` adapter on the real APFS `$TMPDIR`; `uchg` cleanup, `flock` (in-process and cross-process), EACCES, EILSEQ, NFC/NFD, no-follow dir check, parallel relocation scan; U-FS-21/22: the copy flavour (`--copy`); U-FS-23: a partial copy is discarded even with read-only directories and immutable leftovers | `src/fs/apfs.rs` (macOS-only: U-FS-01…08, 12, 13, 17, 18, 21, 22), `src/fs/unix.rs` (shared by Unix adapters: U-FS-09…11, 14…16, 19, 20) and `src/fs/tree.rs` (U-FS-23) |
| U-LFS-01…44 | Linux adapter on real btrfs / XFS / ext4 mounts (env `PARHEL_TEST_*`, skip-or-fail rule above): `volume` (`cow` from a probe, `fs_type` names, no leftovers from the probe, read-only dirs, unknown fs by magic number), `rename_exclusive` (`RENAME_NOREPLACE`: file, dir, empty dir, dangling symlink, 8 racers), `clone_entry` (contents, modes, symlinks never followed, empty dirs, FIFOs, sockets skipped, `user.*` xattrs, nanosecond file and directory mtimes, hard links separated, extents proven shared with FIEMAP, `cross-volume`, ext4 `clone-failed`, unreadable file named, read-only dirs), `copy_entry` (same tree, extents NOT shared even where `copy_file_range` would reflink, across volumes, sparse files, failures), `remove_dir_all` of read-only dirs, `write_file`, and the shared helpers being wired; U-LFS-43: filesystem identity (one per filesystem, shared by btrfs subvolumes, different between filesystems); U-LFS-44: a missing `RENAME_NOREPLACE` is explained | `src/fs/linux.rs` (Linux-only) |
| U-RB-01…12, P1 (U-RB-10…12: a branch is deleted only while it still points at its start commit; U-RB-06: fallback removes only its own registration, never `worktree prune`) | Journal encoding, rollback order, fallbacks, aggregated failure | `src/app/rollback.rs` |
| U-TXN-01…94 | `add` transaction: order, refusals before any mutation (and their precedence), fallback, exclusions, rollback at each step, lost race, SIGINT, operation in progress, flagged index entries, source changed mid-run, journal lock, early failures, destination in submodule, non-UTF-8 destination (U-TXN-60 refused on macOS only; U-TXN-60b accepted on Linux unless it would name the implicit branch), publish intent journaled before the rename, symlinks on the path to an exclusion cloned as links, relocation of untracked files, `.parhelignore`, detaching foreign gitfiles, permissions of created dirs (U-TXN-72; U-TXN-91: applied after the directory's contents, deepest first, so a read-only source directory can be filled), dirty submodules with reinit; U-TXN-80…91: `--copy` (no copy-on-write requirement, cross-volume allowed, copies even where cloning works, the same call sequence as the clone transaction with every `Clone` replaced by `Copy` for nested worktrees, nested destination, `.parhelignore`, submodules, symlink ancestors and relocation, rollback at each step and mid-copy, SIGINT, `--copy` + `--fallback` is a usage error, no other refusal weakened, the refusal for a missing copy-on-write names both `--copy` and `--fallback` without mentioning APFS); U-TXN-92/93: on one filesystem the destination's probe decides, devices of one filesystem (btrfs subvolumes) are not `cross-volume` while distinct filesystems are; U-TXN-94: the worktree's root takes the source's mode only after publishing and repair | `src/app/add.rs` |
| U-DOC-01…19 | `doctor`: findings, read-only by default, `--fix` via journal, foreign gitfiles, live runs (`in-progress`), foreign prunable entries ignored, crash after publish repaired (not rolled back) even when the journal lacks the destination or is lost, scoped cleanup of orphan registrations | `src/app/doctor.rs` |

## Integration tests (`tests/*.rs`, real `git`, real APFS or btrfs/XFS)

Every failure-path test checks the same invariants with `RepoSnapshot::assert_same`:
source HEAD, status, worktree list, branches, `.git/worktrees` entries and file tree
(content, mode, symlink targets) unchanged, no `.parhel-staging-*` left, destination absent
or untouched.

### Creation — `tests/add_basic.rs`
| ID | Behavior |
|---|---|
| I-01 | `--new-branch` from the main checkout: clean, ignored dirs present, registered, id = basename, source independent |
| I-02 | `--branch` existing |
| I-03 | `--detach` |
| I-04 | Explicit ref equal to source HEAD (tag, full sha, short sha) |
| I-05 / I-05b | No branch flag: branch named after basename, created or reused |
| I-06a / I-06b | `--from` a subdirectory with unrelated cwd; linked worktree as source |
| I-07 | Relative destination resolves against cwd |
| I-08 | Destination nested inside the source never contains itself |
| I-09 | Nested linked worktree of the same repo is excluded and unaffected |
| I-09c | Registration recorded through a symlink (hand-edited `gitdir`; git itself canonicalizes): the symlink stays a symlink |

### Target ref ≠ source HEAD — `tests/add_refs.rs`
| ID | Behavior |
|---|---|
| I-10a/b/c | Older / newer / unrelated target: exact tree (add, delete, rename, chmod, symlink retarget, emptied dirs) |
| I-11 | Ignored source file colliding with a tracked target file: destination gets the tracked version |

### Dirty source — `tests/add_dirty.rs`
| ID | Behavior |
|---|---|
| I-12 / I-12b / I-13 | Modified, staged, untracked → `dirty-source` with `--allow-dirty` hint, no side effects |
| I-14 | Ignored-only changes do not count |
| I-15 | `--allow-dirty` carries unstaged/staged/deleted/untracked (staged arrive unstaged, documented) |
| I-16a/b/c | `--allow-dirty` + different ref: no conflict ok; conflict → `checkout-conflict` + rollback |

### Submodules — `tests/add_submodules.rs` (bug 2)
| ID | Behavior |
|---|---|
| I-17 | Initialized submodule → `submodules-present`, no side effects |
| I-18 | `reinit` from main checkout: clean status, own git dir, checkout in destination does not touch source |
| I-19 | `reinit` from linked worktree: branch + commit in destination submodule leave source untouched |
| I-20 | Nested submodules re-initialized recursively |
| I-21 | Declared but uninitialized submodule does not block |
| I-22 | `reinit` works offline and keeps the declared URL |
| I-22b | `--allow-dirty --submodules=reinit` with a dirty submodule → `dirty-submodule`, no trace |

### Fidelity — `tests/add_fidelity.rs`
| ID | Behavior |
|---|---|
| I-23a…k | Symlinks (relative, absolute, dangling, to dir, ignored) not followed; modes; empty dirs; xattrs (libc, both platforms); NFD names and spaces; hard links decoupled; eol attributes; mtimes; directory permissions not widened; I-23j: a read-only (0555) source directory that is an ancestor of a nested worktree or a `.parhelignore` entry is filled, then locked with the source's mode, for clone and `--copy`; I-23k: rollback after that point (failpoint) removes the locked staging. Linux fidelity under both modes: I-L30…I-L40 |
| I-24 | Sparse-checkout → `sparse-checkout` refusal |

### Races — `tests/add_race.rs` (bug 1)
| ID | Behavior |
|---|---|
| I-30 | Destination exists as file / dir / empty dir / dangling symlink → `dest-exists`, untouched |
| I-31 / I-31b | Another parhel run wins while A is paused before publish: winner and uncommitted file intact, winner functional |
| I-32 / I-32b | Empty dir or plain `git worktree add` appears during the run → not replaced |
| I-33 | 8 concurrent runs, same destination: exactly one winner, losers leave no branch/registration |

### Rollback — `tests/add_rollback.rs` (bug 3)
| ID | Behavior |
|---|---|
| I-40a/b | Injected error at every failpoint: no trace, branch reusable; created branch removed |
| I-40c | Panic mid-transaction still rolls back |
| I-41 | Branch in use elsewhere → git's reason forwarded |
| I-42 | `--new-branch` with a taken name never deletes the existing branch |
| I-43 | Unreadable source entry → `clone-failed`, rollback |
| I-44 | SIGINT → exit 130 + rollback |
| I-44b/c | SIGTERM (143), SIGHUP (129) → rollback |
| I-44d | Repeated SIGINT/SIGTERM during rollback do not abort the cleanup |
| I-45 | SIGKILL → `doctor` reports, `doctor --fix` restores, branch reusable |
| I-45b | SIGKILL between the publishing rename and `worktree repair` → `doctor --fix` repairs the destination; branch and user files kept |
| I-45c | Same, with the journal lost (truncated): publication is detected from the disk |
| I-45d | Same, destination in another folder: the staging dir is found through the registration |
| I-45e | A crashed run's branch that gained a commit is kept by `doctor --fix` |
| I-46 | Rollback's fallback cleanup keeps unrelated, unreachable worktrees (unmounted disk) registered; git is made to fail at removing the worktree with an immutable file (macOS) or a read-only directory (Linux) |

### Filesystems — `tests/add_filesystems.rs` (macOS disk images; Linux: see below)
| ID | Behavior |
|---|---|
| I-50 | macOS: destination on another APFS volume → `cross-volume` |
| I-51a/b | macOS: source or destination on HFS+ (no copy-on-write) → `not-cow` with `--fallback` hint |
| I-52a | macOS: `--fallback` → plain worktree + warning |
| I-52b | `--fallback` is inert where copy-on-write works (APFS, btrfs, XFS) |
| I-53 | macOS: case-sensitive APFS |
| I-55a/b/c | macOS: `--copy` on HFS+ and across volumes; the `not-cow` refusal offers `--copy` and `--fallback` |

### Copy-on-write evidence — `tests/add_cow.rs`
| ID | Behavior |
|---|---|
| I-54a | Tracked and ignored files share physical extents (`F_LOG2PHYS_EXT` on macOS, `FIEMAP` on Linux) |
| I-54b | Writes in the destination never reach the source |
| I-54c | macOS: cloning 128 MiB uses < 8 MiB of free space (isolated volume) |

### Environment — `tests/add_env.rs`
| ID | Behavior |
|---|---|
| I-60 | Inherited `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` do not redirect |
| I-61a/b/c | `git parhel ...` via PATH, `git -C`, `git parhel -h` |
| I-62a…f | Bare repo, not a repo, unborn HEAD, inside `.git`, destination = source / in `.git`, missing parent |
| I-63 | git older than 2.29 → `git-too-old` |
| I-64 / I-64b | Option-like ref/branch values rejected (`usage`); `-weird` after `--` is a valid destination |
| I-65 | Non-ASCII branch names |
| I-64c | A destination basename starting with `-` is refused when git would turn it into a branch name |

### CLI — `tests/cli.rs`
| ID | Behavior |
|---|---|
| I-70a…e | Help, `add --help` flags, version, usage errors exit 2, errors only on stderr |

### Doctor — `tests/doctor.rs`
| ID | Behavior |
|---|---|
| I-80a…f | Healthy, volume without copy-on-write (`not-cow`; macOS: HFS+, Linux: I-L14), orphan staging, orphan registration (branch kept without journal), foreign gitfile (never touched), outside repo |
| I-80g | `doctor --fix` during a live `add` reports `in-progress` and touches nothing; the run then succeeds |
| I-80h | `doctor --fix` cleaning an orphan registration keeps unrelated, unreachable worktrees registered |
| I-80k | Legitimate worktrees whose names look like staging (`.parhel-staging-feature`, `.parhel-staging-x/feat`) survive `doctor --fix` |
| I-80j | A forged journal cannot make `doctor --fix` delete a directory or branch outside its staging folder |

### Lifecycle — `tests/add_lifecycle.rs`
| ID | Behavior |
|---|---|
| I-90 / I-90b | Same basename in two parents; basename id taken by a stale registration |
| I-91 | `worktree prune --expire now` and `gc --prune=now` keep the worktree; admin `gitdir` points to the final path |
| I-92 | `worktree lock`/`unlock`/`move`/`remove` work |
| I-93 | `worktree repair` has nothing to do |
| I-94 | 6 concurrent runs to different destinations all succeed |
| I-95 / I-95b | `post-checkout` not run on copy-on-write (decision pinned); run by `--fallback` (I-95b macOS; Linux: I-L07) |
| I-96 | A parhel worktree as source of the next |
| I-97 | Destination parent reached through a symlink |

### Repository layouts and formats — `tests/add_layouts.rs`
| ID | Behavior |
|---|---|
| I-100 | Linked worktree of a bare repository as source |
| I-101 | `.bare` + `.git` gitfile layout, sibling worktrees not cloned |
| I-102 | `--separate-git-dir` source (root `.git` is a file) |
| I-103 | Source is a submodule checkout |
| I-104 | SHA-256 object format |
| I-105 / I-105b | reftable ref backend, including rollback |
| I-106 | Shallow clone |
| I-107 / I-107b | Blobless partial clone: missing blobs fetched; offline → `git-failed` + rollback |
| I-108 | Destination inside another linked worktree |
| I-109 | Destination inside a submodule → `dest-inside-submodule` |
| I-180 | `worktree.useRelativePaths` (git 2.48+): the new worktree works and survives `prune` |
| I-181 | Per-worktree config: same view as a plain `git worktree add`; the source's `config.worktree` untouched |

### Source state — `tests/add_source_state.rs`
| ID | Behavior |
|---|---|
| I-110 / I-111 / I-112 / I-113 / I-114 / I-115 | merge (conflicted / `--no-commit`), rebase, cherry-pick, revert, am in progress → `operation-in-progress`, even with `--allow-dirty` |
| I-116 | bisect in progress is allowed (clean detached checkout) |
| I-117 / I-117b / I-118 | `assume-unchanged` / `skip-worktree` count as dirty; with `--allow-dirty` the content arrives as a visible change |
| I-119 / I-120 / I-121 | Commit, tracked edit or new untracked file in the source mid-run → `source-changed`, no trace |
| I-121b | Ignored files changing mid-run (running builds) are tolerated |
| I-122 / I-123 | clean/smudge filters, same and different ref |
| I-124 | Git LFS (ignored unless `git-lfs` is installed) |

### OS and filesystem limits — `tests/add_limits.rs`
| ID | Behavior |
|---|---|
| I-130 | Unwritable destination parent → `permission-denied` |
| I-131 | macOS: full volume → real error, no trace (Linux: I-L18) |
| I-132 | Paths near `PATH_MAX`: full clone or clean `clone-failed` |
| I-133 | macOS: non-UTF-8 destination → `invalid-path` (APFS rejects it) |
| I-134 | macOS: NFC destination when the NFD name exists → `dest-exists` |
| I-135 / I-136 | macOS: `uchg` files are cloned with their flag; rollback still removes staging (Linux: I-L15, I-L16) |
| I-137 | FIFO and socket inside ignored dirs |
| I-138 | 30k files + 64 MiB in under 10 s |

### Relocation — `tests/add_relocate.rs`
| ID | Behavior |
|---|---|
| I-150 | Script shebang, config and absolute symlink recording the source point at the copy; source untouched |
| I-151 | Real Python venv: `sys.prefix`, `pip` and `activate` of the copy are the copy's |
| I-151b | A virtualenv copied to a path with spaces: `pip` runs (shebang through `env -S`) |
| I-152 | Paths that only look like the source are left alone |
| I-153 | Tracked files are never rewritten |
| I-154 | Files over 1 MiB are left as cloned (still sharing blocks) |
| I-155 | Binaries mentioning the source are reported, not edited |
| I-156 | `--no-relocate` copies as is |
| I-157 | Read-only files are relocated and stay read-only |
| I-160 | A symlink to rewrite and a foreign gitfile to detach inside read-only directories: done, directory modes restored |
| I-158 | `.parhelignore` keeps untracked paths out; tracked files always present |
| I-159 | A linked worktree of another repository inside an ignored folder is detached in the copy |
| I-159b | A nested repo with `--separate-git-dir` in the source follows the copy (commits in the copy never move the source's) |
| I-150b | References into excluded paths (`git clone --shared` alternates into the source's `.git`) are kept; the copy works |

### Blast radius — `tests/blast_radius.rs`
Fixture: four other worktrees (dirty, broken `.git` link, unreachable, nested in
the source). Every byte of them, of `.git/config`, the refs and the other
registrations must be identical after the run.

| ID | Behavior |
|---|---|
| I-170 | Successful `add` |
| I-171 | Refused `add` (the source index is not rewritten) |
| I-172 | Failure at every failpoint, then rollback |
| I-173 | Crash before and after publishing, then `doctor --fix` |
| I-174 | Relocation and gitfile detaching |
| I-175 | Submodule reinit does not rewrite the shared configuration |
| I-175b | Submodule reinit from a linked worktree with its own per-worktree URL keeps the shared config |
| I-176 | `doctor --fix` keeps a non-empty folder that only looks like staging |

### Copy mode — `tests/add_copy.rs` (macOS and Linux)
`--copy` on the `$TMPDIR` volume, where cloning would work: the files must be copied.

| ID | Behavior |
|---|---|
| I-C01 | Files equal and NOT sharing storage (`F_LOG2PHYS_EXT` / `FIEMAP`); strategy reported as a full copy, no warning |
| I-C02 | Ignored files arrive, the source is unchanged, no staging left |
| I-C03 | Writes do not cross between copy and source, either way |
| I-C04 | `--copy --fallback` → `usage` (exit 2), nothing created |
| I-C05 | References to the source path are relocated in copies; `--no-relocate` copies verbatim |
| I-C06 | Existing destination → `dest-exists`, untouched |
| I-C07 | The source-state refusals (`dirty-source`) apply; `--allow-dirty` carries changes |
| I-C08 | Destination inside the source: the staging dir is not copied into itself |
| I-C09 | `.parhelignore` is honored |
| I-C10 | Injected error at every failpoint → no trace |
| I-C11 | SIGINT during the copy → exit 130, rollback |
| I-C12 | SIGKILL after publishing → `doctor --fix` repairs, never removes |

### Linux — `tests/add_linux.rs` (btrfs, XFS, ext4 mounts from the environment)
| ID | Behavior |
|---|---|
| I-L01 / I-L02 | btrfs / XFS: `add` succeeds, worktree clean, ignored and tracked files present and proven shared (FIEMAP), no warning |
| I-L03 | Writes diverge on btrfs and XFS (reflink, not a hard link) |
| I-L04 | ext4: `add` → `not-cow` naming ext4 and offering `--copy` and `--fallback`; zero mutation |
| I-L05 | ext4 `--copy`: equal files, NOT shared |
| I-L06 / I-L07 | ext4 `--fallback`: plain worktree with a warning; runs `post-checkout` |
| I-L08 | Source on ext4 or destination on ext4 → `not-cow` naming the offending path; zero mutation |
| I-L09 | `--copy` between ext4 and btrfs / XFS, both directions |
| I-L10 / I-L11 / I-L12 | btrfs → XFS: `cross-volume` (zero mutation), `--copy` succeeds, `--fallback` gives a plain worktree |
| I-L13 / I-L14 | `doctor`: quiet on btrfs and XFS; `not-cow` finding naming ext4 |
| I-L15 | Read-only directories are filled and then locked: same modes, reflink and copy, three filesystems |
| I-L16 / I-L17 | Rollback removes read-only directories, even when git itself cannot remove the staged worktree and unrelated unreachable worktrees keep their registration |
| I-L18 | A copy that cannot write its data (a write-size limit, `ulimit -f`, stands in for a full disk) fails with a real error and no trace |
| I-L30…I-L39 | Fidelity under reflink (btrfs, XFS) and copy (btrfs, XFS, ext4): modes incl. setgid and odd bits; symlinks (absolute, relative, dangling, to dirs, outside) never followed; file and directory mtimes to the nanosecond; `user.*` xattrs on files and directories; FIFOs kept and sockets skipped; empty dirs; NFC/NFD, CJK, emoji, spaces; destination with spaces and accents; hard links decoupled; sparse files |
| I-L40 | The whole worktree equals the source entry by entry (kind, mode, content hash, link target) in every mode |
| I-L41 | An empty directory created at the destination during assembly is not replaced (`RENAME_NOREPLACE`), reflink and copy |
| I-L42 | Chained worktrees keep sharing |
| I-L43 | Destination on tmpfs: `not-cow` without `--copy`, success with it |
| I-L44 | POSIX ACLs (access and default; files, walked directories, directories created for `.parhelignore`, the worktree root) are copied exactly: btrfs and XFS reflink and copy, ext4 copy |
| I-L45 | A default ACL on the destination directory does not leak into the copies (no named-user access the source never had) |
| I-L46 | A read-only (0555) source top directory on XFS still clones, `doctor` stays quiet |
| I-L47 | Two btrfs subvolumes (different `st_dev`) share storage: not `cross-volume` |
| I-L48 | A NOCOW file (`chattr +C`) fails with `clone-failed` naming the file and offering `--copy` / `.parhelignore`; `--copy` works |

The platform-neutral suites (`add_race.rs`, `add_rollback.rs`, `doctor.rs`, `blast_radius.rs`, ...) run unchanged on Linux. Harness changes for portability: `Sandbox::drop` runs `chflags` only on macOS; `tests/common/volume.rs` gates `DiskImage` (hdiutil) to macOS and adds the env-var mounts (`test_mount`, with `resolve_mount` as its testable core); `tests/common/cow.rs` proves sharing with FIEMAP on Linux and names filesystems from `f_type`; extended attributes go through libc (`tests/common/xattr.rs`) instead of the `xattr` binary; I-23d and I-46 are written for both platforms.

### Walk hardening — `tests/add_walk.rs` (macOS and Linux)
The walk is frozen with the `walk_entry:<path>` failpoint (or `mid_clone`), the source is changed, the walk is released.

| ID | Behavior |
|---|---|
| I-W01 | A directory swapped for a symlink to the outside while the walk is about to enter it → `clone-failed`, nothing from outside copied, no trace (copy; reflink too on Linux) |
| I-W02 | The same for an ancestor of an exclusion between the plan and the clone, every mode |
| I-W03 | A regular file swapped for a FIFO → `clone-failed` at once instead of blocking forever |
| I-W04 | SIGINT while the walk is paused → exit 130, the walk goes no further, full rollback |

### ACLs on macOS — `tests/add_acl.rs`
| ID | Behavior |
|---|---|
| I-A01 | `--copy` gives every entry its source's ACL and none inherited from the destination directory |
| I-A02 | A clone does so for the entries it clones itself (below a cloned directory `clonefile(2)` carries none, documented) |

### Packaging — `tests/packaging.rs`
| ID | Behavior |
|---|---|
| I-P01 | The Homebrew formula renderer accepts semver-like versions and refuses anything it would splice into `sed` |

### Harness self-tests — `tests/harness.rs`
They must pass on both platforms (they prove the tools the product tests rely on). Linux additions: a `cp --reflink=always` copy is detected as shared on btrfs and XFS and a plain copy is not, one file is not "shared" with itself, ext4 never reports sharing, `$TMPDIR` is a reflink filesystem, the three mounts have the announced filesystem and distinct devices, and the skip-or-fail rule of the mount helper (missing variable skips unless required; required, empty or non-directory values fail).

## Store — worktrees without copy-on-write

A per-user btrfs image (`/parhel/<user>`, created by `sudo git parhel setup`) holds a
baseline mirror of each repository and the worktrees, which are reflink clones of it; the
requested destination becomes a symlink to the real worktree. `git worktree remove
<symlink>` was tried with real git: it resolves the link, removes the real worktree and its
registration, and leaves the symlink dangling (I-S16; `doctor` finds and removes it).

Status: **red** (step A of the store work: tests and stubs). Green today, on purpose: the
real parts (U-CLI-20…26, U-STO-60…62 for the journal's link field, U-SET-01, I-SET14 on
macOS, the gated I-SET tests that skip) and guard rails that hold trivially while nothing is
implemented, so they will start meaning something later: the cases of U-STO-20 that must
*not* take the store, U-STO-21b, U-STO-23, U-STO-24, U-STO-58, U-STO-64…66 (nothing is
removed), the "quiet" cases of U-STO-70/71/74/77/79, the retargeted/forged cases of U-STO-82,
and I-S13b, I-S14, I-S15.

### Unit tests with fakes
`World` (recording fake of git and the filesystem) gained `with_store()`, `stat_within`,
`mirror_entry`, `mirror_dir_meta`, `symlink_exclusive`, `read_link`, `append_file`,
`is_writable`, `space`; `FakeHost` records the mutations of the machine for `setup` and
simulates just enough (files, mounts, immutable directories, units) for re-runs.

| ID | Behavior | File |
|---|---|---|
| U-STO-01…04 | Marker `.parhel-store`: round trip, exact text (`format=1` first), order and unknown keys ignored, rejects empty/garbage/newer format/missing keys with `store-unavailable` | `src/domain/store.rs` |
| U-STO-05 | Store path: `PARHEL_STORE` wins, empty is ignored, default `/parhel/<user>` | `src/domain/store.rs` |
| U-STO-06 | Repository id = first 16 bytes of SHA-256 of the path bytes (known vectors, non-UTF-8) | `src/domain/store.rs` |
| U-STO-07 | Sync decision table: unchanged, new, size, mtime to the nanosecond, mode, kind changes, symlink target, FIFO, directory mtime ignored but directory mode compared | `src/domain/store.rs` |
| U-STO-10…16 | Discovery: not configured, available, no marker, other format, garbage, store without copy-on-write, not writable; never mutates | `src/app/store.rs` |
| U-STO-30…44 | Sync: first sync creates parents first and sets directory metadata last, nothing changed → no call, changed file alone (+ its directory's metadata), mode-only change, new file, deleted file, deleted directory in one removal, file ↔ directory replaced (remove then create), symlinks by target, exclusions never copied and removed when new, Ctrl-C, failing copy, half-synced baseline repaired | `src/app/store.rs` |
| U-STO-20 | When the store is taken: ext4 source and destination, copy-on-write source with an ext4 destination, ext4 source with a cow destination, two cow filesystems; not when the source can clone, when the store shares the source's filesystem, or with `--copy` / `--fallback` | `src/app/add.rs` |
| U-STO-21 / 21b | Not configured: `not-cow` also suggests `sudo git parhel setup` (and `--copy` / `--fallback`); where no store path exists nothing is suggested | `src/app/add.rs` |
| U-STO-22 / 22b | Configured but unusable (no marker, garbage marker, not writable, no copy-on-write): `store-unavailable` pointing at `git parhel doctor`, no mutation, never a fallback | `src/app/add.rs` |
| U-STO-23 | `--copy` and `--fallback` ignore an unavailable store entirely | `src/app/add.rs` |
| U-STO-24 | Refusals (dirty source, operation in progress, sparse checkout, destination exists or a dangling symlink, parent missing) come before the store is touched | `src/app/add.rs` |
| U-STO-45 | The route in order: baseline lock, sync, staging in the store, worktree add, clone from the baseline, index, registry, symlink, exclusive rename, repair at the real path, cleanup | `src/app/add.rs` |
| U-STO-46 / 47 | Lock and `meta` (`source=`) next to the baseline; clones come from the baseline tree, never copies | `src/app/add.rs` |
| U-STO-48 / 49 | The journal records the link and the real destination before the symlink exists; `links` registry line `<link>\t<target>` appended first | `src/app/add.rs` |
| U-STO-50 / 51 | Symlink cannot be created → `dest-exists`, nothing removed from the destination; lost exclusive rename → our symlink removed | `src/app/add.rs` |
| U-STO-52 | Real name suffixes `-2`, `-3` (first free); the requested path is unchanged | `src/app/add.rs` |
| U-STO-53 | Relocation rewrites to the real path | `src/app/add.rs` |
| U-STO-54…58 | Failure at every step, failing sync, Ctrl-C during/after the sync, source changed, existing branch: no symlink, no registration, branch only if ours, staging removed last, baseline untouched | `src/app/add.rs` |
| U-STO-59 | Second run for the same repository copies nothing | `src/app/add.rs` |
| U-STO-60…62 | Journal link field: round trip (hostile paths), decodes journals without it, a link without a target is corruption | `src/app/rollback.rs` |
| U-STO-63…67 | Rollback removes the symlink only while it points at the recorded target; keeps a retargeted link, a directory or file in its place, or nothing; names it when it cannot remove it | `src/app/rollback.rs` |
| U-STO-70…72 | Doctor: healthy store silent; no store configured → no store findings; unavailable store is one finding with its reason (deeper checks skipped) | `src/app/doctor.rs` |
| U-STO-73…74 | `store-image-deleted` (mountinfo → loop device → `backing_file` ends with ` (deleted)`); `store-low-space` (host filesystem of the image or the store: under 10% or under 2 GiB) | `src/app/doctor.rs` |
| U-STO-75 | `store-orphan`: a directory under `worktrees/<id>/` git does not know; reported only, never deleted, not fixable | `src/app/doctor.rs` |
| U-STO-76 / 77 | `store-dangling-link` found from the registry and fixed by removing the symlink; entries whose path is retargeted, replaced or gone are ignored | `src/app/doctor.rs` |
| U-STO-78 / 79 | `store-stale-baseline` (source no longer a worktree) fixed by deleting that baseline; a baseline of a linked worktree is not stale | `src/app/doctor.rs` |
| U-STO-80 | `--fix` never deletes anything under `worktrees/` | `src/app/doctor.rs` |
| U-STO-81…83 | Crashed store runs: the symlink goes with the rollback when it still points where the journal says; a forged or retargeted link is never removed; killed after the rename → repair at the real path, keep worktree, branch and symlink | `src/app/doctor.rs` |
| U-STO-84 | `not-cow` is not reported while the store can serve the source (it is when the store is broken or absent) | `src/app/doctor.rs` |
| U-SET-01 | Off Linux: `unsupported-platform`, no mutation (real) | `src/app/setup.rs` |
| U-SET-02…09 | Preconditions: root (`not-root` naming `sudo git parhel setup [--remove]`), `mkfs.btrfs` (`missing-tool` naming `btrfs-progs`), btrfs loadable or not, systemd, unknown user, user from `$SUDO_USER`, inferred root refused and explicit `--user root` honored | `src/app/setup.rs` |
| U-SET-10…14 | The steps in order (image, mkfs, mount point, unit, enable, ownership, layout, marker, `chattr +i`, verify), modes and owners, unit content, unit name from `systemd-escape`, marker | `src/app/setup.rs` |
| U-SET-15…18 | Size, `--preallocate`; re-run is a no-op (`AlreadySetUp`); a stopped store is started again; an existing image is never reformatted | `src/app/setup.rs` |
| U-SET-19…21 | Missing `chattr` or an unprotected image fails loudly leaving a usable store; summary | `src/app/setup.rs` |
| U-SET-30…37 | `--remove`: refuses with worktrees (`store-in-use`, listing them), ignores baselines and empty directories, the order (disable, remove unit, reload, `chattr -i`, delete image, remove mount point), re-protects the directory while other images remain, idempotent, same user rules, partial stores, touches only parhel's paths | `src/app/setup.rs` |
| U-CLI-20…24 | `setup` command line: defaults, flags, `--remove` conflicts, invalid usage, help mentions sudo and `/parhel/<user>` (real) | `src/cli.rs` |
| U-CLI-25 / 26 | `--size`: `K/M/G/T` suffixes, bare bytes; rejects malformed, overflowing, below 256M (real) | `src/domain/request.rs` |

### Integration — `tests/add_store.rs` (Linux; source on ext4, store on btrfs)
The store is a fresh directory under `PARHEL_TEST_BTRFS` with the marker `setup` writes
(`tests/common/store.rs`), announced through `PARHEL_STORE`; the source sandbox is under
`PARHEL_TEST_EXT4`. Same skip-or-fail rule as the other Linux tests. By default every test
command gets a `PARHEL_STORE` that does not exist, so a developer's real store never leaks in.

| ID | Behavior |
|---|---|
| I-S01 | `add` from ext4: symlink at the requested path, worktree in the store, clean, branch, ignored files, git registered the real path, both paths printed |
| I-S02 | The source is unchanged |
| I-S03 | Two worktrees share extents with each other and with the baseline (FIEMAP) |
| I-S04 | Writes do not cross between worktree, baseline, source and sibling worktree |
| I-S05 | Second add after a change copies only the changed file (inode and mtime of the others unchanged) |
| I-S06 | Deleting in the source deletes in the baseline and the next worktree; the earlier worktree keeps it |
| I-S07 | A later sync does not disturb existing worktrees |
| I-S08 / I-S08b | Injected error at every failpoint (existing ones plus `after_sync`, `after_link`) and a panic after the link: no symlink, registration, branch or staging; baseline a complete mirror |
| I-S09a…c | Race at the link: a second run wins / a directory / a dangling symlink appears: `dest-exists`, the interference untouched, the loser leaves nothing |
| I-S10 | SIGINT during the sync (`sync_entry:<path>`): exit 130, no trace; the next run repairs the baseline |
| I-S11 | Relocation points at the real path (file and symlink) |
| I-S12 | No store: `not-cow` suggests `sudo git parhel setup`; nothing created |
| I-S13 / I-S13b | Marker missing / newer format / garbage: `store-unavailable`; `--copy` and `--fallback` bypass the store |
| I-S14 | A source on btrfs does not use the store |
| I-S15 | Refusals (existing destination, dirty source) create nothing in the store |
| I-S16 | `git worktree remove <symlink>` removes the real worktree and leaves the link dangling; the path stays taken until `doctor --fix` |
| I-S17 | Same name twice: `feat` and `feat-2`, each path points at its own |
| I-S18 | `--allow-dirty` carries work; the baseline follows the source back |
| I-S19 | A ref other than HEAD (detached) |
| I-S20 | Modes, nanosecond mtimes and symlinks (absolute, outside, dangling) survive |
| I-S21 | Working through the symlink: commits, `worktree list` shows the real path |
| I-S22 / I-S23 | `.parhelignore` paths and nested worktrees are not mirrored |
| I-S24…S28 | `doctor`: quiet on a healthy store; dangling link (report, `--fix`); orphan directory (never deleted); stale baseline (`--fix` deletes it only); unavailable store |
| I-S29 / I-S30 | SIGKILL after the rename: `doctor --fix` repairs, keeps worktree, branch and link; SIGKILL after the link: `doctor --fix` removes link, registration, branch, staging |

### Setup — `tests/setup.rs` (needs sudo, systemd, btrfs-progs; `PARHEL_TEST_SETUP=1`, CI job `setup (Linux)`)
Without the variable every test prints `SKIPPED: PARHEL_TEST_SETUP=1 is not set ...` and
passes; the tests are serialized (`#[serial(setup)]`) and clean the machine before and after.
They run as an ordinary user and call `sudo -n` only for `setup`, `systemctl`, and the
protected image (see the comment on the CI job for why).

| ID | Behavior |
|---|---|
| I-SET01 | Creates a sparse image (0600 root), the unit (enabled, active, content), a btrfs mount at `/parhel/<user>`, the marker, layout and ownership (0700 the user's) |
| I-SET02 | Re-run changes nothing and says it is already set up |
| I-SET03 | `/var/lib/parhel` is immutable: root cannot delete or rename the image (`EPERM`), writing to it works |
| I-SET04 | `add` from an ext4 source uses the store at its default place; without a store it says to run setup |
| I-SET05 | `systemctl stop` + `start` ("reboot"): unavailable while stopped (`store-unavailable`), back with its marker after |
| I-SET06 | `--remove` refuses with a worktree (`store-in-use` naming it) and succeeds after `git worktree remove` |
| I-SET07 | After `--remove`: no unit, image or mount point, systemd forgets the unit; idempotent; setup works again |
| I-SET08 | `--preallocate` allocates the whole image |
| I-SET09 | Without root: `not-root` naming the sudo command; nothing created |
| I-SET10 | A root shell (no `SUDO_USER`) is refused with a hint for `--user`; nothing created |
| I-SET11 | No `mkfs.btrfs` (empty PATH): `missing-tool` naming `btrfs-progs` |
| I-SET12 | Unknown `--user`: usage error, nothing created |
| I-SET13 | `doctor` reports `store-image-deleted` when the image was deleted under a mounted store |
| I-SET14 | Off Linux (macOS): `setup` is `unsupported-platform` (the only test here that runs unguarded) |

## Not covered (needs infrastructure)

- A matrix of real git versions (only the installed one runs; the minimum is checked with a fake `git`).
- Mutation testing (`cargo mutants`) once the implementation exists, to measure how many regressions the suite catches.
