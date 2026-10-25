# Git worktree disk space FAQ: CoW, dependencies and AI agents

## Why does each Git worktree take so much disk space?

Git worktrees share their repository's object database, including history and objects. Each worktree still needs its own working files. Ignored directories such as `node_modules/`, `.venv/` and build output don't appear in a fresh checkout, so reinstalling and rebuilding them increases storage use further.

If you want a practical walkthrough, start with [how to reduce Git worktree disk usage](reduce-git-worktree-disk-usage.md).

## Does Git already use copy-on-write for worktrees?

`git worktree add` doesn't normally create reflink clones of the checked-out files. It creates a regular working tree. Tools such as [Parhel](https://github.com/myrk-kale/parhel), [git-cow](https://github.com/gabrielkoerich/git-cow) and [simgit](https://github.com/abendrothj/simgit) add approaches that share unchanged file data. [Worktrunk](https://worktrunk.dev/step/#wt-step-copy-ignored) can reflink ignored build artifacts between worktrees.

## Can new worktrees reuse `node_modules`, `.venv` and compiled output?

Yes. `git parhel add` copies the current worktree using CoW when supported. It brings ignored directories along, without needing per-language rules. If the source has `node_modules/`, `.venv/` or `target/`, the new worktree can start with those files present:

```sh
git parhel add ../my-app-feature --new-branch feature/my-feature
```

Some tools save absolute paths in their environments. Parhel relocates common paths in untracked text files and certain symlinks, but embedded binary paths are not changed. Check the environment before relying on it; a Conda environment, for example, may need to be rebuilt.

## Does Parhel copy untracked and Git-ignored files?

Ignored files are copied by default unless excluded through `.parhelignore`. Non-ignored untracked files and uncommitted changes need an explicit `--allow-dirty`; a clean source is the default requirement.

`.parhelignore` uses `.gitignore` syntax and is useful for local databases, secrets or environments that do not relocate safely. It applies only to untracked files. Tracked files still come from Git.

## Do CoW worktrees remain independent after cloning?

Yes. Each file has its own identity in the new checkout. The filesystem shares unchanged physical blocks; writes to one clone allocate separate blocks for the changed data. The source files stay unchanged. The worktrees still share Git history and repository configuration, as normal linked worktrees do.

Worktrees do not isolate databases, ports, network access or credentials. If both copies contain the same `.env`, they may connect to the same production or development service. Use separate configuration and ports when running agents in parallel.

## Can copy-on-write worktrees work on Ubuntu with ext4?

Not through native reflinks on the ext4 volume. Parhel can use a Btrfs-backed store created by a one-time, root-authorized setup:

```sh
sudo git parhel setup
```

The first store baseline consumes real disk space. Subsequent worktrees can share its blocks. The store also needs systemd and Btrfs support. The [ext4 guide](copy-on-write-git-worktrees-ext4.md) covers setup, storage costs and removal.

## Why does `du` still show a large CoW worktree?

Tools such as `du` and Finder can make reflinked files appear to occupy the same amount of space as full copies because they don't attribute shared blocks in a way that measures the marginal cost of another worktree. Compare filesystem free space before and after creation, using `df -h` on the same volume. Background writes can affect that reading, so measure under similar conditions.

## Can an AI coding agent use Parhel?

Yes, if Parhel is installed and the agent can run its Git subcommand. There's also an optional skill that tells supported coding agents how to choose Parhel, use `.parhelignore`, check shared services and clean up worktrees safely:

```sh
npx skills add myrk-kale/parhel
```

The `-g` flag installs it globally through the skills CLI. Installing the skill doesn't install the `git-parhel` executable or automatically make unrelated AI assistants aware of the project.

## Will removing a worktree delete `node_modules`, local databases or logs?

It can. Git's removal checks don't protect ignored files the way developers sometimes expect. Inspect them first:

```sh
git -C ../my-app-feature status --ignored --short
git worktree remove ../my-app-feature
```

If the worktree lives in Parhel's ext4 store, unlock it first and run `git parhel doctor --fix` afterward to clean up the destination symlink. See [removing a store worktree](copy-on-write-git-worktrees-ext4.md#removing-a-store-worktree).

For command flags and uncommon cases such as submodules, sparse checkout or interrupted operations, see the [Parhel README](../README.md) and [test catalog](test-catalog.md).
