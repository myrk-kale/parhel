//! The `add` transaction: validate, assemble in staging, publish atomically.
//!
//! Invariants (each covered by a test below and by integration tests):
//! - every refusal happens before the first mutation;
//! - the destination path is only ever written by the final exclusive rename, so
//!   an existing destination (including another run's worktree) is never touched;
//! - any failure before publish undoes the registration, the branch this run
//!   created, and the staging dir.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::app::rollback::{JOURNAL_FILE, Journal, rollback};
use crate::domain::clone_plan::{self, CloneOp, DirLister};
use crate::domain::exclusions::{ExclusionInput, Exclusions};
use crate::domain::paths::{self, Placement};
use crate::domain::relocate::{MAX_RELOCATE_BYTES, Relocation, Rewrite};
use crate::domain::{AddRequest, BranchMode, Error, ErrorCode, Result, SubmodulePolicy};
use crate::git::parse::ensure_supported;
use crate::ports::{CowFs, Git, RepoInfo, WorktreeAddSpec};

/// Side-effect providers and process context.
#[derive(Clone, Copy)]
pub struct Env<'a> {
    pub git: &'a dyn Git,
    pub fs: &'a dyn CowFs,
    /// Directory relative paths resolve against.
    pub cwd: &'a Path,
    /// Set by the SIGINT handler; checked between steps.
    pub interrupted: &'a AtomicBool,
}

impl std::fmt::Debug for Env<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    CopyOnWrite,
    /// Plain `git worktree add` (only with `--fallback`).
    Fallback,
    /// Every file copied in full (`--copy`): same transaction, no copy-on-write.
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    pub destination: PathBuf,
    /// Commit checked out in the new worktree.
    pub head: String,
    pub strategy: Strategy,
    /// Printed to stderr as `warning: ...`.
    pub warnings: Vec<String>,
    /// Copied files and symlinks whose references to the source path were rewritten.
    pub relocated: usize,
}

/// What relocation changed in the staged worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RelocationReport {
    rewritten: usize,
    relinked: usize,
    /// Binaries (relative paths) that mention the source and were left as is.
    binaries: Vec<PathBuf>,
    /// Entries that could not be examined (e.g. paths too long inside staging).
    unexamined: usize,
    /// Directories whose copied `.git` file linked to another repository's git dir.
    detached: Vec<PathBuf>,
}

/// Everything validated before the first mutation.
struct Plan {
    repo: RepoInfo,
    /// Canonical parent + basename.
    destination: PathBuf,
    parent: PathBuf,
    submodules: Vec<PathBuf>,
}

/// Run `git parhel add`.
pub fn add(env: &Env<'_>, request: &AddRequest) -> Result<AddOutcome> {
    let plan = validate(env, request)?;
    // `--copy` needs neither copy-on-write nor a common volume: the staging
    // directory, the only thing that must share the destination's volume, always does.
    if !request.copy {
        if let Some(problem) = cow_problem(env, &plan)? {
            if !request.fallback {
                return Err(problem);
            }
            return fallback(env, request, &plan, &problem);
        }
    }
    crate::failpoint!("after_validate");
    transaction(env, request, &plan)
}

fn validate(env: &Env<'_>, request: &AddRequest) -> Result<Plan> {
    // The CLI refuses the pair already; callers building a request by hand get the same.
    if request.copy && request.fallback {
        return Err(Error::new(
            ErrorCode::Usage,
            "--copy and --fallback are mutually exclusive: --copy copies the files in \
             full, --fallback creates a plain worktree without them",
        ));
    }
    let raw = paths::resolve_destination(env.cwd, &request.destination);
    let name = paths::destination_basename(&raw)?.to_owned();
    // APFS cannot store names that are not UTF-8; Linux filesystems store any bytes.
    if raw.to_str().is_none() && cfg!(target_os = "macos") {
        return Err(Error::new(
            ErrorCode::InvalidPath,
            format!(
                "destination `{}` is not valid UTF-8, which this filesystem cannot store",
                raw.display()
            ),
        ));
    }
    // The branch git creates from the basename must be a name parhel can journal.
    if request.branch == BranchMode::Default
        && request.reference.is_none()
        && name.to_str().is_none()
    {
        return Err(Error::new(
            ErrorCode::InvalidPath,
            format!(
                "destination name `{}` is not valid UTF-8, so it cannot name the implicit \
                 branch; pass --new-branch, --branch or --detach",
                name.to_string_lossy()
            ),
        ));
    }
    // Without a branch flag git turns the basename into a `git branch` argument.
    if request.branch == BranchMode::Default
        && request.reference.is_none()
        && name.as_encoded_bytes().starts_with(b"-")
    {
        return Err(Error::new(
            ErrorCode::Usage,
            format!(
                "destination name `{}` starts with `-`, which git would read as an option \
                 for the implicit branch; pass --new-branch, --branch or --detach",
                name.to_string_lossy()
            ),
        ));
    }
    ensure_supported(env.git.version()?)?;
    let repo = env
        .git
        .discover(&paths::resolve_destination(env.cwd, &request.from))?;

    let raw_parent = raw.parent().unwrap_or(Path::new("/"));
    if !env.fs.is_dir(raw_parent)? {
        return Err(Error::new(
            ErrorCode::DestParentMissing,
            format!("parent directory `{}` does not exist", raw_parent.display()),
        ));
    }
    let parent = env.fs.canonicalize(raw_parent)?;
    let destination = parent.join(&name);
    let placement = paths::classify_destination(&repo.toplevel, &repo.common_dir, &destination)?;
    if env.fs.exists_no_follow(&destination)? {
        return Err(Error::new(
            ErrorCode::DestExists,
            format!("destination `{}` already exists", destination.display()),
        ));
    }

    let submodules = env.git.initialized_submodules(&repo.toplevel)?;
    if let Placement::InsideSource { relative } = &placement {
        if let Some(sub) = submodules.iter().find(|s| relative.starts_with(s)) {
            return Err(Error::new(
                ErrorCode::DestInsideSubmodule,
                format!(
                    "destination `{}` is inside submodule `{}`",
                    destination.display(),
                    sub.display()
                ),
            ));
        }
    }
    check_source_state(env, request, &repo, &submodules)?;
    Ok(Plan {
        repo,
        destination,
        parent,
        submodules,
    })
}

/// Refusals about the source itself (after every destination-level check).
fn check_source_state(
    env: &Env<'_>,
    request: &AddRequest,
    repo: &RepoInfo,
    submodules: &[PathBuf],
) -> Result<()> {
    if let Some(op) = env.git.operation_in_progress(&repo.toplevel)? {
        return Err(Error::new(
            ErrorCode::OperationInProgress,
            format!(
                "a {op} is in progress in `{}`; finish or abort it first",
                repo.toplevel.display()
            ),
        ));
    }
    if env.git.sparse_checkout_enabled(&repo.toplevel)? {
        return Err(Error::new(
            ErrorCode::SparseCheckout,
            format!(
                "`{}` uses sparse-checkout, which is not supported",
                repo.toplevel.display()
            ),
        ));
    }
    if !request.allow_dirty {
        let flagged = env.git.flagged_index_entries(&repo.toplevel)?;
        if !flagged.is_empty() {
            return Err(Error::new(
                ErrorCode::DirtySource,
                format!(
                    "files flagged assume-unchanged/skip-worktree may hide changes: {}; \
                     commit/clear them or pass --allow-dirty",
                    list(&flagged)
                ),
            ));
        }
        if env.git.is_dirty(&repo.toplevel, &[])? {
            return Err(Error::new(
                ErrorCode::DirtySource,
                format!(
                    "source worktree `{}` has uncommitted changes; commit/stash them or pass \
                     --allow-dirty",
                    repo.toplevel.display()
                ),
            ));
        }
    }
    if !submodules.is_empty() && request.submodules == SubmodulePolicy::Refuse {
        return Err(Error::new(
            ErrorCode::SubmodulesPresent,
            format!(
                "initialized submodules would share state with the source: {}; pass \
                 --submodules=reinit to give them their own git dirs",
                list(submodules)
            ),
        ));
    }
    if request.allow_dirty && request.submodules == SubmodulePolicy::Reinit {
        let mut dirty = Vec::new();
        for sub in submodules {
            if env.git.is_dirty(&repo.toplevel.join(sub), &[])? {
                dirty.push(sub.clone());
            }
        }
        if !dirty.is_empty() {
            return Err(Error::new(
                ErrorCode::DirtySubmodule,
                format!(
                    "submodules with uncommitted changes: {}; --submodules=reinit checks out \
                     their recorded commits, so those changes would not be in the new \
                     worktree. Commit or stash them inside each submodule first",
                    list(&dirty)
                ),
            ));
        }
    }
    Ok(())
}

fn list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why copy-on-write is impossible, if it is.
fn cow_problem(env: &Env<'_>, plan: &Plan) -> Result<Option<Error>> {
    let source = env.fs.volume(&plan.repo.toplevel)?;
    let dest = env.fs.volume(&plan.parent)?;
    let not_cow = |path: &Path, fs_type: &str| {
        Error::new(
            ErrorCode::NotCow,
            format!(
                "copy-on-write is not available on {fs_type} at `{}`; pass --copy to copy \
                 every file in full (uses disk space) or --fallback for a regular worktree \
                 without the copied files",
                path.display()
            ),
        )
    };
    Ok(if !source.cow {
        Some(not_cow(&plan.repo.toplevel, &source.fs_type))
    } else if !dest.cow {
        Some(not_cow(&plan.parent, &dest.fs_type))
    } else if source.device != dest.device {
        Some(Error::new(
            ErrorCode::CrossVolume,
            format!(
                "`{}` and `{}` are on different volumes; pass --copy to copy every file in \
                 full (uses disk space) or --fallback for a regular worktree without the \
                 copied files",
                plan.repo.toplevel.display(),
                plan.parent.display()
            ),
        ))
    } else {
        None
    })
}

fn fallback(
    env: &Env<'_>,
    request: &AddRequest,
    plan: &Plan,
    problem: &Error,
) -> Result<AddOutcome> {
    env.git.worktree_add(
        &plan.repo.toplevel,
        &WorktreeAddSpec {
            path: plan.destination.clone(),
            branch: request.branch.clone(),
            reference: request.reference.clone(),
            no_checkout: false,
        },
    )?;
    Ok(AddOutcome {
        destination: plan.destination.clone(),
        head: env.git.head(&plan.destination)?,
        strategy: Strategy::Fallback,
        relocated: 0,
        warnings: vec![format!(
            "copy-on-write unavailable ({}); created a regular worktree instead",
            problem.message
        )],
    })
}

