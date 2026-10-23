---
name: parhel
description: >-
  Create and retire git worktrees with `git parhel`, which copies a project's
  ignored files (dependencies, build output, local config) into the new
  worktree with copy-on-write, so it is ready to run in seconds. Use when you
  need a second working copy of a repository (parallel tasks, a review
  checkout, a hotfix next to unfinished work, one worktree per agent) and the
  project has heavy dependencies or builds. Also covers the safe workflow:
  keeping copies away from shared databases and services, not losing ignored
  data when a worktree is removed, recovering from interrupted runs, and what
  to do when copy-on-write is not available (stop and ask the user to run the
  one-time `sudo git parhel setup`; never run it yourself).
---

# parhel: copy-on-write git worktrees

`git parhel add` does what `git worktree add` does, then copies the source
worktree's ignored and untracked files (`node_modules`, `target`, `.venv`,
`.env`, caches) into the new worktree. On a copy-on-write filesystem the copy
takes seconds and almost no disk space until files change. That means APFS
on macOS and, on Linux, Btrfs, XFS with reflink, bcachefs or ZFS 2.2+. On
Linux filesystems without it (ext4, for example) the user can set up a
**store** once with `sudo git parhel setup`: a Btrfs image where the
worktrees live, copy-on-write again. Only the user can do that, because it
needs root. Without it, `--copy` copies every file in full and `--fallback`
makes a regular worktree, and both are a last resort (see step 3).

A copy is a full working environment, not a sandbox. It keeps the same
credentials, talks to the same services and lives in the same git repository
as the original. The workflow below exists to keep those facts from hurting
the user.

## When to use it

Use it when the user wants a second checkout that can build, test or run right
away, for example:

- one worktree per agent or per parallel task;
- reviewing or testing a branch without stopping the current work;
- a hotfix next to unfinished changes;
- trying something risky on a throwaway branch.

Prefer plain `git worktree add` when the new checkout should start clean (no
dependencies, no local config), when the project has no heavy ignored files,
or when `git parhel` is not installed (`git parhel --version` fails). Do not
install it without asking the user.

## The workflow

### 1. Check the source

```sh
git status --short          # must be clean, or the user agrees to --allow-dirty
git parhel doctor           # read-only; look for leftovers before adding more
```

- Uncommitted changes make `add` fail with `dirty-source`. Ask the user to
  commit or stash, or whether the changes should come along (`--allow-dirty`).
  Do not decide that on your own.
- Do not run `add` while a build, install or dev server is writing into the
  source: ignored files may be copied half-written.

### 2. Decide what must not be copied

Look at what the copy would inherit before creating it:

```sh
git status --ignored --short | head -50
ls -a .env* 2>/dev/null
```

- If an env file points at a database, bucket, queue or API that is not
  local and disposable, the copy will use the same resource. Tell the user
  and propose excluding it:

  ```sh
  printf '.env\n.env.*\n' >> .parhelignore
  ```

  Then give the new worktree its own settings (a different database name,
  port, bucket prefix) before running anything in it.
- Exclude anything that must not be duplicated: secrets meant for one machine,
  huge caches that are cheap to rebuild, local databases that the copy should
  not share history with. `.parhelignore` uses `.gitignore` syntax and only
  removes files from the copy, never from the source.
- If a tool in the project stores absolute paths with lengths or checksums
  (some caches and lockfiles), consider `--no-relocate` or excluding that
  path; see step 4.

### 3. Create the worktree

```sh
git parhel add ../<project>-<task> --new-branch <branch>
```

- Put the destination next to the project (same volume).
- **If `add` fails with `not-cow` or `cross-volume` on Linux, stop.** Do not
  try `--copy` or `--fallback` to get past it, and do not look for another
  directory to make the error go away. Tell the user that copy-on-write needs
  a one-time setup that only they can run, give them the exact command, and
  wait until they say it is done:

  > Copy-on-write is not available between the source and `<destination>`.
  > On Linux this is fixed once with a store: please run
  > `sudo git parhel setup` (it creates a Btrfs image mounted at
  > `/parhel/<user>`; `git parhel setup --help` says exactly what it does).
  > Tell me when it is done and I will run the same `git parhel add` again.

  Never run `git parhel setup` yourself and never run anything with `sudo`.
  The user may need `sudo "$(command -v git-parhel)" setup` if sudo cannot
  find `git-parhel`.
- **If `add` fails with `store-unavailable`,** a store exists but cannot be
  used (not mounted, marker missing, not writable). Stop here too. You may run
  the read-only checks `git parhel doctor` and `systemctl status <unit>` (the
  unit is named in the error and in `/parhel/<user>/.parhel-store`) and show
  the output. The fix needs root, so it is the user's: most often
  `sudo systemctl start <unit>`, or `sudo git parhel setup`, which is safe to
  run again. Wait for them. `git parhel add` never falls back by itself.
- On macOS `not-cow` means the destination is not on APFS: choose a
  destination on an APFS volume. There is no `setup` there.
- `--copy` and `--fallback` exist, but avoid them. Use one only when the user,
  after being told that `sudo git parhel setup` would give them copy-on-write,
  explicitly chooses it, and say what it costs: `--copy` copies every file in
  full, so each worktree takes the whole disk space and the time of a real
  copy; `--fallback` gives a regular worktree without dependencies or build
  output, which have to be installed and built again. They cannot be combined.
