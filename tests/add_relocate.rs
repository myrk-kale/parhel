//! I-150..I-158: copied files that recorded the source's absolute path are
//! relocated to the new worktree (language-agnostic), and `.parhelignore` keeps
//! paths out of the copy.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use common::*;

fn add_ok(sb: &Sandbox, src: &TestRepo, dest: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["--new-branch", "r"];
    args.extend_from_slice(extra);
    let out = parhel(&sb.env, &src.root, &add_args(dest, &args));
    assert_success(&out);
    out
}

/// An ignored `env/` that behaves like a tool-generated environment: an executable
/// script, a config and an absolute symlink, all recording the source's path.
fn with_environment(src: &TestRepo) {
    src.ignore(&["env/"]);
    let root = src.root.display();
    src.write("env/data/marker", "data");
    src.write(
        "env/bin/tool",
        format!("#!/bin/sh\necho {root}/env/data/marker\n"),
    );
    src.chmod("env/bin/tool", 0o755);
    src.write(
        "env/config.json",
        format!("{{\"root\": \"{root}\", \"cache\": \"{root}/env/data\"}}\n"),
    );
    symlink(src.path("env/data"), src.path("env/current")).unwrap();
}

// I-150
#[test]
fn i150_relocates_scripts_configs_and_absolute_symlinks() {
    let sb = Sandbox::new();
    let src = project(&sb);
    with_environment(&src);
    let source_tree = tree_digest(&src.root);
    let dest = sb.path("dest");
    let out = add_ok(&sb, &src, &dest, &[]);

    let run = Command::new(dest.join("env/bin/tool")).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        dest.join("env/data/marker").to_str().unwrap()
    );
    let config = fs::read_to_string(dest.join("env/config.json")).unwrap();
    assert!(!config.contains(src.root.to_str().unwrap()), "{config}");
    assert!(config.contains(dest.to_str().unwrap()), "{config}");
    assert_eq!(
        fs::read_link(dest.join("env/current")).unwrap(),
        dest.join("env/data")
    );
    assert_eq!(
        fs::metadata(dest.join("env/bin/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        tree_digest(&src.root),
        source_tree,
        "the source is never modified"
    );
    assert!(stdout(&out).contains("Relocated 3"), "{}", stdout(&out));
    TestRepo::open(&sb.env, &dest).assert_clean();
}

// I-151: the reported case, with a real Python virtual environment.
#[test]
fn i151_python_venv_points_at_the_new_worktree() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.ignore(&[".venv/"]);
    let made = Command::new("python3")
        .args(["-m", "venv", ".venv"])
        .current_dir(&src.root)
        .status();
    assert!(made.is_ok_and(|s| s.success()), "setup: python3 -m venv");
    let dest = sb.path("dest");
    add_ok(&sb, &src, &dest, &[]);

    let python = dest.join(".venv/bin/python");
    let prefix = Command::new(&python)
        .args(["-c", "import sys; print(sys.prefix)"])
        .current_dir("/")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&prefix.stdout).trim(),
        dest.join(".venv").to_str().unwrap()
    );
    let pip = Command::new(dest.join(".venv/bin/pip"))
        .arg("--version")
        .current_dir("/")
        .output()
        .unwrap();
    let pip = String::from_utf8_lossy(&pip.stdout).into_owned();
    assert!(
        pip.contains(dest.join(".venv").to_str().unwrap()),
        "pip runs from the source: {pip}"
    );
    let activate = fs::read_to_string(dest.join(".venv/bin/activate")).unwrap();
    assert!(
        !activate.contains(src.root.to_str().unwrap()),
        "activate still names the source"
    );
}

// I-152: paths that only look like the source are left alone.
#[test]
fn i152_similar_paths_are_not_rewritten() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let root = src.root.display();
    let content = format!("{root}-old/a\n{root}2/b\n{root}.bak/c\n/elsewhere{root}/d\n");
    src.write("target/notes.txt", &content);
    let dest = sb.path("dest");
    add_ok(&sb, &src, &dest, &[]);
    assert_eq!(
        fs::read_to_string(dest.join("target/notes.txt")).unwrap(),
        content
    );
}

// I-153: tracked files are git's business: never rewritten.
#[test]
fn i153_tracked_files_are_never_rewritten() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let content = format!("built in {}\n", src.root.display());
    src.write("BUILD_INFO", &content);
    src.commit_all("tracked path");
    let dest = sb.path("dest");
    add_ok(&sb, &src, &dest, &[]);
    assert_eq!(
        fs::read_to_string(dest.join("BUILD_INFO")).unwrap(),
        content
    );
    TestRepo::open(&sb.env, &dest).assert_clean();
}

