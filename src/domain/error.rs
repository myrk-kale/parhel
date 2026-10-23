//! Error type shared by every layer.
//!
//! Every failure carries a stable, kebab-case [`ErrorCode`]. The code is printed as
//! `error[<code>]: <message>` and is part of the public contract: scripts and the
//! integration tests match on it, so codes are never renamed once released.

use std::fmt;
use std::process::ExitCode;

/// Stable identifier of a failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Placeholder returned while a code path has not been implemented yet.
    NotImplemented,
    /// Invalid command-line usage (conflicting or missing arguments).
    Usage,
    /// `--from` (or the cwd) is not inside a Git worktree.
    NotARepo,
    /// The source repository is bare: there is no worktree to clone.
    BareRepo,
    /// The source repository has no commits yet.
    UnbornHead,
    /// `--from` points inside a `.git` directory.
    InsideGitDir,
    /// The installed `git` is older than [`crate::git::parse::MIN_GIT_VERSION`].
    GitTooOld,
    /// A `git` subprocess failed; its stderr is forwarded in the message.
    GitFailed,
    /// The source worktree has uncommitted changes and `--allow-dirty` was not given.
    DirtySource,
    /// The source worktree uses sparse-checkout (unsupported in v1).
    SparseCheckout,
    /// The source has initialized submodules and `--submodules=reinit` was not given.
    SubmodulesPresent,
    /// `--allow-dirty` with `--submodules=reinit`, but a submodule has uncommitted
    /// changes: reinit checks out the recorded commit, so they would be lost.
    DirtySubmodule,
    /// A merge, rebase, cherry-pick, revert or `am` is in progress in the source.
    /// Refused even with `--allow-dirty`: that state lives in the source's admin dir.
    OperationInProgress,
    /// The source HEAD or its files changed while the worktree was being assembled.
    SourceChanged,
    /// Source or destination volume cannot clone files copy-on-write.
    NotCow,
    /// Source and destination are on different volumes (`clonefile` would fail with `EXDEV`).
    CrossVolume,
    /// The destination path already exists (checked up front and again at publish time).
    DestExists,
    /// The destination's parent directory does not exist.
    DestParentMissing,
    /// The destination is inside the repository's `.git` directory.
    DestInsideGitDir,
    /// The destination is the source worktree itself.
    DestIsSource,
    /// The destination is inside a submodule's working tree.
    DestInsideSubmodule,
    /// The path cannot exist on the target filesystem (e.g. not valid UTF-8 on APFS).
    InvalidPath,
    /// The OS refused access (e.g. destination parent not writable).
    PermissionDenied,
    /// Checking out the target ref would overwrite carried-over local changes.
    CheckoutConflict,
    /// `clonefile(2)` failed.
    CloneFailed,
    /// Rolling back a failed `add` did not complete; manual cleanup is required.
    RollbackFailed,
    /// The operation was interrupted (SIGINT, SIGTERM or SIGHUP) and rolled back.
    /// The process exits with `128 + signal` (130, 143, 129).
    Interrupted,
    /// The user's store is configured but cannot be used (not mounted, marker missing,
    /// not on a copy-on-write filesystem, not writable). Never a silent fallback.
    StoreUnavailable,
    /// `setup --remove` refused: worktrees still live in the store.
    StoreInUse,
    /// `add` through the store refused before writing anything: the disk that holds the
    /// store's image, or the store itself, has less room than mirroring the source needs.
    StoreNoSpace,
    /// `setup` needs root and was not run as root.
    NotRoot,
    /// A program `setup` needs (`mkfs.btrfs`, `chattr`, ...) is not installed.
    MissingTool,
    /// The command only exists on Linux, or this Linux lacks what it needs (systemd, btrfs).
    UnsupportedPlatform,
    /// Unexpected I/O error.
    Io,
    /// An error injected by a test failpoint (only with the `failpoints` feature).
    Failpoint,
}

impl ErrorCode {
    /// The stable textual form printed inside `error[...]`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotImplemented => "not-implemented",
            Self::Usage => "usage",
            Self::NotARepo => "not-a-repo",
            Self::BareRepo => "bare-repo",
            Self::UnbornHead => "unborn-head",
            Self::InsideGitDir => "inside-git-dir",
            Self::GitTooOld => "git-too-old",
            Self::GitFailed => "git-failed",
            Self::DirtySource => "dirty-source",
            Self::SparseCheckout => "sparse-checkout",
            Self::SubmodulesPresent => "submodules-present",
            Self::DirtySubmodule => "dirty-submodule",
            Self::OperationInProgress => "operation-in-progress",
            Self::SourceChanged => "source-changed",
            Self::NotCow => "not-cow",
            Self::CrossVolume => "cross-volume",
            Self::DestExists => "dest-exists",
            Self::DestParentMissing => "dest-parent-missing",
            Self::DestInsideGitDir => "dest-inside-git-dir",
            Self::DestIsSource => "dest-is-source",
            Self::DestInsideSubmodule => "dest-inside-submodule",
            Self::InvalidPath => "invalid-path",
            Self::PermissionDenied => "permission-denied",
            Self::CheckoutConflict => "checkout-conflict",
            Self::CloneFailed => "clone-failed",
            Self::RollbackFailed => "rollback-failed",
            Self::Interrupted => "interrupted",
            Self::StoreUnavailable => "store-unavailable",
            Self::StoreInUse => "store-in-use",
            Self::StoreNoSpace => "store-no-space",
            Self::NotRoot => "not-root",
            Self::MissingTool => "missing-tool",
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::Io => "io",
            Self::Failpoint => "failpoint",
        }
    }

    /// Process exit status for this failure: 2 for usage, 130 for SIGINT, 1 otherwise.
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::Usage => ExitCode::from(2),
            Self::Interrupted => ExitCode::from(130),
            _ => ExitCode::FAILURE,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failure with its stable code and a human-readable message.
#[derive(Debug, thiserror::Error)]
#[error("error[{code}]: {message}")]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn not_implemented(what: &str) -> Self {
        Self::new(
            ErrorCode::NotImplemented,
            format!("{what} is not implemented yet"),
        )
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
