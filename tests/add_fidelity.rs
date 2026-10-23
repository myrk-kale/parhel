//! I-23..I-24: the clone reproduces the source faithfully.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use common::xattr;
use common::*;
use rstest::rstest;

fn add_ok(sb: &Sandbox, src: &TestRepo, dest: &Path) -> TestRepo {
    let out = parhel(&sb.env, &src.root, &add_args(dest, &["--new-branch", "f"]));
    assert_success(&out);
    TestRepo::open(&sb.env, dest)
}

// I-23a: symlinks are cloned as links, never followed.
#[test]
fn i23a_symlinks_are_preserved_not_followed() {
    let sb = Sandbox::new();
    let src = project(&sb);
    fs::write(sb.path("outside.txt"), "outside").unwrap();
    src.mkdir("real-dir");
    src.write("real-dir/f", "f");
    src.symlink("rel-link", "README.md");
    src.symlink("dir-link", "real-dir");
    src.symlink("abs-link", sb.path("outside.txt"));
    src.symlink("dangling", "does-not-exist");
    src.commit_all("links");
    // Ignored symlink pointing to a directory outside: must stay a link, not a copy.
    src.symlink("node_modules/linked-pkg", sb.root.join("home"));

    let wt = add_ok(&sb, &src, &sb.path("links"));
    wt.assert_clean();
    for (link, target) in [
        ("rel-link", Path::new("README.md").to_path_buf()),
        ("dir-link", Path::new("real-dir").to_path_buf()),
        ("abs-link", sb.path("outside.txt")),
        ("dangling", Path::new("does-not-exist").to_path_buf()),
        ("node_modules/linked-pkg", sb.root.join("home")),
    ] {
        let md = fs::symlink_metadata(wt.path(link)).unwrap();
        assert!(md.file_type().is_symlink(), "{link} must remain a symlink");
        assert_eq!(fs::read_link(wt.path(link)).unwrap(), target, "{link}");
    }
}

// I-23b: permission bits (exec, read-only) are preserved.
#[test]
fn i23b_permissions_are_preserved() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("run.sh", "#!/bin/sh\n");
    src.chmod("run.sh", 0o755);
    src.write("ro.txt", "read only\n");
    src.commit_all("modes");
    src.chmod("ro.txt", 0o444);
    src.write("target/secret.key", "k");
    src.chmod("target/secret.key", 0o600);

    let wt = add_ok(&sb, &src, &sb.path("modes"));
    let mode = |rel: &str| fs::metadata(wt.path(rel)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("run.sh"), 0o755);
    assert_eq!(mode("ro.txt"), 0o444);
    assert_eq!(mode("target/secret.key"), 0o600);
    wt.assert_clean();
}

// I-23c: empty (ignored) directories survive.
#[test]
fn i23c_empty_directories_are_preserved() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.mkdir("target/empty/deeper");
    let wt = add_ok(&sb, &src, &sb.path("empty"));
    assert!(wt.path("target/empty/deeper").is_dir());
}

// I-23d: extended attributes survive.
#[test]
fn i23d_extended_attributes_are_preserved() {
    let sb = Sandbox::new();
    let src = project(&sb);
    xattr::set(&src.path("README.md"), xattr::TEST_ATTR, b"hello").unwrap();
    let wt = add_ok(&sb, &src, &sb.path("xattr"));
    assert_eq!(
        xattr::get(&wt.path("README.md"), xattr::TEST_ATTR).unwrap(),
        Some(b"hello".to_vec())
    );
}

// I-23e: unicode (NFD) names, spaces in names and in the destination path.
#[test]
fn i23e_unicode_and_spaces() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let nfd = "cafe\u{301} menu.txt";
    src.write(nfd, "nfd\n");
    src.write("dir with spaces/file name.txt", "s\n");
    src.commit_all("names");
    fs::create_dir(sb.path("dest with spaces")).unwrap();
    let wt = add_ok(&sb, &src, &sb.path("dest with spaces/ação"));
    wt.assert_clean();
    assert_eq!(wt.read(nfd), "nfd\n");
    assert_eq!(wt.read("dir with spaces/file name.txt"), "s\n");
}

// I-23f: hard links inside the source do not couple destination and source.
#[test]
fn i23f_hardlinks_do_not_couple_source_and_destination() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("target/a.bin", "shared");
    fs::hard_link(src.path("target/a.bin"), src.path("target/b.bin")).unwrap();
    let wt = add_ok(&sb, &src, &sb.path("hard"));
    assert_eq!(wt.read("target/b.bin"), "shared");
    fs::write(wt.path("target/a.bin"), "changed in dest").unwrap();
    assert_eq!(src.read("target/a.bin"), "shared");
    assert_eq!(src.read("target/b.bin"), "shared");
    let src_ino = fs::metadata(src.path("target/a.bin")).unwrap().ino();
    assert_ne!(
        fs::metadata(wt.path("target/a.bin")).unwrap().ino(),
        src_ino
    );
}

// I-23g: attribute-driven conversions (eol) do not show up as changes.
#[test]
fn i23g_eol_attributes_leave_status_clean() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write(".gitattributes", "*.txt text eol=crlf\n*.bin binary\n");
    src.write("dos.txt", "line1\nline2\n");
    src.commit_all("attrs");
    src.git(&["rm", "-q", "--cached", "-r", "."]);
    src.git(&["reset", "-q", "--hard"]);
    src.assert_clean();
    let wt = add_ok(&sb, &src, &sb.path("eol"));
    wt.assert_clean();
    assert_eq!(
        fs::read(wt.path("dos.txt")).unwrap(),
        fs::read(src.path("dos.txt")).unwrap()
    );
}

