//! [`CowFs`] port on macOS: `clonefile(2)`, `renamex_np(2)`, `statfs(2)`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::domain::Result;
use crate::ports::{CowFs, FileLock, VolumeInfo};

/// `CLONE_NOFOLLOW` from `<sys/clonefile.h>` (not exported by `libc`): clone a
/// symlink itself instead of its target.
pub const CLONE_NOFOLLOW: u32 = 0x0001;

#[derive(Debug, Clone, Copy, Default)]
pub struct ApfsFs;

impl CowFs for ApfsFs {
    fn volume(&self, path: &Path) -> Result<VolumeInfo> {
        let _ = path;
        todo!("ApfsFs::volume")
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        let _ = path;
        todo!("ApfsFs::canonicalize")
    }

    fn exists_no_follow(&self, path: &Path) -> Result<bool> {
        let _ = path;
        todo!("ApfsFs::exists_no_follow")
    }

    fn is_dir(&self, path: &Path) -> Result<bool> {
        let _ = path;
        todo!("ApfsFs::is_dir")
    }

    fn create_staging_dir(&self, parent: &Path) -> Result<PathBuf> {
        let _ = parent;
        todo!("ApfsFs::create_staging_dir")
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<OsString>> {
        let _ = path;
        todo!("ApfsFs::list_dir")
    }

    fn create_dir(&self, path: &Path) -> Result<()> {
        let _ = path;
        todo!("ApfsFs::create_dir")
    }

    fn clone_entry(&self, src: &Path, dst: &Path) -> Result<()> {
        let _ = (src, dst, CLONE_NOFOLLOW);
        todo!("ApfsFs::clone_entry")
    }

    fn rename_exclusive(&self, from: &Path, to: &Path) -> Result<()> {
        let _ = (from, to);
        todo!("ApfsFs::rename_exclusive")
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        let _ = path;
        todo!("ApfsFs::remove_dir_all")
    }

    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let _ = (path, contents);
        todo!("ApfsFs::write_file")
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let _ = path;
        todo!("ApfsFs::read_file")
    }

    fn lock_exclusive(&self, path: &Path) -> Result<FileLock> {
        let _ = path;
        todo!("ApfsFs::lock_exclusive")
    }

    fn is_locked(&self, path: &Path) -> Result<bool> {
        let _ = path;
        todo!("ApfsFs::is_locked")
    }
}

#[cfg(test)]
mod tests {
    //! Adapter tests against the real filesystem (`$TMPDIR` is APFS on macOS).

    use super::*;
    use crate::domain::ErrorCode;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn canon(dir: &tempfile::TempDir) -> PathBuf {
        fs::canonicalize(dir.path()).unwrap()
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

    // U-FS-09
    #[test]
    fn staging_dirs_are_unique_hidden_and_recognized() {
        let d = tmp();
        let a = ApfsFs.create_staging_dir(d.path()).unwrap();
        let b = ApfsFs.create_staging_dir(d.path()).unwrap();
        assert_ne!(a, b);
        for s in [&a, &b] {
            assert!(s.is_dir());
            assert_eq!(s.parent().unwrap(), canon(&d));
            assert!(crate::domain::paths::is_staging_name(
                s.file_name().unwrap()
            ));
        }
    }

    // U-FS-10
    #[test]
    fn exists_no_follow_sees_dangling_symlink() {
        let d = tmp();
        symlink("nowhere", d.path().join("dangling")).unwrap();
        assert!(ApfsFs.exists_no_follow(&d.path().join("dangling")).unwrap());
        assert!(!ApfsFs.exists_no_follow(&d.path().join("absent")).unwrap());
    }

    // U-FS-11: /var -> /private/var on macOS.
    #[test]
    fn canonicalize_resolves_symlinked_tmp() {
        let d = tmp();
        assert_eq!(ApfsFs.canonicalize(d.path()).unwrap(), canon(&d));
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

    // U-FS-14
    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let d = tmp();
        let p = d.path().join("journal");
        fs::write(&p, b"").unwrap();
        assert!(!ApfsFs.is_locked(&p).unwrap());
        let guard = ApfsFs.lock_exclusive(&p).unwrap();
        assert!(ApfsFs.is_locked(&p).unwrap(), "held lock must be visible");
        drop(guard);
        assert!(!ApfsFs.is_locked(&p).unwrap(), "released on drop");
    }

    // U-FS-15
    #[test]
    fn lock_held_by_another_process_is_detected() {
        let d = tmp();
        let p = d.path().join("journal");
        fs::write(&p, b"").unwrap();
        let mut child = std::process::Command::new("/usr/bin/python3")
            .args(["-I", "-c", "import fcntl,sys,time;f=open(sys.argv[1]);fcntl.flock(f,fcntl.LOCK_EX);print('locked',flush=True);time.sleep(30)"])
            .arg(&p)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let held = ApfsFs.is_locked(&p);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(held.unwrap());
    }

    // U-FS-16
    #[test]
    fn staging_in_unwritable_parent_is_permission_denied() {
        let d = tmp();
        let parent = d.path().join("ro");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let res = ApfsFs.create_staging_dir(&parent);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(res.unwrap_err().code, ErrorCode::PermissionDenied);
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
}
