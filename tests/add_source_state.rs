//! I-110..I-124: source states that `status` alone does not describe, and sources
//! that change while `add` runs.

mod common;

use std::fs;

use common::*;
use rstest::rstest;

fn refused(sb: &Sandbox, src: &TestRepo, extra: &[&str], code: &str) -> String {
    let before = src.snapshot();
    let dest = sb.path("dest");
    let mut args = vec!["--new-branch", "n"];
    args.extend_from_slice(extra);
    let out = parhel(&sb.env, &src.root, &add_args(&dest, &args));
    assert_error(&out, code);
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
    stderr(&out)
}

/// `main` and `other` both edit `src/main.rs`; returns the repo on `main`.
fn diverged(sb: &Sandbox) -> TestRepo {
    let src = project(sb);
    src.git(&["checkout", "-q", "-b", "other"]);
    src.write("src/main.rs", "fn main() { other() }\n");
    src.commit_all("other");
    src.git(&["checkout", "-q", "main"]);
    src.write("src/main.rs", "fn main() { mine() }\n");
    src.commit_all("mine");
    src
}

// I-110: conflicted merge, with and without --allow-dirty.
#[rstest]
#[case(&[][..])]
#[case(&["--allow-dirty"][..])]
fn i110_conflicted_merge_is_refused(#[case] extra: &[&str]) {
    let sb = Sandbox::new();
    let src = diverged(&sb);
    assert!(
        !src.git_output(&["merge", "-q", "other"]).status.success(),
        "setup: conflict"
    );
    let err = refused(&sb, &src, extra, "operation-in-progress");
    assert!(err.contains("merge"), "{err}");
}

// I-111: clean merge stopped with --no-commit.
#[test]
fn i111_uncommitted_merge_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["checkout", "-q", "-b", "side"]);
    src.write("side.txt", "s");
    src.commit_all("side");
    src.git(&["checkout", "-q", "main"]);
    src.write("main.txt", "m");
    src.commit_all("main");
    src.git(&["merge", "-q", "--no-commit", "--no-ff", "side"]);
    refused(&sb, &src, &["--allow-dirty"], "operation-in-progress");
}

// I-112
#[test]
fn i112_rebase_in_progress_is_refused() {
    let sb = Sandbox::new();
    let src = diverged(&sb);
    assert!(
        !src.git_output(&["rebase", "-q", "other"]).status.success(),
        "setup: conflict"
    );
    let err = refused(&sb, &src, &["--allow-dirty"], "operation-in-progress");
    assert!(err.contains("rebase"), "{err}");
}

// I-113
#[test]
fn i113_cherry_pick_in_progress_is_refused() {
    let sb = Sandbox::new();
    let src = diverged(&sb);
    assert!(
        !src.git_output(&["cherry-pick", "other"]).status.success(),
        "setup: conflict"
    );
    let err = refused(&sb, &src, &["--allow-dirty"], "operation-in-progress");
    assert!(err.contains("cherry-pick"), "{err}");
}

// I-114
#[test]
fn i114_revert_in_progress_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("f.txt", "1\n");
    src.commit_all("one");
    let target = src.head();
    src.write("f.txt", "2\n");
    src.commit_all("two");
    assert!(
        !src.git_output(&["revert", "--no-edit", &target])
            .status
            .success(),
        "setup: conflict"
    );
    let err = refused(&sb, &src, &["--allow-dirty"], "operation-in-progress");
    assert!(err.contains("revert"), "{err}");
}

// I-115
#[test]
fn i115_am_in_progress_is_refused() {
    let sb = Sandbox::new();
    let src = diverged(&sb);
    let patch = src.git(&[
        "format-patch",
        "-1",
        "other",
        "-o",
        sb.path("patches").to_str().unwrap(),
    ]);
    assert!(
        !src.git_output(&["am", patch.trim()]).status.success(),
        "setup: conflict"
    );
    let err = refused(&sb, &src, &["--allow-dirty"], "operation-in-progress");
    assert!(err.contains("am"), "{err}");
}

// I-116: bisect leaves a clean detached checkout; it is allowed.
#[test]
fn i116_bisect_in_progress_is_allowed() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let good = src.head();
    for i in 0..4 {
        src.write("n.txt", format!("{i}\n"));
        src.commit_all(&format!("c{i}"));
    }
    src.git(&["bisect", "start", "HEAD", &good]);
    src.assert_clean();
    let dest = sb.path("bisect-wt");
    assert_success(&parhel(&sb.env, &src.root, &add_args(&dest, &["--detach"])));
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.head(), src.head());
    wt.assert_clean();
}

// I-117: assume-unchanged hides a modification from `status`.
#[test]
fn i117_assume_unchanged_counts_as_dirty() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["update-index", "--assume-unchanged", "Cargo.toml"]);
    src.write("Cargo.toml", "locally tweaked\n");
    src.assert_clean();
    let err = refused(&sb, &src, &[], "dirty-source");
    assert!(err.contains("Cargo.toml"), "{err}");
}

