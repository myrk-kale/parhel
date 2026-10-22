# parhel

Fast, lightweight Git worktrees for parallel development.

`git parhel add` creates a regular Git worktree by **cloning** the source worktree
with APFS copy-on-write (`clonefile(2)`) instead of checking files out. Ignored
heavyweights (`node_modules/`, `target/`, build caches) come along for free: they
share disk blocks with the source until either side writes to them.

> **Status:** test-first scaffolding. The full test suite is written and failing on
> purpose; the implementation comes next. See [docs/test-catalog.md](docs/test-catalog.md).

## Usage

```sh
git parhel add ../my-feature --new-branch feature/my-feature
git parhel add ../hotfix --branch release/1.2
git parhel add ../review --detach v1.4.0
git parhel doctor [--fix]
```

`add` options:

| Flag | Meaning |
|---|---|
| `--from <path>` | Source worktree (any path inside it). Default: `.` |
| `--branch <name>` | Check out an existing branch |
| `--new-branch <name>` | Create a branch |
| `--detach` | Detached HEAD |
| `[ref]` | Commit-ish to check out (default: source HEAD). May differ from the source HEAD: only differing tracked files are rewritten |
| `--allow-dirty` | Carry uncommitted changes (staged changes arrive unstaged) |
| `--submodules=<refuse\|reinit>` | Initialized submodules are refused by default; `reinit` gives them their own git dirs in the new worktree |
| `--fallback` | When copy-on-write is impossible (not APFS, different volumes), use a plain `git worktree add` |

## Guarantees

- Every refusal happens before anything is created.
- A merge, rebase, cherry-pick, revert or `am` in progress in the source is refused,
  even with `--allow-dirty`.
- Files flagged `assume-unchanged` / `skip-worktree` count as uncommitted changes.
- If the source HEAD or its tracked/untracked files change while the worktree is being
  assembled, the run aborts with `source-changed` (ignored files may change freely).
  With `--allow-dirty`, only HEAD moves are detected.
- User-supplied refs and branch names starting with `-` are rejected, and every
  value is passed to git after `--end-of-options`.
- The destination is published with an atomic, exclusive rename: an existing or
  concurrently created destination is never modified or deleted.
- A failed or interrupted run removes the worktree registration, the branch it
  created (never a pre-existing one) and its staging directory. After a crash,
  `git parhel doctor --fix` does the same using the run's journal. A run that is
  still alive holds a lock on its journal and is never touched by `doctor`.
- SIGINT, SIGTERM and SIGHUP roll back and exit with `128 + signal`; further signals
  during rollback are ignored until cleanup finishes.

## v1 limitations

- macOS / APFS only (other platforms report `not-apfs`; `--fallback` works everywhere).
- Sparse-checkout sources are refused.
- Destinations inside a submodule's working tree are refused.
- Hooks (`post-checkout`) are not run, matching `git worktree add --no-checkout`.
- `.git` files of *other* repositories nested in the source are not detected by `add`;
  `doctor` reports them.

## Development

```sh
cargo test --features failpoints   # full suite; see docs/test-catalog.md
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```
