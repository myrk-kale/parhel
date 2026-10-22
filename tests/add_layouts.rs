//! I-100..I-109: repository layouts and formats.

mod common;

use std::fs;
use std::path::Path;

use common::*;

fn add_ok(sb: &Sandbox, from: &Path, dest: &Path, extra: &[&str]) -> TestRepo {
    let out = parhel(&sb.env, from, &add_args(dest, extra));
    assert_success(&out);
    TestRepo::open(&sb.env, dest)
}

fn with_heavy_ignored(r: &TestRepo) {
    r.write("target/debug/app", "binary");
}

// I-100: bare repository + linked worktrees (common "bare clone" workflow).
#[test]
fn i100_linked_worktree_of_bare_repo_as_source() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let bare = sb.path("bare.git");
    git_in(
        &sb.env,
        &sb.root,
        &[
            "clone",
            "-q",
            "--bare",
            src.root.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let main_wt = sb.path("bare-main");
    git_in(
        &sb.env,
        &bare,
        &["worktree", "add", "-q", main_wt.to_str().unwrap(), "main"],
    );
    let main_wt = TestRepo::open(&sb.env, &main_wt);
    with_heavy_ignored(&main_wt);

    let wt = add_ok(
        &sb,
        &main_wt.root,
        &sb.path("bare-feat"),
        &["--new-branch", "f"],
    );
    wt.assert_clean();
    assert_eq!(wt.common_dir(), fs::canonicalize(&bare).unwrap());
    assert_eq!(wt.read("target/debug/app"), "binary");
}

// I-101: `.bare` + `.git` gitfile layout with worktrees as children of the project.
#[test]
fn i101_dot_bare_layout() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let project_dir = sb.path("proj");
    fs::create_dir(&project_dir).unwrap();
    git_in(
        &sb.env,
        &project_dir,
        &["clone", "-q", "--bare", src.root.to_str().unwrap(), ".bare"],
    );
    fs::write(project_dir.join(".git"), "gitdir: ./.bare\n").unwrap();
    git_in(
        &sb.env,
        &project_dir,
        &["worktree", "add", "-q", "main", "main"],
    );
    let main_wt = TestRepo::open(&sb.env, &project_dir.join("main"));
    with_heavy_ignored(&main_wt);

    let wt = add_ok(
        &sb,
        &main_wt.root,
        &project_dir.join("feat"),
        &["--new-branch", "f"],
    );
    wt.assert_clean();
    assert!(!main_wt.exists("feat"));
    assert!(
        !wt.exists(".bare") && !wt.exists("main"),
        "siblings are not cloned"
    );
    assert_eq!(wt.read("target/debug/app"), "binary");
}

// I-102: `--separate-git-dir`: the root `.git` is a file.
#[test]
fn i102_separate_git_dir() {
    let sb = Sandbox::new();
    let gd = sb.path("gitdir-store");
    let root = sb.path("sep");
    fs::create_dir(&root).unwrap();
    git_in(
        &sb.env,
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            gd.to_str().unwrap(),
        ],
    );
    let repo = TestRepo::open(&sb.env, &root);
    repo.write("a.txt", "a");
    repo.commit_all("a");
    repo.ignore(&["target/"]);
    with_heavy_ignored(&repo);
    assert!(root.join(".git").is_file());
    let gitfile_before = fs::read(root.join(".git")).unwrap();

    let wt = add_ok(&sb, &root, &sb.path("sep-feat"), &["--new-branch", "f"]);
    wt.assert_clean();
    assert_eq!(wt.common_dir(), fs::canonicalize(&gd).unwrap());
    assert_eq!(
        fs::read(root.join(".git")).unwrap(),
        gitfile_before,
        "source gitfile untouched"
    );
}

// I-103: the source worktree is itself a submodule checkout.
#[test]
fn i103_source_is_a_submodule() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let lib = sb.repo("lib");
    lib.write("lib.rs", "v1\n");
    lib.commit_all("lib");
    src.add_submodule(&lib, "libs/lib");
    let sub = TestRepo::open(&sb.env, &src.path("libs/lib"));

    let wt = add_ok(&sb, &sub.root, &sb.path("lib-feat"), &["--new-branch", "f"]);
    wt.assert_clean();
    assert_eq!(wt.read("lib.rs"), "v1\n");
    assert_eq!(wt.common_dir(), sub.common_dir());
    assert!(
        wt.common_dir()
            .starts_with(src.common_dir().join("modules"))
    );
    src.assert_clean();
}