// I-154: files over the limit are not read (and keep sharing storage).
#[test]
fn i154_large_files_are_left_as_cloned() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let mut content = format!("{}\n", src.root.display()).into_bytes();
    content.resize((1 << 20) + 1, b'x');
    fs::write(src.path("target/big.log"), &content).unwrap();
    let dest = sb.path("dest");
    add_ok(&sb, &src, &dest, &[]);
    assert_eq!(fs::read(dest.join("target/big.log")).unwrap(), content);
    assert!(common::cow::shares_storage(
        &src.path("target/big.log"),
        &dest.join("target/big.log")
    ));
}

// I-155: binaries that mention the source are reported, never edited.
#[test]
fn i155_binaries_are_reported_not_edited() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let mut bin = b"\x7fELF\0\0".to_vec();
    bin.extend_from_slice(src.root.to_str().unwrap().as_bytes());
    bin.extend_from_slice(b"/lib\0");
    fs::write(src.path("target/debug/libthing.so"), &bin).unwrap();
    let dest = sb.path("dest");
    let out = add_ok(&sb, &src, &dest, &[]);
    assert_eq!(
        fs::read(dest.join("target/debug/libthing.so")).unwrap(),
        bin
    );
    let err = stderr(&out);
    assert!(
        err.contains("warning") && err.contains("target/debug/libthing.so"),
        "{err}"
    );
    assert!(err.contains(".parhelignore"), "{err}");
}

// I-156
#[test]
fn i156_no_relocate_copies_as_is() {
    let sb = Sandbox::new();
    let src = project(&sb);
    with_environment(&src);
    let dest = sb.path("dest");
    add_ok(&sb, &src, &dest, &["--no-relocate"]);
    let config = fs::read_to_string(dest.join("env/config.json")).unwrap();
    assert!(config.contains(src.root.to_str().unwrap()));
    assert_eq!(
        fs::read_link(dest.join("env/current")).unwrap(),
        src.path("env/data")
    );
}

// I-157: read-only files are relocated and stay read-only.
#[test]
fn i157_read_only_files_are_relocated() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write(
        "target/env.sh",
        format!("export ROOT={}\n", src.root.display()),
    );
    src.chmod("target/env.sh", 0o444);
    let dest = sb.path("dest");
    add_ok(&sb, &src, &dest, &[]);
    assert_eq!(
        fs::read_to_string(dest.join("target/env.sh")).unwrap(),
        format!("export ROOT={}\n", dest.display())
    );
    assert_eq!(
        fs::metadata(dest.join("target/env.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444
    );
}

// I-158: .parhelignore keeps untracked paths out of the copy (tracked files are
// always present: they come from git).
#[test]
fn i158_parhelignore_excludes_untracked_paths() {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.write("tools/keep.txt", "tracked");
    src.write(
        ".parhelignore",
        "# environments we rebuild per worktree\n.conda/\ntools/\n",
    );
    src.commit_all("parhelignore");
    src.ignore(&[".conda/"]);
    src.write(".conda/bin/python", "binary-ish");
    let source_tree = tree_digest(&src.root);
    let dest = sb.path("dest");
    add_ok(&sb, &src, &dest, &[]);
    assert!(!dest.join(".conda").exists(), ".conda/ must not be copied");
    assert_eq!(
        fs::read_to_string(dest.join("tools/keep.txt")).unwrap(),
        "tracked"
    );
    assert!(
        dest.join("node_modules/pkg/index.js").is_file(),
        "other ignored files still copied"
    );
    TestRepo::open(&sb.env, &dest).assert_clean();
    assert_eq!(tree_digest(&src.root), source_tree);
}

// I-159: a linked worktree of another repository inside an ignored folder is
// detached in the copy, so the copy can never move that repository's HEAD/index.
#[test]
fn i159_foreign_linked_worktree_is_detached_in_the_copy() {
    let sb = Sandbox::new();
    let src = project(&sb);
    let other = sb.repo("other");
    let nested = src.path("node_modules/vendored");
    other.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "vend",
        nested.to_str().unwrap(),
    ]);
    let other_head = other.branch_tip("vend");
    let dest = sb.path("dest");
    let out = add_ok(&sb, &src, &dest, &[]);
    assert!(
        !dest.join("node_modules/vendored/.git").exists(),
        "gitfile removed in the copy"
    );
    assert!(
        dest.join("node_modules/vendored/README.md").is_file(),
        "files kept"
    );
    assert!(
        stderr(&out).contains("node_modules/vendored"),
        "{}",
        stderr(&out)
    );
    assert!(
        nested.join(".git").is_file(),
        "the source's worktree is untouched"
    );
    assert_eq!(other.branch_tip("vend"), other_head);
    TestRepo::open(&sb.env, &nested).assert_clean();
}
