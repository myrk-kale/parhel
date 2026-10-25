# How to reduce Git worktree disk usage without changing your Git workflow

If Git worktrees are eating up disk space, check the working files and ignored directories first. Git already shares the repository's object database between its worktrees. What it normally writes again is the checked-out file tree, and each new worktree starts without ignored dependencies and build artifacts. Copy-on-write (CoW) can share the underlying disk blocks of those files while keeping the worktrees independent.

This becomes noticeable with parallel coding agents. Five agents working on five branches can mean five checkouts, several dependency installs and several build directories. The cost depends on the project, but repeating a multi-gigabyte `target/` or `node_modules/` directory is an easy way to run out of space.

## Why `git worktree add` uses more disk than expected

Consider a project with a large working directory:

```sh
git worktree add -b feature/search ../my-app-search
```

That worktree has its own branch, index and working files. It shares the Git objects and history with the original checkout, so Git does not clone the entire repository history. It still needs to materialize the tracked files for the new checkout.

There's another wrinkle: files ignored by Git, including `node_modules/`, `.venv/`, `target/` and generated assets, are absent from a fresh worktree. You usually install dependencies or run a build again. The original worktree still has its copies, so the overall disk usage grows.

You can inspect the current checkout with:

```sh
git worktree list
git status --ignored --short
du -sh .
```

The last command is useful for finding large directories. Keep in mind that `du` is not a reliable measurement of *additional physical disk usage* after CoW cloning, because it can count shared extents in more than one directory.

## Options for reducing disk usage

### Keep a fresh checkout and reinstall what it needs

Plain `git worktree add` is a reasonable choice if the source tree is small, dependencies install quickly or each task needs a completely fresh environment. You can also use your package manager's shared cache or install fewer optional dependencies.

The downside is the repeated setup and output generated inside each worktree. A package cache can reduce downloads without making the whole working tree a CoW clone.

### Reuse ignored files with a worktree manager

Some worktree tools copy ignored directories from an existing checkout. Worktrunk, for example, provides [`wt step copy-ignored`](https://worktrunk.dev/step/#wt-step-copy-ignored), which can use reflinks on compatible filesystems.

This is useful if you already want Worktrunk's branch switching, hooks and agent workflow. You can set up the copy step to run when a new worktree starts. On ext4, however, its ignored-file copy is a full copy rather than a reflink.

### Clone the working tree with copy-on-write

CoW cloning gives the new worktree its own file entries while allowing unchanged file contents to share physical blocks. When either worktree changes a file, the filesystem allocates new blocks for the changed data. Both checkouts can then evolve independently.

[Parhel](https://github.com/myrk-kale/parhel) is a Git extension written for this workflow. It uses CoW for tracked files and brings along ignored directories such as dependencies and build output. It creates a normal Git worktree, so the existing `git worktree list`, `move` and `remove` commands still apply.

## Try a copy-on-write worktree with Parhel

On macOS, install with Homebrew:

```sh
brew install myrk-kale/tap/parhel
```

On Linux or macOS, you can also use Cargo if you have a supported Rust toolchain:

```sh
cargo install parhel
```

Then create a worktree from inside your project:

```sh
git status --short
git parhel doctor
git parhel add ../my-app-search --new-branch feature/search
```

Parhel normally requires the source to be clean. If you intentionally want your uncommitted changes and non-ignored untracked files to come along, there's `--allow-dirty`. Staged changes arrive in the copy unstaged. Ignored files are included by default.

For an existing branch, use `--branch`. To check out a specific commit without a branch, use `--detach`. See the [command reference](../README.md#commands) for other forms.

On APFS, Btrfs and reflink-enabled XFS, this works directly when source and destination can clone across the same filesystem. Linux ext4 needs [one-time store setup](copy-on-write-git-worktrees-ext4.md). You can check your environment with `git parhel doctor`.

### What gets reused?

The mechanism is language-independent. If your project has `.venv/`, `node_modules/`, `target/` or other ignored files, Parhel sees files and directories; it does not need a Python or Node.js integration.

Some generated files contain absolute paths into the original checkout. Parhel rewrites matching paths in untracked text files up to 1 MiB and repoints applicable symlinks. It leaves binary contents alone, so some environments still need to be rebuilt. Conda environments are a common example.

You can exclude paths you don't want copied in `.parhelignore`:

```gitignore
.conda/
.env
.env.*
```

The file uses `.gitignore`-style patterns and affects untracked paths. It does not exclude tracked files from Git's checkout.

## How much space does CoW save?

The [Parhel README](../README.md) reports a test on macOS with 78 MB of `node_modules` spread over 20,000 files, plus a 300 MB build artifact:

| Method | Time | Additional disk used |
| --- | ---: | ---: |
| `git worktree add`, then `cp -R node_modules target` | 6.9 seconds | 383 MiB |
| `git parhel add` | 0.5 seconds | About 8 MiB |

These are measurements of one workload on one machine. Large numbers of small files, changes made immediately after cloning and filesystem metadata can change the result. You should measure your own project.

Take a free-space reading before and after creating a test worktree, on the same filesystem:

```sh
df -h .
# Create the worktree here.
df -h .
```

The difference is approximate because other processes may write to the disk during the test. On ext4, the first Parhel run also has to populate its CoW store with a baseline, so you won't see the same first-run savings as on native CoW filesystems.

## Worktree isolation and shared services

Separate worktrees have separate files and branches. They may still use the same database, queue, cloud account or development-server port. Copying `.env` can copy credentials and connection strings too.

Before running migrations, cleanup scripts or concurrent dev servers, inspect those settings. Give each worktree separate services or ports where necessary, and exclude sensitive files if they should not be duplicated.

Also inspect ignored files before removing a worktree:

```sh
git -C ../my-app-search status --ignored --short
git worktree remove ../my-app-search
```

Git can remove ignored files with the worktree without asking. If you're using Parhel's ext4 store, removal requires an [extra unlock and cleanup step](copy-on-write-git-worktrees-ext4.md#removing-a-store-worktree).

If you are choosing between different tools, see the [comparison of native Git, Parhel, Worktrunk, git-cow and simgit](git-worktree-tools-comparison.md). If you use AI coding agents, Parhel also provides an [optional agent skill](git-worktree-disk-space-faq.md#can-an-ai-coding-agent-use-parhel).

Sources: [Git worktree manual](https://git-scm.com/docs/git-worktree), [Linux reflink interface](https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html), [Parhel README](../README.md).

