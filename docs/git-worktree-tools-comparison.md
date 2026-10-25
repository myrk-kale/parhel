# Parhel vs. Git worktree, Worktrunk, git-cow and simgit: which saves disk space?

Several tools now use copy-on-write to make Git worktrees cheaper. The useful distinction is which files they share, how they behave on your filesystem and whether you want a worktree manager or a small extension to Git.

If you want straightforward worktrees and don't mind fresh dependency installs, native Git is often enough. Worktrunk is designed around managing branches, worktrees and agent sessions. Parhel, git-cow and simgit focus more directly on reducing the cost of additional working trees, with different approaches to caches and filesystems.

This comparison reflects the projects' public documentation checked in October 2026. Commands and support may change.

## Quick comparison

| Tool | What it does | Tracked working files | Ignored dependencies and build output | ext4 approach |
| --- | --- | --- | --- | --- |
| [Git worktree](https://git-scm.com/docs/git-worktree) | Native linked checkouts | Materializes a normal checkout | Starts without ignored files | Normal checkout, no built-in CoW cloning |
| [Parhel](https://github.com/myrk-kale/parhel) | Creates CoW worktrees through `git parhel add` | CoW clone where supported | Clones ignored files from the source worktree | One-time Btrfs-backed CoW store; baseline takes an initial full copy |
| [Worktrunk](https://github.com/max-sixty/worktrunk) | Manages worktrees, branches, hooks and agent workflows | Normal Git checkout | Optional `wt step copy-ignored`, reflinked where supported | Ignored files are copied in full |
| [git-cow](https://github.com/gabrielkoerich/git-cow) | Uses an extra checkout as a warmed CoW source | CoW clone of its extra checkout | Shares selected ignored directories and caches | Falls back to plain `git worktree add` |
| [simgit](https://github.com/abendrothj/simgit) | Creates cheap worktrees from a cached baseline, with agent and cleanup commands | CoW reflinks or overlay-based sharing | Its documented focus is the working-tree baseline | `fuse-overlayfs` where available; otherwise a normal checkout |

All five create or use real Git linked worktrees. They don't create separate copies of the Git repository history.

The `git-cow` row refers to [gabrielkoerich/git-cow](https://github.com/gabrielkoerich/git-cow). There's also [josharian/git-cow-worktree](https://github.com/josharian/git-cow-worktree), a separate tool that reflinks matching tracked files and skips ignored or untracked files.

## When native Git makes sense

```sh
git worktree add -b feature/login ../my-app-login
```

Native Git has no extra installation and fits into existing scripts. Every worktree gets its own checkout and index, while the objects live in the shared repository. You can manage it with `git worktree list`, `lock`, `move`, `remove` and `prune`.

The cost is repeating checked-out files. Ignored dependencies are absent until you create them in the new worktree. If the repository is small or fresh dependency installation is desirable, that may be fine.

## When Parhel makes sense

```sh
git parhel add ../my-app-login --new-branch feature/login
```

Parhel aims to keep the regular Git workflow. It clones the source checkout, including its ignored files, and creates a normal linked worktree. On APFS and Linux filesystems with reflinks, unchanged file data shares disk blocks. On ext4, Parhel can use a Btrfs-backed store after a one-time `sudo git parhel setup`.

It also handles common absolute paths in untracked text files, including virtualenv scripts, while leaving embedded paths in binaries untouched. `.parhelignore` excludes untracked items you don't want carried over. Normal Git worktree commands remain available afterward.

The tradeoff is that ignored files are copied unless excluded, including local configuration and credentials. The store on ext4 needs system setup and an initial baseline. Parhel doesn't manage your agent sessions, branches or dev-server ports for you.

## When Worktrunk makes sense

```sh
wt switch --create feature-login
```

Worktrunk gives worktrees a richer command-line workflow: switching by branch, hooks, cleanup, merge helpers, PR checkout and commands that launch agents. It can also reuse build output. Its [`wt step copy-ignored`](https://worktrunk.dev/step/#wt-step-copy-ignored) command copies ignored files from another worktree and uses reflinks on APFS, Btrfs and reflink-capable XFS.

That feature is real CoW for the ignored files; the tool does not claim to CoW-clone the tracked Git checkout as part of ordinary worktree creation. Its documentation says ext4 and NTFS use full copies for ignored files.

Choose it if you want its management features. If you already have scripts and just want `git parhel add` to save disk space, Parhel has less workflow of its own to adopt.

## When git-cow makes sense

```sh
git cow add -b feat/login ../my-app-login
```

[`gabrielkoerich/git-cow`](https://github.com/gabrielkoerich/git-cow) keeps an extra checkout that it periodically warms from your source checkout. New worktrees are cloned from that baseline. Selected ignored directories, such as build caches, are also shared; additional directories can be configured with `git cow share`.

This can be particularly useful when multiple agents repeatedly use the same build outputs. The baseline can lag behind the current checkout by the configured warm interval. Its README says loose ignored files such as `.env` are not copied, and `.venv` is not selected by default because virtual environments often contain absolute paths. On ext4 it falls back to Git's ordinary checkout.

## When simgit makes sense

```sh
simgit add feat/login
```

[`simgit`](https://github.com/abendrothj/simgit) uses a cached baseline to avoid rewriting the tracked working tree for each agent. It adds commands for running agents in workspaces, collecting garbage, repairing mounts and, when useful, sparse checkout.

Its approach to ext4 is different from Parhel's: it can use unprivileged `fuse-overlayfs` instead of asking the user to set up a Btrfs-backed store. That introduces FUSE as a dependency and has different performance characteristics. Its README provides measurements for tracked-tree duplication; check its current documentation if copying ignored dependencies is the deciding requirement for you.

## Which one should you pick?

- Need no extra tools, or need a pristine checkout? Use `git worktree`.
- Already want branch switching, hooks, agent launch and merge commands? Worktrunk is worth looking at.
- Want an extra warm checkout to reuse selected build caches? Consider git-cow.
- Want cached-baseline worktrees, sparse options or an overlay approach on ext4? Look at simgit.
- Want CoW for your current checkout, including ignored files, while keeping familiar Git commands? Try Parhel.

Reflinks save physical storage, but none of these tools turns a worktree into a process or security sandbox. Databases, ports, environment variables and external services may still be shared. See [how to reduce worktree disk usage safely](reduce-git-worktree-disk-usage.md) and the [ext4 guide](copy-on-write-git-worktrees-ext4.md).

Sources: [Git documentation](https://git-scm.com/docs/git-worktree), [Parhel README](../README.md), [Worktrunk copy-ignored documentation](https://worktrunk.dev/step/#wt-step-copy-ignored), [git-cow README](https://github.com/gabrielkoerich/git-cow), [simgit README](https://github.com/abendrothj/simgit), [git-cow-worktree README](https://github.com/josharian/git-cow-worktree).

