//! [`CowFs`] port on Linux: reflink through `ioctl(FICLONE)` (btrfs, XFS with
//! `reflink=1`, bcachefs, ZFS 2.2+), `renameat2(RENAME_NOREPLACE)`, `statfs(2)`,
//! `fsync`. Everything platform-independent is in [`super::unix`], the tree walk in
//! [`super::tree`].

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use super::tree::{self, Data};
use super::unix::{self, c_path, io_error};
use crate::domain::{Error, ErrorCode, Result};
use crate::ports::{CowFs, FileLock, Scan, VolumeInfo};

#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxFs;

/// `f_type` magic numbers of `statfs(2)` (`<linux/magic.h>`).
const EXT_MAGIC: u64 = 0xef53;
const XFS_MAGIC: u64 = 0x5846_5342;
const BTRFS_MAGIC: u64 = 0x9123_683e;
const BCACHEFS_MAGIC: u64 = 0xca45_1a4e;
const TMPFS_MAGIC: u64 = 0x0102_1994;
const ZFS_MAGIC: u64 = 0x2fc1_2fc1;
const OVERLAYFS_MAGIC: u64 = 0x794c_7630;
const NFS_MAGIC: u64 = 0x6969;
const FUSE_MAGIC: u64 = 0x6573_5546;

/// Name of the filesystem for messages: known magic numbers by name, others in hex.
fn fs_name(magic: u64) -> String {
    match magic {
        EXT_MAGIC => "ext4",
        XFS_MAGIC => "xfs",
        BTRFS_MAGIC => "btrfs",
        BCACHEFS_MAGIC => "bcachefs",
        TMPFS_MAGIC => "tmpfs",
        ZFS_MAGIC => "zfs",
        OVERLAYFS_MAGIC => "overlay",
        NFS_MAGIC => "nfs",
        FUSE_MAGIC => "fuse",
        other => return format!("{other:#x}"),
    }
    .to_owned()
}

/// `ioctl(FICLONE)`: make `dst` (empty) share the storage of `src`.
fn ficlone(src: &File, dst: &File) -> io::Result<()> {
    // SAFETY: valid open fds; FICLONE takes the source fd as its argument.
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONE as _, src.as_raw_fd()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether the directory `dir` is on a filesystem that reflinks, found by trying
/// it on two anonymous files (`O_TMPFILE`: nothing appears on disk). `None` when
/// the probe cannot run there (read-only or unsearchable directory, no `O_TMPFILE`,
/// disk full...): the caller then goes by the filesystem's name.
fn reflink_probe(dir: &Path) -> Option<bool> {
    let dir = c_path(dir).ok()?;
    let anonymous = || {
        // SAFETY: valid C string.
        let fd = unsafe {
            libc::open(
                dir.as_ptr(),
                libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        };
        // SAFETY: a fresh descriptor nobody else owns.
        (fd >= 0).then(|| unsafe { File::from_raw_fd(fd) })
    };
    let (src, dst) = (anonymous()?, anonymous()?);
    // A block of data: a tail that is not block-aligned or an inline extent would
    // be refused by some filesystems for reasons that say nothing about reflinks.
    src.write_all_at(&[0u8; 4096], 0).ok()?;
    match ficlone(&src, &dst) {
        Ok(()) => Some(true),
        Err(e) => match e.raw_os_error() {
            Some(libc::EOPNOTSUPP | libc::EINVAL | libc::EXDEV | libc::ENOTTY) => Some(false),
            _ => None,
        },
    }
}

/// Clone only where the filesystem is known to reflink (probe unavailable).
fn reflinks_by_name(magic: u64) -> bool {
    matches!(magic, BTRFS_MAGIC | BCACHEFS_MAGIC)
}

/// Sync the directory entry of a file just written.
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    match File::open(parent.unwrap_or(Path::new("."))).and_then(|dir| dir.sync_all()) {
        // Some filesystems cannot sync a directory: nothing to wait for there.
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}

/// Durable on Linux: the data (`fsync`), then the name that points at it.
fn sync_file(file: &File) -> io::Result<()> {
    file.sync_all()
}

impl CowFs for LinuxFs {
    fn volume(&self, path: &Path) -> Result<VolumeInfo> {
        let c = c_path(path)?;
        let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: valid C string and out-pointer.
        if unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) } != 0 {
            return Err(io_error(path, &io::Error::last_os_error()));
        }
        // SAFETY: statfs succeeded.
        let st = unsafe { st.assume_init() };
        // The magic numbers are 32-bit patterns; the field's width and sign differ
        // between targets.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let magic = u64::from(st.f_type as u32);
        let cow = reflink_probe(path).unwrap_or_else(|| reflinks_by_name(magic));
        Ok(VolumeInfo {
            fs_type: fs_name(magic),
            cow,
            device: unix::device_of(path)?,
        })
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        unix::canonicalize(path)
    }

    fn exists_no_follow(&self, path: &Path) -> Result<bool> {
        unix::exists_no_follow(path)
    }

    fn is_dir(&self, path: &Path) -> Result<bool> {
        Ok(unix::is_dir(path))
    }

    fn is_dir_no_follow(&self, path: &Path) -> Result<bool> {
        Ok(unix::is_dir_no_follow(path))
    }

    fn create_staging_dir(&self, parent: &Path) -> Result<PathBuf> {
        unix::create_staging_dir(parent)
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<OsString>> {
        unix::list_dir(path)
    }

    fn create_dir(&self, path: &Path) -> Result<()> {
        unix::create_dir(path)
    }

    fn clone_entry(&self, src: &Path, dst: &Path) -> Result<()> {
        tree::transfer(src, dst, Data::Reflink(ficlone))
    }

    fn copy_entry(&self, src: &Path, dst: &Path) -> Result<()> {
        tree::transfer(src, dst, Data::Copy)
    }

    fn rename_exclusive(&self, from: &Path, to: &Path) -> Result<()> {
        let (cf, ct) = (c_path(from)?, c_path(to)?);
        // SAFETY: valid C strings. `renameat2` has no glibc wrapper before 2.28.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                cf.as_ptr(),
                libc::AT_FDCWD,
                ct.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if matches!(e.raw_os_error(), Some(libc::EEXIST | libc::ENOTEMPTY)) {
            return Err(Error::new(
                ErrorCode::DestExists,
                format!("destination `{}` already exists", to.display()),
            ));
        }
        Err(io_error(to, &e))
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        unix::remove_dir_all(path, |path| unix::make_dirs_writable(path, &|_| {}))
    }

    /// Durable write: the journal must reach the disk before the step it
    /// describes.
    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()> {
        unix::write_file(path, contents, sync_file)?;
        sync_parent(path).map_err(|e| io_error(path, &e))
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        unix::read_file(path)
    }

    fn scan(
        &self,
        root: &Path,
        needles: &[Vec<u8>],
        max_len: u64,
        skip: &(dyn Fn(&Path) -> bool + Sync),
    ) -> Result<Scan> {
        Ok(unix::scan(root, needles, max_len, skip))
    }

    fn replace_contents(&self, path: &Path, contents: &[u8]) -> Result<()> {
        unix::replace_contents(path, contents)
    }

    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        unix::copy_mode(from, to)
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        unix::remove_file(path)
    }

    fn replace_symlink(&self, path: &Path, target: &Path) -> Result<()> {
        unix::replace_symlink(path, target)
    }

    fn lock_exclusive(&self, path: &Path) -> Result<FileLock> {
        unix::lock_exclusive(path)
    }

    fn is_locked(&self, path: &Path) -> Result<bool> {
        unix::is_locked(path)
    }
}

