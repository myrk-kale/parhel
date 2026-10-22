//! [`CowFs`] port on macOS: `clonefile(2)`, `renamex_np(2)`, `statfs(2)`, `flock(2)`.

use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::domain::paths::STAGING_PREFIX;
use crate::domain::{Error, ErrorCode, Result};
use crate::ports::{CowFs, FileLock, Scan, VolumeInfo};

/// `CLONE_NOFOLLOW` from `<sys/clonefile.h>` (not exported by `libc`): clone a
/// symlink itself instead of its target.
pub const CLONE_NOFOLLOW: u32 = 0x0001;

#[derive(Debug, Clone, Copy, Default)]
pub struct ApfsFs;

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        Error::new(
            ErrorCode::InvalidPath,
            format!("`{}` contains a NUL byte", path.display()),
        )
    })
}

fn io_error(path: &Path, e: &io::Error) -> Error {
    let code = match (e.raw_os_error(), e.kind()) {
        (Some(libc::EACCES | libc::EPERM), _) | (None, io::ErrorKind::PermissionDenied) => {
            ErrorCode::PermissionDenied
        }
        (Some(libc::EILSEQ), _) => ErrorCode::InvalidPath,
        _ => ErrorCode::Io,
    };
    Error::new(code, format!("`{}`: {e}", path.display()))
}

/// First entry under `root` that cannot be read (to name the culprit when a
/// whole-directory clone fails with `EACCES`).
fn find_unreadable(root: &Path) -> Option<PathBuf> {
    let md = fs::symlink_metadata(root).ok()?;
    if md.is_dir() {
        let Ok(entries) = fs::read_dir(root) else {
            return Some(root.to_path_buf());
        };
        for entry in entries.flatten() {
            if let Some(found) = find_unreadable(&entry.path()) {
                return Some(found);
            }
        }
        None
    } else if md.is_file() && File::open(root).is_err() {
        Some(root.to_path_buf())
    } else {
        None
    }
}

/// Clear BSD flags (`uchg`, `uappnd`) and make directories writable, recursively.
fn make_removable(path: &Path) {
    let Ok(md) = fs::symlink_metadata(path) else {
        return;
    };
    if let Ok(c) = c_path(path) {
        // SAFETY: valid C string; lchflags never follows symlinks.
        unsafe { libc::lchflags(c.as_ptr(), 0) };
    }
    if md.is_dir() {
        let _ = fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_removable(&entry.path());
            }
        }
    }
}

/// The whole file if it is at most `max_len` bytes, `None` if it is larger.
fn read_capped(path: &Path, max_len: u64) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    io::Read::read_to_end(
        &mut io::Read::take(File::open(path)?, max_len + 1),
        &mut buf,
    )?;
    Ok((buf.len() as u64 <= max_len).then_some(buf))
}

/// Whether any needle (all start with `/`) occurs in `bytes`.
fn contains_any(bytes: &[u8], needles: &[Vec<u8>]) -> bool {
    bytes
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'/' && needles.iter().any(|n| bytes[i..].starts_with(n)))
}