fn check_interrupt(env: &Env<'_>) -> Result<()> {
    if env.interrupted.load(Ordering::SeqCst) {
        return Err(Error::new(
            ErrorCode::Interrupted,
            "interrupted; rolled back",
        ));
    }
    Ok(())
}

fn transaction(env: &Env<'_>, request: &AddRequest, plan: &Plan) -> Result<AddOutcome> {
    let staging = env.fs.create_staging_dir(&plan.parent)?;
    let mut journal = Journal {
        repo: plan.repo.toplevel.clone(),
        ..Journal::default()
    };
    let mut lock = None;
    // A panic (bug) must not skip the rollback: catch it and undo like any error.
    let assembled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assemble(env, request, plan, &staging, &mut journal, &mut lock)
    }))
    .unwrap_or_else(|_| {
        Err(Error::new(
            ErrorCode::Io,
            "internal error (panic); rolled back",
        ))
    });
    match assembled {
        Ok(report) => publish(env, request, plan, &staging, &report),
        Err(cause) => {
            #[cfg(feature = "failpoints")]
            let _ = crate::failpoint::eval(crate::failpoint::ROLLBACK_FAILPOINT);
            let undone = rollback(env.git, env.fs, &staging, &journal);
            drop(lock);
            match undone {
                Ok(()) => Err(cause),
                Err(rb) => Err(Error::new(
                    ErrorCode::RollbackFailed,
                    format!("{cause}; {}", rb.message),
                )),
            }
        }
    }
}

/// Build the worktree inside `staging`; returns its path.
fn assemble(
    env: &Env<'_>,
    request: &AddRequest,
    plan: &Plan,
    staging: &Path,
    journal: &mut Journal,
    lock: &mut Option<crate::ports::FileLock>,
) -> Result<RelocationReport> {
    let (git, fs, repo) = (env.git, env.fs, &plan.repo);
    let journal_path = staging.join(JOURNAL_FILE);
    *lock = Some(fs.lock_exclusive(&journal_path)?);
    fs.write_file(&journal_path, &journal.encode())?;
    check_interrupt(env)?;

    let staged = paths::staging_worktree_path(staging, &plan.destination);
    register(env, request, plan, &staged, journal, &journal_path)?;
    journal.worktree = Some(staged.clone());
    fs.write_file(&journal_path, &journal.encode())?;
    crate::failpoint!("after_worktree_add");
    check_interrupt(env)?;

    let exclusions = Exclusions::compute(&ExclusionInput {
        source_root: repo.toplevel.clone(),
        staging_dir: Some(staging.to_path_buf()),
        other_worktrees: git
            .worktrees(&repo.toplevel)?
            .into_iter()
            .map(|w| w.path)
            .collect(),
        submodule_paths: plan.submodules.clone(),
        exclude_submodules: request.submodules == SubmodulePolicy::Reinit,
        user_excluded: git.parhelignored(&repo.toplevel)?,
    });
    clone_tree(env, &repo.toplevel, &staged, &exclusions, request.copy)?;
    crate::failpoint!("after_clone");
    check_interrupt(env)?;

    let staging_rel: Vec<PathBuf> = staging
        .strip_prefix(&repo.toplevel)
        .map(|r| vec![r.to_path_buf()])
        .unwrap_or_default();
    let head_moved = git.head(&repo.toplevel)? != repo.head;
    if head_moved || (!request.allow_dirty && git.is_dirty(&repo.toplevel, &staging_rel)?) {
        return Err(Error::new(
            ErrorCode::SourceChanged,
            format!(
                "`{}` changed while the worktree was being created; run the command again",
                repo.toplevel.display()
            ),
        ));
    }

    let target = git.head(&staged)?;
    git.reset_index(&staged, &repo.head)?;
    if target != repo.head {
        git.checkout_two_way(&staged, &repo.head, &target)?;
    }
    crate::failpoint!("after_index");
    check_interrupt(env)?;

    if request.submodules == SubmodulePolicy::Reinit && !plan.submodules.is_empty() {
        git.submodules_reinit(&staged, &repo.toplevel, &plan.destination)?;
        check_interrupt(env)?;
    }
    let report = relocate_tree(
        env,
        &staged,
        &repo.toplevel,
        &exclusions,
        request.relocate.then(|| {
            Relocation::new(&repo.toplevel, &plan.destination).excluding(exclusions.iter())
        }),
    )?;
    check_interrupt(env)?;
    crate::failpoint!("before_rename");
    check_interrupt(env)?;
    // Record where the worktree is going *before* moving it: if the process dies right
    // after the rename, `doctor --fix` must repair the destination, not undo it.
    journal.publish_to = Some(plan.destination.clone());
    fs.write_file(&journal_path, &journal.encode())?;
    fs.rename_exclusive(&staged, &plan.destination)?;
    Ok(report)
}

/// `git worktree add --no-checkout` at `staged`, journaling the branch git is
/// asked to create before asking (git creates it first and can still fail).
fn register(
    env: &Env<'_>,
    request: &AddRequest,
    plan: &Plan,
    staged: &Path,
    journal: &mut Journal,
    journal_path: &Path,
) -> Result<()> {
    let (git, fs, repo) = (env.git, env.fs, &plan.repo);
    // A branch this run asks git to create. A name that already exists is never
    // ours: git refuses it, and it must survive the rollback.
    let new_branch = match (&request.branch, &request.reference) {
        (BranchMode::New(name), _) => Some(name.clone()),
        (BranchMode::Default, None) => Some(
            paths::destination_basename(&plan.destination)?
                .to_string_lossy()
                .into_owned(),
        ),
        _ => None,
    };
    let implicit_branch = match new_branch {
        Some(name) if !git.branch_exists(&repo.toplevel, &name)? => Some(name),
        _ => None,
    };
    // Journal it before asking: git creates the branch first and can still fail to
    // add the worktree (e.g. racing another `worktree add`), leaving the branch.
    if implicit_branch.is_some() {
        journal.branch_start = Some(match &request.reference {
            Some(rev) => git.resolve_commit(&repo.toplevel, rev)?,
            None => repo.head.clone(),
        });
        journal.created_branch.clone_from(&implicit_branch);
        fs.write_file(journal_path, &journal.encode())?;
    }
    let added = git.worktree_add(
        &repo.toplevel,
        &WorktreeAddSpec {
            path: staged.to_path_buf(),
            branch: request.branch.clone(),
            reference: request.reference.clone(),
            no_checkout: true,
        },
    );
    if let Err(e) = added {
        // Someone created the branch between our check and git's: it is theirs.
        if e.message.contains("already exists") {
            journal.created_branch = None;
            journal.branch_start = None;
        }
        return Err(e);
    }
    Ok(())
}

/// Clone the source into `staged`, minus `exclusions` (copy every file in full
/// instead with `copy`).
fn clone_tree(
    env: &Env<'_>,
    source: &Path,
    staged: &Path,
    exclusions: &Exclusions,
    copy: bool,
) -> Result<()> {
    let fs = env.fs;
    let ops = clone_plan::plan(&Lister { fs, root: source }, exclusions)?;
    for (i, op) in ops.iter().enumerate() {
        match op {
            CloneOp::CreateDir(rel) => {
                fs.create_dir(&staged.join(rel))?;
                fs.copy_mode(&source.join(rel), &staged.join(rel))?;
            }
            CloneOp::Clone(rel) if copy => fs.copy_entry(&source.join(rel), &staged.join(rel))?,
            CloneOp::Clone(rel) => fs.clone_entry(&source.join(rel), &staged.join(rel))?,
        }
        if i == 0 {
            crate::failpoint!("mid_clone");
        }
        check_interrupt(env)?;
    }
    // `git worktree add` created the root with default permissions.
    fs.copy_mode(source, staged)
}

/// Walk the staged worktree's untracked files (tracked files and submodules are
/// git's business and never touched):
/// - copied `.git` files linked to a git dir outside the copy are removed, so the
///   copy cannot drive another repository's HEAD and index;
/// - with a [`Relocation`], references to the source path are rewritten (see
///   [`crate::domain::relocate`]); files over the size limit are not examined.
fn relocate_tree(
    env: &Env<'_>,
    staged: &Path,
    source: &Path,
    exclusions: &Exclusions,
    relocation: Option<Relocation>,
) -> Result<RelocationReport> {
    let fs = env.fs;
    let tracked: HashSet<PathBuf> = env.git.tracked_files(staged)?.into_iter().collect();
    let skip = |rel: &Path| rel == Path::new(".git") || tracked.contains(rel);
    let needles = relocation.as_ref().map_or(&[][..], Relocation::needles);
    let scan = fs.scan(staged, needles, MAX_RELOCATE_BYTES, &skip)?;
    let mut report = RelocationReport {
        unexamined: scan.unreadable,
        ..RelocationReport::default()
    };

    for gitfile in &scan.gitfiles {
        let dir = gitfile.parent().unwrap_or(Path::new(""));
        let path = staged.join(gitfile);
        let detach = match gitfile_link(env, source, exclusions, dir, &path) {
            GitLink::NotAGitfile | GitLink::InsideCopy { absolute: false } => false,
            // Copied along, but named by its source path: follow the copy.
            GitLink::InsideCopy { absolute: true } => match &relocation {
                Some(relocation) => {
                    if let Ok(contents) = fs.read_file(&path) {
                        if let Rewrite::Rewritten(contents) = relocation.rewrite(&contents) {
                            fs.replace_contents(&path, &contents)?;
                            report.rewritten += 1;
                        }
                    }
                    false
                }
                None => true,
            },
            GitLink::Outside => true,
        };
        if detach {
            fs.remove_file(&path)?;
            report.detached.push(dir.to_path_buf());
        }
    }
    let Some(relocation) = relocation else {
        return Ok(report);
    };
    for rel in &scan.matches {
        let path = staged.join(rel);
        let Ok(contents) = fs.read_file(&path) else {
            report.unexamined += 1;
            continue;
        };
        match relocation.rewrite(&contents) {
            Rewrite::Rewritten(contents) => {
                fs.replace_contents(&path, &contents)?;
                report.rewritten += 1;
            }
            Rewrite::BinaryReference => report.binaries.push(rel.clone()),
            Rewrite::Unchanged => {}
        }
    }
    for (rel, target) in &scan.links {
        if let Some(new_target) = relocation.rewrite_link(target) {
            fs.replace_symlink(&staged.join(rel), &new_target)?;
            report.relinked += 1;
        }
    }
    Ok(report)
}

