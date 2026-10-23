//! I-SET01..I-SET15: `git parhel setup`, the store at its real place.
//!
//! These tests change the machine: they create `/var/lib/parhel`, a loop-mounted btrfs
//! image, a systemd unit and `/parhel/<user>`, and they need `sudo`, systemd and the
//! btrfs tools. They run only when `PARHEL_TEST_SETUP=1` (the CI job `setup (Linux)`);
//! otherwise each one prints a notice and passes, so a developer's machine is never
//! touched by accident. `scripts/linux-test.sh` (a container without systemd) does not
//! set it.
//!
//! Who runs what: the tests run as the ordinary user, as the product does. Only the
//! commands that need root go through `sudo -n` (the binary's `setup`, `systemctl`,
//! `rm`/`mv` of the protected image): a worktree is never created by root, and the
//! binary under test sees `SUDO_USER` exactly as a person typing `sudo git parhel setup`
//! would. The CI runner has passwordless sudo.
//!
//! Tests share one store (`/parhel/<user>`), so they are serialized and each one starts
//! from, and returns to, a machine without a store.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::*;
use serial_test::serial;

/// Whether the machine-changing tests are enabled. A skipped test says so.
fn enabled() -> bool {
    let on = std::env::var_os("PARHEL_TEST_SETUP").is_some_and(|v| v == "1");
    if on {
        assert!(
            // SAFETY: `geteuid` has no preconditions.
            unsafe { libc::geteuid() } != 0,
            "run these tests as an ordinary user: they call sudo for the few commands that need root"
        );
    } else {
        eprintln!(
            "SKIPPED: PARHEL_TEST_SETUP=1 is not set (this test creates a store with sudo, \
             systemd and a loop mount; CI job `setup (Linux)` sets it)"
        );
    }
    on
}

fn uid() -> u32 {
    // SAFETY: `getuid` has no preconditions.
    unsafe { libc::getuid() }
}

fn user() -> String {
    common::store::current_user()
}

fn store_root() -> PathBuf {
    PathBuf::from(format!("/parhel/{}", user()))
}

fn image() -> PathBuf {
    PathBuf::from(format!("/var/lib/parhel/{}.img", user()))
}

/// `parhel-<user>.mount` (the user names used by CI contain no characters systemd escapes).
fn unit() -> String {
    format!("parhel-{}.mount", user())
}

fn unit_file() -> PathBuf {
    Path::new("/etc/systemd/system").join(unit())
}

