//! `git parhel setup`: create (or remove) the per-user store. Linux, root.
//!
//! TDD state: only the platform guard is real; the steps are described by the tests.

use std::path::PathBuf;

use crate::domain::{Error, ErrorCode, Result, SetupRequest};
use crate::ports::Host;

/// What the store looks like after `setup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSummary {
    pub user: String,
    /// Mount point (`/parhel/<user>`).
    pub store: PathBuf,
    pub image: PathBuf,
    /// Size the image claims.
    pub apparent: u64,
    /// Space the image occupies on the host.
    pub allocated: u64,
    /// systemd mount unit name.
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    Created(StoreSummary),
    /// A healthy store already existed: nothing was changed.
    AlreadySetUp(StoreSummary),
    Removed(StoreSummary),
    /// `--remove` with no store: nothing to do.
    NothingToRemove {
        user: String,
    },
}

/// Run `git parhel setup`.
pub fn setup(host: &dyn Host, _request: &SetupRequest) -> Result<SetupOutcome> {
    if !host.is_linux() {
        return Err(Error::new(
            ErrorCode::UnsupportedPlatform,
            "`git parhel setup` creates a btrfs store with systemd and only exists on Linux; \
             macOS (APFS) does not need one",
        ));
    }
    Err(Error::not_implemented("setup"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{FakeHost, HostCall, UNIT_DIR, p};
    use crate::domain::request::DEFAULT_STORE_SIZE;
    use crate::ports::ImageUsage;
    use rstest::rstest;
    use std::path::Path;

    const IMG: &str = "/var/lib/parhel/alice.img";
    const ROOT: &str = "/parhel/alice";
    const UNIT: &str = "parhel-alice.mount";
    const UNIT_FILE: &str = "/etc/systemd/system/parhel-alice.mount";
    const MARKER_TEXT: &str =
        "format=1\nuser=alice\nimage=/var/lib/parhel/alice.img\nunit=parhel-alice.mount\n";

    fn request() -> SetupRequest {
        SetupRequest {
            size: DEFAULT_STORE_SIZE,
            preallocate: false,
            user: None,
            remove: false,
        }
    }

    fn removal() -> SetupRequest {
        SetupRequest {
            remove: true,
            ..request()
        }
    }

    fn assert_no_mutation(h: &FakeHost) {
        assert!(
            h.calls().is_empty(),
            "expected no side effects: {:#?}",
            h.lines()
        );
    }

    /// The machine as `setup` leaves it for alice, built by hand (so tests of what
    /// happens afterwards do not depend on the code under test).
    fn installed(h: &FakeHost) {
        for dir in [
            "/var/lib/parhel",
            "/parhel",
            ROOT,
            "/parhel/alice/baselines",
            "/parhel/alice/worktrees",
        ] {
            h.dirs.borrow_mut().insert(p(dir));
        }
        h.images.borrow_mut().insert(
            p(IMG),
            ImageUsage {
                apparent: DEFAULT_STORE_SIZE,
                allocated: 4096,
            },
        );
        h.formatted.borrow_mut().insert(p(IMG));
        h.files.borrow_mut().insert(
            p(UNIT_FILE),
            "[Unit]\nDescription=parhel store for alice\n\n[Mount]\nWhat=/var/lib/parhel/alice.img\n\
             Where=/parhel/alice\nType=btrfs\nOptions=loop,discard,noatime\n\n[Install]\n\
             WantedBy=local-fs.target\n"
                .into(),
        );
        h.files
            .borrow_mut()
            .insert(p("/parhel/alice/.parhel-store"), MARKER_TEXT.into());
        h.mounted.borrow_mut().insert(p(ROOT));
        h.immutable.borrow_mut().insert(p("/var/lib/parhel"));
    }

    fn expected_setup() -> Vec<&'static str> {
        vec![
            "mkdir /var/lib/parhel",
            "image /var/lib/parhel/alice.img 107374182400 sparse",
            "run mkfs.btrfs -q -L parhel-alice /var/lib/parhel/alice.img",
            "mkdir /parhel",
            "mkdir /parhel/alice",
            "write /etc/systemd/system/parhel-alice.mount",
            "run systemctl daemon-reload",
            "run systemctl enable --now parhel-alice.mount",
            "mkdir /parhel/alice",
            "mkdir /parhel/alice/baselines",
            "mkdir /parhel/alice/worktrees",
            "write /parhel/alice/.parhel-store",
            "run chattr +i /var/lib/parhel",
            "verify-immutable /var/lib/parhel /var/lib/parhel/alice.img",
        ]
    }

    // U-SET-01: off Linux the command does not exist, and nothing is attempted. Real
    // (the platform guard is pure).
    #[test]
    fn refuses_off_linux() {
        let h = FakeHost {
            linux: false,
            ..FakeHost::default()
        };
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedPlatform);
        assert_no_mutation(&h);
    }

    // U-SET-02: without root, say which command to run; for removal, the removal.
    #[rstest]
    #[case::setup(request(), "sudo git parhel setup")]
    #[case::remove(removal(), "sudo git parhel setup --remove")]
    fn requires_root_and_names_the_sudo_command(#[case] req: SetupRequest, #[case] command: &str) {
        let h = FakeHost {
            root: false,
            ..FakeHost::default()
        };
        let err = setup(&h, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotRoot, "{err}");
        assert!(err.message.contains(command), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-03: no mkfs.btrfs: name the program and the package, touch nothing.
    #[test]
    fn missing_mkfs_names_the_package() {
        let h = FakeHost::default().without_tool("mkfs.btrfs");
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MissingTool, "{err}");
        assert!(err.message.contains("mkfs.btrfs"), "{}", err.message);
        assert!(err.message.contains("btrfs-progs"), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-04: btrfs in the kernel but not loaded: load it and go on.
    #[test]
    fn loads_the_btrfs_module_when_it_is_only_loadable() {
        let h = FakeHost::default();
        h.files
            .borrow_mut()
            .insert(p("/proc/filesystems"), "nodev\tsysfs\n\text4\n".into());
        setup(&h, &request()).unwrap();
        assert_eq!(
            h.lines().first().map(String::as_str),
            Some("run modprobe btrfs")
        );
        assert!(
            h.lines()
                .contains(&"run systemctl enable --now parhel-alice.mount".to_owned())
        );
    }

    // U-SET-05: ... and if it cannot be loaded, stop before creating anything.
    #[test]
    fn kernel_without_btrfs_is_unsupported() {
        let h = FakeHost::default().failing("modprobe", ErrorCode::Io);
        h.files
            .borrow_mut()
            .insert(p("/proc/filesystems"), "nodev\tsysfs\n\text4\n".into());
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedPlatform, "{err}");
        assert!(err.message.contains("btrfs"), "{}", err.message);
        assert_eq!(
            h.lines(),
            vec!["run modprobe btrfs"],
            "only the attempt to load it"
        );
    }

    // U-SET-06
    #[test]
    fn no_systemd_is_unsupported() {
        let h = FakeHost {
            systemd: false,
            ..FakeHost::default()
        };
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedPlatform, "{err}");
        assert!(err.message.contains("systemd"), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-07
    #[test]
    fn unknown_user_is_a_usage_error() {
        let h = FakeHost::default();
        let req = SetupRequest {
            user: Some("nobody-here".into()),
            ..request()
        };
        let err = setup(&h, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage, "{err}");
        assert!(err.message.contains("nobody-here"), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-08: the store belongs to whoever ran sudo, unless told otherwise.
    #[test]
    fn the_user_defaults_to_the_sudo_user() {
        let h = FakeHost::default();
        let SetupOutcome::Created(summary) = setup(&h, &request()).unwrap() else {
            panic!("expected a new store");
        };
        assert_eq!(summary.user, "alice");
        let h = FakeHost::default();
        let req = SetupRequest {
            user: Some("bob".into()),
            ..request()
        };
        let SetupOutcome::Created(summary) = setup(&h, &req).unwrap() else {
            panic!("expected a new store");
        };
        assert_eq!(summary.user, "bob");
        assert_eq!(summary.store, p("/parhel/bob"));
    }

    // U-SET-09: root is never inferred as the store's owner (a root shell, `sudo -i`),
    // only chosen with an explicit `--user root`.
    #[rstest]
    #[case::root_shell(None)]
    #[case::sudo_from_root(Some("root"))]
    fn inferred_root_is_refused(#[case] sudo_user: Option<&str>) {
        let mut h = FakeHost::default();
        h.env.clear();
        if let Some(user) = sudo_user {
            h.env.insert("SUDO_USER".into(), user.into());
        }
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage, "{err}");
        assert!(err.message.contains("--user"), "{}", err.message);
        assert_no_mutation(&h);
    }

    #[test]
    fn explicit_root_is_honored() {
        let mut h = FakeHost::default();
        h.env.clear();
        let req = SetupRequest {
            user: Some("root".into()),
            ..request()
        };
        let SetupOutcome::Created(summary) = setup(&h, &req).unwrap() else {
            panic!("expected a new store");
        };
        assert_eq!(summary.user, "root");
        assert_eq!(
            h.attrs.borrow()[Path::new("/parhel/root")],
            (0o700, Some((0, 0)))
        );
    }

    // U-SET-10: the steps, in order. Every one is checked by the next.
    #[test]
    fn creates_the_store_step_by_step() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        assert_eq!(h.lines(), expected_setup());
    }

    // U-SET-11: who owns what, and who can read it.
    #[test]
    fn modes_and_owners() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        let attrs = h.attrs.borrow();
        let alice = Some((1000, 1000));
        assert_eq!(attrs[Path::new("/var/lib/parhel")], (0o700, None));
        assert_eq!(attrs[Path::new(IMG)], (0o600, None));
        assert_eq!(attrs[Path::new("/parhel")], (0o755, None));
        assert_eq!(attrs[Path::new(UNIT_FILE)], (0o644, None));
        // The mounted filesystem's root, its directories and the marker are the user's.
        assert_eq!(attrs[Path::new(ROOT)], (0o700, alice));
        assert_eq!(attrs[Path::new("/parhel/alice/baselines")].1, alice);
        assert_eq!(attrs[Path::new("/parhel/alice/worktrees")].1, alice);
        assert_eq!(attrs[Path::new("/parhel/alice/.parhel-store")].1, alice);
    }

    // U-SET-12: the mount unit.
    #[test]
    fn writes_a_mount_unit_that_survives_reboots() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        let unit = h.text(UNIT_FILE).expect("the unit exists");
        let lines: Vec<&str> = unit.lines().collect();
        for line in [
            "What=/var/lib/parhel/alice.img",
            "Where=/parhel/alice",
            "Type=btrfs",
            "Options=loop,discard,noatime",
            "WantedBy=local-fs.target",
        ] {
            assert!(lines.contains(&line), "unit lacks `{line}`:\n{unit}");
        }
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("Description=") && l.contains("parhel")),
            "{unit}"
        );
        let at = |header: &str| lines.iter().position(|l| *l == header).unwrap();
        assert!(at("[Unit]") < at("[Mount]") && at("[Mount]") < at("[Install]"));
        let section = |key: &str| lines.iter().position(|l| l.starts_with(key)).unwrap();
        assert!(at("[Mount]") < section("What=") && section("Options=") < at("[Install]"));
        assert!(at("[Install]") < section("WantedBy="));
    }

    // U-SET-13: the unit is named the way systemd names it (`systemd-escape`), even
    // for user names with a dash, and everything that refers to it uses that name.
    #[test]
    fn unit_name_comes_from_systemd_escape() {
        let mut h = FakeHost::default();
        h.users.push(crate::ports::UserInfo {
            name: "mary-ann".into(),
            uid: 1002,
            gid: 1002,
        });
        let req = SetupRequest {
            user: Some("mary-ann".into()),
            ..request()
        };
        let SetupOutcome::Created(summary) = setup(&h, &req).unwrap() else {
            panic!("expected a new store");
        };
        let unit = "parhel-mary\\x2dann.mount";
        assert_eq!(summary.unit, unit);
        assert!(
            h.text(&format!("{UNIT_DIR}/{unit}")).is_some(),
            "{:?}",
            h.lines()
        );
        assert!(
            h.lines()
                .contains(&format!("run systemctl enable --now {unit}"))
        );
        let marker = h.text("/parhel/mary-ann/.parhel-store").unwrap();
        assert!(marker.contains(&format!("unit={unit}\n")), "{marker}");
    }

    // U-SET-14: the marker `add` looks for.
    #[test]
    fn marker_names_format_user_image_and_unit() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        assert_eq!(
            h.text("/parhel/alice/.parhel-store").as_deref(),
            Some(MARKER_TEXT)
        );
    }

    // U-SET-15: size and allocation.
    #[rstest]
    #[case::default(None, false, 100u64 << 30, "sparse")]
    #[case::size(Some(512u64 << 20), false, 512u64 << 20, "sparse")]
    #[case::preallocated(Some(1u64 << 30), true, 1u64 << 30, "allocated")]
    fn image_size_and_allocation(
        #[case] size: Option<u64>,
        #[case] preallocate: bool,
        #[case] bytes: u64,
        #[case] kind: &str,
    ) {
        let h = FakeHost::default();
        let req = SetupRequest {
            size: size.unwrap_or(DEFAULT_STORE_SIZE),
            preallocate,
            ..request()
        };
        setup(&h, &req).unwrap();
        assert!(
            h.lines().contains(&format!("image {IMG} {bytes} {kind}")),
            "{:?}",
            h.lines()
        );
    }

    // U-SET-16: running it again changes nothing and says so.
    #[test]
    fn rerun_is_a_no_op() {
        let h = FakeHost::default();
        let SetupOutcome::Created(first) = setup(&h, &request()).unwrap() else {
            panic!("expected a new store");
        };
        h.clear_log();
        let SetupOutcome::AlreadySetUp(again) = setup(&h, &request()).unwrap() else {
            panic!("expected the existing store");
        };
        assert_no_mutation(&h);
        assert_eq!(again, first);
    }

    // U-SET-17: a store that was stopped is started again, nothing else.
    #[test]
    fn stopped_store_is_started_not_recreated() {
        let h = FakeHost::default();
        installed(&h);
        h.mounted.borrow_mut().clear();
        setup(&h, &request()).unwrap();
        assert_eq!(
            h.lines(),
            vec!["run systemctl enable --now parhel-alice.mount"]
        );
    }

    // U-SET-18: an image that exists is somebody's data: never formatted again, never
    // truncated, even when the rest is missing.
    #[test]
    fn existing_image_is_never_reformatted() {
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
        h.immutable.borrow_mut().clear();
        setup(&h, &request()).unwrap();
        let lines = h.lines();
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("image ") || l.contains("mkfs")),
            "{lines:?}"
        );
        assert!(lines.contains(&"run systemctl enable --now parhel-alice.mount".to_owned()));
        assert!(h.is_mount_point(Path::new(ROOT)));
    }

    // U-SET-19: no chattr: the store works but its image is unprotected, and setup
    // says so loudly (an error) instead of printing success.
    #[test]
    fn missing_chattr_fails_loudly_but_leaves_a_working_store() {
        let h = FakeHost::default().without_tool("chattr");
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MissingTool, "{err}");
        assert!(
            err.message.contains("chattr") && err.message.contains("e2fsprogs"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains(IMG),
            "names what is unprotected: {}",
            err.message
        );
        let lines = h.lines();
        assert!(lines.contains(&"run systemctl enable --now parhel-alice.mount".to_owned()));
        assert!(h.text("/parhel/alice/.parhel-store").is_some(), "usable");
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("disable") || l.starts_with("rm")),
            "nothing torn down: {lines:?}"
        );
    }

    // U-SET-20: a filesystem that refuses the attribute, or does not honor it, leaves
    // the image deletable: reported as an error that names the image, store kept.
    #[rstest]
    #[case::chattr_fails(FakeHost::default().failing("chattr", ErrorCode::Io))]
    #[case::attribute_not_honored(FakeHost {
        chattr_does_nothing: true,
        ..FakeHost::default()
    })]
    fn unprotected_image_is_reported(#[case] h: FakeHost) {
        let err = setup(&h, &request()).unwrap_err();
        assert!(err.message.contains(IMG), "{}", err.message);
        assert!(err.message.contains("protect"), "{}", err.message);
        let lines = h.lines();
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("disable") || l.starts_with("rm")),
            "the store stays usable: {lines:?}"
        );
        assert!(h.text("/parhel/alice/.parhel-store").is_some());
    }

    // U-SET-21: the summary.
    #[test]
    fn summary_reports_paths_size_and_allocation() {
        let h = FakeHost::default();
        let req = SetupRequest {
            size: 512 << 20,
            ..request()
        };
        let SetupOutcome::Created(summary) = setup(&h, &req).unwrap() else {
            panic!("expected a new store");
        };
        assert_eq!(
            summary,
            StoreSummary {
                user: "alice".into(),
                store: p(ROOT),
                image: p(IMG),
                apparent: 512 << 20,
                allocated: 4096,
                unit: UNIT.into(),
            }
        );
    }

    // ------------------------------------------------------------------- removal

    // U-SET-30: worktrees in the store are not ours to destroy: refuse, naming them.
    #[test]
    fn remove_refuses_while_worktrees_live_in_the_store() {
        let h = FakeHost::default();
        installed(&h);
        let id = "/parhel/alice/worktrees/7005185596c2e32bd0688ebe72725f92";
        for dir in [id.to_owned(), format!("{id}/feat"), format!("{id}/fix-bug")] {
            h.dirs.borrow_mut().insert(p(&dir));
        }
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreInUse, "{err}");
        assert!(
            err.message.contains(&format!("{id}/feat")),
            "{}",
            err.message
        );
        assert!(
            err.message.contains(&format!("{id}/fix-bug")),
            "{}",
            err.message
        );
        assert_no_mutation(&h);
    }

    // U-SET-31: baselines are a cache; only worktrees block the removal.
    #[test]
    fn remove_ignores_baselines_and_empty_worktree_directories() {
        let h = FakeHost::default();
        installed(&h);
        h.dirs.borrow_mut().insert(p("/parhel/alice/baselines/abc"));
        h.dirs.borrow_mut().insert(p("/parhel/alice/worktrees/abc"));
        assert!(matches!(
            setup(&h, &removal()).unwrap(),
            SetupOutcome::Removed(_)
        ));
    }

    // U-SET-32: the removal, in the order that matters: stop using the image, make the
    // directory writable, delete, tidy.
    #[test]
    fn remove_takes_the_store_down_in_order() {
        let h = FakeHost::default();
        installed(&h);
        let SetupOutcome::Removed(summary) = setup(&h, &removal()).unwrap() else {
            panic!("expected removal");
        };
        assert_eq!(
            h.lines(),
            vec![
                "run systemctl disable --now parhel-alice.mount",
                "rm /etc/systemd/system/parhel-alice.mount",
                "run systemctl daemon-reload",
                "run chattr -i /var/lib/parhel",
                "rm /var/lib/parhel/alice.img",
                "rmdir /parhel/alice",
            ]
        );
        assert_eq!(summary.user, "alice");
        assert_eq!(summary.unit, UNIT);
        assert!(
            !h.exists(Path::new(IMG))
                && !h.exists(Path::new(UNIT_FILE))
                && !h.exists(Path::new(ROOT))
        );
    }

    // U-SET-33: other users' images stay protected.
    #[test]
    fn remove_protects_the_directory_again_while_other_images_remain() {
        let h = FakeHost::default();
        installed(&h);
        h.images.borrow_mut().insert(
            p("/var/lib/parhel/bob.img"),
            ImageUsage {
                apparent: 1,
                allocated: 1,
            },
        );
        setup(&h, &removal()).unwrap();
        assert_eq!(
            h.lines().last().map(String::as_str),
            Some("run chattr +i /var/lib/parhel")
        );
        assert!(h.exists(Path::new("/var/lib/parhel/bob.img")));
        assert!(h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
    }

    #[test]
    fn remove_leaves_the_directory_unprotected_when_it_is_empty() {
        let h = FakeHost::default();
        installed(&h);
        setup(&h, &removal()).unwrap();
        assert!(
            !h.lines()
                .iter()
                .any(|l| l == "run chattr +i /var/lib/parhel")
        );
        assert!(!h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
    }

    // U-SET-34: removing what is not there is fine, and does nothing.
    #[test]
    fn remove_is_idempotent() {
        let h = FakeHost::default();
        installed(&h);
        setup(&h, &removal()).unwrap();
        h.clear_log();
        assert_eq!(
            setup(&h, &removal()).unwrap(),
            SetupOutcome::NothingToRemove {
                user: "alice".into()
            }
        );
        assert_no_mutation(&h);
    }

    // U-SET-35: removal follows the same user rules as creation.
    #[test]
    fn remove_infers_the_user_like_creation() {
        let mut h = FakeHost::default();
        h.env.clear();
        assert_eq!(setup(&h, &removal()).unwrap_err().code, ErrorCode::Usage);
        assert_no_mutation(&h);
    }

    // U-SET-36: a mount unit that is gone but a mounted image is not "nothing": the
    // pieces that exist are cleaned up, one by one.
    #[test]
    fn remove_cleans_up_a_partial_store() {
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
        setup(&h, &removal()).unwrap();
        assert!(!h.exists(Path::new(IMG)));
        assert!(
            !h.lines().iter().any(|l| l.contains("disable")),
            "{:?}",
            h.lines()
        );
    }

    // U-SET-37: the removal deletes parhel's unit, image and mount point and nothing else
    // that happens to live next to them.
    #[test]
    fn remove_touches_only_parhels_paths() {
        let h = FakeHost::default();
        installed(&h);
        h.files
            .borrow_mut()
            .insert(p("/etc/systemd/system/other.mount"), "x".into());
        h.files
            .borrow_mut()
            .insert(p("/var/lib/parhel/notes.txt"), "mine".into());
        setup(&h, &removal()).unwrap();
        for call in h.calls() {
            if let HostCall::RemoveFile(path) | HostCall::RemoveDir(path) = call {
                assert!(
                    path == p(UNIT_FILE) || path == p(IMG) || path == p(ROOT),
                    "removed {}",
                    path.display()
                );
            }
        }
    }
}