- With a store, `add` uses it by itself and prints where the worktree really
  lives. The worktree is in the store (`/parhel/<user>/worktrees/<id>/<name>`)
  and the path you asked for is a **symlink** to it. Work from either path;
  git registers the real one. Remember that the store is one image file:
  commits are safe in the repository's `.git`, but uncommitted work inside a
  store worktree goes with the image if it is lost, so tell the user to push
  what matters.
- Other forms: `--branch <existing>`, `--detach <ref>`, a start commit as the
  last argument. With initialized submodules, `--submodules=reinit` is
  required.
- Read the whole output. Warnings list binary files that still name the
  source path, and nested repositories that were detached. Report them to
  the user; do not ignore them.
- On failure, read the error code. parhel rolls back everything it did, so
  the source and the repository are unchanged. Fix the cause and run the same
  command again. Never delete `.parhel-staging-*` folders or edit
  `.git/worktrees` by hand.

| Error | What to do |
|---|---|
| `dirty-source` | Ask: commit, stash, or `--allow-dirty` |
| `dest-exists` | Pick another destination; never remove the existing one to make room |
| `operation-in-progress` | A merge, rebase or cherry-pick is open in the source: ask the user |
| `source-changed` | Something wrote to the source during the copy; wait for it and retry |
| `submodules-present` | Retry with `--submodules=reinit` |
| `not-cow`, `cross-volume` | Linux: stop, tell the user to run `sudo git parhel setup` once, wait. macOS: a destination on APFS. `--copy` or `--fallback` only if the user chooses them after hearing about setup |
| `store-unavailable` | Stop. Show `git parhel doctor` and `systemctl status <unit>`; the user fixes it (`sudo systemctl start <unit>` or `sudo git parhel setup`), then retry |
| `git-failed` | Git's message follows, for example a branch name already in use |
| `rollback-failed` | Run `git parhel doctor`, show the user, then `--fix` (step 6) |

### 4. Work in the copy

- Run everything from inside the new worktree. Its files are independent
  copies: editing them never touches the source.
- Untracked text files that named the source path were rewritten to name the
  copy (virtualenv scripts, config files). Run the project's usual check once
  (tests, `--version`, a build). If a tool rejects a rewritten file, rebuild
  it in the worktree (recreate the virtualenv, reinstall, clear the cache)
  rather than copying it from the source.
- Before any command that changes external state (migrations, seeds,
  `DROP`, queue purges, bucket deletes, deploys, emails), confirm which
  database or service the worktree's configuration points at. If it is shared
  with the source or production, stop and ask the user.
- Parallel copies of a dev server collide on ports and on shared local
  services. Give each worktree its own port and data directory.

### 5. Finish and remove

```sh
git -C <worktree> status --short            # commit or push what matters
git -C <worktree> status --ignored --short  # ignored files are not protected
git worktree remove <worktree>
```

- A worktree in the store is removed the same way, by either path.
  `git worktree remove <symlink>` deletes the real worktree and leaves the
  symlink dangling, which keeps that path taken (`dest-exists`). Then run
  `git parhel doctor --fix`: it removes dangling symlinks that `add` created
  and never a worktree.

- `git worktree remove` refuses only when tracked or untracked files changed.
  Ignored files, such as a local database, logs, generated reports or an
  edited `.env`, are deleted without warning. If the ignored listing shows
  anything the user created in this worktree, ask before removing.
- Do not add `--force` unless the user agrees to lose what it would delete.
- Delete the branch only if it was created for this task and is merged or
  pushed: `git branch -d <branch>` (lowercase `-d`, which refuses unmerged
  work). Never delete branches you did not create.
- External state created from the worktree (database rows, uploaded files)
  stays after the worktree is gone. Mention it to the user if any was created.

### 6. After an interruption

If an `add` was killed, the machine slept or the terminal closed mid-run:

```sh
git parhel doctor           # read-only report
```

Show the findings to the user, then run `git parhel doctor --fix`. It undoes
interrupted runs from their journal, or completes them when the worktree was
already in place, and it never deletes a worktree it cannot prove parhel
created. `in-progress` findings belong to a run that is still active: leave
them alone. With a store, `doctor` also reports `store-unavailable`,
`store-image-deleted` (the image was deleted behind a mounted store: tell the
user at once to copy their work out before unmounting), `store-low-space`,
`store-orphan` and `store-dangling-link`; `--fix` clears dangling links and
stale baselines, never a worktree. Problems that need root are for the user to
fix.

## Rules

- Ask before: `--allow-dirty`, `--copy`, `--fallback`, `--force` on any git command,
  removing a worktree with ignored files the user created, and running
  anything destructive against services the copy shares with the source.
  `--copy` and `--fallback` are never the default way past `not-cow`: first
  tell the user about `sudo git parhel setup` and wait for their choice.
- Never: run `git parhel setup` (with or without `--remove`) or anything with
  `sudo`, touch `/var/lib/parhel`, `/parhel` or the systemd unit of a store,
  hand-edit `.git/worktrees`, delete `.parhel-staging-*` folders, run
  `git worktree prune` to clean up after parhel (use `git parhel doctor`), or
  copy files back from the source into a worktree to "fix" relocation.
- Repository safety: parhel only changes its own worktree registration and a
  branch it created in that run. Every worktree still shares one `.git`, so
  push or back up work the user cannot lose before experiments that rewrite
  history.
