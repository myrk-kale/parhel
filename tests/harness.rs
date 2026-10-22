//! Self-tests of the integration harness. These MUST pass: they prove the tools the
//! product tests rely on (sandboxes, snapshots, COW evidence, disk images, barriers)
//! can tell right from wrong, so a red product test means a product defect.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use common::cow::{deep_copy, fs_type, physical_offset, shares_storage, write_blob};
use common::volume::{DiskImage, ImageFs};
use common::*;
use serial_test::serial;

#[test]
fn sandbox_creates_committed_repo_with_hermetic_identity() {
    let sb = Sandbox::new();
    let repo = sb.repo("src");
    assert_eq!(repo.head().len(), 40);
    assert_eq!(repo.current_branch().as_deref(), Some("main"));
    assert_eq!(repo.git(&["config", "user.email"]), "test@parhel.invalid");
    repo.assert_clean();
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
}

#[test]
fn sandbox_tmp_is_apfs() {
    let sb = Sandbox::new();
    assert_eq!(
        fs_type(&sb.root),
        "apfs",
        "integration tests assume $TMPDIR on APFS"
    );
}

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
