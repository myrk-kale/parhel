//! I-L01..I-L48: Linux filesystems (btrfs and XFS reflink, ext4 without).
//!
//! The filesystems come from the environment (`PARHEL_TEST_BTRFS`, `PARHEL_TEST_XFS`,
//! `PARHEL_TEST_EXT4`, set up by `scripts/linux-test.sh` and CI). A missing variable
//! skips the test with a notice, except under `CI` / `PARHEL_REQUIRE_FS=1`, where it
//! fails. See `common::volume::test_mount`.
#![cfg(target_os = "linux")]

mod common;

use std::fs::{self, File};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use common::cow::{is_independent_copy, shares_storage, write_blob};
use common::volume::{MountedFs, test_mount};
use common::*;
use rstest::rstest;

const MIB: usize = 1 << 20;
const NANOS: u32 = 123_456_789;

/// A sandbox on `fs` (`None`: skip, see the module docs).
fn sandbox_on(fs: MountedFs) -> Option<Sandbox> {
    test_mount(fs).map(|dir| Sandbox::new_in(&dir))
}

/// A project (tracked code, ignored `target/` and `node_modules/`) with a few big
/// files, so sharing or copying can be told apart.
fn project_with_blobs(sb: &Sandbox) -> TestRepo {
    let src = project(sb);
    write_blob(&src.path("assets/tracked.bin"), 2 * MIB, 1);
    src.commit_all("tracked blob");
    write_blob(&src.path("target/ignored.bin"), 4 * MIB, 2);
    write_blob(&src.path("node_modules/pkg/big.bin"), MIB, 3);
    src
}

const BLOBS: [&str; 3] = [
    "assets/tracked.bin",
    "target/ignored.bin",
    "node_modules/pkg/big.bin",
];

fn add(src: &TestRepo, sb: &Sandbox, dest: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["--new-branch", "f"];
    args.extend(extra);
    parhel(&sb.env, &src.root, &add_args(dest, &args))
}

fn assert_same_content(src: &TestRepo, dest: &Path) {
    for rel in BLOBS {
        assert_eq!(
            fs::read(src.path(rel)).unwrap(),
            fs::read(dest.join(rel)).unwrap(),
            "{rel}"
        );
    }
}

fn assert_all_shared(src: &TestRepo, dest: &Path) {
    for rel in BLOBS {
        assert!(
            shares_storage(&src.path(rel), &dest.join(rel)),
            "{rel} was copied, not cloned"
        );
    }
}

fn assert_all_independent(src: &TestRepo, dest: &Path) {
    for rel in BLOBS {
        assert!(
            is_independent_copy(&src.path(rel), &dest.join(rel)),
            "{rel} shares storage with the source"
        );
    }
}

// ------------------------------------------------------------ reflink filesystems

fn reflink_add_works(fs: MountedFs) {
    let Some(sb) = sandbox_on(fs) else {
        return;
    };
    let src = project_with_blobs(&sb);
    let dest = sb.path("cow");
    let out = add(&src, &sb, &dest, &[]);
    assert_success(&out);
    assert!(
        stdout(&out).contains("copy-on-write worktree"),
        "{}",
        stdout(&out)
    );
    assert!(!stderr(&out).contains("warning"), "{}", stderr(&out));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.current_branch().as_deref(), Some("f"));
    assert_eq!(wt.read("target/debug/app"), "binary");
    assert_eq!(
        wt.read("node_modules/pkg/index.js"),
        "module.exports = 1;\n"
    );
    assert_same_content(&src, &dest);
    assert_all_shared(&src, &dest);
    assert_no_staging(&[&sb.root]);
}

// I-L01
#[test]
fn il01_btrfs_add_clones_with_proven_sharing() {
    reflink_add_works(MountedFs::Btrfs);
}

// I-L02
#[test]
fn il02_xfs_add_clones_with_proven_sharing() {
    reflink_add_works(MountedFs::Xfs);
}

// I-L03: writes diverge, so the sharing is copy-on-write and not a hard link.
#[rstest]
fn il03_writes_diverge_on_reflink_filesystems(
    #[values(MountedFs::Btrfs, MountedFs::Xfs)] fs: MountedFs,
) {
    let Some(sb) = sandbox_on(fs) else {
        return;
    };
    let src = project_with_blobs(&sb);
    let original = fs::read(src.path("target/ignored.bin")).unwrap();
    let dest = sb.path("cow");
    assert_success(&add(&src, &sb, &dest, &[]));
    fs::write(dest.join("target/ignored.bin"), b"rewritten").unwrap();
    assert_eq!(fs::read(src.path("target/ignored.bin")).unwrap(), original);
    assert_ne!(
        fs::metadata(dest.join("target/ignored.bin")).unwrap().ino(),
        fs::metadata(src.path("target/ignored.bin")).unwrap().ino()
    );
}

// ------------------------------------------------------------ ext4: no reflink

// I-L04: refused before anything happens, and the message says what to do.
#[test]
fn il04_ext4_add_is_refused_without_touching_anything() {
    let Some(sb) = sandbox_on(MountedFs::Ext4) else {
        return;
    };
    let src = project_with_blobs(&sb);
    let before = src.snapshot();
    let dest = sb.path("dest");
    let out = add(&src, &sb, &dest, &[]);
    assert_error(&out, "not-cow");
    let err = stderr(&out);
    assert!(err.contains("ext4"), "names the filesystem: {err}");
    assert!(err.contains("--copy"), "offers the full copy: {err}");
    assert!(
        err.contains("--fallback"),
        "offers the plain worktree: {err}"
    );
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-L05: `--copy` works on ext4: equal files, not shared.
#[test]
fn il05_ext4_copy_succeeds_with_independent_files() {
    let Some(sb) = sandbox_on(MountedFs::Ext4) else {
        return;
    };
    let src = project_with_blobs(&sb);
    let dest = sb.path("copied");
    let out = add(&src, &sb, &dest, &["--copy"]);
    assert_success(&out);
    assert!(
        stdout(&out).contains("full-copy worktree"),
        "{}",
        stdout(&out)
    );
    assert!(!stderr(&out).contains("warning"), "{}", stderr(&out));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("target/debug/app"), "binary");
    assert_same_content(&src, &dest);
    assert_all_independent(&src, &dest);
    assert_no_staging(&[&sb.root]);
}

