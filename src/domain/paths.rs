//! Destination resolution and staging-path naming.
//!
//! Everything here is lexical: callers canonicalize the parts that already exist
//! (source root, destination parent) before calling in.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::error::Result;

/// Prefix of the per-run staging directory created next to the destination.
/// `doctor` recognizes leftovers by this prefix.
pub const STAGING_PREFIX: &str = ".parhel-staging-";

/// Where the destination sits relative to the source worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Outside the source worktree.
    Outside,
    /// Nested inside the source worktree (e.g. `.worktrees/feat`). The clone must
    /// exclude the staging directory so the worktree never contains itself.
    InsideSource { relative: PathBuf },
}

/// Resolve `raw` against `cwd` and normalize `.`/`..` lexically, without touching
/// the filesystem or following symlinks. Trailing slashes are dropped.
#[must_use]
pub fn resolve_destination(cwd: &Path, raw: &Path) -> PathBuf {
    let _ = (cwd, raw);
    todo!("paths::resolve_destination")
}

/// The final component of the destination, which also becomes the worktree id in
/// `.git/worktrees/<id>`. Fails with `usage` for paths without one (e.g. `/`).
pub fn destination_basename(destination: &Path) -> Result<&OsStr> {
    let _ = destination;
    todo!("paths::destination_basename")
}

/// Classify an absolute, normalized destination against the source worktree root
/// and the repository's common git dir.
///
/// Errors: `dest-is-source` when equal to the source root, `dest-inside-git-dir`
/// when inside the common git dir or the source's `.git`.
pub fn classify_destination(
    source_root: &Path,
    git_common_dir: &Path,
    destination: &Path,
) -> Result<Placement> {
    let _ = (source_root, git_common_dir, destination);
    todo!("paths::classify_destination")
}

/// Path where the worktree is assembled before being published: inside the
/// staging dir, named exactly like the destination so git's worktree id matches.
#[must_use]
pub fn staging_worktree_path(staging_dir: &Path, destination: &Path) -> PathBuf {
    let _ = (staging_dir, destination);
    todo!("paths::staging_worktree_path")
}

