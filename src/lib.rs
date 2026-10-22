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

use std::ffi::OsString;
use std::process::ExitCode;

use domain::Error;

/// Entry point used by `main`: parse, dispatch, print, map to an exit code.
pub fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let _args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let err = Error::not_implemented("git parhel");
    eprintln!("{err}");
    err.code.exit_code()
}
