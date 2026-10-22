//! I-90..I-97: a parhel worktree behaves like any other worktree afterwards.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use common::volume::{DiskImage, ImageFs};
use common::*;
use serial_test::serial;

fn add_ok(sb: &Sandbox, from: &TestRepo, dest: &Path, extra: &[&str]) -> TestRepo {
    let out = parhel(&sb.env, &from.root, &add_args(dest, extra));
    assert_success(&out);
    TestRepo::open(&sb.env, dest)
}

// I-90: same basename in two parents -> git disambiguates the worktree id.
#[test]
fn i90_same_basename_in_different_parents() {
    let sb = Sandbox::new();
    let src = project(&sb);
    fs::create_dir_all(sb.path("a")).unwrap();
    fs::create_dir_all(sb.path("b")).unwrap();
    let wa = add_ok(&sb, &src, &sb.path("a/feat"), &["--new-branch", "fa"]);
    let wb = add_ok(&sb, &src, &sb.path("b/feat"), &["--new-branch", "fb"]);
    assert_ne!(wa.git_dir(), wb.git_dir());
    wa.assert_clean();
    wb.assert_clean();
    assert_eq!(wa.current_branch().as_deref(), Some("fa"));
    assert_eq!(wb.current_branch().as_deref(), Some("fb"));
    let list = src.worktree_paths();
    assert!(
        list.contains(&wa.root) && list.contains(&wb.root),
        "{list:?}"
    );
    assert_no_staging(&[&sb.path("a"), &sb.path("b")]);
}

// I-90b: id taken by a stale admin dir whose worktree vanished.
#[test]
fn i90b_basename_taken_by_stale_registration() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let old = sb.path("old/feat");
    fs::create_dir_all(sb.path("old")).unwrap();
    src.git(&["worktree", "add", "-q", "--detach", old.to_str().unwrap()]);
    fs::remove_dir_all(&old).unwrap();
    let wt = add_ok(&sb, &src, &sb.path("feat"), &["--new-branch", "f"]);
    wt.assert_clean();
    assert!(src.worktree_paths().contains(&wt.root));
}

// I-91: maintenance commands never drop a parhel worktree (proves `repair` ran).
#[test]
fn i91_prune_and_gc_keep_worktree() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let wt = add_ok(&sb, &src, &sb.path("feat"), &["--new-branch", "f"]);
    src.git(&["worktree", "prune", "--expire", "now"]);
    src.git(&["gc", "-q", "--prune=now"]);
    assert!(src.worktree_paths().contains(&wt.root));
    let gitdir_file = wt.git_dir().join("gitdir");
    let recorded = fs::read_to_string(gitdir_file).unwrap();
    assert_eq!(
        Path::new(recorded.trim()),
        wt.path(".git"),
        "admin points to the final path"
    );
    wt.assert_clean();
    wt.write("x", "x");
    wt.commit_all("after gc");
}

// I-92: lock, unlock, move and remove work.
#[test]
fn i92_lock_move_remove() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let wt = add_ok(&sb, &src, &sb.path("feat"), &["--new-branch", "f"]);
    src.git(&[
        "worktree",
        "lock",
        "--reason",
        "testing",
        wt.root.to_str().unwrap(),
    ]);
    // The lock file itself: `worktree list` shows the reason only from git 2.31.
    assert_eq!(
        fs::read_to_string(wt.git_dir().join("locked"))
            .unwrap()
            .trim(),
        "testing"
    );
    src.git(&["worktree", "unlock", wt.root.to_str().unwrap()]);

    let moved = sb.path("moved");
    src.git(&[
        "worktree",
        "move",
        wt.root.to_str().unwrap(),
        moved.to_str().unwrap(),
    ]);
    let moved_wt = TestRepo::open(&sb.env, &moved);
    moved_wt.assert_clean();
    assert!(
        moved_wt.exists("target/debug/app"),
        "ignored files move along"
    );

    let admin = moved_wt.git_dir();
    // `remove` refuses untracked/ignored content unless forced, like any worktree.
    src.git(&["worktree", "remove", "--force", moved.to_str().unwrap()]);
    assert!(!moved.exists());
    assert!(!admin.exists());
    assert!(src.branch_tip("f").is_some(), "remove keeps the branch");
}

