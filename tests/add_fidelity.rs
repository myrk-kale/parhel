//! I-23..I-24: the clone reproduces the source faithfully.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use common::*;

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
    let st = Command::new("xattr")
        .args(["-w", "com.parhel.test", "hello"])
        .arg(src.path("README.md"))
        .status()
        .unwrap();
    assert!(st.success());
    let wt = add_ok(&sb, &src, &sb.path("xattr"));
    let out = Command::new("xattr")
        .args(["-p", "com.parhel.test"])
        .arg(wt.path("README.md"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
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
    src.git(&["sparse-checkout", "set", "--no-cone", "/src/"]);
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
