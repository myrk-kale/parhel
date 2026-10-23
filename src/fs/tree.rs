//! Recursive clone or copy of one filesystem entry, shared by the macOS (`--copy`)
//! and Linux (reflink and `--copy`) adapters.
//!
//! A directory is rebuilt by walking it: every directory is created owner-only so
//! it can be filled whatever the source's mode, files and the other entries are
//! handled by a few threads, and once everything is in place the directories get
//! their extended attributes, ownership, mode and times, deepest first (filling a
//! directory changes its mtime, and a read-only one cannot be filled). Symlinks are
//! recreated, never followed; FIFOs are recreated; sockets and device nodes are
//! skipped, as git does. Hard links become independent files.
//!
//! The source is reached through directory descriptors (see [`super::anchor`]): the
//! root is opened once and every directory below it with `O_NOFOLLOW`, so a
//! component swapped for a symlink while the walk runs is an error and never a way
//! to copy files from outside the source. A regular file that became a FIFO is
//! refused without ever blocking on it. The walk looks at the process's interrupt
//! flag before each entry and while it copies data, so Ctrl-C stops a long copy. The
//! destination is a staging directory only we can reach, and stays path-based.

use std::ffi::{CStr, CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Mutex, PoisonError};

use super::{anchor, unix};
use crate::domain::{Error, ErrorCode, Result};

/// How the data of regular files reaches the destination.
#[derive(Clone, Copy)]
pub(crate) enum Data {
    /// Every byte is read and written again: nothing is shared with the source.
    Copy,
    /// The destination file shares the source's storage; the function makes the
    /// (empty, just created) destination a reflink of the source (`FICLONE`).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Reflink(fn(src: &File, dst: &File) -> io::Result<()>),
}

impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Copy => "Copy",
            Self::Reflink(_) => "Reflink",
        })
    }
}

/// Clone (or copy) `root/rel`, a file, symlink, FIFO or directory tree, to the new
/// path `dst`. Fails, touching nothing, when `dst` exists. Errors: `cross-volume`
/// when a reflink crosses filesystems, `interrupted` on a signal, `clone-failed`
/// otherwise, naming the entry that failed. Whatever was created before a failure is
/// removed again.
pub(crate) fn transfer(root: &Path, rel: &Path, dst: &Path, data: Data) -> Result<()> {
    run(root, rel, dst, data).map_err(|failure| {
        if failure
            .error
            .get_ref()
            .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Cancelled>)
        {
            return Error::new(ErrorCode::Interrupted, "interrupted; rolled back");
        }
        let (verb, code) = match data {
            Data::Copy => ("copy", ErrorCode::CloneFailed),
            Data::Reflink(_) if failure.error.raw_os_error() == Some(libc::EXDEV) => {
                ("clone", ErrorCode::CrossVolume)
            }
            Data::Reflink(_) => ("clone", ErrorCode::CloneFailed),
        };
        Error::new(
            code,
            format!(
                "cannot {verb} `{}`: {}",
                failure.path.display(),
                failure.error
            ),
        )
    })
}

/// What went wrong and on which entry.
struct Failure {
    path: PathBuf,
    error: io::Error,
}

type Step = std::result::Result<(), Failure>;

fn at<T>(path: &Path, result: io::Result<T>) -> std::result::Result<T, Failure> {
    result.map_err(|error| Failure {
        path: path.to_path_buf(),
        error,
    })
}

/// Like [`at`] for something done to the source: a component that is no longer what
/// the listing said (`ELOOP`, `ENOTDIR`) reads as "changed", not as a bare errno.
fn at_src<T>(path: &Path, result: io::Result<T>) -> std::result::Result<T, Failure> {
    at(
        path,
        result.map_err(|e| {
            if anchor::changed(&e) {
                io::Error::other("changed while it was being copied")
            } else {
                e
            }
        }),
    )
}

/// The walk was interrupted by a signal.
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Cancelled {}

