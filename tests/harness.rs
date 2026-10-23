//! Self-tests of the integration harness. These MUST pass: they prove the tools the
//! product tests rely on (sandboxes, snapshots, COW evidence, disk images, barriers)
//! can tell right from wrong, so a red product test means a product defect.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use common::cow::{
    deep_copy, fs_type, is_independent_copy, physical_offset, shares_storage, write_blob,
};
#[cfg(target_os = "linux")]
use common::volume::test_mount;
#[cfg(target_os = "macos")]
use common::volume::{DiskImage, ImageFs};
use common::volume::{MountedFs, resolve_mount};
use common::*;
#[cfg(target_os = "macos")]
use serial_test::serial;

#[test]
fn sandbox_creates_committed_repo_with_hermetic_identity() {
    let sb = Sandbox::new();
    let repo = sb.repo("src");
    assert_eq!(repo.head().len(), 40);
    assert_eq!(repo.current_branch().as_deref(), Some("main"));
    assert_eq!(repo.git(&["config", "user.email"]), "test@parhel.invalid");
    repo.assert_clean();
    // Canonical: no symlink left in the path (macOS: /var -> /private/var).
    assert_eq!(repo.root, fs::canonicalize(&repo.root).unwrap());
    #[cfg(target_os = "macos")]
    assert!(
        repo.root.starts_with("/private"),
        "canonical tmp path: {}",
        repo.root.display()
    );
}

type Mutation = Box<dyn Fn(&TestRepo)>;

#[test]
fn snapshot_is_stable_and_detects_every_kind_of_change() {
    let sb = Sandbox::new();
    let repo = sb.repo("src");
    let base = repo.snapshot();
    base.assert_same(&repo.snapshot());

    let mutations: Vec<Mutation> = vec![
        Box::new(|r| r.write("README.md", "changed")),
        Box::new(|r| r.write("new.txt", "x")),
        Box::new(|r| r.chmod("README.md", 0o755)),
        Box::new(|r| {
            r.git(&["branch", "extra"]);
        }),
        Box::new(|r| {
            let p = r.root.parent().unwrap().join("wt-extra");
            r.git(&["worktree", "add", "-q", "--detach", p.to_str().unwrap()]);
        }),
    ];
    for (i, mutate) in mutations.iter().enumerate() {
        let sb = Sandbox::new();
        let repo = sb.repo("src");
        let before = repo.snapshot();
        mutate(&repo);
        let after = repo.snapshot();
        let caught = std::panic::catch_unwind(|| before.assert_same(&after));
        assert!(caught.is_err(), "mutation #{i} went unnoticed");
    }
}

