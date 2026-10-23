//! Command-line surface: `git parhel <command>`.
//!
//! clap only tokenizes; cross-flag rules live in [`parse`] so they produce the
//! same `error[usage]` contract as every other failure.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::domain::request::{DEFAULT_STORE_SIZE, parse_size};
use crate::domain::{
    AddRequest, BranchMode, DoctorRequest, Error, ErrorCode, Result, SetupRequest, SubmodulePolicy,
};

const ABOUT: &str = "Create Git worktrees with copy-on-write, so node_modules, target/ \
and other ignored build output come along without using extra disk";

const LONG_ABOUT: &str = "\
git parhel creates regular Git worktrees by cloning the source worktree with the \
filesystem's copy-on-write support instead of checking files out. Ignored directories \
such as node_modules/ and target/ come along and share disk blocks with the source \
until one side changes them.

The result is an ordinary worktree: list, move, lock and remove it with \
`git worktree`.

Platform support: macOS on APFS (clonefile) and Linux on Btrfs, XFS with reflink, \
bcachefs or ZFS 2.2+ (FICLONE). Windows is planned. On other filesystems, `add --copy` \
copies every file in full and `add --fallback` creates a regular worktree.";

const AFTER_HELP: &str = "\
Examples:
  git parhel add ../my-feature --new-branch feature/my-feature
  git parhel add ../hotfix --branch release/1.2
  git parhel add ../review --detach v1.4.0
  git parhel doctor --fix
  sudo git parhel setup        # Linux, ext4: a store for worktrees without copy-on-write

Remove a worktree with `git worktree remove <path>`.
Docs: https://github.com/myrk-kale/parhel";

/// Shown above `--help` only (not `-h`, errors or man pages).
const BANNER: &str = include_str!("../assets/banner.txt");

#[derive(Debug, Parser)]
#[command(
    name = "git-parhel",
    bin_name = "git parhel",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    before_long_help = BANNER,
    after_help = AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a copy-on-write worktree.
    #[command(
        long_about = "Create a worktree at <destination> by cloning the source worktree with \
copy-on-write, then check out [ref] (default: the source HEAD). Only tracked files that \
differ between the source HEAD and [ref] are rewritten; everything else, ignored files \
included, is shared with the source until modified.

Nothing is created unless every check passes. The worktree is assembled in a hidden \
.parhel-staging-* directory and published with an atomic, exclusive rename: an existing \
destination is never touched. Failures and Ctrl-C roll back completely.",
        after_help = "Exit status: 0 success, 1 error, 2 usage error, 128+N killed by signal N.
Errors are printed as `error[<code>]: <message>` with a stable <code>."
    )]
    Add(AddArgs),
    /// Diagnose the environment and clean up after crashed runs.
    #[command(
        long_about = "Report what would make `add` refuse or misbehave: a source on a volume \
without copy-on-write, staging directories and worktree registrations left by a crashed run, and \
.git files of other repositories nested in the source. Runs still in progress are \
detected and never touched.

With --fix, leftovers are undone using the crashed run's journal: only what that run \
created is removed, and a worktree that was already published is repaired, not removed."
    )]
    Doctor(DoctorArgs),
    /// Linux: create the store that gives filesystems without copy-on-write (ext4) cheap worktrees.
    #[command(
        long_about = "Create a per-user btrfs image (the store) mounted at /parhel/<user> \
through a systemd mount unit, so `add` can create worktrees from sources on filesystems \
without copy-on-write, such as ext4. The worktree lives in the store and the requested \
destination becomes a symlink to it. Needs root (run it with sudo); the store belongs \
to the user who ran sudo, or to --user.

Safe to re-run: a healthy store is left as it is. With --remove the store is taken down \
(refused while worktrees live in it).",
        after_help = "Examples:
  sudo git parhel setup
  sudo git parhel setup --size 200G --user alice
  sudo git parhel setup --remove"
    )]
    Setup(SetupArgs),
}