#[cfg(test)]
mod tests {
    //! Adapter tests against real filesystems. `$TMPDIR` is on btrfs in the test
    //! container; tests about a given filesystem take their directory from
    //! `PARHEL_TEST_BTRFS`, `PARHEL_TEST_XFS` and `PARHEL_TEST_EXT4`. A missing
    //! variable skips the test with a notice, unless `CI` or `PARHEL_REQUIRE_FS=1`
    //! is set: then it fails.

    use super::*;
    use crate::domain::ErrorCode;
    use crate::fs::unix::c_path;
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::fs::{self, File};
    use std::io::Write;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, UNIX_EPOCH};

    /// `FIEMAP` evidence shared with the integration harness.
    #[allow(dead_code)]
    mod fiemap {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/common/fiemap.rs"
        ));
    }

    // ------------------------------------------------------------------ helpers

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Btrfs,
        Xfs,
        Ext4,
    }

    impl Kind {
        fn var(self) -> &'static str {
            match self {
                Kind::Btrfs => "PARHEL_TEST_BTRFS",
                Kind::Xfs => "PARHEL_TEST_XFS",
                Kind::Ext4 => "PARHEL_TEST_EXT4",
            }
        }

        fn name(self) -> &'static str {
            match self {
                Kind::Btrfs => "btrfs",
                Kind::Xfs => "xfs",
                Kind::Ext4 => "ext4",
            }
        }

        fn magic(self) -> u64 {
            match self {
                Kind::Btrfs => 0x9123_683e,
                Kind::Xfs => 0x5846_5342,
                Kind::Ext4 => 0xef53,
            }
        }

        fn reflinks(self) -> bool {
            self != Kind::Ext4
        }
    }

    const ALL: [Kind; 3] = [Kind::Btrfs, Kind::Xfs, Kind::Ext4];
    const REFLINKING: [Kind; 2] = [Kind::Btrfs, Kind::Xfs];

    fn magic_of(path: &Path) -> u64 {
        let c = c_path(path).unwrap();
        let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: valid C string and out-pointer.
        assert_eq!(unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) }, 0);
        // SAFETY: statfs succeeded.
        u64::try_from(unsafe { st.assume_init() }.f_type).unwrap()
    }

    /// A scratch directory that can always be deleted, even when a test left
    /// read-only directories in it.
    struct Scratch {
        dir: tempfile::TempDir,
    }

    impl Scratch {
        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            make_writable(self.dir.path());
        }
    }

    fn make_writable(path: &Path) {
        let Ok(md) = fs::symlink_metadata(path) else {
            return;
        };
        if md.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_writable(&entry.path());
                }
            }
        }
    }

    /// Scratch directory in `$TMPDIR` (btrfs in the test container).
    fn scratch() -> Scratch {
        Scratch {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn mounts_required() -> bool {
        std::env::var_os("CI").is_some()
            || std::env::var_os("PARHEL_REQUIRE_FS").is_some_and(|v| v == "1")
    }

    /// A scratch directory on `kind`, or `None` (skip) when its mount is not
    /// configured and not required. Panics on a wrong mount: a test about XFS must
    /// never run on something else.
    fn scratch_on(kind: Kind) -> Option<Scratch> {
        let Some(base) = std::env::var_os(kind.var()).filter(|v| !v.is_empty()) else {
            assert!(
                !mounts_required(),
                "{} is not set but the {} tests are required (CI or PARHEL_REQUIRE_FS=1)",
                kind.var(),
                kind.name()
            );
            eprintln!("SKIPPED: {} is not set", kind.var());
            return None;
        };
        let base = fs::canonicalize(base).unwrap();
        assert_eq!(
            magic_of(&base),
            kind.magic(),
            "{}={} is not {}",
            kind.var(),
            base.display(),
            kind.name()
        );
        Some(Scratch {
            dir: tempfile::tempdir_in(base).unwrap(),
        })
    }

    /// Run `body` once per configured filesystem among `kinds`.
    fn on_each(kinds: &[Kind], body: impl Fn(Kind, &Path)) {
        for &kind in kinds {
            if let Some(scratch) = scratch_on(kind) {
                body(kind, scratch.path());
            }
        }
    }

    /// Incompressible data of `size` bytes, synced (so it has real extents).
    fn blob(path: &Path, size: usize, seed: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut x = seed | 1;
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            data.extend_from_slice(&x.to_le_bytes());
        }
        data.truncate(size);
        let mut f = File::create(path).unwrap();
        f.write_all(&data).unwrap();
        f.sync_all().unwrap();
    }

    fn set_mtime(path: &Path, secs: u64, nanos: u32) {
        let when = UNIX_EPOCH + Duration::new(secs, nanos);
        File::open(path).unwrap().set_modified(when).unwrap();
    }

    fn mkfifo(path: &Path, mode: u32) {
        let c = c_path(path).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), mode) }, 0);
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn set_xattr(path: &Path, name: &str, value: &[u8]) {
        let (p, n) = (c_path(path).unwrap(), std::ffi::CString::new(name).unwrap());
        // SAFETY: valid C strings; `value` is readable for its length.
        let rc = unsafe {
            libc::lsetxattr(
                p.as_ptr(),
                n.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        assert_eq!(rc, 0, "setxattr: {}", std::io::Error::last_os_error());
    }

    /// `user.*` attributes of `path` itself.
    fn user_xattrs(path: &Path) -> BTreeMap<String, Vec<u8>> {
        let p = c_path(path).unwrap();
        let mut names = vec![0u8; 16 * 1024];
        // SAFETY: valid C string; `names` is writable for its length.
        let len = unsafe { libc::llistxattr(p.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
        assert!(len >= 0, "llistxattr: {}", std::io::Error::last_os_error());
        names.truncate(usize::try_from(len).unwrap());
        names
            .split(|&b| b == 0)
            .filter(|n| n.starts_with(b"user."))
            .map(|n| {
                let name = std::ffi::CString::new(n).unwrap();
                let mut value = vec![0u8; 4096];
                // SAFETY: valid C strings; `value` is writable for its length.
                let got = unsafe {
                    libc::lgetxattr(
                        p.as_ptr(),
                        name.as_ptr(),
                        value.as_mut_ptr().cast(),
                        value.len(),
                    )
                };
                assert!(got >= 0);
                value.truncate(usize::try_from(got).unwrap());
                (String::from_utf8(n.to_vec()).unwrap(), value)
            })
            .collect()
    }

    /// A source tree with everything a project directory can contain. Returns the
    /// socket listener (the socket file exists while it is alive) and the directory
    /// the `outside-link` symlink points at.
    struct Tree {
        _socket: UnixListener,
        outside: PathBuf,
    }

    /// Fixed instants with a nanosecond part, so truncation to seconds is caught.
    const FILE_MTIME: u64 = 1_577_934_245;
    const DIR_MTIME: u64 = 1_500_000_000;
    const NANOS: u32 = 123_456_789;

    fn build_tree(root: &Path, outside: &Path) -> Tree {
        fs::create_dir_all(root).unwrap();
        fs::create_dir_all(outside).unwrap();
        fs::write(outside.join("SECRET.txt"), "outside the tree").unwrap();
        let w = |rel: &str, data: &[u8], mode: u32| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, data).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        };
        w("plain.txt", b"hello\n", 0o644);
        w("run.sh", b"#!/bin/sh\necho hi\n", 0o755);
        w("readonly.txt", b"look, don't touch\n", 0o444);
        w("private.key", b"secret\n", 0o600);
        w("zero", b"", 0o644);
        w("caf\u{e9} a\u{e7}\u{e3}o.txt", b"nfc\n", 0o644);
        w("cafe\u{301} menu.txt", b"nfd\n", 0o644);
        w("dir with spaces/file name.txt", b"spaces\n", 0o644);
        w(
            "\u{fc}n\u{ef}/\u{e7}\u{f6}d\u{e9}.txt",
            b"unicode dir\n",
            0o644,
        );
        w("hard-a", b"linked\n", 0o644);
        fs::hard_link(root.join("hard-a"), root.join("hard-b")).unwrap();
        blob(&root.join("sub/deep/file.bin"), 1 << 20, 11);
        // A hole in the middle and data at both ends.
        let sparse = root.join("sparse.bin");
        let mut f = File::create(&sparse).unwrap();
        f.write_all(b"head").unwrap();
        f.set_len(4 << 20).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&f, b"tail", (4 << 20) - 4).unwrap();
        f.sync_all().unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("nest/empty")).unwrap();
        fs::create_dir_all(root.join("rodir")).unwrap();
        fs::write(root.join("rodir/inner.txt"), b"inside a read-only dir\n").unwrap();

        symlink(root.join("plain.txt"), root.join("abs-link")).unwrap();
        symlink("plain.txt", root.join("rel-link")).unwrap();
        symlink("no/such/target", root.join("dangling")).unwrap();
        symlink("sub", root.join("dir-link")).unwrap();
        symlink(outside, root.join("outside-link")).unwrap();
        mkfifo(&root.join("pipe"), 0o640);
        let socket = UnixListener::bind(root.join("app.sock")).unwrap();

        set_xattr(&root.join("plain.txt"), "user.parhel.test", b"hello");
        set_xattr(&root.join("plain.txt"), "user.parhel.second", b"2");
        set_xattr(&root.join("sub"), "user.parhel.dir", b"on a directory");

        fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::set_permissions(root.join("rodir"), fs::Permissions::from_mode(0o555)).unwrap();
        // Modification times last: nothing below may touch them again. Files first,
        // then directories from the deepest up.
        let mut dirs = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            dirs.push(dir.clone());
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let md = fs::symlink_metadata(&path).unwrap();
                if md.is_dir() {
                    stack.push(path);
                } else if md.is_file() {
                    set_mtime(&path, FILE_MTIME, NANOS);
                }
            }
        }
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for dir in dirs {
            set_mtime(&dir, DIR_MTIME, NANOS);
        }
        Tree {
            _socket: socket,
            outside: outside.to_path_buf(),
        }
    }

    /// Everything about an entry that a faithful copy must reproduce.
    fn describe(root: &Path) -> BTreeMap<PathBuf, String> {
        fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, String>) {
            let path = root.join(rel);
            let md = fs::symlink_metadata(&path).unwrap();
            let ft = md.file_type();
            let mtime = (md.mtime(), md.mtime_nsec());
            let desc = if ft.is_symlink() {
                format!("link -> {}", fs::read_link(&path).unwrap().display())
            } else if ft.is_dir() {
                let mut names: Vec<_> = fs::read_dir(&path)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .collect();
                names.sort();
                for child in names {
                    walk(root, &rel.join(child.file_name().unwrap()), out);
                }
                format!(
                    "dir mode={:o} uid={} gid={} mtime={mtime:?} xattrs={:?}",
                    md.mode() & 0o7777,
                    md.uid(),
                    md.gid(),
                    user_xattrs(&path)
                )
            } else if ft.is_fifo() {
                format!("fifo mode={:o} uid={}", md.mode() & 0o7777, md.uid())
            } else if ft.is_file() {
                let data = fs::read(&path).unwrap();
                format!(
                    "file mode={:o} uid={} gid={} size={} mtime={mtime:?} sum={:x} xattrs={:?}",
                    md.mode() & 0o7777,
                    md.uid(),
                    md.gid(),
                    md.size(),
                    data.iter()
                        .fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| (h ^ u64::from(b))
                            .wrapping_mul(0x0100_0000_01b3)),
                    user_xattrs(&path)
                )
            } else {
                format!("other {ft:?}")
            };
            out.insert(rel.to_path_buf(), desc);
        }
        let mut out = BTreeMap::new();
        walk(root, Path::new(""), &mut out);
        out
    }

    /// `dst` reproduces `src` entry by entry (sockets are skipped by design).
    fn assert_same_tree(src: &Path, dst: &Path) {
        let mut want = describe(src);
        want.remove(Path::new("app.sock"));
        let got = describe(dst);
        let mut diff = String::new();
        for (path, desc) in &want {
            match got.get(path) {
                None => writeln!(diff, "missing {}", path.display()).unwrap(),
                Some(g) if g != desc => {
                    writeln!(diff, "{}:\n  src {desc}\n  dst {g}", path.display()).unwrap();
                }
                Some(_) => {}
            }
        }
        for path in got.keys().filter(|p| !want.contains_key(*p)) {
            writeln!(diff, "unexpected {}", path.display()).unwrap();
        }
        assert!(diff.is_empty(), "copy differs from the source:\n{diff}");
    }

    fn nanoseconds_survive(path: &Path) {
        let mtime = fs::metadata(path).unwrap().modified().unwrap();
        let since = mtime.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(
            since.subsec_nanos(),
            NANOS,
            "{}: fixture lost its nanoseconds",
            path.display()
        );
    }

    // ------------------------------------------------------------------ volume

    // U-LFS-01
    #[test]
    fn btrfs_is_copy_on_write() {
        let Some(s) = scratch_on(Kind::Btrfs) else {
            return;
        };
        fs::create_dir_all(s.path().join("deep/er")).unwrap();
        for dir in [s.path().to_path_buf(), s.path().join("deep/er")] {
            let v = LinuxFs.volume(&dir).unwrap();
            assert_eq!(v.fs_type, "btrfs");
            assert!(v.cow, "{}", dir.display());
            assert_eq!(v.device, fs::metadata(&dir).unwrap().dev());
        }
    }

    // U-LFS-02
    #[test]
    fn xfs_with_reflink_is_copy_on_write() {
        let Some(s) = scratch_on(Kind::Xfs) else {
            return;
        };
        let v = LinuxFs.volume(s.path()).unwrap();
        assert_eq!(v.fs_type, "xfs");
        assert!(v.cow);
        assert_eq!(v.device, fs::metadata(s.path()).unwrap().dev());
    }

    // U-LFS-03
    #[test]
    fn ext4_is_not_copy_on_write() {
        let Some(s) = scratch_on(Kind::Ext4) else {
            return;
        };
        let v = LinuxFs.volume(s.path()).unwrap();
        assert_eq!(v.fs_type, "ext4");
        assert!(!v.cow);
        assert_eq!(v.device, fs::metadata(s.path()).unwrap().dev());
    }

    // U-LFS-04: the probe is invisible: no file, no directory change, no leftovers.
    #[test]
    fn probing_a_volume_leaves_nothing_behind() {
        on_each(&ALL, |kind, dir| {
            fs::write(dir.join("existing"), "x").unwrap();
            let listing = |d: &Path| {
                let mut names: Vec<_> = fs::read_dir(d)
                    .unwrap()
                    .map(|e| e.unwrap().file_name())
                    .collect();
                names.sort();
                names
            };
            let (names, mtime, nlink) = (
                listing(dir),
                fs::metadata(dir).unwrap().modified().unwrap(),
                fs::metadata(dir).unwrap().nlink(),
            );
            for _ in 0..3 {
                LinuxFs.volume(dir).unwrap();
            }
            assert_eq!(listing(dir), names, "{}", kind.name());
            assert_eq!(fs::metadata(dir).unwrap().modified().unwrap(), mtime);
            assert_eq!(fs::metadata(dir).unwrap().nlink(), nlink);
        });
    }

    // U-LFS-05: the answer comes from a probe of the directory, not from its name.
    #[test]
    fn cow_agrees_with_what_the_filesystem_really_does() {
        on_each(&ALL, |kind, dir| {
            let a = dir.join("a");
            fs::write(&a, "x").unwrap();
            let can_reflink = fs::File::create(dir.join("b")).is_ok() && {
                let (src, dst) = (
                    File::open(&a).unwrap(),
                    File::create(dir.join("b")).unwrap(),
                );
                // SAFETY: valid fds; FICLONE takes the source fd.
                unsafe {
                    libc::ioctl(
                        std::os::fd::AsRawFd::as_raw_fd(&dst),
                        libc::FICLONE as _,
                        std::os::fd::AsRawFd::as_raw_fd(&src),
                    ) == 0
                }
            };
            assert_eq!(can_reflink, kind.reflinks(), "fixture: {}", kind.name());
            assert_eq!(LinuxFs.volume(dir).unwrap().cow, can_reflink);
        });
    }

    // U-LFS-06: asking about a read-only directory (a source repo can be one) still
    // answers, and answers right: the probe cannot write there.
    #[test]
    fn read_only_directory_on_btrfs_is_still_copy_on_write() {
        let Some(s) = scratch_on(Kind::Btrfs) else {
            return;
        };
        let ro = s.path().join("ro");
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let v = LinuxFs.volume(&ro).unwrap();
        assert_eq!(v.fs_type, "btrfs");
        assert!(v.cow);
    }

    // U-LFS-07
    #[test]
    fn volume_of_a_missing_path_is_an_io_error() {
        let s = scratch();
        let err = LinuxFs.volume(&s.path().join("nope")).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert!(err.message.contains("nope"), "{}", err.message);
    }

    // U-LFS-08: filesystems the adapter does not know are named by their magic
    // number (procfs: 0x9fa0) and are not copy-on-write.
    #[test]
    fn unknown_filesystem_is_named_by_magic_and_is_not_cow() {
        let v = LinuxFs.volume(Path::new("/proc")).unwrap();
        assert_eq!(v.fs_type, "0x9fa0");
        assert!(!v.cow);
    }

    // ------------------------------------------------------------------ rename

    // U-LFS-10
    #[test]
    fn rename_exclusive_moves_directories_and_files() {
        on_each(&ALL, |_, dir| {
            fs::create_dir(dir.join("s")).unwrap();
            fs::write(dir.join("s/f"), b"x").unwrap();
            LinuxFs
                .rename_exclusive(&dir.join("s"), &dir.join("t"))
                .unwrap();
            assert!(!dir.join("s").exists());
            assert_eq!(fs::read(dir.join("t/f")).unwrap(), b"x");
            LinuxFs
                .rename_exclusive(&dir.join("t/f"), &dir.join("g"))
                .unwrap();
            assert_eq!(fs::read(dir.join("g")).unwrap(), b"x");
        });
    }

    // U-LFS-11: plain rename(2) would replace a file or an empty dir; NOREPLACE must not.
    #[test]
    fn rename_exclusive_never_replaces_anything() {
        on_each(&ALL, |kind, dir| {
            fs::create_dir(dir.join("s")).unwrap();
            fs::write(dir.join("s/f"), b"mine").unwrap();
            fs::write(dir.join("file"), b"theirs").unwrap();
            fs::create_dir(dir.join("empty-dir")).unwrap();
            fs::create_dir_all(dir.join("full-dir/x")).unwrap();
            symlink("nowhere", dir.join("dangling")).unwrap();
            fs::create_dir(dir.join("real")).unwrap();
            symlink("real", dir.join("link-to-dir")).unwrap();
            for existing in ["file", "empty-dir", "full-dir", "dangling", "link-to-dir"] {
                let err = LinuxFs
                    .rename_exclusive(&dir.join("s"), &dir.join(existing))
                    .unwrap_err();
                assert_eq!(
                    err.code,
                    ErrorCode::DestExists,
                    "{existing} on {}",
                    kind.name()
                );
                assert_eq!(
                    fs::read(dir.join("s/f")).unwrap(),
                    b"mine",
                    "source in place"
                );
            }
            assert_eq!(fs::read(dir.join("file")).unwrap(), b"theirs");
            assert!(dir.join("empty-dir").read_dir().unwrap().next().is_none());
            assert!(dir.join("full-dir/x").is_dir());
            assert!(
                fs::symlink_metadata(dir.join("dangling"))
                    .unwrap()
                    .is_symlink()
            );
            assert!(dir.join("real").read_dir().unwrap().next().is_none());
        });
    }

    // U-LFS-12: only a name clash is `dest-exists`; other failures stay `io`.
    #[test]
    fn rename_exclusive_reports_other_failures_as_io() {
        let s = scratch();
        let err = LinuxFs
            .rename_exclusive(&s.path().join("missing"), &s.path().join("t"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        fs::create_dir(s.path().join("s")).unwrap();
        let err = LinuxFs
            .rename_exclusive(&s.path().join("s"), &s.path().join("no/parent/t"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert!(s.path().join("s").is_dir());
    }

    // U-LFS-13: two racers, one winner, and the loser's data is intact.
    #[test]
    fn rename_exclusive_has_exactly_one_winner_among_racers() {
        let s = scratch();
        let n = 8;
        for i in 0..n {
            fs::create_dir(s.path().join(format!("src{i}"))).unwrap();
            fs::write(s.path().join(format!("src{i}/id")), i.to_string()).unwrap();
        }
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    let (from, to) = (s.path().join(format!("src{i}")), s.path().join("dest"));
                    scope.spawn(move || LinuxFs.rename_exclusive(&from, &to))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        for r in results.iter().filter_map(|r| r.as_ref().err()) {
            assert_eq!(r.code, ErrorCode::DestExists);
        }
        let winner = fs::read_to_string(s.path().join("dest/id")).unwrap();
        for i in 0..n {
            assert_eq!(
                s.path().join(format!("src{i}")).exists(),
                i.to_string() != winner,
                "losers keep their directory"
            );
        }
    }

    // ------------------------------------------------------------------ clone (reflink)

    // U-LFS-20: a file clone has the source's data, mode and times, and shares its storage.
    #[test]
    fn clone_entry_reflinks_a_file() {
        on_each(&REFLINKING, |kind, dir| {
            let (a, b) = (dir.join("a.sh"), dir.join("b.sh"));
            blob(&a, 1 << 20, 3);
            fs::set_permissions(&a, fs::Permissions::from_mode(0o755)).unwrap();
            set_mtime(&a, FILE_MTIME, NANOS);
            LinuxFs.clone_entry(&a, &b).unwrap();
            assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
            assert_eq!(
                fs::metadata(&b).unwrap().permissions().mode() & 0o777,
                0o755
            );
            nanoseconds_survive(&b);
            assert!(
                fiemap::share_first_extent(&a, &b),
                "{}: copied, not cloned",
                kind.name()
            );
        });
    }

    // U-LFS-21: the whole tree: contents, modes, links, empty dirs, FIFOs, xattrs,
    // times (directories' last), and the data of regular files is shared.
    #[test]
    fn clone_entry_reproduces_a_tree_and_shares_its_data() {
        on_each(&REFLINKING, |kind, dir| {
            let tree = build_tree(&dir.join("src"), &dir.join("outside"));
            LinuxFs
                .clone_entry(&dir.join("src"), &dir.join("dst"))
                .unwrap();
            assert_same_tree(&dir.join("src"), &dir.join("dst"));
            nanoseconds_survive(&dir.join("dst/sub/deep/file.bin"));
            nanoseconds_survive(&dir.join("dst/sub"));
            nanoseconds_survive(&dir.join("dst"));
            for rel in ["sub/deep/file.bin", "sparse.bin"] {
                assert!(
                    fiemap::share_first_extent(
                        &dir.join("src").join(rel),
                        &dir.join("dst").join(rel)
                    ),
                    "{}: {rel} was copied, not cloned",
                    kind.name()
                );
            }
            drop(tree);
        });
    }

    // U-LFS-22: symlinks are recreated, never followed (not even when they point at
    // directories or outside the tree), and the socket is skipped.
    #[test]
    fn clone_entry_never_follows_symlinks_and_skips_sockets() {
        on_each(&REFLINKING, |_, dir| {
            let tree = build_tree(&dir.join("src"), &dir.join("outside"));
            LinuxFs
                .clone_entry(&dir.join("src"), &dir.join("dst"))
                .unwrap();
            let dst = dir.join("dst");
            for (link, target) in [
                ("abs-link", dir.join("src/plain.txt")),
                ("rel-link", PathBuf::from("plain.txt")),
                ("dangling", PathBuf::from("no/such/target")),
                ("dir-link", PathBuf::from("sub")),
                ("outside-link", tree.outside.clone()),
            ] {
                let md = fs::symlink_metadata(dst.join(link)).unwrap();
                assert!(md.file_type().is_symlink(), "{link} must stay a link");
                assert_eq!(fs::read_link(dst.join(link)).unwrap(), target, "{link}");
            }
            assert!(
                !dst.join("outside-link/SECRET.txt").exists() || {
                    // Reachable through the link itself, as in the source, but never copied.
                    fs::symlink_metadata(dst.join("outside-link"))
                        .unwrap()
                        .is_symlink()
                }
            );
            assert!(
                fs::symlink_metadata(dst.join("app.sock")).is_err(),
                "sockets are skipped"
            );
            assert!(
                fs::symlink_metadata(dst.join("pipe"))
                    .unwrap()
                    .file_type()
                    .is_fifo()
            );
            // Hard links become two independent files.
            assert_ne!(
                fs::metadata(dst.join("hard-a")).unwrap().ino(),
                fs::metadata(dst.join("hard-b")).unwrap().ino()
            );
            assert_eq!(fs::read(dst.join("hard-b")).unwrap(), b"linked\n");
        });
    }

    // U-LFS-23: a top-level symlink is cloned as a link (like `clonefile(CLONE_NOFOLLOW)`).
    #[test]
    fn clone_entry_clones_a_symlink_itself() {
        on_each(&REFLINKING, |_, dir| {
            fs::create_dir(dir.join("target_dir")).unwrap();
            fs::write(dir.join("target_dir/f"), "x").unwrap();
            symlink("target_dir", dir.join("link")).unwrap();
            LinuxFs
                .clone_entry(&dir.join("link"), &dir.join("link2"))
                .unwrap();
            assert!(
                fs::symlink_metadata(dir.join("link2"))
                    .unwrap()
                    .is_symlink()
            );
            assert_eq!(
                fs::read_link(dir.join("link2")).unwrap(),
                PathBuf::from("target_dir")
            );
            symlink("gone", dir.join("dangling")).unwrap();
            LinuxFs
                .clone_entry(&dir.join("dangling"), &dir.join("dangling2"))
                .unwrap();
            assert_eq!(
                fs::read_link(dir.join("dangling2")).unwrap(),
                PathBuf::from("gone")
            );
        });
    }

    // U-LFS-24: cloning onto an existing path fails and leaves it alone.
    #[test]
    fn clone_entry_onto_existing_path_fails_without_touching_it() {
        on_each(&REFLINKING, |_, dir| {
            fs::write(dir.join("a"), b"new").unwrap();
            fs::write(dir.join("b"), b"keep").unwrap();
            let err = LinuxFs
                .clone_entry(&dir.join("a"), &dir.join("b"))
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::CloneFailed);
            assert_eq!(fs::read(dir.join("b")).unwrap(), b"keep");
            fs::create_dir_all(dir.join("srcdir")).unwrap();
            fs::create_dir_all(dir.join("dstdir")).unwrap();
            fs::write(dir.join("dstdir/mine"), b"keep").unwrap();
            let err = LinuxFs
                .clone_entry(&dir.join("srcdir"), &dir.join("dstdir"))
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::CloneFailed);
            assert_eq!(fs::read(dir.join("dstdir/mine")).unwrap(), b"keep");
        });
    }

    // U-LFS-25
    #[test]
    fn clone_entry_of_a_missing_source_names_it() {
        let s = scratch();
        let err = LinuxFs
            .clone_entry(&s.path().join("nope"), &s.path().join("dst"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        assert!(err.message.contains("nope"), "{}", err.message);
    }

    // U-LFS-26: another volume is `cross-volume` (EXDEV), nothing is left behind.
    #[test]
    fn clone_entry_across_volumes_is_cross_volume() {
        let (Some(a), Some(b)) = (scratch_on(Kind::Btrfs), scratch_on(Kind::Xfs)) else {
            return;
        };
        fs::write(a.path().join("f"), b"x").unwrap();
        let err = LinuxFs
            .clone_entry(&a.path().join("f"), &b.path().join("f"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CrossVolume);
        assert!(!b.path().join("f").exists());
    }

    // U-LFS-27: no reflink on ext4: a clean `clone-failed`, never a silent copy.
    #[test]
    fn clone_entry_on_ext4_fails_instead_of_copying() {
        let Some(s) = scratch_on(Kind::Ext4) else {
            return;
        };
        fs::write(s.path().join("f"), b"x").unwrap();
        let err = LinuxFs
            .clone_entry(&s.path().join("f"), &s.path().join("g"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        fs::create_dir(s.path().join("d")).unwrap();
        fs::write(s.path().join("d/f"), b"x").unwrap();
        let err = LinuxFs
            .clone_entry(&s.path().join("d"), &s.path().join("e"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
    }

    // U-LFS-28: an unreadable file inside the tree is named in the error.
    #[test]
    fn clone_entry_names_the_unreadable_file() {
        on_each(&REFLINKING, |_, dir| {
            fs::create_dir_all(dir.join("src/sub")).unwrap();
            fs::write(dir.join("src/sub/secret.bin"), b"x").unwrap();
            fs::set_permissions(
                dir.join("src/sub/secret.bin"),
                fs::Permissions::from_mode(0o000),
            )
            .unwrap();
            let err = LinuxFs
                .clone_entry(&dir.join("src"), &dir.join("dst"))
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::CloneFailed);
            assert!(err.message.contains("secret.bin"), "{}", err.message);
        });
    }

    // U-LFS-29: a read-only directory is cloned with its mode and its contents (it is
    // made writable only while being filled).
    #[test]
    fn clone_entry_fills_read_only_directories() {
        on_each(&REFLINKING, |_, dir| {
            fs::create_dir_all(dir.join("src/ro/inner")).unwrap();
            fs::write(dir.join("src/ro/inner/f"), b"x").unwrap();
            fs::set_permissions(dir.join("src/ro/inner"), fs::Permissions::from_mode(0o555))
                .unwrap();
            fs::set_permissions(dir.join("src/ro"), fs::Permissions::from_mode(0o500)).unwrap();
            LinuxFs
                .clone_entry(&dir.join("src"), &dir.join("dst"))
                .unwrap();
            let mode = |p: &str| fs::metadata(dir.join(p)).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode("dst/ro"), 0o500);
            assert_eq!(mode("dst/ro/inner"), 0o555);
            assert_eq!(fs::read(dir.join("dst/ro/inner/f")).unwrap(), b"x");
            make_writable(&dir.join("src"));
            make_writable(&dir.join("dst"));
        });
    }

    // ------------------------------------------------------------------ copy

    // U-LFS-30: the copy flavour reproduces the same tree on every filesystem...
    #[test]
    fn copy_entry_reproduces_a_tree() {
        on_each(&ALL, |_, dir| {
            let tree = build_tree(&dir.join("src"), &dir.join("outside"));
            LinuxFs
                .copy_entry(&dir.join("src"), &dir.join("dst"))
                .unwrap();
            assert_same_tree(&dir.join("src"), &dir.join("dst"));
            nanoseconds_survive(&dir.join("dst/sub/deep/file.bin"));
            nanoseconds_survive(&dir.join("dst/sub"));
            nanoseconds_survive(&dir.join("dst"));
            let dst = dir.join("dst");
            for link in [
                "abs-link",
                "rel-link",
                "dangling",
                "dir-link",
                "outside-link",
            ] {
                assert!(
                    fs::symlink_metadata(dst.join(link)).unwrap().is_symlink(),
                    "{link}"
                );
            }
            assert!(fs::symlink_metadata(dst.join("app.sock")).is_err());
            assert_ne!(
                fs::metadata(dst.join("hard-a")).unwrap().ino(),
                fs::metadata(dst.join("hard-b")).unwrap().ino()
            );
            drop(tree);
        });
    }

    // U-LFS-31: ... and owns its data: no extent is shared with the source, even where
    // the filesystem could share (`copy_file_range` would reflink there: it must not).
    #[test]
    fn copy_entry_does_not_share_extents() {
        on_each(&ALL, |kind, dir| {
            blob(&dir.join("src/big.bin"), 9 << 20, 5);
            blob(&dir.join("src/sub/small.bin"), 64 << 10, 6);
            LinuxFs
                .copy_entry(&dir.join("src"), &dir.join("dst"))
                .unwrap();
            for rel in ["big.bin", "sub/small.bin"] {
                assert_eq!(
                    fs::read(dir.join("src").join(rel)).unwrap(),
                    fs::read(dir.join("dst").join(rel)).unwrap()
                );
                assert!(
                    fiemap::are_independent_copies(
                        &dir.join("src").join(rel),
                        &dir.join("dst").join(rel)
                    ),
                    "{}: {rel} shares storage with the source",
                    kind.name()
                );
            }
        });
    }

    // U-LFS-32: a single file, with its mode and nanosecond mtime.
    #[test]
    fn copy_entry_copies_a_file() {
        on_each(&ALL, |_, dir| {
            let (a, b) = (dir.join("a.sh"), dir.join("b.sh"));
            blob(&a, 300_000, 8);
            fs::set_permissions(&a, fs::Permissions::from_mode(0o751)).unwrap();
            set_mtime(&a, FILE_MTIME, NANOS);
            LinuxFs.copy_entry(&a, &b).unwrap();
            assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
            assert_eq!(
                fs::metadata(&b).unwrap().permissions().mode() & 0o777,
                0o751
            );
            nanoseconds_survive(&b);
        });
    }

    // U-LFS-33: no copy-on-write and no common volume needed.
    #[test]
    fn copy_entry_works_across_volumes_and_onto_ext4() {
        for (from, to) in [
            (Kind::Btrfs, Kind::Xfs),
            (Kind::Btrfs, Kind::Ext4),
            (Kind::Ext4, Kind::Btrfs),
            (Kind::Xfs, Kind::Ext4),
        ] {
            let (Some(a), Some(b)) = (scratch_on(from), scratch_on(to)) else {
                continue;
            };
            let _tree = build_tree(&a.path().join("src"), &a.path().join("outside"));
            LinuxFs
                .copy_entry(&a.path().join("src"), &b.path().join("dst"))
                .unwrap();
            assert_same_tree(&a.path().join("src"), &b.path().join("dst"));
        }
    }

    // U-LFS-34: sparse files stay equal byte for byte (holes read as zeros).
    #[test]
    fn copy_entry_preserves_sparse_content() {
        on_each(&ALL, |_, dir| {
            let _tree = build_tree(&dir.join("src"), &dir.join("outside"));
            LinuxFs
                .copy_entry(&dir.join("src/sparse.bin"), &dir.join("sparse.copy"))
                .unwrap();
            let data = fs::read(dir.join("sparse.copy")).unwrap();
            assert_eq!(data.len(), 4 << 20);
            assert_eq!(&data[..4], b"head");
            assert_eq!(&data[(4 << 20) - 4..], b"tail");
            assert!(data[4..(4 << 20) - 4].iter().all(|&b| b == 0));
        });
    }

    // U-LFS-35: same failure contract as the clone flavour.
    #[test]
    fn copy_entry_fails_cleanly() {
        let s = scratch();
        fs::write(s.path().join("a"), b"new").unwrap();
        fs::write(s.path().join("b"), b"keep").unwrap();
        let err = LinuxFs
            .copy_entry(&s.path().join("a"), &s.path().join("b"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        assert_eq!(fs::read(s.path().join("b")).unwrap(), b"keep");
        let err = LinuxFs
            .copy_entry(&s.path().join("nope"), &s.path().join("c"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        assert!(err.message.contains("nope"), "{}", err.message);
        fs::create_dir_all(s.path().join("src/sub")).unwrap();
        fs::write(s.path().join("src/sub/secret.bin"), b"x").unwrap();
        fs::set_permissions(
            s.path().join("src/sub/secret.bin"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let err = LinuxFs
            .copy_entry(&s.path().join("src"), &s.path().join("dst"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        assert!(err.message.contains("secret.bin"), "{}", err.message);
    }

    // ------------------------------------------------------------------ removal, writes

    // U-LFS-40: cleanup of a clone that carries read-only directories (the Linux
    // counterpart of macOS's `uchg`), without ever following a symlink out of it.
    #[test]
    fn remove_dir_all_handles_read_only_directories_and_links() {
        let s = scratch();
        let root = s.path().join("t");
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/b/c/f"), b"x").unwrap();
        fs::write(root.join("a/b/g"), b"x").unwrap();
        fs::create_dir_all(s.path().join("outside")).unwrap();
        fs::write(s.path().join("outside/keep"), b"keep").unwrap();
        symlink(s.path().join("outside"), root.join("a/link")).unwrap();
        for d in ["a/b/c", "a/b", "a"] {
            fs::set_permissions(root.join(d), fs::Permissions::from_mode(0o555)).unwrap();
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
        LinuxFs.remove_dir_all(&root).unwrap();
        assert!(!root.exists());
        assert_eq!(fs::read(s.path().join("outside/keep")).unwrap(), b"keep");
        LinuxFs.remove_dir_all(&root).unwrap();
    }

    // U-LFS-41
    #[test]
    fn write_file_writes_replaces_and_fails_cleanly() {
        let s = scratch();
        let f = s.path().join("journal");
        LinuxFs.write_file(&f, b"first").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"first");
        LinuxFs.write_file(&f, b"2nd").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"2nd", "truncated, not appended");
        LinuxFs.write_file(&f, b"").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"");
        let big = vec![0xA5u8; 3 << 20];
        LinuxFs.write_file(&f, &big).unwrap();
        assert_eq!(fs::read(&f).unwrap(), big);
        let err = LinuxFs
            .write_file(&s.path().join("no/dir/f"), b"x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        let names: Vec<_> = fs::read_dir(s.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("journal")],
            "no temp files"
        );
    }

    // U-LFS-42: the platform-independent operations are wired to the shared helpers.
    #[test]
    fn shared_operations_are_wired() {
        let s = scratch();
        let dir = s.path();
        let fs_ = LinuxFs;
        fs_.create_dir(&dir.join("d")).unwrap();
        assert!(fs_.is_dir(&dir.join("d")).unwrap());
        symlink("d", dir.join("l")).unwrap();
        assert!(fs_.is_dir(&dir.join("l")).unwrap());
        assert!(!fs_.is_dir_no_follow(&dir.join("l")).unwrap());
        assert!(fs_.exists_no_follow(&dir.join("l")).unwrap());
        symlink("gone", dir.join("dangling")).unwrap();
        assert!(fs_.exists_no_follow(&dir.join("dangling")).unwrap());
        assert!(!fs_.exists_no_follow(&dir.join("nothing")).unwrap());
        assert_eq!(
            fs_.canonicalize(&dir.join("l")).unwrap(),
            fs::canonicalize(dir.join("d")).unwrap()
        );
        let staging = fs_.create_staging_dir(dir).unwrap();
        assert!(
            staging.is_dir() && staging.parent() == Some(fs::canonicalize(dir).unwrap().as_path())
        );
        assert!(
            fs_.list_dir(dir)
                .unwrap()
                .contains(&staging.file_name().unwrap().to_owned())
        );
        fs::write(dir.join("ro"), b"old").unwrap();
        fs::set_permissions(dir.join("ro"), fs::Permissions::from_mode(0o444)).unwrap();
        fs_.replace_contents(&dir.join("ro"), b"new").unwrap();
        assert_eq!(fs_.read_file(&dir.join("ro")).unwrap(), b"new");
        assert_eq!(
            fs::metadata(dir.join("ro")).unwrap().permissions().mode() & 0o777,
            0o444
        );
        fs_.replace_symlink(&dir.join("l"), Path::new("elsewhere"))
            .unwrap();
        assert_eq!(
            fs::read_link(dir.join("l")).unwrap(),
            PathBuf::from("elsewhere")
        );
        fs::set_permissions(dir.join("d"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(dir.join("e")).unwrap();
        fs_.copy_mode(&dir.join("d"), &dir.join("e")).unwrap();
        assert_eq!(
            fs::metadata(dir.join("e")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs_.remove_file(&dir.join("ro")).unwrap();
        assert!(!dir.join("ro").exists());
        let lock = fs_.lock_exclusive(&dir.join("lock")).unwrap();
        assert!(fs_.lock_exclusive(&dir.join("lock")).is_err());
        drop(lock);
        assert!(!fs_.is_locked(&dir.join("lock")).unwrap());
        fs::write(dir.join("d/script"), format!("#!{}/bin\n", dir.display())).unwrap();
        let needle = dir.as_os_str().as_encoded_bytes().to_vec();
        let scan = fs_.scan(dir, &[needle], 1 << 20, &|_| false).unwrap();
        assert_eq!(scan.matches, vec![PathBuf::from("d/script")]);
    }
}
