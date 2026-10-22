//! I-80: `git parhel doctor`.

mod common;

use std::fs;

use common::volume::{DiskImage, ImageFs};
use common::*;
use serial_test::serial;

// I-80a
#[test]
fn i80a_healthy_repo() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let out = parhel(&sb.env, &src.root, &["doctor"]);
    assert_success(&out);
    assert!(stdout(&out).contains("no issues"), "{}", stdout(&out));
}

// I-80b
#[test]
#[serial(hdiutil)]
fn i80b_reports_non_apfs_volume() {
    let sb = Sandbox::new();
    let hfs = DiskImage::new(ImageFs::Hfs, 64);
    let src = TestRepo::init_at(&sb.env, &hfs.mount.join("src"));
    let out = parhel(&sb.env, &src.root, &["doctor"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("not-apfs") && stdout(&out).contains("hfs"),
        "{}",
        stdout(&out)
    );
}

// I-80c: stray staging dir (crash before the journal was written).
#[test]
fn i80c_reports_and_fixes_orphan_staging_dir() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let stray = sb.path(".parhel-staging-deadbeef");
    fs::create_dir(&stray).unwrap();
    let out = parhel(&sb.env, &src.root, &["doctor"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("orphan-staging"), "{}", stdout(&out));
    assert!(
        stdout(&out).contains(stray.to_str().unwrap()),
        "{}",
        stdout(&out)
    );
    assert!(stray.exists(), "read-only without --fix");

    assert_success(&parhel(&sb.env, &src.root, &["doctor", "--fix"]));
    assert!(!stray.exists());
    assert_success(&parhel(&sb.env, &src.root, &["doctor"]));
}

// I-80d: registration pointing inside a staging dir whose files are gone.
#[test]
fn i80d_reports_and_fixes_orphan_registration() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let staging = sb.path(".parhel-staging-cafe");
    fs::create_dir(&staging).unwrap();
    let wt = staging.join("feat");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "orphaned",
        wt.to_str().unwrap(),
    ]);
    fs::remove_dir_all(&wt).unwrap();

    let out = parhel(&sb.env, &src.root, &["doctor"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("orphan-registration"),
        "{}",
        stdout(&out)
    );

    assert_success(&parhel(&sb.env, &src.root, &["doctor", "--fix"]));
    assert!(!src.worktree_paths().contains(&wt));
    assert!(!staging.exists());
    // Without a journal doctor cannot prove it created the branch: it must keep it.
    assert!(src.branch_tip("orphaned").is_some());
}

// I-80e: foreign gitfile inside the source is reported, never touched.
#[test]
fn i80e_reports_foreign_gitfile() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let other = sb.repo("other");
    other.linked_worktree(&sb.path("other-wt"), "ow");
    // Simulates a tool that moved another repo's linked worktree under node_modules.
    let gitfile = fs::read_to_string(sb.path("other-wt/.git")).unwrap();
    src.write("node_modules/vendored/.git", &gitfile);
    let out = parhel(&sb.env, &src.root, &["doctor", "--fix"]);
    assert_eq!(out.status.code(), Some(1), "not fixable -> still unhealthy");
    assert!(stdout(&out).contains("foreign-gitfile"), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("node_modules/vendored"),
        "{}",
        stdout(&out)
    );
    assert_eq!(src.read("node_modules/vendored/.git"), gitfile);
}

// I-80f: doctor outside a repository.
#[test]
fn i80f_outside_repository() {
    let sb = Sandbox::new();
    assert_error(&parhel(&sb.env, &sb.root, &["doctor"]), "not-a-repo");
}

// I-80g: doctor --fix while an add is running must leave the live run alone.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i80g_doctor_skips_live_run() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let barrier = Barrier::new(&sb.root, "live");
    let mut child = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "live"]),
        Some(&barrier.spec("after_clone")),
    );
    barrier.wait_reached(&mut child);
    let staging = staging_leftovers(&[&sb.root]);
    assert_eq!(staging.len(), 1);

    let out = parhel(&sb.env, &src.root, &["doctor", "--fix"]);
    assert_success(&out);
    assert!(stdout(&out).contains("in-progress"), "{}", stdout(&out));
    assert!(staging[0].exists(), "live staging untouched");
    assert!(src.branch_tip("live").is_some(), "live branch untouched");

    barrier.release();
    assert_success(&finish(child));
    TestRepo::open(&sb.env, &sb.path("dest")).assert_clean();
}

// I-80h: cleaning a crashed run's registration never drops unrelated worktrees
// whose directories are unreachable (unmounted disks).
#[test]
fn i80h_fix_keeps_unreachable_worktrees_registered() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let external = sb.path("external-wt");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "ext",
        external.to_str().unwrap(),
    ]);
    fs::rename(&external, sb.path("external-wt.unmounted")).unwrap();
    // A crashed run that left a registration inside its staging dir, no journal.
    let staging = sb.path(".parhel-staging-dead");
    fs::create_dir(&staging).unwrap();
    let orphan = staging.join("feat");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "orphaned",
        orphan.to_str().unwrap(),
    ]);
    // An immutable file makes `git worktree remove` fail, forcing the fallback path
    // (the one that used to run a repository-wide `git worktree prune`).
    fs::write(orphan.join("frozen"), "x").unwrap();
    let st = std::process::Command::new("chflags")
        .arg("uchg")
        .arg(orphan.join("frozen"))
        .status()
        .unwrap();
    assert!(st.success());

    assert_success(&parhel(&sb.env, &src.root, &["doctor", "--fix"]));
    let paths = src.worktree_paths();
    assert!(!paths.contains(&orphan), "orphan registration removed");
    assert!(
        paths.contains(&external),
        "unreachable worktree must stay registered"
    );
    assert!(!staging.exists());
}