// I-L06: `--fallback` gives a plain worktree on ext4.
#[test]
fn il06_ext4_fallback_creates_a_plain_worktree() {
    let Some(sb) = sandbox_on(MountedFs::Ext4) else {
        return;
    };
    let src = project_with_blobs(&sb);
    let dest = sb.path("plain");
    let out = add(&src, &sb, &dest, &["--fallback"]);
    assert_success(&out);
    assert!(stderr(&out).contains("warning"), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("regular worktree"),
        "{}",
        stdout(&out)
    );
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.current_branch().as_deref(), Some("f"));
    assert!(wt.exists("src/main.rs"));
    assert!(
        !wt.exists("target"),
        "a plain checkout has no ignored files"
    );
    assert!(!wt.exists("node_modules"));
}

// I-L07: the plain `git worktree add` runs git's hooks (the clone path does not).
#[test]
fn il07_ext4_fallback_runs_post_checkout() {
    let Some(sb) = sandbox_on(MountedFs::Ext4) else {
        return;
    };
    let src = project(&sb);
    let marker = sb.path("hook-ran");
    let hook = src.common_dir().join("hooks/post-checkout");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        &hook,
        format!("#!/bin/sh\necho ran >> '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    assert_success(&add(&src, &sb, &sb.path("feat"), &["--fallback"]));
    assert!(marker.exists(), "plain worktree add runs post-checkout");
}

// I-L08: a destination on ext4 with the source on btrfs is `not-cow` (the
// destination is named), a source on ext4 with the destination on btrfs, too.
#[test]
fn il08_not_cow_names_the_volume_that_lacks_it() {
    let (Some(cow), Some(plain)) = (test_mount(MountedFs::Btrfs), test_mount(MountedFs::Ext4))
    else {
        return;
    };
    let (cow_sb, plain_sb) = (Sandbox::new_in(&cow), Sandbox::new_in(&plain));

    let src = project(&cow_sb);
    let before = src.snapshot();
    let dest = plain_sb.path("dest");
    let out = add(&src, &cow_sb, &dest, &[]);
    assert_error(&out, "not-cow");
    assert!(stderr(&out).contains(dest.parent().unwrap().to_str().unwrap()));
    assert!(stderr(&out).contains("ext4"), "{}", stderr(&out));
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&cow_sb.root, &plain_sb.root]);

    let src = project(&plain_sb);
    let before = src.snapshot();
    let dest = cow_sb.path("dest");
    let out = add(&src, &plain_sb, &dest, &[]);
    assert_error(&out, "not-cow");
    assert!(stderr(&out).contains(src.root.to_str().unwrap()));
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&cow_sb.root, &plain_sb.root]);
}

// I-L09: `--copy` bridges that: ext4 to btrfs and btrfs to ext4.
#[rstest]
fn il09_copy_between_ext4_and_a_cow_volume(
    #[values(MountedFs::Btrfs, MountedFs::Xfs)] cow_fs: MountedFs,
    #[values(true, false)] cow_is_source: bool,
) {
    let (Some(cow), Some(plain)) = (test_mount(cow_fs), test_mount(MountedFs::Ext4)) else {
        return;
    };
    let (cow_sb, plain_sb) = (Sandbox::new_in(&cow), Sandbox::new_in(&plain));
    let (src_sb, dest_sb) = if cow_is_source {
        (&cow_sb, &plain_sb)
    } else {
        (&plain_sb, &cow_sb)
    };
    let src = project_with_blobs(src_sb);
    let dest = dest_sb.path("copied");
    assert_success(&add(&src, src_sb, &dest, &["--copy"]));
    let wt = TestRepo::open(&src_sb.env, &dest);
    wt.assert_clean();
    assert_same_content(&src, &dest);
    assert_all_independent(&src, &dest);
    assert_no_staging(&[&cow_sb.root, &plain_sb.root]);
}

// ------------------------------------------------------------ cross-volume

// I-L10: btrfs -> XFS cannot be cloned: refused, nothing changes anywhere.
#[test]
fn il10_cross_volume_is_refused() {
    let (Some(a), Some(b)) = (test_mount(MountedFs::Btrfs), test_mount(MountedFs::Xfs)) else {
        return;
    };
    let (sb_a, sb_b) = (Sandbox::new_in(&a), Sandbox::new_in(&b));
    let src = project_with_blobs(&sb_a);
    let before = src.snapshot();
    let dest = sb_b.path("dest");
    let out = add(&src, &sb_a, &dest, &[]);
    assert_error(&out, "cross-volume");
    let err = stderr(&out);
    assert!(
        err.contains("--copy") && err.contains("--fallback"),
        "{err}"
    );
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb_a.root, &sb_b.root]);
}

