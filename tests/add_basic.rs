//! I-01..I-09: creating worktrees in the common configurations.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use rstest::rstest;

fn open(sb: &Sandbox, path: &Path) -> TestRepo {
    TestRepo::open(&sb.env, path)
}

/// Every `.git` entry below `root`, excluding `root/.git`.
fn nested_git_entries(root: &Path) -> Vec<PathBuf> {
    tree_digest(root)
        .into_keys()
        .filter(|p| p.file_name().is_some_and(|n| n == ".git"))
        .collect()
}

/// Assertions shared by every successful copy-on-write add.
fn assert_cow_worktree(sb: &Sandbox, src: &TestRepo, wt: &TestRepo) {
    wt.assert_clean();
    assert_eq!(
        wt.read("target/debug/app"),
        "binary",
        "ignored build output carried over"
    );
    assert_eq!(
        wt.read("node_modules/pkg/index.js"),
        "module.exports = 1;\n"
    );
    assert!(
        src.worktree_paths().contains(&wt.root),
        "registered in the source repo"
    );
    assert_eq!(
        wt.common_dir(),
        src.common_dir(),
        "shares the source's object store"
    );
    assert!(
        nested_git_entries(&wt.root).is_empty(),
        "no stray .git below the root"
    );
    assert_no_staging(&[&sb.root, wt.root.parent().unwrap()]);
}

// I-01
#[test]
fn i01_new_branch_from_main_checkout() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let head = src.head();
    let dest = sb.path("feat");

    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "feat/x"]),
    );
    assert_success(&out);
    let wt = open(&sb, &dest);

    assert_cow_worktree(&sb, &src, &wt);
    assert_eq!(wt.current_branch().as_deref(), Some("feat/x"));
    assert_eq!(wt.head(), head);
    assert_eq!(src.branch_tip("feat/x").as_deref(), Some(head.as_str()));
    assert_eq!(
        wt.git_dir(),
        src.common_dir().join("worktrees/feat"),
        "worktree id = basename"
    );
    assert!(
        stdout(&out).contains(dest.to_str().unwrap()),
        "reports where: {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains(&head[..7]),
        "reports HEAD: {}",
        stdout(&out)
    );

    // The source is untouched and independent from the new worktree.
    src.assert_clean();
    assert_eq!(src.current_branch().as_deref(), Some("main"));
    wt.write("src/main.rs", "fn main() { println!(\"feat\"); }\n");
    wt.commit_all("feat work");
    assert_eq!(src.head(), head);
    assert_eq!(src.read("src/main.rs"), "fn main() {}\n");
    src.assert_clean();
}

// I-02
#[test]
fn i02_existing_branch() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["branch", "topic"]);
    let dest = sb.path("topic-wt");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--branch", "topic"]),
    ));
    let wt = open(&sb, &dest);
    assert_cow_worktree(&sb, &src, &wt);
    assert_eq!(wt.current_branch().as_deref(), Some("topic"));
}

// I-03
#[test]
fn i03_detached() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("det");
    assert_success(&parhel(&sb.env, &src.root, &add_args(&dest, &["--detach"])));
    let wt = open(&sb, &dest);
    assert_cow_worktree(&sb, &src, &wt);
    assert_eq!(wt.current_branch(), None);
    assert_eq!(wt.head(), src.head());
}

// I-04
#[rstest]
#[case::tag_detached("tag")]
#[case::full_sha("sha")]
#[case::short_sha("short")]
fn i04_explicit_ref_equal_to_source_head(#[case] kind: &str) {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["tag", "v1"]);
    let head = src.head();
    let reference = match kind {
        "tag" => "v1".to_owned(),
        "sha" => head.clone(),
        _ => head[..10].to_owned(),
    };
    let dest = sb.path("by-ref");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "r", &reference]),
    );
    assert_success(&out);
    let wt = open(&sb, &dest);
    assert_cow_worktree(&sb, &src, &wt);
    assert_eq!(wt.head(), head);
    assert_eq!(wt.current_branch().as_deref(), Some("r"));
}

// I-05: parity with `git worktree add <path>` -> branch named after the basename.
#[test]
fn i05_no_branch_flags_creates_basename_branch() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("quick-fix");
    assert_success(&parhel(&sb.env, &src.root, &add_args(&dest, &[])));
    let wt = open(&sb, &dest);
    assert_cow_worktree(&sb, &src, &wt);
    assert_eq!(wt.current_branch().as_deref(), Some("quick-fix"));
    assert_eq!(src.branch_tip("quick-fix"), Some(src.head()));
}

// I-05b: basename branch that already exists is checked out, not recreated.
#[test]
fn i05b_no_branch_flags_reuses_existing_basename_branch() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["branch", "quick-fix"]);
    let dest = sb.path("quick-fix");
    assert_success(&parhel(&sb.env, &src.root, &add_args(&dest, &[])));
    assert_eq!(
        open(&sb, &dest).current_branch().as_deref(),
        Some("quick-fix")
    );
}

