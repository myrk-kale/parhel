//! [`CowFs`] port on macOS: `clonefile(2)`, `renamex_np(2)`, `statfs(2)`,
//! `F_FULLFSYNC` and BSD flags. Everything platform-independent is in [`super::unix`].

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use super::anchor;
use super::tree::{self, Data};
use super::unix::{self, c_path, io_error};
use crate::domain::store::EntryMeta;
use crate::domain::{Error, ErrorCode, Result};
use crate::ports::{CowFs, FileLock, Scan, SharedLock, Space, VolumeInfo};

/// `CLONE_NOFOLLOW` from `<sys/clonefile.h>` (not exported by `libc`): clone a
/// symlink itself instead of its target.
pub const CLONE_NOFOLLOW: u32 = 0x0001;

#[derive(Debug, Clone, Copy, Default)]
pub struct ApfsFs;

/// On macOS only `F_FULLFSYNC` flushes the drive's cache.
fn full_sync(file: &File) -> io::Result<()> {
    // SAFETY: valid open fd.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        file.sync_all()?;
    }
    Ok(())
}

impl CowFs for ApfsFs {
    fn volume(&self, path: &Path) -> Result<VolumeInfo> {
        let c = c_path(path)?;
        let mut st = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: valid C string and out-pointer.
        if unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) } != 0 {
            return Err(io_error(path, &io::Error::last_os_error()));
        }
        // SAFETY: statfs succeeded.
        let st = unsafe { st.assume_init() };
        let name: Vec<u8> = st
            .f_fstypename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c.to_ne_bytes()[0])
            .collect();
        let fs_type = String::from_utf8_lossy(&name).into_owned();
        // Clones do not cross APFS volumes, whatever container they share.
        let device = unix::device_of(path)?;
        Ok(VolumeInfo {
            cow: fs_type == "apfs",
            fs_type,
            device,
            filesystem: device,
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

    fn list_dir_within(&self, root: &Path, rel: &Path) -> Result<Option<Vec<OsString>>> {
        unix::list_dir_within(root, rel)
    }

    fn clone_within(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()> {
        let src = root.join(rel);
        let failed = |e: &io::Error, culprit: PathBuf| {
            let code = match e.raw_os_error() {
                Some(libc::EXDEV) => ErrorCode::CrossVolume,
                _ => ErrorCode::CloneFailed,
            };
            Error::new(code, format!("cannot clone `{}`: {e}", culprit.display()))
        };
        // The entry's parent is reached from the root without following symlinks, and
        // cloned from there: a directory swapped for a symlink meanwhile is an error.
        let opened = anchor::open_root(root)
            .and_then(|root_fd| anchor::open_parent(root_fd.as_raw_fd(), rel));
        let (parent, name) = opened.map_err(|e| {
            let e = if anchor::changed(&e) {
                io::Error::other("changed while it was being cloned")
            } else {
                e
            };
            failed(&e, src.clone())
        })?;
        let cd = c_path(dst)?;
        // SAFETY: valid descriptor and C strings.
        let rc = unsafe {
            libc::clonefileat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::AT_FDCWD,
                cd.as_ptr(),
                CLONE_NOFOLLOW,
            )
        };
        if rc == 0 {
            // `clonefile` gives the clone the ACLs of the directory it lands in, and
            // none of the entry's own: replace them.
            return tree::copy_acls_at(parent.as_raw_fd(), &name, dst).map_err(|e| failed(&e, src));
        }
        let e = io::Error::last_os_error();
        let culprit = match e.raw_os_error() {
            Some(libc::EACCES | libc::EPERM) => unix::find_unreadable(&src),
            _ => None,
        }
        .unwrap_or(src);
        Err(failed(&e, culprit))
    }

    /// A plain recursive copy: `--copy` must copy, never `clonefile`.
    fn copy_within(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()> {
        tree::transfer(root, rel, dst, Data::Copy)
    }

    fn rename_exclusive(&self, from: &Path, to: &Path) -> Result<()> {
        let (cf, ct) = (c_path(from)?, c_path(to)?);
        // SAFETY: valid C strings.
        if unsafe { libc::renamex_np(cf.as_ptr(), ct.as_ptr(), libc::RENAME_EXCL) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EEXIST) {
            return Err(Error::new(
                ErrorCode::DestExists,
                format!("destination `{}` already exists", to.display()),
            ));
        }
        Err(io_error(to, &e))
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        unix::remove_dir_all(path, unix::make_removable)
    }

    /// Durable write: the journal must reach the disk before the step it
    /// describes.
    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()> {
        unix::write_file(path, contents, full_sync)
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
        tree::copy_permissions(from, to)
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

    fn lock_store_shared(&self, path: &Path) -> Result<SharedLock> {
        unix::lock_store_shared(path)
    }

    fn stat_within(&self, root: &Path, rel: &Path) -> Result<Option<EntryMeta>> {
        tree::stat_entry(root, rel)
    }

    fn mirror_entry(&self, root: &Path, rel: &Path, dst: &Path) -> Result<u64> {
        tree::mirror(root, rel, dst)
    }

    fn mirror_dir_meta(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()> {
        tree::mirror_dir_meta(root, rel, dst)
    }

    fn symlink_exclusive(&self, target: &Path, link: &Path) -> Result<()> {
        unix::symlink_exclusive(target, link)
    }

    fn read_link(&self, path: &Path) -> Result<Option<PathBuf>> {
        unix::read_link(path)
    }

    fn append_file(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        unix::append_file(path, bytes, full_sync)
    }

    fn flush(&self, path: &Path) -> Result<()> {
        unix::flush(path)
    }

    fn is_writable(&self, path: &Path) -> Result<bool> {
        unix::is_writable(path)
    }

    fn space(&self, path: &Path) -> Result<Space> {
        unix::space(path)
    }
}

#[cfg(test)]
mod tests {
    //! Adapter tests against the real filesystem (`$TMPDIR` is APFS on macOS).

    use super::*;
    use crate::domain::ErrorCode;
    use crate::fs::CowFsExt as _;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // U-FS-01
    #[test]
    fn tmpdir_volume_is_apfs_with_device_id() {
        let d = tmp();
        let v = ApfsFs.volume(d.path()).unwrap();
        assert_eq!(v.fs_type, "apfs");
        assert_eq!(v.device, fs::metadata(d.path()).unwrap().dev() as u64);
    }

    // U-FS-02
    #[test]
    fn clones_file_preserving_content_and_mode() {
        let d = tmp();
        let src = d.path().join("a.sh");
        fs::write(&src, b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();
        let dst = d.path().join("b.sh");
        ApfsFs.clone_entry(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap());
        assert_eq!(
            fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    // U-FS-03
    #[test]
    fn clones_directory_recursively() {
        let d = tmp();
        fs::create_dir_all(d.path().join("src/x/y")).unwrap();
        fs::write(d.path().join("src/x/y/f"), b"deep").unwrap();
        ApfsFs
            .clone_entry(&d.path().join("src"), &d.path().join("dst"))
            .unwrap();
        assert_eq!(fs::read(d.path().join("dst/x/y/f")).unwrap(), b"deep");
    }

    // U-FS-04
    #[test]
    fn clones_symlink_itself_not_target() {
        let d = tmp();
        fs::create_dir(d.path().join("target_dir")).unwrap();
        symlink("target_dir", d.path().join("link")).unwrap();
        ApfsFs
            .clone_entry(&d.path().join("link"), &d.path().join("link2"))
            .unwrap();
        let md = fs::symlink_metadata(d.path().join("link2")).unwrap();
        assert!(md.file_type().is_symlink());
        assert_eq!(
            fs::read_link(d.path().join("link2")).unwrap(),
            PathBuf::from("target_dir")
        );
    }

    // U-FS-05
    #[test]
    fn clone_onto_existing_path_fails_without_touching_it() {
        let d = tmp();
        fs::write(d.path().join("a"), b"new").unwrap();
        fs::write(d.path().join("b"), b"keep").unwrap();
        let err = ApfsFs
            .clone_entry(&d.path().join("a"), &d.path().join("b"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        assert_eq!(fs::read(d.path().join("b")).unwrap(), b"keep");
    }

    // U-FS-06
    #[test]
    fn rename_exclusive_moves_directory() {
        let d = tmp();
        fs::create_dir(d.path().join("s")).unwrap();
        fs::write(d.path().join("s/f"), b"x").unwrap();
        ApfsFs
            .rename_exclusive(&d.path().join("s"), &d.path().join("t"))
            .unwrap();
        assert!(!d.path().join("s").exists());
        assert_eq!(fs::read(d.path().join("t/f")).unwrap(), b"x");
    }

    // U-FS-07: plain rename(2) would replace an empty dir; RENAME_EXCL must not.
    #[test]
    fn rename_exclusive_refuses_existing_empty_dir() {
        let d = tmp();
        fs::create_dir(d.path().join("s")).unwrap();
        fs::create_dir(d.path().join("t")).unwrap();
        let err = ApfsFs
            .rename_exclusive(&d.path().join("s"), &d.path().join("t"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::DestExists);
        assert!(d.path().join("s").is_dir(), "source left in place");
    }

    // U-FS-08
    #[test]
    fn rename_exclusive_refuses_dangling_symlink() {
        let d = tmp();
        fs::create_dir(d.path().join("s")).unwrap();
        symlink("nowhere", d.path().join("t")).unwrap();
        let err = ApfsFs
            .rename_exclusive(&d.path().join("s"), &d.path().join("t"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::DestExists);
        assert!(
            fs::symlink_metadata(d.path().join("t"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    fn chflags(path: &Path, flag: &str) {
        let st = std::process::Command::new("chflags")
            .arg(flag)
            .arg(path)
            .status()
            .unwrap();
        assert!(st.success());
    }

    // U-FS-12: immutable files copied by the clone must not block cleanup.
    #[test]
    fn remove_dir_all_clears_immutable_flags() {
        let d = tmp();
        fs::create_dir_all(d.path().join("t/sub")).unwrap();
        fs::write(d.path().join("t/sub/locked"), b"x").unwrap();
        chflags(&d.path().join("t/sub/locked"), "uchg");
        let res = ApfsFs.remove_dir_all(&d.path().join("t"));
        if d.path().join("t/sub/locked").exists() {
            chflags(&d.path().join("t/sub/locked"), "nouchg");
        }
        res.unwrap();
        assert!(!d.path().join("t").exists());
    }

    // U-FS-13: clone keeps BSD flags (documents why U-FS-12 matters).
    #[test]
    fn clone_preserves_immutable_flag() {
        let d = tmp();
        fs::write(d.path().join("a"), b"x").unwrap();
        chflags(&d.path().join("a"), "uchg");
        let res = ApfsFs.clone_entry(&d.path().join("a"), &d.path().join("b"));
        let immutable = fs::remove_file(d.path().join("b")).is_err();
        chflags(&d.path().join("a"), "nouchg");
        if d.path().join("b").exists() {
            chflags(&d.path().join("b"), "nouchg");
        }
        res.unwrap();
        assert!(immutable, "clone should carry uchg");
    }

    // U-FS-17: APFS rejects names that are not valid UTF-8.
    #[test]
    fn rename_to_non_utf8_name_is_invalid_path() {
        use std::os::unix::ffi::OsStrExt;
        let d = tmp();
        fs::create_dir(d.path().join("s")).unwrap();
        let bad = d.path().join(std::ffi::OsStr::from_bytes(b"bad-\xff"));
        let err = ApfsFs
            .rename_exclusive(&d.path().join("s"), &bad)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidPath);
    }

    // U-FS-18: APFS is normalization-insensitive; NFC and NFD name the same entry.
    #[test]
    fn rename_exclusive_sees_normalization_equivalent_name() {
        let d = tmp();
        fs::create_dir(d.path().join("s")).unwrap();
        fs::create_dir(d.path().join("cafe\u{301}")).unwrap();
        let err = ApfsFs
            .rename_exclusive(&d.path().join("s"), &d.path().join("caf\u{e9}"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::DestExists);
    }

    // U-FS-21: the copy flavour reproduces a tree like a clone: contents, modes,
    // symlinks (never followed), empty directories.
    #[test]
    fn copy_entry_reproduces_a_tree() {
        let d = tmp();
        let src = d.path().join("src");
        fs::create_dir_all(src.join("sub/empty")).unwrap();
        fs::write(src.join("sub/run.sh"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(src.join("sub/run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(src.join("ro.txt"), b"ro").unwrap();
        fs::set_permissions(src.join("ro.txt"), fs::Permissions::from_mode(0o444)).unwrap();
        symlink("sub", src.join("dir-link")).unwrap();
        symlink("nowhere", src.join("dangling")).unwrap();
        ApfsFs.copy_entry(&src, &d.path().join("dst")).unwrap();
        let dst = d.path().join("dst");
        assert_eq!(fs::read(dst.join("sub/run.sh")).unwrap(), b"#!/bin/sh\n");
        let mode = |p: &str| fs::metadata(dst.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode("sub/run.sh"), mode("ro.txt")), (0o755, 0o444));
        assert!(dst.join("sub/empty").is_dir());
        for (link, target) in [("dir-link", "sub"), ("dangling", "nowhere")] {
            assert!(fs::symlink_metadata(dst.join(link)).unwrap().is_symlink());
            assert_eq!(
                fs::read_link(dst.join(link)).unwrap(),
                PathBuf::from(target)
            );
        }
    }

    // U-FS-22: ... and fails like it: onto an existing path, nothing is touched.
    #[test]
    fn copy_entry_onto_existing_path_fails_without_touching_it() {
        let d = tmp();
        fs::write(d.path().join("a"), b"new").unwrap();
        fs::write(d.path().join("b"), b"keep").unwrap();
        let err = ApfsFs
            .copy_entry(&d.path().join("a"), &d.path().join("b"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        assert_eq!(fs::read(d.path().join("b")).unwrap(), b"keep");
    }
}