// I-L11: ... and `--copy` crosses volumes.
#[test]
fn il11_copy_crosses_volumes() {
    let (Some(a), Some(b)) = (test_mount(MountedFs::Btrfs), test_mount(MountedFs::Xfs)) else {
        return;
    };
    let (sb_a, sb_b) = (Sandbox::new_in(&a), Sandbox::new_in(&b));
    let src = project_with_blobs(&sb_a);
    let before = src.snapshot();
    let dest = sb_b.path("copied");
    let out = add(&src, &sb_a, &dest, &["--copy"]);
    assert_success(&out);
    let wt = TestRepo::open(&sb_a.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("target/debug/app"), "binary");
    assert_same_content(&src, &dest);
    assert_all_independent(&src, &dest);
    assert_ne!(
        fs::metadata(&dest).unwrap().dev(),
        fs::metadata(&src.root).unwrap().dev()
    );
    assert_eq!(before.tree, src.snapshot().tree, "source untouched");
    assert_no_staging(&[&sb_a.root, &sb_b.root]);
}

// I-L12: `--fallback` also covers a cross-volume destination.
#[test]
fn il12_fallback_covers_cross_volume() {
    let (Some(a), Some(b)) = (test_mount(MountedFs::Btrfs), test_mount(MountedFs::Xfs)) else {
        return;
    };
    let (sb_a, sb_b) = (Sandbox::new_in(&a), Sandbox::new_in(&b));
    let src = project(&sb_a);
    let dest = sb_b.path("plain");
    let out = add(&src, &sb_a, &dest, &["--fallback"]);
    assert_success(&out);
    assert!(stderr(&out).contains("warning"), "{}", stderr(&out));
    assert!(!TestRepo::open(&sb_a.env, &dest).exists("target"));
}

// ------------------------------------------------------------ doctor

// I-L13: doctor on a healthy reflink volume.
#[rstest]
fn il13_doctor_is_quiet_on_reflink_volumes(
    #[values(MountedFs::Btrfs, MountedFs::Xfs)] fs: MountedFs,
) {
    let Some(sb) = sandbox_on(fs) else {
        return;
    };
    let src = project(&sb);
    let out = parhel(&sb.env, &src.root, &["doctor"]);
    assert_success(&out);
    assert!(stdout(&out).contains("no issues"), "{}", stdout(&out));
}

// I-L14: doctor on ext4 reports `not-cow` and names the filesystem.
#[test]
fn il14_doctor_reports_ext4_as_not_cow() {
    let Some(sb) = sandbox_on(MountedFs::Ext4) else {
        return;
    };
    let src = TestRepo::init_at(&sb.env, &sb.path("src"));
    let out = parhel(&sb.env, &src.root, &["doctor"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("not-cow") && stdout(&out).contains("ext4"),
        "{}",
        stdout(&out)
    );
}

// ------------------------------------------------------------ read-only directories

fn project_with_read_only_dir(sb: &Sandbox) -> TestRepo {
    let src = project(sb);
    src.write("target/ro/inner/file.txt", "inside");
    src.write("target/ro/top.txt", "top");
    src.chmod("target/ro/inner", 0o555);
    src.chmod("target/ro", 0o555);
    src
}

// I-L15: read-only directories are filled and then locked, in both modes.
#[rstest]
#[case::reflink_btrfs(MountedFs::Btrfs, false)]
#[case::reflink_xfs(MountedFs::Xfs, false)]
#[case::copy_btrfs(MountedFs::Btrfs, true)]
#[case::copy_xfs(MountedFs::Xfs, true)]
#[case::copy_ext4(MountedFs::Ext4, true)]
fn il15_read_only_directories_are_reproduced(#[case] fs: MountedFs, #[case] copy: bool) {
    let Some(sb) = sandbox_on(fs) else {
        return;
    };
    let src = project_with_read_only_dir(&sb);
    let dest = sb.path("dest");
    assert_success(&add(&src, &sb, &dest, if copy { &["--copy"] } else { &[] }));
    let mode = |rel: &str| fs::metadata(dest.join(rel)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("target/ro"), 0o555);
    assert_eq!(mode("target/ro/inner"), 0o555);
    assert_eq!(
        fs::read_to_string(dest.join("target/ro/inner/file.txt")).unwrap(),
        "inside"
    );
    TestRepo::open(&sb.env, &dest).assert_clean();
}

// I-L16: a failed run that cloned read-only directories still removes everything.
#[rstest]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn il16_rollback_removes_read_only_directories(#[values(false, true)] copy: bool) {
    let Some(sb) = sandbox_on(MountedFs::Btrfs) else {
        return;
    };
    let src = project_with_read_only_dir(&sb);
    let before = src.snapshot();
    let mut args = vec!["--new-branch", "u"];
    if copy {
        args.push("--copy");
    }
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &args),
        "after_clone=error",
    );
    assert_error(&out, "failpoint");
    assert_no_staging(&[&sb.root]);
    before.assert_same(&src.snapshot());
    assert!(!sb.path("dest").exists());
}

// I-L17: even when git itself cannot delete the staged worktree (read-only
// directories), rollback falls back to parhel's own cleanup.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn il17_rollback_survives_git_failing_to_remove_the_worktree() {
    let Some(sb) = sandbox_on(MountedFs::Btrfs) else {
        return;
    };
    let src = project_with_read_only_dir(&sb);
    let other = sb.path("elsewhere");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "ext",
        other.to_str().unwrap(),
    ]);
    fs::rename(&other, sb.path("elsewhere.unmounted")).unwrap();
    let before = src.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "d"]),
        "after_clone=error",
    );
    assert_error(&out, "failpoint");
    assert!(
        src.worktree_paths().contains(&other),
        "the unreachable worktree lost its registration"
    );
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// ------------------------------------------------------------ write failures