// Each bool is an independent command-line switch.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Args)]
pub struct AddArgs {
    /// Any path inside the source worktree.
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
    /// Carry uncommitted changes from the source (staged changes arrive unstaged).
    #[arg(long)]
    pub allow_dirty: bool,
    /// Use a plain `git worktree add` when copy-on-write is impossible
    /// (no copy-on-write on the volume, different volumes).
    #[arg(long)]
    pub fallback: bool,
    /// Copy every file in full instead of cloning it copy-on-write: uses disk space,
    /// works on any filesystem and across volumes. Cannot be combined with --fallback.
    #[arg(long)]
    pub copy: bool,
    /// Leave copied files that mention the source's path as they are (by default
    /// such paths are rewritten to the new worktree's).
    #[arg(long)]
    pub no_relocate: bool,
    /// Initialized submodules: `refuse`, or `reinit` them with their own git dirs.
    #[arg(long, value_enum, value_name = "policy", default_value = "refuse")]
    pub submodules: SubmodulesArg,
    /// Where to create the worktree (its parent must exist).
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

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Size of the store image: a number with an optional K, M, G or T suffix.
    /// The image is sparse, so it takes disk space only as it fills.
    #[arg(long, value_name = "size")]
    pub size: Option<String>,
    /// Allocate the whole image now instead of letting it grow.
    #[arg(long)]
    pub preallocate: bool,
    /// Whose store to create (default: the user who ran sudo).
    #[arg(long, value_name = "name")]
    pub user: Option<String>,
    /// Take the store down: unmount, remove the unit and the image.
    #[arg(long)]
    pub remove: bool,
}

/// What the process should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Add(AddRequest),
    Doctor(DoctorRequest),
    Setup(SetupRequest),
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
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let text = e.render().to_string();
            return match e.kind() {
                ErrorKind::DisplayHelp => Ok(Invocation::Help(text)),
                ErrorKind::DisplayVersion => Ok(Invocation::Version(text)),
                _ => {
                    let text = text.trim_end();
                    Err(usage(text.strip_prefix("error: ").unwrap_or(text)))
                }
            };
        }
    };
    match cli.command {
        Command::Add(args) => args.into_request().map(Invocation::Add),
        Command::Doctor(args) => Ok(Invocation::Doctor(DoctorRequest {
            from: args.from,
            fix: args.fix,
        })),
        Command::Setup(args) => args.into_request().map(Invocation::Setup),
    }
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Usage, message)
}

/// Branch and ref names can never start with `-` in git; rejecting them keeps
/// user input from ever being read as an option.
fn check_name(flag: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(usage(format!("{flag} needs a non-empty value")));
    }
    if value.starts_with('-') {
        return Err(usage(format!("{flag} `{value}` looks like an option")));
    }
    Ok(())
}

impl SetupArgs {
    fn into_request(self) -> Result<SetupRequest> {
        if self.remove && (self.size.is_some() || self.preallocate) {
            return Err(usage(
                "--remove takes the store down; --size and --preallocate only apply when creating it",
            ));
        }
        if let Some(user) = &self.user {
            check_name("--user", user)?;
        }
        let size = match &self.size {
            Some(text) => parse_size(text)?,
            None => DEFAULT_STORE_SIZE,
        };
        Ok(SetupRequest {
            size,
            preallocate: self.preallocate,
            user: self.user,
            remove: self.remove,
        })
    }
}

impl AddArgs {
    fn into_request(self) -> Result<AddRequest> {
        let exclusive = [
            self.branch.is_some(),
            self.new_branch.is_some(),
            self.detach,
        ];
        if exclusive.iter().filter(|&&set| set).count() > 1 {
            return Err(usage(
                "--branch, --new-branch and --detach are mutually exclusive",
            ));
        }
        if self.copy && self.fallback {
            return Err(usage(
                "--copy and --fallback are mutually exclusive: --copy copies the files \
                 in full, --fallback creates a plain worktree without them",
            ));
        }
        if self.branch.is_some() && self.reference.is_some() {
            return Err(usage(
                "--branch already names what to check out; drop [ref]",
            ));
        }
        if let Some(b) = &self.branch {
            check_name("--branch", b)?;
        }
        if let Some(b) = &self.new_branch {
            check_name("--new-branch", b)?;
        }
        if let Some(r) = &self.reference {
            check_name("[ref]", r)?;
        }
        let branch = match (self.branch, self.new_branch, self.detach) {
            (Some(b), _, _) => BranchMode::Existing(b),
            (_, Some(b), _) => BranchMode::New(b),
            (_, _, true) => BranchMode::Detach,
            _ => BranchMode::Default,
        };
        Ok(AddRequest {
            from: self.from,
            destination: self.destination,
            reference: self.reference,
            branch,
            allow_dirty: self.allow_dirty,
            fallback: self.fallback,
            copy: self.copy,
            relocate: !self.no_relocate,
            submodules: match self.submodules {
                SubmodulesArg::Refuse => SubmodulePolicy::Refuse,
                SubmodulesArg::Reinit => SubmodulePolicy::Reinit,
            },
        })
    }
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
                copy: false,
                submodules: SubmodulePolicy::Refuse,
                relocate: true,
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
    #[case::copy_and_fallback(&["--copy", "--fallback", "d"])]
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

