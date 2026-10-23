//! I-A01..I-A02: macOS ACLs (`chmod +a`) travel with the files. `clonefile(2)` drops
//! them (and gives the clone those of the directory it lands in), so the copy and
//! the clone both set them again; a default ACL on the destination directory must not
//! leak into the worktree. (Linux POSIX ACLs: I-L44, I-L45.)
#![cfg(target_os = "macos")]

mod common;

use std::path::Path;
use std::process::Command;

use common::*;

fn chmod_acl(path: &Path, ace: &str) {
    let st = Command::new("chmod")
        .arg("+a")
        .arg(ace)
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success(), "chmod +a {ace} {}", path.display());
}

/// The ACL entries of `path` itself, as `ls -led` prints them.
fn acl_lines(path: &Path) -> Vec<String> {
    let out = Command::new("ls").arg("-led").arg(path).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .map(|l| l.trim().to_owned())
        .collect()
}

const READ: &str = "user:_www allow read";
const LIST: &str = "user:_www allow list,search,file_inherit,directory_inherit";
const SPOTLIGHT: &str = "user:_spotlight allow read,file_inherit,directory_inherit";

struct Fixture {
    sb: Sandbox,
    src: TestRepo,
}

/// `.env` and `secrets/` are ignored (cloned as entries of their own), `secrets/.env`
/// is nested. Everything created in the sandbox afterwards inherits `SPOTLIGHT`.
fn fixture() -> Fixture {
    let sb = Sandbox::new();
    let src = project(&sb);
    src.ignore(&[".env", "secrets/"]);
    src.write(".env", "TOKEN=1\n");
    src.write("secrets/.env", "TOKEN=2\n");
    chmod_acl(&src.path(".env"), READ);
    chmod_acl(&src.path("secrets"), LIST);
    chmod_acl(&src.path("secrets/.env"), READ);
    chmod_acl(&sb.root, SPOTLIGHT);
    Fixture { sb, src }
}

fn add(fx: &Fixture, mode: &[&str]) -> std::path::PathBuf {
    let dest = fx.sb.path("dest");
    let mut args = vec!["--new-branch", "a"];
    args.extend(mode);
    assert_success(&parhel(&fx.sb.env, &fx.src.root, &add_args(&dest, &args)));
    dest
}

fn assert_same_acl(fx: &Fixture, dest: &Path, rel: &str) {
    assert_eq!(
        acl_lines(&dest.join(rel)),
        acl_lines(&fx.src.path(rel)),
        "ACL of `{rel}`"
    );
}

// I-A01: `--copy` gives every entry the ACL of its source, and no inherited one.
#[test]
fn ia01_copy_preserves_acls_and_inherits_none() {
    let fx = fixture();
    let dest = add(&fx, &["--copy"]);
    for rel in [".env", "secrets", "secrets/.env", "target/debug/app", ""] {
        assert_same_acl(&fx, &dest, rel);
    }
}

// I-A02: a clone does for the entries it makes itself; below a cloned directory
// `clonefile(2)` carries no ACLs, which is why `--copy` exists.
#[test]
fn ia02_clone_preserves_the_acls_of_the_entries_it_clones() {
    let fx = fixture();
    let dest = add(&fx, &[]);
    for rel in [".env", "secrets", "target/debug/app", ""] {
        assert_same_acl(&fx, &dest, rel);
    }
}
