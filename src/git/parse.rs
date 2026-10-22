//! Parsers for git's machine-readable output. Pure functions over bytes.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::domain::{Error, ErrorCode, Result};

fn malformed(what: &str, detail: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::GitFailed,
        format!("unexpected `git {what}` output: {detail}"),
    )
}

fn path_from(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// NUL-separated fields, without the empty tail after the final NUL.
fn fields(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    let trimmed = output.strip_suffix(b"\0").unwrap_or(output);
    trimmed
        .split(|&b| b == 0)
        .filter(move |_| !trimmed.is_empty())
}

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
    let rest = output
        .trim()
        .strip_prefix("git version ")
        .ok_or_else(|| malformed("--version", output.trim()))?;
    let token = rest.split_whitespace().next().unwrap_or_default();
    let mut parts = token.split('.');
    let mut number = |required: bool| -> Result<u32> {
        match parts.next().map(str::parse::<u32>) {
            Some(Ok(n)) => Ok(n),
            None | Some(Err(_)) if !required => Ok(0),
            _ => Err(malformed("--version", output.trim())),
        }
    };
    Ok(GitVersion {
        major: number(true)?,
        minor: number(true)?,
        patch: number(false)?,
    })
}

/// `git-too-old` when `version < MIN_GIT_VERSION`.
pub fn ensure_supported(version: GitVersion) -> Result<()> {
    if version < MIN_GIT_VERSION {
        return Err(Error::new(
            ErrorCode::GitTooOld,
            format!(
                "git {}.{}.{} is too old; parhel needs git {}.{} or newer",
                version.major,
                version.minor,
                version.patch,
                MIN_GIT_VERSION.major,
                MIN_GIT_VERSION.minor
            ),
        ));
    }
    Ok(())
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
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for field in output.split(|&b| b == 0) {
        if field.is_empty() {
            entries.extend(current.take());
            continue;
        }
        let (key, value) = match field.iter().position(|&b| b == b' ') {
            Some(i) => (&field[..i], Some(&field[i + 1..])),
            None => (field, None),
        };
        if key == b"worktree" {
            entries.extend(current.take());
            current = Some(WorktreeEntry {
                path: path_from(value.unwrap_or_default()),
                ..WorktreeEntry::default()
            });
            continue;
        }
        let entry = current
            .as_mut()
            .ok_or_else(|| malformed("worktree list", "attribute before `worktree` line"))?;
        let text = || String::from_utf8_lossy(value.unwrap_or_default()).into_owned();
        match key {
            b"HEAD" => entry.head = Some(text()),
            b"branch" => entry.branch = Some(text()),
            b"bare" => entry.bare = true,
            b"detached" => entry.detached = true,
            b"locked" => entry.locked = Some(text()),
            b"prunable" => entry.prunable = Some(text()),
            _ => {}
        }
    }
    entries.extend(current);
    Ok(entries)
}

/// Whether `git status --porcelain=v1 -z --untracked-files=all --ignored=no` output
/// describes any change. Ignored entries (`!!`) never count.
#[must_use]
pub fn status_is_dirty(output: &[u8]) -> bool {
    let mut records = fields(output);
    while let Some(record) = records.next() {
        if record.len() < 2 {
            continue;
        }
        if record.starts_with(b"!!") {
            continue;
        }
        if matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C') {
            records.next();
        }
        return true;
    }
    false
}

/// Gitlink (submodule) paths from `git ls-files --stage -z` (mode `160000`).
pub fn parse_gitlinks(output: &[u8]) -> Result<Vec<PathBuf>> {
    let mut links = Vec::new();
    for record in fields(output) {
        let tab = record
            .iter()
            .position(|&b| b == b'\t')
            .ok_or_else(|| malformed("ls-files --stage", "missing tab"))?;
        if record.starts_with(b"160000 ") {
            links.push(path_from(&record[tab + 1..]));
        }
    }
    Ok(links)
}

/// Parse `git rev-parse <rev>` output into a full object id (SHA-1 or SHA-256).
pub fn parse_object_id(output: &str) -> Result<String> {
    let oid = output.trim();
    let valid = matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if valid {
        Ok(oid.to_owned())
    } else {
        Err(malformed("rev-parse", oid))
    }
}

/// Parse `git config --type=bool --get <key>` output; absent key (`None`) is `false`.
pub fn parse_config_bool(output: Option<&str>) -> Result<bool> {
    match output.map(str::trim) {
        None | Some("" | "false") => Ok(false),
        Some("true") => Ok(true),
        Some(other) => Err(malformed("config --type=bool", other)),
    }
}

/// Paths flagged `assume-unchanged` (lowercase tag) or `skip-worktree` (`S`) in
/// `git ls-files -v -z` output.
pub fn parse_flagged_entries(output: &[u8]) -> Result<Vec<PathBuf>> {
    let mut flagged = Vec::new();
    for record in fields(output) {
        let (Some(&tag), Some(b' ')) = (record.first(), record.get(1)) else {
            return Err(malformed("ls-files -v", "missing tag"));
        };
        if tag.is_ascii_lowercase() || tag == b'S' {
            flagged.push(path_from(&record[2..]));
        }
    }
    Ok(flagged)
}

/// Operation in progress given the names of marker files present in the worktree's
/// git dir (`MERGE_HEAD`, `rebase-merge`, `rebase-apply`, `rebase-apply/applying`,
/// `CHERRY_PICK_HEAD`, `REVERT_HEAD`). `BISECT_LOG` is not an operation that
/// leaves files modified, so it is ignored.
#[must_use]
pub fn operation_from_markers(present: &[&str]) -> Option<&'static str> {
    let has = |m: &str| present.contains(&m);
    if has("rebase-apply/applying") {
        Some("am")
    } else if has("rebase-merge") || has("rebase-apply") {
        Some("rebase")
    } else if has("MERGE_HEAD") {
        Some("merge")
    } else if has("CHERRY_PICK_HEAD") {
        Some("cherry-pick")
    } else if has("REVERT_HEAD") {
        Some("revert")
    } else {
        None
    }
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
