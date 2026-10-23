//! I-C01..I-C12: `--copy`, the full copy that needs no copy-on-write (macOS and Linux).
//! The sandbox is on `$TMPDIR` (a copy-on-write volume), where `--copy` has to copy
//! although it could clone: the files must be equal and must NOT share storage.

mod common;

use std::fs;
use std::path::Path;

use common::cow::{is_independent_copy, shares_storage, write_blob};
use common::*;
use rstest::rstest;

const MIB: usize = 1 << 20;

fn copy_ok(sb: &Sandbox, src: &TestRepo, dest: &Path, extra: &[&str]) -> (TestRepo, String) {
    let branch = dest
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .replace('.', "-");
    let mut args = vec!["--copy", "--new-branch", &branch];
    args.extend(extra);
    let out = parhel(&sb.env, &src.root, &add_args(dest, &args));
    assert_success(&out);
    (TestRepo::open(&sb.env, dest), stdout(&out))
}

// I-C01: files are copied, not cloned, even where cloning would work.
#[test]
fn ic01_copy_duplicates_data_instead_of_sharing_it() {
    let sb = Sandbox::new();
    let src = project(&sb);
    write_blob(&src.path("assets/tracked.bin"), 2 * MIB, 1);
    src.commit_all("tracked blob");
    write_blob(&src.path("target/ignored.bin"), 4 * MIB, 2);
    write_blob(&src.path("node_modules/pkg/big.bin"), MIB, 3);
    let dest = sb.path("copied");

    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--copy", "--new-branch", "c"]),
    );
    assert_success(&out);

    assert!(
        stdout(&out).contains("full-copy worktree"),
        "reports the strategy: {}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("copy-on-write"), "{}", stdout(&out));
    assert!(
        !stderr(&out).contains("warning"),
        "a requested copy is not a degradation: {}",
        stderr(&out)
    );
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    for rel in [
        "assets/tracked.bin",
        "target/ignored.bin",
        "node_modules/pkg/big.bin",
    ] {
        assert_eq!(
            fs::read(wt.path(rel)).unwrap(),
            fs::read(src.path(rel)).unwrap(),
            "{rel} differs"
        );
        assert!(
            !shares_storage(&src.path(rel), &dest.join(rel)),
            "{rel} was cloned"
        );
        assert!(
            is_independent_copy(&src.path(rel), &dest.join(rel)),
            "{rel} shares blocks with the source"
        );
    }
    assert_eq!(wt.current_branch().as_deref(), Some("c"));
}

// I-C02: the ordinary files arrive too (ignored build output is the point).
#[test]
fn ic02_copy_brings_ignored_files_and_leaves_the_source_alone() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let before = src.snapshot();
    let (wt, _) = copy_ok(&sb, &src, &sb.path("copied"), &[]);
    wt.assert_clean();
    assert_eq!(wt.read("target/debug/app"), "binary");
    assert_eq!(
        wt.read("node_modules/pkg/index.js"),
        "module.exports = 1;\n"
    );
    assert_eq!(wt.read("src/main.rs"), "fn main() {}\n");
    // The source gained a worktree, nothing else.
    let after = src.snapshot();
    assert_eq!(before.tree, after.tree);
    assert_eq!(before.status, after.status);
    assert_no_staging(&[&sb.root]);
}

// I-C03: writes diverge in both directions.
#[test]
fn ic03_writes_do_not_cross_between_copy_and_source() {
    let sb = Sandbox::new();
    let src = project(&sb);
    write_blob(&src.path("target/ignored.bin"), MIB, 4);
    let original = fs::read(src.path("target/ignored.bin")).unwrap();
    let dest = sb.path("copied");
    copy_ok(&sb, &src, &dest, &[]);
    fs::write(dest.join("target/ignored.bin"), b"rewritten").unwrap();
    assert_eq!(fs::read(src.path("target/ignored.bin")).unwrap(), original);
    src.write("target/debug/app", "changed in source");
    assert_eq!(
        fs::read_to_string(dest.join("target/debug/app")).unwrap(),
        "binary"
    );
}

// I-C04: the two escape hatches are alternatives.
#[test]
fn ic04_copy_with_fallback_is_a_usage_error() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let before = src.snapshot();
    let dest = sb.path("never");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--copy", "--fallback", "--new-branch", "x"]),
    );
    assert_error(&out, "usage");
    assert!(
        stderr(&out).contains("--copy") && stderr(&out).contains("--fallback"),
        "{}",
        stderr(&out)
    );
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-C05: references to the source path are relocated in copies exactly as in clones.
#[test]
fn ic05_copy_relocates_references_to_the_source() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write(
        "target/debug/run.sh",
        format!("#!/bin/sh\ncd {}\n", src.root.display()),
    );
    src.symlink("target/debug/self", src.path("target/debug"));
    let dest = sb.path("copied");
    let (wt, stdout) = copy_ok(&sb, &src, &dest, &[]);
    assert_eq!(
        wt.read("target/debug/run.sh"),
        format!("#!/bin/sh\ncd {}\n", dest.display())
    );
    assert_eq!(
        fs::read_link(wt.path("target/debug/self")).unwrap(),
        dest.join("target/debug")
    );
    assert!(stdout.contains("Relocated 2"), "{stdout}");
    assert_eq!(
        src.read("target/debug/run.sh"),
        format!("#!/bin/sh\ncd {}\n", src.root.display()),
        "source untouched"
    );
    // `--no-relocate` copies verbatim.
    let (plain, _) = copy_ok(&sb, &src, &sb.path("verbatim"), &["--no-relocate"]);
    assert!(
        plain
            .read("target/debug/run.sh")
            .contains(src.root.to_str().unwrap())
    );
}

