<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/myrk-kale/parhel/main/assets/logo-dark.png">
    <img alt="parhel: copy-on-write git worktrees" src="https://raw.githubusercontent.com/myrk-kale/parhel/main/assets/logo-light.png" width="420">
  </picture>
</p>

New Git worktrees in under a second, with `node_modules`, `target/` and the rest
of your build output already in place.

[![CI](https://github.com/myrk-kale/parhel/actions/workflows/ci.yml/badge.svg)](https://github.com/myrk-kale/parhel/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/myrk-kale/parhel?sort=semver)](https://github.com/myrk-kale/parhel/releases)
[![crates.io](https://img.shields.io/crates/v/parhel.svg)](https://crates.io/crates/parhel)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform: macOS and Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey)

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

parhel runs on macOS (APFS) and on Linux (Btrfs, XFS with reflink and other
filesystems that can reflink). Windows is planned; see
[Platform support](#platform-support).

parhel is at version 0.x. More than 430 tests run it against real git and real
APFS, Btrfs, XFS and ext4 volumes, but command-line details may still change
before 1.0. If
something looks wrong, please
[open an issue](https://github.com/myrk-kale/parhel/issues/new/choose).

## Contents

- [Quick start](#quick-start)
- [Install](#install)
  - [macOS](#macos)
  - [Linux](#linux)
  - [Check that git can find it](#check-that-git-can-find-it)
  - [Verify a download](#verify-a-download)
  - [Skill for AI coding agents](#skill-for-ai-coding-agents)
- [Read this before you rely on it](#read-this-before-you-rely-on-it)
- [Commands](#commands)
- [Platform support](#platform-support)
- [How it works](#how-it-works)
- [What parhel writes, and where](#what-parhel-writes-and-where)
- [What happens when things go wrong](#what-happens-when-things-go-wrong)
- [Security and privacy](#security-and-privacy)
- [Limitations](#limitations)
- [FAQ](#faq)
- [Error codes](#error-codes)
- [Uninstall](#uninstall)
- [Development](#development)
- [Credits](#credits)

## Quick start

Install it:

```sh
# macOS
brew install myrk-kale/tap/parhel

# Linux, any distribution
curl -fsSL https://raw.githubusercontent.com/myrk-kale/parhel/main/install.sh | sh
```

On Debian, Ubuntu, Fedora and similar distributions you can install a `.deb` or
`.rpm` package instead; see [Linux](#linux). Then create your first worktree:

```sh
cd ~/code/my-app                       # any path inside your worktree

# New branch, new worktree, dependencies and build output included:
git parhel add ../my-app-login --new-branch feature/login

cd ../my-app-login
npm test                               # no `npm install` needed
```

When you are done with it, remove the worktree with git itself:

```sh
git worktree remove ../my-app-login
```

Before you rely on parhel, read
[Read this before you rely on it](#read-this-before-you-rely-on-it): a copy
shares your `.env` files with the original, and removing a worktree deletes its
ignored files without asking. The other forms of `add` (existing branch,
detached, another start commit) are under [Commands](#commands).

## Install

You need git 2.29 or newer and one of these:

- macOS on an APFS volume (the default since macOS 10.13). The git that ships
  with Xcode's command line tools works.
- Linux on a filesystem that can reflink: Btrfs or XFS created with reflink
  support (the default since xfsprogs 5.1). bcachefs and ZFS 2.2 and newer
  should work too, but parhel is not tested on them. On any other filesystem
  (ext4, tmpfs, NFS...), `--copy` and `--fallback` still work; see
  [Platform support](#platform-support).

### macOS

With Homebrew:

```sh
brew install myrk-kale/tap/parhel
```

Without Homebrew, the install script picks the release archive for your system,
puts the binary in `~/.local/bin` and checks its SHA-256 before installing:

```sh
curl -fsSL https://raw.githubusercontent.com/myrk-kale/parhel/main/install.sh | sh
```

Set `PARHEL_VERSION=0.3.0` to install a specific version, or
`PARHEL_INSTALL_DIR` to put the binary somewhere else.

With Cargo (Rust 1.85 or newer), from crates.io or straight from the repository:

```sh
cargo install parhel
cargo install --git https://github.com/myrk-kale/parhel
```

Or download the archive from the
[releases page](https://github.com/myrk-kale/parhel/releases), check it against
its `.sha256` file and unpack it. It holds `git-parhel`, the man pages, the
license and the changelog:

| System | Archive |
| --- | --- |
| macOS, Apple silicon and Intel | `parhel-v<version>-universal-apple-darwin.tar.gz` |

### Linux

The Linux binaries are statically linked (musl): they run on any distribution,
glibc or not, and need nothing but `git`. Releases ship them as packages for
Debian and Fedora families and as plain archives, for x86_64 and aarch64.

The package file names contain the version, so there is no fixed
`latest/download` link. This looks up the latest release:

```sh
version=$(curl -fsSL https://api.github.com/repos/myrk-kale/parhel/releases/latest \
  | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p')
base="https://github.com/myrk-kale/parhel/releases/download/v${version}"
```

**Debian, Ubuntu and derivatives** (`.deb`), for amd64 and arm64:

```sh
deb="parhel_${version}_$(dpkg --print-architecture).deb"
curl -fsSLO "${base}/${deb}"
sudo apt install "./${deb}"
```

**Fedora, RHEL, openSUSE and derivatives** (`.rpm`), for x86_64 and aarch64.
`dnf` installs from a URL directly:

```sh
sudo dnf install "${base}/parhel-${version}-1.$(uname -m).rpm"
```

`zypper` installs from a URL too, but it refuses an unsigned package unless you
pass `--no-gpg-checks`: the packages are not GPG-signed (see
[Verify a download](#verify-a-download)).

```sh
sudo zypper --no-gpg-checks install "${base}/parhel-${version}-1.$(uname -m).rpm"
```

The packages install `/usr/bin/git-parhel` and the man pages. They require `git`
and recommend `btrfs-progs`, which a planned `setup` command will use to prepare
a Btrfs volume; parhel does not call it today. Packages are attached to the
releases after 0.3.0. A signed apt or dnf repository, so that
`apt install parhel` works without downloading anything and the package manager
updates parhel for you, does not exist yet. For now, upgrade by installing the
newer package.

**Homebrew**, with [Homebrew on Linux](https://docs.brew.sh/Homebrew-on-Linux):

```sh
brew install myrk-kale/tap/parhel
```

**Install script**, for any Linux on x86_64 or aarch64. It puts the binary in
`~/.local/bin` and checks the archive's SHA-256 before installing:

```sh
curl -fsSL https://raw.githubusercontent.com/myrk-kale/parhel/main/install.sh | sh
```

Set `PARHEL_VERSION=0.3.0` to install a specific version, or
`PARHEL_INSTALL_DIR` to put the binary somewhere else.

**Cargo** (Rust 1.85 or newer):

```sh
cargo install parhel
cargo install --git https://github.com/myrk-kale/parhel
```

**Archives**, from the
[releases page](https://github.com/myrk-kale/parhel/releases). Check one against
its `.sha256` file and unpack it. It holds `git-parhel`, the man pages, the
license and the changelog:

| System | Archive |
| --- | --- |
| Linux, x86_64 | `parhel-v<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux, aarch64 | `parhel-v<version>-aarch64-unknown-linux-musl.tar.gz` |

**Which filesystem?** Copy-on-write works on Btrfs and on XFS with reflink,
which is the default for XFS created with xfsprogs 5.1 or newer. Check with
`git parhel doctor`: it reports `not-cow` for a worktree on a filesystem that
cannot reflink. On ext4, tmpfs, NFS and the like, `git parhel add` stops with
`not-cow` until you pass `--copy` (every file copied in full, slower and larger,
but the same worktree) or `--fallback` (a plain `git worktree add`, without the
ignored files). See [Platform support](#platform-support).

### Check that git can find it

```sh
git parhel --version
```

Git runs any executable called `git-<name>` on your `PATH` as `git <name>`, so
installing the `git-parhel` binary is all the setup there is.

### Verify a download

Release binaries, archives and packages carry a signed build provenance
attestation, so you can check that a download was built by this repository's
release workflow from a given commit:

```sh
gh attestation verify parhel-v0.3.0-x86_64-unknown-linux-musl.tar.gz -R myrk-kale/parhel
gh attestation verify parhel_0.3.0_amd64.deb -R myrk-kale/parhel
```

Every file on the release page also has a `.sha256` file: `sha256sum -c
<file>.sha256` (`shasum -a 256 -c` on macOS). The `.deb` and `.rpm` files are not
signed with a GPG key, which is also why there is no package repository yet.

### Skill for AI coding agents

If an agent (Claude Code, Codex, Cursor and others) creates worktrees for you,
install the parhel skill. It tells the agent when parhel fits, and walks it
through the steps in
[Read this before you rely on it](#read-this-before-you-rely-on-it): keeping
`.env` files that reach shared services out of the copy, checking ignored files
before removing a worktree, and recovering with `doctor` instead of by hand.

```sh
npx skills add myrk-kale/parhel          # this project only
npx skills add myrk-kale/parhel -g       # every project
```

It uses the [skills](https://github.com/vercel-labs/skills) CLI, which asks
which agents to install it for. The skill itself is
[skills/parhel/SKILL.md](skills/parhel/SKILL.md).

## Read this before you rely on it

A copy shares more with your project than its files suggest.

- **It talks to the same services.** Your `.env` files are copied too, so the
  worktree reaches the same database, bucket or queue as the original. A
  migration or a cleanup script run there changes the real thing, and removing
  the worktree does not undo it. If that is not what you want, add the files
  to `.parhelignore` and give each worktree its own settings:

  ```sh
  printf '.env\n.env.*\n' >> .parhelignore
  ```

- **Removing a worktree deletes its ignored files without asking.**
  `git worktree remove` checks only tracked and untracked files. A local
  database, a hand-edited config or results you never committed are deleted
  even though `git status` was clean. Before removing, look at what is there:

  ```sh
  git -C ../my-app-login status --ignored --short
  ```

- **Copied paths are rewritten.** Untracked text files that named your
  project's path now name the copy's (see
  [Copied files that point at the source](#copied-files-that-point-at-the-source)).
  When a tool rejects a rewritten file, rebuild it in the worktree, or use
  `--no-relocate`.
- **All worktrees share one repository.** parhel changes only its own worktree
  registration and the branch it created, and the
  [blast-radius tests](docs/test-catalog.md) check that on every release. Still,
  push or back up work you cannot lose, as you would before any tool that
  writes to `.git`.

## Commands

### `git parhel add [options] <destination> [ref]`

Creates a worktree at `<destination>` and checks out `[ref]`, which defaults to
the commit your current worktree is on. The parent directory of
`<destination>` has to exist already.

Common forms:

```sh
git parhel add ../my-app-login --new-branch feature/login   # new branch
git parhel add ../hotfix --branch release/1.2      # existing branch
git parhel add ../review --detach v1.4.0           # detached at a tag/commit
git parhel add ../spike                            # new branch named "spike"
git parhel add ../older --new-branch old-ui 3f2a9c1   # start from another commit
```

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
| `--fallback` | If copy-on-write is not possible (unsupported filesystem, or source and destination on different volumes), create a regular worktree instead of failing. The ignored files are not copied |
| `--copy` | Copy every file in full instead of cloning it. Slower and uses disk space, but works on any filesystem and across volumes, and brings the ignored files along. Cannot be combined with `--fallback` |
| `--no-relocate` | Copy untracked files exactly as they are, without rewriting paths that point at the source (see [Copied files that point at the source](#copied-files-that-point-at-the-source)) |

Besides tracked files, the copy brings along ignored files. Untracked files
that are not ignored only come along with `--allow-dirty`, because without it a
worktree that has them is refused.

#### Copied files that point at the source

Tools often write the absolute path of the folder they ran in: script shebangs
in a Python virtualenv, generated configs, caches, symlinks made by `npm link`.
Copied as they are, those files would keep pointing at the source. Running the
copy's `.venv/bin/pip`, for example, would install packages into the source's
environment.

parhel treats the copy as if the folder had been moved. After cloning, it looks
through the untracked files of the new worktree and replaces the source's path
with the destination's in text files up to 1 MiB, and re-points absolute
symlinks that lead into the source. It does this the same way for every
language and tool. It only replaces whole paths (`/code/app` is not touched
inside `/code/app2`), never edits tracked files, and only the rewritten files
stop sharing disk blocks with the source. The command prints how many files it
relocated.

Binaries are never edited, because changing the length of a path inside one
would corrupt it. If a binary mentions the source, `add` names it in a warning.
Files larger than 1 MiB are not examined. Pass `--no-relocate` to skip this step.

A copied `.git` file that links a folder to another repository's git data (for
example a linked worktree of another project sitting inside `node_modules/`)
would let the copy change that repository's HEAD and index. parhel removes such
`.git` files from the copy, always, and lists the folders in a warning.

#### Leaving things out: `.parhelignore`

Some environments cannot be relocated, typically ones whose binaries embed their
own location (conda environments are the usual example). List them in a
`.parhelignore` file at the root of the repository, with `.gitignore` syntax, and
they are not copied at all:

```gitignore
# rebuilt per worktree
.conda/
```

Only untracked paths are affected; tracked files always come from git.

### `git parhel doctor [--fix] [--from <path>]`

Reports problems in the repository and its environment:

- the worktree is on a filesystem without copy-on-write (`not-cow`);
- an earlier run crashed and left a hidden `.parhel-staging-*` directory, or a
  worktree registration inside one;
- a `.git` file inside your tree belongs to a different repository (`add`
  detaches such files in the copy; `doctor` points them out in the source).

`--fix` cleans up after crashed runs. It reads the journal the run left behind
and removes only what that run created. If the crash happened after the new
worktree was already in place, `--fix` repairs it and leaves it alone. Runs that
are still going hold a lock on their journal, and `doctor` skips them.

### Help

`git parhel -h` prints a short summary. `git parhel --help` and
`man git-parhel` open the manual page, which the Homebrew formula, the install
script and the `.deb` and `.rpm` packages all install.

## Platform support

| Platform | Status | Mechanism |
|---|---|---|
| macOS, APFS | Supported | `clonefile(2)` |
| Linux: Btrfs, XFS with reflink | Supported, tested | reflink copies (`ioctl(FICLONE)`) per file |
| Linux: bcachefs, ZFS 2.2+ | Expected to work, not tested | the same reflinks; found by the same probe |
| Linux: other filesystems (ext4, tmpfs, NFS...) | `--copy` or `--fallback` | full copies, or a regular worktree |
| Windows | Planned | |

parhel does not go by the filesystem's name. On Linux it asks the directory
itself, with two anonymous temporary files and a reflink between them, so a
mount that cannot reflink is detected even when its type usually can (an XFS
created without `reflink=1`, for example). When the source and the destination
are on the same filesystem, the test runs in the destination's directory, which
parhel has to write to anyway, so a read-only source directory does not matter.
If a test cannot run at all, it falls back to the name: Btrfs and bcachefs count
as copy-on-write, everything else does not.

Two Btrfs subvolumes are one filesystem, and reflinks work between them, so
`add` does not call them different volumes. A file marked NOCOW on Btrfs
(`chattr +C`, usual for databases and VM images) cannot share its blocks with the
copy-on-write file a clone creates. `add` then stops with `clone-failed`, names
the file, and suggests `--copy` or listing it in `.parhelignore`; it never
switches to a full copy by itself.

When the source or the destination is on a filesystem without copy-on-write, or
when they are on different volumes, `add` stops with `not-cow` or
`cross-volume` before touching anything. You then have two ways out:

- `--copy` copies every file in full through the same steps as a clone: the
  staging directory, the exclusive rename, the rollback and the path rewriting.
  It needs no copy-on-write and works across volumes, but it takes the time and
  the disk space of a real copy.
- `--fallback` gives you a regular `git worktree add`. It is fast and small,
  and the ignored files are not copied.

On Linux, symlinks are recreated as symlinks and never followed. FIFOs are
recreated. Sockets and device files are skipped, as git does. Hard links inside
the source become separate files in the copy. User extended attributes
(`user.*`), modes, owners where permitted, and modification times to the
nanosecond are kept, so build tools do not see every file as changed.

POSIX ACLs (`setfacl`) are copied with the files, directories included. An
entry created in a directory that has a default ACL inherits it, so parhel also
removes what the source did not have: a file the source kept private does not
become readable by a user named in the destination's default ACL. On macOS the
extended ACLs (`chmod +a`) are kept the same way. There is one limit: macOS's
`clonefile(2)` drops the ACLs of the entries inside a directory it clones, so
parhel restores them only on the entries it clones one by one; use `--copy` when
those ACLs matter.

The code keeps filesystem access behind one interface (`CowFs` in
`src/ports.rs`), so adding a platform means writing a new implementation of it
and running the same test suite against it.

## How it works

`add` goes through five steps:

1. It checks everything it can before making changes: the git version, the
   state of the source worktree, the destination, and (unless you pass
   `--copy`) whether both are on the same volume and that volume has
   copy-on-write. Any refusal happens here, so a refused command leaves no
   trace.
2. It registers a new worktree with `git worktree add --no-checkout`, inside a
   hidden staging directory next to the destination.
3. It clones the source into the staging directory. On macOS that is
   `clonefile(2)`, and a whole directory takes a single call; parhel only walks
   into a directory when something inside it must be left out: the root `.git`,
   other worktrees nested in the source and, with `reinit`, submodules. On
   Linux no call clones a directory, so parhel walks the tree and clones each
   file with `ioctl(FICLONE)`, on a few threads. With `--copy`, every file is
   copied in full instead, on both platforms.
4. It builds the index from the source commit. If `[ref]` points somewhere else,
   a two-way checkout rewrites the files that differ.
5. It moves the finished worktree to the destination with
   `renamex_np(RENAME_EXCL)` (macOS) or `renameat2(RENAME_NOREPLACE)` (Linux),
   which fail instead of overwriting anything, and then points this worktree's
   registration at its final path.

```
 source worktree                      .parhel-staging-xxxx/ (next to the destination)
 ├── .git            (excluded)       └── my-feature/
 ├── src/            ─ clone ─────▶       ├── .git   ← git worktree add --no-checkout
 ├── node_modules/   ─ clone ─────▶       ├── src/
 └── target/         ─ clone ─────▶       ├── node_modules/
                                          └── target/
                                                │  index ← read-tree (+ two-way checkout if [ref] differs)
                                                ▼
                            exclusive rename ─▶ ../my-feature   then re-point its registration
```

A clone shares data blocks with the original. When either side writes to a
block, the filesystem copies that block first, so from the moment the clone
exists the two worktrees change independently. With `--copy` nothing is shared
from the start.

## What parhel writes, and where

A new tool that runs inside your repositories should be able to say exactly what
it touches. These are all the writes `add` makes:

| Write | Where |
|---|---|
| Hidden staging folder, journal | `.parhel-staging-*` next to the destination; removed at the end |
| New worktree registration and, if requested, a new branch | `.git/worktrees/<id>` and `refs/heads/<branch>`, through `git worktree add` |
| Cloned files, rewritten paths, removed foreign `.git` files | Only inside the new worktree |
| Pointing the registration at the final path | Only that registration's `gitdir` file |

It never writes to other worktrees, never changes `.git/config` (submodule
settings are written only if they differ, and only for the new worktree's
submodules), and never rewrites another worktree's index: every git command it
runs has `GIT_OPTIONAL_LOCKS=0`, so even `git status` leaves the index alone. It
does not run `git worktree repair` or `git worktree prune`, because both also
modify other worktrees' metadata.

`doctor --fix` removes only what a crashed run's journal says it created, and
it treats the journal as untrusted data: the paths it names must lie inside the
staging folder it was found in, git only runs in the repository you started
`doctor` from, and every claim is checked against the disk. It
deletes that run's branch only if the branch still points at the commit it was
created on, and it never deletes a `.parhel-staging-*` folder that holds files
it cannot attribute to parhel.

The test suite checks this directly: in every scenario (success, refusal,
failure at each step, crash plus `doctor --fix`, relocation, submodules) it
compares every other worktree, the shared configuration, the refs and the
registrations byte for byte with their state before the run.

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
- Symlinks are copied as symlinks and never followed, including when a
  directory of the source is swapped for a symlink while the copy runs: the walk
  opens each directory without following links, so that is an error and nothing
  outside the source is copied. A file replaced by a FIFO meanwhile is refused
  instead of waited for, and Ctrl-C stops a long copy at the next entry and rolls
  back.
- Cleaning up after a failure removes only what the run created. It never runs
  `git worktree prune`, which would also unregister other worktrees whose folders
  happen to be unreachable, such as one on an unmounted disk.
- Directories parhel creates get the source's permissions, so a private source
  does not produce a world-readable copy. The worktree's root takes them last,
  once it is in place, so a read-only source root works too.
- With `--allow-dirty --submodules=reinit`, a submodule with uncommitted changes
  stops the command (`dirty-submodule`): reinit checks out the recorded commit,
  so those changes would not be in the copy.

## Security and privacy

parhel has no telemetry and makes no network requests of its own. The only
network access that can happen is git's: `--submodules=reinit` clones each
submodule from your local copy, and git contacts the submodule's remote only if
a needed commit is missing locally.

Things worth knowing before you use it:

- Secrets come along. Ignored files are copied, and that includes `.env`
  files and other local credentials. They keep their permissions, and new
  directories get the source's, but if you create a worktree in a shared or
  synced folder, those files go with it. List anything that must not be copied
  in `.parhelignore`.
- Relocation edits text in the copy. Rewriting the source path changes the
  length of a file's contents. A format that stores its own length or checksum
  (some serialized caches, lockfiles with integrity hashes) may then be rejected
  by its tool, which usually rebuilds it. Your source is never edited. Use
  `--no-relocate`, or list such paths in `.parhelignore`.
- Builds running during `add`. parhel stops if tracked or untracked files
  change while it copies, but ignored files may change freely, so a build
  writing its cache at that moment can leave a half-written cache file in the
  copy. Run `add` when nothing is building, or rebuild in the new worktree.
- The binary is not signed by Apple. Instead of an Apple signature, every
  release has a build provenance attestation (see [Install](#install)).
  Homebrew and the install script are not affected by Gatekeeper.
- Git does the git work. parhel runs the `git` on your `PATH` (2.29 or newer;
  CI also tests 2.29 and 2.39 built from source), so your git configuration and
  its safety checks such as `safe.directory` apply.

Report vulnerabilities privately: see [SECURITY.md](SECURITY.md).

## Limitations

- Copy-on-write works on APFS (macOS) and on Linux filesystems that reflink.
  Elsewhere, `--copy` copies every file and `--fallback` creates a regular
  worktree. See [Platform support](#platform-support).
- For a clone, source and destination must be on the same volume, because
  clones cannot cross volumes. `--copy` and `--fallback` cover this case too.
- Worktrees that use sparse-checkout are refused.
- Hooks such as `post-checkout` do not run, the same as with
  `git worktree add --no-checkout`. With `--fallback`, git runs them as usual.
- Destinations inside a submodule's working tree are refused.
- With `worktree.useRelativePaths` (git 2.48+), the new worktree's links are
  written as absolute paths. git accepts both forms.

## FAQ

**Does the new worktree depend on the source?**
No. Once it exists, the two worktrees share the repository and nothing else,
as with `git worktree add`. You can delete or rebuild the source without
affecting the copy.

**Why do Finder and `du` report the full size?**
They count each file's logical size. Cloned blocks are stored once until one
side changes them, so free space (`df -h`) shows what was actually used.

**My filesystem has no copy-on-write. What now?**
Pass `--copy` to get the same worktree with every file copied in full (it takes
longer and uses disk space), or `--fallback` for a plain `git worktree add`
without the ignored files.

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
| `not-cow` / `cross-volume` | Use a destination on the same volume, on a filesystem with copy-on-write, or pass `--copy` (full copy) or `--fallback` (regular worktree) |
| `submodules-present` | Pass `--submodules=reinit` |
| `dirty-submodule` | Commit or stash the changes inside the submodule, or drop `--allow-dirty` |
| `operation-in-progress` | Finish or abort the merge, rebase or cherry-pick first |
| `source-changed` | Something changed the source during the run. Run the command again |
| `checkout-conflict` | With `--allow-dirty`, your local changes conflict with `[ref]` |
| `sparse-checkout` | Not supported yet |
| `git-failed` | Git's own message follows, for example when a branch already exists |
| `git-too-old` | Upgrade git to 2.29 or newer |
| `rollback-failed` | Run `git parhel doctor --fix`. The message also lists the manual steps |

Less common codes: `not-a-repo`, `bare-repo`, `unborn-head`, `inside-git-dir`,
`dest-is-source`, `dest-inside-git-dir`, `dest-inside-submodule`,
`invalid-path`, `permission-denied`, `clone-failed` (also when a `--copy` fails, or
when a filesystem refuses to share one file's blocks),
`interrupted`, `usage`,
`io`.

## Uninstall

```sh
brew uninstall parhel                 # Homebrew
sudo apt remove parhel                # Debian, Ubuntu (.deb)
sudo dnf remove parhel                # Fedora, RHEL (.rpm)
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

The integration tests drive real git on real volumes. On macOS they create
throwaway APFS disk images with `hdiutil`, which needs no root. On Linux they
use loop-mounted Btrfs, XFS and ext4 images; `scripts/linux-test.sh` sets those
up in a Docker container, so you can run the Linux suite from a Mac. To check
that files were cloned rather than copied, they compare physical block
addresses (`F_LOG2PHYS_EXT` on macOS, `FIEMAP` on Linux), and they
use failpoints that work across processes to reproduce races and crashes on
demand. [docs/test-catalog.md](docs/test-catalog.md) lists every behavior the
tests cover, and [CONTRIBUTING.md](CONTRIBUTING.md) explains how to get
started.

`scripts/build-packages.sh <musl target> <dir>` builds the `.deb` and `.rpm` from
a binary you have already built for that target, and
`scripts/test-package.sh <docker image> <package>` installs one in a container
and runs the smoke test as a normal user. CI does both on every change.

## Credits

The idea comes from
[git-worktree-apfs](https://github.com/dannolan/git-worktree-apfs). parhel is a
separate implementation in Rust, built around a transaction that can be rolled
back.

## License

[MIT](LICENSE)
