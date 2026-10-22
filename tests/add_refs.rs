//! I-10..I-11: target commit different from the source HEAD.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::*;

/// Source with two commits differing in every way a checkout must reconcile.
/// Returns (repo, old commit, new commit); the source sits on the new one.
fn two_commits(sb: &Sandbox) -> (TestRepo, String, String) {
    let r = project(sb);
    r.write("mod.txt", "v1\n");
    r.write("del.txt", "will be deleted\n");
    r.write("old-name.txt", "renamed content\n");
    r.write("script.sh", "#!/bin/sh\n");
    r.write("nested/deep/file.txt", "deep v1\n");
    r.symlink("link", "mod.txt");
    let old = r.commit_all("old");

    r.write("mod.txt", "v2\n");
    r.remove("del.txt");
    fs::rename(r.path("old-name.txt"), r.path("new-name.txt")).unwrap();
    r.chmod("script.sh", 0o755);
    r.remove("link");
    r.symlink("link", "new-name.txt");
    r.write("added.txt", "only in new\n");
    r.remove("nested");
    let new = r.commit_all("new");
    r.assert_clean();
    (r, old, new)
}

fn mode(wt: &TestRepo, rel: &str) -> u32 {
    fs::metadata(wt.path(rel)).unwrap().permissions().mode() & 0o777
}

// I-10a: target older than source HEAD.
#[test]
fn i10a_checkout_older_commit_yields_exact_tree() {
    let sb = Sandbox::new();
    let (src, old, _new) = two_commits(&sb);
    let before = src.snapshot();
    let dest = sb.path("old-wt");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "o", &old]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);

    assert_eq!(wt.head(), old);
    wt.assert_clean();
    assert_eq!(wt.git(&["diff", "HEAD", "--stat"]), "");
    assert_eq!(wt.read("mod.txt"), "v1\n");
    assert_eq!(wt.read("del.txt"), "will be deleted\n");
    assert!(wt.exists("old-name.txt") && !wt.exists("new-name.txt"));
    assert_eq!(mode(&wt, "script.sh"), 0o644);
    assert_eq!(
        fs::read_link(wt.path("link")).unwrap().to_str(),
        Some("mod.txt")
    );
    assert!(!wt.exists("added.txt"));
    assert_eq!(wt.read("nested/deep/file.txt"), "deep v1\n");
    assert_eq!(
        wt.read("target/debug/app"),
        "binary",
        "ignored files still cloned"
    );

    let mut after = src.snapshot();
    after.worktrees.clone_from(&before.worktrees);
    after.branches.clone_from(&before.branches);
    after.admin_dirs.clone_from(&before.admin_dirs);
    before.assert_same(&after);
}

// I-10b: target newer than source HEAD.
#[test]
fn i10b_checkout_newer_commit_yields_exact_tree() {
    let sb = Sandbox::new();
    let (src, old, new) = two_commits(&sb);
    src.git(&["checkout", "-q", "--detach", &old]);
    src.assert_clean();
    let dest = sb.path("new-wt");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--branch", "main"]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);

    assert_eq!(wt.head(), new);
    wt.assert_clean();
    assert_eq!(wt.read("mod.txt"), "v2\n");
    assert!(!wt.exists("del.txt"));
    assert!(!wt.exists("old-name.txt") && wt.exists("new-name.txt"));
    assert_eq!(mode(&wt, "script.sh"), 0o755);
    assert_eq!(
        fs::read_link(wt.path("link")).unwrap().to_str(),
        Some("new-name.txt")
    );
    assert_eq!(wt.read("added.txt"), "only in new\n");
    assert!(!wt.exists("nested"), "emptied directories are removed");
    assert_eq!(
        wt.read("node_modules/pkg/index.js"),
        "module.exports = 1;\n"
    );
    assert_eq!(src.head(), old, "source untouched");
}

// I-10c: unrelated history (orphan branch) also works.
#[test]
fn i10c_checkout_unrelated_branch() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["checkout", "-q", "--orphan", "docs"]);
    src.git(&["rm", "-rq", "--cached", "."]);
    for f in ["README.md", "Cargo.toml", ".gitignore"] {
        src.remove(f);
    }
    src.remove("src");
    src.write("index.md", "docs\n");
    // Not `commit_all`: with .gitignore gone, target/ and node_modules/ would be added.
    src.git(&["add", "index.md"]);
    src.git(&["commit", "-q", "-m", "docs root"]);
    src.git(&["checkout", "-q", "-f", "main"]);
    assert!(
        !src.exists("index.md"),
        "git removes files tracked only on `docs`"
    );
    src.assert_clean();

    let dest = sb.path("docs-wt");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--branch", "docs"]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);
    assert!(wt.exists("index.md") && !wt.exists("src/main.rs") && !wt.exists("Cargo.toml"));
    // `docs` has no .gitignore, so the carried-over build output is untracked there —
    // exactly what `git switch docs` would show in the source itself.
    assert_eq!(
        wt.status(),
        "?? node_modules/pkg/index.js\n?? target/debug/app"
    );
    assert_eq!(wt.git(&["diff", "HEAD", "--stat"]), "");
}

// I-11: an ignored file in the source that is tracked in the target is replaced by
// the tracked version in the destination; the source copy is untouched.
#[test]
fn i11_ignored_file_colliding_with_tracked_target_file() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let base = src.head();
    src.git(&["checkout", "-q", "-b", "with-artifact"]);
    src.write("target/debug/app", "tracked artifact\n");
    src.git(&["add", "-f", "target/debug/app"]);
    src.git(&["commit", "-q", "-m", "track artifact"]);
    src.git(&["checkout", "-q", "main"]);
    src.write("target/debug/app", "binary");
    assert_eq!(src.head(), base);
    src.assert_clean();

    let dest = sb.path("artifact");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--branch", "with-artifact"]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("target/debug/app"), "tracked artifact\n");
    assert_eq!(src.read("target/debug/app"), "binary");
}
