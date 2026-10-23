//! Filesystem adapters.

/// Directory-descriptor access to the source tree, shared by the walk and `clonefileat`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod anchor;
#[cfg(target_os = "macos")]
pub mod apfs;
#[cfg(target_os = "linux")]
pub mod linux;
/// Clone or copy a directory tree, shared by the macOS and Linux adapters.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod tree;
/// Platform-independent Unix helpers shared by the macOS and Linux adapters.
#[cfg(unix)]
pub mod unix;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub mod unsupported;

#[cfg(target_os = "macos")]
pub use apfs::ApfsFs as PlatformFs;
#[cfg(target_os = "linux")]
pub use linux::LinuxFs as PlatformFs;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub use unsupported::UnsupportedFs as PlatformFs;

/// The adapters' tests clone and copy loose paths: `src` is `parent/name`.
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(crate) trait CowFsExt: crate::ports::CowFs {
    fn clone_entry(
        &self,
        src: &std::path::Path,
        dst: &std::path::Path,
    ) -> crate::domain::Result<()> {
        let (root, name) = split(src);
        self.clone_within(root, name, dst)
    }

    fn copy_entry(
        &self,
        src: &std::path::Path,
        dst: &std::path::Path,
    ) -> crate::domain::Result<()> {
        let (root, name) = split(src);
        self.copy_within(root, name, dst)
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
impl<T: crate::ports::CowFs> CowFsExt for T {}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn split(src: &std::path::Path) -> (&std::path::Path, &std::path::Path) {
    let root = src.parent().unwrap_or(std::path::Path::new("/"));
    (
        root,
        std::path::Path::new(src.file_name().expect("a named entry")),
    )
}
