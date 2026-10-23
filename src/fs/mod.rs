//! Filesystem adapters.

#[cfg(target_os = "macos")]
pub mod apfs;
/// Platform-independent Unix helpers shared by the macOS and (future) Linux adapters.
#[cfg(unix)]
pub mod unix;
#[cfg(not(target_os = "macos"))]
pub mod unsupported;

#[cfg(target_os = "macos")]
pub use apfs::ApfsFs as PlatformFs;
#[cfg(not(target_os = "macos"))]
pub use unsupported::UnsupportedFs as PlatformFs;
