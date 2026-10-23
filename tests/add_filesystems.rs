//! I-50..I-53: real volume boundaries via throwaway disk images.

mod common;

use common::volume::{DiskImage, ImageFs};
use common::*;
use serial_test::serial;

// I-50
#[test]
#[serial(hdiutil)]
fn i50_destination_on_another_apfs_volume_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let other = DiskImage::new(ImageFs::Apfs, 128);
    let before = src.snapshot();
    let dest = other.mount.join("dest");
    let out = parhel(&sb.env, &src.root, &add_args(&dest, &["--new-branch", "x"]));
    assert_error(&out, "cross-volume");
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root, &other.mount]);
}

// I-51a
#[test]
#[serial(hdiutil)]
fn i51a_source_on_hfs_is_refused() {
    let sb = Sandbox::new();
    let hfs = DiskImage::new(ImageFs::Hfs, 128);
    let src = TestRepo::init_at(&sb.env, &hfs.mount.join("src"));
    let before = src.snapshot();
    let dest = hfs.mount.join("dest");
    let out = parhel(&sb.env, &src.root, &add_args(&dest, &["--new-branch", "x"]));
    assert_error(&out, "not-cow");
    assert!(
        stderr(&out).contains("hfs"),
        "names the filesystem: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("--fallback"),
        "suggests the escape hatch: {}",
        stderr(&out)
    );
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
}

// I-51b
#[test]
#[serial(hdiutil)]
fn i51b_destination_on_hfs_is_refused_as_not_cow() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let hfs = DiskImage::new(ImageFs::Hfs, 128);
    let before = src.snapshot();
    let dest = hfs.mount.join("dest");
    let out = parhel(&sb.env, &src.root, &add_args(&dest, &["--new-branch", "x"]));
    assert_error(&out, "not-cow");
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&hfs.mount]);
}

// I-52a: --fallback produces a regular worktree with a warning.
#[test]
#[serial(hdiutil)]
fn i52a_fallback_creates_plain_worktree() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let hfs = DiskImage::new(ImageFs::Hfs, 128);
    let dest = hfs.mount.join("dest");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "fb", "--fallback"]),
    );
    assert_success(&out);
    assert!(stderr(&out).contains("warning"), "{}", stderr(&out));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.current_branch().as_deref(), Some("fb"));
    assert!(wt.exists("src/main.rs"));
    assert!(!wt.exists("target"), "plain checkout has no ignored files");
    assert!(!wt.exists("node_modules"));
}

// I-52b: --fallback is inert when copy-on-write works.
#[test]
fn i52b_fallback_flag_is_inert_on_apfs() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("dest");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "fb", "--fallback"]),
    );
    assert_success(&out);
    assert!(!stderr(&out).contains("warning"), "{}", stderr(&out));
    assert!(
        TestRepo::open(&sb.env, &dest).exists("target/debug/app"),
        "copy-on-write was used"
    );
}

// I-53: case-sensitive APFS.
#[test]
#[serial(hdiutil)]
fn i53_case_sensitive_apfs_volume() {
    let sb = Sandbox::new();
    let cs = DiskImage::new(ImageFs::ApfsCaseSensitive, 128);
    let src = TestRepo::init_at(&sb.env, &cs.mount.join("src"));
    src.write("Readme.md", "mixed\n");
    src.write("README.MD", "upper\n");
    src.commit_all("case");
    let dest = cs.mount.join("dest");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "cs"]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("Readme.md"), "mixed\n");
    assert_eq!(wt.read("README.MD"), "upper\n");
}
