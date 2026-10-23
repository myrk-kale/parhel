//! The machine, for `git parhel setup`: privileged system calls and programs.

use std::ffi::OsString;
use std::path::Path;

use crate::domain::{Error, Result};
use crate::ports::{Host, ImageUsage, Owner, UserInfo};

/// The real [`Host`].
///
/// TDD stub: the platform queries are real (they are one line each) and everything
/// that touches the system answers `not-implemented` until `setup` is implemented.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemHost;

fn stub<T>(what: &str) -> Result<T> {
    Err(Error::not_implemented(what))
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
    fn user(&self, _name: &str) -> Result<Option<UserInfo>> {
        stub("looking up users")
    }
    fn tool_exists(&self, _tool: &str) -> bool {
        false
    }
    fn systemd_running(&self) -> bool {
        false
    }
    fn read_text(&self, _path: &Path) -> Result<Option<String>> {
        stub("reading files as root")
    }
    fn exists(&self, _path: &Path) -> bool {
        false
    }
    fn list_dir(&self, _path: &Path) -> Result<Vec<OsString>> {
        stub("listing directories as root")
    }
    fn is_mount_point(&self, _path: &Path) -> bool {
        false
    }
    fn run(&self, _program: &str, _args: &[&str]) -> Result<String> {
        stub("running programs")
    }
    fn create_dir(&self, _path: &Path, _mode: u32, _owner: Owner) -> Result<()> {
        stub("creating directories as root")
    }
    fn write_file(&self, _path: &Path, _contents: &[u8], _mode: u32, _owner: Owner) -> Result<()> {
        stub("writing files as root")
    }
    fn create_image(&self, _path: &Path, _bytes: u64, _preallocate: bool) -> Result<()> {
        stub("creating the image")
    }
    fn image_usage(&self, _path: &Path) -> Result<ImageUsage> {
        stub("measuring the image")
    }
    fn verify_immutable(&self, _dir: &Path, _image: &Path) -> Result<()> {
        stub("verifying the immutable directory")
    }
    fn remove_file(&self, _path: &Path) -> Result<()> {
        stub("removing files as root")
    }
    fn remove_dir(&self, _path: &Path) -> Result<()> {
        stub("removing directories as root")
    }
}
