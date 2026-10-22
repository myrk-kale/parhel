//! [`Git`] port implemented by invoking the `git` executable.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use crate::domain::{BranchMode, Error, ErrorCode, Result};
use crate::git::parse::{self, GitVersion, WorktreeEntry};
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

    fn run<S: AsRef<OsStr>>(&self, cwd: &Path, args: &[S]) -> Result<Output> {
        self.runner.run(cwd, args)
    }

    fn stdout<S: AsRef<OsStr>>(&self, cwd: &Path, args: &[S]) -> Result<String> {
        let out = self.run(cwd, args)?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_owned())
    }

    fn succeeds<S: AsRef<OsStr>>(&self, cwd: &Path, args: &[S]) -> Result<bool> {
        Ok(self.runner.output(cwd, args)?.status.success())
    }

    fn absolute_git_dir(&self, worktree: &Path) -> Result<PathBuf> {
        Ok(PathBuf::from(
            self.stdout(worktree, &["rev-parse", "--absolute-git-dir"])?,
        ))
    }

    /// `submodule.<name>.path` -> name, from `<worktree>/.gitmodules`.
    fn submodule_name(&self, worktree: &Path, path: &Path) -> Result<String> {
        let out = self.runner.output(
            worktree,
            &[
                "config",
                "-z",
                "-f",
                ".gitmodules",
                "--get-regexp",
                r"^submodule\..*\.path$",
            ],
        )?;
        for record in out.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let Some(nl) = record.iter().position(|&b| b == b'\n') else {
                continue;
            };
            let (key, value) = (&record[..nl], &record[nl + 1..]);
            if Path::new(OsStr::from_bytes(value)) == path {
                let key = String::from_utf8_lossy(key);
                if let Some(name) = key
                    .strip_prefix("submodule.")
                    .and_then(|k| k.strip_suffix(".path"))
                {
                    return Ok(name.to_owned());
                }
            }
        }
        Err(Error::new(
            ErrorCode::GitFailed,
            format!(
                "submodule `{}` is not declared in .gitmodules",
                path.display()
            ),
        ))
    }

    /// Clone each initialized submodule of `source` into `worktree` from the
    /// source's own submodule repository, then recurse.
    fn reinit_recursive(&self, worktree: &Path, source: &Path, final_path: &Path) -> Result<()> {
        for path in self.initialized_submodules(source)? {
            let name = self.submodule_name(worktree, &path)?;
            let source_sub = source.join(&path);
            let source_git_dir = self.absolute_git_dir(&source_sub)?;
            let mut url_override = OsString::from(format!("submodule.{name}.url="));
            url_override.push(&source_git_dir);
            let path_arg = path.as_os_str();
            self.run(
                worktree,
                &[
                    OsStr::new("-c"),
                    OsStr::new("protocol.file.allow=always"),
                    OsStr::new("-c"),
                    &url_override,
                    OsStr::new("submodule"),
                    OsStr::new("update"),
                    OsStr::new("--init"),
                    OsStr::new("--quiet"),
                    OsStr::new("--"),
                    path_arg,
                ],
            )?;
            // The URL override above keeps `--init` from recording the URL, which
            // would leave the submodule inactive: record the source's value.
            let key = format!("submodule.{name}.url");
            let declared = self.runner.output(source, &["config", "--get", &key])?;
            if declared.status.success() {
                let url = OsStr::from_bytes(declared.stdout.trim_ascii_end()).to_owned();
                self.run(worktree, &[OsStr::new("config"), OsStr::new(&key), &url])?;
            }
            let dest_sub = worktree.join(&path);
            let origin = self
                .runner
                .output(&source_sub, &["config", "--get", "remote.origin.url"])?;
            if origin.status.success() {
                let url = OsStr::from_bytes(origin.stdout.trim_ascii_end()).to_owned();
                self.run(
                    &dest_sub,
                    &[OsStr::new("config"), OsStr::new("remote.origin.url"), &url],
                )?;
            }
            // git links the submodule and its git dir with relative paths, which break
            // when the staging dir is renamed into place: pin both ends absolutely
            // to the final location.
            let module_git_dir = self.absolute_git_dir(&dest_sub)?;
            let final_sub = final_path.join(&path);
            let mut gitfile = b"gitdir: ".to_vec();
            gitfile.extend_from_slice(module_git_dir.as_os_str().as_bytes());
            gitfile.push(b'\n');
            self.reinit_recursive(&dest_sub, &source_sub, &final_sub)?;
            fs::write(dest_sub.join(".git"), gitfile)
                .map_err(|e| Error::new(ErrorCode::Io, format!("{}: {e}", dest_sub.display())))?;
            self.run(
                worktree,
                &[
                    OsStr::new("config"),
                    OsStr::new("--file"),
                    module_git_dir.join("config").as_os_str(),
                    OsStr::new("core.worktree"),
                    final_sub.as_os_str(),
                ],
            )?;
        }
        Ok(())
    }
}

