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
  - [On macOS](#on-macos)
  - [On Linux](#on-linux)
- [Install](#install)
  - [macOS](#macos)
  - [Linux](#linux)
  - [Check that git can find it](#check-that-git-can-find-it)
  - [Verify a download](#verify-a-download)
  - [Skill for AI coding agents](#skill-for-ai-coding-agents)
- [Read this before you rely on it](#read-this-before-you-rely-on-it)
- [Commands](#commands)
- [Platform support](#platform-support)
- [Filesystems without copy-on-write](#filesystems-without-copy-on-write)
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

Pick your system. Every path ends with the same `git parhel add`; only the
installation and, on some Linux filesystems, a one-time setup differ.

### On macOS

```sh
brew install myrk-kale/tap/parhel

cd ~/code/my-app                       # any path inside your worktree
git parhel add ../my-app-login --new-branch feature/login
cd ../my-app-login && npm test         # no `npm install` needed
```

When you are done with it:

```sh
git worktree remove ../my-app-login
```

Copy-on-write comes from APFS, the default on every Mac since 2017. Nothing
else to set up.

### On Linux

Install it. On Debian, Ubuntu, Fedora and similar distributions you can use a
`.deb` or `.rpm` package instead; see [Linux](#linux) under Install.

```sh
curl -fsSL https://raw.githubusercontent.com/myrk-kale/parhel/main/install.sh | sh
```

Then check which filesystem your projects live on:

```sh
stat -f -c %T ~/code                   # btrfs, xfs, or ext2/ext3 (which is how ext4 shows up)
```

#### On btrfs or XFS

Nothing to set up. Copy-on-write is built into the filesystem (XFS needs
`reflink=1`, the default since xfsprogs 5.1; bcachefs and ZFS 2.2+ should work
too, untested).

```sh
cd ~/code/my-app
git parhel add ../my-app-login --new-branch feature/login
cd ../my-app-login && npm test         # no `npm install` needed
```

When you are done with it:

```sh
git worktree remove ../my-app-login
```

#### On ext4 and other filesystems

ext4, the default on most distributions, cannot share data between files. Run
this once: it creates a copy-on-write area for your user (a btrfs image mounted
at `/parhel/<user>`, protected against deletion) that `add` then uses on its own.
See [Filesystems without copy-on-write](#filesystems-without-copy-on-write) for
what it creates and how to remove it.

```sh
sudo git parhel setup
```

Then, as on btrfs:

```sh
cd ~/code/my-app
git parhel add ../my-app-login --new-branch feature/login
cd ../my-app-login && npm test
```

`../my-app-login` is a symlink to the worktree, which lives in the
copy-on-write area. That worktree is locked in git, so removing it takes one
more step ([why](#removing-a-store-worktree)):

```sh
git worktree unlock ../my-app-login
git worktree remove ../my-app-login
git parhel doctor --fix                # removes the leftover symlink
```

Without the setup, `add` refuses with `not-cow`. `--copy` (a full copy, using
the disk space of one) and `--fallback` (a plain `git worktree add`, without
dependencies or build output) work anywhere, but they give up what parhel is for.

### Before you rely on it

Read [Read this before you rely on it](#read-this-before-you-rely-on-it): a copy
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
deb="parhel_${version}-1_$(dpkg --print-architecture).deb"
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

The `.deb` and `.rpm` packages are not GPG-signed, so your package manager
cannot check them: before you install one, check it against its `.sha256` file
and with `gh attestation verify` (see [Verify a download](#verify-a-download)).

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
gh attestation verify parhel_0.3.0-1_amd64.deb -R myrk-kale/parhel
```

Every file on the release page also has a `.sha256` file: `sha256sum -c
<file>.sha256` (`shasum -a 256 -c` on macOS).

The `.deb` and `.rpm` packages are **not GPG-signed**. `apt`, `dnf` and `rpm`
cannot check them against a key, so nothing checks them for you: verify each
package yourself before you install it, with its `.sha256` file and with `gh
attestation verify` (which proves it came from this repository's release
workflow), as in the commands above. That is also why there is no package
repository yet.

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

- the worktree is on a filesystem without copy-on-write (`not-cow`), unless
  the [store](#filesystems-without-copy-on-write) can serve it;
- the findings about the store, when there is one (`store-unavailable`,
  `store-worktrees-unreachable`, `store-image-deleted`, `store-low-space`,
  `store-orphan`, `store-dangling-link`, `store-stale-baseline`);
- an earlier run crashed and left a hidden `.parhel-staging-*` directory, or a
  worktree registration inside one;
- a `.git` file inside your tree belongs to a different repository (`add`
  detaches such files in the copy; `doctor` points them out in the source).

`--fix` cleans up after crashed runs. It reads the journal the run left behind
and removes only what that run created. If the crash happened after the new
worktree was already in place, `--fix` repairs it and leaves it alone. Runs that
are still going hold a lock on their journal, and `doctor` skips them. For the
store, `--fix` also removes dangling symlinks created by `add` (never one that a
run of `add` still going has just made: its journal is locked, and the symlink is
its own) and baselines whose source is gone; it never deletes a worktree. What it
changes in the store it changes while it holds the store's lock (see [Removing the
store](#removing-the-store)), so `setup --remove` cannot delete the image under a
repair; a store that is being removed is reported as `store-unavailable` and left
alone.

### `git parhel setup [--size <size>] [--preallocate] [--user <name>] [--remove [--discard-contents]]`

Linux only (anywhere else it stops with `unsupported-platform`). Creates the
[store](#filesystems-without-copy-on-write) once, or with `--remove` takes it
down. It changes the system (an image file, a mount, a systemd unit), so it needs
root:

```sh
sudo git parhel setup
```

If `sudo` cannot find `git-parhel` (installed in `~/.local/bin` or `~/.cargo/bin`,
which sudo's `PATH` usually lacks), give the full path:
`sudo "$(command -v git-parhel)" setup`.

| Option | Meaning |
|---|---|
| `--size <size>` | Size the image claims: a number with an optional `K`, `M`, `G` or `T` suffix (binary multiples). Default `100G`, minimum `256M`. The image is sparse, so it takes disk space only as it fills |
| `--preallocate` | Reserve the whole size on disk now (`fallocate`). The disk space is yours from the start, and the store can never run out of room because the host did |
| `--user <name>` | Whose store. Defaults to the user who ran `sudo` (`$SUDO_USER`). A root shell does not name anyone, so it is refused unless you pass `--user`; `--user root` is honored when you ask for it. The name must be an account name, `[a-z_][a-z0-9_.-]*`: it ends up in paths, a unit file and a mount option, so `%` (systemd expands it), spaces, backslashes and the like are a usage error, whether typed or inferred from `$SUDO_USER` |
| `--remove` | Take the store down (see [Removing the store](#removing-the-store)). Cannot be combined with `--size` or `--preallocate` |
| `--discard-contents` | With `--remove`: delete the image even when it could not be looked into or holds files that are not the store's own. Worktrees still block the removal |

Needs `mkfs.btrfs` (package `btrfs-progs`), `blkid` and `mount` (`util-linux`),
`chattr` (`e2fsprogs`), a kernel with btrfs (the module is loaded when it is
only available) and systemd as the init system. Containers and WSL1 usually
lack the last one. The programs it starts are looked for before anything is
created, except `chattr`, whose absence leaves the store working but not
protected (see below).

**Which programs root runs.** `setup` runs as root, often through a `sudo` that
keeps your `PATH`, and a `PATH` that names a directory you can write to would
make root run whatever is first in it. So as root it does not read `PATH` at all.
It looks for `mkfs.btrfs`, `blkid`, `mount`, `umount`, `modprobe`, `chattr`,
`systemctl` and `systemd-escape` in `/usr/sbin`, `/usr/bin`, `/sbin` and `/bin`,
following symbolic links one at a time, and runs one only if the program, every
link on the way to it and every directory above it belong to root and cannot be
written by their group or by others. A program that fails the check is reported as
`missing-tool`, naming the file and the check it failed; a program installed
anywhere else (`/usr/local`, a Nix profile) is not found. What it starts gets
`PATH=/usr/sbin:/usr/bin:/sbin:/bin` and `LC_ALL=C`, and nothing else of the
environment it was given. This is only for root: `add`, `doctor` and everything else
you run as yourself still use your `PATH`.

**What it creates**, in this order, each step only when it is missing:

| What | Where |
|---|---|
| The directory for images, owned by root, mode 0700 | `/var/lib/parhel` |
| The image, mode 0600, root's. Sparse, or fully allocated with `--preallocate`, formatted with `mkfs.btrfs -L parhel-<user>` | `/var/lib/parhel/<user>.img` |
| The mount point (parents 0755, root's) | `/parhel/<user>` |
| The systemd mount unit (`What=` the image, `Where=` the mount point, `Type=btrfs`, `Options=loop,discard,noatime,nosuid,nodev`, wanted by `local-fs.target`), then `systemctl daemon-reload` and `systemctl enable --now`. The name is whatever `systemd-escape --path --suffix=mount /parhel/<user>` says, so `parhel-alice.mount` | `/etc/systemd/system/` |
| Inside the mounted store: the root directory owned by the user (mode 0700), `baselines/`, `worktrees/`, the `.parhel-store` marker and the empty lock file `.parhel-store.lock` (the user's, mode 0600; see [Removing the store](#removing-the-store)) | `/parhel/<user>` |
| The protection below | `/var/lib/parhel` |

The store is mounted again at every boot by the unit. A boot where the image
cannot be mounted does not stop the system from starting: `add` then answers
`store-unavailable`, and `systemctl status parhel-<user>.mount` says why.

**The protection.** The last step is `chattr +i` on `/var/lib/parhel`, the
directory, not the image. An immutable directory forbids creating, deleting and
renaming entries in it, even for root, while the image stays writable for the
loop device. `setup` then checks it: removing the image and creating entries
beside it (renaming goes through the same two checks) fail with `EPERM`, and
opening the image for writing works. The probes are chosen so that they cannot
destroy anything if the protection turns out to be missing: removal is probed with
`rmdir` on the image, which a writable directory refuses with `ENOTDIR` and never
deletes, and creation with a symlink that is removed again. If `chattr` is missing, or the
filesystem under `/var/lib` does not honor the attribute, the store is left
working, `setup` fails loudly and names the image that is not protected, and
running it again after fixing the cause only adds the protection. Setting up a
second user has to lift the attribute to create the new image file, and does so
only around that one call: the attribute is back before the image is sized
(`--preallocate` can take long), formatted or mounted, SIGINT, SIGTERM and SIGHUP
are held back for that moment, and the new image is checked like the first
one. Sizing the image and `mkfs` run with the directory protected, so a Ctrl-C
there leaves a blank image that the next run finishes, never an unprotected
directory. (A `kill -9` or a power cut in that one call can still leave the
directory unprotected: `setup` again puts it back.)

**Running it again** is safe. A healthy store is reported and nothing is
changed. A store whose unit was stopped is started, one that lost its protection
gets it back, and a run that stopped half way is finished. An image that holds a
filesystem is never formatted; only an image with no signature at all (what an
interrupted setup leaves) is, and a mounted image is not even examined. A unit,
marker or image that belongs to something else, or a store that is mounted
without its image, is reported as `store-unavailable` and left alone. So is
anything mounted at `/parhel/<user>` that is not the user's image (a tmpfs, another
disk, a loop device of another file): `setup` checks that the mount source is a
loop device backed by exactly that image before it lays anything out inside,
re-protects on its behalf, or (for `--remove`) unmounts it.

#### Removing the store

```sh
sudo git parhel setup --remove            # add --user <name> if it is not yours
```

It looks inside before it deletes anything. Through the mount when the store is
up; by starting its unit when it is stopped; and when the unit is gone (deleted
by hand) by mounting the image **read-only** (`-t btrfs -o
loop,ro,nodev,nosuid,noexec`, plus `rescue=nologreplay` where the kernel has
it, so that looking changes nothing) on a private directory under `/run`, which
is always unmounted again. An image with no filesystem at all is deleted without
mounting anything.

**It cannot race an `add`.** Checking that no worktree is in the store and then
deleting the image are two steps, and an `add` could publish a worktree between
them. So the store has a lock, the file `.parhel-store.lock` in its root. `add`
holds it *shared* from the moment it decides to use the store until its worktree
is published (or the run is undone), and `doctor --fix` holds it while it changes
the store. `--remove` takes it *exclusively*, without waiting, before it looks at
anything:

- If a run holds it, `--remove` stops with `store-in-use` ("a run is in
  progress"), having looked at nothing and deleted nothing. Wait for the run to
  finish and try again. `--discard-contents` does not override this.
- If `--remove` holds it, an `add` does not start: it stops with
  `store-unavailable` ("the store is being removed") before it writes anything.
- An open file keeps a mount busy, so the lock cannot be held through the
  unmount. Just before it stops the unit, `--remove` writes `removing` into the
  lock file and lets go of the lock: an `add` that takes the lock after that reads
  the mark and does not start either. If the removal then fails, it takes the mark
  out again. If it was killed, the store stays closed to `add` until `sudo git
  parhel setup`, which takes the mark out and says the store was restored.
- An image whose unit is gone is mounted read-only on a private directory of
  root's (mode 0700, under `/run`), never at `/parhel/<user>`. No user can enter
  it and `add` finds a store only by the marker at `/parhel/<user>`, so no run can
  be using it, and no lock is taken. (Someone mounting the image at the store's
  place by hand, as root, while `--remove` runs is outside what a lock can guard.)

A store set up by an earlier version has no lock file: the first `add` or `setup`
makes it. What `--remove` finds decides:

| Found | Result |
|---|---|
| A run in progress (it holds the store's lock) | `store-in-use`, saying a run is in progress. Nothing was looked at or deleted. `--discard-contents` does not override this |
| A worktree (a directory under `worktrees/<repository>/`) | `store-in-use`, listing each where it appears once the store is mounted. The image is untouched. `--discard-contents` does not override this |
| Files in the store root other than `.parhel-store`, `.parhel-store.lock`, `baselines/`, `worktrees/` and `links` | `store-in-use`, listing them; `--discard-contents` deletes them with the store |
| The image does not mount, holds another filesystem, or its marker is missing next to other files or is not this user's | `store-unavailable`, saying that nothing was deleted and giving the commands to look by hand (`mount -t btrfs -o loop,ro,... <image> /mnt`) and to insist; `--discard-contents` goes on |
| Something that is not the user's image mounted at `/parhel/<user>` | `store-unavailable`; it is not unmounted |
| Nothing but the store's own layout | removed |

Remove each worktree with `git worktree unlock <path>` and `git worktree remove
<path>` from its repository first (and let `git parhel doctor --fix` clear the
symlinks left behind); baselines are a cache and do not block it. Before it
deletes, `--remove` prints the symlinks recorded in the store's `links`
registry: they will dangle, and the registry that names them goes with the
store. Then it stops and disables the unit, deletes it, reloads systemd, lifts
the protection, deletes the image and the mount point, and puts the protection
back if other users' images remain. Running it with nothing to remove does
nothing. `/parhel` and `/var/lib/parhel` stay, empty. A unit started only to look
is stopped again if looking fails.

#### Risks

- The store is **one image file** on the host's disk, and it holds **every**
  store worktree: the files of all of them, uncommitted work and ignored files
  included. Lose the image and you lose all of them at once.
- `chattr +i` on `/var/lib/parhel` stops an accidental `rm` or `mv` of the image,
  by root too. That is all it does. It does not protect against a failing disk,
  a corrupted filesystem, a full disk or a deliberate `chattr -i`, and it is **not
  a backup**: parhel keeps no copy of the image.
- Commits and branches are **safe**: they live in your repository's own `.git`,
  outside the image. Push work you cannot lose, as you would before any tool that
  writes to `.git`.
- A sparse image can outgrow the disk under it. `git parhel doctor` warns with
  `store-low-space`; `--preallocate` avoids the surprise.

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
| Linux: other filesystems (ext4, tmpfs, NFS...) | Through the [store](#filesystems-without-copy-on-write), or `--copy` or `--fallback` | a Btrfs image where the worktree lives (copy-on-write there), full copies, or a regular worktree |
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
`cross-volume` before touching anything, unless the Linux user has a
[store](#filesystems-without-copy-on-write), which `add` then uses by itself. You
have two more ways out:

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

## Filesystems without copy-on-write

On Linux, a source on ext4 (or any filesystem that cannot reflink) can still get
copy-on-write worktrees through the **store**: a Btrfs image, one per user, that
is mounted at `/parhel/<user>` and created once with
[`sudo git parhel setup`](#git-parhel-setup---size-size---preallocate---user-name---remove---discard-contents).
The worktree is created inside the store, where files can share blocks, and the
path you asked for becomes a symlink to it.

`add` takes this route by itself, without a flag, when copy-on-write is not
possible between the source and the destination (`not-cow`, or two filesystems,
`cross-volume`), you have a store, and the store is on another filesystem than the
source. `--copy` and `--fallback` keep their meaning and never look at the store.

```
$ git parhel add ../feature --new-branch feature
Created copy-on-write (store) worktree at /home/you/work/feature
The worktree lives in the store at /parhel/you/worktrees/<id>/feature; /home/you/work/feature is a symlink to it
HEAD 4f3c1d2a9b8e
```

How it works:

- **A baseline per repository.** The store keeps a mirror of the source worktree,
  `baselines/<id>/tree`, where `<id>` is a hash of the source's path. Each `add`
  first brings it up to date: it compares every file with the source (type, size,
  modification time to the nanosecond, mode, symlink target) and copies only what
  changed, deletes what the source deleted, and keeps modes, extended attributes,
  POSIX ACLs and times. The first run copies everything (parhel prints a line with
  the size before it starts when it is over 1 GiB); the next ones copy what you
  changed since. The same exclusions apply as for any clone: the root `.git`,
  nested worktrees, submodules with `--submodules=reinit`, `.parhelignore`.
- **A clone of the baseline.** The worktree is a reflink clone of the baseline
  inside the store, then the usual steps run against your repository: the
  registration, the index, a checkout of a ref other than HEAD, submodules. Every
  worktree shares its blocks with the baseline and with the others, so the second
  one costs almost nothing. Rewriting the baseline never reaches a worktree that
  already exists, because each one is an independent clone.
- **The symlink.** git registers the worktree at its real path in the store,
  `<store>/worktrees/<id>/<name>` (`<name>-2`, `<name>-3`... when the name is
  taken), and so do the paths parhel rewrites inside it (virtual environments,
  scripts, symlinks that pointed at the source): they stay valid however you
  reach the worktree. The path you asked for is created last and never replaces
  anything: if something appeared there meanwhile, the run stops with
  `dest-exists` and removes what it made. The symlink is recorded in the run's
  journal and in `<store>/links` before it is created.
- **Two runs on one repository** take turns on the baseline: the second one waits
  for the first to finish copying and cloning, then runs. Each run also reserves
  the real name it takes (`feat`, else `feat-2`...) with a lock held to the end,
  so two runs with destinations of the same name never both take it; the symlink
  is checked once the worktree is published, and a symlink that was removed
  meanwhile is an error naming where the worktree is and the `ln -s` that links it.
- **Removal.** `add` holds the store's lock (shared) from the moment it decides to
  use the store until the worktree is published, so `setup --remove` cannot delete
  the image between its check for worktrees and its deletion
  ([how](#removing-the-store)). While a removal runs, `add` stops with
  `store-unavailable` ("the store is being removed").
- **Room.** The image is a sparse file that grows into the disk under
  `/var/lib/parhel`. Before the first write, `add` measures what the mirror will
  copy (the whole tree the first time; later the entries that changed, found by a
  pass that only looks, skipped when both disks have 64 GiB free) plus a margin
  against the free room of that disk and of the store, and stops with
  `store-no-space`, naming the one that is short, instead of letting the disk fill
  under the filesystem inside.
- **`PARHEL_STORE`** names another store directory (tests and unusual setups). By
  default it is `/parhel/<user>`, the user being the account that runs the command.

If a store is configured and cannot be used (not mounted, no `.parhel-store`
marker, not writable, not on a filesystem that can reflink), `add` stops with
`store-unavailable`; it never falls back by itself to a regular worktree or a full
copy. Run `git parhel doctor`, and check the mount with `systemctl status`. Where
there is no store, `not-cow` also suggests `sudo git parhel setup`.

### Removing a store worktree

A worktree in the store is **locked** in git (`git worktree lock --reason "parhel
store: <mount point>"`), so that `git worktree prune` - which `git gc` runs - does
not forget its registration, and the branch checked out in it, while the store's
mount is down. The price is one more step when you remove it:

```sh
git worktree unlock <path>        # <path>: the symlink you asked for, or the real path
git worktree remove <path>        # --force if it has uncommitted changes
git parhel doctor --fix           # removes the dangling symlink
```

or, in one step, `git worktree remove --force --force <path>` (git's override for a
locked worktree; the second `--force` is the one that matters). Git follows the
symlink, removes the real worktree and its registration, and leaves the symlink
behind, dangling. That path stays taken (`add` would answer `dest-exists`) until
you delete the symlink yourself or run `git parhel doctor --fix`, which removes
dangling symlinks that `add` created (it knows them from `<store>/links`, and
removes a path only while it still is a symlink to exactly the worktree it was
made for). **Never run `git worktree prune` to clean up a store worktree**: with the
store down it forgets every registration that is not locked, and the work in
those directories is then known to nothing.

### What `doctor` says about a store

Only when a store is configured:

| Finding | Meaning |
|---|---|
| `store-unavailable` | The store is configured and cannot be used; the reason follows. Nothing below is checked |
| `store-worktrees-unreachable` | With the store not reachable, git still lists worktrees of this repository in it. Reported only, with the ones that are not locked named: `git worktree prune` or `git gc` would forget those. Do not prune; bring the store back (`systemctl status <unit>`), and lock any that are not (`git worktree lock <path>`) |
| `store-image-deleted` | The store is mounted but the image file behind it was deleted. Copy your work out before unmounting: the data goes away with the last reference |
| `store-low-space` | The filesystem holding the image has under 10% or 2 GiB free (the sparse image grows into it), or the store itself has under 10% free |
| `store-orphan` | A directory under `worktrees/<id>/` that git does not know. Reported only: it may hold work, and `--fix` never deletes a worktree directory |
| `store-dangling-link` | A symlink created by `add` whose worktree is gone. `--fix` removes the symlink |
| `store-stale-baseline` | A baseline whose source is no longer a git worktree. `--fix` deletes that baseline, which is a cache |

With a store that can serve it, a source on ext4 is no longer a `not-cow` finding.

### Risks and limits

- The store is **one image file** in the system's disk, and it holds every store
  worktree, so losing the image loses all of them. `chattr +i` on its directory
  stops an accidental `rm`; it does not protect against a failing disk or a
  corrupted filesystem, and it is not a backup. Commits and branches live in your
  repository's own `.git`, outside the image, so they are safe if the image is
  lost. Uncommitted work in a store worktree is not: commit and push what you
  cannot lose.
- The baseline is a cache of the source. The sync compares size, modification
  time to the nanosecond and mode, **and** the status-change time (`ctime`), which
  no program can set: a file rewritten with the same size and its times put back
  (`touch -d`, `cp -p`, a build tool) is still noticed, as is a change of an
  extended attribute or an ACL. Each finished sync records when it began in the
  baseline's `meta`; the next copies every entry changed since then, less a 2 s
  margin (the clock that stamps files ticks coarsely, and on FAT or a network
  filesystem it is not the machine's), so a file saved just before an `add` is
  copied again by the next one. This relies on the filesystem keeping `ctime`
  honest (every Linux filesystem that can hold a worktree does, FAT and exFAT
  aside) and on its clock being close to the machine's (a network filesystem
  whose server is minutes off can hide a change). A sync that died without a word
  (power loss, `kill -9`), a baseline whose `meta` has no such record, and a first
  sync are not trusted at all: the whole source is mirrored again.
- A disk that fills up while the mirror is written (the room was measured and
  something else wrote meanwhile) fails the run with the system's message, leaves
  no worktree and no symlink, and the next run (with room, or with less in the
  source) works from where the baseline stands.
- A directory that is read-only in the source (a vendored tree, a module cache) is
  mirrored read-only; the sync opens it for its owner for the moment it changes
  something in it, and restores its mode with the rest of its metadata.
- The symlink path may not contain a tab or a newline (the registry has one line
  per link).

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
| With a store: the baseline, the worktree, the registry of links | `<store>/baselines/<id>` (the mirror `tree/`, `meta`, the lock `lock`, and `names/<name>`, empty lock files that keep a real name for the run that took it), `<store>/worktrees/<id>/<name>` and `<store>/links`, all inside the store, and the store's empty lock file `<store>/.parhel-store.lock` (made if missing); plus the symlink at the destination you asked for, and a lock on the worktree's registration in git (`git worktree lock`) |

`git parhel setup` is the one command that writes outside your repositories, as
root, and only at the places listed under [its reference](#git-parhel-setup---size-size---preallocate---user-name---remove---discard-contents):
`/var/lib/parhel`, `/parhel/<user>` and one unit in `/etc/systemd/system`.
`--remove` deletes exactly those and nothing that merely lies next to them.
As root it does not read `PATH`: it runs its tools only from `/usr/sbin`,
`/usr/bin`, `/sbin` and `/bin`, and only when root owns them and nobody else can
write to them ([which programs root runs](#git-parhel-setup---size-size---preallocate---user-name---remove---discard-contents)).

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
  Elsewhere on Linux the [store](#filesystems-without-copy-on-write) provides it;
  without one, `--copy` copies every file and `--fallback` creates a regular
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
Run `git worktree remove <path>`. Ignored files do not block it. A worktree in
the [store](#removing-a-store-worktree) is locked: `git worktree unlock <path>`
first, then remove it, then `git parhel doctor --fix` for the symlink.

**Can I move, lock or prune it?**
Yes, with the usual `git worktree move`, `lock`, `unlock`, `prune` and `repair`
- except in the store, where the worktrees are locked on purpose and `prune` is
the command that would lose them while the store is down.

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
| `not-cow` / `cross-volume` | Use a destination on the same volume, on a filesystem with copy-on-write, set up the [store](#filesystems-without-copy-on-write) (`sudo git parhel setup`, Linux), or pass `--copy` (full copy) or `--fallback` (regular worktree) |
| `store-unavailable` | The store exists but cannot be used (not mounted, marker missing, no copy-on-write, not writable), or it is being removed (`setup --remove` holds its lock, or was killed after marking it; wait, or run `sudo git parhel setup`). Run `git parhel doctor` and look at `systemctl status` of its mount; `--copy` and `--fallback` do not need it. From `setup`: what is mounted at `/parhel/<user>` is not the user's image, or `--remove` could not look inside the image (it says that nothing was deleted and how to look yourself or insist with `--discard-contents`) |
| `store-no-space` | `add` through the store found the disk that holds the store's image, or the store, too small for what it would copy (the message gives the figures and which one is short). Free some space there, or use `--copy` / `--fallback`, which do not use the store |
| `not-root` | `git parhel setup` changes the system and must run as root; the message gives the exact `sudo` command |
| `missing-tool` | A program `setup` needs is not installed in `/usr/sbin`, `/usr/bin`, `/sbin` or `/bin`, or is there but root does not trust it (the message names the file and the check: owned by someone else, or writable by group or others): `mkfs.btrfs` (package `btrfs-progs`), `blkid` or `mount` (`util-linux`), `systemctl` (`systemd`) or `chattr` (`e2fsprogs`). With `chattr` the store was created but its image is not protected; install the package and run `setup` again |
| `unsupported-platform` | `setup` only exists on Linux, and there it needs systemd and a kernel with btrfs |
| `store-in-use` | `setup --remove` refuses while worktrees live in the store, or while it holds files that are not the store's own; the message lists them. Remove the worktrees with `git worktree unlock` and `git worktree remove` first (`--discard-contents` covers the other files, never worktrees). It also refuses, having looked at nothing, while an `add`, a `doctor --fix` or another removal is running against the store: wait for it |
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

If you set up a [store](#filesystems-without-copy-on-write) on Linux, take it down
first, while `git parhel` is still installed: `sudo git parhel setup --remove`
(after removing the worktrees that live in it).

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
