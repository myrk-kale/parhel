//! I-12..I-16: uncommitted changes in the source.

mod common;

use common::*;

fn refused_without_side_effects(sb: &Sandbox, src: &TestRepo) {
    let before = src.snapshot();
    let dest = sb.path("dirty-wt");
    let out = parhel(&sb.env, &src.root, &add_args(&dest, &["--new-branch", "d"]));
    assert_error(&out, "dirty-source");
    assert!(
        stderr(&out).contains("--allow-dirty"),
        "hint: {}",
        stderr(&out)
    );
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-12
#[test]
fn i12_modified_tracked_file_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("src/main.rs", "changed\n");
    refused_without_side_effects(&sb, &src);
}

// I-12b
#[test]
fn i12b_staged_change_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("src/main.rs", "staged\n");
    src.git(&["add", "src/main.rs"]);
    refused_without_side_effects(&sb, &src);
}

// I-13: untracked files count (parity with the original's --untracked-files=all).
#[test]
fn i13_untracked_file_is_refused() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("deep/new/untracked.txt", "u\n");
    refused_without_side_effects(&sb, &src);
}

// I-14: ignored changes are not "dirty".
#[test]
fn i14_ignored_changes_do_not_block() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("target/debug/new-artifact", "fresh build\n");
    let dest = sb.path("ok");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "ok"]),
    ));
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.read("target/debug/new-artifact"), "fresh build\n");
    wt.assert_clean();
}

// I-15: --allow-dirty carries every kind of local change.
#[test]
fn i15_allow_dirty_carries_local_changes() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("unstaged.txt", "base\n");
    src.write("staged.txt", "base\n");
    src.write("deleted.txt", "base\n");
    src.commit_all("files");
    src.write("unstaged.txt", "unstaged edit\n");
    src.write("staged.txt", "staged edit\n");
    src.git(&["add", "staged.txt"]);
    src.remove("deleted.txt");
    src.write("untracked.txt", "new\n");
    let src_status = src.status();

    let dest = sb.path("carried");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "c", "--allow-dirty"]),
    );
    assert_success(&out);
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.read("unstaged.txt"), "unstaged edit\n");
    assert_eq!(wt.read("staged.txt"), "staged edit\n");
    assert!(!wt.exists("deleted.txt"));
    assert_eq!(wt.read("untracked.txt"), "new\n");
    // Documented: the index is rebuilt from HEAD, so staged changes arrive unstaged.
    let status = wt.status();
    for line in [
        " M unstaged.txt",
        " M staged.txt",
        " D deleted.txt",
        "?? untracked.txt",
    ] {
        assert!(
            status.lines().any(|l| l == line),
            "missing `{line}` in:\n{status}"
        );
    }
    assert_eq!(
        src.status(),
        src_status,
        "source status untouched (staged stays staged)"
    );
}

// I-16a: --allow-dirty + different target, local change in a file the target does not touch.
#[test]
fn i16a_allow_dirty_with_different_ref_without_conflict() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let old = src.head();
    src.write("other.txt", "added later\n");
    src.commit_all("later");
    src.write("src/main.rs", "local edit\n");

    let dest = sb.path("carry-old");
    let args = add_args(&dest, &["--new-branch", "co", "--allow-dirty", &old]);
    assert_success(&parhel(&sb.env, &src.root, &args));
    let wt = TestRepo::open(&sb.env, &dest);
    assert_eq!(wt.head(), old);
    assert!(!wt.exists("other.txt"));
    assert_eq!(wt.read("src/main.rs"), "local edit\n");
    assert_eq!(wt.status(), " M src/main.rs");
}

// I-16b: --allow-dirty + different target touching the locally modified file.
#[test]
fn i16b_allow_dirty_with_conflicting_ref_rolls_back() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let old = src.head();
    src.write("src/main.rs", "committed later\n");
    src.commit_all("later");
    src.write("src/main.rs", "local edit\n");
    let before = src.snapshot();

    let dest = sb.path("conflict");
    let args = add_args(&dest, &["--new-branch", "cf", "--allow-dirty", &old]);
    let out = parhel(&sb.env, &src.root, &args);
    assert_error(&out, "checkout-conflict");
    assert!(
        stderr(&out).contains("src/main.rs"),
        "names the file: {}",
        stderr(&out)
    );
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
    assert_no_staging(&[&sb.root]);
}

// I-16c: untracked file in the source colliding with a tracked file of the target.
#[test]
fn i16c_allow_dirty_untracked_colliding_with_target_rolls_back() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.git(&["checkout", "-q", "-b", "has-notes"]);
    src.write("notes.txt", "tracked on branch\n");
    src.commit_all("notes");
    src.git(&["checkout", "-q", "main"]);
    src.write("notes.txt", "my untracked notes\n");
    let before = src.snapshot();

    let dest = sb.path("notes");
    let out = parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--branch", "has-notes", "--allow-dirty"]),
    );
    assert_error(&out, "checkout-conflict");
    assert!(!dest.exists());
    before.assert_same(&src.snapshot());
}
