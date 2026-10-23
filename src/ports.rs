//! Ports: the side effects the application layer needs, as traits.
//!
//! Production adapters live in [`crate::git`] and [`crate::fs`]; unit tests drive the
//! application layer through recording fakes (`app::fakes`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::domain::{BranchMode, Result};
use crate::git::parse::{GitVersion, WorktreeEntry};

/// Facts about the source worktree, discovered from `--from`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Canonical root of the source worktree (`git rev-parse --show-toplevel`).
    pub toplevel: PathBuf,
    /// This worktree's git dir (`.git` or `.git/worktrees/<id>`), canonical.
    pub git_dir: PathBuf,
    /// The repository's common git dir, canonical.
    pub common_dir: PathBuf,
    /// Commit checked out in the source.
    pub head: String,
}

/// Arguments of `git worktree add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeAddSpec {
    pub path: PathBuf,
    pub branch: BranchMode,
    pub reference: Option<String>,
    /// `--no-checkout` (copy-on-write path) vs a regular checkout (fallback path).
    pub no_checkout: bool,
}

/// Everything parhel asks of git.
pub trait Git {
    fn version(&self) -> Result<GitVersion>;
    /// Errors: `not-a-repo`, `bare-repo`, `unborn-head`, `inside-git-dir`.
    fn discover(&self, from: &Path) -> Result<RepoInfo>;
    /// Uncommitted changes (ignored files excluded), skipping paths under `exclude`
    /// (relative to `worktree`; used to ignore parhel's own staging dir).
    fn is_dirty(&self, worktree: &Path, exclude: &[PathBuf]) -> Result<bool>;
    fn sparse_checkout_enabled(&self, worktree: &Path) -> Result<bool>;
    /// Index entries flagged `assume-unchanged` or `skip-worktree`: `status` cannot
    /// vouch for them, so they count as dirty.
    fn flagged_index_entries(&self, worktree: &Path) -> Result<Vec<PathBuf>>;
    /// `"merge"`, `"rebase"`, `"cherry-pick"`, `"revert"` or `"am"` when one is in progress.
    fn operation_in_progress(&self, worktree: &Path) -> Result<Option<String>>;
    /// Gitlink paths (relative) whose submodule is initialized in `worktree`.
    fn initialized_submodules(&self, worktree: &Path) -> Result<Vec<PathBuf>>;
    fn worktrees(&self, repo: &Path) -> Result<Vec<WorktreeEntry>>;
    /// Paths in the index of `worktree` (files and gitlinks), relative.
    fn tracked_files(&self, worktree: &Path) -> Result<Vec<PathBuf>>;
    /// Untracked paths of `worktree` matching its `.parhelignore` (gitignore
    /// syntax), relative; directories are reported once. Empty without the file.
    fn parhelignored(&self, worktree: &Path) -> Result<Vec<PathBuf>>;
    fn branch_exists(&self, repo: &Path, name: &str) -> Result<bool>;
    /// Full object id of the commit `rev` points to.
    fn resolve_commit(&self, repo: &Path, rev: &str) -> Result<String>;
    fn head(&self, worktree: &Path) -> Result<String>;

    fn worktree_add(&self, repo: &Path, spec: &WorktreeAddSpec) -> Result<()>;
    /// Populate the index from `commit` without touching files (`read-tree`), then refresh.
    fn reset_index(&self, worktree: &Path, commit: &str) -> Result<()>;
    /// Two-way merge `from` -> `to` updating files (`read-tree -m -u`); ignored files
    /// may be overwritten, local modifications may not (`checkout-conflict`).
    fn checkout_two_way(&self, worktree: &Path, from: &str, to: &str) -> Result<()>;
    /// Initialize submodules in `worktree` with their own git dirs, borrowing objects
    /// from `source` so no network is needed when they are present locally.
    /// `worktree` is still in staging; links are written for its final location
    /// `final_path`, so they stay valid after the publishing rename.
    fn submodules_reinit(&self, worktree: &Path, source: &Path, final_path: &Path) -> Result<()>;
    /// Point the registration of the worktree moved from `old` to `new` at its new
    /// location. Touches only that worktree's admin entry and `.git` file (unlike
    /// `git worktree repair`, which also rewrites other worktrees' links).
    fn repoint_worktree(&self, repo: &Path, old: &Path, new: &Path) -> Result<()>;
    fn worktree_remove_force(&self, repo: &Path, path: &Path) -> Result<()>;
    /// Remove only the registration (`.git/worktrees/<id>`) whose gitdir points to
    /// `path`, even when `path` no longer exists. Unlike `worktree prune`, it never
    /// touches other worktrees whose directories are merely unreachable (e.g. on an
    /// unmounted disk). No-op when nothing is registered at `path`.
    fn unregister_worktree(&self, repo: &Path, path: &Path) -> Result<()>;
    /// Delete `refs/heads/<name>` only if it still points at `expected`, atomically
    /// (`git update-ref -d`): a branch that moved meanwhile is never deleted.
    fn branch_delete(&self, repo: &Path, name: &str, expected: &str) -> Result<()>;
}

