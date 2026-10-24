# Security policy

parhel runs `git` and filesystem operations on your repositories. If you find a
way to make it delete, overwrite or expose data it should not touch (including
through crafted branch names, refs, paths or repository contents), please report
it privately via
[GitHub security advisories](https://github.com/myrk-kale/parhel/security/advisories/new)
rather than a public issue. We aim to respond within a week.

Only the latest release is supported.

## What to expect from parhel

- It writes only inside the new worktree, its own hidden staging folder and that
  worktree's registration in `.git/worktrees/`. The test suite compares every
  other worktree, `.git/config` and the refs byte for byte before and after each
  scenario. A change outside those places is a vulnerability: please report it.
- No telemetry, no network requests of its own.
- Release archives and packages have a build provenance attestation:
  `gh attestation verify <file> -R myrk-kale/parhel`, and a `.sha256` file each.
  The `.deb` and `.rpm` packages are **not GPG-signed**, so `apt`, `dnf` and `rpm`
  cannot check them against a key: verify each one with its `.sha256` file and
  `gh attestation verify` before you install it.
- `git parhel setup` (Linux) is the one command that runs as root. As root it does
  not read `PATH`: it runs `mkfs.btrfs`, `mount`, `systemctl` and the other tools
  it needs only from `/usr/sbin`, `/usr/bin`, `/sbin` and `/bin`, and only when the
  program, the links to it and the directories above it belong to root and cannot
  be written by their group or by others. A `sudo` configuration that keeps an
  unsafe `PATH` therefore cannot make it run a program a user controls. The files
  it opens in the user's own store (the marker, the lock file) are opened without
  following symbolic links, without blocking on a FIFO and only if they are regular
  files.
- The store is one image file that holds every store worktree. Its directory is
  made immutable (`chattr +i`) to stop an accidental deletion; that is not a backup
  and does not protect against disk failure or corruption. Commits live in each
  repository's own `.git`, outside the image: push work you cannot lose.

