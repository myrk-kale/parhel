//! Which programs root may run.
//!
//! `setup` runs as root, often through a `sudo` whose configuration keeps the caller's
//! `PATH`. A `PATH` that names a directory the user can write to would make root run
//! whatever the user left there as `mkfs.btrfs` or `systemctl`. So root does not consult
//! `PATH` at all: it looks in [`TRUSTED_DIRS`] and runs a program only if nobody but root
//! could have put it there.
//!
//! The decision is a pure function of facts about paths ([`Facts`]), and the walk that
//! collects them goes through [`Disk`], so the rules are tested with made-up owners and
//! modes (a test cannot `chown` to root) as well as against the real disk.

use std::io;
use std::path::{Path, PathBuf};

/// The directories root looks for programs in, in this order. `PATH` is not consulted.
pub const TRUSTED_DIRS: [&str; 4] = ["/usr/sbin", "/usr/bin", "/sbin", "/bin"];

/// The `PATH` handed to what root runs: the same directories, so that the helpers a tool
/// starts by name (`mount.btrfs`, `systemd-...`) come from them too.
pub const TRUSTED_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Symbolic links followed to reach the program (the kernel allows 40).
const MAX_LINKS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// What the check needs to know about a path (from `lstat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub kind: Kind,
    pub uid: u32,
    /// Permission bits (`st_mode & 0o7777`).
    pub mode: u32,
}

/// The disk, as far as the check looks at it.
pub trait Disk {
    /// `lstat`: never follows a link at the last component.
    fn lstat(&self, path: &Path) -> io::Result<Facts>;
    /// The path with every link resolved.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;
}

/// What is trusted.
#[derive(Debug, Clone, Copy)]
pub struct Policy<'a> {
    /// The only user that may own a program, a link to it and the directories on the way:
    /// root (0).
    pub owner: u32,
    /// Directories above this one are not examined (`/` examines everything). Tests use a
    /// temporary directory, whose parents (`/tmp`) are not theirs to judge.
    pub anchor: &'a Path,
}

/// What looking for a program came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Where to run it from (the path as spelled, so a program that behaves according to
    /// the name it is run by - `modprobe` is a link to `kmod` - still does).
    Found(PathBuf),
    /// Nothing by that name is executable in any of the directories.
    Missing,
    /// Something is, but it cannot be trusted: which file failed which check.
    Refused(String),
}

/// The answer for one candidate path.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Trusted(PathBuf),
    NotThere,
    Refused(String),
}

fn owner_name(uid: u32) -> String {
    if uid == 0 {
        "root".to_owned()
    } else {
        format!("uid {uid}")
    }
}

/// Why `path`, a directory with `facts`, cannot be trusted to hold only what its owner put
/// there.
fn directory_problem(path: &Path, facts: &Facts, policy: &Policy<'_>) -> Option<String> {
    let what = format!("the directory `{}`", path.display());
    if facts.kind != Kind::Dir {
        return Some(format!("{what} is not a directory"));
    }
    ownership_problem(&what, facts, policy)
}

/// Why the program `path` (a regular file with `facts`) cannot be trusted.
fn program_problem(path: &Path, facts: &Facts, policy: &Policy<'_>) -> Option<String> {
    let what = format!("`{}`", path.display());
    ownership_problem(&what, facts, policy)
}

/// Owned by the trusted user and writable by no one else.
fn ownership_problem(what: &str, facts: &Facts, policy: &Policy<'_>) -> Option<String> {
    if facts.uid != policy.owner {
        return Some(format!(
            "{what} is owned by {}, not by {}",
            owner_name(facts.uid),
            owner_name(policy.owner)
        ));
    }
    let writers = match (facts.mode & 0o020 != 0, facts.mode & 0o002 != 0) {
        (false, false) => return None,
        (true, false) => "its group",
        (false, true) => "everyone",
        (true, true) => "its group and everyone",
    };
    Some(format!(
        "{what} can be written by {writers} (mode {:04o})",
        facts.mode & 0o7777
    ))
}

/// Every directory from the anchor down to `dir` (which has no links in it) must be
/// trusted: whoever can write to one of them can replace what is below.
fn check_directories(disk: &dyn Disk, policy: &Policy<'_>, dir: &Path) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    for component in dir.components() {
        prefix.push(component);
        if !prefix.starts_with(policy.anchor) {
            continue;
        }
        let facts = disk
            .lstat(&prefix)
            .map_err(|e| format!("the directory `{}`: {e}", prefix.display()))?;
        if let Some(why) = directory_problem(&prefix, &facts, policy) {
            return Err(why);
        }
    }
    Ok(())
}

