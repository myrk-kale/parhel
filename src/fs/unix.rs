//! Platform-independent Unix parts of the [`CowFs`](crate::ports::CowFs) port,
//! shared by the macOS and Linux adapters: path and metadata queries, staging
//! directories, the relocation scan, `flock`-based locks and plain file helpers.
//! Only the copy-on-write primitives (clone, exclusive rename, filesystem
//! identity, durable sync, unlocking immutable files) live in the adapters.

use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::domain::paths::STAGING_PREFIX;
use crate::domain::{Error, ErrorCode, Result};
use crate::ports::{FileLock, Scan};

pub(crate) fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        Error::new(
            ErrorCode::InvalidPath,
            format!("`{}` contains a NUL byte", path.display()),
        )
    })
}

pub(crate) fn io_error(path: &Path, e: &io::Error) -> Error {
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
pub(crate) fn find_unreadable(root: &Path) -> Option<PathBuf> {
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

pub(crate) fn scan(
    root: &Path,
    needles: &[Vec<u8>],
    max_len: u64,
    skip: &(dyn Fn(&Path) -> bool + Sync),
) -> Scan {
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
    scan
}

/// `st_dev` of `path` (follows symlinks).
pub(crate) fn device_of(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path).map_err(|e| io_error(path, &e))?.dev())
}

pub(crate) fn canonicalize(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|e| io_error(path, &e))
}

pub(crate) fn exists_no_follow(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => Ok(false),
        Err(e) => Err(io_error(path, &e)),
    }
}

pub(crate) fn is_dir(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
}

pub(crate) fn is_dir_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

pub(crate) fn create_staging_dir(parent: &Path) -> Result<PathBuf> {
    let parent = canonicalize(parent)?;
    let dir = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .rand_bytes(8)
        .tempdir_in(&parent)
        .map_err(|e| io_error(&parent, &e))?;
    Ok(dir.keep())
}

pub(crate) fn list_dir(path: &Path) -> Result<Vec<OsString>> {
    fs::read_dir(path)
        .and_then(|rd| rd.map(|e| e.map(|e| e.file_name())).collect())
        .map_err(|e| io_error(path, &e))
}

pub(crate) fn create_dir(path: &Path) -> Result<()> {
    fs::create_dir(path).map_err(|e| io_error(path, &e))
}

/// Remove a tree; if that fails, `make_removable` (platform specific: clear
/// immutable flags, make directories writable) runs on it and removal is retried.
pub(crate) fn remove_dir_all(path: &Path, make_removable: fn(&Path)) -> Result<()> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    if fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    make_removable(path);
    fs::remove_dir_all(path).map_err(|e| io_error(path, &e))
}

/// Write `contents` to `path`, then make them durable with `sync` (the
/// platform's strongest flush: `F_FULLFSYNC` on macOS, `fsync` elsewhere).
pub(crate) fn write_file(
    path: &Path,
    contents: &[u8],
    sync: fn(&File) -> io::Result<()>,
) -> Result<()> {
    let mut file = File::create(path).map_err(|e| io_error(path, &e))?;
    io::Write::write_all(&mut file, contents).map_err(|e| io_error(path, &e))?;
    sync(&file).map_err(|e| io_error(path, &e))
}

pub(crate) fn read_file(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|e| io_error(path, &e))
}

pub(crate) fn replace_contents(path: &Path, contents: &[u8]) -> Result<()> {
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

pub(crate) fn copy_mode(from: &Path, to: &Path) -> Result<()> {
    let perms = fs::metadata(from)
        .map_err(|e| io_error(from, &e))?
        .permissions();
    fs::set_permissions(to, perms).map_err(|e| io_error(to, &e))
}

pub(crate) fn remove_file(path: &Path) -> Result<()> {
    fs::remove_file(path).map_err(|e| io_error(path, &e))
}

pub(crate) fn replace_symlink(path: &Path, target: &Path) -> Result<()> {
    fs::remove_file(path).map_err(|e| io_error(path, &e))?;
    std::os::unix::fs::symlink(target, path).map_err(|e| io_error(path, &e))
}

pub(crate) fn lock_exclusive(path: &Path) -> Result<FileLock> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| io_error(path, &e))?;
    flock(&file, libc::LOCK_EX | libc::LOCK_NB).map_err(|e| io_error(path, &e))?;
    Ok(FileLock::new(file))
}

pub(crate) fn is_locked(path: &Path) -> Result<bool> {
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

#[cfg(test)]
mod tests {
    //! Shared helpers against the real filesystem (`$TMPDIR`).

    use super::*;
    use crate::domain::ErrorCode;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn canon(dir: &tempfile::TempDir) -> PathBuf {
        fs::canonicalize(dir.path()).unwrap()
    }

    // U-FS-09
    #[test]
    fn staging_dirs_are_unique_hidden_and_recognized() {
        let d = tmp();
        let a = create_staging_dir(d.path()).unwrap();
        let b = create_staging_dir(d.path()).unwrap();
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
        assert!(exists_no_follow(&d.path().join("dangling")).unwrap());
        assert!(!exists_no_follow(&d.path().join("absent")).unwrap());
    }

    // U-FS-11: /var -> /private/var on macOS.
    #[test]
    fn canonicalize_resolves_symlinked_tmp() {
        let d = tmp();
        assert_eq!(canonicalize(d.path()).unwrap(), canon(&d));
    }

    // U-FS-14
    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let d = tmp();
        let p = d.path().join("journal");
        fs::write(&p, b"").unwrap();
        assert!(!is_locked(&p).unwrap());
        let guard = lock_exclusive(&p).unwrap();
        assert!(is_locked(&p).unwrap(), "held lock must be visible");
        drop(guard);
        // Parallel tests fork constantly; a child between fork and exec briefly
        // shares the lock's open file description. Release must happen promptly.
        let released = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            !is_locked(&p).unwrap()
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
        let held = is_locked(&p);
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
        let res = create_staging_dir(&parent);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(res.unwrap_err().code, ErrorCode::PermissionDenied);
    }

    // U-FS-19
    #[test]
    fn is_dir_no_follow_rejects_symlinks() {
        let d = tmp();
        fs::create_dir(d.path().join("dir")).unwrap();
        fs::write(d.path().join("file"), b"x").unwrap();
        symlink("dir", d.path().join("link")).unwrap();
        assert!(is_dir_no_follow(&d.path().join("dir")));
        assert!(!is_dir_no_follow(&d.path().join("link")));
        assert!(is_dir(&d.path().join("link")), "is_dir still follows");
        assert!(!is_dir_no_follow(&d.path().join("file")));
        assert!(!is_dir_no_follow(&d.path().join("absent")));
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
        let mut scan = scan(&root, &[b"/w/src".to_vec()], 100, &skip);
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
