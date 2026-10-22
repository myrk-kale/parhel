//! Parsers for git's machine-readable output. Pure functions over bytes.

use std::path::PathBuf;

use crate::domain::Result;

/// Oldest git supported: `git worktree repair` appeared in 2.29.
pub const MIN_GIT_VERSION: GitVersion = GitVersion {
    major: 2,
    minor: 29,
    patch: 0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// Parse `git --version` output, tolerating vendor suffixes
/// (`git version 2.39.5 (Apple Git-154)`, `2.45.1.windows.1`). Errors: `git-failed`.
pub fn parse_version(output: &str) -> Result<GitVersion> {
    let _ = output;
    todo!("parse_version")
}

/// `git-too-old` when `version < MIN_GIT_VERSION`.
pub fn ensure_supported(version: GitVersion) -> Result<()> {
    let _ = version;
    todo!("ensure_supported")
}

/// One record of `git worktree list --porcelain -z`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// Commit id, absent for bare entries.
    pub head: Option<String>,
    /// Full ref name (`refs/heads/x`) when on a branch.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    /// `Some("")` when locked without a reason.
    pub locked: Option<String>,
    /// `Some(reason)` when git reports the entry as prunable.
    pub prunable: Option<String>,
}

/// Parse `git worktree list --porcelain -z`. Errors: `git-failed` on malformed input.
pub fn parse_worktree_list(output: &[u8]) -> Result<Vec<WorktreeEntry>> {
    let _ = output;
    todo!("parse_worktree_list")
}

/// Whether `git status --porcelain=v1 -z --untracked-files=all --ignored=no` output
/// describes any change. Ignored entries (`!!`) never count.
#[must_use]
pub fn status_is_dirty(output: &[u8]) -> bool {
    let _ = output;
    todo!("status_is_dirty")
}

/// Gitlink (submodule) paths from `git ls-files --stage -z` (mode `160000`).
pub fn parse_gitlinks(output: &[u8]) -> Result<Vec<PathBuf>> {
    let _ = output;
    todo!("parse_gitlinks")
}

/// Parse `git rev-parse <rev>` output into a full object id (SHA-1 or SHA-256).
pub fn parse_object_id(output: &str) -> Result<String> {
    let _ = output;
    todo!("parse_object_id")
}

/// Parse `git config --type=bool --get <key>` output; absent key (`None`) is `false`.
pub fn parse_config_bool(output: Option<&str>) -> Result<bool> {
    let _ = output;
    todo!("parse_config_bool")
}

/// Paths flagged `assume-unchanged` (lowercase tag) or `skip-worktree` (`S`) in
/// `git ls-files -v -z` output.
pub fn parse_flagged_entries(output: &[u8]) -> Result<Vec<PathBuf>> {
    let _ = output;
    todo!("parse_flagged_entries")
}

