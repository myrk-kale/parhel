//! Extended attributes through libc (no `xattr`/`setfattr` binaries needed).

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// An attribute name every filesystem under test accepts for regular users.
#[cfg(target_os = "macos")]
pub const TEST_ATTR: &str = "com.parhel.test";
#[cfg(target_os = "linux")]
pub const TEST_ATTR: &str = "user.parhel.test";

fn c(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
}

/// Set `name` on `path` itself (a symlink is not followed).
pub fn set(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let (p, n) = (c(path), CString::new(name).unwrap());
    // SAFETY: valid C strings; `value` is readable for its length.
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::lsetxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::setxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Value of `name` on `path` itself, `None` when absent.
pub fn get(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let (p, n) = (c(path), CString::new(name).unwrap());
    let mut buf = vec![0u8; 4096];
    // SAFETY: valid C strings; `buf` is writable for its length.
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::lgetxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::getxattr(
            p.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if rc == -1 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            #[cfg(target_os = "linux")]
            Some(libc::ENODATA) => Ok(None),
            #[cfg(target_os = "macos")]
            Some(libc::ENOATTR) => Ok(None),
            _ => Err(e),
        };
    }
    buf.truncate(usize::try_from(rc).unwrap());
    Ok(Some(buf))
}

/// Every `user.*` attribute of `path` itself with its value.
#[cfg(target_os = "linux")]
pub fn user_attrs(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let p = c(path);
    let mut names = vec![0u8; 64 * 1024];
    // SAFETY: valid C string; `names` is writable for its length.
    let len = unsafe { libc::llistxattr(p.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    assert!(
        len >= 0,
        "llistxattr({}): {}",
        path.display(),
        io::Error::last_os_error()
    );
    names.truncate(usize::try_from(len).unwrap());
    names
        .split(|&b| b == 0)
        .filter(|n| n.starts_with(b"user."))
        .map(|n| {
            let name = String::from_utf8(n.to_vec()).unwrap();
            let value = get(path, &name).unwrap().unwrap_or_default();
            (name, value)
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub fn user_attrs(_: &Path) -> BTreeMap<String, Vec<u8>> {
    unimplemented!("only used by the Linux fidelity tests")
}