// I-93: `git worktree repair` has nothing to fix.
#[test]
fn i93_repair_is_a_no_op() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let wt = add_ok(&sb, &src, &sb.path("feat"), &["--new-branch", "f"]);
    let before = src.git(&["worktree", "list", "--porcelain"]);
    let out = src.git_output(&["worktree", "repair", wt.root.to_str().unwrap()]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "nothing to repair"
    );
    assert_eq!(src.git(&["worktree", "list", "--porcelain"]), before);
}

// I-94: concurrent runs to different destinations in the same parent all succeed.
#[test]
fn i94_concurrent_adds_to_different_destinations() {
    const N: usize = 6;
    let sb = Sandbox::new();
    let src = project(&sb);
    let children: Vec<_> = (0..N)
        .map(|i| {
            spawn_parhel(
                &sb.env,
                &src.root,
                &add_args(
                    &sb.path(format!("wt{i}")),
                    &["--new-branch", &format!("b{i}")],
                ),
                None,
            )
        })
        .collect();
    for (i, child) in children.into_iter().enumerate() {
        let out = finish(child);
        assert_success(&out);
        let wt = TestRepo::open(&sb.env, &sb.path(format!("wt{i}")));
        wt.assert_clean();
        assert_eq!(wt.current_branch(), Some(format!("b{i}")));
    }
    assert_eq!(src.worktree_paths().len(), N + 1);
    assert_no_staging(&[&sb.root]);
}

fn install_post_checkout_hook(src: &TestRepo, marker: &Path) {
    let hook = src.common_dir().join("hooks/post-checkout");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        &hook,
        format!("#!/bin/sh\necho ran >> '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
}

// I-95: decision pinned — hooks are not run by the copy-on-write path.
#[test]
fn i95_post_checkout_hook_not_run() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let marker = sb.path("hook-ran");
    install_post_checkout_hook(&src, &marker);
    add_ok(&sb, &src, &sb.path("feat"), &["--new-branch", "f"]);
    assert!(!marker.exists(), "post-checkout must not run");
}

// I-95b: the fallback is a plain `git worktree add`, so git runs the hook.
#[test]
#[serial(hdiutil)]
fn i95b_post_checkout_hook_runs_on_fallback() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let marker = sb.path("hook-ran");
    install_post_checkout_hook(&src, &marker);
    let hfs = DiskImage::new(ImageFs::Hfs, 64);
    add_ok(
        &sb,
        &src,
        &hfs.mount.join("feat"),
        &["--new-branch", "f", "--fallback"],
    );
    assert!(marker.exists(), "plain worktree add runs post-checkout");
}

// I-96: a parhel worktree can be the source of the next one.
#[test]
fn i96_chained_worktrees() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let first = add_ok(&sb, &src, &sb.path("first"), &["--new-branch", "one"]);
    first.write("target/debug/extra", "built in first");
    let second = add_ok(&sb, &first, &sb.path("second"), &["--new-branch", "two"]);
    second.assert_clean();
    assert_eq!(second.read("target/debug/extra"), "built in first");
    assert_eq!(second.common_dir(), src.common_dir());
    assert_eq!(src.worktree_paths().len(), 3);
}

// I-97: destination parent reached through a symlink.
#[test]
fn i97_destination_parent_through_symlink() {
    let sb = Sandbox::new();
    let src = project(&sb);
    fs::create_dir(sb.path("real")).unwrap();
    symlink(sb.path("real"), sb.path("link")).unwrap();
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&sb.path("link/feat"), &["--new-branch", "f"]),
    );
    assert_success(&out);
    let wt = TestRepo::open(&sb.env, &sb.path("real/feat"));
    wt.assert_clean();
    assert!(
        src.worktree_paths().contains(&wt.root),
        "registered under the canonical path"
    );
    assert_no_staging(&[&sb.path("real")]);
}
