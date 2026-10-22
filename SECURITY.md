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
- Release archives have a build provenance attestation:
  `gh attestation verify <archive> -R myrk-kale/parhel`.

