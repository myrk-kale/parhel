//! I-130..I-139: OS and filesystem limits.

mod common;

#[cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::fs;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
#[cfg(target_os = "macos")]
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::cow::write_blob;
#[cfg(target_os = "macos")]
use common::volume::{DiskImage, ImageFs};
use common::*;
#[cfg(target_os = "macos")]
use serial_test::serial;

#[cfg(target_os = "macos")]
fn is_immutable(p: &Path) -> bool {
    let out = Command::new("ls").arg("-lO").arg(p).output().unwrap();
    String::from_utf8_lossy(&out.stdout).contains("uchg")
}

#[cfg(target_os = "macos")]
fn chflags(p: &Path, flag: &str) {
    assert!(
        Command::new("chflags")
            .arg(flag)
            .arg(p)
            .status()
            .unwrap()
            .success()
    );
}

// I-130
#[test]
fn i130_unwritable_destination_parent() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let parent = sb.path("ro");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
    let _restore = RestoreMode(parent.clone(), 0o755);
    let before = src.snapshot();
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&parent.join("feat"), &["--new-branch", "f"]),
    );
    assert_error(&out, "permission-denied");
    assert!(
        stderr(&out).contains(parent.to_str().unwrap()),
        "{}",
        stderr(&out)
    );
    before.assert_same(&src.snapshot());
}

// I-131: volume full — whatever step fails, nothing is left behind.
// macOS only (APFS image). Linux: I-L18 in `add_linux.rs`, where a write-size
// limit stands in for a full disk.
#[cfg(target_os = "macos")]
#[test]
#[serial(hdiutil)]
fn i131_disk_full_leaves_no_trace() {
    let sb = Sandbox::new();
    let img = DiskImage::new(ImageFs::Apfs, 64);
    let src = TestRepo::init_at(&sb.env, &img.mount.join("src"));
    src.ignore(&["filler/"]);
    let before = src.snapshot();
    // Fill the volume to the last block, with shrinking chunks.
    let mut i = 0;
    for chunk in [1 << 20, 64 << 10, 4 << 10, 512] {
        loop {
            let p = img.mount.join(format!("fill-{i}"));
            i += 1;
            if fs::write(&p, vec![0xA5u8; chunk]).is_err() {
                let _ = fs::remove_file(&p);
                break;
            }
        }
    }
    let dest = img.mount.join("dest");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "full"]),
    );
    let worktree_ok = out.status.success() && TestRepo::open(&sb.env, &dest).status().is_empty();
    for j in 0..i {
        let _ = fs::remove_file(img.mount.join(format!("fill-{j}")));
    }
    // Copy-on-write needs almost no space, so success is legitimate — but only a
    // complete, clean worktree. Otherwise: a real error and no trace.
    if out.status.success() {
        assert!(
            worktree_ok,
            "succeeded on a full disk but the worktree is not clean"
        );
    } else {
        let err = stderr(&out);
        assert!(
            ["error[git-failed]", "error[clone-failed]", "error[io]"]
                .iter()
                .any(|c| err.contains(c)),
            "disk-full maps to a real failure: {err}"
        );
        before.assert_same(&src.snapshot());
        assert!(!dest.exists());
        assert_no_staging(&[&img.mount]);
    }
}

// I-132: very deep trees near PATH_MAX either clone fully or fail cleanly.
#[test]
fn i132_paths_near_path_max() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let budget = 1000usize.saturating_sub(src.root.as_os_str().len() + 20);
    let mut rel = std::path::PathBuf::from("node_modules");
    while rel.as_os_str().len() + 12 < budget {
        rel.push("dddddddddd");
    }
    src.write(rel.join("leaf.js"), "deep");
    let before = src.snapshot();
    let dest = sb.path("deep-feat-with-a-longer-name");
    let out = parhel(&sb.env, &src.root, &add_args(&dest, &["--new-branch", "d"]));
    if out.status.success() {
        assert_eq!(
            fs::read_to_string(dest.join(&rel).join("leaf.js")).unwrap(),
            "deep"
        );
    } else {
        assert_error(&out, "clone-failed");
        before.assert_same(&src.snapshot());
        assert_no_staging(&[&sb.root]);
    }
}

