//! I-40..I-45: failures leave no trace (bug 3 of the original: a failed run left a
//! registration in `.git/worktrees` that kept the branch "already used").

mod common;

use std::fs;

use common::*;
use rstest::rstest;

/// After a failed run: no trace, and `branch` is still usable by plain git.
fn assert_no_trace(sb: &Sandbox, src: &TestRepo, before: &RepoSnapshot, branch: &str) {
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
    assert!(!sb.path("dest").exists());
    let probe = sb.path("probe");
    src.git(&["worktree", "add", "-q", probe.to_str().unwrap(), branch]);
    assert_eq!(
        TestRepo::open(&sb.env, &probe).current_branch().as_deref(),
        Some(branch)
    );
}

// I-40a: injected error at each step, with an existing branch (the original's repro).
#[rstest]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn i40a_injected_failure_with_existing_branch_leaves_no_trace(
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
    src.git(&["branch", "topic"]);
    let before = src.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--branch", "topic"]),
        &format!("{failpoint}=error"),
    );
    assert_error(&out, "failpoint");
    assert_no_trace(&sb, &src, &before, "topic");
}

// I-40b: injected error at each step with --new-branch: the new branch is removed.
#[rstest]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn i40b_injected_failure_with_new_branch_removes_it(
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
    let before = src.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "fresh"]),
        &format!("{failpoint}=error"),
    );
    assert_error(&out, "failpoint");
    assert_eq!(src.branch_tip("fresh"), None);
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
    // The name is free again.
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "fresh"]),
    ));
}

// I-40c: a panic (bug) inside the transaction still rolls back.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i40c_panic_mid_transaction_still_rolls_back() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["branch", "topic"]);
    let before = src.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--branch", "topic"]),
        "after_clone=panic",
    );
    assert!(!out.status.success());
    // Proof the transaction got as far as the failpoint (an inert binary leaves no
    // trace either, so "no trace" alone proves nothing).
    assert!(
        stderr(&out).contains("failpoint `after_clone` panicked"),
        "{}",
        stderr(&out)
    );
    assert_no_trace(&sb, &src, &before, "topic");
}

// I-41: branch already checked out elsewhere -> git's error, nothing left behind.
#[test]
fn i41_branch_in_use_elsewhere() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.linked_worktree(&sb.path("holder"), "busy");
    let before = src.snapshot();
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--branch", "busy"]),
    );
    assert_error(&out, "git-failed");
    let err = stderr(&out);
    assert!(
        err.contains("already checked out") || err.contains("already used by worktree"),
        "forwards git's reason: {err}"
    );
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-42: --new-branch with a taken name must not delete the existing branch.
#[test]
fn i42_new_branch_name_taken_keeps_existing_branch() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["branch", "taken"]);
    let tip = src.branch_tip("taken");
    let before = src.snapshot();
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "taken"]),
    );
    assert_error(&out, "git-failed");
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
    assert_eq!(src.branch_tip("taken"), tip);
    before.assert_same(&src.snapshot());
}

// I-43: a real clone failure (unreadable directory) rolls back.
#[test]
fn i43_unreadable_source_entry_fails_clone_and_rolls_back() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.mkdir("target/locked");
    src.write("target/locked/data", "x");
    let locked = src.path("target/locked");
    let _restore = RestoreMode(locked.clone(), 0o755);
    fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    let before = src.snapshot();

    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "u"]),
    );
    assert_error(&out, "clone-failed");
    assert!(
        stderr(&out).contains("target/locked"),
        "names the entry: {}",
        stderr(&out)
    );
    assert_eq!(src.branch_tip("u"), None);
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-44: SIGINT mid-run -> exit 130, full rollback.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i44_sigint_rolls_back() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["branch", "topic"]);
    let before = src.snapshot();
    let barrier = Barrier::new(&sb.root, "int");
    let mut child = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--branch", "topic"]),
        Some(&barrier.spec("after_clone")),
    );
    barrier.wait_reached(&mut child);
    // SAFETY: signalling our own child process.
    let rc = unsafe { libc::kill(i32::try_from(child.id()).unwrap(), libc::SIGINT) };
    assert_eq!(rc, 0);
    barrier.release();
    let out = finish(child);
    assert_error(&out, "interrupted");
    assert_no_trace(&sb, &src, &before, "topic");
}

// I-45: SIGKILL leaves a journal; `doctor` reports it and `doctor --fix` undoes it.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i45_sigkill_leftovers_are_repaired_by_doctor() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let before = src.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "crashed"]),
        "before_rename=kill",
    );
    assert_eq!(out.status.code(), None, "killed by signal");
    assert_eq!(
        staging_leftovers(&[&sb.root]).len(),
        1,
        "a crash leaves its staging dir"
    );

    let report = parhel(&sb.env, &src.root, &["doctor"]);
    assert_eq!(
        report.status.code(),
        Some(1),
        "issues found:\n{}",
        stdout(&report)
    );
    let text = stdout(&report);
    assert!(
        text.contains("orphan-staging") && text.contains("orphan-registration"),
        "{text}"
    );
    assert_eq!(
        staging_leftovers(&[&sb.root]).len(),
        1,
        "doctor without --fix is read-only"
    );

    let fix = parhel(&sb.env, &src.root, &["doctor", "--fix"]);
    assert_success(&fix);
    assert_eq!(
        src.branch_tip("crashed"),
        None,
        "branch created by the crashed run removed"
    );
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "crashed"]),
    ));
}

