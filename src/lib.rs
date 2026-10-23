//! `git parhel`: copy-on-write Git worktrees.
//!
//! Layout (hexagonal):
//! - [`domain`]: pure rules (requests, paths, exclusions, clone plan, errors);
//! - [`ports`]: traits for the side effects the application needs;
//! - [`git`], [`fs`]: adapters (the `git` binary, APFS syscalls);
//! - [`app`]: the `add` transaction and `doctor`;
//! - [`cli`]: argument parsing.

pub mod app;
pub mod cli;
pub mod domain;
pub mod failpoint;
pub mod fs;
pub mod git;
pub mod ports;
pub mod signals;
pub mod sys;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use app::{AddOutcome, DoctorReport, Env, Finding, SetupOutcome, StoreSummary};
use cli::Invocation;
use domain::{Error, ErrorCode};

/// Entry point used by `main`: parse, dispatch, print, map to an exit code.
pub fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let invocation = match cli::parse(args) {
        Ok(invocation) => invocation,
        Err(e) => return fail(&e),
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            return fail(&Error::new(
                ErrorCode::Io,
                format!("current directory: {e}"),
            ));
        }
    };
    let git = git::GitCli::default();
    let fs = fs::PlatformFs;
    let store = store_path();
    match invocation {
        Invocation::Help(text) | Invocation::Version(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Invocation::Add(request) => {
            let interrupted = signals::install();
            let env = Env {
                git: &git,
                fs: &fs,
                cwd: &cwd,
                interrupted,
                store: store.as_deref(),
            };
            match app::add(&env, &request) {
                Ok(outcome) => {
                    report_added(&outcome);
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e),
            }
        }
        Invocation::Setup(request) => match app::setup(&sys::SystemHost, &request) {
            Ok(outcome) => {
                report_setup(&outcome);
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        Invocation::Doctor(request) => {
            let interrupted = signals::install();
            let env = Env {
                git: &git,
                fs: &fs,
                cwd: &cwd,
                interrupted,
                store: store.as_deref(),
            };
            match app::doctor(&env, &request) {
                Ok(report) => print_doctor(&report),
                Err(e) => fail(&e),
            }
        }
    }
}

/// Where the user's store lives: `PARHEL_STORE`, else `/parhel/<user>` (the user is the
/// account of the process, not `$USER`, which `sudo` and `su` leave behind). `None` where
/// stores do not exist (they are Linux only), so nothing is looked up or suggested.
#[cfg(target_os = "linux")]
fn store_path() -> Option<PathBuf> {
    let user = fs::unix::current_user_name()?;
    Some(domain::store::store_path(
        std::env::var_os("PARHEL_STORE").as_deref(),
        &user,
    ))
}

#[cfg(not(target_os = "linux"))]
fn store_path() -> Option<PathBuf> {
    None
}

fn fail(error: &Error) -> ExitCode {
    eprintln!("{error}");
    match (error.code, signals::received()) {
        (ErrorCode::Interrupted, Some(signal)) => {
            ExitCode::from(u8::try_from(128 + signal).unwrap_or(130))
        }
        _ => error.code.exit_code(),
    }
}

fn report_added(outcome: &AddOutcome) {
    for warning in &outcome.warnings {
        eprintln!("warning: {warning}");
    }
    let how = match outcome.strategy {
        app::Strategy::CopyOnWrite => "copy-on-write",
        app::Strategy::Fallback => "regular",
        app::Strategy::Copy => "full-copy",
        app::Strategy::Store => "copy-on-write (store)",
    };
    println!(
        "Created {how} worktree at {}",
        outcome.destination.display()
    );
    if let Some(real) = &outcome.real_path {
        println!(
            "The worktree lives in the store at {}; {} is a symlink to it",
            real.display(),
            outcome.destination.display()
        );
    }
    if outcome.relocated > 0 {
        println!(
            "Relocated {} copied file(s) that pointed at the source",
            outcome.relocated
        );
    }
    println!("HEAD {}", outcome.head.get(..12).unwrap_or(&outcome.head));
}

fn describe(path: &Path) -> String {
    path.display().to_string()
}

fn print_doctor(report: &DoctorReport) -> ExitCode {
    if report.findings.is_empty() {
        println!("no issues found");
        return ExitCode::SUCCESS;
    }
    for finding in &report.findings {
        let detail = match finding {
            Finding::NotCow { path, fs_type } => format!("{} is on {fs_type}", describe(path)),
            Finding::OrphanStaging { path }
            | Finding::OrphanRegistration { path }
            | Finding::ForeignGitfile { path }
            | Finding::InProgress { path }
            | Finding::StoreOrphan { path }
            | Finding::StoreStaleBaseline { path } => describe(path),
            Finding::StoreUnavailable { path, reason } => {
                format!("{}: {reason}", describe(path))
            }
            Finding::StoreImageDeleted { path, image } => {
                format!("{} is backed by {image}", describe(path))
            }
            Finding::StoreLowSpace { path, free, total } => {
                format!("{} has {free} of {total} bytes free", describe(path))
            }
            Finding::StoreDanglingLink { link, target } => {
                format!("{} -> {}", describe(link), describe(target))
            }
            Finding::StoreWorktreesUnreachable {
                root,
                worktrees,
                unlocked,
            } => unreachable_detail(root, worktrees, unlocked),
        };
        let status = if report.fixed.contains(finding) {
            " (fixed)"
        } else if finding.fixable() {
            " (run `git parhel doctor --fix`)"
        } else {
            ""
        };
        println!("{}: {detail}{status}", finding.code());
    }
    if report.healthy() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// What `store-worktrees-unreachable` says: which worktrees, which of them a `prune` would
/// forget, and what to do (nothing destructive).
fn unreachable_detail(root: &Path, worktrees: &[PathBuf], unlocked: &[PathBuf]) -> String {
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| describe(p))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!(
        "{} is not available and git lists {} worktree(s) of this repository in it: {}. Do \
         not run `git worktree prune` or `git gc` until it is back: they forget the \
         registration of a worktree whose directory is missing",
        describe(root),
        worktrees.len(),
        list(worktrees)
    );
    if !unlocked.is_empty() {
        text.push_str(&format!(
            "; these are not locked and would be forgotten ({}): lock each with `git worktree \
             lock <path>`",
            list(unlocked)
        ));
    }
    text
}

/// `1073741824` as `1.0 GiB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn report_setup(outcome: &SetupOutcome) {
    let show = |summary: &StoreSummary| {
        println!("store   {}", summary.store.display());
        println!("image   {}", summary.image.display());
        println!(
            "size    {} ({} bytes), {} allocated now",
            human_size(summary.apparent),
            summary.apparent,
            human_size(summary.allocated)
        );
        println!("unit    {}", summary.unit);
        println!(
            "remove  sudo git parhel setup --remove --user {}",
            summary.user
        );
    };
    match outcome {
        SetupOutcome::Created(summary) => {
            println!("Created the store for {}", summary.user);
            show(summary);
            println!(
                "The image is one file: /var/lib/parhel is immutable, so even root cannot \
                 delete it by accident. Commits are safe in each repository's .git; push \
                 work you cannot lose."
            );
        }
        SetupOutcome::AlreadySetUp(summary) => {
            println!("The store for {} is already set up", summary.user);
            show(summary);
        }
        SetupOutcome::Restored(summary) => {
            println!(
                "The store for {} was set up already; it is mounted and protected again",
                summary.user
            );
            show(summary);
        }
        SetupOutcome::Removed(summary) => {
            println!("Removed the store for {}", summary.user);
            show(summary);
        }
        SetupOutcome::NothingToRemove { user } => {
            println!("No store for {user}: nothing to remove");
        }
    }
}
