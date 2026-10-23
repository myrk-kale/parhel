//! `/proc/self/mountinfo`: what is mounted where, and from what.
//!
//! Shared by `setup` (is the thing mounted at the store's place this store's image?) and
//! `doctor` (was the image behind the mounted store deleted?).

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// Where it is mounted.
    pub point: PathBuf,
    /// Filesystem type (`btrfs`, `tmpfs`, ...).
    pub fstype: String,
    /// What it is mounted from (`/dev/loop3`, `tmpfs`, ...).
    pub source: String,
}

/// The entries of a `mountinfo` file, in the order the kernel lists them (a mount made
/// later on the same place comes later). Lines that are not mountinfo are skipped.
#[must_use]
pub fn parse(mountinfo: &str) -> Vec<MountEntry> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let point = before.split(' ').nth(4)?;
            let mut after = after.split(' ');
            let fstype = after.next()?;
            let source = after.next()?;
            Some(MountEntry {
                point: PathBuf::from(OsString::from_vec(unescape(point))),
                fstype: fstype.to_owned(),
                source: unescape_text(source),
            })
        })
        .collect()
}

/// The mount that is exactly at `path` and, when more than one was made there, the one
/// on top.
#[must_use]
pub fn mount_at<'a>(entries: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
    entries.iter().rev().find(|entry| entry.point == path)
}

/// The deepest mount that holds `path` (the one `path` lives on), the last of equals.
#[must_use]
pub fn mount_holding<'a>(entries: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
    let mut best: Option<&MountEntry> = None;
    for entry in entries {
        if path.starts_with(&entry.point)
            && best.is_none_or(|b| entry.point.as_os_str().len() >= b.point.as_os_str().len())
        {
            best = Some(entry);
        }
    }
    best
}

fn unescape_text(field: &str) -> String {
    String::from_utf8_lossy(&unescape(field)).into_owned()
}

/// `\040` (space), `\011`, `\012` and `\134` as the kernel writes them: a backslash and
/// three octal digits stand for that byte.
#[must_use]
pub fn unescape(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let octal = bytes.get(at + 1..at + 4).filter(|digits| {
            bytes[at] == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        });
        if let Some(digits) = octal {
            let value = digits
                .iter()
                .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(value & 0xff).unwrap_or(b'?'));
            at += 4;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "36 35 98:0 /mnt1 /mnt/with\\040space rw,noatime master:1 - ext3 /dev/root rw\n\
                        37 35 0:3 / /proc rw - proc proc rw\n\
                        garbage\n\
                        40 36 0:40 / /parhel/alice rw - btrfs /dev/loop3 rw,ssd\n\
                        41 40 0:41 / /parhel/alice rw - tmpfs tmpfs rw\n";

    // U-SYS-01: the kernel's escapes in /proc/self/mountinfo, the fields we read.
    #[test]
    fn mountinfo_fields_are_unescaped_and_parsed() {
        let points: Vec<PathBuf> = parse(INFO).into_iter().map(|e| e.point).collect();
        assert_eq!(
            points,
            vec![
                PathBuf::from("/mnt/with space"),
                PathBuf::from("/proc"),
                PathBuf::from("/parhel/alice"),
                PathBuf::from("/parhel/alice"),
            ]
        );
        assert_eq!(unescape("a\\134b\\012c\\9"), b"a\\b\nc\\9");
        let entries = parse(INFO);
        assert_eq!(
            (entries[0].fstype.as_str(), entries[0].source.as_str()),
            ("ext3", "/dev/root")
        );
    }

    // U-SYS-01b: the mount on top wins when two were made on one place; the deepest
    // mount holds a path below it.
    #[test]
    fn lookups_find_the_topmost_and_the_deepest_mount() {
        let entries = parse(INFO);
        let top = mount_at(&entries, Path::new("/parhel/alice")).unwrap();
        assert_eq!(top.source, "tmpfs");
        assert!(mount_at(&entries, Path::new("/parhel")).is_none());
        let holding = mount_holding(&entries, Path::new("/parhel/alice/worktrees/x")).unwrap();
        assert_eq!(holding.source, "tmpfs", "the last of equals");
        let holding = mount_holding(&entries, Path::new("/mnt/with space/a")).unwrap();
        assert_eq!(holding.source, "/dev/root");
        assert!(mount_holding(&entries, Path::new("/elsewhere")).is_none());
    }
}