// I-133: APFS cannot store non-UTF-8 names -> refused before anything happens.
// macOS only: Linux stores any byte but `/` and NUL.
#[cfg(target_os = "macos")]
#[test]
fn i133_non_utf8_destination_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let before = src.snapshot();
    let dest = sb.root.join(OsStr::from_bytes(b"feat-\xff"));
    let out = parhel(
        &sb.env,
        &src.root,
        &[OsStr::new("add"), dest.as_os_str(), OsStr::new("--detach")],
    );
    assert_error(&out, "invalid-path");
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-134: APFS is normalization-insensitive: NFC request vs existing NFD dir.
// macOS only: Linux filesystems compare names byte for byte.
#[cfg(target_os = "macos")]
#[test]
fn i134_normalization_equivalent_destination_exists() {
    let sb = Sandbox::new();
    let src = project(&sb);
    fs::create_dir(sb.path("cafe\u{301}")).unwrap();
    fs::write(sb.path("cafe\u{301}/mine.txt"), "keep").unwrap();
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("caf\u{e9}"), &["--new-branch", "c"]),
    );
    assert_error(&out, "dest-exists");
    assert_eq!(
        fs::read_to_string(sb.path("cafe\u{301}/mine.txt")).unwrap(),
        "keep"
    );
}

// I-135: immutable (uchg) ignored file is cloned with its flag.
// macOS only (BSD flags); the Linux counterpart is I-L15 (read-only directories).
#[cfg(target_os = "macos")]
#[test]
fn i135_immutable_file_is_cloned() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("target/frozen.bin", "frozen");
    chflags(&src.path("target/frozen.bin"), "uchg");
    let dest = sb.path("dest");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "u"]),
    ));
    assert_eq!(
        fs::read_to_string(dest.join("target/frozen.bin")).unwrap(),
        "frozen"
    );
    assert!(is_immutable(&dest.join("target/frozen.bin")));
}

// I-136: rollback removes staging even when it contains immutable files.
// macOS only (BSD flags); the Linux counterpart is I-L16 (read-only directories).
#[cfg(target_os = "macos")]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i136_rollback_with_immutable_file() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("target/frozen.bin", "frozen");
    chflags(&src.path("target/frozen.bin"), "uchg");
    let before = src.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "u"]),
        "after_clone=error",
    );
    assert_error(&out, "failpoint");
    assert_no_staging(&[&sb.root]);
    before.assert_same(&src.snapshot());
    assert!(
        is_immutable(&src.path("target/frozen.bin")),
        "source flag untouched"
    );
}

// I-137: FIFOs and sockets (dev servers) inside ignored dirs do not break the clone.
#[test]
fn i137_fifo_and_socket_in_ignored_dir() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.mkdir("target/run");
    let fifo = src.path("target/run/pipe");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let sock_path = src.path("target/run/app.sock");
    let _sock = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
    let dest = sb.path("dest");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "s"]),
    ));
    let md = fs::symlink_metadata(dest.join("target/run/pipe")).unwrap();
    assert!(md.file_type().is_fifo());
    TestRepo::open(&sb.env, &dest).assert_clean();
}

// I-138: performance budget on a large ignored tree (30k files).
#[test]
fn i138_large_tree_within_time_budget() {
    let sb = Sandbox::new();
    let src = project(&sb);
    for d in 0..300 {
        for f in 0..100 {
            src.write(format!("node_modules/p{d}/f{f}.js"), "x");
        }
    }
    write_blob(&src.path("target/big.bin"), 64 << 20, 5);
    let start = Instant::now();
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "p"]),
    );
    let elapsed = start.elapsed();
    assert_success(&out);
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    assert!(sb.path("dest/node_modules/p299/f99.js").is_file());
}