fn check_interrupt() -> io::Result<()> {
    if crate::signals::raised() {
        Err(io::Error::other(Cancelled))
    } else {
        Ok(())
    }
}

/// Before the entry `rel` (relative to the source root) is opened: the failpoint of
/// the walk, then the interrupt flag.
fn enter(rel: &Path) -> Step {
    let hit = || -> Result<()> {
        crate::failpoint!("walk_entry", rel);
        Ok(())
    };
    at(rel, hit().map_err(io::Error::other))?;
    at(rel, check_interrupt())
}

/// What an entry is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink,
    Fifo,
    /// Sockets and device nodes: not part of a worktree.
    Other,
}

fn kind_of_mode(mode: libc::mode_t) -> Kind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => Kind::Dir,
        libc::S_IFREG => Kind::File,
        libc::S_IFLNK => Kind::Symlink,
        libc::S_IFIFO => Kind::Fifo,
        _ => Kind::Other,
    }
}

/// The kind a listing reports; `None` when the filesystem does not say.
fn kind_of_dirent(d_type: u8) -> Option<Kind> {
    match d_type {
        libc::DT_DIR => Some(Kind::Dir),
        libc::DT_REG => Some(Kind::File),
        libc::DT_LNK => Some(Kind::Symlink),
        libc::DT_FIFO => Some(Kind::Fifo),
        libc::DT_UNKNOWN => None,
        _ => Some(Kind::Other),
    }
}

/// What `stat` says about an entry that matters here.
#[derive(Clone, Copy)]
struct Meta {
    mode: libc::mode_t,
    uid: libc::uid_t,
    gid: libc::gid_t,
    atime: libc::timespec,
    mtime: libc::timespec,
    size: u64,
    /// BSD file flags.
    #[cfg(target_os = "macos")]
    flags: u32,
}

impl Meta {
    fn new(st: &libc::stat) -> Self {
        Self {
            mode: st.st_mode,
            uid: st.st_uid,
            gid: st.st_gid,
            atime: libc::timespec {
                tv_sec: st.st_atime,
                tv_nsec: st.st_atime_nsec,
            },
            mtime: libc::timespec {
                tv_sec: st.st_mtime,
                tv_nsec: st.st_mtime_nsec,
            },
            size: u64::try_from(st.st_size).unwrap_or(0),
            #[cfg(target_os = "macos")]
            flags: st.st_flags,
        }
    }

    fn kind(&self) -> Kind {
        kind_of_mode(self.mode)
    }
}

/// The source side of a walk: the root, opened once, and its path for messages.
#[derive(Clone, Copy)]
struct Source<'a> {
    root: &'a Path,
    fd: RawFd,
}

impl Source<'_> {
    /// The entry `rel` as a path, for messages only.
    fn shown(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }

    fn open_dir(&self, rel: &Path) -> std::result::Result<OwnedFd, Failure> {
        at_src(&self.shown(rel), anchor::open_rel_dir(self.fd, rel))
    }
}

fn run(root: &Path, rel: &Path, dst: &Path, data: Data) -> Step {
    let root_fd = at(root, anchor::open_root(root))?;
    let source = Source {
        root,
        fd: root_fd.as_raw_fd(),
    };
    let shown = source.shown(rel);
    enter(rel)?;
    let (parent, name) = at_src(&shown, anchor::open_parent(source.fd, rel))?;
    let st = at_src(&shown, anchor::stat_at(parent.as_raw_fd(), &name))?;
    match kind_of_mode(st.st_mode) {
        Kind::Dir => copy_dir(source, rel, dst, data),
        Kind::Other => Ok(()),
        kind => copy_leaf(
            parent.as_raw_fd(),
            &name,
            &shown,
            dst,
            kind,
            data,
            &mut Vec::new(),
        ),
    }
}

// ------------------------------------------------------------------ leaves