#[test]
fn staging_leftover_scan_finds_only_staging_dirs() {
    let sb = Sandbox::new();
    fs::create_dir(sb.path(".parhel-staging-abc")).unwrap();
    fs::create_dir(sb.path("parhel-staging-not")).unwrap();
    assert_eq!(
        staging_leftovers(&[&sb.root]),
        vec![sb.path(".parhel-staging-abc")]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn cow_evidence_distinguishes_clone_from_copy() {
    let sb = Sandbox::new();
    let a = sb.path("a.bin");
    write_blob(&a, 1 << 20, 42);
    let cloned = sb.path("cloned.bin");
    let st = Command::new("cp")
        .arg("-c")
        .arg(&a)
        .arg(&cloned)
        .status()
        .unwrap();
    assert!(st.success());
    let copied = sb.path("copied.bin");
    deep_copy(&a, &copied);
    assert!(physical_offset(&a).unwrap() > 0);
    assert!(
        shares_storage(&a, &cloned),
        "clonefile copy must share blocks"
    );
    assert!(
        !shares_storage(&a, &copied),
        "deep copy must not share blocks"
    );
    assert!(is_independent_copy(&a, &copied));
    assert!(!is_independent_copy(&a, &cloned));
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_tmp_is_apfs() {
    let sb = Sandbox::new();
    assert_eq!(
        fs_type(&sb.root),
        "apfs",
        "integration tests assume $TMPDIR on APFS"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[serial(hdiutil)]
fn disk_images_mount_with_requested_filesystem() {
    let apfs = DiskImage::new(ImageFs::Apfs, 64);
    let hfs = DiskImage::new(ImageFs::Hfs, 64);
    let cs = DiskImage::new(ImageFs::ApfsCaseSensitive, 64);
    assert_eq!(fs_type(&apfs.mount), "apfs");
    assert_eq!(fs_type(&hfs.mount), "hfs");
    assert_eq!(fs_type(&cs.mount), "apfs");
    fs::write(cs.mount.join("A"), "upper").unwrap();
    fs::write(cs.mount.join("a"), "lower").unwrap();
    assert_eq!(
        fs::read_to_string(cs.mount.join("A")).unwrap(),
        "upper",
        "case-sensitive"
    );
    // Different volumes have different device ids.
    assert_ne!(
        fs::metadata(&apfs.mount).unwrap().dev(),
        fs::metadata(&cs.mount).unwrap().dev()
    );
}

#[cfg(target_os = "macos")]
#[test]
#[serial(hdiutil)]
fn disk_image_detaches_on_drop() {
    let mount = {
        let img = DiskImage::new(ImageFs::Apfs, 64);
        img.mount.clone()
    };
    let mounts = String::from_utf8(Command::new("mount").output().unwrap().stdout).unwrap();
    assert!(
        !mounts.contains(mount.to_str().unwrap()),
        "still mounted: {}",
        mount.display()
    );
}

#[test]
fn barrier_coordinates_with_waiting_process() {
    let sb = Sandbox::new();
    let barrier = Barrier::new(&sb.root, "t");
    let spec = barrier.spec("x");
    let path = spec.strip_prefix("x=wait:").unwrap().to_owned();
    let mut child = Command::new("sh")
        .args([
            "-c",
            r#"touch "$1.reached"; while [ ! -f "$1" ]; do sleep 0.02; done"#,
            "sh",
            &path,
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    barrier.wait_reached(&mut child);
    barrier.release();
    assert!(finish(child).status.success());
}

#[test]
#[should_panic(expected = "before reaching the failpoint")]
fn barrier_fails_fast_when_process_exits_early() {
    let sb = Sandbox::new();
    let barrier = Barrier::new(&sb.root, "t");
    let mut child = Command::new("true")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    barrier.wait_reached(&mut child);
}

#[test]
fn assert_error_rejects_wrong_code() {
    let out = Command::new("sh")
        .args(["-c", "echo 'error[not-implemented]: x' >&2; exit 1"])
        .output()
        .unwrap();
    assert_error(&out, "not-implemented");
    let caught = std::panic::catch_unwind(|| assert_error(&out, "dest-exists"));
    assert!(
        caught.is_err(),
        "a stub's not-implemented must not satisfy a refusal test"
    );
}

#[test]
fn parhel_binary_is_built() {
    assert!(Path::new(&parhel_bin()).is_file());
}

/// The `#[values(...)]` lists in `add_rollback.rs` must cover every product failpoint.
#[test]
fn rollback_tests_cover_every_failpoint() {
    let src = include_str!("add_rollback.rs");
    for fp in parhel::failpoint::ADD_FAILPOINTS {
        assert!(
            src.matches(&format!("\"{fp}\"")).count() >= 2,
            "{fp} not covered"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cleans_up_immutable_and_unreadable_fixtures() {
    let root = {
        let sb = Sandbox::new();
        fs::create_dir_all(sb.path("x/locked")).unwrap();
        fs::write(sb.path("x/frozen"), "f").unwrap();
        assert!(
            Command::new("chflags")
                .arg("uchg")
                .arg(sb.path("x/frozen"))
                .status()
                .unwrap()
                .success()
        );
        fs::set_permissions(
            sb.path("x/locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();
        sb.root.clone()
    };
    assert!(!root.exists(), "sandbox left {}", root.display());
}

#[test]
fn assert_interrupted_checks_status_and_code() {
    let ok = Command::new("sh")
        .args(["-c", "echo 'error[interrupted]: x' >&2; exit 143"])
        .output()
        .unwrap();
    assert_interrupted(&ok, libc::SIGTERM);
    let wrong = Command::new("sh")
        .args(["-c", "echo 'error[interrupted]: x' >&2; exit 130"])
        .output()
        .unwrap();
    assert!(std::panic::catch_unwind(|| assert_interrupted(&wrong, libc::SIGTERM)).is_err());
}

#[test]
fn rollback_failpoint_is_exercised() {
    let src = include_str!("add_rollback.rs");
    assert!(src.contains(&format!("\"{}\"", parhel::failpoint::ROLLBACK_FAILPOINT)));
}

#[test]
fn publish_failpoint_is_exercised() {
    let src = include_str!("add_rollback.rs");
    assert!(src.contains(&format!("{}=kill", parhel::failpoint::PUBLISH_FAILPOINT)));
}

#[test]
fn publish_failpoint_variants_are_exercised() {
    let src = include_str!("add_rollback.rs");
    assert!(src.contains("i45c_publication_detected_even_with_lost_journal"));
}

// ---------------------------------------------------------------- Linux evidence

/// Copy-on-write evidence on Linux (`FIEMAP`): a reflink is detected as shared, an
/// ordinary copy is not, and one file compared with itself is not "shared" either.
#[cfg(target_os = "linux")]
fn reflink_evidence_on(fs_kind: MountedFs) {
    let Some(dir) = test_mount(fs_kind) else {
        return;
    };
    let sb = Sandbox::new_in(&dir);
    let a = sb.path("a.bin");
    write_blob(&a, 1 << 20, 42);
    let reflinked = sb.path("reflinked.bin");
    let st = Command::new("cp")
        .args(["--reflink=always"])
        .arg(&a)
        .arg(&reflinked)
        .status()
        .unwrap();
    assert!(st.success(), "cp --reflink=always on {fs_kind:?}");
    let copied = sb.path("copied.bin");
    deep_copy(&a, &copied);
    let forced_copy = sb.path("forced-copy.bin");
    let st = Command::new("cp")
        .args(["--reflink=never"])
        .arg(&a)
        .arg(&forced_copy)
        .status()
        .unwrap();
    assert!(st.success());

    assert!(physical_offset(&a).unwrap() > 0);
    assert!(
        shares_storage(&a, &reflinked),
        "a reflink must be detected as shared"
    );
    assert!(
        !is_independent_copy(&a, &reflinked),
        "a reflink does not own its data"
    );
    for copy in [&copied, &forced_copy] {
        assert!(
            !shares_storage(&a, copy),
            "{} is a plain copy, not shared",
            copy.display()
        );
        assert!(
            is_independent_copy(&a, copy),
            "{} owns its data",
            copy.display()
        );
    }
    // Same physical block but nothing shared: the SHARED flag matters, too.
    assert!(!shares_storage(&a, &a), "one file is not a clone of itself");
}

#[cfg(target_os = "linux")]
#[test]
fn reflink_evidence_distinguishes_clone_from_copy_on_btrfs() {
    reflink_evidence_on(MountedFs::Btrfs);
}

#[cfg(target_os = "linux")]
#[test]
fn reflink_evidence_distinguishes_clone_from_copy_on_xfs() {
    reflink_evidence_on(MountedFs::Xfs);
}

// ext4 cannot reflink: the evidence must never report sharing there.
#[cfg(target_os = "linux")]
#[test]
fn copy_evidence_on_ext4_never_reports_sharing() {
    let Some(dir) = test_mount(MountedFs::Ext4) else {
        return;
    };
    let sb = Sandbox::new_in(&dir);
    let a = sb.path("a.bin");
    write_blob(&a, 1 << 20, 7);
    let refused = Command::new("cp")
        .args(["--reflink=always"])
        .arg(&a)
        .arg(sb.path("nope.bin"))
        .output()
        .unwrap();
    assert!(!refused.status.success(), "ext4 has no reflink");
    let copied = sb.path("copied.bin");
    deep_copy(&a, &copied);
    assert!(!shares_storage(&a, &copied));
    assert!(is_independent_copy(&a, &copied));
}

#[cfg(target_os = "linux")]
#[test]
fn sandbox_tmp_is_on_a_reflink_filesystem() {
    let sb = Sandbox::new();
    let fs = fs_type(&sb.root);
    assert!(
        ["btrfs", "xfs", "bcachefs", "zfs"].contains(&fs.as_str()),
        "integration tests assume $TMPDIR on a reflink filesystem, found {fs} \
         (scripts/linux-test.sh sets it up)"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn test_mounts_have_the_announced_filesystem_and_distinct_devices() {
    let mut devices = Vec::new();
    for kind in [MountedFs::Btrfs, MountedFs::Xfs, MountedFs::Ext4] {
        let Some(dir) = test_mount(kind) else {
            continue;
        };
        assert_eq!(fs_type(&dir), kind.fs_type());
        // Writable by the (unprivileged) test user.
        let probe = dir.join(format!("probe-{}", std::process::id()));
        fs::write(&probe, "x").unwrap();
        fs::remove_file(&probe).unwrap();
        devices.push(fs::metadata(&dir).unwrap().dev());
    }
    let mut unique = devices.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), devices.len(), "different mounts: {devices:?}");
}

// ---------------------------------------------------------------- mount helper

#[test]
fn missing_mount_variable_skips_when_not_required() {
    let notice = resolve_mount(MountedFs::Xfs, None, false).unwrap_err();
    assert!(
        notice.contains("PARHEL_TEST_XFS") && notice.contains("SKIPPED"),
        "{notice}"
    );
    // An empty value is a missing value.
    assert!(resolve_mount(MountedFs::Xfs, Some("".into()), false).is_err());
}

#[test]
fn missing_mount_variable_fails_when_required() {
    for kind in [MountedFs::Btrfs, MountedFs::Xfs, MountedFs::Ext4] {
        let caught = std::panic::catch_unwind(|| resolve_mount(kind, None, true));
        let panic = caught.expect_err("required mounts must not skip");
        let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(message.contains(kind.var()), "{message}");
        let empty = std::panic::catch_unwind(|| resolve_mount(kind, Some("".into()), true));
        assert!(empty.is_err(), "empty value is missing");
    }
}

#[test]
fn mount_variable_must_name_an_existing_directory() {
    let sb = Sandbox::new();
    let dir = resolve_mount(MountedFs::Btrfs, Some(sb.root.clone().into()), false).unwrap();
    assert_eq!(dir, sb.root);
    for required in [false, true] {
        let file = sb.path("file");
        fs::write(&file, "x").unwrap();
        for bad in [sb.path("missing"), file] {
            let caught = std::panic::catch_unwind(|| {
                resolve_mount(MountedFs::Btrfs, Some(bad.clone().into()), required)
            });
            assert!(caught.is_err(), "{} (required={required})", bad.display());
        }
    }
}

#[test]
fn mounted_fs_names_match_fs_type_output() {
    assert_eq!(MountedFs::Btrfs.var(), "PARHEL_TEST_BTRFS");
    assert_eq!(MountedFs::Xfs.var(), "PARHEL_TEST_XFS");
    assert_eq!(MountedFs::Ext4.var(), "PARHEL_TEST_EXT4");
    assert_eq!(
        [MountedFs::Btrfs, MountedFs::Xfs, MountedFs::Ext4].map(MountedFs::fs_type),
        ["btrfs", "xfs", "ext4"]
    );
}

// ---------------------------------------------------------------- other helpers

#[test]
fn sandbox_cleans_up_unreadable_and_read_only_fixtures() {
    let root = {
        let sb = Sandbox::new();
        fs::create_dir_all(sb.path("x/locked")).unwrap();
        fs::create_dir_all(sb.path("x/readonly")).unwrap();
        fs::write(sb.path("x/readonly/f"), "f").unwrap();
        for (dir, mode) in [("x/locked", 0o000), ("x/readonly", 0o555)] {
            fs::set_permissions(
                sb.path(dir),
                std::os::unix::fs::PermissionsExt::from_mode(mode),
            )
            .unwrap();
        }
        sb.root.clone()
    };
    assert!(!root.exists(), "sandbox left {}", root.display());
}

#[test]
fn xattr_helper_round_trips_and_reports_absence() {
    use common::xattr;
    let sb = Sandbox::new();
    let f = sb.path("f");
    fs::write(&f, "x").unwrap();
    assert_eq!(xattr::get(&f, xattr::TEST_ATTR).unwrap(), None);
    xattr::set(&f, xattr::TEST_ATTR, b"value").unwrap();
    assert_eq!(
        xattr::get(&f, xattr::TEST_ATTR).unwrap(),
        Some(b"value".to_vec())
    );
}
