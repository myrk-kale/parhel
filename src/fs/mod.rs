//! Filesystem adapters.

#[cfg(target_os = "macos")]
pub mod apfs;
#[cfg(target_os = "linux")]
pub mod linux;
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