fn flock(file: &File, op: libc::c_int) -> io::Result<()> {
    // SAFETY: valid open fd.
    if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
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
        let device = fs::metadata(path).map_err(|e| io_error(path, &e))?.dev();
        Ok(VolumeInfo {
            fs_type: String::from_utf8_lossy(&name).into_owned(),
            device,
        })
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        fs::canonicalize(path).map_err(|e| io_error(path, &e))
    }

    fn exists_no_follow(&self, path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => Ok(false),
            Err(e) => Err(io_error(path, &e)),
        }
    }

    fn is_dir(&self, path: &Path) -> Result<bool> {
        Ok(fs::metadata(path).is_ok_and(|m| m.is_dir()))
    }

    fn is_dir_no_follow(&self, path: &Path) -> Result<bool> {
        Ok(fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()))
    }

    fn create_staging_dir(&self, parent: &Path) -> Result<PathBuf> {
        let parent = self.canonicalize(parent)?;
        let dir = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .rand_bytes(8)
            .tempdir_in(&parent)
            .map_err(|e| io_error(&parent, &e))?;
        Ok(dir.keep())
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<OsString>> {
        fs::read_dir(path)
            .and_then(|rd| rd.map(|e| e.map(|e| e.file_name())).collect())
            .map_err(|e| io_error(path, &e))
    }

    fn create_dir(&self, path: &Path) -> Result<()> {
        fs::create_dir(path).map_err(|e| io_error(path, &e))
    }

    fn clone_entry(&self, src: &Path, dst: &Path) -> Result<()> {
        let (cs, cd) = (c_path(src)?, c_path(dst)?);
        // SAFETY: valid C strings.
        if unsafe { libc::clonefile(cs.as_ptr(), cd.as_ptr(), CLONE_NOFOLLOW) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        let code = match e.raw_os_error() {
            Some(libc::EXDEV) => ErrorCode::CrossVolume,
            _ => ErrorCode::CloneFailed,
        };
        let culprit = match e.raw_os_error() {
            Some(libc::EACCES | libc::EPERM) => find_unreadable(src),
            _ => None,
        }
        .unwrap_or_else(|| src.to_path_buf());
        Err(Error::new(
            code,
            format!("cannot clone `{}`: {e}", culprit.display()),
        ))
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
        if fs::symlink_metadata(path).is_err() {
            return Ok(());
        }
        if fs::remove_dir_all(path).is_ok() {
            return Ok(());
        }
        make_removable(path);
        fs::remove_dir_all(path).map_err(|e| io_error(path, &e))
    }

    /// Durable write: the journal must reach the disk before the step it
    /// describes. On macOS only `F_FULLFSYNC` flushes the drive's cache.
    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()> {
        let mut file = File::create(path).map_err(|e| io_error(path, &e))?;
        io::Write::write_all(&mut file, contents).map_err(|e| io_error(path, &e))?;
        // SAFETY: valid open fd.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            file.sync_all().map_err(|e| io_error(path, &e))?;
        }
        Ok(())
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        fs::read(path).map_err(|e| io_error(path, &e))
    }

    fn scan(
        &self,
        root: &Path,
        needles: &[Vec<u8>],
        max_len: u64,
        skip: &(dyn Fn(&Path) -> bool + Sync),
    ) -> Result<Scan> {
        let mut scan = Scan::default();
        let mut files = Vec::new();
        let mut stack = vec![PathBuf::new()];
        // The listing already says what each entry is: no lstat per file.
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(root.join(&dir)) else {
                scan.unreadable += 1;
                continue;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    scan.unreadable += 1;
                    continue;
                };
                let rel = dir.join(entry.file_name());
                if skip(&rel) {
                    continue;
                }
                let Ok(kind) = entry.file_type() else {
                    scan.unreadable += 1;
                    continue;
                };
                if kind.is_dir() {
                    stack.push(rel);
                } else if kind.is_symlink() {
                    if let Ok(target) = fs::read_link(entry.path()) {
                        let bytes = target.as_os_str().as_bytes();
                        if needles.iter().any(|n| bytes.starts_with(n)) {
                            scan.links.push((rel, target));
                        }
                    }
                } else if kind.is_file() {
                    if entry.file_name() == ".git" {
                        scan.gitfiles.push(rel);
                    } else if !needles.is_empty() {
                        files.push(rel);
                    }
                }
            }
        }
        // Reading dominates (one open per file, slower on fresh clones): parallel.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(4));
        let chunk = files.len().div_ceil(threads).max(1);
        let parts: Vec<(Vec<PathBuf>, usize)> = std::thread::scope(|s| {
            let handles: Vec<_> = files
                .chunks(chunk)
                .map(|part| {
                    s.spawn(move || {
                        let (mut hits, mut unreadable) = (Vec::new(), 0);
                        for rel in part {
                            match read_capped(&root.join(rel), max_len) {
                                Ok(Some(bytes)) if contains_any(&bytes, needles) => {
                                    hits.push(rel.clone());
                                }
                                Ok(_) => {}
                                Err(_) => unreadable += 1,
                            }
                        }
                        (hits, unreadable)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });
        for (hits, unreadable) in parts {
            scan.matches.extend(hits);
            scan.unreadable += unreadable;
        }
        scan.matches.sort();
        scan.gitfiles.sort();
        scan.links.sort();
        Ok(scan)
    }

    fn replace_contents(&self, path: &Path, contents: &[u8]) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::metadata(path)
            .map_err(|e| io_error(path, &e))?
            .permissions();
        let read_only = perms.mode() & 0o200 == 0;
        if read_only {
            fs::set_permissions(path, fs::Permissions::from_mode(perms.mode() | 0o200))
                .map_err(|e| io_error(path, &e))?;
        }
        let written = fs::write(path, contents).map_err(|e| io_error(path, &e));
        if read_only {
            fs::set_permissions(path, perms).map_err(|e| io_error(path, &e))?;
        }
        written
    }

    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        let perms = fs::metadata(from)
            .map_err(|e| io_error(from, &e))?
            .permissions();
        fs::set_permissions(to, perms).map_err(|e| io_error(to, &e))
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).map_err(|e| io_error(path, &e))
    }

    fn replace_symlink(&self, path: &Path, target: &Path) -> Result<()> {
        fs::remove_file(path).map_err(|e| io_error(path, &e))?;
        std::os::unix::fs::symlink(target, path).map_err(|e| io_error(path, &e))
    }

    fn lock_exclusive(&self, path: &Path) -> Result<FileLock> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| io_error(path, &e))?;
        flock(&file, libc::LOCK_EX | libc::LOCK_NB).map_err(|e| io_error(path, &e))?;
        Ok(FileLock::new(file))
    }

    fn is_locked(&self, path: &Path) -> Result<bool> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_error(path, &e)),
        };
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => {
                let _ = flock(&file, libc::LOCK_UN);
                Ok(false)
            }
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(true),
            Err(e) => Err(io_error(path, &e)),
        }
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
        // Parallel tests fork constantly; a child between fork and exec briefly
        // shares the lock's open file description. Release must happen promptly.
        let released = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            !ApfsFs.is_locked(&p).unwrap()
        });
        assert!(released, "released on drop");
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

    // U-FS-19
    #[test]
    fn is_dir_no_follow_rejects_symlinks() {
        let d = tmp();
        fs::create_dir(d.path().join("dir")).unwrap();
        fs::write(d.path().join("file"), b"x").unwrap();
        symlink("dir", d.path().join("link")).unwrap();
        assert!(ApfsFs.is_dir_no_follow(&d.path().join("dir")).unwrap());
        assert!(!ApfsFs.is_dir_no_follow(&d.path().join("link")).unwrap());
        assert!(
            ApfsFs.is_dir(&d.path().join("link")).unwrap(),
            "is_dir still follows"
        );
        assert!(!ApfsFs.is_dir_no_follow(&d.path().join("file")).unwrap());
        assert!(!ApfsFs.is_dir_no_follow(&d.path().join("absent")).unwrap());
    }

    // U-FS-20: the parallel scan finds exactly the relocation candidates.
    #[test]
    fn scan_reports_matches_gitfiles_and_links() {
        let d = tmp();
        let root = canon(&d);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("skipped")).unwrap();
        fs::write(root.join("a/hit.txt"), b"x=/w/src/y").unwrap();
        fs::write(root.join("a/miss.txt"), b"nothing").unwrap();
        fs::write(root.join("a/b/.git"), b"gitdir: /else\n").unwrap();
        fs::write(root.join("skipped/hit.txt"), b"/w/src").unwrap();
        let mut big = b"/w/src".to_vec();
        big.resize(200, b'x');
        fs::write(root.join("a/big.txt"), &big).unwrap();
        symlink("/w/src/lib", root.join("a/abs")).unwrap();
        symlink("rel", root.join("a/rel")).unwrap();
        for i in 0..500 {
            fs::write(
                root.join(format!("a/f{i}")),
                if i == 250 { &b"/w/src"[..] } else { b"z" },
            )
            .unwrap();
        }
        let skip = |p: &Path| p == Path::new("skipped");
        let mut scan = ApfsFs
            .scan(&root, &[b"/w/src".to_vec()], 100, &skip)
            .unwrap();
        scan.matches.sort();
        assert_eq!(
            scan.matches,
            vec![PathBuf::from("a/f250"), PathBuf::from("a/hit.txt")]
        );
        assert_eq!(scan.gitfiles, vec![PathBuf::from("a/b/.git")]);
        assert_eq!(
            scan.links,
            vec![(PathBuf::from("a/abs"), PathBuf::from("/w/src/lib"))]
        );
        assert_eq!(scan.unreadable, 0);
    }
}
