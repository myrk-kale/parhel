//! Validated requests produced by the CLI layer.

use std::path::PathBuf;

/// How the new worktree's HEAD is chosen. Mirrors `git worktree add` semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchMode {
    /// No branch flag: git creates a branch named after the destination's basename
    /// (or checks out `[ref]` when it names a local branch).
    Default,
    /// `--branch <name>`: check out an existing branch.
    Existing(String),
    /// `--new-branch <name>`: create the branch (`git worktree add -b`).
    New(String),
    /// `--detach`: detached HEAD at `[ref]` (or the source HEAD).
    Detach,
}

/// What to do with initialized submodules in the source worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubmodulePolicy {
    /// Refuse to create the worktree (`error[submodules-present]`).
    #[default]
    Refuse,
    /// Skip submodule directories during the clone and re-initialize them in the
    /// destination with their own git dirs, borrowing objects from the source.
    Reinit,
}

/// `git parhel add` after argument validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRequest {
    /// `--from`: any path inside the source worktree (default `.`), as typed.
    pub from: PathBuf,
    /// `<destination>`, as typed (relative paths resolve against the cwd).
    pub destination: PathBuf,
    /// Optional `[ref]` commit-ish.
    pub reference: Option<String>,
    pub branch: BranchMode,
    /// `--allow-dirty`: carry uncommitted changes from the source.
    pub allow_dirty: bool,
    /// `--fallback`: use a plain `git worktree add` when copy-on-write is impossible.
    pub fallback: bool,
    pub submodules: SubmodulePolicy,
    /// Rewrite the source's absolute path in copied untracked files (`--no-relocate`
    /// turns it off).
    pub relocate: bool,
}

/// `git parhel doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorRequest {
    /// `--from`: any path inside the repository to diagnose (default `.`).
    pub from: PathBuf,
    /// `--fix`: repair what can be repaired safely.
    pub fix: bool,
}
