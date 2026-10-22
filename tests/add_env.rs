//! I-60..I-63: process environment and repository discovery.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::*;

// I-60: GIT_DIR / GIT_WORK_TREE inherited from the caller do not redirect parhel.
#[test]
fn i60_inherited_git_env_does_not_redirect() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let decoy = sb.repo("decoy");
    let decoy_before = decoy.snapshot();
    let dest = sb.path("dest");
    let out = parhel_cmd(&sb.env, &src.root, &add_args(&dest, &["--new-branch", "e"]))
        .env("GIT_DIR", decoy.path(".git"))
        .env("GIT_WORK_TREE", &decoy.root)
        .env("GIT_INDEX_FILE", decoy.path(".git/index"))
        .output()
        .unwrap();
    assert_success(&out);
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.common_dir(), src.common_dir());
    assert!(wt.exists("src/main.rs"));
    decoy_before.assert_same(&decoy.snapshot());
}

// I-61a: installed on PATH, invoked as a git subcommand.
#[test]
fn i61a_invoked_as_git_subcommand() {
    let mut sb = Sandbox::new();
    let bin = install_extension(&sb.root);
    sb.with_path_prefix(vec![bin]);
    let src = project(&sb);
    let dest = sb.path("via-git");
    let out = sb
        .env
        .git_command(&src.root)
        .args(["parhel", "add", dest.to_str().unwrap(), "--new-branch", "g"])
        .output()
        .unwrap();
    assert_success(&out);
    TestRepo::open(&sb.env, &dest).assert_clean();
}

// I-61b: `git -C <dir> parhel add ../x` resolves relative to <dir>.
#[test]
fn i61b_git_dash_c_changes_resolution_base() {
    let mut sb = Sandbox::new();
    let bin = install_extension(&sb.root);
    sb.with_path_prefix(vec![bin]);
    let src = project(&sb);
    let out = sb
        .env
        .git_command(&sb.root)
        .args([
            "-C",
            "src",
            "parhel",
            "add",
            "../dash-c",
            "--new-branch",
            "c",
        ])
        .output()
        .unwrap();
    assert_success(&out);
    assert!(sb.path("dash-c/.git").is_file());
    assert!(src.worktree_paths().contains(&sb.path("dash-c")));
}

// I-61c: help through git (`git parhel --help` is intercepted by git as man page;
// `git parhel -h` reaches us).
#[test]
fn i61c_short_help_through_git() {
    let mut sb = Sandbox::new();
    let bin = install_extension(&sb.root);
    sb.with_path_prefix(vec![bin]);
    let out = sb
        .env
        .git_command(&sb.root)
        .args(["parhel", "-h"])
        .output()
        .unwrap();
    assert_success(&out);
    assert!(stdout(&out).contains("add") && stdout(&out).contains("doctor"));
}

// I-62: discovery errors.
#[test]
fn i62a_bare_repository_is_refused() {
    let sb = Sandbox::new();
    let bare = sb.path("bare.git");
    git_in(
        &sb.env,
        &sb.root,
        &["init", "-q", "--bare", bare.to_str().unwrap()],
    );
    let out = parhel(&sb.env, &bare, &add_args(&sb.path("d"), &[]));
    assert_error(&out, "bare-repo");
    assert!(!sb.path("d").exists());
}

#[test]
fn i62b_outside_any_repository_is_refused() {
    let sb = Sandbox::new();
    let plain = sb.path("plain");
    fs::create_dir(&plain).unwrap();
    let out = parhel(&sb.env, &plain, &add_args(&sb.path("d"), &[]));
    assert_error(&out, "not-a-repo");
}

#[test]
fn i62c_unborn_head_is_refused() {
    let sb = Sandbox::new();
    let empty = sb.empty_repo("empty");
    let out = parhel(&sb.env, &empty.root, &add_args(&sb.path("d"), &[]));
    assert_error(&out, "unborn-head");
    assert_eq!(empty.git(&["worktree", "list"]).lines().count(), 1);
}