/// `sudo -n <program> <args>`: a command that needs root.
fn sudo(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Output {
    Command::new("sudo")
        .arg("-n")
        .arg(program)
        .args(args)
        .output()
        .expect("sudo is available")
}

/// `sudo git-parhel setup <args>`, the way a person types it.
fn setup(args: &[&str]) -> Output {
    let mut all = vec!["setup"];
    all.extend(args);
    sudo(parhel_bin(), &all)
}

fn shell_ok(script: &str) -> bool {
    sudo("sh", &["-c", script]).status.success()
}

/// `/var/lib/parhel` is root's and mode 0700 (the images are nobody else's business),
/// so the ordinary user cannot even see whether an image is there: `Path::exists`
/// answers `false` for a permission error, which would make every "is gone" assertion
/// pass for the wrong reason. Whatever is asked about the images is asked as root.
fn root_exists(path: impl AsRef<Path>) -> bool {
    sudo("test", &["-e", path.as_ref().to_str().unwrap()])
        .status
        .success()
}

/// What `stat` says about a file, read as root.
struct RootStat {
    len: u64,
    allocated: u64,
    mode: u32,
    uid: u32,
    inode: u64,
    mtime: i64,
}

fn root_stat(path: impl AsRef<Path>) -> RootStat {
    let out = sudo(
        "stat",
        &[
            "-c",
            "%s %b %B %a %u %i %Y",
            path.as_ref().to_str().unwrap(),
        ],
    );
    assert_success(&out);
    let text = stdout(&out);
    let f: Vec<&str> = text.split_whitespace().collect();
    RootStat {
        len: f[0].parse().unwrap(),
        allocated: f[1].parse::<u64>().unwrap() * f[2].parse::<u64>().unwrap(),
        mode: u32::from_str_radix(f[3], 8).unwrap(),
        uid: f[4].parse().unwrap(),
        inode: f[5].parse().unwrap(),
        mtime: f[6].parse().unwrap(),
    }
}

/// Starts from a machine without a store and puts it back on drop, whatever happened.
struct Machine;

impl Machine {
    fn clean() -> Self {
        remove_store();
        Self
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        remove_store();
    }
}

/// Best effort: used before and after every test.
fn remove_store() {
    // Anything left in the store would make `--remove` refuse: clear the worktrees.
    let _ = sudo(
        "sh",
        &[
            "-c",
            &format!("rm -rf {}/worktrees/*", store_root().display()),
        ],
    );
    let _ = setup(&["--remove"]);
}

/// A source on ext4 and its sandbox; `None` (notice) without `PARHEL_TEST_EXT4`.
fn ext4_source() -> Option<(Sandbox, TestRepo)> {
    let ext4 = common::volume::test_mount(common::volume::MountedFs::Ext4)?;
    let sb = Sandbox::new_in(&ext4);
    let src = project(&sb);
    Some((sb, src))
}

/// `git-parhel add` with the default store location (no `PARHEL_STORE`).
fn add_default(sb: &Sandbox, src: &TestRepo, dest: &Path, branch: &str) -> Output {
    let args = vec!["--new-branch", branch];
    parhel_cmd(&sb.env, &src.root, &add_args(dest, &args))
        .env_remove("PARHEL_STORE")
        .output()
        .unwrap()
}

// I-SET01: `sudo git parhel setup` creates the image, the unit, the mount and the
// marker, owned by the user who ran sudo.
#[test]
#[serial(setup)]
fn iset01_setup_creates_image_unit_mount_and_marker() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    let out = setup(&["--size", "512M"]);
    assert_success(&out);
    let said = stdout(&out);
    assert!(said.contains(store_root().to_str().unwrap()), "{said}");
    assert!(said.contains(image().to_str().unwrap()), "{said}");

    // The image: sparse (a 512M claim, almost nothing allocated), private to root.
    let md = root_stat(image());
    assert_eq!(md.len, 512 << 20);
    assert!(md.allocated < 64 << 20, "allocated {} bytes", md.allocated);
    assert_eq!(md.mode, 0o600);
    assert_eq!(md.uid, 0);

    // The unit exists, is enabled and active; the store is mounted as btrfs.
    let text = fs::read_to_string(unit_file()).unwrap();
    for line in [
        format!("What={}", image().display()),
        format!("Where={}", store_root().display()),
        "Type=btrfs".to_owned(),
        "Options=loop,discard,noatime,nosuid,nodev".to_owned(),
        "WantedBy=local-fs.target".to_owned(),
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "unit lacks `{line}`:\n{text}"
        );
    }
    assert!(shell_ok(&format!(
        "systemctl is-enabled {} && systemctl is-active {}",
        unit(),
        unit()
    )));
    assert_eq!(common::cow::fs_type(&store_root()), "btrfs");

    // The marker and the layout, all the user's.
    let marker = fs::read_to_string(store_root().join(".parhel-store")).unwrap();
    assert_eq!(marker, common::store::marker_text(&user()));
    let root = fs::metadata(store_root()).unwrap();
    assert_eq!(root.mode() & 0o7777, 0o700);
    assert_eq!(root.uid(), uid());
    for dir in ["baselines", "worktrees"] {
        let md = fs::metadata(store_root().join(dir)).unwrap();
        assert!(md.is_dir() && md.uid() == uid(), "{dir}");
    }
}

// I-SET02: running it again is a no-op that says so.
#[test]
#[serial(setup)]
fn iset02_rerun_changes_nothing() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    assert_success(&setup(&["--size", "512M"]));
    let image_before = root_stat(image());
    let unit_before = fs::read_to_string(unit_file()).unwrap();
    fs::write(store_root().join("worktrees/keep"), "x").unwrap();

    let out = setup(&["--size", "512M"]);
    assert_success(&out);
    assert!(stdout(&out).contains("already"), "{}", stdout(&out));
    let image_after = root_stat(image());
    assert_eq!(
        (image_after.inode, image_after.mtime),
        (image_before.inode, image_before.mtime)
    );
    assert_eq!(fs::read_to_string(unit_file()).unwrap(), unit_before);
    assert!(
        store_root().join("worktrees/keep").exists(),
        "the store's contents are untouched"
    );
}

