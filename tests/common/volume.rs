//! Real filesystem boundaries.
//!
//! macOS: throwaway disk images (`hdiutil`, no root needed; serialize users with
//! `#[serial(hdiutil)]`). Linux: filesystems mounted before the run
//! (`scripts/linux-test.sh`, CI) and announced through `PARHEL_TEST_BTRFS`,
//! `PARHEL_TEST_XFS`, `PARHEL_TEST_EXT4` and `PARHEL_TEST_BTRFS_SMALL`: see [`test_mount`].

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
use tempfile::TempDir;

/// A filesystem the Linux suite is given a mount of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountedFs {
    Btrfs,
    Xfs,
    Ext4,
    /// A btrfs of a few hundred megabytes, to fill (the tests that need a full disk).
    SmallBtrfs,
}

impl MountedFs {
    /// Environment variable naming a directory on this filesystem.
    pub fn var(self) -> &'static str {
        match self {
            Self::Btrfs => "PARHEL_TEST_BTRFS",
            Self::Xfs => "PARHEL_TEST_XFS",
            Self::Ext4 => "PARHEL_TEST_EXT4",
            Self::SmallBtrfs => "PARHEL_TEST_BTRFS_SMALL",
        }
    }

    /// What `common::cow::fs_type` must answer for it.
    pub fn fs_type(self) -> &'static str {
        match self {
            Self::Btrfs | Self::SmallBtrfs => "btrfs",
            Self::Xfs => "xfs",
            Self::Ext4 => "ext4",
        }
    }
}

/// Whether a missing mount is an error: always in CI and with `PARHEL_REQUIRE_FS=1`,
/// so a misconfigured runner can never turn the filesystem tests into silent passes.
pub fn mounts_required() -> bool {
    std::env::var_os("CI").is_some()
        || std::env::var_os("PARHEL_REQUIRE_FS").is_some_and(|v| v == "1")
}

/// The decision behind [`test_mount`], on explicit inputs: `Ok(dir)` when `value`
/// names an existing directory, `Err(skip notice)` when it is missing and mounts are
/// not required. Panics when it is missing and required, or names something that is
/// not a directory.
pub fn resolve_mount(
    fs: MountedFs,
    value: Option<OsString>,
    required: bool,
) -> Result<PathBuf, String> {
    let var = fs.var();
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        assert!(
            !required,
            "{var} is not set but the {} tests are required (CI or PARHEL_REQUIRE_FS=1); \
             run scripts/linux-test.sh",
            fs.fs_type()
        );
        return Err(format!(
            "SKIPPED: {var} is not set (point it at a directory on {} to run this test)",
            fs.fs_type()
        ));
    };
    let dir = PathBuf::from(value);
    assert!(dir.is_dir(), "{var}={} is not a directory", dir.display());
    Ok(fs::canonicalize(dir).unwrap())
}

/// A directory on `fs` for a test that needs that filesystem, from the environment.
/// Returns `None` after printing a skip notice when the variable is unset and mounts
/// are not required; panics when it is required, or when the directory is on another
/// filesystem than announced (a wrong mount must not make tests lie).
///
/// ```ignore
/// let Some(dir) = test_mount(MountedFs::Btrfs) else { return };
/// ```
pub fn test_mount(fs: MountedFs) -> Option<PathBuf> {
    match resolve_mount(fs, std::env::var_os(fs.var()), mounts_required()) {
        Ok(dir) => {
            let actual = super::cow::fs_type(&dir);
            assert_eq!(
                actual,
                fs.fs_type(),
                "{}={} is on {actual}, not {}",
                fs.var(),
                dir.display(),
                fs.fs_type()
            );
            Some(dir)
        }
        Err(notice) => {
            eprintln!("{notice}");
            None
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
pub enum ImageFs {
    Apfs,
    ApfsCaseSensitive,
    Hfs,
}

#[cfg(target_os = "macos")]
impl ImageFs {
    fn hdiutil_name(self) -> &'static str {
        match self {
            Self::Apfs => "APFS",
            Self::ApfsCaseSensitive => "Case-sensitive APFS",
            Self::Hfs => "HFS+",
        }
    }
}

/// A mounted sparse image; detached (forcefully) on drop.
#[cfg(target_os = "macos")]
pub struct DiskImage {
    _dir: TempDir,
    /// Canonical mount point.
    pub mount: PathBuf,
}

#[cfg(target_os = "macos")]
impl DiskImage {
    pub fn new(fs_kind: ImageFs, size_mb: u32) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("parhel-img-")
            .tempdir()
            .unwrap();
        let image = dir.path().join("vol.sparseimage");
        let mount = dir.path().join("mnt");
        fs::create_dir(&mount).unwrap();
        run(Command::new("hdiutil")
            .args(["create", "-quiet", "-type", "SPARSE"])
            .args([
                "-size",
                &format!("{size_mb}m"),
                "-fs",
                fs_kind.hdiutil_name(),
                "-volname",
                "parhel-test",
                image.to_str().unwrap(),
            ]));
        run(Command::new("hdiutil").args([
            "attach",
            "-quiet",
            "-nobrowse",
            "-noautoopen",
            "-owners",
            "on",
            "-mountpoint",
            mount.to_str().unwrap(),
            image.to_str().unwrap(),
        ]));
        let mount = fs::canonicalize(&mount).unwrap();
        Self { _dir: dir, mount }
    }
}

#[cfg(target_os = "macos")]
impl Drop for DiskImage {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", "-quiet", "-force", self.mount.to_str().unwrap()])
            .status();
    }
}

#[cfg(target_os = "macos")]
fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