#[test]
fn i62d_from_inside_git_dir_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let from = src.path(".git");
    let out = parhel(
        &sb.env,
        &sb.root,
        &add_args(&sb.path("d"), &["--from", from.to_str().unwrap()]),
    );
    assert_error(&out, "inside-git-dir");
}

#[test]
fn i62e_destination_is_source_or_inside_git_dir() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let before = src.snapshot();
    assert_error(
        &parhel(&sb.env, &src.root, &add_args(&src.root, &[])),
        "dest-is-source",
    );
    assert_error(
        &parhel(&sb.env, &src.root, &add_args(&src.path(".git/wt"), &[])),
        "dest-inside-git-dir",
    );
    before.assert_same(&src.snapshot());
}

#[test]
fn i62f_missing_destination_parent_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("no/such/parent/d"), &[]),
    );
    assert_error(&out, "dest-parent-missing");
    assert!(
        !sb.path("no").exists(),
        "parents are not created implicitly"
    );
}

// I-63: git older than the minimum.
#[test]
fn i63_old_git_is_refused() {
    let mut sb = Sandbox::new();
    let fake = sb.path("fake-git");
    fs::create_dir(&fake).unwrap();
    let real_git = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'git version 2.20.0'; exit 0; fi\nexec {} \"$@\"\n",
        real_git.trim()
    );
    fs::write(fake.join("git"), script).unwrap();
    fs::set_permissions(fake.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    sb.with_path_prefix(vec![fake]);
    let src = project(&sb);
    let out = parhel(&sb.env, &src.root, &add_args(&sb.path("d"), &[]));
    assert_error(&out, "git-too-old");
    assert!(
        stderr(&out).contains("2.20.0") && stderr(&out).contains("2.29"),
        "{}",
        stderr(&out)
    );
}

// I-64: option-like ref/branch values never reach git as options.
#[rstest::rstest]
#[case::ref_orphan(&["--", "--orphan"][..])]
#[case::ref_short_flag(&["--", "-b"][..])]
#[case::new_branch_dash(&["--new-branch=-f"][..])]
#[case::branch_dash(&["--branch=--force"][..])]
fn i64_option_like_values_are_rejected(#[case] extra: &[&str]) {
    let sb = Sandbox::new();
    let src = project(&sb);
    let before = src.snapshot();
    let dest = sb.path("d");
    let mut args = vec!["add".to_owned(), dest.to_str().unwrap().to_owned()];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let out = parhel(&sb.env, &src.root, &args);
    assert_error(&out, "usage");
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
}

// I-64b: a destination named like an option is just a path.
#[test]
fn i64b_dash_destination_is_a_path() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let out = parhel(
        &sb.env,
        &sb.root,
        &["add", "--from", "src", "--new-branch", "w", "--", "-weird"],
    );
    assert_success(&out);
    TestRepo::open(&sb.env, &sb.path("-weird")).assert_clean();
    assert!(src.worktree_paths().contains(&sb.path("-weird")));
}

// I-65: non-ASCII branch names.
#[test]
fn i65_unicode_branch_name() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("u"), &["--new-branch", "feat/ação-ç"]),
    );
    assert_success(&out);
    assert_eq!(
        TestRepo::open(&sb.env, &sb.path("u"))
            .current_branch()
            .as_deref(),
        Some("feat/ação-ç")
    );
}

// I-64c: without a branch flag the destination's basename becomes git's branch
// name; one starting with `-` is refused instead of reaching `git branch`.
#[test]
fn i64c_option_like_destination_basename_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let before = src.snapshot();
    let out = parhel(&sb.env, &src.root, &["add", "../-m"]);
    assert_error(&out, "usage");
    assert!(!sb.path("-m").exists());
    before.assert_same(&src.snapshot());
}