// I-SET03: the directory holding the image is immutable: not even root can delete or
// rename the image, while the loop device keeps writing to it.
#[test]
#[serial(setup)]
fn iset03_the_image_cannot_be_deleted_or_renamed_but_stays_writable() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    assert_success(&setup(&["--size", "512M"]));
    let image = image();
    let image = image.to_str().unwrap();

    let lsattr = sudo("lsattr", &["-d", "/var/lib/parhel"]);
    assert!(
        stdout(&lsattr)
            .split_whitespace()
            .next()
            .is_some_and(|f| f.contains('i')),
        "{}",
        stdout(&lsattr)
    );
    let rm = sudo("rm", &["-f", image]);
    assert!(!rm.status.success(), "root deleted the image");
    assert!(
        stderr(&rm).contains("Operation not permitted"),
        "{}",
        stderr(&rm)
    );
    let mv = sudo("mv", &[image, &format!("{image}.moved")]);
    assert!(!mv.status.success(), "root renamed the image");
    assert!(
        stderr(&mv).contains("Operation not permitted"),
        "{}",
        stderr(&mv)
    );
    // Opening it for writing (what the loop device does) still works.
    assert!(shell_ok(&format!(": >> '{image}'")));
    assert!(root_exists(image));
    // And the store keeps working.
    fs::write(store_root().join("worktrees/probe"), "x").unwrap();
}

// I-SET04: `add` from an ext4 source uses the store at its default place: no
// PARHEL_STORE, no flag.
#[test]
#[serial(setup)]
fn iset04_add_from_ext4_uses_the_default_store() {
    if !enabled() {
        return;
    }
    let Some((sb, src)) = ext4_source() else {
        return;
    };
    let machine = Machine::clean();
    assert_success(&setup(&["--size", "512M"]));
    let dest = sb.path("wt");
    let out = add_default(&sb, &src, &dest, "f1");
    assert_success(&out);
    let real = fs::read_link(&dest).expect("a symlink");
    assert!(
        real.starts_with(store_root().join("worktrees")),
        "{}",
        real.display()
    );
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("target/debug/app"), "binary");
    // Without a store `add` says how to get one.
    drop(machine);
    let again = add_default(&sb, &src, &sb.path("wt2"), "f2");
    assert_error(&again, "not-cow");
    assert!(
        stderr(&again).contains("sudo git parhel setup"),
        "{}",
        stderr(&again)
    );
}

// I-SET05: "reboot": stop the unit and start it again; the store comes back with its
// contents. While it is stopped `add` says `store-unavailable`, and `setup` brings it back.
#[test]
#[serial(setup)]
fn iset05_the_store_survives_stopping_and_starting_the_unit() {
    if !enabled() {
        return;
    }
    let Some((sb, src)) = ext4_source() else {
        return;
    };
    let _machine = Machine::clean();
    assert_success(&setup(&["--size", "512M"]));
    let dest = sb.path("wt");
    assert_success(&add_default(&sb, &src, &dest, "f1"));
    let real = fs::read_link(&dest).unwrap();
    // Nothing is using the store, so it can be stopped: detach the worktree first.
    // (A store worktree is locked: two forces remove it without unlocking first.)
    src.git(&[
        "worktree",
        "remove",
        "--force",
        "--force",
        real.to_str().unwrap(),
    ]);
    let _ = fs::remove_file(&dest);

    assert!(shell_ok(&format!("systemctl stop {}", unit())));
    assert!(!shell_ok(&format!(
        "mountpoint -q {}",
        store_root().display()
    )));
    let blocked = add_default(&sb, &src, &sb.path("wt2"), "f2");
    assert_error(&blocked, "store-unavailable");
    assert!(
        stderr(&blocked).contains("systemctl"),
        "{}",
        stderr(&blocked)
    );

    assert!(shell_ok(&format!("systemctl start {}", unit())));
    assert!(
        store_root().join(".parhel-store").exists(),
        "the marker is back"
    );
    assert_success(&add_default(&sb, &src, &sb.path("wt3"), "f3"));
}

