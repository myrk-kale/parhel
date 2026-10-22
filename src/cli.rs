//! Command-line surface: `git parhel <command>`.
//!
//! clap only tokenizes; cross-flag rules live in [`parse`] so they produce the
//! same `error[usage]` contract as every other failure.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::domain::{AddRequest, DoctorRequest, Result};

#[derive(Debug, Parser)]
#[command(name = "git-parhel", bin_name = "git parhel", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a copy-on-write worktree.
    Add(AddArgs),
    /// Diagnose the environment and clean up after crashed runs.
    Doctor(DoctorArgs),
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Path inside the source worktree to clone.
    #[arg(long, value_name = "path", default_value = ".")]
    pub from: PathBuf,
    /// Check out an existing branch.
    #[arg(long, value_name = "name")]
    pub branch: Option<String>,
    /// Create and check out a new branch.
    #[arg(long, value_name = "name")]
    pub new_branch: Option<String>,
    /// Detach HEAD at the target commit.
    #[arg(long)]
    pub detach: bool,
    /// Carry uncommitted changes from the source.
    #[arg(long)]
    pub allow_dirty: bool,
    /// Use a plain `git worktree add` when copy-on-write is impossible.
    #[arg(long)]
    pub fallback: bool,
    /// What to do with initialized submodules.
    #[arg(long, value_enum, value_name = "policy", default_value = "refuse")]
    pub submodules: SubmodulesArg,
    /// Where to create the worktree.
    pub destination: PathBuf,
    /// Commit-ish to check out (default: the source HEAD).
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SubmodulesArg {
    Refuse,
    Reinit,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Path inside the repository to diagnose.
    #[arg(long, value_name = "path", default_value = ".")]
    pub from: PathBuf,
    /// Repair leftovers of crashed runs.
    #[arg(long)]
    pub fix: bool,
}

/// What the process should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Add(AddRequest),
    Doctor(DoctorRequest),
    /// `--help` (exit 0): text to print on stdout.
    Help(String),
    /// `--version` (exit 0): text to print on stdout.
    Version(String),
}