/// Copy the non-directory `name` of the open directory `parent` to `dst`; `shown`
/// is its path for messages.
fn copy_leaf(
    parent: RawFd,
    name: &CStr,
    shown: &Path,
    dst: &Path,
    kind: Kind,
    data: Data,
    buf: &mut Vec<u8>,
) -> Step {
    match kind {
        Kind::File => copy_file(parent, name, shown, dst, data, buf),
        Kind::Symlink => {
            let md = Meta::new(&at_src(shown, anchor::stat_at(parent, name))?);
            let target = at_src(shown, anchor::read_link_at(parent, name))?;
            at(dst, std::os::unix::fs::symlink(target, dst))?;
            // Only macOS symlinks carry attributes (and can be opened for them).
            #[cfg(target_os = "macos")]
            let held = Some(at_src(shown, anchor::open_symlink_at(parent, name))?);
            #[cfg(not(target_os = "macos"))]
            let held: Option<OwnedFd> = None;
            let done = restore(shown, held.as_ref().map(AsRawFd::as_raw_fd), dst, &md, kind);
            undo_on_error(dst, done)
        }
        Kind::Fifo => {
            let held = at_src(shown, anchor::open_node_at(parent, name))?;
            let md = Meta::new(&at(shown, anchor::fstat(held.as_raw_fd()))?);
            if md.kind() != Kind::Fifo {
                return at(
                    shown,
                    Err(io::Error::other("changed while it was being copied")),
                );
            }
            let c = at(dst, cstr(dst))?;
            // SAFETY: valid C string.
            if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
                return at(dst, Err(io::Error::last_os_error()));
            }
            let done = restore(shown, Some(held.as_raw_fd()), dst, &md, kind);
            undo_on_error(dst, done)
        }
        Kind::Dir | Kind::Other => Ok(()),
    }
}

fn copy_file(
    parent: RawFd,
    name: &CStr,
    shown: &Path,
    dst: &Path,
    data: Data,
    buf: &mut Vec<u8>,
) -> Step {
    // Opened without blocking: the listing may be stale and a FIFO in its place
    // would hold `open` forever. Only a regular file is copied.
    let input = File::from(at_src(shown, anchor::open_node_at(parent, name))?);
    let md = Meta::new(&at(shown, anchor::fstat(input.as_raw_fd()))?);
    if md.kind() != Kind::File {
        return at(
            shown,
            Err(io::Error::other("changed while it was being copied")),
        );
    }
    at(shown, anchor::make_blocking(input.as_raw_fd()))?;
    // Owner-writable until the end: attributes are set before the final mode.
    let output = at(
        dst,
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dst),
    )?;
    let filled = match data {
        Data::Reflink(clone) => at(shown, clone(&input, &output)),
        Data::Copy => at(shown, copy_data(&input, &output, md.size, buf)),
    };
    drop(output);
    undo_on_error(
        dst,
        filled.and_then(|()| restore(shown, Some(input.as_raw_fd()), dst, &md, Kind::File)),
    )
}

/// Remove `dst` (just created by the caller) when `result` is a failure.
fn undo_on_error(dst: &Path, result: Step) -> Step {
    if result.is_err() {
        let _ = fs::remove_file(dst);
    }
    result
}

/// Below this size a file is copied straight through, without asking where its
/// holes are.
const SPARSE_MIN: u64 = 64 * 1024;
const BUFFER: usize = 256 * 1024;

