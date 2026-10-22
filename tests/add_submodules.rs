//! I-17..I-22: submodules (bug 2 of the original: cloned submodule gitfiles kept
//! pointing at the source, so operations in the destination changed the source).

mod common;

use std::fs;
use std::path::Path;

use common::*;

fn lib_repo(sb: &Sandbox, name: &str) -> TestRepo {
    let l = sb.repo(name);
    l.write("lib.rs", "v1\n");
    l.commit_all("lib v1");
    l.write("lib.rs", "v2\n");
    l.commit_all("lib v2");
    l
}

/// `src` (main checkout) with an initialized submodule at `libs/lib`.
fn with_submodule(sb: &Sandbox) -> (TestRepo, TestRepo) {
    let src = project(sb);
    let lib = lib_repo(sb, "lib");
    src.add_submodule(&lib, "libs/lib");
    src.assert_clean();
    (src, lib)
}

fn reinit_args(dest: &Path, branch: &str) -> Vec<String> {
    add_args(dest, &["--new-branch", branch, "--submodules=reinit"])
}

/// The submodule in `wt` must have its own git dir under the new worktree's admin dir.
fn assert_submodule_isolated(wt: &TestRepo, sub_rel: &str, source_sub_git_dir: &Path) {
    let sub = TestRepo::open(&wt.env, &wt.path(sub_rel));
    let sub_git_dir = sub.git_dir();
    assert_ne!(
        sub_git_dir, source_sub_git_dir,
        "must not reuse the source's submodule git dir"
    );
    assert!(
        sub_git_dir.starts_with(wt.git_dir()),
        "submodule git dir {} should live under {}",
        sub_git_dir.display(),
        wt.git_dir().display()
    );
}

// I-17
#[test]
fn i17_initialized_submodule_is_refused_by_default() {
    let sb = Sandbox::new();
    let (src, _lib) = with_submodule(&sb);
    let before = src.snapshot();
    let dest = sb.path("sub-wt");
    let out = parhel(&sb.env, &src.root, &add_args(&dest, &["--new-branch", "s"]));
    assert_error(&out, "submodules-present");
    let err = stderr(&out);
    assert!(
        err.contains("libs/lib") && err.contains("--submodules=reinit"),
        "{err}"
    );
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-18: from the main checkout. Original symptom: `git status` broken in the destination.
#[test]
fn i18_reinit_from_main_checkout() {
    let sb = Sandbox::new();
    let (src, _lib) = with_submodule(&sb);
    let src_sub = TestRepo::open(&sb.env, &src.path("libs/lib"));
    let src_sub_head = src_sub.head();
    let src_sub_git_dir = src_sub.git_dir();

    let dest = sb.path("sub-wt");
    assert_success(&parhel(&sb.env, &src.root, &reinit_args(&dest, "s")));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("libs/lib/lib.rs"), "v2\n");
    assert_submodule_isolated(&wt, "libs/lib", &src_sub_git_dir);

    let wt_sub = TestRepo::open(&sb.env, &wt.path("libs/lib"));
    assert_eq!(wt_sub.head(), src_sub_head);
    wt_sub.git(&["checkout", "-q", "HEAD~1"]);
    assert_eq!(wt_sub.read("lib.rs"), "v1\n");
    assert_eq!(
        src_sub.head(),
        src_sub_head,
        "source submodule HEAD unchanged"
    );
    assert_eq!(
        src_sub.read("lib.rs"),
        "v2\n",
        "source submodule files unchanged"
    );
    src.assert_clean();
}

// I-19: from a linked worktree — the exact reproduction of the original bug.
#[test]
fn i19_reinit_from_linked_worktree_isolates_submodule() {
    let sb = Sandbox::new();
    let (src, _lib) = with_submodule(&sb);
    let linked = src.linked_worktree(&sb.path("linked"), "lb");
    linked.git(&["submodule", "update", "--init", "-q"]);
    let linked_sub = TestRepo::open(&sb.env, &linked.path("libs/lib"));
    let linked_sub_head = linked_sub.head();
    let linked_sub_branch = linked_sub.current_branch();

    let dest = sb.path("sub-wt");
    assert_success(&parhel(&sb.env, &linked.root, &reinit_args(&dest, "s")));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_submodule_isolated(&wt, "libs/lib", &linked_sub.git_dir());

    let wt_sub = TestRepo::open(&sb.env, &wt.path("libs/lib"));
    wt_sub.git(&["checkout", "-q", "-b", "hack", "HEAD~1"]);
    wt_sub.write("hack.txt", "x");
    wt_sub.commit_all("hack");

    assert_eq!(linked_sub.head(), linked_sub_head);
    assert_eq!(linked_sub.current_branch(), linked_sub_branch);
    assert_eq!(linked_sub.read("lib.rs"), "v2\n");
    assert!(!linked_sub.exists("hack.txt"));
    linked.assert_clean();
}

// I-20
#[test]
fn i20_nested_submodules_are_reinitialized_recursively() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let leaf = sb.repo("leaf");
    leaf.write("leaf.txt", "leaf\n");
    leaf.commit_all("leaf");
    let lib = lib_repo(&sb, "lib");
    lib.add_submodule(&leaf, "deps/leaf");
    src.add_submodule(&lib, "libs/lib");
    src.git(&["submodule", "update", "--init", "--recursive", "-q"]);
    let src_leaf_git_dir = TestRepo::open(&sb.env, &src.path("libs/lib/deps/leaf")).git_dir();

    let dest = sb.path("nested");
    assert_success(&parhel(&sb.env, &src.root, &reinit_args(&dest, "n")));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("libs/lib/deps/leaf/leaf.txt"), "leaf\n");
    let status = wt.git(&["submodule", "status", "--recursive"]);
    assert!(
        status.lines().all(|l| l.starts_with(' ')),
        "every submodule initialized and at the recorded commit:\n{status}"
    );
    assert_submodule_isolated(&wt, "libs/lib/deps/leaf", &src_leaf_git_dir);
}

// I-21: declared but not initialized submodules are not a blocker.
#[test]
fn i21_uninitialized_submodule_does_not_block() {
    let sb = Sandbox::new();
    let (src, _lib) = with_submodule(&sb);
    src.git(&["submodule", "deinit", "-q", "-f", "libs/lib"]);
    src.assert_clean();
    let dest = sb.path("deinit");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "d"]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert!(wt.path("libs/lib").is_dir());
    assert_eq!(fs::read_dir(wt.path("libs/lib")).unwrap().count(), 0);
}

// I-22: reinit borrows objects from the source; works with the remote gone.
#[test]
fn i22_reinit_works_offline() {
    let sb = Sandbox::new();
    let (src, lib) = with_submodule(&sb);
    let url = lib.root.to_str().unwrap().to_owned();
    fs::rename(&lib.root, sb.path("lib-moved")).unwrap();

    let dest = sb.path("offline");
    let out = parhel(&sb.env, &src.root, &reinit_args(&dest, "o"));
    assert_success(&out);
    let wt = TestRepo::open(&sb.env, &dest);
    wt.assert_clean();
    assert_eq!(wt.read("libs/lib/lib.rs"), "v2\n");
    let wt_sub = TestRepo::open(&sb.env, &wt.path("libs/lib"));
    assert_eq!(
        wt_sub.git(&["config", "remote.origin.url"]),
        url,
        "keeps the declared URL"
    );
}