// I-SET06: `--remove` refuses while a worktree lives in the store, naming it, and
// succeeds once it is gone.
#[test]
#[serial(setup)]
fn iset06_remove_refuses_with_a_worktree_and_succeeds_without() {
    if !enabled() {
        return;
    }
    let Some((sb, src)) = ext4_source() else {
        return;
    };
    let _machine = Machine::clean();
    assert_success(&setup(&["--size", "512M"]));
    let dest = sb.path("wt");
    assert_success(&add_default(&sb, &src, &dest, "f1"));
    let real = fs::read_link(&dest).unwrap();

    let refused = setup(&["--remove"]);
    assert_error(&refused, "store-in-use");
    assert!(
        stderr(&refused).contains(real.to_str().unwrap()),
        "{}",
        stderr(&refused)
    );
    assert!(
        root_exists(image()) && unit_file().exists(),
        "nothing was removed"
    );

    src.git(&[
        "worktree",
        "remove",
        "--force",
        "--force",
        dest.to_str().unwrap(),
    ]);
    let out = setup(&["--remove"]);
    assert_success(&out);
}

// I-SET07: after `--remove` there is no unit, no image, no mount point; the dangling
// link of a removed worktree is the user's to clear (doctor does); a new setup works.
#[test]
#[serial(setup)]
fn iset07_remove_leaves_nothing_behind() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    assert_success(&setup(&["--size", "512M"]));
    assert_success(&setup(&["--remove"]));
    assert!(!unit_file().exists(), "unit left");
    assert!(!root_exists(image()), "image left");
    assert!(!store_root().exists(), "mount point left");
    assert!(
        !shell_ok(&format!("systemctl cat {}", unit())),
        "systemd still knows the unit"
    );
    assert!(!shell_ok(&format!(
        "mountpoint -q {}",
        store_root().display()
    )));
    // Idempotent.
    let again = setup(&["--remove"]);
    assert_success(&again);
    // And the machine can be set up again.
    assert_success(&setup(&["--size", "512M"]));
    assert!(store_root().join(".parhel-store").exists());
}

// I-SET08: `--preallocate` allocates the whole image.
#[test]
#[serial(setup)]
fn iset08_preallocate_allocates_the_whole_image() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    assert_success(&setup(&["--size", "256M", "--preallocate"]));
    let md = root_stat(image());
    assert_eq!(md.len, 256 << 20);
    assert!(
        md.allocated >= 256 << 20,
        "allocated only {} bytes",
        md.allocated
    );
}

// I-SET09: without root the command says so and names the command to run; nothing is
// created.
#[test]
#[serial(setup)]
fn iset09_without_root_it_asks_for_sudo() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    for (args, command) in [
        (&["--size", "512M"][..], "sudo git parhel setup"),
        (&["--remove"][..], "sudo git parhel setup --remove"),
    ] {
        let mut full = vec!["setup"];
        full.extend(args);
        let out = Command::new(parhel_bin()).args(&full).output().unwrap();
        assert_error(&out, "not-root");
        assert!(stderr(&out).contains(command), "{}", stderr(&out));
    }
    assert!(!root_exists(image()) && !unit_file().exists());
}

// I-SET10: a root shell does not make root the owner of the store; an explicit
// `--user root` does.
#[test]
#[serial(setup)]
fn iset10_root_is_never_inferred() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    // `sudo env -u SUDO_USER ...`: what a root shell looks like to the binary.
    let out = sudo(
        "env",
        &[
            "-u",
            "SUDO_USER",
            parhel_bin().to_str().unwrap(),
            "setup",
            "--size",
            "512M",
        ],
    );
    assert_error(&out, "usage");
    assert!(stderr(&out).contains("--user"), "{}", stderr(&out));
    assert!(!root_exists(image()));
    assert!(!root_exists("/var/lib/parhel/root.img"));
}

// I-SET11: a missing mkfs.btrfs is reported with the package that provides it.
#[test]
#[serial(setup)]
fn iset11_missing_btrfs_progs_names_the_package() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    let out = sudo(
        "env",
        &[
            "PATH=/nonexistent",
            parhel_bin().to_str().unwrap(),
            "setup",
            "--size",
            "512M",
        ],
    );
    assert_error(&out, "missing-tool");
    assert!(stderr(&out).contains("btrfs-progs"), "{}", stderr(&out));
    assert!(!root_exists(image()) && !unit_file().exists());
}

// I-SET12: an unknown user is a usage error and nothing is created.
#[test]
#[serial(setup)]
fn iset12_unknown_user() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    let out = setup(&["--user", "no-such-user-parhel", "--size", "512M"]);
    assert_error(&out, "usage");
    assert!(
        stderr(&out).contains("no-such-user-parhel"),
        "{}",
        stderr(&out)
    );
    assert!(!root_exists("/var/lib/parhel/no-such-user-parhel.img"));
}