/// Copy the bytes of `input` into `output`, keeping holes of large sparse files.
/// Plain `read`/`write`: `copy_file_range` and friends would reflink where the
/// filesystem can, and a copy must not share storage.
fn copy_data(input: &File, output: &File, len: u64, buf: &mut Vec<u8>) -> io::Result<()> {
    if buf.is_empty() {
        buf.resize(BUFFER, 0);
    }
    if len >= SPARSE_MIN && copy_extents(input, output, len, buf)? {
        return Ok(());
    }
    let (mut reader, mut writer) = (input, output);
    loop {
        check_interrupt()?;
        let n = match reader.read(buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        writer.write_all(&buf[..n])?;
    }
}

/// `lseek(fd, offset, whence)`: `Ok(None)` past the last data (`ENXIO`).
fn seek(file: &File, offset: u64, whence: libc::c_int) -> io::Result<Option<u64>> {
    let offset = libc::off_t::try_from(offset).map_err(io::Error::other)?;
    // SAFETY: valid open fd.
    let found = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if found >= 0 {
        return Ok(u64::try_from(found).ok());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ENXIO) {
        Ok(None)
    } else {
        Err(e)
    }
}

/// Copy only the data extents of `input`, then extend `output` to `len`. `false`
/// when the filesystem cannot tell holes from data (nothing was written then).
fn copy_extents(input: &File, output: &File, len: u64, buf: &mut [u8]) -> io::Result<bool> {
    let mut pos = 0;
    let mut first = true;
    loop {
        let data = match seek(input, pos, libc::SEEK_DATA) {
            Ok(Some(data)) => data,
            Ok(None) => break,
            Err(_) if first => return Ok(false),
            Err(e) => return Err(e),
        };
        first = false;
        let hole = seek(input, data, libc::SEEK_HOLE)?.unwrap_or(len);
        let mut offset = data;
        while offset < hole {
            check_interrupt()?;
            let want = usize::try_from(hole - offset).map_or(buf.len(), |n| n.min(buf.len()));
            let n = input.read_at(&mut buf[..want], offset)?;
            if n == 0 {
                // Shorter than it was: nothing more to copy.
                break;
            }
            output.write_all_at(&buf[..n], offset)?;
            offset += n as u64;
        }
        pos = hole;
    }
    if output.metadata()?.len() < len {
        output.set_len(len)?;
    }
    Ok(true)
}

// ------------------------------------------------------------------ metadata

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Give `dst` everything about the source entry that is not its data: extended
/// attributes (read through `src`, the entry opened by the walk), owner (when
/// permitted), mode and times (nanoseconds). Last step of every entry: nothing may
/// touch `dst` afterwards.
fn restore(shown: &Path, src: Option<RawFd>, dst: &Path, md: &Meta, kind: Kind) -> Step {
    if let Some(src) = src {
        copy_xattrs(shown, src, dst, kind)?;
    }
    let c = at(dst, cstr(dst))?;
    if owner_differs(md) {
        // SAFETY: valid C string; lchown never follows symlinks. Not permitted for
        // other owners unless root: the copy then belongs to whoever made it.
        unsafe { libc::lchown(c.as_ptr(), md.uid, md.gid) };
    }
    if kind != Kind::Symlink {
        // After the owner: changing it clears setuid and setgid. After the access
        // ACL, which rewrites the group bits: the source's mode is the final word.
        // SAFETY: valid C string.
        if unsafe { libc::chmod(c.as_ptr(), md.mode & 0o7777) } != 0 {
            return at(dst, Err(io::Error::last_os_error()));
        }
    }
    let times = [md.atime, md.mtime];
    // SAFETY: valid C string and two timespecs.
    let set = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if set != 0 {
        return at(dst, Err(io::Error::last_os_error()));
    }
    #[cfg(target_os = "macos")]
    restore_flags(&c, md);
    Ok(())
}

fn owner_differs(md: &Meta) -> bool {
    // SAFETY: no arguments, cannot fail.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    md.uid != uid || md.gid != gid
}

/// BSD file flags a user may set: `nodump`, `uchg`, `uappnd`, `opaque`, `hidden`.
/// (Not `compressed` and the like, which the filesystem decides.)
#[cfg(target_os = "macos")]
fn restore_flags(c: &CString, md: &Meta) {
    const SETTABLE: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x8000;
    let flags = md.flags & SETTABLE;
    if flags != 0 {
        // SAFETY: valid C string; lchflags never follows symlinks.
        unsafe { libc::lchflags(c.as_ptr(), flags) };
    }
}

fn copy_xattrs(shown: &Path, src: RawFd, dst: &Path, kind: Kind) -> Step {
    let names = match xattr::list(src) {
        Ok(names) => names,
        Err(e) if xattr::unsupported(&e) => Vec::new(),
        Err(e) => return at(shown, Err(e)),
    };
    let to = at(dst, cstr(dst))?;
    let mut present = Vec::new();
    for name in names.split(|&b| b == 0).filter(|n| xattr::wanted(n)) {
        let name = CString::new(name).expect("split at NUL");
        let value = match xattr::get(src, &name) {
            Ok(value) => value,
            // Removed meanwhile.
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => continue,
            Err(e) => return at(shown, Err(e)),
        };
        if let Err(e) = xattr::set(&to, &name, &value) {
            if !xattr::tolerated(&e) {
                return at(dst, Err(e));
            }
        }
        present.push(name);
    }
    // What the source does not have, the copy must not either: an entry created in
    // a directory with a default ACL inherited one, and with it access the source
    // never gave.
    xattr::drop_inherited(&to, &present, kind == Kind::Dir);
    Ok(())
}

/// Extended attributes of an entry itself (never of a symlink's target).
mod xattr {
    use std::ffi::CStr;
    use std::io;
    use std::os::fd::RawFd;

    /// POSIX ACLs on Linux are the attributes `system.posix_acl_access` (and, on a
    /// directory, `system.posix_acl_default`, which new entries inherit).
    #[cfg(target_os = "linux")]
    const ACL_NAMES: [&[u8]; 2] = [b"system.posix_acl_access", b"system.posix_acl_default"];

    /// Run `call(buffer, size)` (a `*xattr` syscall) with a buffer of the size it asks
    /// for, again if the value grew meanwhile.
    fn read_sized(call: impl Fn(*mut libc::c_void, usize) -> isize) -> io::Result<Vec<u8>> {
        loop {
            let Ok(need) = usize::try_from(call(std::ptr::null_mut(), 0)) else {
                return Err(io::Error::last_os_error());
            };
            if need == 0 {
                return Ok(Vec::new());
            }
            let mut buf = vec![0u8; need];
            if let Ok(got) = usize::try_from(call(buf.as_mut_ptr().cast(), buf.len())) {
                buf.truncate(got);
                return Ok(buf);
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ERANGE) {
                return Err(e);
            }
        }
    }

    /// Names of the attributes of the open entry `fd`, each followed by a NUL.
    pub(super) fn list(fd: RawFd) -> io::Result<Vec<u8>> {
        read_sized(|buf, size| {
            // SAFETY: valid descriptor; `buf` is null or writable for `size` bytes.
            #[cfg(target_os = "linux")]
            let got = unsafe { libc::flistxattr(fd, buf.cast(), size) };
            // SAFETY: as above.
            #[cfg(target_os = "macos")]
            let got = unsafe { libc::flistxattr(fd, buf.cast(), size, 0) };
            got
        })
    }

    pub(super) fn get(fd: RawFd, name: &CStr) -> io::Result<Vec<u8>> {
        read_sized(|buf, size| {
            // SAFETY: valid descriptor and C string; `buf` is null or writable for
            // `size` bytes.
            #[cfg(target_os = "linux")]
            let got = unsafe { libc::fgetxattr(fd, name.as_ptr(), buf, size) };
            // SAFETY: as above.
            #[cfg(target_os = "macos")]
            let got = unsafe { libc::fgetxattr(fd, name.as_ptr(), buf, size, 0, 0) };
            got
        })
    }

    pub(super) fn set(path: &CStr, name: &CStr, value: &[u8]) -> io::Result<()> {
        // SAFETY: valid C strings; `value` is readable for its length.
        #[cfg(target_os = "linux")]
        let rc = unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        // SAFETY: as above.
        #[cfg(target_os = "macos")]
        let rc = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Remove the ACLs `path` has that the source did not (`present` are the
    /// attributes copied from it); errors mean there was nothing to remove.
    #[cfg(target_os = "linux")]
    pub(super) fn drop_inherited(path: &CStr, present: &[std::ffi::CString], dir: bool) {
        for name in ACL_NAMES {
            let default = name.ends_with(b"default");
            if (default && !dir) || present.iter().any(|p| p.as_bytes() == name) {
                continue;
            }
            let name = std::ffi::CString::new(name).expect("no NUL");
            // SAFETY: valid C strings.
            unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) };
        }
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::missing_const_for_fn)]
    pub(super) fn drop_inherited(_path: &CStr, _present: &[std::ffi::CString], _dir: bool) {}

    /// The filesystem has no extended attributes.
    pub(super) fn unsupported(e: &io::Error) -> bool {
        e.raw_os_error() == Some(libc::ENOTSUP)
    }

    /// Linux: `user.*` belongs to the user and the POSIX ACLs are access control
    /// (`security.*` and `trusted.*` are the kernel's and the administrator's).
    /// macOS: everything.
    pub(super) fn wanted(name: &[u8]) -> bool {
        if name.is_empty() {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            name.starts_with(b"user.") || ACL_NAMES.contains(&name)
        }
        #[cfg(target_os = "macos")]
        {
            true
        }
    }

    /// Attributes the destination cannot hold are dropped (a filesystem without
    /// them; on macOS, system-protected ones such as `com.apple.provenance`).
    pub(super) fn tolerated(e: &io::Error) -> bool {
        cfg!(target_os = "macos") || unsupported(e)
    }
}

