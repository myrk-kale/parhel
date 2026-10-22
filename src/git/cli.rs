//! [`Git`] port implemented by invoking the `git` executable.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::domain::Result;
use crate::git::parse::{GitVersion, WorktreeEntry};
use crate::git::runner::GitRunner;
use crate::ports::{Git, RepoInfo, WorktreeAddSpec};

#[derive(Debug, Clone, Default)]
pub struct GitCli {
    runner: GitRunner,
}

impl GitCli {
    #[must_use]
    pub fn new(runner: GitRunner) -> Self {
        Self { runner }
    }
}

/// Arguments of `git worktree add` for `spec`. User-controlled values (branch,
/// ref, path) always follow `--end-of-options`, so none can be read as a flag.
#[must_use]
pub fn worktree_add_args(spec: &WorktreeAddSpec) -> Vec<OsString> {
    let _ = spec;
    todo!("worktree_add_args")
}

impl Git for GitCli {
    fn version(&self) -> Result<GitVersion> {
        let _ = &self.runner;
        todo!("GitCli::version")
    }

    fn discover(&self, from: &Path) -> Result<RepoInfo> {
        let _ = from;
        todo!("GitCli::discover")
    }

    fn is_dirty(&self, worktree: &Path) -> Result<bool> {
        let _ = worktree;
        todo!("GitCli::is_dirty")
    }

    fn sparse_checkout_enabled(&self, worktree: &Path) -> Result<bool> {
        let _ = worktree;
        todo!("GitCli::sparse_checkout_enabled")
    }

    fn flagged_index_entries(&self, worktree: &Path) -> Result<Vec<PathBuf>> {
        let _ = worktree;
        todo!("GitCli::flagged_index_entries")
    }

    fn operation_in_progress(&self, worktree: &Path) -> Result<Option<String>> {
        let _ = worktree;
        todo!("GitCli::operation_in_progress")
    }

    fn initialized_submodules(&self, worktree: &Path) -> Result<Vec<PathBuf>> {
        let _ = worktree;
        todo!("GitCli::initialized_submodules")
    }

    fn worktrees(&self, repo: &Path) -> Result<Vec<WorktreeEntry>> {
        let _ = repo;
        todo!("GitCli::worktrees")
    }

    fn branch_exists(&self, repo: &Path, name: &str) -> Result<bool> {
        let _ = (repo, name);
        todo!("GitCli::branch_exists")
    }

    fn resolve_commit(&self, repo: &Path, rev: &str) -> Result<String> {
        let _ = (repo, rev);
        todo!("GitCli::resolve_commit")
    }

    fn head(&self, worktree: &Path) -> Result<String> {
        let _ = worktree;
        todo!("GitCli::head")
    }

    fn worktree_add(&self, repo: &Path, spec: &WorktreeAddSpec) -> Result<()> {
        let _ = (repo, spec);
        todo!("GitCli::worktree_add")
    }

    fn reset_index(&self, worktree: &Path, commit: &str) -> Result<()> {
        let _ = (worktree, commit);
        todo!("GitCli::reset_index")
    }

    fn checkout_two_way(&self, worktree: &Path, from: &str, to: &str) -> Result<()> {
        let _ = (worktree, from, to);
        todo!("GitCli::checkout_two_way")
    }

    fn submodules_reinit(&self, worktree: &Path, source: &Path) -> Result<()> {
        let _ = (worktree, source);
        todo!("GitCli::submodules_reinit")
    }

    fn worktree_repair(&self, repo: &Path, path: &Path) -> Result<()> {
        let _ = (repo, path);
        todo!("GitCli::worktree_repair")
    }

    fn worktree_remove_force(&self, repo: &Path, path: &Path) -> Result<()> {
        let _ = (repo, path);
        todo!("GitCli::worktree_remove_force")
    }

    fn worktree_prune(&self, repo: &Path) -> Result<()> {
        let _ = repo;
        todo!("GitCli::worktree_prune")
    }

    fn branch_delete(&self, repo: &Path, name: &str) -> Result<()> {
        let _ = (repo, name);
        todo!("GitCli::branch_delete")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::BranchMode;
    use rstest::rstest;

    fn args(spec: &WorktreeAddSpec) -> Vec<String> {
        worktree_add_args(spec)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    fn spec(branch: BranchMode, reference: Option<&str>, no_checkout: bool) -> WorktreeAddSpec {
        WorktreeAddSpec {
            path: PathBuf::from("/w/s/feat"),
            branch,
            reference: reference.map(Into::into),
            no_checkout,
        }
    }

    // U-ARGS-01
    #[test]
    fn new_branch_no_checkout() {
        assert_eq!(
            args(&spec(BranchMode::New("feat".into()), None, true)),
            [
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                "feat",
                "--end-of-options",
                "/w/s/feat"
            ]
        );
    }

    // U-ARGS-02
    #[rstest]
    #[case(BranchMode::Existing("topic".into()), None, &["worktree", "add", "--end-of-options", "/w/s/feat", "topic"][..])]
    #[case(BranchMode::Detach, Some("v1"), &["worktree", "add", "--detach", "--end-of-options", "/w/s/feat", "v1"][..])]
    #[case(BranchMode::Default, None, &["worktree", "add", "--end-of-options", "/w/s/feat"][..])]
    #[case(BranchMode::Default, Some("main"), &["worktree", "add", "--end-of-options", "/w/s/feat", "main"][..])]
    #[case(BranchMode::New("x".into()), Some("v1"), &["worktree", "add", "-b", "x", "--end-of-options", "/w/s/feat", "v1"][..])]
    fn maps_every_mode(
        #[case] branch: BranchMode,
        #[case] reference: Option<&str>,
        #[case] want: &[&str],
    ) {
        assert_eq!(args(&spec(branch, reference, false)), want);
    }

    // U-ARGS-03: hostile values can never become options.
    #[test]
    fn hostile_values_stay_after_end_of_options() {
        let got = args(&WorktreeAddSpec {
            path: PathBuf::from("/w/-rf"),
            branch: BranchMode::Detach,
            reference: Some("--orphan".into()),
            no_checkout: true,
        });
        let eoo = got.iter().position(|a| a == "--end-of-options").unwrap();
        let path = got.iter().position(|a| a == "/w/-rf").unwrap();
        let reference = got.iter().position(|a| a == "--orphan").unwrap();
        assert!(eoo < path && path < reference, "{got:?}");
        assert_eq!(got.iter().filter(|a| *a == "--orphan").count(), 1);
    }
}