/// `git-parhel add ...` under a file-size limit (`ulimit -f`, in 512-byte blocks,
/// SIGXFSZ ignored so writes fail with EFBIG instead of killing the process): a
/// stand-in for a full disk that does not need to fill one.
fn add_with_file_size_limit(sb: &Sandbox, src: &TestRepo, dest: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["--new-branch".to_owned(), "f".to_owned()];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let mut cmd = Command::new("sh");
    cmd.current_dir(&src.root)
        .arg("-c")
        .arg(r#"trap '' 25; ulimit -f 128; exec "$0" "$@""#)
        .arg(parhel_bin())
        .args(add_args(
            dest,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        ));
    sb.env.apply(&mut cmd);
    cmd.output().unwrap()
}

// I-L18: a copy that cannot write its data fails cleanly: real error, no trace.
#[test]
fn il18_copy_that_runs_out_of_room_leaves_no_trace() {
    let Some(sb) = sandbox_on(MountedFs::Btrfs) else {
        return;
    };
    let src = project_with_blobs(&sb);
    let before = src.snapshot();
    let dest = sb.path("copied");
    let out = add_with_file_size_limit(&sb, &src, &dest, &["--copy"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        ["error[clone-failed]", "error[io]"]
            .iter()
            .any(|c| err.contains(c)),
        "a failed copy is a real error: {err}"
    );
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
    let probe = sb.path("probe");
    src.git(&["worktree", "add", "-q", "-b", "f", probe.to_str().unwrap()]);
}

// ------------------------------------------------------------ fidelity (both modes)

/// Source/destination pairs for the fidelity tests: a reflink on btrfs and XFS, a
/// full copy on all three filesystems.
fn fidelity_cases() -> Vec<(MountedFs, bool)> {
    vec![
        (MountedFs::Btrfs, false),
        (MountedFs::Xfs, false),
        (MountedFs::Btrfs, true),
        (MountedFs::Xfs, true),
        (MountedFs::Ext4, true),
    ]
}

/// Run `body(sandbox, source, destination)` after `git parhel add` in each mode on
/// each filesystem, with `prepare` building the source first.
fn fidelity(prepare: impl Fn(&Sandbox, &TestRepo), check: impl Fn(&Sandbox, &TestRepo, &Path)) {
    for (fs, copy) in fidelity_cases() {
        let Some(sb) = sandbox_on(fs) else {
            continue;
        };
        let src = project(&sb);
        prepare(&sb, &src);
        let dest = sb.path("dest");
        let out = add(&src, &sb, &dest, if copy { &["--copy"] } else { &[] });
        assert_success(&out);
        eprintln!("checking {fs:?} copy={copy}");
        check(&sb, &src, &dest);
    }
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

// I-L30
#[test]
fn il30_modes_are_preserved() {
    fidelity(
        |_, src| {
            src.write("target/run.sh", "#!/bin/sh\n");
            src.chmod("target/run.sh", 0o755);
            src.write("target/ro.txt", "ro");
            src.chmod("target/ro.txt", 0o444);
            src.write("target/secret.key", "k");
            src.chmod("target/secret.key", 0o600);
            src.write("target/odd.bin", "o");
            src.chmod("target/odd.bin", 0o641);
            src.mkdir("target/private");
            src.chmod("target/private", 0o700);
            src.mkdir("target/group");
            src.chmod("target/group", 0o2750);
        },
        |_, src, dest| {
            for rel in [
                "target/run.sh",
                "target/ro.txt",
                "target/secret.key",
                "target/odd.bin",
                "target/private",
                "target/group",
            ] {
                assert_eq!(mode_of(&dest.join(rel)), mode_of(&src.path(rel)), "{rel}");
            }
            assert_eq!(mode_of(&dest.join("target/run.sh")) & 0o777, 0o755);
            assert_eq!(mode_of(&dest.join("target/ro.txt")) & 0o777, 0o444);
            TestRepo::open(&src.env, dest).assert_clean();
        },
    );
}

// I-L31: symlinks are recreated as links, whatever they point at, and never followed.
#[test]
fn il31_symlinks_are_preserved_and_never_followed() {
    fidelity(
        |sb, src| {
            fs::create_dir_all(sb.path("outside/dir")).unwrap();
            fs::write(sb.path("outside/dir/SECRET.txt"), "outside").unwrap();
            fs::write(sb.path("outside/file.txt"), "outside file").unwrap();
            src.write("target/real/f.txt", "f");
            src.symlink("target/abs-file", sb.path("outside/file.txt"));
            src.symlink("target/abs-dir", sb.path("outside/dir"));
            src.symlink("target/rel-file", "real/f.txt");
            src.symlink("target/rel-dir", "real");
            src.symlink("target/dangling", "does/not/exist");
            src.symlink("target/up", "../src/main.rs");
        },
        |sb, src, dest| {
            for (link, target) in [
                ("target/abs-file", sb.path("outside/file.txt")),
                ("target/abs-dir", sb.path("outside/dir")),
                ("target/rel-file", PathBuf::from("real/f.txt")),
                ("target/rel-dir", PathBuf::from("real")),
                ("target/dangling", PathBuf::from("does/not/exist")),
                ("target/up", PathBuf::from("../src/main.rs")),
            ] {
                let md = fs::symlink_metadata(dest.join(link)).unwrap();
                assert!(md.file_type().is_symlink(), "{link} is not a link");
                assert_eq!(fs::read_link(dest.join(link)).unwrap(), target, "{link}");
            }
            // The directories behind the links were not copied into the worktree.
            assert!(
                !walk_names(dest).iter().any(|n| n == "SECRET.txt"),
                "a link was followed"
            );
            assert_eq!(
                walk_names(dest).iter().filter(|n| *n == "f.txt").count(),
                1,
                "`target/rel-dir` was followed"
            );
            TestRepo::open(&src.env, dest).assert_clean();
        },
    );
}

/// Names of every entry under `root` (not following symlinks), `.git` excluded.
fn walk_names(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == ".git" {
                continue;
            }
            out.push(entry.file_name().to_string_lossy().into_owned());
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            }
        }
    }
    out
}

fn set_mtime(path: &Path, secs: u64) {
    File::open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::new(secs, NANOS))
        .unwrap();
}