// I-23h: modification times of cloned files are preserved (keeps build caches warm).
#[test]
fn i23h_mtimes_are_preserved() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let st = Command::new("touch")
        .args(["-t", "202001020304"])
        .arg(src.path("target/debug/app"))
        .status()
        .unwrap();
    assert!(st.success());
    let wt = add_ok(&sb, &src, &sb.path("mtime"));
    let m = |p: &Path| fs::metadata(p).unwrap().mtime();
    assert_eq!(
        m(&wt.path("target/debug/app")),
        m(&src.path("target/debug/app"))
    );
}

// I-24: sparse-checkout is refused in v1.
#[test]
fn i24_sparse_checkout_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    // The pre-`sparse-checkout` mechanism, valid on every supported git version.
    src.git(&["config", "core.sparseCheckout", "true"]);
    std::fs::write(src.git_dir().join("info/sparse-checkout"), "/src/\n").unwrap();
    src.git(&["read-tree", "-mu", "HEAD"]);
    let before = src.snapshot();
    let dest = sb.path("sparse");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "sp"]),
    );
    assert_error(&out, "sparse-checkout");
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
}

// I-23i: a private source (0700) does not produce a world-readable copy.
#[test]
fn i23i_directory_permissions_are_not_widened() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.ignore(&[".env"]);
    src.write(".env", "SECRET=1\n");
    src.chmod(".env", 0o600);
    // An ancestor of an excluded path (a nested worktree) is created by parhel.
    let nested = src.path(".worktrees/other");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "other",
        nested.to_str().unwrap(),
    ]);
    src.chmod(".worktrees", 0o700);
    src.chmod(".", 0o700);
    let wt = add_ok(&sb, &src, &sb.path("private"));
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&wt.root), 0o700, "worktree root");
    assert_eq!(mode(&wt.path(".worktrees")), 0o700, "created ancestor");
    assert_eq!(mode(&wt.path(".env")), 0o600);
    src.chmod(".", 0o755);
}

/// Makes the given directories writable again when dropped, so the sandbox can be
/// removed even when an assertion failed.
struct Unlock(Vec<std::path::PathBuf>);

impl Drop for Unlock {
    fn drop(&mut self) {
        for dir in &self.0 {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }
    }
}

// I-23j: a read-only source directory that is an ancestor of an exclusion (a nested
// worktree, a `.parhelignore` entry) is rebuilt by parhel and filled before it is
// locked: every entry arrives and the mode is the source's, for clones and copies.
#[rstest]
fn i23j_read_only_ancestors_of_exclusions_are_filled_then_locked(
    #[values(false, true)] copy: bool,
) {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write(".parhelignore", "ro/inner/skip/\n");
    src.write("ro/kept.txt", "kept\n");
    src.write("ro/inner/kept.txt", "kept too\n");
    src.commit_all("read-only tree");
    // Ignored by git (so untracked) and kept out of the copy by `.parhelignore`.
    src.ignore(&["ro/inner/skip/"]);
    src.write("ro/inner/skip/big.bin", "skipped\n");
    // Ignored files and a nested worktree below another read-only directory.
    src.write(".worktrees/notes.txt", "notes\n");
    let nested = src.path(".worktrees/other");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "other",
        nested.to_str().unwrap(),
    ]);
    let locked = ["ro/inner", "ro", ".worktrees"].map(|d| src.path(d));
    let _unlock_source = Unlock(locked.to_vec());
    for dir in &locked {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    }

    let dest = sb.path("locked");
    let _unlock_dest = Unlock(
        ["ro/inner", "ro", ".worktrees"]
            .map(|d| dest.join(d))
            .to_vec(),
    );
    let mut extra = vec!["--new-branch", "ro-copy"];
    if copy {
        extra.push("--copy");
    }
    let out = parhel(&sb.env, &src.root, &add_args(&dest, &extra));
    assert_success(&out);

    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("ro/kept.txt"), "kept\n");
    assert_eq!(wt.read("ro/inner/kept.txt"), "kept too\n");
    assert_eq!(wt.read(".worktrees/notes.txt"), "notes\n");
    assert!(!wt.exists("ro/inner/skip"), "`.parhelignore` entry copied");
    assert!(!wt.exists(".worktrees/other"), "nested worktree copied");
    let mode = |rel: &str| fs::metadata(wt.path(rel)).unwrap().permissions().mode() & 0o777;
    for rel in ["ro", "ro/inner", ".worktrees"] {
        assert_eq!(mode(rel), 0o555, "{rel}");
    }
    assert_no_staging(&[&sb.root]);
}

// I-23k: a failure after the read-only ancestors were locked still removes the
// staging directory (`remove_dir_all` makes directories writable) and leaves no trace.
#[rstest]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn i23k_rollback_removes_locked_read_only_ancestors(#[values(false, true)] copy: bool) {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write(".worktrees/notes.txt", "notes\n");
    let nested = src.path(".worktrees/other");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "other",
        nested.to_str().unwrap(),
    ]);
    let _unlock = Unlock(vec![src.path(".worktrees")]);
    src.chmod(".worktrees", 0o555);
    let before = src.snapshot();

    let mut args = add_args(&sb.path("dest"), &["--new-branch", "u"]);
    if copy {
        args.push("--copy".to_owned());
    }
    let out = parhel_with_failpoints(&sb.env, &src.root, &args, "after_clone=error");
    assert_error(&out, "failpoint");
    assert_eq!(src.branch_tip("u"), None);
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
    assert!(!sb.path("dest").exists());
}