/// STAGE 1 (red): what the lookup did before the fix - any executable regular file will
/// do, whoever owns it and whoever can write to it.
fn check(disk: &dyn Disk, _policy: &Policy<'_>, candidate: &Path) -> Verdict {
    match disk.lstat(candidate) {
        Ok(facts) if facts.kind == Kind::File && facts.mode & 0o111 != 0 => {
            Verdict::Trusted(candidate.to_path_buf())
        }
        _ => Verdict::NotThere,
    }
}

fn is_absent(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR))
}

/// Look for `program` in `dirs`, in order. A candidate that cannot be trusted is passed
/// over for a later one that can; if no directory has one that can, and some had one that
/// could not, that is the answer ([`Lookup::Refused`]) rather than "not installed".
pub fn find(disk: &dyn Disk, policy: &Policy<'_>, dirs: &[PathBuf], program: &str) -> Lookup {
    let mut refusal = None;
    for dir in dirs {
        let candidate = dir.join(program);
        match check(disk, policy, &candidate) {
            Verdict::Trusted(_) => return Lookup::Found(candidate),
            Verdict::NotThere => {}
            Verdict::Refused(why) => {
                refusal.get_or_insert(why);
            }
        }
    }
    refusal.map_or(Lookup::Missing, Lookup::Refused)
}

/// Whether `path` itself (a program given with its directory) can be trusted.
pub fn find_path(disk: &dyn Disk, policy: &Policy<'_>, path: &Path) -> Lookup {
    match check(disk, policy, path) {
        Verdict::Trusted(_) => Lookup::Found(path.to_path_buf()),
        Verdict::NotThere => Lookup::Missing,
        Verdict::Refused(why) => Lookup::Refused(why),
    }
}

/// The real disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealDisk;

impl Disk for RealDisk {
    fn lstat(&self, path: &Path) -> io::Result<Facts> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let md = std::fs::symlink_metadata(path)?;
        let kind = match md.file_type() {
            t if t.is_symlink() => Kind::Symlink,
            t if t.is_dir() => Kind::Dir,
            t if t.is_file() => Kind::File,
            _ => Kind::Other,
        };
        Ok(Facts {
            kind,
            uid: md.uid(),
            mode: md.permissions().mode() & 0o7777,
        })
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Component;

    const ROOT: u32 = 0;
    const USER: u32 = 1000;