// ------------------------------------------------------------------ directories

/// A directory created in the destination, to be finished once it is full.
struct Dir {
    /// Relative to the source root.
    rel: PathBuf,
    dst: PathBuf,
    /// Of the source directory as opened by the walk.
    md: Option<Meta>,
}

/// Entries of one directory (a slice of it) to copy by one worker.
struct Job {
    rel: PathBuf,
    dst: PathBuf,
    entries: Vec<(OsString, Kind)>,
}

/// Entries per job: small enough to spread a huge directory over the workers.
const JOB_ENTRIES: usize = 256;

fn mkdir(dst: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(dst)
}

/// Remove a partial result, immutable leftovers included.
fn discard(dst: &Path) {
    let _ = unix::remove_dir_all(dst, unix::make_removable);
}

fn copy_dir(source: Source<'_>, rel: &Path, dst: &Path, data: Data) -> Step {
    // Fails when `dst` exists: nothing has been created yet, nothing to undo.
    at(dst, mkdir(dst))?;
    let mut dirs = vec![Dir {
        rel: rel.to_path_buf(),
        dst: dst.to_path_buf(),
        md: None,
    }];
    let done = fill(source, &mut dirs, data).and_then(|()| finish(source, &dirs));
    if done.is_err() {
        discard(dst);
    }
    done
}

