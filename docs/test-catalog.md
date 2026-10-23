# Test catalog

Every behavior `git parhel` must have, as an ID, mapped to the tests that pin it down.
Unit test IDs appear as `// U-XXX-NN` comments above the test; integration test
functions are prefixed with their ID (`i31_...`).

Development was test-first: the suite was written against stubs (`todo!()` bodies and a
binary that answers `error[not-implemented]`) and every product test failed; only the
harness self-tests (`tests/harness.rs`, `failpoint::tests`) passed. The implementation
now passes the whole suite (I-124 is ignored unless `git-lfs` is installed).

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

Requirements: macOS with `$TMPDIR` on APFS, `git` ≥ 2.29, `hdiutil` (disk-image tests,
no root needed), `xattr`, `chflags`, `mkfifo`. Optional: `git-lfs` (I-124). Tests that mount images are serialized with `#[serial(hdiutil)]`.

## Known bugs of the original (`dannolan/git-worktree-apfs`) and the tests that cover them

| Bug | Fix in parhel | Tests |
|---|---|---|
| 1. A concurrent run deletes the winner's worktree (`RemoveAll(destination)` after losing) | Assemble in `.parhel-staging-*`, publish with `renamex_np(RENAME_EXCL)`, never delete the destination | U-TXN-43, U-FS-07/08, I-30, I-31, I-31b, I-32, I-32b, I-33 |
| 2. Submodule gitfiles still point at the source | Refuse by default; `--submodules=reinit` excludes submodule dirs and re-inits them with their own git dirs | U-EXCL-06, U-TXN-32, I-17…I-22 (I-19 reproduces the reported case) |
| 3. A failed run leaves its registration in `.git/worktrees`, so the branch stays "in use" | Write-ahead journal + rollback (worktree remove, branch delete only if created here, staging removal); `doctor --fix` after a crash | U-RB-*, U-TXN-40…48, U-DOC-03…05, I-40…I-45 |

## Unit tests (`src/**`, run with fakes or pure inputs)

| ID | Area | File |
|---|---|---|
| U-CLI-01…16 | Flag parsing, conflicts, defaults, help/version, non-UTF-8 paths, option-like names | `src/cli.rs` |
| U-ARGS-01…03 | `git worktree add` argv: every mode, values always after `--end-of-options` | `src/git/cli.rs` |
| U-PATH-01…19, P1…P3 | Lexical destination resolution, placement (`dest-is-source`, `dest-inside-git-dir`, nested), staging naming | `src/domain/paths.rs` |
| U-EXCL-01…10, P1…P2 | Exclusion set: root `.git`, nested staging, nested worktrees, submodules, component-wise matching | `src/domain/exclusions.rs` |
| U-GIT-01…03 | Re-pointing touches only its own registration; branch deletion is compare-and-delete (`update-ref -d`); option-like revisions never reach git | `src/git/cli.rs` |
| U-REL-01…08, P1 | Relocation of the source path: recorded forms (shebang, JSON, `file://`), boundaries (`/w/src2`, `/w/src.bak`, `/other/w/src`), `/private` alias, binaries reported, symlinks | `src/domain/relocate.rs` |
| U-PLAN-01…07, P1 | Minimal clone plan (descend only into ancestors of exclusions); property: every non-excluded leaf covered exactly once | `src/domain/clone_plan.rs` |
| U-PARSE-01…20, P1…P4 | `git --version`, `worktree list -z`, dirty status, gitlinks, object ids, bool config, `ls-files -v` flags, in-progress operation markers; properties incl. "never panics" | `src/git/parse.rs` |
| U-ENV-01…05 | Child `git` env sanitization, locale, cwd/args, error mapping, `GIT_OPTIONAL_LOCKS=0` | `src/git/runner.rs` |
| U-FS-01…20 | `clonefile`/`renamex_np`/`statfs` adapter on the real APFS `$TMPDIR`; `uchg` cleanup, `flock` (in-process and cross-process), EACCES, EILSEQ, NFC/NFD, no-follow dir check, parallel relocation scan | `src/fs/apfs.rs` (macOS-only: U-FS-01…08, 12, 13, 17, 18) and `src/fs/unix.rs` (shared by Unix adapters: U-FS-09…11, 14…16, 19, 20) |
| U-RB-01…12, P1 (U-RB-10…12: a branch is deleted only while it still points at its start commit; U-RB-06: fallback removes only its own registration, never `worktree prune`) | Journal encoding, rollback order, fallbacks, aggregated failure | `src/app/rollback.rs` |
| U-TXN-01…79 | `add` transaction: order, refusals before any mutation (and their precedence), fallback, exclusions, rollback at each step, lost race, SIGINT, operation in progress, flagged index entries, source changed mid-run, journal lock, early failures, destination in submodule, non-UTF-8 destination, publish intent journaled before the rename, symlinks on the path to an exclusion cloned as links, relocation of untracked files, `.parhelignore`, detaching foreign gitfiles, permissions of created dirs, dirty submodules with reinit | `src/app/add.rs` |
| U-DOC-01…19 | `doctor`: findings, read-only by default, `--fix` via journal, foreign gitfiles, live runs (`in-progress`), foreign prunable entries ignored, crash after publish repaired (not rolled back) even when the journal lacks the destination or is lost, scoped cleanup of orphan registrations | `src/app/doctor.rs` |

## Integration tests (`tests/*.rs`, real `git`, real APFS)

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
| I-23a…i | Symlinks (relative, absolute, dangling, to dir, ignored) not followed; modes; empty dirs; xattrs; NFD names and spaces; hard links decoupled; eol attributes; mtimes; directory permissions not widened |
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
| I-46 | Rollback's fallback cleanup keeps unrelated, unreachable worktrees (unmounted disk) registered |

### Filesystems — `tests/add_filesystems.rs` (disk images)
| ID | Behavior |
|---|---|
| I-50 | Destination on another APFS volume → `cross-volume` |
| I-51a/b | Source or destination on HFS+ (no copy-on-write) → `not-cow` with `--fallback` hint |
| I-52a/b | `--fallback` → plain worktree + warning; inert on APFS |
| I-53 | Case-sensitive APFS |

### Copy-on-write evidence — `tests/add_cow.rs`
| ID | Behavior |
|---|---|
| I-54a | Tracked and ignored files share physical extents (`F_LOG2PHYS_EXT`) |
| I-54b | Writes in the destination never reach the source |
| I-54c | Cloning 128 MiB uses < 8 MiB of free space (isolated volume) |

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
| I-80a…f | Healthy, volume without copy-on-write (`not-cow`), orphan staging, orphan registration (branch kept without journal), foreign gitfile (never touched), outside repo |
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
| I-95 / I-95b | `post-checkout` not run on copy-on-write (decision pinned); run by `--fallback` |
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
| I-131 | Full volume → real error, no trace |
| I-132 | Paths near `PATH_MAX`: full clone or clean `clone-failed` |
| I-133 | Non-UTF-8 destination → `invalid-path` (APFS rejects it) |
| I-134 | NFC destination when the NFD name exists → `dest-exists` |
| I-135 / I-136 | `uchg` files are cloned with their flag; rollback still removes staging |
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

## Not covered (needs infrastructure)

- A matrix of real git versions (only the installed one runs; the minimum is checked with a fake `git`).
- Mutation testing (`cargo mutants`) once the implementation exists, to measure how many regressions the suite catches.
