//! Placeholder adapter for platforms without a copy-on-write backend yet.
//! Every volume reports an unknown filesystem type, so `add` refuses with
//! `not-apfs` (or uses `--fallback`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::domain::{Error, Result};
use crate::ports::{CowFs, EntryKind, FileLock, VolumeInfo};

#[derive(Debug, Clone, Copy, Default)]
pub struct UnsupportedFs;

impl CowFs for UnsupportedFs {
    fn volume(&self, _: &Path) -> Result<VolumeInfo> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn canonicalize(&self, _: &Path) -> Result<PathBuf> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn exists_no_follow(&self, _: &Path) -> Result<bool> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn is_dir(&self, _: &Path) -> Result<bool> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn is_dir_no_follow(&self, _: &Path) -> Result<bool> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn create_staging_dir(&self, _: &Path) -> Result<PathBuf> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn list_dir(&self, _: &Path) -> Result<Vec<OsString>> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn create_dir(&self, _: &Path) -> Result<()> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn clone_entry(&self, _: &Path, _: &Path) -> Result<()> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn rename_exclusive(&self, _: &Path, _: &Path) -> Result<()> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn remove_dir_all(&self, _: &Path) -> Result<()> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn write_file(&self, _: &Path, _: &[u8]) -> Result<()> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn read_file(&self, _: &Path) -> Result<Vec<u8>> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn lock_exclusive(&self, _: &Path) -> Result<FileLock> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn is_locked(&self, _: &Path) -> Result<bool> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn entry_kind(&self, _: &Path) -> Result<EntryKind> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn replace_contents(&self, _: &Path, _: &[u8]) -> Result<()> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn replace_symlink(&self, _: &Path, _: &Path) -> Result<()> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
    fn remove_file(&self, _: &Path) -> Result<()> {
        Err(Error::not_implemented("copy-on-write on this platform"))
    }
}
