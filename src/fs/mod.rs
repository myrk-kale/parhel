//! Filesystem adapters.

#[cfg(target_os = "macos")]
pub mod apfs;
#[cfg(not(target_os = "macos"))]
pub mod unsupported;

#[cfg(target_os = "macos")]
pub use apfs::ApfsFs as PlatformFs;
#[cfg(not(target_os = "macos"))]
pub use unsupported::UnsupportedFs as PlatformFs;