fn mtime(path: &Path) -> Duration {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
}

// I-L32: file and directory mtimes, to the nanosecond: build caches rely on them.
#[test]
fn il32_mtimes_are_preserved_to_the_nanosecond() {
    fidelity(
        |_, src| {
            src.write("target/debug/deps/lib.rlib", "rlib");
            src.write("target/debug/deps/lib.d", "d");
            src.write("node_modules/pkg/index.js", "js");
            for (i, rel) in [
                "target/debug/deps/lib.rlib",
                "target/debug/deps/lib.d",
                "node_modules/pkg/index.js",
            ]
            .iter()
            .enumerate()
            {
                set_mtime(&src.path(rel), 1_577_934_245 + i as u64);
            }
            // Directories last: creating the files above changed them.
            for (i, rel) in [
                "target/debug/deps",
                "target/debug",
                "target",
                "node_modules/pkg",
                "node_modules",
            ]
            .iter()
            .enumerate()
            {
                set_mtime(&src.path(rel), 1_500_000_000 + i as u64);
            }
        },
        |_, src, dest| {
            for rel in [
                "target/debug/deps/lib.rlib",
                "target/debug/deps/lib.d",
                "node_modules/pkg/index.js",
                "target/debug/deps",
                "target/debug",
                "target",
                "node_modules/pkg",
                "node_modules",
            ] {
                let want = mtime(&src.path(rel));
                assert_eq!(want.subsec_nanos(), NANOS, "fixture {rel}");
                assert_eq!(mtime(&dest.join(rel)), want, "{rel}");
            }
        },
    );
}

// I-L33: user extended attributes, on files and on directories.
#[test]
fn il33_user_xattrs_are_preserved() {
    fidelity(
        |_, src| {
            src.write("target/tagged.bin", "t");
            src.mkdir("target/tagged-dir");
            xattr::set(&src.path("target/tagged.bin"), "user.parhel.one", b"1").unwrap();
            xattr::set(
                &src.path("target/tagged.bin"),
                "user.parhel.two",
                b"two words",
            )
            .unwrap();
            xattr::set(&src.path("target/tagged-dir"), "user.parhel.dir", b"d").unwrap();
            xattr::set(&src.path("README.md"), xattr::TEST_ATTR, b"tracked").unwrap();
        },
        |_, src, dest| {
            for rel in ["target/tagged.bin", "target/tagged-dir"] {
                let want = xattr::user_attrs(&src.path(rel));
                assert_eq!(
                    want.len(),
                    if rel.ends_with("bin") { 2 } else { 1 },
                    "fixture"
                );
                assert_eq!(xattr::user_attrs(&dest.join(rel)), want, "{rel}");
            }
            // Tracked files are cloned along with everything else.
            assert_eq!(
                xattr::get(&dest.join("README.md"), xattr::TEST_ATTR).unwrap(),
                Some(b"tracked".to_vec())
            );
        },
    );
}

// I-L34: FIFOs are recreated; sockets are skipped without failing the run.
#[test]
fn il34_fifos_are_kept_and_sockets_skipped() {
    use std::os::unix::net::UnixListener;
    for (fs_kind, copy) in fidelity_cases() {
        let Some(sb) = sandbox_on(fs_kind) else {
            continue;
        };
        let src = project(&sb);
        src.mkdir("target/run");
        assert!(
            Command::new("mkfifo")
                .args(["-m", "640"])
                .arg(src.path("target/run/pipe"))
                .status()
                .unwrap()
                .success()
        );
        let _socket = UnixListener::bind(src.path("target/run/app.sock")).unwrap();
        let dest = sb.path("dest");
        let out = add(&src, &sb, &dest, if copy { &["--copy"] } else { &[] });
        assert_success(&out);
        let md = fs::symlink_metadata(dest.join("target/run/pipe")).unwrap();
        assert!(md.file_type().is_fifo(), "{fs_kind:?} copy={copy}");
        assert_eq!(md.permissions().mode() & 0o777, 0o640);
        assert!(
            fs::symlink_metadata(dest.join("target/run/app.sock")).is_err(),
            "sockets are not copied"
        );
        TestRepo::open(&sb.env, &dest).assert_clean();
    }
}

// I-L35: empty directories, deeply nested.
#[test]
fn il35_empty_directories_are_preserved() {
    fidelity(
        |_, src| {
            src.mkdir("target/empty/deeper/still");
            src.mkdir("node_modules/.cache");
        },
        |_, _, dest| {
            assert!(dest.join("target/empty/deeper/still").is_dir());
            assert!(dest.join("node_modules/.cache").is_dir());
            assert_eq!(
                fs::read_dir(dest.join("target/empty/deeper/still"))
                    .unwrap()
                    .count(),
                0
            );
        },
    );
}

// I-L36: names Linux treats as plain bytes: NFC and NFD forms are two entries,
// spaces, non-ASCII destination.
#[test]
fn il36_non_ascii_names_and_spaces() {
    fidelity(
        |_, src| {
            src.write("target/caf\u{e9}.txt", "nfc");
            src.write("target/cafe\u{301}.txt", "nfd");
            src.write("target/dir with spaces/file name.txt", "s");
            src.write("target/\u{65e5}\u{672c}\u{8a9e}/\u{540d}\u{524d}.txt", "ja");
            src.write("target/emoji-\u{1f980}.txt", "crab");
            src.write("target/-leading-dash", "d");
        },
        |_, src, dest| {
            assert_eq!(
                fs::read_to_string(dest.join("target/caf\u{e9}.txt")).unwrap(),
                "nfc"
            );
            assert_eq!(
                fs::read_to_string(dest.join("target/cafe\u{301}.txt")).unwrap(),
                "nfd"
            );
            assert_eq!(
                fs::read_to_string(dest.join("target/dir with spaces/file name.txt")).unwrap(),
                "s"
            );
            assert_eq!(
                fs::read_to_string(
                    dest.join("target/\u{65e5}\u{672c}\u{8a9e}/\u{540d}\u{524d}.txt")
                )
                .unwrap(),
                "ja"
            );
            assert_eq!(
                fs::read_to_string(dest.join("target/emoji-\u{1f980}.txt")).unwrap(),
                "crab"
            );
            assert_eq!(
                fs::read_to_string(dest.join("target/-leading-dash")).unwrap(),
                "d"
            );
            TestRepo::open(&src.env, dest).assert_clean();
        },
    );
}

