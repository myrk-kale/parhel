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

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use app::{AddOutcome, DoctorReport, Env, Finding};
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
            };
            match app::add(&env, &request) {
                Ok(outcome) => {
                    report_added(&outcome);
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e),
            }
        }
        Invocation::Doctor(request) => {
            let interrupted = signals::install();
            let env = Env {
                git: &git,
                fs: &fs,
                cwd: &cwd,
                interrupted,
            };
            match app::doctor(&env, &request) {
                Ok(report) => print_doctor(&report),
                Err(e) => fail(&e),
            }
        }
    }
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
    };
    println!(
        "Created {how} worktree at {}",
        outcome.destination.display()
    );
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
            | Finding::InProgress { path } => describe(path),
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
