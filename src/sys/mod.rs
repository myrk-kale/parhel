//! The machine, for `git parhel setup`: privileged system calls and programs.
//!
//! `setup` runs as root and works in places a user controls (the store's own
//! filesystem is owned by the user once it is set up), so the adapter never follows a
//! symbolic link at the last component of a path it reads, writes or changes: a link
//! planted there must not turn the command into an overwrite of `/etc/...` or a
//! `chown` of someone else's directory.

use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::domain::{Error, ErrorCode, Result};
use crate::fs::unix::io_error;
use crate::ports::{Host, ImageUsage, Owner, UserInfo};

/// The real [`Host`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemHost;

/// `FS_IMMUTABLE_FL` of `<linux/fs.h>`.
#[cfg(target_os = "linux")]
const FS_IMMUTABLE_FL: libc::c_int = 0x0000_0010;

/// Flags that make `open` refuse a symbolic link at the last component.
const NO_FOLLOW: libc::c_int = libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// The most `read_text` reads: what it is used for (a marker, a unit, `/proc/filesystems`)
/// is a few hundred bytes, and a user can make the files it reads as large as they like.
const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Programs are looked up in the absolute directories of `PATH` only: an empty or
/// relative entry would make root run whatever the current directory holds.
fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return is_executable(Path::new(program)).then(|| PathBuf::from(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|md| md.is_file() && md.permissions().mode() & 0o111 != 0)
}

/// The mount points listed in `/proc/self/mountinfo` (fifth field, octal-escaped).
fn mount_points(mountinfo: &str) -> impl Iterator<Item = PathBuf> + '_ {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(OsString::from_vec(unescape_octal(field))))
}

/// `\040` (space), `\011`, `\012` and `\134` as the kernel writes them.
fn unescape_octal(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let octal = bytes.get(at + 1..at + 4).filter(|digits| {
            bytes[at] == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        });
        if let Some(digits) = octal {
            let value = digits
                .iter()
                .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(value & 0xff).unwrap_or(b'?'));
            at += 4;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    out
}

