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

use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Mutex, PoisonError};

use super::unix;
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

/// Clone (or copy) `src`, a file, symlink, FIFO or directory tree, to the new path
/// `dst`. Fails, touching nothing, when `dst` exists. Errors: `cross-volume` when a
/// reflink crosses filesystems, `clone-failed` otherwise, naming the entry that failed.
/// Whatever was created before a failure is removed again.
pub(crate) fn transfer(src: &Path, dst: &Path, data: Data) -> Result<()> {
    run(src, dst, data).map_err(|failure| {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink,
    Fifo,
    /// Sockets and device nodes: not part of a worktree.
    Other,
}

fn kind_of(ft: fs::FileType) -> Kind {
    if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else if ft.is_symlink() {
        Kind::Symlink
    } else if ft.is_fifo() {
        Kind::Fifo
    } else {
        Kind::Other
    }
}

fn run(src: &Path, dst: &Path, data: Data) -> Step {
    let md = at(src, fs::symlink_metadata(src))?;
    match kind_of(md.file_type()) {
        Kind::Dir => copy_dir(src, dst, md, data),
        Kind::Other => Ok(()),
        kind => copy_leaf(src, dst, kind, data, &mut Vec::new()),
    }
}

// ------------------------------------------------------------------ leaves

fn copy_leaf(src: &Path, dst: &Path, kind: Kind, data: Data, buf: &mut Vec<u8>) -> Step {
    match kind {
        Kind::File => copy_file(src, dst, data, buf),
        Kind::Symlink => {
            let md = at(src, fs::symlink_metadata(src))?;
            let target = at(src, fs::read_link(src))?;
            at(dst, std::os::unix::fs::symlink(target, dst))?;
            undo_on_error(dst, restore(src, dst, &md, kind))
        }
        Kind::Fifo => {
            let md = at(src, fs::symlink_metadata(src))?;
            let c = at(dst, cstr(dst))?;
            // SAFETY: valid C string.
            if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
                return at(dst, Err(io::Error::last_os_error()));
            }
            undo_on_error(dst, restore(src, dst, &md, kind))
        }
        Kind::Dir | Kind::Other => Ok(()),
    }
}

fn copy_file(src: &Path, dst: &Path, data: Data, buf: &mut Vec<u8>) -> Step {
    let input = at(
        src,
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(src),
    )?;
    // The listing may be stale: only a regular file is copied.
    let md = at(src, input.metadata())?;
    if !md.is_file() {
        return at(
            src,
            Err(io::Error::other("changed while it was being copied")),
        );
    }
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
        Data::Reflink(clone) => at(src, clone(&input, &output)),
        Data::Copy => at(src, copy_data(&input, &output, md.len(), buf)),
    };
    drop(output);
    undo_on_error(
        dst,
        filled.and_then(|()| restore(src, dst, &md, Kind::File)),
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

/// Give `dst` everything about `src` that is not its data: extended attributes,
/// owner (when permitted), mode and times (nanoseconds). Last step of every entry:
/// nothing may touch `dst` afterwards.
fn restore(src: &Path, dst: &Path, md: &fs::Metadata, kind: Kind) -> Step {
    copy_xattrs(src, dst)?;
    let c = at(dst, cstr(dst))?;
    if owner_differs(md) {
        // SAFETY: valid C string; lchown never follows symlinks. Not permitted for
        // other owners unless root: the copy then belongs to whoever made it.
        unsafe { libc::lchown(c.as_ptr(), md.uid(), md.gid()) };
    }
    if kind != Kind::Symlink {
        // After the owner: changing it clears setuid and setgid.
        // SAFETY: valid C string.
        let mode = libc::mode_t::try_from(md.mode() & 0o7777).unwrap_or(0o600);
        if unsafe { libc::chmod(c.as_ptr(), mode) } != 0 {
            return at(dst, Err(io::Error::last_os_error()));
        }
    }
    let times = [
        libc::timespec {
            tv_sec: md.atime(),
            tv_nsec: md.atime_nsec(),
        },
        libc::timespec {
            tv_sec: md.mtime(),
            tv_nsec: md.mtime_nsec(),
        },
    ];
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

fn owner_differs(md: &fs::Metadata) -> bool {
    // SAFETY: no arguments, cannot fail.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    md.uid() != uid || md.gid() != gid
}

/// BSD file flags a user may set: `nodump`, `uchg`, `uappnd`, `opaque`, `hidden`.
/// (Not `compressed` and the like, which the filesystem decides.)
#[cfg(target_os = "macos")]
fn restore_flags(c: &CString, md: &fs::Metadata) {
    use std::os::macos::fs::MetadataExt as _;
    const SETTABLE: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x8000;
    let flags = md.st_flags() & SETTABLE;
    if flags != 0 {
        // SAFETY: valid C string; lchflags never follows symlinks.
        unsafe { libc::lchflags(c.as_ptr(), flags) };
    }
}

fn copy_xattrs(src: &Path, dst: &Path) -> Step {
    let from = at(src, cstr(src))?;
    let names = match xattr::list(&from) {
        Ok(names) => names,
        Err(e) if xattr::unsupported(&e) => return Ok(()),
        Err(e) => return at(src, Err(e)),
    };
    let to = at(dst, cstr(dst))?;
    for name in names.split(|&b| b == 0).filter(|n| xattr::wanted(n)) {
        let name = CString::new(name).expect("split at NUL");
        let value = match xattr::get(&from, &name) {
            Ok(value) => value,
            // Removed meanwhile.
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => continue,
            Err(e) => return at(src, Err(e)),
        };
        if let Err(e) = xattr::set(&to, &name, &value) {
            if !xattr::tolerated(&e) {
                return at(dst, Err(e));
            }
        }
    }
    Ok(())
}

/// Extended attributes of an entry itself (never of a symlink's target).
mod xattr {
    use std::ffi::CStr;
    use std::io;

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

    /// Names of the attributes, each followed by a NUL.
    pub(super) fn list(path: &CStr) -> io::Result<Vec<u8>> {
        read_sized(|buf, size| {
            // SAFETY: valid C string; `buf` is null or writable for `size` bytes.
            #[cfg(target_os = "linux")]
            let got = unsafe { libc::llistxattr(path.as_ptr(), buf.cast(), size) };
            // SAFETY: as above.
            #[cfg(target_os = "macos")]
            let got =
                unsafe { libc::listxattr(path.as_ptr(), buf.cast(), size, libc::XATTR_NOFOLLOW) };
            got
        })
    }

    pub(super) fn get(path: &CStr, name: &CStr) -> io::Result<Vec<u8>> {
        read_sized(|buf, size| {
            // SAFETY: valid C strings; `buf` is null or writable for `size` bytes.
            #[cfg(target_os = "linux")]
            let got = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), buf, size) };
            // SAFETY: as above.
            #[cfg(target_os = "macos")]
            let got = unsafe {
                libc::getxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    buf,
                    size,
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            };
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

    /// The filesystem has no extended attributes.
    pub(super) fn unsupported(e: &io::Error) -> bool {
        e.raw_os_error() == Some(libc::ENOTSUP)
    }

    /// Linux: only `user.*` belongs to the user (`security.*`, `system.*` and
    /// `trusted.*` are the kernel's and the administrator's). macOS: everything.
    pub(super) fn wanted(name: &[u8]) -> bool {
        !name.is_empty() && (cfg!(target_os = "macos") || name.starts_with(b"user."))
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
    src: PathBuf,
    dst: PathBuf,
    md: fs::Metadata,
}

/// Entries of one directory (a slice of it) to copy by one worker.
struct Job {
    src: PathBuf,
    dst: PathBuf,
    entries: Vec<(OsString, Kind)>,
}

/// Entries per job: small enough to spread a huge directory over the workers.
const JOB_ENTRIES: usize = 256;

fn mkdir(dst: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(dst)
}

fn copy_dir(src: &Path, dst: &Path, md: fs::Metadata, data: Data) -> Step {
    // Fails when `dst` exists: nothing has been created yet, nothing to undo.
    at(dst, mkdir(dst))?;
    let mut dirs = vec![Dir {
        src: src.to_path_buf(),
        dst: dst.to_path_buf(),
        md,
    }];
    let done = fill(&mut dirs, data).and_then(|()| finish(&dirs));
    if done.is_err() {
        let _ = unix::remove_dir_all(dst, |path| unix::make_dirs_writable(path, &|_| {}));
    }
    done
}

/// Create every directory below the first of `dirs` (appended to it, parents before
/// children) and copy the other entries, in parallel.
fn fill(dirs: &mut Vec<Dir>, data: Data) -> Step {
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
                    if let Err(failure) = run_job(&job, data, &mut buf) {
                        record(failure);
                    }
                }
            });
        }
        if let Err(failure) = list_and_make_dirs(dirs, &tx, &failed) {
            record(failure);
        }
        drop(tx);
    });
    first
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .map_or(Ok(()), Err)
}

