# parhel

New Git worktrees in under a second, with `node_modules`, `target/` and the rest
of your build output already in place.

[![CI](https://github.com/myrk-kale/parhel/actions/workflows/ci.yml/badge.svg)](https://github.com/myrk-kale/parhel/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/myrk-kale/parhel?sort=semver)](https://github.com/myrk-kale/parhel/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform: macOS](https://img.shields.io/badge/platform-macOS%20(APFS)-lightgrey)

```sh
git parhel add ../my-feature --new-branch feature/my-feature
```

A fresh `git worktree add` gives you the tracked files and nothing else, so the
first thing you do in it is reinstall dependencies and rebuild from scratch.
`git parhel add` creates the same kind of worktree, but it copies your current
working tree with the filesystem's copy-on-write cloning. Ignored directories
such as `node_modules/`, `target/` or `.venv/` arrive with it, and they take no
extra disk space until one of the two copies changes a file.

The result is an ordinary Git worktree. `git worktree list`, `move`, `lock` and
`remove` all work on it.

Measured on macOS with a project holding 78 MB of `node_modules` (20,000 files)
and a 300 MB build artifact:

| | Time | Extra disk used |
|---|---|---|
| `git worktree add`, then `cp -R node_modules target` | 6.9 s | 383 MiB |
| `git parhel add` | 0.5 s | about 8 MiB |

parhel currently runs on macOS (APFS). Linux is next, then Windows; see
[Platform support](#platform-support).

parhel is at version 0.x. More than 430 tests run it against real git and real
APFS volumes, but command-line details may still change before 1.0. If
something looks wrong, please
[open an issue](https://github.com/myrk-kale/parhel/issues/new/choose).

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Platform support](#platform-support)
- [How it works](#how-it-works)
- [What happens when things go wrong](#what-happens-when-things-go-wrong)
- [Limitations](#limitations)
- [FAQ](#faq)
- [Error codes](#error-codes)
- [Uninstall](#uninstall)
- [Development](#development)
- [Credits](#credits)

## Install

You need macOS on an APFS volume (the default since macOS 10.13) and git 2.29
or newer. The git that ships with Xcode's command line tools works.

With Homebrew:

```sh
brew install myrk-kale/tap/parhel
```

Without Homebrew, the install script puts the binary in `~/.local/bin` and
checks its SHA-256 before installing:

```sh
curl -fsSL https://raw.githubusercontent.com/myrk-kale/parhel/main/install.sh | sh
```

Set `PARHEL_VERSION=0.1.0` to install a specific version, or
`PARHEL_INSTALL_DIR` to put the binary somewhere else.

From source, with Rust 1.85 or newer:

```sh
cargo install --git https://github.com/myrk-kale/parhel
```

Then check that git can find it:

```sh
git parhel --version
```

Git runs any executable called `git-<name>` on your `PATH` as `git <name>`, so
installing the `git-parhel` binary is all the setup there is.

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

To get rid of a worktree, use git itself:

```sh
git worktree remove ../my-app-login
```

## Commands

### `git parhel add [options] <destination> [ref]`

Creates a worktree at `<destination>` and checks out `[ref]`, which defaults to
the commit your current worktree is on. The parent directory of
`<destination>` has to exist already.

| Option | Meaning |
|---|---|
| `--new-branch <name>` | Create a new branch and check it out |
| `--branch <name>` | Check out an existing branch |
| `--detach` | Check out `[ref]` with a detached HEAD |
| (none of the three) | Same as `git worktree add`: use a branch named after the destination folder, creating it if needed |
| `[ref]` | Any commit, tag or branch. If it differs from your current commit, parhel rewrites only the tracked files that differ and keeps sharing the rest |
| `--from <path>` | Which worktree to copy, given as any path inside it. Defaults to the current directory |
| `--allow-dirty` | Copy uncommitted changes too. Staged changes arrive unstaged. Without this flag, a worktree with uncommitted changes is refused |
| `--submodules=refuse\|reinit` | By default, initialized submodules are refused. `reinit` gives each submodule in the new worktree its own git directory, cloned from your local copy, so it works offline |
| `--fallback` | If copy-on-write is not possible (unsupported filesystem, or source and destination on different volumes), create a regular worktree instead of failing |

Besides tracked files, the copy brings along ignored files. Untracked files
that are not ignored only come along with `--allow-dirty`, because without it a
worktree that has them is refused.

### `git parhel doctor [--fix] [--from <path>]`

Reports problems in the repository and its environment:

- the worktree is on a filesystem parhel cannot clone on;
- an earlier run crashed and left a hidden `.parhel-staging-*` directory, or a
  worktree registration inside one;
- a `.git` file inside your tree belongs to a different repository, so a clone
  would carry that repository's state along.

`--fix` cleans up after crashed runs. It reads the journal the run left behind
and removes only what that run created. If the crash happened after the new
worktree was already in place, `--fix` repairs it and leaves it alone. Runs that
are still going hold a lock on their journal, and `doctor` skips them.

### Help

`git parhel -h` prints a short summary. `git parhel --help` and
`man git-parhel` open the manual page, which the Homebrew formula and the
install script both install.

## Platform support

| Platform | Status | Mechanism |
|---|---|---|
| macOS, APFS | Supported | `clonefile(2)` |
| Linux, Btrfs or XFS | Planned next | reflink copies (`FICLONE`) |
| Windows | Planned after Linux | |

On a filesystem without copy-on-write support, or when the source and the
destination are on different volumes, `add` stops with `not-apfs` or
`cross-volume` before touching anything. Pass `--fallback` to get a regular
`git worktree add` in those cases.

The code keeps filesystem access behind one interface (`CowFs` in
`src/ports.rs`), so adding a platform means writing a new implementation of it
and running the same test suite against it.

## How it works

On macOS, `add` goes through five steps:

1. It checks everything it can before making changes: the git version, the
   state of the source worktree, the destination, and whether both are on the
   same APFS volume. Any refusal happens here, so a refused command leaves no
   trace.
2. It registers a new worktree with `git worktree add --no-checkout`, inside a
   hidden staging directory next to the destination.
3. It clones the source into the staging directory with `clonefile(2)`. A whole
   directory takes a single call; parhel only walks into a directory when
   something inside it must be left out: the root `.git`, other worktrees
   nested in the source and, with `reinit`, submodules.
4. It builds the index from the source commit. If `[ref]` points somewhere else,
   a two-way checkout rewrites the files that differ.
5. It moves the finished worktree to the destination with
   `renamex_np(RENAME_EXCL)`, which fails instead of overwriting anything, and
   then runs `git worktree repair`.

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

An APFS clone shares data blocks with the original. When either side writes to
a block, APFS copies that block first, so from the moment the clone exists the
two worktrees change independently.

## What happens when things go wrong

parhel started as a reimplementation of
[git-worktree-apfs](https://github.com/dannolan/git-worktree-apfs), and the
test suite began with three problems found in that tool:

| Situation | git-worktree-apfs | parhel |
|---|---|---|
| Two runs target the same destination | The run that loses deletes the winner's worktree, uncommitted work included | Only an exclusive rename ever writes the destination, so an existing destination is never modified or deleted, even one created a millisecond earlier |
| The source has submodules | The copied submodules still point at the source's git directories, so a checkout in the copy changes the source | Refused by default. `--submodules=reinit` gives the copies their own git directories |
| A run fails partway | The temporary worktree stays registered, and its branch shows as "already used by worktree" | The run keeps a journal and undoes its own changes: the registration, the branch it created (never one that already existed) and the staging directory |

Other cases the tests cover:

- Ctrl-C, `SIGTERM` and `SIGHUP` roll the run back, and the process exits with
  `128 + signal`. More signals during the rollback do not stop it.
- After a hard crash (`kill -9`, power loss), `git parhel doctor --fix`
  finishes or undoes the run, using its journal.
- If the source's HEAD or files change while the copy is being made, the run
  stops with `source-changed` rather than leave you with a mismatched worktree.
- A merge, rebase, cherry-pick, revert or `am` in progress in the source is
  refused.
- Files marked `assume-unchanged` or `skip-worktree` count as uncommitted
  changes, because `git status` does not check them.
- Branch and ref names that start with `-` are rejected, and parhel passes all
  of your values to git after `--end-of-options`, so none of them can be read as
  an option.
- Symlinks are copied as symlinks and never followed.

## Limitations

- Copy-on-write only works on macOS with APFS for now. Elsewhere, `--fallback`
  creates a regular worktree. See [Platform support](#platform-support).
- Source and destination must be on the same volume, because clones cannot
  cross volumes. `--fallback` covers this case too.
- Worktrees that use sparse-checkout are refused.
- Hooks such as `post-checkout` do not run, the same as with
  `git worktree add --no-checkout`. With `--fallback`, git runs them as usual.
- Destinations inside a submodule's working tree are refused.

## FAQ

**Does the new worktree depend on the source?**
No. Once it exists, the two worktrees share the repository and nothing else,
as with `git worktree add`. You can delete or rebuild the source without
affecting the copy.

**Why do Finder and `du` report the full size?**
They count each file's logical size. Cloned blocks are stored once until one
side changes them, so free space (`df -h`) shows what was actually used.

**How do I remove a worktree?**
Run `git worktree remove <path>`. Ignored files do not block it.

**Can I move, lock or prune it?**
Yes, with the usual `git worktree move`, `lock`, `unlock`, `prune` and `repair`.

**What about Git LFS, filters and line endings?**
When `[ref]` differs from the source commit, the files that change go through
git's normal checkout, so clean/smudge filters and `.gitattributes` apply. When
`[ref]` is the same commit, files are cloned exactly as they are in the source,
which means they are already smudged.

**macOS refuses to run the binary I downloaded from the releases page.**
Gatekeeper quarantines files downloaded with a browser. Homebrew and the
install script do not have this problem. For a manual download, run
`xattr -d com.apple.quarantine ./git-parhel`.

**Which git does it use?**
Whatever `git` is first on your `PATH`, version 2.29 or newer.

## Error codes

Errors look like `error[<code>]: <message>`. The codes will not change between
releases, so scripts can rely on them. The exit status is 1 for errors, 2 for
usage errors and `128 + signal` when a signal interrupted the run.

| Code | Meaning and what to do |
|---|---|
| `dirty-source` | Commit or stash your changes, or pass `--allow-dirty` |
| `dest-exists` | Choose another destination. parhel never overwrites |
| `dest-parent-missing` | Create the parent directory first |
| `not-apfs` / `cross-volume` | Use a destination on the same APFS volume, or pass `--fallback` |
| `submodules-present` | Pass `--submodules=reinit` |
| `operation-in-progress` | Finish or abort the merge, rebase or cherry-pick first |
| `source-changed` | Something changed the source during the run. Run the command again |
| `checkout-conflict` | With `--allow-dirty`, your local changes conflict with `[ref]` |
| `sparse-checkout` | Not supported yet |
| `git-failed` | Git's own message follows, for example when a branch already exists |
| `git-too-old` | Upgrade git to 2.29 or newer |
| `rollback-failed` | Run `git parhel doctor --fix`. The message also lists the manual steps |

Less common codes: `not-a-repo`, `bare-repo`, `unborn-head`, `inside-git-dir`,
`dest-is-source`, `dest-inside-git-dir`, `dest-inside-submodule`,
`invalid-path`, `permission-denied`, `clone-failed`, `interrupted`, `usage`,
`io`.

## Uninstall

```sh
brew uninstall parhel                 # Homebrew
rm ~/.local/bin/git-parhel ~/.local/share/man/man1/git-parhel*.1   # install script
cargo uninstall parhel                # cargo
```

Worktrees you already created keep working. They are regular git worktrees.

## Development

```sh
cargo test --features failpoints     # full suite: unit, property and integration tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

The integration tests drive real git on real APFS volumes. They create
throwaway disk images with `hdiutil`, which needs no root. To check that files
were cloned rather than copied, they compare physical block addresses, and they
use failpoints that work across processes to reproduce races and crashes on
demand. [docs/test-catalog.md](docs/test-catalog.md) lists every behavior the
tests cover, and [CONTRIBUTING.md](CONTRIBUTING.md) explains how to get
started.

## Credits

The idea comes from
[git-worktree-apfs](https://github.com/dannolan/git-worktree-apfs). parhel is a
separate implementation in Rust, built around a transaction that can be rolled
back.

## License

[MIT](LICENSE)
