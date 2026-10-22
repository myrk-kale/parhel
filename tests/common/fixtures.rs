//! Canonical source repositories used across product tests.

use std::path::Path;

use super::repo::{Sandbox, TestRepo};

/// A typical project at `<sandbox>/src`: tracked code plus heavy ignored dirs
/// (`target/`, `node_modules/`) whose presence in the new worktree is the point
/// of copy-on-write. Clean status, on `main`.
pub fn project(sb: &Sandbox) -> TestRepo {
    let r = sb.repo("src");
    r.write("src/main.rs", "fn main() {}\n");
    r.write("Cargo.toml", "[package]\nname = \"demo\"\n");
    r.commit_all("code");
    r.ignore(&["target/", "node_modules/", ".worktrees/"]);
    r.write("target/debug/app", "binary");
    r.write("node_modules/pkg/index.js", "module.exports = 1;\n");
    r.assert_clean();
    r
}

/// `["add", <dest>, extra...]` as owned strings.
pub fn add_args(dest: &Path, extra: &[&str]) -> Vec<String> {
    let mut v = vec!["add".to_owned(), dest.to_str().unwrap().to_owned()];
    v.extend(extra.iter().map(|s| (*s).to_owned()));
    v
}
