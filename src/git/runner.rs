//! Spawning `git` with a sanitized environment.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::domain::Result;

/// Variables that would redirect git away from the repository we target with
/// `-C`/`current_dir`. They are removed from every child `git` process.
pub const SANITIZED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// Builds and runs `git` subprocesses.
#[derive(Debug, Clone)]
pub struct GitRunner {
    program: PathBuf,
}

impl Default for GitRunner {
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
        }
    }
}

impl GitRunner {
    /// Use a specific git executable (resolved through `PATH` when not absolute).
    #[must_use]
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// A `Command` for `git <args>` running in `cwd`, with [`SANITIZED_ENV`] removed
    /// and `LC_ALL=C` so messages are parseable.
    #[must_use]
    pub fn command<S: AsRef<OsStr>>(&self, cwd: &Path, args: &[S]) -> Command {
        let _ = (cwd, args, &self.program);
        todo!("GitRunner::command")
    }

    /// Run to completion. Non-zero exit maps to `git-failed` with git's stderr.
    pub fn run<S: AsRef<OsStr>>(&self, cwd: &Path, args: &[S]) -> Result<Output> {
        let _ = (cwd, args);
        todo!("GitRunner::run")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn envs(cmd: &Command) -> HashMap<OsString, Option<OsString>> {
        cmd.get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(ToOwned::to_owned)))
            .collect()
    }

    // U-ENV-01
    #[test]
    fn removes_every_redirecting_variable() {
        let cmd = GitRunner::default().command(Path::new("/tmp"), &["status"]);
        let envs = envs(&cmd);
        for var in SANITIZED_ENV {
            assert_eq!(
                envs.get(OsStr::new(var)),
                Some(&None),
                "{var} must be env_remove'd"
            );
        }
    }

    // U-ENV-02
    #[test]
    fn forces_c_locale() {
        let cmd = GitRunner::default().command(Path::new("/tmp"), &["status"]);
        assert_eq!(
            envs(&cmd).get(OsStr::new("LC_ALL")),
            Some(&Some(OsString::from("C")))
        );
    }

    // U-ENV-03
    #[test]
    fn runs_in_requested_dir_with_given_args() {
        let cmd = GitRunner::with_program("/opt/git/bin/git")
            .command(Path::new("/w/repo"), &["worktree", "list"]);
        assert_eq!(cmd.get_program(), OsStr::new("/opt/git/bin/git"));
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/w/repo")));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec![OsStr::new("worktree"), OsStr::new("list")]);
    }

    // U-ENV-04
    #[test]
    fn run_maps_failure_to_git_failed_with_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let err = GitRunner::default()
            .run(
                dir.path(),
                &["rev-parse", "--verify", "definitely-not-a-ref"],
            )
            .unwrap_err();
        assert_eq!(err.code, crate::domain::ErrorCode::GitFailed);
        assert!(
            err.message.contains("rev-parse"),
            "message names the command: {}",
            err.message
        );
    }
}