/// Arguments of `git worktree add` for `spec`. User-controlled values (branch,
/// ref, path) always follow `--end-of-options`, so none can be read as a flag.
#[must_use]
pub fn worktree_add_args(spec: &WorktreeAddSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["worktree".into(), "add".into()];
    if spec.no_checkout {
        args.push("--no-checkout".into());
    }
    let mut commit_ish = spec.reference.clone();
    match &spec.branch {
        BranchMode::New(name) => {
            args.push("-b".into());
            args.push(name.into());
        }
        BranchMode::Detach => args.push("--detach".into()),
        BranchMode::Existing(name) => commit_ish = Some(name.clone()),
        BranchMode::Default => {}
    }
    args.push("--end-of-options".into());
    args.push(spec.path.clone().into_os_string());
    args.extend(commit_ish.map(OsString::from));
    args
}

/// `-z` output of `ls-files`, with the trailing `/` git adds to directories removed.
fn nul_separated_paths(output: &[u8]) -> Vec<PathBuf> {
    output
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsStr::from_bytes(p.strip_suffix(b"/").unwrap_or(p))))
        .collect()
}

fn git_dir_markers(git_dir: &Path) -> Vec<&'static str> {
    [
        "MERGE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "rebase-apply/applying",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ]
    .into_iter()
    .filter(|m| git_dir.join(m).exists())
    .collect()
}

/// Equal as written, or once the existing parent directories are resolved
/// (`/var/...` vs `/private/var/...`).
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let resolve = |p: &Path| -> Option<PathBuf> {
        let (parent, name) = (p.parent()?, p.file_name()?);
        let mut base = parent;
        let mut tail = vec![name];
        while !base.exists() {
            tail.push(base.file_name()?);
            base = base.parent()?;
        }
        let mut out = fs::canonicalize(base).ok()?;
        out.extend(tail.iter().rev());
        Some(out)
    };
    matches!((resolve(a), resolve(b)), (Some(x), Some(y)) if x == y)
}

fn canonical(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path)
        .map_err(|e| Error::new(ErrorCode::Io, format!("{}: {e}", path.display())))
}

impl Git for GitCli {
    fn version(&self) -> Result<GitVersion> {
        parse::parse_version(&self.stdout(Path::new("/"), &["--version"])?)
    }