// I-C06: an existing destination is never touched.
#[test]
fn ic06_copy_refuses_an_existing_destination() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("taken");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("mine.txt"), "keep").unwrap();
    let before = src.snapshot();
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--copy", "--new-branch", "x"]),
    );
    assert_error(&out, "dest-exists");
    assert_eq!(fs::read_to_string(dest.join("mine.txt")).unwrap(), "keep");
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 1);
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-C07: every refusal that applies to clones applies to copies.
#[test]
fn ic07_copy_keeps_the_source_state_refusals() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("src/main.rs", "fn main() { dirty(); }\n");
    let before = src.snapshot();
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("d"), &["--copy", "--new-branch", "x"]),
    );
    assert_error(&out, "dirty-source");
    before.assert_same(&src.snapshot());
    let (wt, _) = copy_ok(&sb, &src, &sb.path("with-changes"), &["--allow-dirty"]);
    assert_eq!(wt.read("src/main.rs"), "fn main() { dirty(); }\n");
}

// I-C08: a destination inside the source is not copied into itself.
#[test]
fn ic08_copy_into_a_directory_of_the_source_does_not_recurse() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = src.path(".worktrees/feat");
    fs::create_dir_all(src.path(".worktrees")).unwrap();
    let (wt, _) = copy_ok(&sb, &src, &dest, &[]);
    wt.assert_clean();
    // The folder itself is an ancestor of the destination: created empty.
    assert!(
        !wt.exists(".worktrees/feat") && staging_leftovers(&[&wt.path(".worktrees")]).is_empty(),
        "the destination or its staging dir was copied into itself"
    );
    assert_eq!(walk_count(&wt.path(".worktrees")), 0);
    assert_eq!(wt.read("target/debug/app"), "binary");
    assert_no_staging(&[&src.path(".worktrees"), &sb.root]);
}

/// Entries directly inside `dir`, zero when it does not exist.
fn walk_count(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, Iterator::count)
}

// I-C09: `.parhelignore` applies to copies, too.
#[test]
fn ic09_parhelignore_is_honored_by_copy() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write(".parhelignore", "node_modules/\n");
    src.commit_all("parhelignore");
    let (wt, _) = copy_ok(&sb, &src, &sb.path("copied"), &[]);
    assert!(!wt.exists("node_modules"));
    assert!(wt.exists("target/debug/app"));
}

// I-C10: a failure at any step leaves no trace.
#[rstest]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn ic10_copy_failure_at_each_step_leaves_no_trace(
    #[values(
        "after_validate",
        "after_worktree_add",
        "mid_clone",
        "after_clone",
        "after_index",
        "before_rename"
    )]
    failpoint: &str,
) {
    let sb = Sandbox::new();
    let src = project(&sb);
    write_blob(&src.path("target/ignored.bin"), MIB, 9);
    let before = src.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--copy", "--new-branch", "u"]),
        &format!("{failpoint}=error"),
    );
    assert_error(&out, "failpoint");
    assert_eq!(src.branch_tip("u"), None);
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
    assert!(!sb.path("dest").exists());
}

// I-C11: SIGINT during the copy -> exit 130 and full rollback.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn ic11_sigint_during_copy_rolls_back() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let before = src.snapshot();
    let barrier = Barrier::new(&sb.root, "copy-int");
    let mut child = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--copy", "--new-branch", "u"]),
        Some(&barrier.spec("after_clone")),
    );
    barrier.wait_reached(&mut child);
    send_signal(&child, libc::SIGINT);
    barrier.release();
    let out = finish(child);
    assert_error(&out, "interrupted");
    assert_eq!(src.branch_tip("u"), None);
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-C12: killed after the copy was published: doctor repairs, never removes it.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn ic12_killed_after_publishing_a_copy_is_repaired_by_doctor() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("dest");
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--copy", "--new-branch", "published"]),
        "after_rename=kill",
    );
    assert_eq!(out.status.code(), None, "killed by signal");
    fs::write(dest.join("work.txt"), "user work").unwrap();
    assert_success(&parhel(&sb.env, &src.root, &["doctor", "--fix"]));
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.current_branch().as_deref(), Some("published"));
    assert_eq!(wt.status(), "?? work.txt");
    assert_eq!(wt.read("target/debug/app"), "binary");
    assert_no_staging(&[&sb.root]);
}