/// What [`CowFs::scan`] found under a root (paths relative to it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// Regular files of at most `max_len` bytes containing one of the needles.
    pub matches: Vec<PathBuf>,
    /// Regular files named `.git` (gitfiles), wherever they are.
    pub gitfiles: Vec<PathBuf>,
    /// Symlinks whose target starts with one of the needles, with that target.
    pub links: Vec<(PathBuf, PathBuf)>,
    /// Entries that could not be read.
    pub unreadable: usize,
}

/// Filesystem identity of a path's volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Name of the filesystem (`"apfs"`, `"hfs"`, `"exfat"`, ...), for messages only.
    pub fs_type: String,
    /// Whether files can be cloned copy-on-write on this volume. Decided by the
    /// adapter (APFS: always; Linux: a probe), never by comparing `fs_type`.
    pub cow: bool,
    /// `st_dev` of the path.
    pub device: u64,
}

/// Everything parhel asks of the filesystem.
pub trait CowFs {
    fn volume(&self, path: &Path) -> Result<VolumeInfo>;
    fn canonicalize(&self, path: &Path) -> Result<PathBuf>;
    /// `lstat`-based: a dangling symlink exists.
    fn exists_no_follow(&self, path: &Path) -> Result<bool>;
    /// Follows symlinks (a destination parent may be reached through one).
    fn is_dir(&self, path: &Path) -> Result<bool>;
    /// `lstat`-based: a symlink to a directory is not a directory.
    fn is_dir_no_follow(&self, path: &Path) -> Result<bool>;
    /// Atomically create a fresh, uniquely named `.parhel-staging-*` dir in `parent`.
    fn create_staging_dir(&self, parent: &Path) -> Result<PathBuf>;
    fn list_dir(&self, path: &Path) -> Result<Vec<OsString>>;
    fn create_dir(&self, path: &Path) -> Result<()>;
    /// `clonefile(src, dst, CLONE_NOFOLLOW)`. Errors: `clone-failed`, `cross-volume`.
    fn clone_entry(&self, src: &Path, dst: &Path) -> Result<()>;
    /// `renamex_np(from, to, RENAME_EXCL)`. Errors: `dest-exists` when `to` exists
    /// (even as an empty dir), `io` otherwise.
    fn rename_exclusive(&self, from: &Path, to: &Path) -> Result<()>;
    /// Recursive removal that also clears BSD immutable flags (`uchg`) the clone
    /// may have copied from the source.
    fn remove_dir_all(&self, path: &Path) -> Result<()>;
    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()>;
    /// Walk `root` (not following symlinks, skipping relative paths for which
    /// `skip` returns true and everything below them) and report candidates for
    /// relocation. Reads files in parallel: this runs over every untracked file.
    fn scan(
        &self,
        root: &Path,
        needles: &[Vec<u8>],
        max_len: u64,
        skip: &(dyn Fn(&Path) -> bool + Sync),
    ) -> Result<Scan>;
    /// Overwrite an existing file's contents, keeping its permissions (even when
    /// it is read-only).
    fn replace_contents(&self, path: &Path, contents: &[u8]) -> Result<()>;
    fn remove_file(&self, path: &Path) -> Result<()>;
    /// Give `to` the permission bits of `from` (directories the clone did not copy
    /// itself must not end up more open than the source).
    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()>;
    /// Point an existing symlink at `target`.
    fn replace_symlink(&self, path: &Path, target: &Path) -> Result<()>;
    fn read_file(&self, path: &Path) -> Result<Vec<u8>>;
    /// Exclusive advisory lock (`flock`) on `path`, held until the guard drops.
    /// Released by the kernel if the process dies, so crashes stay recoverable.
    fn lock_exclusive(&self, path: &Path) -> Result<FileLock>;
    /// Whether another live process holds the lock on `path`.
    fn is_locked(&self, path: &Path) -> Result<bool>;
}

/// Guard of [`CowFs::lock_exclusive`].
#[derive(Debug)]
pub struct FileLock {
    _file: Option<std::fs::File>,
}

impl FileLock {
    #[must_use]
    pub fn new(file: std::fs::File) -> Self {
        Self { _file: Some(file) }
    }

    /// A guard that holds nothing (fakes, platforms without locking).
    #[must_use]
    pub fn detached() -> Self {
        Self { _file: None }
    }
}