fn run_job(job: &Job, data: Data, buf: &mut Vec<u8>) -> Step {
    for (name, kind) in &job.entries {
        copy_leaf(&job.src.join(name), &job.dst.join(name), *kind, data, buf)?;
    }
    Ok(())
}

/// Breadth-first over the directories (`dirs` doubles as the queue): create each
/// subdirectory in the destination and hand the other entries to the workers.
fn list_and_make_dirs(
    dirs: &mut Vec<Dir>,
    tx: &std::sync::mpsc::SyncSender<Job>,
    failed: &AtomicBool,
) -> Step {
    let mut next = 0;
    while next < dirs.len() && !failed.load(Ordering::SeqCst) {
        let (src, dst) = (dirs[next].src.clone(), dirs[next].dst.clone());
        next += 1;
        let mut entries = Vec::new();
        let send = |entries: &mut Vec<(OsString, Kind)>| {
            let job = Job {
                src: src.clone(),
                dst: dst.clone(),
                entries: std::mem::take(entries),
            };
            // Only fails when every worker is gone, which only a failure causes.
            let _ = tx.send(job);
        };
        for entry in at(&src, fs::read_dir(&src))? {
            let entry = at(&src, entry)?;
            let name = entry.file_name();
            let kind = kind_of(at(&entry.path(), entry.file_type())?);
            match kind {
                Kind::Dir => {
                    let (child_src, child_dst) = (src.join(&name), dst.join(&name));
                    let md = at(&child_src, fs::symlink_metadata(&child_src))?;
                    at(&child_dst, mkdir(&child_dst))?;
                    dirs.push(Dir {
                        src: child_src,
                        dst: child_dst,
                        md,
                    });
                }
                Kind::Other => {}
                _ => {
                    entries.push((name, kind));
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
fn finish(dirs: &[Dir]) -> Step {
    for dir in dirs.iter().rev() {
        restore(&dir.src, &dir.dst, &dir.md, Kind::Dir)?;
    }
    Ok(())
}