    fn discover(&self, from: &Path) -> Result<RepoInfo> {
        let cwd = if from.is_dir() {
            from
        } else {
            from.parent().unwrap_or(from)
        };
        if !cwd.is_dir() {
            return Err(Error::new(
                ErrorCode::NotARepo,
                format!("`{}` does not exist", from.display()),
            ));
        }
        let probe = self.runner.output(
            cwd,
            &["rev-parse", "--is-bare-repository", "--is-inside-git-dir"],
        )?;
        if !probe.status.success() {
            return Err(Error::new(
                ErrorCode::NotARepo,
                format!("`{}` is not inside a Git worktree", from.display()),
            ));
        }
        let text = String::from_utf8_lossy(&probe.stdout);
        let mut lines = text.lines();
        if lines.next() == Some("true") {
            return Err(Error::new(
                ErrorCode::BareRepo,
                format!(
                    "`{}` is a bare repository: there is no worktree to clone",
                    from.display()
                ),
            ));
        }
        if lines.next() == Some("true") {
            return Err(Error::new(
                ErrorCode::InsideGitDir,
                format!("`{}` is inside a .git directory", from.display()),
            ));
        }
        let toplevel = canonical(Path::new(
            &self.stdout(cwd, &["rev-parse", "--show-toplevel"])?,
        ))?;
        let dirs = self.stdout(
            &toplevel,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
            ],
        )?;
        let mut dirs = dirs.lines();
        let git_dir = canonical(Path::new(dirs.next().unwrap_or_default()))?;
        let common_dir = canonical(Path::new(dirs.next().unwrap_or_default()))?;
        let head = self
            .runner
            .output(&toplevel, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])?;
        if !head.status.success() {
            return Err(Error::new(
                ErrorCode::UnbornHead,
                format!("`{}` has no commits yet", toplevel.display()),
            ));
        }
        let head = parse::parse_object_id(&String::from_utf8_lossy(&head.stdout))?;
        Ok(RepoInfo {
            toplevel,
            git_dir,
            common_dir,
            head,
        })
    }

    fn is_dirty(&self, worktree: &Path, exclude: &[PathBuf]) -> Result<bool> {
        let mut args: Vec<OsString> = [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--",
            ".",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        for path in exclude {
            let mut spec = OsString::from(":(exclude,literal)");
            spec.push(path);
            args.push(spec);
        }
        let out = self.run(worktree, &args)?;
        Ok(parse::status_is_dirty(&out.stdout))
    }

    fn sparse_checkout_enabled(&self, worktree: &Path) -> Result<bool> {
        let out = self.runner.output(
            worktree,
            &["config", "--type=bool", "--get", "core.sparseCheckout"],
        )?;
        let text = String::from_utf8_lossy(&out.stdout);
        parse::parse_config_bool(out.status.success().then_some(text.as_ref()))
    }

    fn flagged_index_entries(&self, worktree: &Path) -> Result<Vec<PathBuf>> {
        parse::parse_flagged_entries(&self.run(worktree, &["ls-files", "-v", "-z"])?.stdout)
    }

    fn operation_in_progress(&self, worktree: &Path) -> Result<Option<String>> {
        let git_dir = self.absolute_git_dir(worktree)?;
        Ok(parse::operation_from_markers(&git_dir_markers(&git_dir)).map(str::to_owned))
    }

    fn initialized_submodules(&self, worktree: &Path) -> Result<Vec<PathBuf>> {
        let out = self.run(worktree, &["ls-files", "--stage", "-z"])?;
        Ok(parse::parse_gitlinks(&out.stdout)?
            .into_iter()
            .filter(|p| fs::symlink_metadata(worktree.join(p).join(".git")).is_ok())
            .collect())
    }

    fn worktrees(&self, repo: &Path) -> Result<Vec<WorktreeEntry>> {
        let out = self
            .runner
            .output(repo, &["worktree", "list", "--porcelain", "-z"])?;
        if out.status.success() {
            return parse::parse_worktree_list(&out.stdout);
        }
        // git < 2.36 has no `-z`: newline-separated, same structure.
        let out = self.run(repo, &["worktree", "list", "--porcelain"])?;
        let z: Vec<u8> = out
            .stdout
            .iter()
            .map(|&b| if b == b'\n' { 0 } else { b })
            .collect();
        parse::parse_worktree_list(&z)
    }

    fn tracked_files(&self, worktree: &Path) -> Result<Vec<PathBuf>> {
        let out = self.run(worktree, &["ls-files", "-z"])?;
        Ok(nul_separated_paths(&out.stdout))
    }

    fn parhelignored(&self, worktree: &Path) -> Result<Vec<PathBuf>> {
        let file = worktree.join(".parhelignore");
        if !file.is_file() {
            return Ok(Vec::new());
        }
        let mut exclude_from = OsString::from("--exclude-from=");
        exclude_from.push(&file);
        let out = self.run(
            worktree,
            &[
                OsStr::new("ls-files"),
                OsStr::new("-z"),
                OsStr::new("--others"),
                OsStr::new("--ignored"),
                OsStr::new("--directory"),
                &exclude_from,
            ],
        )?;
        Ok(nul_separated_paths(&out.stdout))
    }

    fn branch_exists(&self, repo: &Path, name: &str) -> Result<bool> {
        self.succeeds(
            repo,
            &["rev-parse", "--verify", "-q", &format!("refs/heads/{name}")],
        )
    }

    fn resolve_commit(&self, repo: &Path, rev: &str) -> Result<String> {
        let out = self.stdout(
            repo,
            &[
                "rev-parse",
                "--verify",
                "-q",
                "--end-of-options",
                &format!("{rev}^{{commit}}"),
            ],
        )?;
        parse::parse_object_id(&out)
    }

    fn head(&self, worktree: &Path) -> Result<String> {
        parse::parse_object_id(&self.stdout(worktree, &["rev-parse", "HEAD"])?)
    }

    fn worktree_add(&self, repo: &Path, spec: &WorktreeAddSpec) -> Result<()> {
        // git reads every registration while adding one; another `worktree add`
        // running at the same moment can leave one half-written. That race is
        // transient: retry it, and only it.
        let mut attempt = 0;
        loop {
            match self.run(repo, &worktree_add_args(spec)) {
                Err(e)
                    if attempt < 5
                        && e.message.contains("failed to read")
                        && e.message.contains("worktrees/") =>
                {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
                }
                other => return other.map(drop),
            }
        }
    }

    fn reset_index(&self, worktree: &Path, commit: &str) -> Result<()> {
        self.run(worktree, &["read-tree", "--end-of-options", commit])?;
        // Exit status 1 only means some entries differ (expected with --allow-dirty).
        self.runner
            .output(worktree, &["update-index", "-q", "--refresh"])?;
        Ok(())
    }

    fn checkout_two_way(&self, worktree: &Path, from: &str, to: &str) -> Result<()> {
        let out = self.runner.output(
            worktree,
            &[
                "read-tree",
                "-m",
                "-u",
                "--exclude-per-directory=.gitignore",
                from,
                to,
            ],
        )?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        let conflict = [
            "not uptodate",
            "would be overwritten",
            "would be removed",
            "would lose",
        ]
        .iter()
        .any(|m| stderr.contains(m));
        let code = if conflict {
            ErrorCode::CheckoutConflict
        } else {
            ErrorCode::GitFailed
        };
        Err(Error::new(
            code,
            format!("checking out the target commit failed: {stderr}"),
        ))
    }

    fn submodules_reinit(&self, worktree: &Path, source: &Path, final_path: &Path) -> Result<()> {
        self.reinit_recursive(worktree, source, final_path)
    }

    fn worktree_repair(&self, repo: &Path, path: &Path) -> Result<()> {
        self.run(
            repo,
            &[
                OsStr::new("worktree"),
                OsStr::new("repair"),
                path.as_os_str(),
            ],
        )
        .map(drop)
    }

    fn worktree_remove_force(&self, repo: &Path, path: &Path) -> Result<()> {
        self.run(
            repo,
            &[
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                OsStr::new("--force"),
                path.as_os_str(),
            ],
        )
        .map(drop)
    }

    fn unregister_worktree(&self, repo: &Path, path: &Path) -> Result<()> {
        let common = self.stdout(
            repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let admin_root = Path::new(&common).join("worktrees");
        let Ok(entries) = fs::read_dir(&admin_root) else {
            return Ok(());
        };
        let target = path.join(".git");
        for entry in entries.flatten() {
            let Ok(recorded) = fs::read(entry.path().join("gitdir")) else {
                continue;
            };
            let recorded = PathBuf::from(OsStr::from_bytes(recorded.trim_ascii_end()));
            if same_path(&recorded, &target) {
                return fs::remove_dir_all(entry.path()).map_err(|e| {
                    Error::new(ErrorCode::Io, format!("{}: {e}", entry.path().display()))
                });
            }
        }
        Ok(())
    }

    fn branch_delete(&self, repo: &Path, name: &str) -> Result<()> {
        self.run(repo, &["branch", "-q", "-D", name]).map(drop)
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