/// Parse `argv` (including `argv[0]`). Errors: `usage`.
pub fn parse<I, T>(args: I) -> Result<Invocation>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let _ = args.into_iter().map(Into::into).collect::<Vec<OsString>>();
    todo!("cli::parse")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BranchMode, ErrorCode, SubmodulePolicy};
    use rstest::rstest;
    use std::path::Path;

    fn add(args: &[&str]) -> Result<AddRequest> {
        let full = ["git-parhel", "add"].iter().chain(args).copied();
        match parse(full)? {
            Invocation::Add(req) => Ok(req),
            other => panic!("expected add, got {other:?}"),
        }
    }

    fn usage(args: &[&str]) {
        let err = add(args).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage, "{args:?} -> {err}");
    }

    // U-CLI-01
    #[test]
    fn minimal_add_uses_defaults() {
        let req = add(&["../feat"]).unwrap();
        assert_eq!(
            req,
            AddRequest {
                from: PathBuf::from("."),
                destination: PathBuf::from("../feat"),
                reference: None,
                branch: BranchMode::Default,
                allow_dirty: false,
                fallback: false,
                submodules: SubmodulePolicy::Refuse,
            }
        );
    }

    // U-CLI-02: the README example.
    #[test]
    fn new_branch_example_from_readme() {
        let req = add(&["../my-feature", "--new-branch", "feature/my-feature"]).unwrap();
        assert_eq!(req.destination, Path::new("../my-feature"));
        assert_eq!(req.branch, BranchMode::New("feature/my-feature".into()));
    }

    // U-CLI-03
    #[rstest]
    #[case(&["--branch", "topic", "d"], BranchMode::Existing("topic".into()))]
    #[case(&["--branch=topic", "d"], BranchMode::Existing("topic".into()))]
    #[case(&["--new-branch=x/y", "d"], BranchMode::New("x/y".into()))]
    #[case(&["--detach", "d"], BranchMode::Detach)]
    #[case(&["d"], BranchMode::Default)]
    fn maps_branch_flags(#[case] args: &[&str], #[case] want: BranchMode) {
        assert_eq!(add(args).unwrap().branch, want);
    }

    // U-CLI-04: flags may follow positionals (parity with the original's normalizeAddArgs).
    #[test]
    fn flags_after_positionals() {
        let req = add(&[
            "d",
            "v1.0",
            "--detach",
            "--allow-dirty",
            "--from",
            "sub/dir",
        ])
        .unwrap();
        assert_eq!(req.reference.as_deref(), Some("v1.0"));
        assert_eq!(req.branch, BranchMode::Detach);
        assert!(req.allow_dirty);
        assert_eq!(req.from, Path::new("sub/dir"));
    }

    // U-CLI-05
    #[test]
    fn boolean_and_policy_flags() {
        let req = add(&["--fallback", "--submodules=reinit", "--from=/abs", "d"]).unwrap();
        assert!(req.fallback);
        assert_eq!(req.submodules, SubmodulePolicy::Reinit);
        assert_eq!(req.from, Path::new("/abs"));
    }

    // U-CLI-06: conflicting combinations.
    #[rstest]
    #[case::branch_and_new(&["--branch", "a", "--new-branch", "b", "d"])]
    #[case::branch_and_detach(&["--branch", "a", "--detach", "d"])]
    #[case::new_and_detach(&["--new-branch", "a", "--detach", "d"])]
    #[case::branch_and_ref(&["--branch", "a", "d", "v1"])]
    fn rejects_conflicts(#[case] args: &[&str]) {
        usage(args);
    }

    // U-CLI-07
    #[rstest]
    #[case::missing_destination(&[])]
    #[case::extra_positional(&["d", "ref", "extra"])]
    #[case::branch_without_value(&["d", "--branch"])]
    #[case::empty_new_branch(&["--new-branch=", "d"])]
    #[case::bad_submodules(&["--submodules=sometimes", "d"])]
    #[case::removed_original_flag(&["--allow-mismatched-ref", "d"])]
    #[case::unknown_flag(&["--frobnicate", "d"])]
    fn rejects_invalid_usage(#[case] args: &[&str]) {
        usage(args);
    }

    // U-CLI-08
    #[test]
    fn doctor_defaults_and_fix() {
        assert_eq!(
            parse(["git-parhel", "doctor"]).unwrap(),
            Invocation::Doctor(DoctorRequest {
                from: PathBuf::from("."),
                fix: false
            })
        );
        assert_eq!(
            parse(["git-parhel", "doctor", "--fix", "--from", "x"]).unwrap(),
            Invocation::Doctor(DoctorRequest {
                from: PathBuf::from("x"),
                fix: true
            })
        );
    }

    // U-CLI-09
    #[rstest]
    #[case(&["git-parhel", "--help"])]
    #[case(&["git-parhel", "help"])]
    #[case(&["git-parhel", "add", "--help"])]
    fn help_is_not_an_error(#[case] argv: &[&str]) {
        match parse(argv.iter().copied()).unwrap() {
            Invocation::Help(text) => assert!(text.contains("Usage"), "{text}"),
            other => panic!("expected help, got {other:?}"),
        }
    }

    // U-CLI-10
    #[test]
    fn version_reports_crate_version() {
        match parse(["git-parhel", "--version"]).unwrap() {
            Invocation::Version(text) => assert!(text.contains(env!("CARGO_PKG_VERSION"))),
            other => panic!("expected version, got {other:?}"),
        }
    }

    // U-CLI-11
    #[rstest]
    #[case(&["git-parhel"])]
    #[case(&["git-parhel", "remove", "x"])]
    fn missing_or_unknown_command_is_usage(#[case] argv: &[&str]) {
        assert_eq!(
            parse(argv.iter().copied()).unwrap_err().code,
            ErrorCode::Usage
        );
    }

    // U-CLI-12: non-UTF-8 paths are accepted (APFS allows any bytes but `/` and NUL).
    #[test]
    fn accepts_non_utf8_destination() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(b"feat-\xff".to_vec());
        let argv = vec![
            OsString::from("git-parhel"),
            OsString::from("add"),
            raw.clone(),
        ];
        match parse(argv).unwrap() {
            Invocation::Add(req) => assert_eq!(req.destination.as_os_str(), raw),
            other => panic!("{other:?}"),
        }
    }

    // U-CLI-13: values that look like options are rejected (defense in depth on top
    // of `--end-of-options` in the git adapter).
    #[rstest]
    #[case::ref_after_double_dash(&["d", "--", "--orphan"])]
    #[case::new_branch_dash(&["--new-branch=-x", "d"])]
    #[case::branch_dash(&["--branch=-x", "d"])]
    fn rejects_option_like_names(#[case] args: &[&str]) {
        usage(args);
    }

    // U-CLI-14: a destination starting with `-` is a path, accepted after `--`.
    #[test]
    fn dash_destination_after_double_dash_is_a_path() {
        assert_eq!(
            add(&["--", "-weird"]).unwrap().destination,
            Path::new("-weird")
        );
    }
}