// I-SET13: `doctor` reports a deleted backing image while the store is still mounted.
#[test]
#[serial(setup)]
fn iset13_doctor_reports_a_deleted_backing_image() {
    if !enabled() {
        return;
    }
    let Some((sb, src)) = ext4_source() else {
        return;
    };
    let _machine = Machine::clean();
    assert_success(&setup(&["--size", "512M"]));
    // Only possible by lifting the protection: that is the accident being detected.
    assert!(shell_ok("chattr -i /var/lib/parhel"));
    assert!(
        sudo("rm", &["-f", image().to_str().unwrap()])
            .status
            .success()
    );
    let out = parhel_cmd(&sb.env, &src.root, &["doctor"])
        .env_remove("PARHEL_STORE")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("store-image-deleted"),
        "{}",
        stdout(&out)
    );
    assert!(shell_ok("chattr +i /var/lib/parhel"));
}

/// A second account for I-SET15, and its store, gone on drop whatever happened.
struct SecondUser;

const SECOND: &str = "parhelbob";

impl SecondUser {
    fn create() -> Self {
        let _ = sudo("userdel", &[SECOND]);
        let made = sudo(
            "useradd",
            &["--no-create-home", "--shell", "/usr/sbin/nologin", SECOND],
        );
        assert!(made.status.success(), "useradd: {}", stderr(&made));
        Self
    }
}

impl Drop for SecondUser {
    fn drop(&mut self) {
        let _ = setup(&["--remove", "--user", SECOND]);
        let _ = sudo("userdel", &[SECOND]);
    }
}

// I-SET15: a second user's store goes into the directory that protects the first one's
// image: the protection is lifted for the new image and is back afterwards, for both.
#[test]
#[serial(setup)]
fn iset15_a_second_users_store_keeps_every_image_protected() {
    if !enabled() {
        return;
    }
    let _machine = Machine::clean();
    assert_success(&setup(&["--size", "256M"]));
    let _bob = SecondUser::create();
    let out = setup(&["--size", "256M", "--user", SECOND]);
    assert_success(&out);

    let bob_image = format!("/var/lib/parhel/{SECOND}.img");
    let bob_store = format!("/parhel/{SECOND}");
    assert!(
        root_exists(image()) && root_exists(&bob_image),
        "both images exist"
    );
    assert!(shell_ok(&format!("mountpoint -q {bob_store}")));
    assert!(shell_ok(&format!(
        "mountpoint -q {}",
        store_root().display()
    )));
    let bob_uid = String::from_utf8(sudo("id", &["-u", SECOND]).stdout)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert_eq!(fs::metadata(&bob_store).unwrap().uid(), bob_uid);

    let lsattr = sudo("lsattr", &["-d", "/var/lib/parhel"]);
    assert!(
        stdout(&lsattr)
            .split_whitespace()
            .next()
            .is_some_and(|f| f.contains('i')),
        "{}",
        stdout(&lsattr)
    );
    for victim in [image().to_str().unwrap().to_owned(), bob_image.clone()] {
        let rm = sudo("rm", &["-f", &victim]);
        assert!(!rm.status.success(), "root deleted {victim}");
        assert!(
            stderr(&rm).contains("Operation not permitted"),
            "{}",
            stderr(&rm)
        );
    }

    // Removing one user's store leaves the other's image where it is, still protected.
    assert_success(&setup(&["--remove", "--user", SECOND]));
    assert!(!root_exists(&bob_image) && root_exists(image()));
    let rm = sudo("rm", &["-f", image().to_str().unwrap()]);
    assert!(
        !rm.status.success(),
        "the remaining image lost its protection"
    );
}

// I-SET14: off Linux `setup` is an `unsupported-platform` error and touches nothing.
// Runs everywhere it can: it is safe by construction.
#[cfg(not(target_os = "linux"))]
#[test]
fn iset14_setup_off_linux_is_unsupported() {
    let sb = Sandbox::new();
    let out = parhel(&sb.env, &sb.root, &["setup"]);
    assert_error(&out, "unsupported-platform");
    assert!(
        stderr(&out).to_lowercase().contains("linux"),
        "{}",
        stderr(&out)
    );
}