    fn policy() -> Policy<'static> {
        Policy {
            owner: ROOT,
            anchor: Path::new("/"),
        }
    }

    fn dirs() -> Vec<PathBuf> {
        TRUSTED_DIRS.iter().map(PathBuf::from).collect()
    }

    /// A disk made of facts, so owners and modes can be anything (a test cannot `chown`
    /// to root). Links are listed apart: `canonicalize` resolves the ones in directory
    /// position.
    #[derive(Default)]
    struct Made {
        facts: RefCell<HashMap<PathBuf, Facts>>,
        links: RefCell<HashMap<PathBuf, PathBuf>>,
    }

    impl Made {
        /// The usual layout of a distribution: `/`, `/usr`, `/usr/bin`, `/usr/sbin`, all
        /// root's and 0755, and `/bin`, `/sbin` links to them.
        fn distribution() -> Self {
            let made = Self::default();
            for dir in ["/", "/usr", "/usr/bin", "/usr/sbin"] {
                made.dir(dir, ROOT, 0o755);
            }
            made.link("/bin", "usr/bin", ROOT);
            made.link("/sbin", "usr/sbin", ROOT);
            made
        }

        fn dir(&self, path: &str, uid: u32, mode: u32) {
            self.set(path, Kind::Dir, uid, mode);
        }

        fn file(&self, path: &str, uid: u32, mode: u32) {
            self.set(path, Kind::File, uid, mode);
        }

        fn set(&self, path: &str, kind: Kind, uid: u32, mode: u32) {
            self.facts
                .borrow_mut()
                .insert(PathBuf::from(path), Facts { kind, uid, mode });
        }

        fn link(&self, path: &str, target: &str, uid: u32) {
            self.set(path, Kind::Symlink, uid, 0o777);
            self.links
                .borrow_mut()
                .insert(PathBuf::from(path), PathBuf::from(target));
        }
    }

    impl Disk for Made {
        fn lstat(&self, path: &Path) -> io::Result<Facts> {
            self.facts
                .borrow()
                .get(path)
                .copied()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            let mut real = PathBuf::from("/");
            for component in path.components().skip(1) {
                match component {
                    Component::ParentDir => {
                        real.pop();
                        continue;
                    }
                    Component::CurDir => continue,
                    _ => real.push(component),
                }
                let mut hops = 0;
                while let Some(target) = self.links.borrow().get(&real).cloned() {
                    hops += 1;
                    if hops > MAX_LINKS {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    let parent = real.parent().unwrap_or(Path::new("/")).to_path_buf();
                    real = if target.is_absolute() {
                        target
                    } else {
                        parent.join(target)
                    };
                }
                if !self.facts.borrow().contains_key(&real) {
                    return Err(io::Error::from_raw_os_error(libc::ENOENT));
                }
            }
            Ok(real)
        }

        fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
            self.links
                .borrow()
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))
        }
    }

    fn found(lookup: &Lookup) -> &Path {
        match lookup {
            Lookup::Found(path) => path,
            other => panic!("expected a program, got {other:?}"),
        }
    }

    fn refused(lookup: Lookup) -> String {
        match lookup {
            Lookup::Refused(why) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // U-SYS-20: a program in a trusted directory, owned by root and writable by no one
    // else, with root's directories above it, is found - also through the links a
    // distribution has (`/bin`, `/sbin`) - in the order of the list.
    #[test]
    fn a_program_that_only_root_could_have_put_there_is_found() {
        let made = Made::distribution();
        made.file("/usr/sbin/mkfs.btrfs", ROOT, 0o755);
        made.file("/usr/bin/mount", ROOT, 0o755);
        made.file("/usr/sbin/mount", ROOT, 0o755);
        let sbin = find(&made, &policy(), &dirs(), "mkfs.btrfs");
        assert_eq!(found(&sbin), Path::new("/usr/sbin/mkfs.btrfs"));
        // The first directory of the list wins.
        let mount = find(&made, &policy(), &dirs(), "mount");
        assert_eq!(found(&mount), Path::new("/usr/sbin/mount"));
        // Through the link: `/sbin/x` is `/usr/sbin/x`.
        let via = find(&made, &policy(), &[PathBuf::from("/sbin")], "mkfs.btrfs");
        assert_eq!(found(&via), Path::new("/sbin/mkfs.btrfs"));
        assert_eq!(
            find(&made, &policy(), &dirs(), "not-installed"),
            Lookup::Missing
        );
    }

    // U-SYS-21: a program a user owns is not run as root, and the refusal names the file
    // and the check.
    #[test]
    fn a_program_owned_by_somebody_else_is_refused_naming_the_file() {
        let made = Made::distribution();
        made.file("/usr/bin/mkfs.btrfs", USER, 0o755);
        let why = refused(find(&made, &policy(), &dirs(), "mkfs.btrfs"));
        assert!(why.contains("/usr/bin/mkfs.btrfs"), "{why}");
        assert!(why.contains("owned by uid 1000, not by root"), "{why}");
    }

    // U-SYS-22: ... and so is one that root owns but anybody could replace: writable by its
    // group, by everyone, or both.
    #[rstest]
    #[case::group(0o775, "its group")]
    #[case::everyone(0o757, "everyone")]
    #[case::both(0o777, "its group and everyone")]
    fn a_program_others_can_write_to_is_refused(#[case] mode: u32, #[case] who: &str) {
        let made = Made::distribution();
        made.file("/usr/bin/mkfs.btrfs", ROOT, mode);
        let why = refused(find(&made, &policy(), &dirs(), "mkfs.btrfs"));
        assert!(why.contains("/usr/bin/mkfs.btrfs"), "{why}");
        assert!(why.contains(&format!("written by {who}")), "{why}");
        assert!(why.contains(&format!("mode {mode:04o}")), "{why}");
    }

    // U-SYS-23: the directory that holds the program, and every one above it, must be
    // root's and closed: whoever can write there can swap the program for theirs. The
    // refusal names the directory, not the program.
    #[rstest]
    #[case::holder_writable_by_group("/usr/bin", ROOT, 0o775, "can be written by its group")]
    #[case::holder_writable_by_everyone("/usr/bin", ROOT, 0o757, "can be written by everyone")]
    #[case::sticky_does_not_help("/usr/bin", ROOT, 0o1777, "can be written by")]
    #[case::holder_owned_by_a_user("/usr/bin", USER, 0o755, "owned by uid 1000")]
    #[case::parent_writable("/usr", ROOT, 0o777, "can be written by")]
    #[case::root_of_the_tree_owned_by_a_user("/", USER, 0o755, "owned by uid 1000")]
    fn a_directory_on_the_way_that_others_control_makes_the_program_untrustworthy(
        #[case] dir: &str,
        #[case] uid: u32,
        #[case] mode: u32,
        #[case] said: &str,
    ) {
        let made = Made::distribution();
        made.file("/usr/bin/mkfs.btrfs", ROOT, 0o755);
        made.dir(dir, uid, mode);
        let why = refused(find(&made, &policy(), &dirs(), "mkfs.btrfs"));
        assert!(
            why.contains(&format!("the directory `{dir}`")),
            "names the directory: {why}"
        );
        assert!(why.contains(said), "{why}");
    }

    // U-SYS-24: a link is followed to the program it leads to, and every directory on the
    // way there is judged, wherever it leads: a link to a directory the user controls is
    // refused naming that directory.
    #[test]
    fn links_are_followed_and_what_they_lead_through_is_judged() {
        let made = Made::distribution();
        // `/usr/sbin/mkfs.btrfs -> ../lib/btrfs/mkfs` (relative), root's all the way.
        made.dir("/usr/lib", ROOT, 0o755);
        made.dir("/usr/lib/btrfs", ROOT, 0o755);
        made.file("/usr/lib/btrfs/mkfs", ROOT, 0o755);
        made.link("/usr/sbin/mkfs.btrfs", "../lib/btrfs/mkfs", ROOT);
        let ok = find(&made, &policy(), &dirs(), "mkfs.btrfs");
        assert_eq!(found(&ok), Path::new("/usr/sbin/mkfs.btrfs"));

        // The same link, but the directory it leads to belongs to a user.
        made.dir("/usr/lib/btrfs", USER, 0o755);
        let why = refused(find(&made, &policy(), &dirs(), "mkfs.btrfs"));
        assert!(why.contains("/usr/lib/btrfs"), "{why}");

        // An absolute link out of the system, to the user's own directory.
        let made = Made::distribution();
        made.dir("/home", ROOT, 0o755);
        made.dir("/home/me", USER, 0o755);
        made.file("/home/me/mkfs", USER, 0o755);
        made.link("/usr/sbin/mkfs.btrfs", "/home/me/mkfs", ROOT);
        let why = refused(find(&made, &policy(), &dirs(), "mkfs.btrfs"));
        assert!(why.contains("/home/me"), "{why}");
    }

    // U-SYS-24b: ... a link in the middle of a chain counts too (`alternatives`): its
    // directory and its owner are judged, not only the last hop's.
    #[test]
    fn every_link_of_a_chain_is_judged() {
        let made = Made::distribution();
        made.dir("/etc", ROOT, 0o755);
        made.dir("/etc/alternatives", ROOT, 0o755);
        made.dir("/opt", ROOT, 0o755);
        made.file("/opt/real", ROOT, 0o755);
        made.link("/usr/bin/editor", "/etc/alternatives/editor", ROOT);
        made.link("/etc/alternatives/editor", "/opt/real", ROOT);
        let ok = find(&made, &policy(), &dirs(), "editor");
        assert_eq!(found(&ok), Path::new("/usr/bin/editor"));

        made.dir("/etc/alternatives", ROOT, 0o777);
        let why = refused(find(&made, &policy(), &dirs(), "editor"));
        assert!(why.contains("/etc/alternatives"), "{why}");

        made.dir("/etc/alternatives", ROOT, 0o755);
        made.link("/etc/alternatives/editor", "/opt/real", USER);
        let why = refused(find(&made, &policy(), &dirs(), "editor"));
        assert!(why.contains("/etc/alternatives/editor"), "{why}");
        assert!(why.contains("owned by uid 1000"), "{why}");
    }

    // U-SYS-25: links that lead nowhere or in circles are not programs, and do not hang.
    #[test]
    fn dangling_and_circular_links_are_not_programs() {
        let made = Made::distribution();
        made.link("/usr/bin/dangling", "/nowhere/x", ROOT);
        assert_eq!(find(&made, &policy(), &dirs(), "dangling"), Lookup::Missing);
        made.link("/usr/bin/a", "b", ROOT);
        made.link("/usr/bin/b", "a", ROOT);
        let why = refused(find(&made, &policy(), &dirs(), "a"));
        assert!(why.contains("too many symbolic links"), "{why}");
    }

    // U-SYS-26: a candidate that cannot be trusted does not hide a trusted one further
    // down the list; when none can be trusted the refusal wins over "not installed".
    #[test]
    fn a_trusted_program_later_in_the_list_is_used_over_a_refused_one() {
        let made = Made::distribution();
        made.file("/usr/sbin/blkid", USER, 0o755);
        made.file("/usr/bin/blkid", ROOT, 0o755);
        let ok = find(&made, &policy(), &dirs(), "blkid");
        assert_eq!(found(&ok), Path::new("/usr/bin/blkid"));

        made.file("/usr/bin/blkid", ROOT, 0o777);
        let why = refused(find(&made, &policy(), &dirs(), "blkid"));
        assert!(why.contains("/usr/sbin/blkid"), "the first refusal: {why}");
    }

    // U-SYS-27: what cannot be run is not the program: a file without an execute bit, a
    // directory of that name.
    #[test]
    fn a_file_that_cannot_be_run_is_not_the_program() {
        let made = Made::distribution();
        made.file("/usr/bin/data", ROOT, 0o644);
        made.dir("/usr/bin/folder", ROOT, 0o755);
        assert_eq!(find(&made, &policy(), &dirs(), "data"), Lookup::Missing);
        assert_eq!(find(&made, &policy(), &dirs(), "folder"), Lookup::Missing);
    }

    // U-SYS-28: the same rules against the real disk. A test cannot be root, so the
    // trusted owner is the user running it and the anchor is the temporary directory (its
    // parents, `/tmp`, are the system's).
    #[test]
    fn the_rules_hold_on_a_real_disk() {
        // SAFETY: `geteuid` has no preconditions.
        let me = unsafe { libc::geteuid() };
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let mode = |path: &Path, bits: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
        };
        mode(&bin, 0o755);
        let tool = bin.join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        mode(&tool, 0o755);
        let policy = Policy {
            owner: me,
            anchor: &root,
        };
        let search = [bin.clone()];

        assert_eq!(
            find(&RealDisk, &policy, &search, "tool"),
            Lookup::Found(tool.clone())
        );
        assert_eq!(find(&RealDisk, &policy, &search, "other"), Lookup::Missing);

        // Somebody else owns it: the same check with another trusted owner. The first
        // directory it looks at is the anchor.
        let stranger = Policy {
            owner: me.wrapping_add(1),
            ..policy
        };
        let why = refused(find(&RealDisk, &stranger, &search, "tool"));
        assert!(why.contains(root.to_str().unwrap()), "{why}");
        assert!(why.contains(&format!("owned by uid {me}")), "{why}");

        // The program or its directory open to others.
        mode(&tool, 0o757);
        let why = refused(find(&RealDisk, &policy, &search, "tool"));
        assert!(why.contains(tool.to_str().unwrap()), "{why}");
        mode(&tool, 0o755);
        mode(&bin, 0o775);
        let why = refused(find(&RealDisk, &policy, &search, "tool"));
        assert!(why.contains(bin.to_str().unwrap()), "{why}");
        mode(&bin, 0o755);

        // A link out of the tree, to a directory anyone can write to.
        let open = root.join("open");
        std::fs::create_dir(&open).unwrap();
        mode(&open, 0o777);
        std::fs::write(open.join("tool2"), "#!/bin/sh\n").unwrap();
        mode(&open.join("tool2"), 0o755);
        symlink(open.join("tool2"), bin.join("tool2")).unwrap();
        let why = refused(find(&RealDisk, &policy, &search, "tool2"));
        assert!(why.contains(open.to_str().unwrap()), "{why}");
    }

    // U-SYS-31: the layouts real distributions have pass the check unchanged. Against the
    // machine the tests run on (the trusted owner is root, so this needs no privilege):
    // the shell is found in one of the trusted directories, and a directory a user could
    // write to is not trusted whatever it holds.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_distributions_own_programs_are_trusted() {
        let policy = policy();
        let sh = find(&RealDisk, &policy, &dirs(), "sh");
        let sh = found(&sh);
        assert!(
            TRUSTED_DIRS.iter().any(|d| sh.starts_with(d)),
            "{}",
            sh.display()
        );
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("sh");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !matches!(find_path(&RealDisk, &policy, &tool), Lookup::Found(_)),
            "a program under {} was trusted",
            tmp.path().display()
        );
    }
}