    // U-CLI-15: the banner decorates `--help` only; `-h` stays compact for scripts.
    #[test]
    fn banner_only_on_long_help() {
        let help = |flag: &str| match parse(["git-parhel", flag]).unwrap() {
            Invocation::Help(text) => text,
            other => panic!("{other:?}"),
        };
        assert!(
            help("--help").starts_with(BANNER.trim_end()),
            "{}",
            help("--help")
        );
        assert!(help("--help").contains("o   -( O )-   o"));
        assert!(!help("-h").contains("( O )"));
    }

    // U-CLI-16
    #[test]
    fn no_relocate_flag() {
        assert!(!add(&["--no-relocate", "d"]).unwrap().relocate);
    }

    // U-CLI-17: `--copy` asks for a full copy; off by default.
    #[test]
    fn copy_flag() {
        assert!(!add(&["d"]).unwrap().copy);
        let req = add(&["--copy", "d"]).unwrap();
        assert!(req.copy && !req.fallback);
    }

    // U-CLI-18: the two escape hatches are alternatives, so combining them is a usage
    // error that names both flags.
    #[test]
    fn copy_with_fallback_names_both_flags() {
        let err = add(&["--fallback", "--copy", "d"]).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage);
        assert!(
            err.message.contains("--copy") && err.message.contains("--fallback"),
            "{}",
            err.message
        );
    }

    // U-CLI-19: the help says what `--copy` costs and what it allows.
    #[test]
    fn copy_help_mentions_disk_space_and_cross_filesystem_use() {
        let Invocation::Help(text) = parse(["git-parhel", "add", "--help"]).unwrap() else {
            panic!("expected help");
        };
        assert!(text.contains("--copy"), "{text}");
        // The help wraps lines: compare words.
        let copy = text
            .split("--copy")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(copy.contains("disk space"), "{text}");
        assert!(copy.contains("across volumes"), "{text}");
    }

    fn setup(args: &[&str]) -> Result<SetupRequest> {
        let full = ["git-parhel", "setup"].iter().chain(args).copied();
        match parse(full)? {
            Invocation::Setup(req) => Ok(req),
            other => panic!("expected setup, got {other:?}"),
        }
    }

    // U-CLI-20: `setup` with no options is a 100G sparse store for the sudo user.
    #[test]
    fn setup_defaults() {
        assert_eq!(
            setup(&[]).unwrap(),
            SetupRequest {
                size: 100 << 30,
                preallocate: false,
                user: None,
                remove: false,
            }
        );
    }

    // U-CLI-21
    #[test]
    fn setup_flags() {
        let req = setup(&["--size", "200G", "--preallocate", "--user", "alice"]).unwrap();
        assert_eq!(req.size, 200 << 30);
        assert!(req.preallocate && !req.remove);
        assert_eq!(req.user.as_deref(), Some("alice"));
        assert_eq!(setup(&["--size=512m"]).unwrap().size, 512 << 20);
        assert!(setup(&["--remove"]).unwrap().remove);
        assert_eq!(
            setup(&["--user=root"]).unwrap().user.as_deref(),
            Some("root")
        );
    }

    // U-CLI-22: removing a store has no size; the contradiction is reported, not ignored.
    #[rstest]
    #[case::size(&["--remove", "--size", "1G"])]
    #[case::preallocate(&["--remove", "--preallocate"])]
    fn setup_remove_conflicts(#[case] args: &[&str]) {
        let err = setup(args).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage);
        assert!(err.message.contains("--remove"), "{}", err.message);
    }

    // U-CLI-23
    #[rstest]
    #[case::bad_size(&["--size", "lots"])]
    #[case::small_size(&["--size", "1M"])]
    #[case::size_without_value(&["--size"])]
    #[case::empty_user(&["--user="])]
    #[case::option_like_user(&["--user=-x"])]
    #[case::positional(&["alice"])]
    #[case::unknown_flag(&["--frobnicate"])]
    fn setup_rejects_invalid_usage(#[case] args: &[&str]) {
        assert_eq!(setup(args).unwrap_err().code, ErrorCode::Usage, "{args:?}");
    }

    // U-CLI-24: the help says it needs root and what it creates.
    #[test]
    fn setup_help_says_it_needs_sudo() {
        let Invocation::Help(text) = parse(["git-parhel", "setup", "--help"]).unwrap() else {
            panic!("expected help");
        };
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for needle in [
            "sudo",
            "/parhel/<user>",
            "--size",
            "--preallocate",
            "--user",
            "--remove",
        ] {
            assert!(flat.contains(needle), "help lacks `{needle}`: {text}");
        }
        let Invocation::Help(top) = parse(["git-parhel", "--help"]).unwrap() else {
            panic!("expected help");
        };
        assert!(top.contains("setup"), "{top}");
    }
}