/// Create every directory below the first of `dirs` (appended to it, parents before
/// children) and copy the other entries, in parallel.
fn fill(source: Source<'_>, dirs: &mut Vec<Dir>, data: Data) -> Step {
    let failed = AtomicBool::new(false);
    let first = Mutex::new(None);
    let record = |failure: Failure| {
        failed.store(true, Ordering::SeqCst);
        first
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(failure);
    };
    let (tx, rx) = sync_channel::<Job>(256);
    let rx = Mutex::new(rx);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut buf = Vec::new();
                loop {
                    let job = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
                    let Ok(job) = job else { return };
                    if failed.load(Ordering::SeqCst) {
                        continue;
                    }
                    if let Err(failure) = run_job(source, &job, data, &mut buf) {
                        record(failure);
                    }
                }
            });
        }
        if let Err(failure) = list_and_make_dirs(source, dirs, &tx, &failed) {
            record(failure);
        }
        drop(tx);
    });
    first
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .map_or(Ok(()), Err)
}

fn run_job(source: Source<'_>, job: &Job, data: Data, buf: &mut Vec<u8>) -> Step {
    let dir = source.open_dir(&job.rel)?;
    for (name, kind) in &job.entries {
        let rel = job.rel.join(name);
        enter(&rel)?;
        let shown = source.shown(&rel);
        let c = at(&shown, anchor::cstring(name))?;
        copy_leaf(
            dir.as_raw_fd(),
            &c,
            &shown,
            &job.dst.join(name),
            *kind,
            data,
            buf,
        )?;
    }
    Ok(())
}