// I-L37: a non-ASCII destination path with spaces.
#[test]
fn il37_destination_with_spaces_and_accents() {
    for (fs_kind, copy) in fidelity_cases() {
        let Some(sb) = sandbox_on(fs_kind) else {
            continue;
        };
        let src = project(&sb);
        fs::create_dir(sb.path("dest with spaces")).unwrap();
        let dest = sb.path("dest with spaces/a\u{e7}\u{e3}o");
        assert_success(&add(&src, &sb, &dest, if copy { &["--copy"] } else { &[] }));
        let wt = TestRepo::open(&sb.env, &dest);
        wt.assert_clean();
        assert_eq!(wt.read("target/debug/app"), "binary");
    }
}

// I-L38: hard links in the source become independent files.
#[test]
fn il38_hard_links_do_not_couple_source_and_destination() {
    fidelity(
        |_, src| {
            src.write("target/a.bin", "shared");
            fs::hard_link(src.path("target/a.bin"), src.path("target/b.bin")).unwrap();
        },
        |_, src, dest| {
            assert_eq!(
                fs::read_to_string(dest.join("target/b.bin")).unwrap(),
                "shared"
            );
            fs::write(dest.join("target/a.bin"), "changed in dest").unwrap();
            assert_eq!(src.read("target/a.bin"), "shared");
            assert_eq!(src.read("target/b.bin"), "shared");
            assert_eq!(
                fs::read_to_string(dest.join("target/b.bin")).unwrap(),
                "shared"
            );
            assert_ne!(
                fs::metadata(dest.join("target/a.bin")).unwrap().ino(),
                fs::metadata(src.path("target/a.bin")).unwrap().ino()
            );
        },
    );
}

// I-L39: sparse files read back identically.
#[test]
fn il39_sparse_files_are_equal() {
    fidelity(
        |_, src| {
            fs::create_dir_all(src.path("target")).unwrap();
            let f = File::create(src.path("target/sparse.bin")).unwrap();
            f.set_len(8 << 20).unwrap();
            f.write_all_at(b"head", 0).unwrap();
            f.write_all_at(b"middle", 4 << 20).unwrap();
            f.write_all_at(b"tail", (8 << 20) - 4).unwrap();
            f.sync_all().unwrap();
        },
        |_, src, dest| {
            let (a, b) = (
                src.path("target/sparse.bin"),
                dest.join("target/sparse.bin"),
            );
            assert_eq!(
                fs::metadata(&a).unwrap().len(),
                fs::metadata(&b).unwrap().len()
            );
            assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
        },
    );
}

// I-L40: the whole worktree equals the source entry by entry: kinds, modes,
// contents, link targets (nothing but `.git` differs).
#[test]
fn il40_worktree_equals_source_entry_by_entry() {
    fidelity(
        |sb, src| {
            write_blob(&src.path("target/big.bin"), 3 * MIB, 77);
            write_blob(&src.path("node_modules/a/b/c/d.bin"), 100_000, 78);
            src.write("target/run.sh", "#!/bin/sh\n");
            src.chmod("target/run.sh", 0o755);
            src.write("target/zero", "");
            src.mkdir("target/empty");
            src.symlink("target/link", "run.sh");
            src.symlink("target/outside", sb.path("home"));
            src.write("docs/caf\u{e9}.md", "docs");
            src.commit_all("docs");
        },
        |_, src, dest| {
            let want = tree_digest(&src.root);
            let got = tree_digest(dest);
            let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
            let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "missing {missing:?}, extra {extra:?}"
            );
            for (path, desc) in &want {
                assert_eq!(&got[path], desc, "{}", path.display());
            }
        },
    );
}

// ------------------------------------------------------------ races and layouts

// I-L41: RENAME_NOREPLACE is what keeps an existing destination safe: a directory
// that appears while the worktree is being assembled is never replaced (the
// platform-neutral race suite, `add_race.rs`, runs unchanged on Linux; this one
// pins the empty-directory case, where plain rename(2) would succeed).
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[rstest]
fn il41_empty_directory_created_during_assembly_is_not_replaced(#[values(false, true)] copy: bool) {
    let Some(sb) = sandbox_on(MountedFs::Btrfs) else {
        return;
    };
    let src = project(&sb);
    let dest = sb.path("dest");
    let barrier = Barrier::new(&sb.root, "race");
    let mut args = vec!["--new-branch", "r"];
    if copy {
        args.push("--copy");
    }
    let mut child = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &args),
        Some(&barrier.spec("before_rename")),
    );
    barrier.wait_reached(&mut child);
    fs::create_dir(&dest).unwrap();
    barrier.release();
    let out = finish(child);
    assert_error(&out, "dest-exists");
    assert!(dest.is_dir());
    assert_eq!(
        fs::read_dir(&dest).unwrap().count(),
        0,
        "still the empty dir"
    );
    assert_no_staging(&[&sb.root]);
    assert_eq!(src.branch_tip("r"), None);
}

