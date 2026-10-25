# Copy-on-write Git worktrees on ext4: what works and what needs setup

ext4 does not provide the file reflink operation used for ordinary copy-on-write clones. That means `cp --reflink=always` cannot make a space-saving clone of a file on a typical ext4 volume. You can still run normal Git worktrees there, but their checked-out files take their own disk space.

Parhel supports CoW worktrees on ext4 by creating a Btrfs-backed store and placing the new worktrees inside it. Your original repository can stay on ext4. The requested worktree path becomes a symlink to the real checkout in the store.

This takes a one-time system setup and an initial copy of the source into the store. It is useful when you will create several worktrees, particularly with large dependencies and build output. It will not make the first baseline free.

## Why a reflink won't work directly on ext4

APFS on macOS has `clonefile(2)`. Linux filesystems such as Btrfs and reflink-enabled XFS support the `FICLONE` operation. They can give two files distinct identities while allowing unchanged data blocks to be shared.

ext4 lacks that reflink interface. Even if you use a tool that knows how to request CoW, the filesystem must be able to perform it. Running a normal `git worktree add` still works; it just checks out the files in the usual way.

Other approaches exist, including overlay filesystems such as `fuse-overlayfs`. [simgit](https://github.com/abendrothj/simgit) uses one on Linux systems without reflinks. Parhel instead uses a Btrfs image, so its worktrees get the same native reflink semantics that they would on a Btrfs volume.

## Set up Parhel's CoW store

First install Parhel and check the repository's environment:

```sh
git parhel --version
git parhel doctor
stat -f -c %T .
```

On Linux, `stat` may report an ext4 volume as `ext2/ext3`. `git parhel doctor` checks whether CoW is usable and reports problems with the store if one already exists.

Parhel's setup needs Linux with systemd, Btrfs kernel support and tools including `btrfs-progs`, `util-linux` and `e2fsprogs`. The setup command requires root because it creates a filesystem image, a mount and a systemd unit. Read the [setup reference](../README.md#git-parhel-setup---size-size---preallocate---user-name---remove---discard-contents) before running it:

```sh
sudo git parhel setup
```

If `sudo` cannot find `git-parhel` because it was installed in your user-local `PATH`, use the absolute executable path:

```sh
sudo "$(command -v git-parhel)" setup
```

The default store uses a sparse Btrfs image under `/var/lib/parhel` and mounts it at `/parhel/<user>`. Its default logical size is 100 GiB, but a sparse image does not reserve all 100 GiB on the host disk when created. You can specify a different size with `--size` and explicitly reserve space with `--preallocate`.

Now create a worktree as usual:

```sh
git parhel add ../my-app-login --new-branch feature/login
```

Parhel detects that the source cannot be reflinked into the requested location and uses the store automatically. Your `../my-app-login` path is a symlink; the Git worktree registration points to the real worktree inside the store.

## What takes disk space on the first and later runs?

The store maintains a baseline for each source repository. On the first run, it copies the source working tree into that baseline. This consumes physical storage and takes time, especially if the source has large ignored directories.

Afterward, Parhel updates the baseline with changed files and reflinks it to create each new worktree. Existing worktrees remain independent: changing the source or refreshing the baseline doesn't change their files.

The savings come from reusing the same unchanged data across subsequent worktrees. Storage still grows for modified file blocks, new build output, metadata and anything excluded from sharing.

If the store cannot be used, `git parhel add` stops with an error rather than silently making a full copy. Two explicit alternatives are available:

- `--copy` creates a full file copy, including ignored files. It uses the corresponding disk space.
- `--fallback` uses a normal `git worktree add`. Ignored dependencies and build output are not brought along.

Neither option provides CoW sharing. See [Parhel's platform support](../README.md#platform-support) for details.

## Removing a store worktree

Because the real checkout is inside the mounted store, Parhel locks its Git worktree registration. This protects against Git forgetting the worktree if the store is temporarily unmounted.

Before removal, check both tracked and ignored files. Git may delete ignored files without warning:

```sh
git -C ../my-app-login status --short
git -C ../my-app-login status --ignored --short
```

After saving anything you need, remove it with:

```sh
git worktree unlock ../my-app-login
git worktree remove ../my-app-login
git parhel doctor --fix
```

The last command cleans up the dangling symlink. Ordinary worktrees outside the store only need `git worktree remove`.

Don't delete the store image manually. It can hold several worktrees and their uncommitted or ignored files. It is also not a backup: an image protected from accidental deletion can still be lost to disk or filesystem failure.

## Should you use a store on ext4?

If you usually create one short-lived worktree in a small repository, the setup and baseline may not pay for themselves. A regular worktree can be simpler.

If you repeatedly create large worktrees for parallel tasks, or want copies of `.venv`, `node_modules` or build directories without reinstalling everything, the initial baseline can be worth the cost. Measure free space after the first run and after later worktrees to see the actual gain on your machine.

Related: [Reducing Git worktree disk usage](reduce-git-worktree-disk-usage.md), [worktree tools compared](git-worktree-tools-comparison.md).

Sources: [Parhel store documentation](../README.md#filesystems-without-copy-on-write), [Linux `FICLONE` documentation](https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html).