/// Whether a directory entry name is a parhel staging directory.
#[must_use]
pub fn is_staging_name(name: &OsStr) -> bool {
    let _ = name;
    todo!("paths::is_staging_name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ErrorCode;
    use rstest::rstest;

    // U-PATH-01..06: lexical resolution of the destination argument.
    #[rstest]
    #[case::relative_sibling("/work/repo", "../feat", "/work/feat")]
    #[case::relative_child("/work/repo", "wt/feat", "/work/repo/wt/feat")]
    #[case::absolute_ignores_cwd("/work/repo", "/elsewhere/feat", "/elsewhere/feat")]
    #[case::dot_segments("/work/repo", "./a/./b/../feat", "/work/repo/a/feat")]
    #[case::trailing_slash("/work/repo", "../feat/", "/work/feat")]
    #[case::many_parents_clamp_at_root("/a", "../../../feat", "/feat")]
    fn resolves_destination_lexically(#[case] cwd: &str, #[case] raw: &str, #[case] want: &str) {
        assert_eq!(
            resolve_destination(Path::new(cwd), Path::new(raw)),
            PathBuf::from(want)
        );
    }

    // U-PATH-07: unicode and spaces pass through untouched (no normalization).
    #[test]
    fn resolve_preserves_unicode_and_spaces() {
        let nfd = "cafe\u{301} wt";
        let got = resolve_destination(Path::new("/w"), Path::new(nfd));
        assert_eq!(got, Path::new("/w").join(nfd));
        assert_ne!(
            got,
            Path::new("/w").join("caf\u{e9} wt"),
            "must not NFC-normalize"
        );
    }

    // U-PATH-08: resolution must not follow symlinks (purely lexical).
    #[test]
    fn resolve_does_not_touch_filesystem() {
        let got = resolve_destination(Path::new("/definitely/not/existing"), Path::new("x/../y"));
        assert_eq!(got, PathBuf::from("/definitely/not/existing/y"));
    }

    // U-PATH-09
    #[test]
    fn basename_is_last_component() {
        assert_eq!(
            destination_basename(Path::new("/w/feat-x")).unwrap(),
            OsStr::new("feat-x")
        );
    }

    // U-PATH-10
    #[test]
    fn basename_of_root_is_usage_error() {
        let err = destination_basename(Path::new("/")).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage);
    }

    // U-PATH-11..16: placement of destination relative to source and git dir.
    #[test]
    fn destination_equal_to_source_is_rejected() {
        let err = classify_destination(
            Path::new("/w/repo"),
            Path::new("/w/repo/.git"),
            Path::new("/w/repo"),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::DestIsSource);
    }

    #[rstest]
    #[case::inside_dot_git("/w/repo/.git/worktrees/x")]
    #[case::dot_git_itself("/w/repo/.git")]
    fn destination_inside_git_dir_is_rejected(#[case] dest: &str) {
        let err = classify_destination(
            Path::new("/w/repo"),
            Path::new("/w/repo/.git"),
            Path::new(dest),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::DestInsideGitDir);
    }

    #[test]
    fn destination_inside_common_dir_of_linked_source_is_rejected() {
        // Source is a linked worktree; its common dir lives in the main checkout.
        let err = classify_destination(
            Path::new("/w/linked"),
            Path::new("/w/main/.git"),
            Path::new("/w/main/.git/feat"),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::DestInsideGitDir);
    }

    #[test]
    fn destination_nested_in_source_is_allowed_and_reported() {
        let got = classify_destination(
            Path::new("/w/repo"),
            Path::new("/w/repo/.git"),
            Path::new("/w/repo/.worktrees/feat"),
        )
        .unwrap();
        assert_eq!(
            got,
            Placement::InsideSource {
                relative: PathBuf::from(".worktrees/feat")
            }
        );
    }

    #[rstest]
    #[case::sibling("/w/feat")]
    #[case::prefix_lookalike("/w/repo-feat")]
    fn destination_outside_source(#[case] dest: &str) {
        let got = classify_destination(
            Path::new("/w/repo"),
            Path::new("/w/repo/.git"),
            Path::new(dest),
        );
        assert_eq!(got.unwrap(), Placement::Outside);
    }

    #[test]
    fn dot_git_lookalike_is_a_normal_nested_dir() {
        // `.gitx` shares a prefix with `.git` but is an ordinary directory.
        let got = classify_destination(
            Path::new("/w/repo"),
            Path::new("/w/repo/.git"),
            Path::new("/w/repo/.gitx/feat"),
        );
        assert_eq!(
            got.unwrap(),
            Placement::InsideSource {
                relative: PathBuf::from(".gitx/feat")
            }
        );
    }

    // U-PATH-17..19: staging naming.
    #[test]
    fn staging_path_keeps_destination_basename() {
        let got = staging_worktree_path(
            Path::new("/w/.parhel-staging-ab12"),
            Path::new("/w/feat-login"),
        );
        assert_eq!(got, PathBuf::from("/w/.parhel-staging-ab12/feat-login"));
    }

    #[rstest]
    #[case(".parhel-staging-ab12", true)]
    #[case(".parhel-staging-x", true)]
    #[case(".parhel-staging-", false)]
    #[case("parhel-staging-ab12", false)]
    #[case(".parhel-stagingab12", false)]
    #[case("feat", false)]
    fn recognizes_staging_names(#[case] name: &str, #[case] want: bool) {
        assert_eq!(is_staging_name(OsStr::new(name)), want);
    }

    #[test]
    fn staging_prefix_is_hidden_dotfile() {
        // Leftovers must not clutter `ls` output and must be easy to grep.
        assert!(STAGING_PREFIX.starts_with('.'));
        assert!(is_staging_name(OsStr::new(&format!("{STAGING_PREFIX}zz"))));
    }

    // U-PATH-P1..P3: properties over arbitrary relative inputs.
    mod props {
        use super::*;
        use proptest::prelude::*;
        use std::path::Component;

        fn segment() -> impl Strategy<Value = String> {
            prop_oneof![
                Just(".".to_owned()),
                Just("..".to_owned()),
                "[a-zA-Z0-9 _é\\-]{1,8}",
            ]
        }

        fn raw_path() -> impl Strategy<Value = String> {
            (
                prop::collection::vec(segment(), 1..8),
                any::<bool>(),
                any::<bool>(),
            )
                .prop_map(|(segs, absolute, trailing)| {
                    let mut s = segs.join("/");
                    if absolute {
                        s.insert(0, '/');
                    }
                    if trailing {
                        s.push('/');
                    }
                    s
                })
        }

        proptest! {
            #[test]
            fn resolved_is_absolute_and_normal(raw in raw_path()) {
                let got = resolve_destination(Path::new("/base/cwd"), Path::new(&raw));
                prop_assert!(got.is_absolute());
                prop_assert!(got.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_))));
            }

            #[test]
            fn resolution_is_idempotent(raw in raw_path()) {
                let once = resolve_destination(Path::new("/base/cwd"), Path::new(&raw));
                let twice = resolve_destination(Path::new("/elsewhere"), &once);
                prop_assert_eq!(once, twice);
            }

            #[test]
            fn staging_path_always_inside_staging_dir(name in "[a-zA-Z0-9_-]{1,12}") {
                let dest = Path::new("/w").join(&name);
                let got = staging_worktree_path(Path::new("/w/.parhel-staging-x"), &dest);
                prop_assert_eq!(got.parent(), Some(Path::new("/w/.parhel-staging-x")));
                prop_assert_eq!(got.file_name(), dest.file_name());
            }
        }
    }
}