/// Give `file` its owner (`None`: root). Nothing is asked of the kernel when the file
/// already has it, so a process that is not root can still keep what is already its own.
fn fchown(file: &File, owner: Owner) -> io::Result<()> {
    let (uid, gid) = owner.unwrap_or((0, 0));
    let current = file.metadata()?;
    if (current.uid(), current.gid()) == (uid, gid) {
        return Ok(());
    }
    // SAFETY: the descriptor is open for the duration of the call.
    if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// `mode_t` is 16 bits on macOS; modes here are at most 0o7777.
#[allow(clippy::cast_possible_truncation, clippy::unnecessary_cast)]
fn fchmod(file: &File, mode: u32) -> io::Result<()> {
    // SAFETY: the descriptor is open for the duration of the call.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Make the directory entry that holds `path` durable.
fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

fn errno_of(e: &io::Error) -> Option<i32> {
    e.raw_os_error()
}

impl Host for SystemHost {
    fn is_linux(&self) -> bool {
        cfg!(target_os = "linux")
    }

    fn is_root(&self) -> bool {
        // SAFETY: `geteuid` has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn user(&self, name: &str) -> Result<Option<UserInfo>> {
        let Ok(c_name) = CString::new(name) else {
            return Ok(None);
        };
        // SAFETY: `passwd` is plain data; `getpwnam_r` fills it in.
        let mut record: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0u8; 1024];
        loop {
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: every pointer is valid for the call; `buffer` outlives it.
            let rc = unsafe {
                libc::getpwnam_r(
                    c_name.as_ptr(),
                    &raw mut record,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &raw mut found,
                )
            };
            match rc {
                0 if found.is_null() => return Ok(None),
                0 => {
                    return Ok(Some(UserInfo {
                        name: name.to_owned(),
                        uid: record.pw_uid,
                        gid: record.pw_gid,
                    }));
                }
                libc::ERANGE if buffer.len() < (1 << 20) => buffer.resize(buffer.len() * 2, 0),
                // POSIX lets "no such user" come back as an error too.
                libc::ENOENT | libc::ESRCH | libc::EBADF | libc::EPERM => return Ok(None),
                errno => {
                    return Err(Error::new(
                        ErrorCode::Io,
                        format!(
                            "looking up `{name}`: {}",
                            io::Error::from_raw_os_error(errno)
                        ),
                    ));
                }
            }
        }
    }

    fn tool_exists(&self, tool: &str) -> bool {
        resolve(tool).is_some()
    }

    fn systemd_running(&self) -> bool {
        // What `sd_booted()` checks.
        Path::new("/run/systemd/system").is_dir() && resolve("systemctl").is_some()
    }

    fn read_text(&self, path: &Path) -> Result<Option<String>> {
        // Root reads files a user can write: opening a FIFO for reading blocks until
        // somebody writes to it, so the file is opened without blocking, must turn out
        // to be a regular file, and is read up to a cap and no further.
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(NO_FOLLOW | libc::O_NONBLOCK)
            .open(path);
        let file = match opened {
            Ok(file) => file,
            Err(e) if matches!(errno_of(&e), Some(libc::ENOENT | libc::ENOTDIR)) => {
                return Ok(None);
            }
            Err(e) if errno_of(&e) == Some(libc::ELOOP) => {
                return Err(Error::new(
                    ErrorCode::Io,
                    format!(
                        "`{}` is a symbolic link: refusing to follow it as root",
                        path.display()
                    ),
                ));
            }
            Err(e) => return Err(io_error(path, &e)),
        };
        if !file.metadata().map_err(|e| io_error(path, &e))?.is_file() {
            return Err(Error::new(
                ErrorCode::Io,
                format!(
                    "`{}` is not a regular file: refusing to read it",
                    path.display()
                ),
            ));
        }
        let mut bytes = Vec::new();
        io::Read::read_to_end(
            &mut io::Read::take(&file, MAX_TEXT_BYTES as u64 + 1),
            &mut bytes,
        )
        .map_err(|e| io_error(path, &e))?;
        if bytes.len() > MAX_TEXT_BYTES {
            return Err(Error::new(
                ErrorCode::Io,
                format!(
                    "`{}` is larger than {MAX_TEXT_BYTES} bytes: refusing to read it",
                    path.display()
                ),
            ));
        }
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn exists(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok()
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<OsString>> {
        // A link where a directory should be is not listed: its target is not ours.
        match fs::symlink_metadata(path) {
            Ok(md) if md.is_dir() => {}
            Ok(_) => return Ok(Vec::new()),
            Err(e) if matches!(errno_of(&e), Some(libc::ENOENT | libc::ENOTDIR)) => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(io_error(path, &e)),
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(path).map_err(|e| io_error(path, &e))? {
            names.push(entry.map_err(|e| io_error(path, &e))?.file_name());
        }
        names.sort();
        Ok(names)
    }

    fn is_mount_point(&self, path: &Path) -> bool {
        if let Ok(text) = fs::read_to_string("/proc/self/mountinfo") {
            return mount_points(&text).any(|mount| mount == path);
        }
        // No procfs: a mount point is where the device changes.
        let (Ok(here), Some(parent)) = (fs::symlink_metadata(path), path.parent()) else {
            return false;
        };
        fs::symlink_metadata(parent).is_ok_and(|up| up.dev() != here.dev())
    }

    #[cfg(target_os = "linux")]
    fn is_immutable(&self, path: &Path) -> bool {
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | NO_FOLLOW)
            .open(path);
        let Ok(dir) = opened else {
            return false;
        };
        let mut flags: libc::c_int = 0;
        // SAFETY: the descriptor is open and `flags` is an `int`, which is what the
        // kernel stores for `FS_IOC_GETFLAGS`.
        let rc = unsafe { libc::ioctl(dir.as_raw_fd(), libc::FS_IOC_GETFLAGS, &raw mut flags) };
        rc == 0 && flags & FS_IMMUTABLE_FL != 0
    }

    #[cfg(not(target_os = "linux"))]
    fn is_immutable(&self, _path: &Path) -> bool {
        false
    }

    fn filesystem_of(&self, image: &Path) -> Result<Option<String>> {
        let Some(blkid) = resolve("blkid") else {
            return Err(Error::new(
                ErrorCode::MissingTool,
                "`blkid` (package util-linux) is needed to check the image for a filesystem",
            ));
        };
        let out = Command::new(blkid)
            .args(["-p", "-o", "export"])
            .arg(image)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| io_error(image, &e))?;
        match out.status.code() {
            // Nothing was found: no filesystem, no partition table, no RAID member.
            Some(2) => Ok(None),
            Some(0) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let found = text
                    .lines()
                    .find_map(|l| l.strip_prefix("TYPE="))
                    .map_or_else(|| "an unrecognized signature".to_owned(), str::to_owned);
                Ok(Some(found))
            }
            // Ambiguous (more than one signature) or a failure: not blank either way.
            _ => Err(Error::new(
                ErrorCode::Io,
                format!(
                    "`blkid -p {}` failed ({}): {}",
                    image.display(),
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            )),
        }
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let Some(exe) = resolve(program) else {
            return Err(Error::new(
                ErrorCode::MissingTool,
                format!("`{program}` is not installed (it is not in PATH)"),
            ));
        };
        let out = Command::new(&exe)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::new(ErrorCode::Io, format!("could not run `{program}`: {e}")))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let said = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("no output");
        Err(Error::new(
            ErrorCode::Io,
            format!(
                "`{program} {}` failed ({}): {said}",
                args.join(" "),
                out.status
            ),
        ))
    }

    fn create_dir(&self, path: &Path, mode: u32, owner: Owner) -> Result<()> {
        match fs::DirBuilder::new().mode(mode).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error(path, &e)),
        }
        // Mode and owner are set through the descriptor of a directory opened without
        // following links: what is there must be a directory, not a link to one.
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | NO_FOLLOW)
            .open(path)
            .map_err(|e| io_error(path, &e))?;
        fchown(&dir, owner).map_err(|e| io_error(path, &e))?;
        fchmod(&dir, mode).map_err(|e| io_error(path, &e))
    }

    fn write_file(&self, path: &Path, contents: &[u8], mode: u32, owner: Owner) -> Result<()> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(Error::new(
                ErrorCode::InvalidPath,
                format!("`{}` is not a file path", path.display()),
            ));
        };
        // A new name, then a rename over the old one: the file is never half written,
        // and a link planted at `path` is replaced, never followed.
        let mut temporary = OsString::from(".");
        temporary.push(name);
        temporary.push(".parhel-tmp");
        let temporary = dir.join(temporary);
        let _ = fs::remove_file(&temporary);
        let written = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(NO_FOLLOW)
                .open(&temporary)?;
            fchown(&file, owner)?;
            fchmod(&file, mode)?;
            file.write_all(contents)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            sync_parent(path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&temporary);
            return Err(io_error(path, &e));
        }
        Ok(())
    }

    fn create_image(&self, path: &Path, bytes: u64, preallocate: bool) -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NO_FOLLOW)
            .open(path)
            .map_err(|e| io_error(path, &e))?;
        let sized = (|| -> io::Result<()> {
            // Created by the process that is root: no chown needed.
            fchmod(&file, 0o600)?;
            if preallocate {
                allocate(&file, bytes)?;
            } else {
                file.set_len(bytes)?;
            }
            file.sync_all()?;
            sync_parent(path)
        })();
        if let Err(e) = sized {
            drop(file);
            let _ = fs::remove_file(path);
            let why = match errno_of(&e) {
                Some(libc::ENOSPC) => format!("not enough free space for {bytes} bytes"),
                Some(libc::EOPNOTSUPP) => {
                    "this filesystem cannot preallocate; leave --preallocate out".to_owned()
                }
                Some(libc::EFBIG) => "this filesystem cannot hold a file that large".to_owned(),
                _ => e.to_string(),
            };
            return Err(Error::new(
                ErrorCode::Io,
                format!("creating `{}`: {why}", path.display()),
            ));
        }
        Ok(())
    }

    fn image_usage(&self, path: &Path) -> Result<ImageUsage> {
        let md = fs::symlink_metadata(path).map_err(|e| io_error(path, &e))?;
        Ok(ImageUsage {
            apparent: md.len(),
            allocated: md.blocks() * 512,
        })
    }

    fn verify_immutable(&self, dir: &Path, image: &Path) -> Result<()> {
        let unprotected = |detail: String| {
            Err(Error::new(
                ErrorCode::Io,
                format!(
                    "{} can be removed or renamed: {} accepts changes ({detail})",
                    image.display(),
                    dir.display()
                ),
            ))
        };
        let image_c = CString::new(image.as_os_str().as_bytes()).map_err(|_| {
            Error::new(
                ErrorCode::InvalidPath,
                format!("`{}` contains a NUL byte", image.display()),
            )
        })?;
        // Removing, renaming and creating an entry all go through the same check on the
        // directory, which an immutable one fails with EPERM before it looks at any
        // entry. The probes are chosen so that a directory that is NOT protected loses
        // nothing: `rmdir` of a regular file is refused (ENOTDIR) where an `unlink` would
        // delete the image, a rename of the image onto itself is accepted by the kernel
        // before it checks anything (so it proves nothing), and a symlink that did get
        // created is removed again.
        // SAFETY: `image_c` is a valid C string.
        let removal = unsafe { libc::rmdir(image_c.as_ptr()) };
        match (removal != 0).then(io::Error::last_os_error) {
            Some(e) if errno_of(&e) == Some(libc::EPERM) => {}
            Some(e) => return unprotected(format!("removal was refused with `{e}`, not EPERM")),
            None => return unprotected("removal was accepted".to_owned()),
        }
        match creation_is_refused(dir) {
            Ok(true) => {}
            Ok(false) => return unprotected("creating an entry was accepted".to_owned()),
            Err(e) => {
                return unprotected(format!(
                    "creating an entry was refused with `{e}`, not EPERM"
                ));
            }
        }
        // The loop device opens the image for writing: that must keep working.
        OpenOptions::new()
            .write(true)
            .open(image)
            .map(drop)
            .map_err(|e| {
                Error::new(
                    ErrorCode::Io,
                    format!(
                        "{} is protected but cannot be opened for writing: {e}",
                        image.display()
                    ),
                )
            })
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).map_err(|e| io_error(path, &e))
    }

    fn remove_dir(&self, path: &Path) -> Result<()> {
        fs::remove_dir(path).map_err(|e| io_error(path, &e))
    }
}

