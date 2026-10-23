//! Reaching the source tree through directory descriptors.
//!
//! A path names a directory only until somebody replaces one of its components with
//! a symlink: `open("/src/a/b")` then follows it wherever it leads. The walk over the
//! source therefore opens the root once (`O_NOFOLLOW`) and descends with
//! `openat(dirfd, name, O_DIRECTORY | O_NOFOLLOW)`, one component at a time, and
//! looks at every entry through its parent's descriptor: a component swapped for a
//! symlink is an error (`ELOOP`/`ENOTDIR`), never a way out of the source.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

pub(crate) fn cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a fresh descriptor nobody else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Open the root of a walk. Its own final component is not followed; the path above
/// it is the caller's (a canonical path).
pub(crate) fn open_root(path: &Path) -> io::Result<OwnedFd> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let c = cstring(path.as_os_str())?;
    // SAFETY: valid C string.
    owned(unsafe { libc::open(c.as_ptr(), DIR_FLAGS) })
}

/// The directory `name` of `dir`, never through a symlink.
pub(crate) fn open_dir_at(dir: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    // SAFETY: valid descriptor and C string.
    owned(unsafe { libc::openat(dir, name.as_ptr(), DIR_FLAGS) })
}

/// The directory `root/rel`, descending one component at a time. `rel` must be a
/// plain relative path; the empty path is the root itself.
pub(crate) fn open_rel_dir(root: RawFd, rel: &Path) -> io::Result<OwnedFd> {
    let mut current: Option<OwnedFd> = None;
    for component in rel.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a plain relative path",
            ));
        };
        let at = current.as_ref().map_or(root, AsRawFd::as_raw_fd);
        current = Some(open_dir_at(at, &cstring(name)?)?);
    }
    match current {
        Some(dir) => Ok(dir),
        None => open_dir_at(root, c"."),
    }
}

/// The directory that holds `root/rel` (opened without following symlinks) and the
/// entry's name in it.
pub(crate) fn open_parent(root: RawFd, rel: &Path) -> io::Result<(OwnedFd, CString)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "not a plain relative path");
    let name = rel.file_name().ok_or_else(invalid)?;
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(invalid());
    }
    let parent = open_rel_dir(root, rel.parent().unwrap_or(Path::new("")))?;
    Ok((parent, cstring(name)?))
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`.
pub(crate) fn stat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid descriptor, C string and out-pointer.
    let rc = unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat succeeded.
    Ok(unsafe { st.assume_init() })
}

pub(crate) fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid descriptor and out-pointer.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat succeeded.
    Ok(unsafe { st.assume_init() })
}

/// `readlinkat`: the target of the symlink `name` of `dir`.
pub(crate) fn read_link_at(dir: RawFd, name: &CStr) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: valid descriptor and C string; `buf` is writable for its length.
        let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        let Ok(n) = usize::try_from(n) else {
            return Err(io::Error::last_os_error());
        };
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_vec(buf)));
        }
        // Possibly truncated: ask again with room to spare.
        buf.resize(buf.len() * 4, 0);
    }
}

/// Open `name` of `dir` for reading metadata and data without ever blocking or
/// following a symlink: a FIFO swapped in for a file opens at once (and is refused
/// by the caller), a symlink is `ELOOP`.
pub(crate) fn open_node_at(dir: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let flags =
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid descriptor and C string.
    owned(unsafe { libc::openat(dir, name.as_ptr(), flags) })
}

/// Open the symlink `name` of `dir` itself (not what it points to), to read its
/// extended attributes. macOS only: Linux symlinks have none.
#[cfg(target_os = "macos")]
pub(crate) fn open_symlink_at(dir: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_SYMLINK | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: valid descriptor and C string.
    owned(unsafe { libc::openat(dir, name.as_ptr(), flags) })
}

/// Clear `O_NONBLOCK` (set to open without blocking).
pub(crate) fn make_blocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: valid descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: valid descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An entry of a listing: its name and the type the directory says it has
/// (`DT_UNKNOWN` when the filesystem does not tell).
pub(crate) struct Entry {
    pub(crate) name: OsString,
    pub(crate) d_type: u8,
}

/// Entries of the open directory `dir` (not `.` and `..`). The descriptor is
/// duplicated: the directory stream owns its copy and `dir` stays usable.
pub(crate) fn list(dir: RawFd) -> io::Result<Vec<Entry>> {
    // SAFETY: valid descriptor.
    let copy = owned(unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: `fdopendir` takes over the descriptor on success.
    let stream = unsafe { libc::fdopendir(copy.as_raw_fd()) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    std::mem::forget(copy);
    let mut entries = Vec::new();
    let result = loop {
        errno_clear();
        // SAFETY: `stream` is an open directory stream used by this thread only.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let e = io::Error::last_os_error();
            break if e.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(e)
            };
        }
        // SAFETY: readdir returned a valid entry whose name is NUL-terminated.
        let (name, d_type) = unsafe {
            (
                CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes(),
                (*entry).d_type,
            )
        };
        if name != b"." && name != b".." {
            entries.push(Entry {
                name: OsString::from_vec(name.to_vec()),
                d_type,
            });
        }
    };
    // SAFETY: closes the stream and the descriptor it owns.
    unsafe { libc::closedir(stream) };
    result.map(|()| entries)
}

fn errno_clear() {
    // SAFETY: the thread's own errno.
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
    // SAFETY: the thread's own errno.
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
}

/// Names in the directory `root/rel`; `None` when it is not a directory (a symlink to
/// one is not) or is gone.
pub(crate) fn list_names(root: RawFd, rel: &Path) -> io::Result<Option<Vec<OsString>>> {
    let dir = match open_rel_dir(root, rel) {
        Ok(dir) => dir,
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    Ok(Some(
        list(dir.as_raw_fd())?.into_iter().map(|e| e.name).collect(),
    ))
}

/// The error for a component that changed under the walk.
pub(crate) fn changed(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
}