// I-L42: chained worktrees on a reflink volume share with each other.
#[test]
fn il42_chained_worktrees_keep_sharing() {
    let Some(sb) = sandbox_on(MountedFs::Btrfs) else {
        return;
    };
    let src = project_with_blobs(&sb);
    let first_path = sb.path("first");
    assert_success(&add(&src, &sb, &first_path, &[]));
    let first = TestRepo::open(&sb.env, &first_path);
    let second_path = sb.path("second");
    let out = parhel(
        &sb.env,
        &first.root,
        &add_args(&second_path, &["--new-branch", "two"]),
    );
    assert_success(&out);
    assert!(shares_storage(
        &first.path("target/ignored.bin"),
        &second_path.join("target/ignored.bin")
    ));
    TestRepo::open(&sb.env, &second_path).assert_clean();
}

// I-L43: a destination on tmpfs-like volumes (no reflink, other device): `--copy`
// still works from a reflink volume.
#[test]
fn il43_copy_to_tmpfs_when_available() {
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() || common::cow::fs_type(shm) != "tmpfs" {
        eprintln!("SKIPPED: /dev/shm is not a tmpfs here");
        return;
    }
    let Some(sb) = sandbox_on(MountedFs::Btrfs) else {
        return;
    };
    let src = project(&sb);
    let holder = tempfile::Builder::new()
        .prefix("parhel-it-")
        .tempdir_in(shm)
        .unwrap();
    let dest = fs::canonicalize(holder.path()).unwrap().join("copied");
    let refused = add(&src, &sb, &dest, &[]);
    assert_error(&refused, "not-cow");
    assert!(stderr(&refused).contains("tmpfs"), "{}", stderr(&refused));
    assert_success(&add(&src, &sb, &dest, &["--copy"]));
    TestRepo::open(&sb.env, &dest).assert_clean();
}

// ------------------------------------------------------------ POSIX ACLs

use common::xattr::{self, acl};

/// An access ACL naming user 4242 (`named` permissions, masked by `mask`) on top of
/// the owner's `owner`; the group and others get nothing, but `group`.
fn acl_for(owner: u16, named: u16, group: u16, mask: u16, other: u16) -> Vec<u8> {
    acl::encode(&[
        (acl::USER_OBJ, owner, acl::NOBODY),
        (acl::USER, named, 4242),
        (acl::GROUP_OBJ, group, acl::NOBODY),
        (acl::MASK, mask, acl::NOBODY),
        (acl::OTHER, other, acl::NOBODY),
    ])
}

fn acl_of(path: &Path, name: &str) -> Option<Vec<u8>> {
    xattr::get(path, name).unwrap()
}

/// The filesystem/mode matrix of the ACL tests: ext4 can only be copied to.
fn acl_cases() -> Vec<(MountedFs, Vec<&'static str>)> {
    vec![
        (MountedFs::Btrfs, vec![]),
        (MountedFs::Btrfs, vec!["--copy"]),
        (MountedFs::Xfs, vec![]),
        (MountedFs::Xfs, vec!["--copy"]),
        (MountedFs::Ext4, vec!["--copy"]),
    ]
}

// I-L44: POSIX ACLs are part of who may read a file: they are copied with the files,
// the directories created for `.parhelignore` and the root of the worktree, so a
// `.env` that only its owner and one named user could read stays that way.
#[test]
fn il44_posix_acls_are_preserved() {
    for (fs_kind, mode) in acl_cases() {
        let Some(sb) = sandbox_on(fs_kind) else {
            continue;
        };
        let src = project(&sb);
        src.ignore(&["secrets/", "src/skipped/"]);
        src.write(".parhelignore", "src/skipped/\n");
        src.commit_all("parhelignore");
        src.write("secrets/.env", "TOKEN=1\n");
        src.write("src/skipped/log.txt", "left out");
        let file_acl = acl_for(0o6, 0o4, 0o0, 0o4, 0o0);
        let dir_acl = acl_for(0o7, 0o5, 0o0, 0o5, 0o0);
        xattr::set(&src.path("secrets/.env"), acl::ACCESS, &file_acl).unwrap();
        for dir in ["secrets", "src"] {
            xattr::set(&src.path(dir), acl::ACCESS, &dir_acl).unwrap();
            xattr::set(&src.path(dir), acl::DEFAULT, &dir_acl).unwrap();
        }
        let root_acl = acl_for(0o7, 0o5, 0o5, 0o5, 0o5);
        xattr::set(&src.root, acl::ACCESS, &root_acl).unwrap();

        let dest = sb.path("dest");
        assert_success(&add(&src, &sb, &dest, &mode));
        let label = format!("{} {mode:?}", fs_kind.fs_type());
        for rel in ["secrets/.env", "secrets", "src", ""] {
            assert_eq!(
                acl_of(&dest.join(rel), acl::ACCESS),
                acl_of(&src.path(rel), acl::ACCESS),
                "access ACL of `{rel}` ({label})"
            );
            assert_eq!(
                acl_of(&dest.join(rel), acl::DEFAULT),
                acl_of(&src.path(rel), acl::DEFAULT),
                "default ACL of `{rel}` ({label})"
            );
            assert_eq!(
                mode_of(&dest.join(rel)),
                mode_of(&src.path(rel)),
                "mode of `{rel}` ({label})"
            );
        }
        assert_eq!(
            acl_of(&dest.join("secrets/.env"), acl::ACCESS),
            Some(file_acl)
        );
    }
}