/// Breadth-first over the directories (`dirs` doubles as the queue): create each
/// subdirectory in the destination and hand the other entries to the workers.
fn list_and_make_dirs(
    source: Source<'_>,
    dirs: &mut Vec<Dir>,
    tx: &std::sync::mpsc::SyncSender<Job>,
    failed: &AtomicBool,
) -> Step {
    let mut next = 0;
    while next < dirs.len() && !failed.load(Ordering::SeqCst) {
        let (rel, dst) = (dirs[next].rel.clone(), dirs[next].dst.clone());
        let index = next;
        next += 1;
        let shown = source.shown(&rel);
        let dir = source.open_dir(&rel)?;
        dirs[index].md = Some(Meta::new(&at(&shown, anchor::fstat(dir.as_raw_fd()))?));
        let mut entries = Vec::new();
        let send = |entries: &mut Vec<(OsString, Kind)>| {
            let job = Job {
                rel: rel.clone(),
                dst: dst.clone(),
                entries: std::mem::take(entries),
            };
            // Only fails when every worker is gone, which only a failure causes.
            let _ = tx.send(job);
        };
        for entry in at_src(&shown, anchor::list(dir.as_raw_fd()))? {
            let child_rel = rel.join(&entry.name);
            let kind = if let Some(kind) = kind_of_dirent(entry.d_type) {
                kind
            } else {
                let c = at(&shown, anchor::cstring(&entry.name))?;
                let st = at_src(&child_rel, anchor::stat_at(dir.as_raw_fd(), &c))?;
                kind_of_mode(st.st_mode)
            };
            match kind {
                Kind::Dir => {
                    enter(&child_rel)?;
                    let child_dst = dst.join(&entry.name);
                    at(&child_dst, mkdir(&child_dst))?;
                    dirs.push(Dir {
                        rel: child_rel,
                        dst: child_dst,
                        md: None,
                    });
                }
                Kind::Other => {}
                _ => {
                    entries.push((entry.name, kind));
                    if entries.len() >= JOB_ENTRIES {
                        send(&mut entries);
                    }
                }
            }
        }
        if !entries.is_empty() {
            send(&mut entries);
        }
    }
    Ok(())
}

/// Attributes, owner, mode and times of every directory, deepest first.
fn finish(source: Source<'_>, dirs: &[Dir]) -> Step {
    for dir in dirs.iter().rev() {
        let Some(md) = dir.md else { continue };
        let held = source.open_dir(&dir.rel)?;
        restore(
            &source.shown(&dir.rel),
            Some(held.as_raw_fd()),
            &dir.dst,
            &md,
            Kind::Dir,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // U-FS-23: a partial copy is removed even when it holds read-only directories
    // and (macOS) immutable files.
    #[test]
    fn discard_removes_read_only_directories() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let partial = d.path().join("partial");
        fs::create_dir_all(partial.join("sub/deeper")).unwrap();
        fs::write(partial.join("sub/deeper/f"), b"x").unwrap();
        fs::set_permissions(
            partial.join("sub/deeper"),
            fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        fs::set_permissions(partial.join("sub"), fs::Permissions::from_mode(0o500)).unwrap();
        #[cfg(target_os = "macos")]
        {
            let st = std::process::Command::new("chflags")
                .arg("uchg")
                .arg(partial.join("sub/deeper/f"))
                .status()
                .unwrap();
            assert!(st.success());
        }
        discard(&partial);
        assert!(!partial.exists());
    }
}