/// Where a copied gitfile leads.
enum GitLink {
    NotAGitfile,
    /// To a git dir that is copied along (`absolute`: named by its source path).
    InsideCopy {
        absolute: bool,
    },
    /// To a git dir the copy does not contain (another repository, or an
    /// excluded part of the source).
    Outside,
}

/// Classify the gitfile at `path` (copied from `<source>/<dir>/.git`).
fn gitfile_link(
    env: &Env<'_>,
    source: &Path,
    exclusions: &Exclusions,
    dir: &Path,
    path: &Path,
) -> GitLink {
    let Ok(contents) = env.fs.read_file(path) else {
        return GitLink::Outside;
    };
    let text = String::from_utf8_lossy(&contents);
    let Some(target) = text.trim().strip_prefix("gitdir:") else {
        return GitLink::NotAGitfile;
    };
    let target = Path::new(target.trim());
    let resolved = paths::resolve_destination(&source.join(dir), target);
    match resolved.strip_prefix(source) {
        Ok(inside) if !exclusions.is_excluded(inside) => GitLink::InsideCopy {
            absolute: target.is_absolute(),
        },
        _ => GitLink::Outside,
    }
}

/// After the exclusive rename the worktree is ours and valid: never roll it back.
fn publish(
    env: &Env<'_>,
    request: &AddRequest,
    plan: &Plan,
    staging: &Path,
    report: &RelocationReport,
) -> Result<AddOutcome> {
    crate::failpoint!("after_rename");
    let mut warnings = Vec::new();
    let staged = paths::staging_worktree_path(staging, &plan.destination);
    env.git
        .repoint_worktree(&plan.repo.toplevel, &staged, &plan.destination)
        .map_err(|e| {
            Error::new(
                e.code,
                format!(
                    "{}; the worktree exists at `{}` but its registration is stale: run \
                 `git worktree repair {}`",
                    e.message,
                    plan.destination.display(),
                    plan.destination.display()
                ),
            )
        })?;
    if let Err(e) = env.fs.remove_dir_all(staging) {
        warnings.push(format!(
            "could not remove `{}`: {}",
            staging.display(),
            e.message
        ));
    }
    if !report.binaries.is_empty() {
        let shown: Vec<String> = report
            .binaries
            .iter()
            .take(5)
            .map(|p| p.display().to_string())
            .collect();
        let more = report.binaries.len().saturating_sub(shown.len());
        warnings.push(format!(
            "{} binary file(s) mention the source path and were copied as is: {}{}. If \
             they misbehave, list them in .parhelignore so they are not copied.",
            report.binaries.len(),
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
    }
    if !report.detached.is_empty() {
        let shown: Vec<String> = report
            .detached
            .iter()
            .take(5)
            .map(|p| p.display().to_string())
            .collect();
        warnings.push(format!(
            "{} copied folder(s) were linked to another repository's git data and were \
             detached in the new worktree (their .git file was removed): {}",
            report.detached.len(),
            shown.join(", ")
        ));
    }
    if report.unexamined > 0 {
        warnings.push(format!(
            "{} copied path(s) could not be checked for references to the source \
             (unreadable or too long); they were copied as is",
            report.unexamined
        ));
    }
    Ok(AddOutcome {
        destination: plan.destination.clone(),
        head: env.git.head(&plan.destination)?,
        strategy: if request.copy {
            Strategy::Copy
        } else {
            Strategy::CopyOnWrite
        },
        warnings,
        relocated: report.rewritten + report.relinked,
    })
}

/// [`DirLister`] over the real source tree through the [`CowFs`] port.
struct Lister<'a> {
    fs: &'a dyn CowFs,
    root: &'a Path,
}

impl DirLister for Lister<'_> {
    fn list(&self, relative: &Path) -> io::Result<Vec<OsString>> {
        let path = self.root.join(relative);
        // No-follow: a symlink is cloned as a link, never descended into.
        match self.fs.is_dir_no_follow(&path) {
            Ok(true) => {}
            Ok(false) => return Err(io::Error::from(io::ErrorKind::NotADirectory)),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
        self.fs
            .list_dir(&path)
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{Call, FAKE_STAGING, SHA_A, SHA_B, Step, World, apfs, p};
    use crate::app::rollback::{JOURNAL_FILE, Journal};
    use crate::domain::{BranchMode, ErrorCode, SubmodulePolicy};
    use crate::git::parse::{GitVersion, WorktreeEntry};
    use crate::ports::{VolumeInfo, WorktreeAddSpec};
    use rstest::rstest;
    use std::ffi::OsString;
    use std::sync::atomic::Ordering;

    const DEST: &str = "/w/feat";
    const STAGING: &str = "/w/.parhel-staging-test";
    const STAGED_WT: &str = "/w/.parhel-staging-test/feat";

    fn request(branch: BranchMode) -> AddRequest {
        AddRequest {
            from: p("."),
            destination: p("../feat"),
            reference: None,
            branch,
            allow_dirty: false,
            fallback: false,
            copy: false,
            submodules: SubmodulePolicy::Refuse,
            relocate: true,
        }
    }

    fn new_branch() -> AddRequest {
        request(BranchMode::New("feat".into()))
    }

    fn run(w: &World, req: &AddRequest) -> Result<AddOutcome> {
        let env = Env {
            git: w,
            fs: w,
            cwd: Path::new("/w/repo"),
            interrupted: &w.interrupted,
        };
        add(&env, req)
    }

    fn assert_no_mutation(w: &World) {
        assert!(
            w.calls().is_empty(),
            "expected no side effects, got {:#?}",
            w.calls()
        );
    }

    fn assert_rolled_back(w: &World, branch_created: Option<&str>) {
        let calls = w.calls();
        let last = calls.last().expect("rollback ran");
        assert_eq!(
            *last,
            Call::RemoveDirAll(p(STAGING)),
            "staging removed last: {calls:#?}"
        );
        let registered = w.count(|c| matches!(c, Call::WorktreeAdd(_))) == 1
            && !w.fail.borrow().contains_key(&Step::WorktreeAdd);
        if registered {
            assert_eq!(
                w.count(|c| *c == Call::RemoveForce { path: p(STAGED_WT) }),
                1
            );
        } else {
            assert_eq!(w.count(|c| matches!(c, Call::RemoveForce { .. })), 0);
        }
        match branch_created {
            Some(b) => assert_eq!(w.count(|c| *c == Call::BranchDelete(b.into())), 1),
            None => assert_eq!(w.count(|c| matches!(c, Call::BranchDelete(_))), 0),
        }
        assert!(!w.touched(Path::new(DEST)) || w.count(|c| matches!(c, Call::Rename { .. })) == 1);
        assert_eq!(
            w.count(|c| *c == Call::RemoveDirAll(p(DEST))),
            0,
            "dest never deleted"
        );
    }

    // ---------------------------------------------------------------- happy path

    // U-TXN-01
    #[test]
    fn happy_path_runs_steps_in_order() {
        let w = World::default();
        let out = run(&w, &new_branch()).unwrap();

        let staging = w
            .position(|c| *c == Call::CreateStaging { parent: p("/w") })
            .unwrap();
        let add = w
            .position(|c| {
                *c == Call::WorktreeAdd(WorktreeAddSpec {
                    path: p(STAGED_WT),
                    branch: BranchMode::New("feat".into()),
                    reference: None,
                    no_checkout: true,
                })
            })
            .unwrap();
        let first_clone = w.position(|c| matches!(c, Call::Clone { .. })).unwrap();
        let index = w
            .position(|c| matches!(c, Call::ResetIndex { .. }))
            .unwrap();
        let rename = w
            .position(|c| {
                *c == Call::Rename {
                    from: p(STAGED_WT),
                    to: p(DEST),
                }
            })
            .unwrap();
        let repair = w
            .position(|c| *c == Call::Repair { path: p(DEST) })
            .unwrap();
        let cleanup = w
            .position(|c| *c == Call::RemoveDirAll(p(STAGING)))
            .unwrap();
        assert!(staging < add && add < first_clone && first_clone < index);
        assert!(index < rename && rename < repair && repair < cleanup);

        assert_eq!(
            w.count(|c| matches!(c, Call::RemoveForce { .. } | Call::BranchDelete(_))),
            0
        );
        assert_eq!(
            out,
            AddOutcome {
                destination: p(DEST),
                head: SHA_A.into(),
                strategy: Strategy::CopyOnWrite,
                warnings: vec![],
                relocated: 0,
            }
        );
    }

    // U-TXN-02
    #[test]
    fn clones_every_top_level_entry_except_dot_git() {
        let w = World::default();
        run(&w, &new_branch()).unwrap();
        let clones: Vec<_> = w
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Clone { .. }))
            .collect();
        assert_eq!(
            clones,
            vec![
                Call::Clone {
                    src: p("/w/repo/node_modules"),
                    dst: p(&format!("{STAGED_WT}/node_modules"))
                },
                Call::Clone {
                    src: p("/w/repo/src"),
                    dst: p(&format!("{STAGED_WT}/src"))
                },
            ]
        );
    }

    // U-TXN-03
    #[test]
    fn same_target_commit_only_resets_index() {
        let w = World::default();
        run(&w, &new_branch()).unwrap();
        assert_eq!(
            w.count(|c| *c
                == Call::ResetIndex {
                    worktree: p(STAGED_WT),
                    commit: SHA_A.into()
                }),
            1
        );
        assert_eq!(w.count(|c| matches!(c, Call::CheckoutTwoWay { .. })), 0);
    }

    // U-TXN-04
    #[test]
    fn different_target_commit_checks_out_two_way_after_reset() {
        let w = World {
            staging_head: SHA_B.into(),
            ..World::default()
        };
        let mut req = new_branch();
        req.reference = Some("v1".into());
        let out = run(&w, &req).unwrap();
        let reset = w
            .position(|c| {
                *c == Call::ResetIndex {
                    worktree: p(STAGED_WT),
                    commit: SHA_A.into(),
                }
            })
            .unwrap();
        let checkout = w
            .position(|c| {
                *c == Call::CheckoutTwoWay {
                    worktree: p(STAGED_WT),
                    from: SHA_A.into(),
                    to: SHA_B.into(),
                }
            })
            .unwrap();
        assert!(reset < checkout);
        assert_eq!(out.head, SHA_B);
    }

    // U-TXN-05
    #[rstest]
    #[case::existing(BranchMode::Existing("topic".into()))]
    #[case::detach(BranchMode::Detach)]
    #[case::default(BranchMode::Default)]
    fn forwards_branch_mode_and_ref_to_git(#[case] mode: BranchMode) {
        let w = World::default();
        let mut req = request(mode.clone());
        req.reference = Some("main".into());
        run(&w, &req).unwrap();
        assert_eq!(
            w.count(|c| {
                *c == Call::WorktreeAdd(WorktreeAddSpec {
                    path: p(STAGED_WT),
                    branch: mode.clone(),
                    reference: Some("main".into()),
                    no_checkout: true,
                })
            }),
            1
        );
    }

    // U-TXN-06: journal persisted before the registration and updated before cloning.
    #[test]
    fn journal_is_written_ahead_of_each_mutation() {
        let w = World::default().failing(Step::Clone, ErrorCode::CloneFailed);
        let _ = run(&w, &new_branch());
        let journal = p(&format!("{STAGING}/{JOURNAL_FILE}"));
        let first_write = w
            .position(|c| *c == Call::WriteFile(journal.clone()))
            .unwrap();
        let add = w.position(|c| matches!(c, Call::WorktreeAdd(_))).unwrap();
        assert!(
            first_write < add,
            "journal must exist before git registers anything"
        );
        let writes: Vec<_> = w
            .calls()
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == Call::WriteFile(journal.clone()))
            .map(|(i, _)| i)
            .collect();
        let clone = w.position(|c| matches!(c, Call::Clone { .. })).unwrap();
        assert!(
            writes.iter().any(|&i| add < i && i < clone),
            "journal updated after add"
        );
    }

    // U-TXN-07: the persisted journal content (what doctor --fix would read after a crash).
    #[test]
    fn journal_records_registration_and_created_branch() {
        let w = World::default().failing(Step::ResetIndex, ErrorCode::GitFailed);
        // Prevent rollback from erasing evidence: make staging removal fail too.
        w.fail
            .borrow_mut()
            .insert(Step::RemoveDirAll, ErrorCode::Io);
        let _ = run(&w, &new_branch());
        let bytes = w.files.borrow()[&p(&format!("{STAGING}/{JOURNAL_FILE}"))].clone();
        let j = Journal::decode(&bytes).unwrap();
        assert_eq!(j.repo, p("/w/repo"));
        assert_eq!(j.worktree, Some(p(STAGED_WT)));
        assert_eq!(j.created_branch.as_deref(), Some("feat"));
    }

    // ---------------------------------------------------------------- refusals

    // U-TXN-10..: each refusal happens before any mutation.
    #[test]
    fn refuses_dirty_source() {
        let w = World {
            dirty: true,
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::DirtySource
        );
        assert_no_mutation(&w);
    }

    #[test]
    fn allow_dirty_proceeds() {
        let w = World {
            dirty: true,
            ..World::default()
        };
        let mut req = new_branch();
        req.allow_dirty = true;
        run(&w, &req).unwrap();
        assert_eq!(w.count(|c| matches!(c, Call::Rename { .. })), 1);
    }

    #[test]
    fn refuses_sparse_checkout() {
        let w = World {
            sparse: true,
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::SparseCheckout
        );
        assert_no_mutation(&w);
    }

    #[test]
    fn refuses_initialized_submodules_by_default() {
        let w = World {
            submodules: vec![p("libs/a")],
            ..World::default()
        };
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::SubmodulesPresent);
        assert!(err.message.contains("libs/a") && err.message.contains("--submodules=reinit"));
        assert_no_mutation(&w);
    }

    #[test]
    fn refuses_old_git() {
        let w = World {
            version: GitVersion {
                major: 2,
                minor: 28,
                patch: 0,
            },
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::GitTooOld
        );
        assert_no_mutation(&w);
    }

    #[rstest]
    #[case(ErrorCode::NotARepo)]
    #[case(ErrorCode::BareRepo)]
    #[case(ErrorCode::UnbornHead)]
    #[case(ErrorCode::InsideGitDir)]
    fn propagates_discovery_errors(#[case] code: ErrorCode) {
        let w = World {
            discover_error: Some(code),
            ..World::default()
        };
        assert_eq!(run(&w, &new_branch()).unwrap_err().code, code);
        assert_no_mutation(&w);
    }

    #[test]
    fn refuses_source_without_cow() {
        let w = World {
            volumes: vec![
                (p("/"), apfs(1)),
                (
                    p("/w/repo"),
                    VolumeInfo {
                        fs_type: "hfs".into(),
                        cow: false,
                        device: 2,
                    },
                ),
            ],
            ..World::default()
        };
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotCow);
        assert!(err.message.contains("/w/repo") && err.message.contains("hfs"));
        assert_no_mutation(&w);
    }

    // The decision comes from `cow`, never from the filesystem's name.
    #[test]
    fn cow_flag_decides_not_the_fs_name() {
        let w = World {
            volumes: vec![
                (p("/"), apfs(1)),
                (
                    p("/w/repo"),
                    VolumeInfo {
                        fs_type: "apfs".into(),
                        cow: false,
                        device: 1,
                    },
                ),
            ],
            ..World::default()
        };
        assert_eq!(run(&w, &new_branch()).unwrap_err().code, ErrorCode::NotCow);
        assert_no_mutation(&w);
    }

    #[test]
    fn refuses_destination_parent_without_cow() {
        let w = World {
            volumes: vec![
                (
                    p("/"),
                    VolumeInfo {
                        fs_type: "exfat".into(),
                        cow: false,
                        device: 2,
                    },
                ),
                (p("/w/repo"), apfs(1)),
            ],
            ..World::default()
        };
        assert_eq!(run(&w, &new_branch()).unwrap_err().code, ErrorCode::NotCow);
        assert_no_mutation(&w);
    }

    #[test]
    fn refuses_cross_volume() {
        let w = World {
            volumes: vec![(p("/"), apfs(1)), (p("/w/repo"), apfs(7))],
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::CrossVolume
        );
        assert_no_mutation(&w);
    }

    #[test]
    fn refuses_existing_destination() {
        let w = World::default();
        w.existing.borrow_mut().insert(p(DEST));
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::DestExists
        );
        assert_no_mutation(&w);
    }

    #[test]
    fn refuses_missing_destination_parent() {
        let w = World::default();
        let mut req = new_branch();
        req.destination = p("/nope/feat");
        assert_eq!(
            run(&w, &req).unwrap_err().code,
            ErrorCode::DestParentMissing
        );
        assert_no_mutation(&w);
    }

    #[rstest]
    #[case::is_source("/w/repo", ErrorCode::DestIsSource)]
    #[case::in_git_dir("/w/repo/.git/feat", ErrorCode::DestInsideGitDir)]
    fn refuses_bad_destination_placement(#[case] dest: &str, #[case] code: ErrorCode) {
        let w = World::default();
        let mut req = new_branch();
        req.destination = p(dest);
        assert_eq!(run(&w, &req).unwrap_err().code, code);
        assert_no_mutation(&w);
    }

    // ---------------------------------------------------------------- fallback

    // U-TXN-20
    #[test]
    fn fallback_uses_plain_worktree_add_without_cow() {
        let w = World {
            volumes: vec![(
                p("/"),
                VolumeInfo {
                    fs_type: "hfs".into(),
                    cow: false,
                    device: 1,
                },
            )],
            ..World::default()
        };
        let mut req = new_branch();
        req.fallback = true;
        let out = run(&w, &req).unwrap();
        assert_eq!(
            w.calls(),
            vec![Call::WorktreeAdd(WorktreeAddSpec {
                path: p(DEST),
                branch: BranchMode::New("feat".into()),
                reference: None,
                no_checkout: false,
            })]
        );
        assert_eq!(out.strategy, Strategy::Fallback);
        assert!(
            out.warnings.iter().any(|m| m.contains("copy-on-write")),
            "{:?}",
            out.warnings
        );
    }

    // U-TXN-21
    #[test]
    fn fallback_also_covers_cross_volume() {
        let w = World {
            volumes: vec![(p("/"), apfs(1)), (p("/w/repo"), apfs(7))],
            ..World::default()
        };
        let mut req = new_branch();
        req.fallback = true;
        assert_eq!(run(&w, &req).unwrap().strategy, Strategy::Fallback);
    }

    // U-TXN-22
    #[test]
    fn fallback_flag_is_unused_when_cow_is_possible() {
        let w = World::default();
        let mut req = new_branch();
        req.fallback = true;
        assert_eq!(run(&w, &req).unwrap().strategy, Strategy::CopyOnWrite);
        assert_eq!(w.count(|c| matches!(c, Call::Clone { .. })), 2);
    }

    // U-TXN-23: fallback never bypasses safety refusals.
    #[test]
    fn fallback_does_not_bypass_dirty_or_dest_checks() {
        let w = World {
            dirty: true,
            volumes: vec![(
                p("/"),
                VolumeInfo {
                    fs_type: "hfs".into(),
                    cow: false,
                    device: 1,
                },
            )],
            ..World::default()
        };
        let mut req = new_branch();
        req.fallback = true;
        assert_eq!(run(&w, &req).unwrap_err().code, ErrorCode::DirtySource);
        assert_no_mutation(&w);
    }

    // ---------------------------------------------------------------- exclusions

    // U-TXN-30
    #[test]
    fn destination_nested_in_source_never_clones_staging() {
        let w = World::default();
        w.tree
            .borrow_mut()
            .get_mut(&p("/w/repo"))
            .unwrap()
            .push(OsString::from(".worktrees"));
        w.tree.borrow_mut().insert(p("/w/repo/.worktrees"), vec![]);
        let mut req = new_branch();
        req.destination = p("/w/repo/.worktrees/feat");
        run(&w, &req).unwrap();
        let staging = p("/w/repo/.worktrees").join(FAKE_STAGING);
        assert!(
            w.calls()
                .iter()
                .all(|c| !matches!(c, Call::Clone { src, .. } if src.starts_with(&staging))),
            "{:#?}",
            w.calls()
        );
        assert_eq!(
            w.count(|c| *c
                == Call::Rename {
                    from: staging.join("feat"),
                    to: p("/w/repo/.worktrees/feat")
                }),
            1
        );
    }

    // U-TXN-31
    #[test]
    fn nested_linked_worktree_is_not_cloned() {
        let mut w = World::default();
        w.worktrees.push(WorktreeEntry {
            path: p("/w/repo/wt/other"),
            head: Some(SHA_A.into()),
            branch: Some("refs/heads/other".into()),
            ..WorktreeEntry::default()
        });
        w.tree
            .borrow_mut()
            .get_mut(&p("/w/repo"))
            .unwrap()
            .push(OsString::from("wt"));
        w.tree.borrow_mut().insert(
            p("/w/repo/wt"),
            vec![OsString::from("other"), OsString::from("notes")],
        );
        run(&w, &new_branch()).unwrap();
        assert_eq!(
            w.count(
                |c| matches!(c, Call::Clone { src, .. } if src.starts_with("/w/repo/wt/other"))
            ),
            0
        );
        assert_eq!(
            w.count(|c| *c == Call::CreateDir(p(&format!("{STAGED_WT}/wt"))))
                + w.count(
                    |c| matches!(c, Call::Clone { src, .. } if *src == p("/w/repo/wt/notes"))
                ),
            2
        );
    }

    // U-TXN-32
    #[test]
    fn reinit_skips_submodule_dirs_and_reinitializes_before_publish() {
        let w = World {
            submodules: vec![p("src")],
            ..World::default()
        };
        let mut req = new_branch();
        req.submodules = SubmodulePolicy::Reinit;
        run(&w, &req).unwrap();
        assert_eq!(
            w.count(|c| matches!(c, Call::Clone { src, .. } if src.starts_with("/w/repo/src"))),
            0
        );
        let reinit = w
            .position(|c| {
                *c == Call::SubmodulesReinit {
                    worktree: p(STAGED_WT),
                }
            })
            .unwrap();
        let index = w
            .position(|c| matches!(c, Call::ResetIndex { .. }))
            .unwrap();
        let rename = w.position(|c| matches!(c, Call::Rename { .. })).unwrap();
        assert!(index < reinit && reinit < rename);
    }

    // ---------------------------------------------------------------- rollback

    // U-TXN-40: failure at each pre-publish step rolls back exactly what was created.
    #[rstest]
    #[case::worktree_add(Step::WorktreeAdd, ErrorCode::GitFailed, None)]
    #[case::clone(Step::Clone, ErrorCode::CloneFailed, Some("feat"))]
    #[case::reset_index(Step::ResetIndex, ErrorCode::GitFailed, Some("feat"))]
    #[case::rename_io(Step::Rename, ErrorCode::Io, Some("feat"))]
    fn rolls_back_on_failure(
        #[case] step: Step,
        #[case] code: ErrorCode,
        #[case] branch: Option<&str>,
    ) {
        let w = World::default().failing(step, code);
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            code,
            "original error surfaces"
        );
        assert_rolled_back(&w, branch);
    }

    // U-TXN-41
    #[test]
    fn rolls_back_when_two_way_checkout_conflicts() {
        let w = World {
            staging_head: SHA_B.into(),
            ..World::default()
        }
        .failing(Step::CheckoutTwoWay, ErrorCode::CheckoutConflict);
        let mut req = new_branch();
        req.allow_dirty = true;
        req.reference = Some("v1".into());
        assert_eq!(run(&w, &req).unwrap_err().code, ErrorCode::CheckoutConflict);
        assert_rolled_back(&w, Some("feat"));
    }

    // U-TXN-42
    #[test]
    fn rolls_back_when_submodule_reinit_fails() {
        let w = World {
            submodules: vec![p("src")],
            ..World::default()
        }
        .failing(Step::SubmodulesReinit, ErrorCode::GitFailed);
        let mut req = new_branch();
        req.submodules = SubmodulePolicy::Reinit;
        assert_eq!(run(&w, &req).unwrap_err().code, ErrorCode::GitFailed);
        assert_rolled_back(&w, Some("feat"));
    }

    // U-TXN-43: lost race at publish time -> destination untouched (bug 1).
    #[test]
    fn lost_publish_race_never_touches_destination() {
        let w = World::default().failing(Step::Rename, ErrorCode::DestExists);
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::DestExists
        );
        assert_rolled_back(&w, Some("feat"));
        let after_rename: Vec<_> = w
            .calls()
            .into_iter()
            .skip_while(|c| !matches!(c, Call::Rename { .. }))
            .skip(1)
            .collect();
        assert!(
            after_rename
                .iter()
                .all(|c| c.touched_paths().iter().all(|t| !t.starts_with(DEST))),
            "{after_rename:#?}"
        );
    }

    // U-TXN-44: pre-existing branches are never deleted (bug 3 / I-42).
    #[rstest]
    #[case::existing(BranchMode::Existing("main".into()))]
    #[case::new_branch_name_taken(BranchMode::New("main".into()))]
    #[case::detach(BranchMode::Detach)]
    fn never_deletes_branch_it_did_not_create(#[case] mode: BranchMode) {
        let fail_at = if matches!(mode, BranchMode::New(_)) {
            Step::WorktreeAdd
        } else {
            Step::Clone
        };
        let w = World::default().failing(fail_at, ErrorCode::GitFailed);
        let _ = run(&w, &request(mode));
        assert_eq!(
            w.count(|c| matches!(c, Call::BranchDelete(_))),
            0,
            "{:#?}",
            w.calls()
        );
    }

    // U-TXN-45: implicit basename branch is deleted only if this run created it.
    #[rstest]
    #[case::created(false, 1)]
    #[case::preexisting(true, 0)]
    fn default_mode_deletes_implicit_branch_only_if_created(
        #[case] preexisting: bool,
        #[case] deletes: usize,
    ) {
        let w = World::default().failing(Step::Clone, ErrorCode::CloneFailed);
        if preexisting {
            w.branches.borrow_mut().insert("feat".into());
        }
        let _ = run(&w, &request(BranchMode::Default));
        assert_eq!(
            w.count(|c| *c == Call::BranchDelete("feat".into())),
            deletes
        );
    }

    // U-TXN-46: failure after publish keeps the (valid) worktree and says how to repair.
    #[test]
    fn repair_failure_after_publish_keeps_destination() {
        let w = World::default().failing(Step::Repair, ErrorCode::GitFailed);
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::GitFailed);
        assert!(
            err.message.contains("git worktree repair"),
            "{}",
            err.message
        );
        assert_eq!(
            w.count(|c| matches!(c, Call::RemoveForce { .. } | Call::BranchDelete(_))),
            0
        );
        assert_eq!(w.count(|c| *c == Call::RemoveDirAll(p(DEST))), 0);
    }

    // U-TXN-47: rollback failure is reported with the original cause.
    #[test]
    fn rollback_failure_reports_both_errors() {
        let w = World::default().failing(Step::Clone, ErrorCode::CloneFailed);
        w.fail
            .borrow_mut()
            .insert(Step::RemoveDirAll, ErrorCode::Io);
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::RollbackFailed);
        assert!(
            err.message.contains("clone-failed"),
            "original cause kept: {}",
            err.message
        );
        assert!(
            err.message.contains(STAGING),
            "names leftover: {}",
            err.message
        );
    }

    // U-TXN-48: SIGINT between steps -> interrupted + full rollback.
    #[rstest]
    #[case(Step::WorktreeAdd)]
    #[case(Step::Clone)]
    #[case(Step::ResetIndex)]
    fn interrupt_rolls_back(#[case] after: Step) {
        let w = World::default();
        w.interrupt_after.set(Some(after));
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::Interrupted
        );
        assert!(w.interrupted.load(Ordering::SeqCst));
        assert_rolled_back(&w, Some("feat"));
        assert_eq!(
            w.count(|c| matches!(c, Call::Rename { .. })),
            0,
            "never publishes"
        );
    }

    // ---------------------------------------------------------------- source state

    // U-TXN-50: operation in progress is refused even with --allow-dirty.
    #[rstest]
    #[case(false)]
    #[case(true)]
    fn refuses_operation_in_progress(#[case] allow_dirty: bool) {
        let w = World {
            operation: Some("merge".into()),
            ..World::default()
        };
        let mut req = new_branch();
        req.allow_dirty = allow_dirty;
        let err = run(&w, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationInProgress);
        assert!(err.message.contains("merge"), "{}", err.message);
        assert_no_mutation(&w);
    }

    // U-TXN-51: assume-unchanged / skip-worktree entries count as dirty.
    #[test]
    fn flagged_index_entries_are_dirty() {
        let w = World {
            flagged: vec![p("config/local.yml")],
            ..World::default()
        };
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::DirtySource);
        assert!(err.message.contains("config/local.yml"), "{}", err.message);
        assert!(err.message.contains("assume-unchanged"), "{}", err.message);
        assert_no_mutation(&w);
    }

    // U-TXN-52
    #[test]
    fn flagged_index_entries_pass_with_allow_dirty() {
        let w = World {
            flagged: vec![p("config/local.yml")],
            ..World::default()
        };
        let mut req = new_branch();
        req.allow_dirty = true;
        assert_eq!(run(&w, &req).unwrap().strategy, Strategy::CopyOnWrite);
    }

    // U-TXN-53: source HEAD moved while cloning -> abort, never publish.
    #[test]
    fn source_head_moving_during_clone_aborts() {
        let w = World {
            source_head_after_clone: Some(SHA_B.into()),
            ..World::default()
        };
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::SourceChanged);
        assert_eq!(
            w.count(|c| matches!(c, Call::Rename { .. })),
            0,
            "never publishes"
        );
        assert_rolled_back(&w, Some("feat"));
    }

    // U-TXN-54: source edited while cloning -> abort.
    #[test]
    fn source_edited_during_clone_aborts() {
        let w = World {
            dirty_after_clone: true,
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::SourceChanged
        );
        assert_eq!(w.count(|c| matches!(c, Call::Rename { .. })), 0);
        assert_rolled_back(&w, Some("feat"));
    }

    // U-TXN-55: documented limit — with --allow-dirty only HEAD moves are detected.
    #[test]
    fn allow_dirty_only_detects_head_moves() {
        let w = World {
            dirty: true,
            dirty_after_clone: true,
            ..World::default()
        };
        let mut req = new_branch();
        req.allow_dirty = true;
        run(&w, &req).unwrap();
        let w = World {
            dirty: true,
            source_head_after_clone: Some(SHA_B.into()),
            ..World::default()
        };
        assert_eq!(run(&w, &req).unwrap_err().code, ErrorCode::SourceChanged);
    }

    // U-TXN-56: the journal is locked for the whole run, before anything is registered.
    #[test]
    fn journal_lock_taken_before_registration() {
        let w = World::default();
        run(&w, &new_branch()).unwrap();
        let lock = w
            .position(|c| *c == Call::Lock(p(&format!("{STAGING}/{JOURNAL_FILE}"))))
            .expect("journal locked");
        let add = w.position(|c| matches!(c, Call::WorktreeAdd(_))).unwrap();
        assert!(lock < add);
    }

    // ---------------------------------------------------------------- early failures

    // U-TXN-57: staging cannot be created -> nothing else attempted.
    #[test]
    fn staging_creation_failure_attempts_nothing_else() {
        let w = World::default().failing(Step::CreateStaging, ErrorCode::PermissionDenied);
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(w.calls(), vec![Call::CreateStaging { parent: p("/w") }]);
    }

    // U-TXN-58: journal cannot be written -> staging removed, git never touched.
    #[test]
    fn journal_write_failure_removes_staging_only() {
        let w = World::default().failing(Step::WriteJournal, ErrorCode::Io);
        assert_eq!(run(&w, &new_branch()).unwrap_err().code, ErrorCode::Io);
        assert_eq!(w.count(|c| matches!(c, Call::WorktreeAdd(_))), 0);
        assert_eq!(w.calls().last(), Some(&Call::RemoveDirAll(p(STAGING))));
    }

    // U-TXN-59: destination inside a submodule's working tree.
    #[test]
    fn refuses_destination_inside_submodule() {
        let w = World {
            submodules: vec![p("libs/a")],
            ..World::default()
        };
        w.tree.borrow_mut().insert(p("/w/repo/libs/a"), vec![]);
        let mut req = new_branch();
        req.submodules = SubmodulePolicy::Reinit;
        req.destination = p("/w/repo/libs/a/wt");
        assert_eq!(
            run(&w, &req).unwrap_err().code,
            ErrorCode::DestInsideSubmodule
        );
        assert_no_mutation(&w);
    }

    // U-TXN-60: names APFS cannot store are refused up front, not at publish time.
    // macOS only: Linux filesystems store any byte but `/` and NUL (U-TXN-60b).
    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_non_utf8_destination_before_mutation() {
        use std::os::unix::ffi::OsStrExt;
        let w = World::default();
        let mut req = new_branch();
        req.destination = Path::new("/w").join(std::ffi::OsStr::from_bytes(b"feat-\xff"));
        assert_eq!(run(&w, &req).unwrap_err().code, ErrorCode::InvalidPath);
        assert_no_mutation(&w);
    }

    // U-TXN-60b: where names are bytes, a non-UTF-8 destination is fine, except that
    // it cannot name the implicit branch (which the journal stores as text).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_utf8_destination_is_accepted_unless_it_names_the_branch() {
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new("/w").join(std::ffi::OsStr::from_bytes(b"feat-\xff"));
        let w = World::default();
        let mut req = new_branch();
        req.destination.clone_from(&bad);
        assert_eq!(run(&w, &req).unwrap().destination, bad);
        let w = World::default();
        let mut req = new_branch();
        req.destination = bad;
        req.branch = BranchMode::Default;
        assert_eq!(run(&w, &req).unwrap_err().code, ErrorCode::InvalidPath);
        assert_no_mutation(&w);
    }

    // U-TXN-61: refusal order — cheapest, most specific first; no mutation in any case.
    #[test]
    fn several_problems_report_the_first_in_documented_order() {
        // operation-in-progress wins over dirty-source (a merge implies a dirty tree).
        let w = World {
            dirty: true,
            operation: Some("rebase".into()),
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::OperationInProgress
        );
        // usage-level destination problems win over repository state.
        let w = World {
            dirty: true,
            ..World::default()
        };
        let mut req = new_branch();
        req.destination = p("/w/repo");
        assert_eq!(run(&w, &req).unwrap_err().code, ErrorCode::DestIsSource);
        assert_no_mutation(&w);
    }

    // U-TXN-63: the destination is journaled before the rename (crash recovery).
    #[test]
    fn publish_intent_is_journaled_before_rename() {
        let w = World::default();
        run(&w, &new_branch()).unwrap();
        let journal = p(&format!("{STAGING}/{JOURNAL_FILE}"));
        let last_write = w
            .calls()
            .iter()
            .rposition(|c| *c == Call::WriteFile(journal.clone()))
            .unwrap();
        let rename = w.position(|c| matches!(c, Call::Rename { .. })).unwrap();
        assert!(last_write < rename, "journal updated before publishing");
        let j = Journal::decode(&w.files.borrow()[&journal]).unwrap();
        assert_eq!(j.publish_to, Some(p(DEST)));
        assert_eq!(j.worktree, Some(p(STAGED_WT)));
    }

    // U-TXN-62: a symlink on the path to an exclusion is cloned as a link, never
    // descended into (that would turn the link into a real directory).
    #[test]
    fn symlink_ancestor_of_exclusion_is_cloned_as_link() {
        let mut w = World::default();
        w.worktrees.push(WorktreeEntry {
            path: p("/w/repo/lnk/wt"),
            ..WorktreeEntry::default()
        });
        w.tree
            .borrow_mut()
            .get_mut(&p("/w/repo"))
            .unwrap()
            .push(OsString::from("lnk"));
        w.tree.borrow_mut().insert(
            p("/w/repo/lnk"),
            vec![OsString::from("wt"), OsString::from("f")],
        );
        w.symlinks.borrow_mut().insert(p("/w/repo/lnk"));
        run(&w, &new_branch()).unwrap();
        assert_eq!(
            w.count(|c| *c
                == Call::Clone {
                    src: p("/w/repo/lnk"),
                    dst: p(&format!("{STAGED_WT}/lnk"))
                }),
            1,
            "{:#?}",
            w.calls()
        );
        assert_eq!(
            w.count(|c| matches!(c, Call::CreateDir(d) if d.ends_with("lnk"))),
            0
        );
    }

    // ---------------------------------------------------------------- relocation

    /// Staged worktree content: a script, a binary and a tracked file that all
    /// mention the source, plus an absolute symlink into the source.
    fn world_with_references() -> World {
        World {
            staged_files: vec![
                (p("env/bin/tool"), b"#!/w/repo/env/bin/python\n".to_vec()),
                (p("env/data.bin"), b"\0/w/repo/lib\0".to_vec()),
                (p("tracked.txt"), b"see /w/repo\n".to_vec()),
            ],
            staged_links: vec![(p("env/link"), p("/w/repo/env/bin"))],
            tracked: vec![p("tracked.txt")],
            ..World::default()
        }
    }

    // U-TXN-64
    #[test]
    fn relocates_untracked_text_and_symlinks_before_publishing() {
        let w = world_with_references();
        let out = run(&w, &new_branch()).unwrap();
        let tool = p(&format!("{STAGED_WT}/env/bin/tool"));
        let rewrite = w
            .position(|c| *c == Call::Rewrite(tool.clone()))
            .expect("script rewritten");
        assert_eq!(w.files.borrow()[&tool], b"#!/w/feat/env/bin/python\n");
        let relink = w
            .position(|c| {
                *c == Call::Relink {
                    path: p(&format!("{STAGED_WT}/env/link")),
                    target: p("/w/feat/env/bin"),
                }
            })
            .expect("symlink relinked");
        let index = w
            .position(|c| matches!(c, Call::ResetIndex { .. }))
            .unwrap();
        let rename = w.position(|c| matches!(c, Call::Rename { .. })).unwrap();
        assert!(index < rewrite && rewrite < rename && relink < rename);
        assert_eq!(out.relocated, 2);
    }

    // U-TXN-65
    #[test]
    fn never_touches_tracked_files_or_binaries() {
        let w = world_with_references();
        let out = run(&w, &new_branch()).unwrap();
        assert_eq!(
            w.count(|c| matches!(c, Call::Rewrite(p) if p.ends_with("tracked.txt"))),
            0
        );
        assert_eq!(
            w.count(|c| matches!(c, Call::Rewrite(p) if p.ends_with("data.bin"))),
            0
        );
        let warning = out
            .warnings
            .iter()
            .find(|m| m.contains("env/data.bin"))
            .expect("warned");
        assert!(warning.contains(".parhelignore"), "{warning}");
    }

    // U-TXN-66
    #[test]
    fn no_relocate_leaves_everything_as_cloned() {
        let w = world_with_references();
        let mut req = new_branch();
        req.relocate = false;
        let out = run(&w, &req).unwrap();
        assert_eq!(
            w.count(|c| matches!(c, Call::Rewrite(_) | Call::Relink { .. })),
            0
        );
        assert_eq!(out.relocated, 0);
    }

    // U-TXN-67: large files are not examined.
    #[test]
    fn large_files_are_not_examined() {
        let w = world_with_references();
        w.big
            .borrow_mut()
            .insert(p(&format!("{STAGED_WT}/env/bin/tool")));
        run(&w, &new_branch()).unwrap();
        assert_eq!(w.count(|c| matches!(c, Call::Rewrite(_))), 0);
    }

    // U-TXN-68: submodules (gitlinks in the index) are other repositories.
    #[test]
    fn does_not_descend_into_submodules() {
        let w = World {
            staged_files: vec![(p("libs/a/conf"), b"/w/repo\n".to_vec())],
            tracked: vec![p("libs/a")],
            ..World::default()
        };
        run(&w, &new_branch()).unwrap();
        assert_eq!(w.count(|c| matches!(c, Call::Rewrite(_))), 0);
    }

    // U-TXN-69: .parhelignore paths are not cloned at all.
    #[test]
    fn parhelignored_paths_are_not_cloned() {
        let w = World {
            parhelignored: vec![p("node_modules")],
            ..World::default()
        };
        run(&w, &new_branch()).unwrap();
        assert_eq!(
            w.count(|c| matches!(c, Call::Clone { src, .. } if src.ends_with("node_modules"))),
            0
        );
        assert_eq!(
            w.count(|c| matches!(c, Call::Clone { .. })),
            1,
            "src/ still cloned"
        );
    }

    // U-TXN-70: a copied gitfile linked to a git dir outside the copy would let the
    // copy drive another repository's HEAD and index: it is removed, with a warning.
    #[rstest]
    #[case::relocate(true)]
    #[case::no_relocate(false)]
    fn detaches_gitfiles_linked_outside_the_copy(#[case] relocate: bool) {
        let w = World {
            staged_files: vec![(
                p("vendor/lib/.git"),
                b"gitdir: /elsewhere/other/.git/worktrees/lib\n".to_vec(),
            )],
            ..World::default()
        };
        let mut req = new_branch();
        req.relocate = relocate;
        let out = run(&w, &req).unwrap();
        assert_eq!(
            w.count(|c| *c == Call::RemoveFile(p(&format!("{STAGED_WT}/vendor/lib/.git")))),
            1
        );
        let warning = out
            .warnings
            .iter()
            .find(|m| m.contains("vendor/lib"))
            .expect("warned");
        assert!(warning.contains("another repository"), "{warning}");
    }

    // U-TXN-71: a gitfile whose git dir is copied along (a nested repository with
    // its own .git dir) keeps working in the copy.
    #[test]
    fn keeps_gitfiles_resolving_inside_the_copy() {
        let w = World {
            staged_files: vec![(
                p("vendor/repo/sub/.git"),
                b"gitdir: ../.git/modules/sub\n".to_vec(),
            )],
            ..World::default()
        };
        run(&w, &new_branch()).unwrap();
        assert_eq!(w.count(|c| matches!(c, Call::RemoveFile(_))), 0);
    }

    // U-TXN-72: directories parhel creates (the worktree root, ancestors of excluded
    // paths) get the source's permission bits, never the defaults.
    #[test]
    fn created_directories_copy_the_source_mode() {
        let mut w = World::default();
        w.worktrees.push(WorktreeEntry {
            path: p("/w/repo/wt/other"),
            ..WorktreeEntry::default()
        });
        w.tree
            .borrow_mut()
            .get_mut(&p("/w/repo"))
            .unwrap()
            .push(OsString::from("wt"));
        w.tree
            .borrow_mut()
            .insert(p("/w/repo/wt"), vec![OsString::from("other")]);
        run(&w, &new_branch()).unwrap();
        for (from, to) in [
            ("/w/repo", STAGED_WT.to_owned()),
            ("/w/repo/wt", format!("{STAGED_WT}/wt")),
        ] {
            assert_eq!(
                w.count(|c| *c
                    == Call::CopyMode {
                        from: p(from),
                        to: p(&to)
                    }),
                1,
                "{from} -> {to}: {:#?}",
                w.calls()
            );
        }
        let mode = w.position(|c| matches!(c, Call::CopyMode { .. })).unwrap();
        let rename = w.position(|c| matches!(c, Call::Rename { .. })).unwrap();
        assert!(mode < rename);
    }

    // U-TXN-73: reinit cannot carry uncommitted submodule changes: refuse instead
    // of silently dropping them.
    #[test]
    fn allow_dirty_with_reinit_refuses_dirty_submodules() {
        let w = World {
            submodules: vec![p("libs/a"), p("libs/b")],
            dirty_paths: [p("/w/repo/libs/b")].into(),
            ..World::default()
        };
        let mut req = new_branch();
        req.allow_dirty = true;
        req.submodules = SubmodulePolicy::Reinit;
        let err = run(&w, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::DirtySubmodule);
        assert!(
            err.message.contains("libs/b") && !err.message.contains("libs/a"),
            "{}",
            err.message
        );
        assert_no_mutation(&w);
    }

    // U-TXN-74: clean submodules with --allow-dirty and reinit are fine.
    #[test]
    fn allow_dirty_with_reinit_accepts_clean_submodules() {
        let w = World {
            submodules: vec![p("libs/a")],
            ..World::default()
        };
        let mut req = new_branch();
        req.allow_dirty = true;
        req.submodules = SubmodulePolicy::Reinit;
        run(&w, &req).unwrap();
    }

    // U-TXN-75: git creates the `-b` branch before it can fail to add the worktree;
    // the branch is journaled ahead so the rollback removes it.
    #[test]
    fn branch_created_by_a_failed_worktree_add_is_removed() {
        let w = World::default().failing(Step::WorktreeAdd, ErrorCode::GitFailed);
        // The fake creates the branch before failing, like git.
        w.fail_after_branch.set(true);
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::GitFailed
        );
        assert_eq!(w.count(|c| *c == Call::BranchDelete("feat".into())), 1);
        assert!(!w.branches.borrow().contains("feat"));
    }

    // U-TXN-76: the journal names the branch before git is asked to create it.
    #[test]
    fn branch_is_journaled_before_worktree_add() {
        let w = World::default().failing(Step::WorktreeAdd, ErrorCode::GitFailed);
        w.fail
            .borrow_mut()
            .insert(Step::RemoveDirAll, ErrorCode::Io);
        let _ = run(&w, &new_branch());
        let j =
            Journal::decode(&w.files.borrow()[&p(&format!("{STAGING}/{JOURNAL_FILE}"))]).unwrap();
        assert_eq!(j.created_branch.as_deref(), Some("feat"));
        assert_eq!(j.branch_start.as_deref(), Some(SHA_A));
    }

    // U-TXN-77: a gitfile pointing (absolutely) at a git dir that is copied along
    // must follow the copy, or the copy would drive the source's nested repo.
    #[rstest]
    #[case::relocated(true)]
    #[case::detached_without_relocation(false)]
    fn gitfile_into_a_copied_git_dir_follows_the_copy(#[case] relocate: bool) {
        let w = World {
            staged_files: vec![(p("nested/.git"), b"gitdir: /w/repo/nestedgit\n".to_vec())],
            ..World::default()
        };
        let mut req = new_branch();
        req.relocate = relocate;
        run(&w, &req).unwrap();
        let gitfile = p(&format!("{STAGED_WT}/nested/.git"));
        if relocate {
            assert_eq!(w.files.borrow()[&gitfile], b"gitdir: /w/feat/nestedgit\n");
        } else {
            assert_eq!(w.count(|c| *c == Call::RemoveFile(gitfile.clone())), 1);
        }
    }

    // U-TXN-78: without a branch flag, git derives a branch name from the
    // destination's basename and passes it to `git branch`: a name starting with
    // `-` would be read as an option there.
    #[test]
    fn refuses_option_like_destination_name_for_the_implicit_branch() {
        let w = World::default();
        let mut req = request(BranchMode::Default);
        req.destination = p("/w/-m");
        assert_eq!(run(&w, &req).unwrap_err().code, ErrorCode::Usage);
        assert_no_mutation(&w);
        // With an explicit branch mode the basename is only a path: fine.
        let w = World::default();
        let mut req = new_branch();
        req.destination = p("/w/-m");
        run(&w, &req).unwrap();
    }

    // U-TXN-79: a branch someone else created in the meantime (git refuses with
    // "already exists") is not this run's, even at the same commit.
    #[test]
    fn branch_created_by_someone_else_meanwhile_is_kept() {
        let w = World::default().failing(Step::WorktreeAdd, ErrorCode::GitFailed);
        w.fail_message
            .replace("fatal: a branch named 'feat' already exists".into());
        w.fail_after_branch.set(true);
        let _ = run(&w, &new_branch());
        assert_eq!(
            w.count(|c| matches!(c, Call::BranchDelete(_))),
            0,
            "{:#?}",
            w.calls()
        );
    }

    // ---------------------------------------------------------------- --copy

    fn copy_request() -> AddRequest {
        AddRequest {
            copy: true,
            ..new_branch()
        }
    }

    fn volume(fs_type: &str, cow: bool, device: u64) -> VolumeInfo {
        VolumeInfo {
            fs_type: fs_type.into(),
            cow,
            device,
        }
    }

    fn copies(w: &World) -> Vec<Call> {
        w.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Copy { .. }))
            .collect()
    }

    // U-TXN-80: `--copy` has no copy-on-write requirement: a source on ext4 is fine.
    #[test]
    fn copy_needs_no_cow_volume() {
        let w = World {
            volumes: vec![(p("/"), volume("ext4", false, 1))],
            ..World::default()
        };
        let out = run(&w, &copy_request()).unwrap();
        assert_eq!(
            out,
            AddOutcome {
                destination: p(DEST),
                head: SHA_A.into(),
                strategy: Strategy::Copy,
                warnings: vec![],
                relocated: 0,
            }
        );
        assert_eq!(w.count(|c| matches!(c, Call::Rename { .. })), 1);
    }

    // U-TXN-81: ... nor the same volume: only the staging dir (always next to the
    // destination) has to be on the destination's volume.
    #[test]
    fn copy_allows_cross_volume_and_mixed_filesystems() {
        let w = World {
            volumes: vec![
                (p("/"), volume("xfs", true, 1)),
                (p("/w/repo"), volume("ext4", false, 7)),
            ],
            ..World::default()
        };
        let out = run(&w, &copy_request()).unwrap();
        assert_eq!(out.strategy, Strategy::Copy);
        assert_eq!(
            w.count(|c| *c == Call::CreateStaging { parent: p("/w") }),
            1,
            "staging next to the destination"
        );
    }

    // U-TXN-82: `--copy` means copy, even where cloning would work.
    #[test]
    fn copy_copies_where_cloning_would_work() {
        let w = World::default();
        let out = run(&w, &copy_request()).unwrap();
        assert_eq!(out.strategy, Strategy::Copy);
        assert_eq!(w.count(|c| matches!(c, Call::Clone { .. })), 0);
        assert_eq!(
            copies(&w),
            vec![
                Call::Copy {
                    src: p("/w/repo/node_modules"),
                    dst: p(&format!("{STAGED_WT}/node_modules"))
                },
                Call::Copy {
                    src: p("/w/repo/src"),
                    dst: p(&format!("{STAGED_WT}/src"))
                },
            ]
        );
    }

    /// Worlds that exercise the plan: exclusions, relocation, nested layouts.
    fn rich_world(scenario: &str) -> (World, AddRequest) {
        let mut w = world_with_references();
        let mut req = new_branch();
        match scenario {
            "nested-worktree" => {
                w.worktrees.push(WorktreeEntry {
                    path: p("/w/repo/wt/other"),
                    head: Some(SHA_A.into()),
                    branch: Some("refs/heads/other".into()),
                    ..WorktreeEntry::default()
                });
                w.tree
                    .borrow_mut()
                    .get_mut(&p("/w/repo"))
                    .unwrap()
                    .push(OsString::from("wt"));
                w.tree.borrow_mut().insert(
                    p("/w/repo/wt"),
                    vec![OsString::from("other"), OsString::from("notes")],
                );
            }
            "nested-destination" => {
                w.tree
                    .borrow_mut()
                    .get_mut(&p("/w/repo"))
                    .unwrap()
                    .push(OsString::from(".worktrees"));
                w.tree.borrow_mut().insert(p("/w/repo/.worktrees"), vec![]);
                req.destination = p("/w/repo/.worktrees/feat");
            }
            "parhelignore" => w.parhelignored = vec![p("node_modules")],
            "submodule-reinit" => {
                w.submodules = vec![p("src")];
                req.submodules = SubmodulePolicy::Reinit;
            }
            "symlink-ancestor" => {
                w.worktrees.push(WorktreeEntry {
                    path: p("/w/repo/lnk/wt"),
                    ..WorktreeEntry::default()
                });
                w.tree
                    .borrow_mut()
                    .get_mut(&p("/w/repo"))
                    .unwrap()
                    .push(OsString::from("lnk"));
                w.tree.borrow_mut().insert(
                    p("/w/repo/lnk"),
                    vec![OsString::from("wt"), OsString::from("f")],
                );
                w.symlinks.borrow_mut().insert(p("/w/repo/lnk"));
            }
            "no-relocate" => req.relocate = false,
            "plain" => {}
            other => panic!("unknown scenario {other}"),
        }
        (w, req)
    }

    // U-TXN-83: `--copy` is the same transaction with a different leaf operation:
    // the very same calls in the very same order (same exclusions, same journal,
    // same relocation, same publication) with every `Clone` replaced by a `Copy`.
    #[rstest]
    #[case::plain("plain")]
    #[case::nested_worktree("nested-worktree")]
    #[case::nested_destination("nested-destination")]
    #[case::parhelignore("parhelignore")]
    #[case::submodule_reinit("submodule-reinit")]
    #[case::symlink_ancestor("symlink-ancestor")]
    #[case::no_relocate("no-relocate")]
    fn copy_runs_the_clone_transaction_with_copies(#[case] scenario: &str) {
        let (cloning, req) = rich_world(scenario);
        let cloned = run(&cloning, &req).unwrap();
        let (copying, req) = rich_world(scenario);
        let copied = run(&copying, &AddRequest { copy: true, ..req }).unwrap();

        let as_copies: Vec<Call> = cloning
            .calls()
            .into_iter()
            .map(|c| match c {
                Call::Clone { src, dst } => Call::Copy { src, dst },
                other => other,
            })
            .collect();
        assert!(
            as_copies.iter().any(|c| matches!(c, Call::Copy { .. })),
            "scenario copies something"
        );
        assert_eq!(copying.calls(), as_copies);
        assert_eq!(
            copying.files.borrow().clone(),
            cloning.files.borrow().clone(),
            "same relocation"
        );
        assert_eq!(copied.relocated, cloned.relocated);
        assert_eq!(copied.warnings, cloned.warnings);
        assert_eq!(copied.strategy, Strategy::Copy);
        assert_eq!(cloned.strategy, Strategy::CopyOnWrite);
    }

    // U-TXN-84: a failure at each pre-publish step rolls back exactly as with clones.
    #[rstest]
    #[case::worktree_add(Step::WorktreeAdd, ErrorCode::GitFailed, None)]
    #[case::copy(Step::Clone, ErrorCode::CloneFailed, Some("feat"))]
    #[case::reset_index(Step::ResetIndex, ErrorCode::GitFailed, Some("feat"))]
    #[case::rename_io(Step::Rename, ErrorCode::Io, Some("feat"))]
    fn copy_rolls_back_on_failure(
        #[case] step: Step,
        #[case] code: ErrorCode,
        #[case] branch: Option<&str>,
    ) {
        let w = World::default().failing(step, code);
        assert_eq!(
            run(&w, &copy_request()).unwrap_err().code,
            code,
            "original error surfaces"
        );
        if step != Step::WorktreeAdd {
            assert!(!copies(&w).is_empty(), "the files are copied, not cloned");
            assert_eq!(w.count(|c| matches!(c, Call::Clone { .. })), 0);
        }
        assert_rolled_back(&w, branch);
    }

    // U-TXN-85: the failure that matters: the copy itself fails midway (disk full,
    // unreadable file). The error is the copy's, the copy attempted is not a clone,
    // and nothing outlives the run.
    #[test]
    fn copy_failing_midway_rolls_back_completely() {
        let w = World::default().failing(Step::Clone, ErrorCode::CloneFailed);
        let err = run(&w, &copy_request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        assert_eq!(copies(&w).len(), 1, "stopped at the first failing copy");
        assert_eq!(w.count(|c| matches!(c, Call::Clone { .. })), 0);
        assert_eq!(w.count(|c| matches!(c, Call::Rename { .. })), 0);
        assert_rolled_back(&w, Some("feat"));
    }

    // U-TXN-86: SIGINT during a long copy -> interrupted + full rollback.
    #[test]
    fn copy_interrupt_rolls_back() {
        let w = World::default();
        w.interrupt_after.set(Some(Step::Clone));
        assert_eq!(
            run(&w, &copy_request()).unwrap_err().code,
            ErrorCode::Interrupted
        );
        assert_eq!(copies(&w).len(), 1, "interrupted after the first copy");
        assert_rolled_back(&w, Some("feat"));
        assert_eq!(w.count(|c| matches!(c, Call::Rename { .. })), 0);
    }

    // U-TXN-87: the two escape hatches are alternatives, whoever builds the request.
    #[test]
    fn copy_with_fallback_is_a_usage_error_before_any_mutation() {
        let w = World::default();
        let mut req = copy_request();
        req.fallback = true;
        let err = run(&w, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage);
        assert!(
            err.message.contains("--copy") && err.message.contains("--fallback"),
            "{}",
            err.message
        );
        assert_no_mutation(&w);
    }

    // U-TXN-88: `--copy` does not weaken any other refusal.
    #[rstest]
    #[case::dirty(World { dirty: true, ..World::default() }, ErrorCode::DirtySource)]
    #[case::sparse(World { sparse: true, ..World::default() }, ErrorCode::SparseCheckout)]
    #[case::submodules(
        World { submodules: vec![p("libs/a")], ..World::default() },
        ErrorCode::SubmodulesPresent
    )]
    fn copy_does_not_bypass_safety_refusals(#[case] w: World, #[case] code: ErrorCode) {
        assert_eq!(run(&w, &copy_request()).unwrap_err().code, code);
        assert_no_mutation(&w);
    }

    // U-TXN-89: refusing for lack of copy-on-write offers both ways out, and says
    // what each costs. Platform-neutral wording: no mention of APFS.
    #[rstest]
    #[case::source_not_cow(
        vec![(p("/"), apfs(1)), (p("/w/repo"), volume("ext4", false, 1))],
        ErrorCode::NotCow
    )]
    #[case::destination_not_cow(
        vec![(p("/"), volume("ext4", false, 2)), (p("/w/repo"), apfs(1))],
        ErrorCode::NotCow
    )]
    #[case::cross_volume(vec![(p("/"), apfs(1)), (p("/w/repo"), apfs(7))], ErrorCode::CrossVolume)]
    fn refusal_offers_copy_and_fallback(
        #[case] volumes: Vec<(PathBuf, VolumeInfo)>,
        #[case] code: ErrorCode,
    ) {
        let w = World {
            volumes,
            ..World::default()
        };
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, code);
        assert!(err.message.contains("--copy"), "{}", err.message);
        assert!(err.message.contains("--fallback"), "{}", err.message);
        assert!(!err.message.contains("APFS"), "{}", err.message);
        assert_no_mutation(&w);
    }

    // U-TXN-91: a source directory that is an ancestor of an exclusion is recreated
    // empty; it takes the source's mode only after everything inside it is in place
    // (a read-only source directory could not be filled otherwise), deepest first,
    // for clones and copies alike.
    #[rstest::rstest]
    fn created_directories_take_their_mode_after_their_contents(#[values(false, true)] copy: bool) {
        let mut w = World::default();
        for other in ["/w/repo/wt/other", "/w/repo/wt/deep/other"] {
            w.worktrees.push(WorktreeEntry {
                path: p(other),
                ..WorktreeEntry::default()
            });
        }
        w.tree
            .borrow_mut()
            .get_mut(&p("/w/repo"))
            .unwrap()
            .push(OsString::from("wt"));
        w.tree.borrow_mut().insert(
            p("/w/repo/wt"),
            ["deep", "notes", "other"].map(OsString::from).to_vec(),
        );
        w.tree.borrow_mut().insert(
            p("/w/repo/wt/deep"),
            ["data", "other"].map(OsString::from).to_vec(),
        );
        let mut req = new_branch();
        req.copy = copy;
        run(&w, &req).unwrap();

        let calls = w.calls();
        let mode_at = |to: &str| {
            calls
                .iter()
                .position(|c| matches!(c, Call::CopyMode { to: t, .. } if *t == p(to)))
                .unwrap_or_else(|| panic!("no mode for {to}: {calls:#?}"))
        };
        let (deep, wt) = (
            mode_at(&format!("{STAGED_WT}/wt/deep")),
            mode_at(&format!("{STAGED_WT}/wt")),
        );
        let root = mode_at(STAGED_WT);
        assert!(deep < wt && wt < root, "deepest first: {calls:#?}");
        // Every entry created or filled below a directory precedes its mode.
        for (i, call) in calls.iter().enumerate() {
            let below = |dir: &str| match call {
                Call::Clone { dst, .. } | Call::Copy { dst, .. } | Call::CreateDir(dst) => {
                    dst.starts_with(format!("{STAGED_WT}/{dir}"))
                }
                _ => false,
            };
            assert!(
                !below("wt/deep") || i < deep,
                "{call:?} after the mode of deep"
            );
            assert!(!below("wt") || i < wt, "{call:?} after the mode of wt");
        }
    }

    // U-TXN-90: without --copy nothing changes: still clones, still reports
    // copy-on-write.
    #[test]
    fn without_copy_flag_nothing_is_copied() {
        let w = World::default();
        let out = run(&w, &new_branch()).unwrap();
        assert_eq!(out.strategy, Strategy::CopyOnWrite);
        assert_eq!(w.count(|c| matches!(c, Call::Copy { .. })), 0);
    }
}