/// Whether the kernel refuses (EPERM) to create an entry in `dir`: the probe is a
/// symlink, and one that did get created is removed again.
fn creation_is_refused(dir: &Path) -> io::Result<bool> {
    let probe = dir.join(format!(".parhel-probe-{}", std::process::id()));
    match std::os::unix::fs::symlink(".", &probe) {
        Err(e) if errno_of(&e) == Some(libc::EPERM) => Ok(true),
        Err(e) => Err(e),
        Ok(()) => {
            fs::remove_file(&probe)?;
            Ok(false)
        }
    }
}

/// Reserve `bytes` of disk for the whole file.
#[cfg(target_os = "linux")]
fn allocate(file: &File, bytes: u64) -> io::Result<()> {
    let length =
        libc::off_t::try_from(bytes).map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
    // SAFETY: the descriptor is open for writing.
    let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, length) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn allocate(_file: &File, _bytes: u64) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // U-SYS-01: the kernel's escapes in /proc/self/mountinfo.
    #[test]
    fn mountinfo_fields_are_unescaped() {
        let text = "36 35 98:0 /mnt1 /mnt/with\\040space rw,noatime master:1 - ext3 /dev/root rw\n\
                    37 35 0:3 / /proc rw - proc proc rw\n\
                    garbage\n";
        let points: Vec<PathBuf> = mount_points(text).collect();
        assert_eq!(
            points,
            vec![PathBuf::from("/mnt/with space"), PathBuf::from("/proc"),]
        );
        assert_eq!(unescape_octal("a\\134b\\012c\\9"), b"a\\b\nc\\9");
    }

    // U-SYS-02: only absolute PATH entries are searched.
    #[test]
    fn programs_are_looked_up_in_absolute_directories_only() {
        let dir = tmp();
        let exe = dir.path().join("tool-here");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve(exe.to_str().unwrap()), Some(exe.clone()));
        assert_eq!(resolve("definitely-not-a-program-parhel"), None);
        // A file that is not executable is not a tool.
        let plain = dir.path().join("plain");
        fs::write(&plain, "x").unwrap();
        assert_eq!(resolve(plain.to_str().unwrap()), None);
    }

    // U-SYS-03: `run` reports the program's own words when it fails.
    #[test]
    fn run_returns_stdout_and_reports_stderr_on_failure() {
        let host = SystemHost;
        assert_eq!(host.run("sh", &["-c", "echo hello"]).unwrap(), "hello\n");
        let err = host
            .run("sh", &["-c", "echo boom >&2; exit 3"])
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert!(err.message.contains("boom"), "{}", err.message);
        let missing = host
            .run("definitely-not-a-program-parhel", &[])
            .unwrap_err();
        assert_eq!(missing.code, ErrorCode::MissingTool);
    }

    // U-SYS-04: directories get the mode and owner asked for, and a link is not a
    // directory.
    #[test]
    fn create_dir_sets_the_mode_and_refuses_a_link() {
        let dir = tmp();
        let host = SystemHost;
        let made = dir.path().join("made");
        // SAFETY: `getuid`/`getgid` have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        host.create_dir(&made, 0o700, Some((uid, gid))).unwrap();
        host.create_dir(&made, 0o750, Some((uid, gid))).unwrap();
        assert_eq!(
            fs::metadata(&made).unwrap().permissions().mode() & 0o7777,
            0o750
        );

        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let link = dir.path().join("link");
        symlink(&elsewhere, &link).unwrap();
        let err = host.create_dir(&link, 0o700, Some((uid, gid))).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io, "{err}");
        assert_ne!(
            fs::metadata(&elsewhere).unwrap().permissions().mode() & 0o7777,
            0o700,
            "the link's target was changed"
        );
    }

    // U-SYS-05: a file is replaced whole, and a link at its place is replaced, not followed.
    #[test]
    fn write_file_replaces_atomically_and_never_follows_a_link() {
        let dir = tmp();
        let host = SystemHost;
        // SAFETY: `getuid`/`getgid` have no preconditions.
        let owner = Some(unsafe { (libc::getuid(), libc::getgid()) });
        let target = dir.path().join("victim");
        fs::write(&target, "precious").unwrap();
        let file = dir.path().join("unit");
        symlink(&target, &file).unwrap();

        host.write_file(&file, b"new", 0o644, owner).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "precious");
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert!(
            !fs::symlink_metadata(&file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // A leftover of an interrupted write does not get in the way.
        fs::write(dir.path().join(".unit.parhel-tmp"), "stale").unwrap();
        host.write_file(&file, b"newer", 0o600, owner).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "newer");
        assert!(!dir.path().join(".unit.parhel-tmp").exists());
    }

    // U-SYS-06: reading as root does not follow a planted link.
    #[test]
    fn read_text_refuses_a_link_and_reports_a_missing_file_as_none() {
        let dir = tmp();
        let host = SystemHost;
        let secret = dir.path().join("secret");
        fs::write(&secret, "root's business").unwrap();
        let marker = dir.path().join("marker");
        symlink(&secret, &marker).unwrap();
        let err = host.read_text(&marker).unwrap_err();
        assert!(err.message.contains("symbolic link"), "{}", err.message);
        assert!(!err.message.contains("business"));
        assert_eq!(host.read_text(&dir.path().join("none")).unwrap(), None);
        assert_eq!(
            host.read_text(&secret).unwrap().as_deref(),
            Some("root's business")
        );
    }

    // U-SYS-06b: root reads files a user can write (the marker, a unit): a FIFO planted
    // there must not hang it (open for reading blocks until a writer shows up), and a
    // device, a socket or a directory is not text either.
    #[test]
    fn read_text_refuses_what_is_not_a_regular_file_without_blocking() {
        let dir = tmp();
        let fifo = dir.path().join("marker");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        // A read that blocks is a thread that never returns: the test reports it instead
        // of hanging with it.
        std::thread::spawn(move || {
            let _ = tx.send(SystemHost.read_text(&path));
        });
        let answer = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("read_text blocked on a FIFO");
        let err = answer.unwrap_err();
        assert!(
            err.message.contains("not a regular file"),
            "{}",
            err.message
        );
        let err = SystemHost.read_text(dir.path()).unwrap_err();
        assert!(
            err.message.contains("not a regular file"),
            "{}",
            err.message
        );
    }

    // U-SYS-06c: ... nor may it read without end: a file a user made huge is refused
    // (the biggest thing parhel reads this way is a few hundred bytes).
    #[test]
    fn read_text_refuses_a_file_larger_than_the_cap() {
        let dir = tmp();
        let host = SystemHost;
        let at_cap = dir.path().join("at-cap");
        fs::write(&at_cap, vec![b'a'; MAX_TEXT_BYTES]).unwrap();
        assert_eq!(
            host.read_text(&at_cap).unwrap().map(|t| t.len()),
            Some(MAX_TEXT_BYTES)
        );
        let big = dir.path().join("big");
        fs::write(&big, vec![b'a'; MAX_TEXT_BYTES + 1]).unwrap();
        let err = host.read_text(&big).unwrap_err();
        assert!(err.message.contains("larger than"), "{}", err.message);
    }

    // U-SYS-07: listing does not look through a link.
    #[test]
    fn list_dir_does_not_follow_links_and_tolerates_absence() {
        let dir = tmp();
        let host = SystemHost;
        fs::create_dir(dir.path().join("real")).unwrap();
        fs::write(dir.path().join("real/a"), "").unwrap();
        symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        assert_eq!(
            host.list_dir(&dir.path().join("real")).unwrap(),
            vec![OsString::from("a")]
        );
        let none: Vec<OsString> = Vec::new();
        assert_eq!(host.list_dir(&dir.path().join("link")).unwrap(), none);
        assert_eq!(host.list_dir(&dir.path().join("none")).unwrap(), none);
        assert_eq!(host.list_dir(&dir.path().join("real/a")).unwrap(), none);
    }

    // U-SYS-08: sparse and preallocated images, measured.
    #[test]
    fn images_are_sparse_or_fully_allocated() {
        let dir = tmp();
        let host = SystemHost;
        let sparse = dir.path().join("sparse.img");
        host.create_image(&sparse, 64 << 20, false).unwrap();
        let usage = host.image_usage(&sparse).unwrap();
        assert_eq!(usage.apparent, 64 << 20);
        assert!(usage.allocated < 1 << 20, "{usage:?}");
        assert_eq!(
            fs::metadata(&sparse).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Never over an existing image.
        let err = host.create_image(&sparse, 1 << 20, false).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io, "{err}");
        assert_eq!(fs::metadata(&sparse).unwrap().len(), 64 << 20);
        // `fallocate` is Linux's: elsewhere preallocating says so and leaves nothing.
        let full = dir.path().join("full.img");
        match host.create_image(&full, 4 << 20, true) {
            Ok(()) => assert!(host.image_usage(&full).unwrap().allocated >= 4 << 20),
            Err(e) => {
                assert!(!full.exists(), "a failed image is removed");
                assert!(e.message.contains("creating"), "{}", e.message);
            }
        }
    }

    // U-SYS-09: the check of the protection never destroys the image it checks, even
    // when there is no protection at all.
    #[test]
    fn verifying_an_unprotected_directory_reports_it_and_keeps_the_image() {
        let dir = tmp();
        let host = SystemHost;
        let image = dir.path().join("a.img");
        fs::write(&image, "data").unwrap();
        let err = host.verify_immutable(dir.path(), &image).unwrap_err();
        assert!(err.message.contains("a.img"), "{}", err.message);
        assert_eq!(fs::read_to_string(&image).unwrap(), "data", "image intact");
        assert!(!host.is_immutable(dir.path()));
    }

    // U-SYS-09b: the creation probe says "accepted" for a plain directory and leaves
    // nothing behind.
    #[test]
    fn the_creation_probe_cleans_up_after_itself() {
        let dir = tmp();
        assert!(!creation_is_refused(dir.path()).unwrap());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        let err = creation_is_refused(&dir.path().join("missing")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn unknown_users_are_none_and_root_is_zero() {
        let host = SystemHost;
        assert_eq!(host.user("no-such-user-parhel").unwrap(), None);
        assert_eq!(host.user("bad\0name").unwrap(), None);
        let root = host.user("root").unwrap().expect("root exists");
        assert_eq!((root.uid, root.gid), (0, 0));
    }
}
