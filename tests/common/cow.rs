//! Evidence that two files share physical storage (copy-on-write) on APFS.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Device offset of the first byte of `path` (`fcntl(F_LOG2PHYS_EXT)`).
pub fn physical_offset(path: &Path) -> io::Result<i64> {
    let file = File::open(path)?;
    let mut l2p = libc::log2phys {
        l2p_flags: 0,
        l2p_contigbytes: 1,
        l2p_devoffset: 0,
    };
    // SAFETY: valid fd and a properly initialized `log2phys` the kernel writes into.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_LOG2PHYS_EXT, &mut l2p) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(l2p.l2p_devoffset)
}

/// Whether both files start at the same physical block (i.e. they are clones).
pub fn shares_storage(a: &Path, b: &Path) -> bool {
    physical_offset(a).unwrap() == physical_offset(b).unwrap()
}

fn statfs(path: &Path) -> libc::statfs {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut st = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: valid C string and out-pointer.
    let rc = unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) };
    assert_eq!(
        rc,
        0,
        "statfs({}): {}",
        path.display(),
        io::Error::last_os_error()
    );
    // SAFETY: statfs succeeded.
    unsafe { st.assume_init() }
}

/// `f_fstypename` of the volume holding `path` (`"apfs"`, `"hfs"`, ...).
pub fn fs_type(path: &Path) -> String {
    let st = statfs(path);
    let bytes: Vec<u8> = st
        .f_fstypename
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c.to_ne_bytes()[0])
        .collect();
    String::from_utf8(bytes).unwrap()
}

pub fn free_bytes(path: &Path) -> u64 {
    let st = statfs(path);
    st.f_bavail * u64::from(st.f_bsize)
}

/// Write `size` bytes of incompressible pseudo-random data and fsync (forces
/// block allocation so `F_LOG2PHYS_EXT` has something to report).
pub fn write_blob(path: &Path, size: usize, seed: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut x = seed | 1;
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf.truncate(size);
    let mut f = File::create(path).unwrap();
    f.write_all(&buf).unwrap();
    f.sync_all().unwrap();
}

/// Byte-for-byte copy that never clones (unlike `std::fs::copy` on macOS).
pub fn deep_copy(src: &Path, dst: &Path) {
    let data = fs::read(src).unwrap();
    let mut f = File::create(dst).unwrap();
    f.write_all(&data).unwrap();
    f.sync_all().unwrap();
}