/// Pause at `after_clone`, deliver `signal`, release, expect exit `128 + signal`.
fn signal_mid_run(signal: i32) {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["branch", "topic"]);
    let before = src.snapshot();
    let barrier = Barrier::new(&sb.root, "sig");
    let mut child = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--branch", "topic"]),
        Some(&barrier.spec("after_clone")),
    );
    barrier.wait_reached(&mut child);
    send_signal(&child, signal);
    barrier.release();
    let out = finish(child);
    assert_interrupted(&out, signal);
    assert_no_trace(&sb, &src, &before, "topic");
}

// I-44b
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i44b_sigterm_rolls_back() {
    signal_mid_run(libc::SIGTERM);
}

// I-44c: terminal closed.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i44c_sighup_rolls_back() {
    signal_mid_run(libc::SIGHUP);
}

// I-44d: impatient Ctrl-C while rolling back must not abort the cleanup.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i44d_signals_during_rollback_do_not_abort_cleanup() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["branch", "topic"]);
    let before = src.snapshot();
    let barrier = Barrier::new(&sb.root, "rb");
    let mut child = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--branch", "topic"]),
        Some(&format!(
            "after_clone=error;{}",
            barrier.spec("during_rollback")
        )),
    );
    barrier.wait_reached(&mut child);
    send_signal(&child, libc::SIGINT);
    send_signal(&child, libc::SIGINT);
    send_signal(&child, libc::SIGTERM);
    barrier.release();
    let out = finish(child);
    assert_error(&out, "failpoint");
    assert_no_trace(&sb, &src, &before, "topic");
}

// I-45b: killed after the publishing rename, before `worktree repair`. The worktree
// at the destination is valid; `doctor --fix` must repair it, never roll it back.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i45b_crash_after_publish_is_repaired_not_rolled_back() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("dest");
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "published"]),
        "after_rename=kill",
    );
    assert_eq!(out.status.code(), None, "killed by signal");
    assert!(
        dest.join(".git").is_file(),
        "rename happened before the crash"
    );
    fs::write(dest.join("first-work.txt"), "user started working").unwrap();

    let report = parhel(&sb.env, &src.root, &["doctor"]);
    assert_eq!(report.status.code(), Some(1), "{}", stdout(&report));

    assert_success(&parhel(&sb.env, &src.root, &["doctor", "--fix"]));
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.current_branch().as_deref(), Some("published"));
    assert_eq!(
        wt.status(),
        "?? first-work.txt",
        "worktree intact, user file kept"
    );
    assert!(
        src.worktree_paths().contains(&wt.root),
        "registered at the destination"
    );
    assert_eq!(src.worktree_paths().len(), 2);
    assert_no_staging(&[&sb.root]);
    assert_success(&parhel(&sb.env, &src.root, &["doctor"]));
}

/// A linked worktree whose directory is unreachable, like one on an unmounted disk.
fn unreachable_worktree(sb: &Sandbox, src: &TestRepo) -> std::path::PathBuf {
    let wt = sb.path("external-wt");
    src.git(&["worktree", "add", "-q", "-b", "ext", wt.to_str().unwrap()]);
    fs::rename(&wt, sb.path("external-wt.unmounted")).unwrap();
    wt
}

// I-46: rollback's fallback cleanup must not drop unrelated registrations.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i46_rollback_keeps_unreachable_worktrees_registered() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let external = unreachable_worktree(&sb, &src);
    // An immutable file makes `git worktree remove` fail, forcing the fallback path.
    src.write("target/frozen.bin", "frozen");
    let st = std::process::Command::new("chflags")
        .arg("uchg")
        .arg(src.path("target/frozen.bin"))
        .status()
        .unwrap();
    assert!(st.success());
    let before = src.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&sb.path("dest"), &["--new-branch", "d"]),
        "after_clone=error",
    );
    assert_error(&out, "failpoint");
    assert!(
        src.worktree_paths().contains(&external),
        "the unreachable worktree lost its registration"
    );
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-45c: killed after the rename, and the journal's last write never reached the
// disk (simulated by truncating it): doctor must still repair, not remove.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i45c_publication_detected_even_with_lost_journal() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("dest");
    let out = parhel_with_failpoints(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "published"]),
        "after_rename=kill",
    );
    assert_eq!(out.status.code(), None, "killed by signal");
    let staging = staging_leftovers(&[&sb.root]);
    assert_eq!(staging.len(), 1);
    fs::write(staging[0].join("journal"), b"").unwrap();
    fs::write(dest.join("work.txt"), "user work").unwrap();

    assert_success(&parhel(&sb.env, &src.root, &["doctor", "--fix"]));
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.current_branch().as_deref(), Some("published"));
    assert_eq!(wt.status(), "?? work.txt");
    assert!(src.worktree_paths().contains(&wt.root));
    assert!(src.branch_tip("published").is_some(), "branch kept");
    assert_no_staging(&[&sb.root]);
}
