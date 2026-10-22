//! Throwaway disk images (`hdiutil`) to test real filesystem boundaries.
//! Requires macOS; no root needed. Serialize users with `#[serial(hdiutil)]`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

#[derive(Debug, Clone, Copy)]
pub enum ImageFs {
    Apfs,
    ApfsCaseSensitive,
    Hfs,
}

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
pub struct DiskImage {
    _dir: TempDir,
    /// Canonical mount point.
    pub mount: PathBuf,
}

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

impl Drop for DiskImage {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", "-quiet", "-force", self.mount.to_str().unwrap()])
            .status();
    }
}

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