/// Operation in progress given the names of marker files present in the worktree's
/// git dir (`MERGE_HEAD`, `rebase-merge`, `rebase-apply`, `rebase-apply/applying`,
/// `CHERRY_PICK_HEAD`, `REVERT_HEAD`). `BISECT_LOG` is not an operation that
/// leaves files modified, so it is ignored.
#[must_use]
pub fn operation_from_markers(present: &[&str]) -> Option<&'static str> {
    let _ = present;
    todo!("operation_from_markers")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ErrorCode;
    use rstest::rstest;

    // U-PARSE-01..05: version parsing.
    #[rstest]
    #[case::plain("git version 2.47.1\n", (2, 47, 1))]
    #[case::apple("git version 2.39.5 (Apple Git-154)\n", (2, 39, 5))]
    #[case::windows("git version 2.45.1.windows.1\n", (2, 45, 1))]
    #[case::two_components("git version 2.30\n", (2, 30, 0))]
    #[case::rc("git version 2.48.0.rc1\n", (2, 48, 0))]
    fn parses_version(#[case] out: &str, #[case] want: (u32, u32, u32)) {
        let v = parse_version(out).unwrap();
        assert_eq!((v.major, v.minor, v.patch), want);
    }

    #[rstest]
    #[case("")]
    #[case("hub version 2.14.2")]
    #[case("git version abc")]
    fn rejects_garbage_version(#[case] out: &str) {
        assert_eq!(parse_version(out).unwrap_err().code, ErrorCode::GitFailed);
    }

    #[rstest]
    #[case((2, 29, 0), true)]
    #[case((2, 47, 1), true)]
    #[case((3, 0, 0), true)]
    #[case((2, 28, 9), false)]
    #[case((1, 99, 0), false)]
    fn enforces_minimum_version(#[case] v: (u32, u32, u32), #[case] ok: bool) {
        let got = ensure_supported(GitVersion {
            major: v.0,
            minor: v.1,
            patch: v.2,
        });
        if ok {
            got.unwrap();
        } else {
            assert_eq!(got.unwrap_err().code, ErrorCode::GitTooOld);
        }
    }

    // U-PARSE-06..09: worktree list.
    const SHA_A: &str = "1111111111111111111111111111111111111111";
    const SHA_B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn parses_worktree_list_variants() {
        let out = format!(
            "worktree /w/main\0HEAD {SHA_A}\0branch refs/heads/main\0\0\
             worktree /w/det\0HEAD {SHA_B}\0detached\0\0\
             worktree /w/locked wt\0HEAD {SHA_A}\0branch refs/heads/x\0locked reason here\0\0\
             worktree /w/gone\0HEAD {SHA_A}\0branch refs/heads/y\0locked\0prunable gitdir file points to non-existent location\0\0"
        );
        let got = parse_worktree_list(out.as_bytes()).unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].path, PathBuf::from("/w/main"));
        assert_eq!(got[0].branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(got[0].head.as_deref(), Some(SHA_A));
        assert!(got[1].detached && got[1].branch.is_none());
        assert_eq!(got[2].path, PathBuf::from("/w/locked wt"));
        assert_eq!(got[2].locked.as_deref(), Some("reason here"));
        assert_eq!(got[3].locked.as_deref(), Some(""));
        assert!(got[3].prunable.as_deref().unwrap().contains("non-existent"));
    }

    #[test]
    fn parses_bare_main_entry() {
        let out =
            format!("worktree /w/repo.git\0bare\0\0worktree /w/wt\0HEAD {SHA_A}\0detached\0\0");
        let got = parse_worktree_list(out.as_bytes()).unwrap();
        assert!(got[0].bare && got[0].head.is_none());
        assert_eq!(got[1].path, PathBuf::from("/w/wt"));
    }

    #[test]
    fn worktree_path_with_newline_survives_z_format() {
        let out = format!("worktree /w/odd\nname\0HEAD {SHA_A}\0detached\0\0");
        let got = parse_worktree_list(out.as_bytes()).unwrap();
        assert_eq!(got[0].path, PathBuf::from("/w/odd\nname"));
    }

    #[test]
    fn rejects_record_without_worktree_line() {
        let out = format!("HEAD {SHA_A}\0detached\0\0");
        assert_eq!(
            parse_worktree_list(out.as_bytes()).unwrap_err().code,
            ErrorCode::GitFailed
        );
    }

    // U-PARSE-10..12: dirty detection.
    #[rstest]
    #[case::empty("", false)]
    #[case::only_ignored("!! target/\0!! node_modules/\0", false)]
    #[case::modified(" M src/a.rs\0", true)]
    #[case::staged("M  src/a.rs\0", true)]
    #[case::untracked("?? new.txt\0", true)]
    #[case::deleted(" D gone.txt\0", true)]
    #[case::rename_with_orig("R  new.txt\0old.txt\0", true)]
    #[case::ignored_then_untracked("!! target/\0?? x\0", true)]
    fn detects_dirty(#[case] out: &str, #[case] dirty: bool) {
        assert_eq!(status_is_dirty(out.as_bytes()), dirty);
    }

    // U-PARSE-13..14: gitlinks.
    #[test]
    fn extracts_only_gitlinks() {
        let out = format!(
            "100644 {SHA_A} 0\tREADME.md\0160000 {SHA_B} 0\tlibs/a\0\
             120000 {SHA_A} 0\tlink\0160000 {SHA_A} 0\tsub with space\0"
        );
        assert_eq!(
            parse_gitlinks(out.as_bytes()).unwrap(),
            vec![PathBuf::from("libs/a"), PathBuf::from("sub with space")]
        );
    }

    #[test]
    fn no_gitlinks_yields_empty() {
        let out = format!("100755 {SHA_A} 0\tbin/run\0");
        assert_eq!(
            parse_gitlinks(out.as_bytes()).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    // U-PARSE-15..16: object ids.
    #[rstest]
    #[case::sha1(SHA_A)]
    #[case::sha256("3333333333333333333333333333333333333333333333333333333333333333")]
    fn parses_object_ids(#[case] oid: &str) {
        assert_eq!(parse_object_id(&format!("{oid}\n")).unwrap(), oid);
    }

    #[rstest]
    #[case("")]
    #[case("HEAD")]
    #[case("abc123")]
    #[case("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")]
    fn rejects_invalid_object_ids(#[case] out: &str) {
        assert_eq!(parse_object_id(out).unwrap_err().code, ErrorCode::GitFailed);
    }

    // U-PARSE-17: sparse-checkout config.
    #[rstest]
    #[case(None, false)]
    #[case(Some("true\n"), true)]
    #[case(Some("false\n"), false)]
    fn parses_config_bool(#[case] out: Option<&str>, #[case] want: bool) {
        assert_eq!(parse_config_bool(out).unwrap(), want);
    }

    #[test]
    fn rejects_non_bool_config() {
        assert_eq!(
            parse_config_bool(Some("maybe")).unwrap_err().code,
            ErrorCode::GitFailed
        );
    }

    // U-PARSE-18..19: index flags.
    #[test]
    fn extracts_assume_unchanged_and_skip_worktree() {
        let out = b"H tracked.txt\0h assumed.txt\0S skipped.txt\0s both.txt\0? untracked\0";
        assert_eq!(
            parse_flagged_entries(out).unwrap(),
            vec![
                PathBuf::from("assumed.txt"),
                PathBuf::from("skipped.txt"),
                PathBuf::from("both.txt"),
            ]
        );
    }

    #[test]
    fn no_flags_yields_empty() {
        assert_eq!(
            parse_flagged_entries(b"H a\0H dir/b c\0").unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    // U-PARSE-20: operation markers.
    #[rstest]
    #[case(&[], None)]
    #[case(&["MERGE_HEAD"], Some("merge"))]
    #[case(&["rebase-merge"], Some("rebase"))]
    #[case(&["rebase-apply"], Some("rebase"))]
    #[case(&["rebase-apply", "rebase-apply/applying"], Some("am"))]
    #[case(&["CHERRY_PICK_HEAD"], Some("cherry-pick"))]
    #[case(&["REVERT_HEAD"], Some("revert"))]
    #[case(&["BISECT_LOG"], None)]
    #[case(&["ORIG_HEAD", "FETCH_HEAD"], None)]
    fn detects_operation(#[case] markers: &[&str], #[case] want: Option<&str>) {
        assert_eq!(operation_from_markers(markers), want);
    }

    // U-PARSE-P1..P3
    mod props {
        use super::*;
        use proptest::prelude::*;

        fn ignored_records(paths: &[String]) -> String {
            paths.iter().fold(String::new(), |mut acc, p| {
                acc.push_str("!! ");
                acc.push_str(p);
                acc.push('\0');
                acc
            })
        }

        proptest! {
            #[test]
            fn version_round_trips(a in 0u32..10, b in 0u32..100, c in 0u32..100) {
                let v = parse_version(&format!("git version {a}.{b}.{c}\n")).unwrap();
                prop_assert_eq!((v.major, v.minor, v.patch), (a, b, c));
            }

            #[test]
            fn only_ignored_entries_are_clean(paths in prop::collection::vec("[a-z/]{1,10}", 0..10)) {
                let out = ignored_records(&paths);
                prop_assert!(!status_is_dirty(out.as_bytes()));
            }

            #[test]
            fn any_change_entry_is_dirty(
                ignored in prop::collection::vec("[a-z]{1,8}", 0..5),
                xy in prop::sample::select(vec![" M", "M ", "A ", " D", "??", "UU", " T"]),
            ) {
                let mut out = ignored_records(&ignored);
                out.push_str(xy);
                out.push_str(" changed\0");
                prop_assert!(status_is_dirty(out.as_bytes()));
            }

            #[test]
            fn worktree_list_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
                let _ = parse_worktree_list(&bytes);
            }
        }
    }
}
