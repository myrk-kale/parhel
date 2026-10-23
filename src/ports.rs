//! Ports: the side effects the application layer needs, as traits.
//!
//! Production adapters live in [`crate::git`] and [`crate::fs`]; unit tests drive the
//! application layer through recording fakes (`app::fakes`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::domain::store::EntryMeta;
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
    /// `git worktree lock --reason <reason> <path>`: `git worktree prune`, and the `gc`
    /// that runs it, leave a locked worktree's registration alone even when its
    /// directory is out of reach (a store whose mount is down).
    fn worktree_lock(&self, repo: &Path, path: &Path, reason: &str) -> Result<()>;
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
    /// Identity of the filesystem that holds the path, equal for two paths that can
    /// share storage even when their `device` differs (btrfs subvolumes have one
    /// `st_dev` each). Where the adapter cannot tell better it is the `device`.
    pub filesystem: u64,
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
    /// Entries of the directory `root/rel` of the source tree, reached from `root`
    /// without following any symlink; `None` when it is not a directory (a symlink
    /// to one is not) or is gone.
    fn list_dir_within(&self, root: &Path, rel: &Path) -> Result<Option<Vec<OsString>>>;
    /// Clone the entry `root/rel` (file, symlink, FIFO or directory tree) to the new
    /// path `dst`: copy-on-write, symlinks never followed, metadata preserved. The
    /// entry is reached from `root` by descending one component at a time without
    /// following symlinks, so a directory swapped for a symlink meanwhile is an error
    /// and never a way out of the source. Errors: `clone-failed`, `cross-volume`.
    fn clone_within(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()>;
    /// The `--copy` flavour of [`clone_within`](Self::clone_within): same contract but
    /// every file's data is copied in full, never shared, and the volumes need not
    /// support copy-on-write nor be the same. Errors: `clone-failed`.
    fn copy_within(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()>;
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

    // --- the store (Linux): sync of the baseline, the symlink at the destination.

    /// `lstat` of the entry `root/rel` (`rel` empty: `root` itself), reached from
    /// `root` without following any symlink; `None` when it does not exist (or is a
    /// socket or device node, which are not part of a worktree).
    fn stat_within(&self, root: &Path, rel: &Path) -> Result<Option<EntryMeta>>;
    /// Make `dst` a copy of the source entry `root/rel`, replacing what `dst` holds
    /// when it is of the same kind: files are copied in full (never reflinked: the
    /// baseline is on another filesystem) through a temporary name and renamed over
    /// `dst`; symlinks and FIFOs are recreated; a directory is created empty when
    /// missing (its children are separate calls). Returns the bytes copied.
    /// Metadata that depends on the children (directory mtime) is left to
    /// [`mirror_dir_meta`](Self::mirror_dir_meta).
    fn mirror_entry(&self, root: &Path, rel: &Path, dst: &Path) -> Result<u64>;
    /// Give the directory `dst` the mode, ownership-independent metadata (xattrs,
    /// ACLs, mtime) of the source directory `root/rel`; called once its children are
    /// in place.
    fn mirror_dir_meta(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()>;
    /// `symlink(target, link)` that never replaces anything. Errors: `dest-exists`
    /// when `link` exists (even as a dangling symlink).
    fn symlink_exclusive(&self, target: &Path, link: &Path) -> Result<()>;
    /// Target of the symlink at `path`; `None` when it is not a symlink or is gone.
    fn read_link(&self, path: &Path) -> Result<Option<PathBuf>>;
    /// Append `bytes` to `path` (created if missing) and sync it.
    fn append_file(&self, path: &Path, bytes: &[u8]) -> Result<()>;
    /// Make everything written to the filesystem holding `path` durable, and report a
    /// write that failed after it was accepted (`syncfs`: `ENOSPC` and `EIO` of delayed
    /// allocation surface here and nowhere else).
    fn flush(&self, path: &Path) -> Result<()>;
    /// Whether the current user can create entries in the directory `path`.
    fn is_writable(&self, path: &Path) -> Result<bool>;
    /// Free and total space of the filesystem holding `path` (`statvfs`).
    fn space(&self, path: &Path) -> Result<Space>;
}

/// Space of a filesystem, in bytes (what an unprivileged user can use).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

/// A local account, as `setup` needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// Owner of something `setup` creates: `None` is root.
pub type Owner = Option<(u32, u32)>;

/// Size of an image file: what it claims and what it occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageUsage {
    pub apparent: u64,
    pub allocated: u64,
}

/// What is mounted at a store's mount point, judged against the image it should be from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountBacking {
    /// Nothing is mounted there.
    NotMounted,
    /// A loop device whose backing file is the image (also when that file was deleted).
    Image,
    /// Anything else, described for the message: a `tmpfs`, another disk, a loop device
    /// backed by a different file.
    Other(String),
}

