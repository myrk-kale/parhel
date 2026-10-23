// Evidence of shared storage on Linux through `ioctl(FS_IOC_FIEMAP)`.
//
// btrfs and XFS flag extents referenced by more than one file with
// `FIEMAP_EXTENT_SHARED`, so "two files start at the same physical offset *and* the
// extent is flagged shared" is the Linux counterpart of macOS's `F_LOG2PHYS_EXT` proof.
//
// This file is included by `tests/common/cow.rs` and, with `include!`, by the unit
// tests of `src/fs/linux.rs`: it must stay self-contained and use no inner attributes.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// Flush delayed allocations first, so every extent has a physical address.
const FIEMAP_FLAG_SYNC: u32 = 1;
/// The extent is referenced by more than one file (reflink / snapshot).
pub const FIEMAP_EXTENT_SHARED: u32 = 0x0000_2000;
/// `_IOWR('f', 11, struct fiemap)`.
const FS_IOC_FIEMAP: u64 = 0xC020_660B;
const MAX_EXTENTS: usize = 256;

/// `struct fiemap_extent`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Extent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    reserved64: [u64; 2],
    pub flags: u32,
    reserved: [u32; 3],
}

/// `struct fiemap` followed by room for `MAX_EXTENTS` extents.
#[repr(C)]
struct Request {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extents: [Extent; MAX_EXTENTS],
}

/// The first (up to 256) extents of `path`, after flushing its dirty data.
pub fn extents(path: &Path) -> io::Result<Vec<Extent>> {
    let file = File::open(path)?;
    let mut request = Box::new(Request {
        start: 0,
        length: u64::MAX,
        flags: FIEMAP_FLAG_SYNC,
        mapped_extents: 0,
        extent_count: u32::try_from(MAX_EXTENTS).unwrap(),
        reserved: 0,
        extents: [Extent::default(); MAX_EXTENTS],
    });
    // SAFETY: valid fd and a `fiemap` request with room for `extent_count` extents.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &mut *request) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    let mapped = request.mapped_extents as usize;
    Ok(request.extents[..mapped.min(MAX_EXTENTS)].to_vec())
}

/// The extent holding the first byte of data, `None` for an empty or inline file.
pub fn first_extent(path: &Path) -> io::Result<Option<Extent>> {
    Ok(extents(path)?.into_iter().next())
}

/// Both files start at the same physical block and the clone's extent is flagged
/// shared: they are reflinks of each other.
pub fn share_first_extent(original: &Path, clone: &Path) -> bool {
    match (first_extent(original), first_extent(clone)) {
        (Ok(Some(a)), Ok(Some(b))) => {
            a.physical == b.physical && a.physical != 0 && b.flags & FIEMAP_EXTENT_SHARED != 0
        }
        _ => false,
    }
}

/// `copy` owns its data: its first extent is on different blocks than the original's
/// and not flagged shared. (The original may well be shared with other files.) Panics
/// if a file has no extent, so an inline or empty file cannot pass for "not shared".
pub fn are_independent_copies(original: &Path, copy: &Path) -> bool {
    let first = |p: &Path| {
        first_extent(p)
            .unwrap_or_else(|e| panic!("FIEMAP {}: {e}", p.display()))
            .unwrap_or_else(|| panic!("{} has no extent (empty or inline)", p.display()))
    };
    let (a, b) = (first(original), first(copy));
    a.physical != b.physical && b.flags & FIEMAP_EXTENT_SHARED == 0
}