// I-06a
#[test]
fn i06a_from_subdirectory_with_unrelated_cwd() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dest = sb.path("from-sub");
    let from = src.path("src");
    let args = add_args(
        &dest,
        &["--new-branch", "s", "--from", from.to_str().unwrap()],
    );
    assert_success(&parhel(&sb.env, &sb.root, &args));
    let wt = open(&sb, &dest);
    assert_cow_worktree(&sb, &src, &wt);
    assert!(
        wt.path("src/main.rs").is_file(),
        "cloned from the worktree root, not the subdir"
    );
}

// I-06b
#[test]
fn i06b_linked_worktree_as_source() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let linked = src.linked_worktree(&sb.path("linked"), "lb");
    linked.write("src/lib.rs", "pub fn f() {}\n");
    linked.commit_all("linked work");
    linked.write("target/debug/app", "binary");
    linked.write("node_modules/pkg/index.js", "module.exports = 1;\n");
    let linked_admin = linked.git_dir();
    let linked_before = linked.snapshot();

    let dest = sb.path("from-linked");
    assert_success(&parhel(
        &sb.env,
        &linked.root,
        &add_args(&dest, &["--new-branch", "y"]),
    ));
    let wt = open(&sb, &dest);
    assert_cow_worktree(&sb, &src, &wt);
    assert_eq!(wt.head(), linked.head());
    assert!(wt.path("src/lib.rs").is_file());
    assert_ne!(wt.git_dir(), linked_admin);
    assert_eq!(linked.current_branch().as_deref(), Some("lb"));
    let mut after = linked.snapshot();
    // Registering a new worktree legitimately changes the shared lists.
    after.worktrees.clone_from(&linked_before.worktrees);
    after.branches.clone_from(&linked_before.branches);
    after.admin_dirs.clone_from(&linked_before.admin_dirs);
    linked_before.assert_same(&after);
}

// I-07: relative destination resolves against the cwd, not the source.
#[test]
fn i07_relative_destination_resolves_against_cwd() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let cwd = sb.path("a/b");
    fs::create_dir_all(&cwd).unwrap();
    let out = parhel(
        &sb.env,
        &cwd,
        &[
            "add",
            "../../rel-dest",
            "--from",
            "../../src",
            "--new-branch",
            "rel",
        ],
    );
    assert_success(&out);
    assert!(sb.path("rel-dest/.git").is_file());
    assert!(!sb.root.parent().unwrap().join("rel-dest").exists());
    assert_cow_worktree(&sb, &src, &open(&sb, &sb.path("rel-dest")));
}

// I-08: destination nested inside the (ignored part of the) source.
#[test]
fn i08_destination_nested_in_source() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.mkdir(".worktrees");
    let dest = src.path(".worktrees/feat");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "n"]),
    ));
    let wt = open(&sb, &dest);
    assert_cow_worktree(&sb, &src, &wt);
    assert!(
        !wt.exists(".worktrees/feat"),
        "worktree must not contain itself"
    );
    assert_no_staging(&[&src.path(".worktrees"), &wt.path(".worktrees")]);
    src.assert_clean();
}

// I-09: another linked worktree nested in the source is not cloned.
#[test]
fn i09_nested_linked_worktree_is_excluded() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let other = src.linked_worktree(&src.path(".worktrees/other"), "other");
    let other_head = other.head();
    let dest = sb.path("feat");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "n"]),
    ));
    let wt = open(&sb, &dest);
    assert_cow_worktree(&sb, &src, &wt);
    assert!(
        !wt.exists(".worktrees/other"),
        "nested worktree must be excluded"
    );

    wt.write("x.txt", "x");
    wt.commit_all("in new worktree");
    assert_eq!(other.head(), other_head);
    assert_eq!(other.current_branch().as_deref(), Some("other"));
    other.assert_clean();
}

// I-09c: a registration recorded through a symlink (git canonicalizes paths, so this
// only happens if the admin `gitdir` file is edited by hand) must not turn the
// symlink into a real directory in the destination.
#[test]
fn i09c_symlink_on_path_to_exclusion_stays_a_symlink() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let elsewhere = sb.path("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("outside.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(&elsewhere, src.path("node_modules/linked")).unwrap();
    let real_wt = elsewhere.join("wt");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        real_wt.to_str().unwrap(),
    ]);
    let admin = TestRepo::open(&sb.env, &real_wt).git_dir();
    let through_link = src.path("node_modules/linked/wt/.git");
    fs::write(
        admin.join("gitdir"),
        format!("{}\n", through_link.display()),
    )
    .unwrap();
    assert!(
        src.git(&["worktree", "list", "--porcelain"])
            .contains("node_modules/linked/wt"),
        "setup: registration goes through the symlink"
    );

    let dest = sb.path("dest");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "s"]),
    ));
    let md = fs::symlink_metadata(dest.join("node_modules/linked")).unwrap();
    assert!(md.file_type().is_symlink(), "must remain a symlink");
    assert_eq!(
        fs::read_link(dest.join("node_modules/linked")).unwrap(),
        elsewhere
    );
    assert_eq!(
        fs::read_to_string(elsewhere.join("outside.txt")).unwrap(),
        "outside"
    );
}