/// Everything `git parhel setup` asks of the machine: privileged system calls,
/// programs, systemd. Reads are separate from mutations so a fake can log only the
/// latter.
pub trait Host {
    fn is_linux(&self) -> bool;
    fn is_root(&self) -> bool;
    /// An environment variable of the process (`SUDO_USER`).
    fn env_var(&self, name: &str) -> Option<String>;
    /// `getpwnam`.
    fn user(&self, name: &str) -> Result<Option<UserInfo>>;
    /// Whether `tool` is found in the directories root searches.
    fn tool_exists(&self, tool: &str) -> bool;
    /// Whether systemd is PID 1 (`systemctl` can manage units).
    fn systemd_running(&self) -> bool;
    /// Contents of a text file (`/proc/filesystems`, a unit, the marker); `None` when missing.
    fn read_text(&self, path: &Path) -> Result<Option<String>>;
    /// `lstat` succeeds.
    fn exists(&self, path: &Path) -> bool;
    /// Names in a directory; empty when it does not exist.
    fn list_dir(&self, path: &Path) -> Result<Vec<OsString>>;
    fn is_mount_point(&self, path: &Path) -> bool;
    /// What backs the filesystem mounted at `mount_point`, against `image`: only a loop
    /// device whose backing file is the image is the store's own. Anything else mounted
    /// there is somebody's, and `setup` must not lay out, protect or unmount it.
    fn mount_backing(&self, mount_point: &Path, image: &Path) -> Result<MountBacking>;
    /// Whether the immutable attribute (`chattr +i`) is set on `path`. A filesystem
    /// that cannot say answers `false`.
    fn is_immutable(&self, path: &Path) -> bool;
    /// What `blkid -p` finds on the image: the filesystem type, or `None` when it has
    /// no signature at all (blank, or an earlier setup that stopped before `mkfs`).
    fn filesystem_of(&self, image: &Path) -> Result<Option<String>>;
    /// Run a program to completion, returning its stdout. Errors carry its stderr.
    fn run(&self, program: &str, args: &[&str]) -> Result<String>;
    /// Create the directory when missing; either way set its mode and owner.
    fn create_dir(&self, path: &Path, mode: u32, owner: Owner) -> Result<()>;
    /// Write a file (replacing), with the mode and owner given.
    fn write_file(&self, path: &Path, contents: &[u8], mode: u32, owner: Owner) -> Result<()>;
    /// Create the image file, empty, exclusively (mode 0600, root's): an existing file
    /// (or link) is an error.
    fn create_image(&self, path: &Path) -> Result<()>;
    /// Give the image its size: `ftruncate` up to `bytes` (a sparse file; never smaller
    /// than it is), and with `preallocate` `fallocate` the whole size. Idempotent, so a run
    /// that stopped half way is finished by running it again.
    fn size_image(&self, path: &Path, bytes: u64, preallocate: bool) -> Result<()>;
    /// Run `f` with SIGINT, SIGTERM and SIGHUP held back: a signal that arrives meanwhile
    /// is delivered after `f` returns. For the few moments a Ctrl-C would leave something
    /// half done that matters.
    fn with_signals_deferred(&self, f: &mut dyn FnMut() -> Result<()>) -> Result<()>;
    fn image_usage(&self, path: &Path) -> Result<ImageUsage>;
    /// After `chattr +i` on `dir`: removing `image` and creating entries beside it fail
    /// with `EPERM` (renaming goes through the same two checks) while opening the image
    /// for writing still works. The probes never change the image, so a directory that
    /// turns out not to be protected loses nothing.
    fn verify_immutable(&self, dir: &Path, image: &Path) -> Result<()>;
    fn remove_file(&self, path: &Path) -> Result<()>;
    /// Remove an empty directory.
    fn remove_dir(&self, path: &Path) -> Result<()>;
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