// I-L45: the reverse: a default ACL on the directory the worktree is created in must
// not leak into the copies. Entries made there inherit its named users, and a mode
// restored on top would give them access the source never had.
#[test]
fn il45_inherited_acls_do_not_widen_access() {
    for (fs_kind, mode) in acl_cases() {
        let Some(sb) = sandbox_on(fs_kind) else {
            continue;
        };
        let src = project(&sb);
        src.ignore(&["secrets/"]);
        src.write("secrets/.env", "TOKEN=1\n");
        src.chmod("secrets/.env", 0o640);
        src.mkdir("secrets/empty");
        // Everything created in the sandbox from now on inherits user 4242's rwx.
        let inherit = acl_for(0o7, 0o7, 0o5, 0o7, 0o5);
        xattr::set(&sb.root, acl::DEFAULT, &inherit).unwrap();

        let dest = sb.path("dest");
        assert_success(&add(&src, &sb, &dest, &mode));
        let label = format!("{} {mode:?}", fs_kind.fs_type());
        for rel in [
            "secrets/.env",
            "secrets",
            "secrets/empty",
            "target/debug/app",
        ] {
            assert_eq!(
                acl_of(&dest.join(rel), acl::ACCESS),
                None,
                "`{rel}` gained an access ACL ({label})"
            );
            assert_eq!(
                acl_of(&dest.join(rel), acl::DEFAULT),
                None,
                "`{rel}` gained a default ACL ({label})"
            );
            assert_eq!(
                mode_of(&dest.join(rel)),
                mode_of(&src.path(rel)),
                "mode of `{rel}` ({label})"
            );
        }
        assert_eq!(acl_of(&dest, acl::ACCESS), None, "worktree root ({label})");
        assert_eq!(acl_of(&dest, acl::DEFAULT), None, "worktree root ({label})");
    }
}

// ------------------------------------------------------------ what the kernel decides

/// A btrfs subvolume in the btrfs test mount: another `st_dev`, the same filesystem.
/// Deleted on drop when the unprivileged user is allowed to (otherwise it stays in the
/// throwaway test filesystem).
struct Subvolume(PathBuf);

impl Subvolume {
    fn create() -> Option<Self> {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = test_mount(MountedFs::Btrfs)?;
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = base.join(format!("parhel-subvol-{}-{n}", std::process::id()));
        let out = Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&path)
            .output()
            .expect("btrfs-progs is needed for the subvolume tests");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(Self(path))
    }
}

impl Drop for Subvolume {
    fn drop(&mut self) {
        let _ = Command::new("btrfs")
            .args(["subvolume", "delete", "--commit-after"])
            .arg(&self.0)
            .output();
    }
}

// I-L46: a source whose top directory is read-only (0555) on XFS still clones: the
// reflink probe cannot create its anonymous files there, but the destination's
// directory is writable and on the same filesystem, so its probe answers.
#[test]
fn il46_read_only_source_directory_does_not_hide_reflink_support() {
    let Some(sb) = sandbox_on(MountedFs::Xfs) else {
        return;
    };
    let src = project_with_blobs(&sb);
    let _restore = RestoreMode(src.root.clone(), 0o755);
    src.chmod("", 0o555);
    let dest = sb.path("cow");
    let out = add(&src, &sb, &dest, &[]);
    assert_success(&out);
    assert!(
        stdout(&out).contains("copy-on-write worktree"),
        "{}",
        stdout(&out)
    );
    assert_all_shared(&src, &dest);
    let doctor = parhel(&sb.env, &src.root, &["doctor"]);
    assert_success(&doctor);
    assert!(stdout(&doctor).contains("no issues"), "{}", stdout(&doctor));
}

// I-L47: btrfs subvolumes have a device number each but share one filesystem, and
// reflinks work between them: a worktree in another subvolume is cloned, not refused
// as `cross-volume`.
#[test]
fn il47_btrfs_subvolumes_share_storage() {
    let (Some(a), Some(b)) = (Subvolume::create(), Subvolume::create()) else {
        return;
    };
    assert_ne!(
        fs::metadata(&a.0).unwrap().dev(),
        fs::metadata(&b.0).unwrap().dev(),
        "different st_dev"
    );
    let sb = Sandbox::new_in(&a.0);
    let src = project_with_blobs(&sb);
    let dest = b.0.join("dest");
    let out = add(&src, &sb, &dest, &[]);
    assert_success(&out);
    assert!(
        stdout(&out).contains("copy-on-write worktree"),
        "{}",
        stdout(&out)
    );
    assert_all_shared(&src, &dest);
    TestRepo::open(&sb.env, &dest).assert_clean();
    let _ = fs::remove_dir_all(&dest);
}

// I-L48: btrfs refuses to share the extents of a NOCOW file (`chattr +C`, kept for
// databases and VM images) with the COW file the clone creates: a clear error naming
// the file and the two ways out, never a bare `Invalid argument` and never a silent
// full copy.
#[test]
fn il48_nocow_file_fails_with_a_hint() {
    let Some(sb) = sandbox_on(MountedFs::Btrfs) else {
        return;
    };
    let src = project(&sb);
    src.ignore(&["vm/"]);
    src.mkdir("vm");
    let st = Command::new("chattr")
        .arg("+C")
        .arg(src.path("vm"))
        .status()
        .expect("chattr (e2fsprogs) is needed for this test");
    assert!(st.success());
    write_blob(&src.path("vm/disk.img"), MIB, 7);
    let dest = sb.path("cow");
    let out = add(&src, &sb, &dest, &[]);
    assert_error(&out, "clone-failed");
    let err = stderr(&out);
    assert!(err.contains("vm/disk.img"), "{err}");
    assert!(err.contains("NOCOW") || err.contains("chattr +C"), "{err}");
    assert!(
        err.contains("--copy") && err.contains(".parhelignore"),
        "{err}"
    );
    assert!(!dest.exists());
    assert_no_staging(&[&sb.root]);

    // The offered way out works.
    assert_success(&add(&src, &sb, &dest, &["--copy"]));
    assert_eq!(
        fs::read(dest.join("vm/disk.img")).unwrap(),
        fs::read(src.path("vm/disk.img")).unwrap()
    );
}