// I-104: SHA-256 object format.
#[test]
fn i104_sha256_repository() {
    let sb = Sandbox::new();
    let root = sb.path("sha256");
    fs::create_dir(&root).unwrap();
    git_in(
        &sb.env,
        &root,
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    let repo = TestRepo::open(&sb.env, &root);
    repo.write("a.txt", "a");
    let old = repo.commit_all("a");
    repo.write("a.txt", "b");
    repo.commit_all("b");
    assert_eq!(old.len(), 64);

    let wt = add_ok(
        &sb,
        &root,
        &sb.path("sha-feat"),
        &["--new-branch", "f", &old],
    );
    wt.assert_clean();
    assert_eq!(wt.head(), old);
    assert_eq!(wt.read("a.txt"), "a");
}

// I-105: reftable ref backend (rollback must delete the branch through git).
#[test]
fn i105_reftable_repository() {
    let sb = Sandbox::new();
    let root = sb.path("reftable");
    fs::create_dir(&root).unwrap();
    git_in(
        &sb.env,
        &root,
        &["init", "-q", "-b", "main", "--ref-format=reftable"],
    );
    let repo = TestRepo::open(&sb.env, &root);
    repo.write("a.txt", "a");
    repo.commit_all("a");
    let wt = add_ok(&sb, &root, &sb.path("rt-feat"), &["--new-branch", "f"]);
    wt.assert_clean();
    assert_eq!(repo.branch_tip("f"), Some(repo.head()));
}

// I-105b: rollback on reftable.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i105b_reftable_rollback_removes_branch() {
    let sb = Sandbox::new();
    let root = sb.path("reftable");
    fs::create_dir(&root).unwrap();
    git_in(
        &sb.env,
        &root,
        &["init", "-q", "-b", "main", "--ref-format=reftable"],
    );
    let repo = TestRepo::open(&sb.env, &root);
    repo.write("a.txt", "a");
    repo.commit_all("a");
    let before = repo.snapshot();
    let out = parhel_with_failpoints(
        &sb.env,
        &root,
        &add_args(&sb.path("rt-feat"), &["--new-branch", "f"]),
        "after_clone=error",
    );
    assert_error(&out, "failpoint");
    before.assert_same(&repo.snapshot());
}

// I-106: shallow clone as source.
#[test]
fn i106_shallow_clone_source() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let url = format!("file://{}", src.root.display());
    git_in(
        &sb.env,
        &sb.root,
        &["clone", "-q", "--depth", "1", &url, "shallow"],
    );
    let shallow = TestRepo::open(&sb.env, &sb.path("shallow"));
    with_heavy_ignored(&shallow);
    let wt = add_ok(
        &sb,
        &shallow.root,
        &sb.path("shallow-feat"),
        &["--new-branch", "f"],
    );
    wt.assert_clean();
    assert_eq!(wt.read("target/debug/app"), "binary");
}

/// Blobless partial clone of a repo whose older commit has different content.
fn partial_clone(sb: &Sandbox) -> (TestRepo, TestRepo, String) {
    let origin = project(sb);
    origin.write("data.txt", "old content\n");
    let old = origin.commit_all("old");
    origin.write("data.txt", "new content\n");
    origin.commit_all("new");
    origin.git(&["config", "uploadpack.allowFilter", "true"]);
    let url = format!("file://{}", origin.root.display());
    git_in(
        &sb.env,
        &sb.root,
        &["clone", "-q", "--filter=blob:none", &url, "partial"],
    );
    let partial = TestRepo::open(&sb.env, &sb.path("partial"));
    (origin, partial, old)
}

// I-107: partial clone — target needs blobs fetched from the promisor remote.
#[test]
fn i107_partial_clone_fetches_missing_blobs() {
    let sb = Sandbox::new();
    let (_origin, partial, old) = partial_clone(&sb);
    let wt = add_ok(
        &sb,
        &partial.root,
        &sb.path("pc-feat"),
        &["--new-branch", "f", &old],
    );
    wt.assert_clean();
    assert_eq!(wt.read("data.txt"), "old content\n");
}

// I-107b: partial clone offline with missing blobs -> git error, no trace.
#[test]
fn i107b_partial_clone_offline_rolls_back() {
    let sb = Sandbox::new();
    let (origin, partial, old) = partial_clone(&sb);
    fs::rename(&origin.root, sb.path("origin-gone")).unwrap();
    let before = partial.snapshot();
    let out = parhel(
        &sb.env,
        &partial.root,
        &add_args(&sb.path("pc-feat"), &["--new-branch", "f", &old]),
    );
    assert_error(&out, "git-failed");
    before.assert_same(&partial.snapshot());
    assert!(!sb.path("pc-feat").exists());
    assert_no_staging(&[&sb.root]);
}

// I-108: destination inside another (non-source) linked worktree's ignored dir.
#[test]
fn i108_destination_inside_another_worktree() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let other = src.linked_worktree(&sb.path("other"), "o");
    other.mkdir(".worktrees");
    let wt = add_ok(
        &sb,
        &src.root,
        &other.path(".worktrees/x"),
        &["--new-branch", "x"],
    );
    wt.assert_clean();
    other.assert_clean();
    assert_eq!(other.current_branch().as_deref(), Some("o"));
}

// I-109: destination inside a submodule working tree is refused.
#[test]
fn i109_destination_inside_submodule_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let lib = sb.repo("lib");
    src.add_submodule(&lib, "libs/lib");
    let before = src.snapshot();
    let dest = src.path("libs/lib/wt");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "x", "--submodules=reinit"]),
    );
    assert_error(&out, "dest-inside-submodule");
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
}
