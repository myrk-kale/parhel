# parhel

**Git worktrees in milliseconds, with your `node_modules` and `target/` already there.**

[![CI](https://github.com/myrk-kale/parhel/actions/workflows/ci.yml/badge.svg)](https://github.com/myrk-kale/parhel/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/myrk-kale/parhel?sort=semver)](https://github.com/myrk-kale/parhel/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform: macOS](https://img.shields.io/badge/platform-macOS%20(APFS)-lightgrey)

```sh
git parhel add ../my-feature --new-branch feature/my-feature
```

`git parhel add` creates a **regular Git worktree** by cloning your current
worktree with APFS copy-on-write (`clonefile(2)`) instead of checking files out.
Ignored heavyweights (`node_modules/`, `target/`, `.venv/`, build caches) come
along instantly and use **no extra disk space** until one side modifies them.
You skip the reinstall and the cold rebuild that a fresh worktree needs.

| Project with 78 MB `node_modules` (20k files) + 300 MB build artifact | Time | Extra disk |
|---|---|---|
| `git worktree add` + `cp -R node_modules target` | 6.9 s | 383 MiB |
| `git parhel add` | **0.5 s** | **~8 MiB** |

> **Status: 0.x, experimental.** The behavior is covered by 430+ tests against real
> git and real APFS volumes, but the CLI may still change before 1.0. Please
> [report anything odd](https://github.com/myrk-kale/parhel/issues/new/choose).

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [How it works](#how-it-works)
- [Safety guarantees](#safety-guarantees)
- [Limitations](#limitations)
- [FAQ](#faq)
- [Error codes](#error-codes)
- [Uninstall](#uninstall)
- [Development](#development)
- [Credits](#credits)

## Install

Requirements: macOS with an APFS volume (the default since macOS 10.13) and
git 2.29 or newer (Apple's bundled git is fine).

**Homebrew** (recommended):

```sh
brew install myrk-kale/tap/parhel
```

**Install script** (no Homebrew; installs to `~/.local/bin`, verifies the SHA-256):

```sh
curl -fsSL https://raw.githubusercontent.com/myrk-kale/parhel/main/install.sh | sh
```

Set `PARHEL_VERSION=0.1.0` to pin a version or `PARHEL_INSTALL_DIR` to change the
location.

**From source** (Rust 1.85+):

```sh
cargo install --git https://github.com/myrk-kale/parhel
```

Check it works:

```sh
git parhel --version
```

Git finds any `git-<name>` executable on your `PATH`, so the `git-parhel` binary
becomes the `git parhel` command. No git configuration is needed.

## Quick start

```sh
cd ~/code/my-app                       # any path inside your worktree

# New branch, new worktree, dependencies and build output included:
git parhel add ../my-app-login --new-branch feature/login

cd ../my-app-login
npm test                               # no `npm install` needed
```

Other common forms:

```sh
git parhel add ../hotfix --branch release/1.2      # existing branch
git parhel add ../review --detach v1.4.0           # detached at a tag/commit
git parhel add ../spike                            # new branch named "spike"
git parhel add ../older --new-branch old-ui 3f2a9c1   # start from another commit
```

When you're done, use plain git:

```sh
git worktree remove ../my-app-login
```

## Commands

### `git parhel add [options] <destination> [ref]`

Creates a worktree at `<destination>` (its parent directory must exist) and checks
out `[ref]` (default: the source HEAD).

| Option | Meaning |
|---|---|
| `--new-branch <name>` | Create and check out a new branch |
| `--branch <name>` | Check out an existing branch |
| `--detach` | Detached HEAD at `[ref]` |
| *(none of the above)* | Like `git worktree add`: a branch named after the destination's basename (created if missing) |
| `[ref]` | Any commit-ish. It may differ from the source HEAD: only the tracked files that differ are rewritten, everything else stays shared |
| `--from <path>` | The source worktree, as any path inside it (default: current directory) |
| `--allow-dirty` | Carry uncommitted changes into the new worktree (staged changes arrive unstaged). Without it, a dirty source is refused |
| `--submodules=refuse\|reinit` | Initialized submodules are refused by default. `reinit` gives each submodule its own git dir in the new worktree, cloned from the source's copy (works offline) |
| `--fallback` | When copy-on-write is impossible (not APFS, different volumes), create a regular worktree instead of failing |

Only **ignored** files are cloned along with tracked ones. A clean source is
required unless you pass `--allow-dirty`, so untracked files never leak in by
surprise.

### `git parhel doctor [--fix] [--from <path>]`

Checks the repository and the environment:

- whether the source volume is APFS;
- leftovers from a crashed run (hidden `.parhel-staging-*` directories and the
  worktree registrations inside them);
- `.git` files of *other* repositories nested in your tree, which a clone would
  carry along.

`--fix` undoes leftovers using the crashed run's journal: only what that run
created is removed, and a worktree that was already published is repaired, not
deleted. Runs still in progress are detected and never touched.

### Help

`git parhel -h` prints a summary; `git parhel --help` (and `man git-parhel`) opens
the manual when installed with Homebrew or the install script.

## How it works

```
 source worktree                      .parhel-staging-xxxx/ (next to the destination)
 ├── .git            (excluded)       └── my-feature/
 ├── src/            ─ clonefile ─▶       ├── .git   ← git worktree add --no-checkout
 ├── node_modules/   ─ clonefile ─▶       ├── src/
 └── target/         ─ clonefile ─▶       ├── node_modules/
                                          └── target/
                                                │  index ← read-tree (+ two-way checkout if [ref] differs)
                                                ▼
                     renamex_np(RENAME_EXCL) ─▶ ../my-feature   then `git worktree repair`
```

1. **Validate everything first:** git version, repository state, destination,
   volume type. Any refusal happens before anything is created.
2. **Register** a worktree with `git worktree add --no-checkout` inside a private
   staging directory on the destination's volume.
3. **Clone** the source tree into it with `clonefile(2)`, skipping the root
   `.git`, other worktrees nested in the source and (with `reinit`) submodules.
   Whole directories are cloned in one syscall.
4. **Build the index** from the source HEAD; if `[ref]` differs, a two-way
   checkout rewrites only the files that differ.
5. **Publish** with an exclusive atomic rename to the destination, then let git
   repair the registration.

APFS clones share data blocks; a block is copied only when one side writes to it.
Your source and the new worktree are fully independent from that moment on.

## Safety guarantees

parhel is a reimplementation of the idea behind
[git-worktree-apfs](https://github.com/dannolan/git-worktree-apfs). Its failure
modes were the starting point of the test suite:

| Failure | git-worktree-apfs | parhel |
|---|---|---|
| Two runs target the same destination | The loser deletes the winner's worktree, including uncommitted work | The destination is only ever written by an exclusive rename. An existing destination, even one created a millisecond earlier, is never modified or deleted |
| Source has submodules | Their `.git` files keep pointing at the source, so a checkout in the copy changes the source | Refused by default; `--submodules=reinit` gives them their own git dirs |
| A run fails midway | The temporary worktree stays registered, and its branch reports "already used by worktree" | Write-ahead journal plus rollback: the registration, the branch created by this run (never a pre-existing one) and the staging dir are removed |

Also:

- Ctrl-C, `SIGTERM` and `SIGHUP` roll back cleanly (exit status `128 + signal`).
  Signals during the rollback do not interrupt it.
- After a crash (`kill -9`, power loss), `git parhel doctor --fix` finishes or
  undoes the run from its journal.
- If the source HEAD or its files change while the worktree is being created,
  the run aborts with `source-changed` instead of producing an inconsistent copy.
- A merge, rebase, cherry-pick, revert or `am` in progress in the source is refused.
- Files flagged `assume-unchanged` or `skip-worktree` count as local changes,
  since `git status` cannot vouch for them.
- Values you pass are never interpreted as git options: branch and ref names
  starting with `-` are rejected, and everything is passed after `--end-of-options`.
- Symlinks are cloned as symlinks and never followed.

## Limitations

- **macOS / APFS only.** Elsewhere, `--fallback` gives you a regular worktree.
  Linux reflink support (Btrfs, XFS) is planned.
- **Source and destination must be on the same volume.** `clonefile` cannot cross
  volumes; use `--fallback` to get a regular worktree instead.
- **Sparse-checkout sources** are refused.
- **Hooks** (`post-checkout`) are not run, matching `git worktree add --no-checkout`.
  With `--fallback`, git runs them as usual.
- **Destinations inside a submodule's working tree** are refused.

## FAQ

**Does the new worktree depend on the source?**
No. After creation they are two independent worktrees sharing only the repository,
exactly like `git worktree add`. Deleting or rebuilding the source does not affect
the copy.

**Why does Finder / `du` show the full size?**
Those tools count logical sizes. Cloned blocks are shared until written, so free
disk space (`df -h`) is the honest measure.

**How do I remove a worktree?**
`git worktree remove <path>`. Ignored files do not block it.

**Can I move, lock or prune it?**
Yes. It is a normal worktree: `git worktree move`, `lock`, `unlock`, `prune` and
`repair` all work.

**What about Git LFS, filters and line endings?**
Files go through git's normal checkout path when `[ref]` differs, so clean/smudge
filters and `.gitattributes` apply as usual. With the same ref, files are cloned
as they are in your source (already smudged).

**I downloaded the tarball in a browser and macOS refuses to run it.**
Browser downloads are quarantined by Gatekeeper. Homebrew and the install script
are not affected. For a manual download:
`xattr -d com.apple.quarantine ./git-parhel`.

**Which git does it use?**
The `git` on your `PATH`, the same one you use. It needs 2.29 or newer.

## Error codes

Errors are printed as `error[<code>]: <message>`. Codes are stable, so you can
script against them. Exit status is 1 for errors, 2 for usage errors and
`128 + signal` when interrupted.

| Code | Meaning / what to do |
|---|---|
| `dirty-source` | Commit or stash your changes, or pass `--allow-dirty` |
| `dest-exists` | Pick another destination; parhel never overwrites |
| `dest-parent-missing` | Create the parent directory first |
| `not-apfs` / `cross-volume` | Use a path on the same APFS volume, or `--fallback` |
| `submodules-present` | Pass `--submodules=reinit` |
| `operation-in-progress` | Finish or abort the merge/rebase/cherry-pick first |
| `source-changed` | Something modified the source mid-run; run it again |
| `checkout-conflict` | With `--allow-dirty`, your local changes clash with `[ref]` |
| `sparse-checkout` | Not supported yet |
| `git-failed` | git's own message follows (e.g. branch already exists) |
| `git-too-old` | Upgrade git to 2.29+ |
| `rollback-failed` | Run `git parhel doctor --fix`; the message lists manual steps |

Other codes: `not-a-repo`, `bare-repo`, `unborn-head`, `inside-git-dir`,
`dest-is-source`, `dest-inside-git-dir`, `dest-inside-submodule`, `invalid-path`,
`permission-denied`, `clone-failed`, `interrupted`, `usage`, `io`.

## Uninstall

```sh
brew uninstall parhel                 # Homebrew
rm ~/.local/bin/git-parhel ~/.local/share/man/man1/git-parhel*.1   # install script
cargo uninstall parhel                # cargo
```

Worktrees you created stay valid. They are regular git worktrees.

## Development

```sh
cargo test --features failpoints     # full suite: unit, property and integration tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

The integration tests run real git against real APFS volumes. They create
throwaway disk images with `hdiutil`, so no root is needed. They also prove
copy-on-write physically by comparing block addresses, and use cross-process
failpoints to reproduce races and crashes deterministically. See
[docs/test-catalog.md](docs/test-catalog.md) for every covered behavior and
[CONTRIBUTING.md](CONTRIBUTING.md) to get started.

## Credits

The idea comes from
[git-worktree-apfs](https://github.com/dannolan/git-worktree-apfs). parhel is an
independent Rust implementation with a transactional design.

## License

[MIT](LICENSE)