// I-117b: with --allow-dirty the content is carried and visible as a change.
#[test]
fn i117b_assume_unchanged_carried_with_allow_dirty() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["update-index", "--assume-unchanged", "Cargo.toml"]);
    src.write("Cargo.toml", "locally tweaked\n");
    let dest = sb.path("dest");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "n", "--allow-dirty"]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.read("Cargo.toml"), "locally tweaked\n");
    assert_eq!(
        wt.status(),
        " M Cargo.toml",
        "destination does not inherit the flag"
    );
}

// I-118
#[test]
fn i118_skip_worktree_counts_as_dirty() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["update-index", "--skip-worktree", "Cargo.toml"]);
    src.write("Cargo.toml", "local override\n");
    src.assert_clean();
    let err = refused(&sb, &src, &[], "dirty-source");
    assert!(err.contains("Cargo.toml"), "{err}");
}

/// Pause `add` after cloning, let `mutate` touch the source, resume: must abort.
fn changed_during_run(mutate: impl FnOnce(&TestRepo)) {
    let sb = Sandbox::new();
    let src = project(&sb);
    let worktrees = src.worktree_paths();
    let barrier = Barrier::new(&sb.root, "src");
    let mut child = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "n"]),
        Some(&barrier.spec("after_clone")),
    );
    barrier.wait_reached(&mut child);
    mutate(&src);
    barrier.release();
    let out = finish(child);
    assert_error(&out, "source-changed");
    assert!(!sb.path("dest").exists());
    assert_eq!(src.worktree_paths(), worktrees);
    assert_eq!(src.branch_tip("n"), None);
    assert_no_staging(&[&sb.root]);
}

// I-119
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i119_commit_in_source_during_run_aborts() {
    changed_during_run(|src| {
        src.write("late.txt", "late");
        src.commit_all("late commit");
    });
}

// I-120
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i120_edit_in_source_during_run_aborts() {
    changed_during_run(|src| src.write("src/main.rs", "edited mid-run\n"));
}

// I-121
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i121_new_untracked_file_during_run_aborts() {
    changed_during_run(|src| src.write("surprise.txt", "x"));
}

// I-121b: changes to ignored files during the run are fine (builds keep running).
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i121b_ignored_changes_during_run_are_tolerated() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let barrier = Barrier::new(&sb.root, "ign");
    let mut child = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "n"]),
        Some(&barrier.spec("after_clone")),
    );
    barrier.wait_reached(&mut child);
    src.write("target/debug/app", "rebuilt");
    barrier.release();
    assert_success(&finish(child));
}

/// Filter `upper`: stored upper-case in the repo, lower-case in the worktree.
fn with_filter(sb: &Sandbox) -> TestRepo {
    let src = project(sb);
    src.git(&["config", "filter.upper.clean", "tr a-z A-Z"]);
    src.git(&["config", "filter.upper.smudge", "tr A-Z a-z"]);
    src.git(&["config", "filter.upper.required", "true"]);
    src.write(".gitattributes", "*.flt filter=upper\n");
    src.write("doc.flt", "version one\n");
    src.commit_all("filtered v1");
    src
}

// I-122: same commit — status clean despite the filter.
#[test]
fn i122_clean_smudge_filter_same_ref() {
    let sb = Sandbox::new();
    let src = with_filter(&sb);
    assert_eq!(src.git(&["show", "HEAD:doc.flt"]), "VERSION ONE");
    let dest = sb.path("dest");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "n"]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("doc.flt"), "version one\n");
}

// I-123: different commit — target files go through the smudge filter.
#[test]
fn i123_clean_smudge_filter_different_ref() {
    let sb = Sandbox::new();
    let src = with_filter(&sb);
    let v1 = src.head();
    src.write("doc.flt", "version two\n");
    src.commit_all("filtered v2");
    let dest = sb.path("dest");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "n", &v1]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("doc.flt"), "version one\n", "smudged, not raw blob");
}

// I-124: Git LFS objects (requires git-lfs installed).
#[test]
#[ignore = "requires git-lfs (brew install git-lfs); run with --ignored"]
fn i124_git_lfs_files() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["lfs", "install", "--local"]);
    src.git(&["lfs", "track", "*.bin"]);
    fs::write(src.path("big.bin"), vec![7u8; 1 << 20]).unwrap();
    let v1 = src.commit_all("lfs v1");
    fs::write(src.path("big.bin"), vec![9u8; 1 << 20]).unwrap();
    src.commit_all("lfs v2");
    for (name, reference, byte) in [("same", None, 9u8), ("older", Some(v1.as_str()), 7u8)] {
        let dest = sb.path(name);
        let mut extra = vec!["--new-branch", name];
        extra.extend(reference);
        assert_success(&parhel(&sb.env, &src.root, &add_args(&dest, &extra)));
        let wt = TestRepo::open(&sb.env, &dest);
        wt.assert_clean();
        assert_eq!(
            fs::read(wt.path("big.bin")).unwrap(),
            vec![byte; 1 << 20],
            "{name}"
        );
    }
}
