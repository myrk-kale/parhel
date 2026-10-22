//! I-30..I-33: an existing or concurrently created destination is never touched
//! (bug 1 of the original: the losing run `RemoveAll`'d the winner's worktree).

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use common::*;
use rstest::rstest;

// I-30
#[rstest]
#[case::file("file")]
#[case::non_empty_dir("dir")]
#[case::empty_dir("empty")]
#[case::dangling_symlink("dangling")]
fn i30_existing_destination_is_refused_and_untouched(#[case] kind: &str) {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("taken");
    match kind {
        "file" => fs::write(&dest, "precious").unwrap(),
        "dir" => {
            fs::create_dir(&dest).unwrap();
            fs::write(dest.join("work.txt"), "precious").unwrap();
        }
        "empty" => fs::create_dir(&dest).unwrap(),
        _ => symlink("nowhere", &dest).unwrap(),
    }
    let parent_before = tree_digest(&sb.root);
    let before = src.snapshot();

    let out = parhel(&sb.env, &src.root, &add_args(&dest, &["--new-branch", "t"]));
    assert_error(&out, "dest-exists");
    assert_eq!(
        tree_digest(&sb.root),
        parent_before,
        "nothing around the destination changed"
    );
    before.assert_same(&src.snapshot());
}

/// Run A until it is about to publish, let `interfere` create the destination, release A.
fn race_at_publish(interfere: impl FnOnce(&Sandbox, &TestRepo, &Path)) {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("contested");
    let before_branches = src.branches();
    let barrier = Barrier::new(&sb.root, "a");
    let mut a = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "from-a"]),
        Some(&barrier.spec("before_rename")),
    );
    barrier.wait_reached(&mut a);

    interfere(&sb, &src, &dest);
    let dest_before_release = tree_digest(&dest);
    // Everything registered except the loser's own in-flight staging registration.
    let worktrees_before_release: Vec<_> = src
        .worktree_paths()
        .into_iter()
        .filter(|p| !p.to_string_lossy().contains(".parhel-staging-"))
        .collect();

    barrier.release();
    let out = finish(a);
    assert_error(&out, "dest-exists");

    assert_eq!(
        tree_digest(&dest),
        dest_before_release,
        "loser must not touch the destination"
    );
    assert_eq!(
        src.worktree_paths(),
        worktrees_before_release,
        "loser left no registration"
    );
    assert_eq!(
        src.branch_tip("from-a"),
        None,
        "loser deleted the branch it created"
    );
    for name in before_branches.keys() {
        assert!(
            src.branch_tip(name).is_some(),
            "pre-existing branch {name} intact"
        );
    }
    assert_no_staging(&[&sb.root]);
}

// I-31: another parhel run wins; the user starts working in it.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i31_losing_run_preserves_winning_worktree_and_uncommitted_work() {
    race_at_publish(|sb, src, dest| {
        let out = parhel(
            &sb.env,
            &src.root,
            &add_args(dest, &["--new-branch", "from-b"]),
        );
        assert_success(&out);
        fs::write(dest.join("uncommitted.txt"), "do not lose me").unwrap();
    });
}

// I-31b: after the race, the winner is a fully working worktree.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i31b_winner_remains_functional_after_race() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("contested");
    let barrier = Barrier::new(&sb.root, "a");
    let mut a = spawn_parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "from-a"]),
        Some(&barrier.spec("before_rename")),
    );
    barrier.wait_reached(&mut a);
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "from-b"]),
    ));
    barrier.release();
    assert_error(&finish(a), "dest-exists");

    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.current_branch().as_deref(), Some("from-b"));
    wt.assert_clean();
    wt.write("work.txt", "w");
    wt.commit_all("work in winner");
    let paths = src.worktree_paths();
    assert_eq!(paths.iter().filter(|p| **p == wt.root).count(), 1);
    assert_eq!(paths.len(), 2, "main + winner only: {paths:?}");
}

// I-32: a third party creates an empty dir (plain rename(2) would replace it).
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i32_empty_dir_created_during_run_is_not_replaced() {
    race_at_publish(|_, _, dest| fs::create_dir(dest).unwrap());
}

// I-32b: plain `git worktree add` wins the race.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i32b_plain_git_worktree_winning_is_respected() {
    race_at_publish(|_, src, dest| {
        src.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "plain",
            dest.to_str().unwrap(),
        ]);
        fs::write(dest.join("mine.txt"), "x").unwrap();
    });
}

// I-33: stress — N concurrent runs on one destination, no failpoints.
#[test]
fn i33_concurrent_runs_exactly_one_wins() {
    const N: usize = 8;
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("stress");
    let before_worktrees = src.worktree_paths().len();

    let children: Vec<_> = (0..N)
        .map(|i| {
            let mut cmd: Command = parhel_cmd(
                &sb.env,
                &src.root,
                &add_args(&dest, &["--new-branch", &format!("s{i}")]),
            );
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd.spawn().unwrap()
        })
        .collect();
    let outs: Vec<_> = children.into_iter().map(finish).collect();

    let winners: Vec<_> = outs
        .iter()
        .enumerate()
        .filter(|(_, o)| o.status.success())
        .collect();
    assert_eq!(winners.len(), 1, "exactly one winner");
    for (i, o) in outs.iter().enumerate().filter(|(_, o)| !o.status.success()) {
        assert!(
            stderr(o).contains("error[dest-exists]") || stderr(o).contains("error[git-failed]"),
            "loser {i} failed for an unexpected reason: {}",
            stderr(o)
        );
    }
    let winner = winners[0].0;
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.current_branch(), Some(format!("s{winner}")));
    wt.assert_clean();
    assert_eq!(src.worktree_paths().len(), before_worktrees + 1);
    let branches = src.branches();
    for i in (0..N).filter(|&i| i != winner) {
        assert!(
            !branches.contains_key(&format!("s{i}")),
            "loser s{i} left its branch"
        );
    }
    assert_no_staging(&[&sb.root]);
}
