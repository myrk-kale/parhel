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
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::app::rollback::{JOURNAL_FILE, Journal, StoreLink, rollback};
use crate::app::store::{discover, first_sync_notice, first_sync_size, sync_baseline};
use crate::domain::clone_plan::{self, CloneOp, DirLister};
use crate::domain::exclusions::{ExclusionInput, Exclusions};
use crate::domain::paths::{self, Placement};
use crate::domain::relocate::{MAX_RELOCATE_BYTES, Relocation, Rewrite};
use crate::domain::store::{
    BASELINES_DIR, LINKS_FILE, MARKER_FILE, Marker, Store, StoreState, WORKTREES_DIR, last_sync_of,
    render_meta, repo_id,
};
use crate::domain::{AddRequest, BranchMode, Error, ErrorCode, Result, SubmodulePolicy};
use crate::git::parse::ensure_supported;
use crate::ports::{CowFs, FileLock, Git, RepoInfo, WorktreeAddSpec};

/// Side-effect providers and process context.
#[derive(Clone, Copy)]
pub struct Env<'a> {
    pub git: &'a dyn Git,
    pub fs: &'a dyn CowFs,
    /// Directory relative paths resolve against.
    pub cwd: &'a Path,
    /// Set by the SIGINT handler; checked between steps.
    pub interrupted: &'a AtomicBool,
    /// Where the user's store lives (`PARHEL_STORE`, else `/parhel/<user>` on Linux);
    /// `None` where stores do not exist (macOS), so nothing is looked up or suggested.
    pub store: Option<&'a Path>,
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
    /// A reflink clone of the store's baseline; the destination is a symlink to it.
    Store,
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
    /// Store runs: where the worktree really lives (`destination` is a symlink to it).
    pub real_path: Option<PathBuf>,
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
            if request.fallback {
                return fallback(env, request, &plan, &problem);
            }
            // The only way out of the problem that keeps the worktree copy-on-write.
            let store = usable_store(env, &plan, problem)?;
            crate::failpoint!("after_validate");
            return store_transaction(env, request, &plan, &store);
        }
    }
    crate::failpoint!("after_validate");
    transaction(env, request, &plan, None)
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
    Ok(if source.filesystem == dest.filesystem {
        // One filesystem: the destination's parent, where the run creates its staging
        // directory, is writable, so what the adapter learned there is the answer. The
        // source directory may well be read-only, and then it can only guess.
        (!dest.cow).then(|| not_cow(&plan.parent, &dest.fs_type))
    } else if !source.cow {
        Some(not_cow(&plan.repo.toplevel, &source.fs_type))
    } else if !dest.cow {
        Some(not_cow(&plan.parent, &dest.fs_type))
    } else {
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
    })
}

/// What stands between a refusal (`problem`) and the store: the store of the user when it
/// can serve this run, else the refusal itself, with the way to get a store when there
/// is none, or `store-unavailable` when there is one that cannot be used (never a silent
/// fallback to something that is not copy-on-write).
fn usable_store(env: &Env<'_>, plan: &Plan, problem: Error) -> Result<Store> {
    let Some(path) = env.store else {
        return Err(problem);
    };
    match discover(env.fs, path)? {
        StoreState::NotConfigured => Err(Error::new(
            problem.code,
            format!(
                "{}; or run `sudo git parhel setup` once to create a copy-on-write store that \
                 `git parhel add` then uses on its own",
                problem.message
            ),
        )),
        StoreState::Unavailable { root, reason } => {
            let unit = env
                .fs
                .read_file(&root.join(MARKER_FILE))
                .ok()
                .and_then(|bytes| Marker::parse(&String::from_utf8_lossy(&bytes)).ok())
                .map(|marker| format!(" `{}`", marker.unit));
            Err(Error::new(
                ErrorCode::StoreUnavailable,
                format!(
                    "the store at `{}` cannot be used: {reason}. Run `git parhel doctor` to see \
                     what is wrong; `systemctl status`{} shows whether its mount is up. \
                     --copy and --fallback work without the store",
                    root.display(),
                    unit.map_or_else(
                        || " of its mount unit".to_owned(),
                        |unit| format!(" of the mount unit{unit}")
                    )
                ),
            ))
        }
        StoreState::Available(store) => {
            // A store on the source's own filesystem would not help: the normal answer.
            let source = env.fs.volume(&plan.repo.toplevel)?;
            let held = env.fs.volume(&store.root)?;
            if held.filesystem == source.filesystem {
                Err(problem)
            } else {
                // Git registers canonical paths: so does the store route.
                Ok(Store {
                    root: env.fs.canonicalize(&store.root)?,
                    ..store
                })
            }
        }
    }
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
        real_path: None,
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

/// The store route: mirror the source into the store's baseline, clone the worktree
/// from the baseline (a reflink inside the store, so it costs almost nothing) and leave
/// a symlink at the requested destination.
///
/// Everything up to the sync only reads (and the sync only writes the baseline, a
/// cache that any later run repairs). The baseline lock is held from the sync to the
/// end of the clone, so no clone sees a half-synced baseline, and released before the
/// worktree is published.
fn store_transaction(
    env: &Env<'_>,
    request: &AddRequest,
    plan: &Plan,
    store: &Store,
) -> Result<AddOutcome> {
    let (fs, repo) = (env.fs, &plan.repo);
    let id = repo_id(&repo.toplevel);
    let baseline_dir = store.root.join(BASELINES_DIR).join(&id);
    let worktrees_dir = store.root.join(WORKTREES_DIR);
    let real_dir = worktrees_dir.join(&id);
    let name = paths::destination_basename(&plan.destination)?;
    refuse_registry_hostile(&plan.destination, &real_dir.join(name))?;

    let exclusions = Exclusions::compute(&ExclusionInput {
        source_root: repo.toplevel.clone(),
        staging_dir: None,
        other_worktrees: env
            .git
            .worktrees(&repo.toplevel)?
            .into_iter()
            .map(|w| w.path)
            .collect(),
        submodule_paths: plan.submodules.clone(),
        exclude_submodules: request.submodules == SubmodulePolicy::Reinit,
        user_excluded: [
            env.git.parhelignored(&repo.toplevel)?,
            links_inside(env, store, &repo.toplevel),
        ]
        .concat(),
    });

    for dir in [
        &store.root.join(BASELINES_DIR),
        &baseline_dir,
        &worktrees_dir,
    ] {
        ensure_dir(fs, dir)?;
    }
    let baseline_lock = lock_baseline(env, &baseline_dir.join("lock"))?;
    let baseline = baseline_dir.join("tree");
    let meta_path = baseline_dir.join("meta");
    // A sync that died without cleaning up (power loss, SIGKILL) may have left files
    // whose metadata is right and whose data never reached the disk, and the sync only
    // compares metadata: that baseline is not trusted, it is made again.
    let unfinished = baseline_unfinished(fs, &meta_path);
    if unfinished {
        fs.remove_dir_all(&baseline)?;
    }
    // The sync compares size, mtime and mode, which a program can put back after
    // rewriting a file; what it cannot put back is the status-change time, so entries
    // changed since the last finished sync began are copied whatever those say. With no
    // such sync on record (a first run, a `meta` that lost the field, a baseline that was
    // just discarded) nothing is trusted.
    let last_sync = if unfinished {
        None
    } else {
        fs.read_file(&meta_path)
            .ok()
            .and_then(|meta| last_sync_of(&meta))
    };
    // Names the repository the baseline mirrors, for `doctor`, before the first write
    // into it, and says that the baseline is being written (a baseline found in that
    // state is discarded, so the moment of the last finished sync is of no use to it).
    let write_meta = |state: &str, last_sync: Option<i128>| {
        let synced = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        fs.write_file(
            &meta_path,
            render_meta(&repo.toplevel, synced, state, last_sync).as_bytes(),
        )
    };
    // Taken before the first entry is looked at: whatever is changed after this is
    // changed after the sync began.
    let started = unix_nanos();
    write_meta("syncing", None)?;
    if let Some(bytes) = first_sync_size(env.fs, &repo.toplevel, &baseline, &exclusions)? {
        if let Some(notice) = first_sync_notice(bytes) {
            eprintln!("parhel: {notice}");
        }
    }
    let synced = sync_baseline(
        env.fs,
        &repo.toplevel,
        &baseline,
        &exclusions,
        env.interrupted,
        last_sync,
    );
    // Whatever the sync did is on disk (or reported as failed: a full disk can show only
    // here) before the baseline is called finished. A stopped sync is finished too: it
    // leaves whole files only, and the next one completes it - against the sync before
    // it, because this one did not look at everything.
    let flushed = fs.flush(&baseline_dir);
    if flushed.is_ok() {
        write_meta(
            "synced",
            if synced.is_ok() {
                Some(started)
            } else {
                last_sync
            },
        )?;
    }
    synced?;
    flushed?;
    crate::failpoint!("after_sync");
    check_interrupt(env)?;

    let real = free_name(fs, &real_dir, name)?;
    let route = StoreRun {
        root: store.root.clone(),
        baseline,
        worktrees_dir,
        real,
        links: store.root.join(LINKS_FILE),
        exclusions,
    };
    transaction(env, request, plan, Some((&route, baseline_lock)))
}

/// Now, in nanoseconds since the epoch (the unit of a status-change time).
fn unix_nanos() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX))
}

/// Whether the baseline's `meta` says that a sync began and never reported its end.
fn baseline_unfinished(fs: &dyn CowFs, meta_path: &Path) -> bool {
    fs.read_file(meta_path).is_ok_and(|meta| {
        meta.split(|&b| b == b'\n')
            .any(|line| line == b"state=syncing")
    })
}

/// The registry of links is `path TAB target NEWLINE`: refuse (before anything is
/// created) the paths it could not hold.
fn refuse_registry_hostile(destination: &Path, real: &Path) -> Result<()> {
    for path in [destination, real] {
        if path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|&b| b == b'\t' || b == b'\n')
        {
            return Err(Error::new(
                ErrorCode::Usage,
                format!(
                    "`{}` contains a tab or a newline, which the store's registry of links \
                     cannot hold; choose another destination",
                    path.display().to_string().escape_debug()
                ),
            ));
        }
    }
    Ok(())
}

/// Symlinks the store's registry knows that live inside `source` and still point where
/// they were created, as paths relative to it: a worktree requested inside the source
/// leaves its link there, and the baseline (so every later worktree) must not mirror it.
fn links_inside(env: &Env<'_>, store: &Store, source: &Path) -> Vec<PathBuf> {
    let Ok(registry) = env.fs.read_file(&store.root.join(LINKS_FILE)) else {
        return Vec::new();
    };
    let mut inside = Vec::new();
    for line in registry.split(|&b| b == b'\n') {
        let Some(tab) = line.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let link = Path::new(std::ffi::OsStr::from_bytes(&line[..tab]));
        let target = Path::new(std::ffi::OsStr::from_bytes(&line[tab + 1..]));
        if let Ok(rel) = link.strip_prefix(source) {
            let ours = matches!(env.fs.read_link(link), Ok(Some(t)) if t == target);
            if ours && !rel.as_os_str().is_empty() && !inside.iter().any(|p: &PathBuf| p == rel) {
                inside.push(rel.to_path_buf());
            }
        }
    }
    inside
}

/// The directory `dir`, created if missing. Another run creating it at the same moment
/// (two first runs for one repository) is not a failure.
fn ensure_dir(fs: &dyn CowFs, dir: &Path) -> Result<()> {
    if fs.is_dir(dir)? {
        return Ok(());
    }
    fs.create_dir(dir)
        .or_else(|e| if fs.is_dir(dir)? { Ok(()) } else { Err(e) })
}

/// `dir/name`, or the first of `dir/name-2`, `dir/name-3`... that is free.
fn free_name(fs: &dyn CowFs, dir: &Path, name: &std::ffi::OsStr) -> Result<PathBuf> {
    let mut candidate = dir.join(name);
    let mut n = 2u32;
    while fs.exists_no_follow(&candidate)? {
        let mut suffixed = name.to_os_string();
        suffixed.push(format!("-{n}"));
        candidate = dir.join(suffixed);
        n += 1;
    }
    Ok(candidate)
}

/// The exclusive lock of a baseline. Another run of the same repository holds it while
/// it syncs and clones: wait for it (a minute of waiting is one sync of a big tree) and
/// say so, instead of failing a second `add` that is perfectly legitimate.
fn lock_baseline(env: &Env<'_>, path: &Path) -> Result<FileLock> {
    let mut announced = false;
    loop {
        check_interrupt(env)?;
        match env.fs.lock_exclusive(path) {
            Ok(lock) => return Ok(lock),
            Err(e) => {
                if !env.fs.is_locked(path).unwrap_or(false) {
                    return Err(e);
                }
                if !announced {
                    eprintln!(
                        "parhel: waiting for another run on this repository to finish with the \
                         store"
                    );
                    announced = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// What a store run adds to the transaction: where the worktree is cloned from and where
/// it ends up.
struct StoreRun {
    /// The store's root, named in the lock reason of the worktree.
    root: PathBuf,
    /// `<store>/baselines/<id>/tree`: the clone source instead of the live source.
    baseline: PathBuf,
    /// `<store>/worktrees`: where the staging directory lives.
    worktrees_dir: PathBuf,
    /// The real path of the worktree, `<store>/worktrees/<id>/<name>`.
    real: PathBuf,
    /// `<store>/links`: the registry of the symlinks created.
    links: PathBuf,
    /// What neither the baseline nor the clone contain (the same set for both).
    exclusions: Exclusions,
}

impl StoreRun {
    /// Where the finished worktree is: its real path.
    fn final_path<'a>(route: Option<&'a Self>, plan: &'a Plan) -> &'a Path {
        route.map_or(plan.destination.as_path(), |r| r.real.as_path())
    }
}

fn transaction(
    env: &Env<'_>,
    request: &AddRequest,
    plan: &Plan,
    route: Option<(&StoreRun, FileLock)>,
) -> Result<AddOutcome> {
    let (route, mut baseline_lock) = match route {
        Some((route, lock)) => (Some(route), Some(lock)),
        None => (None, None),
    };
    let staging_parent = route.map_or(plan.parent.as_path(), |r| r.worktrees_dir.as_path());
    let staging = env.fs.create_staging_dir(staging_parent)?;
    let mut journal = Journal {
        repo: plan.repo.toplevel.clone(),
        ..Journal::default()
    };
    let mut lock = None;
    // A panic (bug) must not skip the rollback: catch it and undo like any error.
    let assembled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assemble(
            env,
            request,
            plan,
            route,
            &staging,
            &mut journal,
            &mut lock,
            &mut baseline_lock,
        )
    }))
    .unwrap_or_else(|_| {
        Err(Error::new(
            ErrorCode::Io,
            "internal error (panic); rolled back",
        ))
    });
    match assembled {
        Ok(report) => {
            // Only the clone needs the baseline to hold still.
            drop(baseline_lock);
            publish(env, request, plan, route, &staging, &report)
        }
        Err(cause) => {
            #[cfg(feature = "failpoints")]
            let _ = crate::failpoint::eval(crate::failpoint::ROLLBACK_FAILPOINT);
            let undone = rollback(env.git, env.fs, &staging, &journal);
            drop(lock);
            drop(baseline_lock);
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
#[allow(clippy::too_many_arguments)]
fn assemble(
    env: &Env<'_>,
    request: &AddRequest,
    plan: &Plan,
    route: Option<&StoreRun>,
    staging: &Path,
    journal: &mut Journal,
    lock: &mut Option<FileLock>,
    baseline_lock: &mut Option<FileLock>,
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

    let exclusions = match route {
        Some(route) => route.exclusions.clone(),
        None => Exclusions::compute(&ExclusionInput {
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
        }),
    };
    // A store run clones the baseline, which the sync just made equal to the source.
    let cloned_from = route.map_or(repo.toplevel.as_path(), |r| r.baseline.as_path());
    clone_tree(env, cloned_from, &staged, &exclusions, request.copy)?;
    drop(baseline_lock.take());
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

    // Paths written into the worktree (links, reinitialized submodules, rewritten
    // references) name where it will really be: its real path in the store, which is
    // valid however the user reaches it.
    let final_path = StoreRun::final_path(route, plan);
    if request.submodules == SubmodulePolicy::Reinit && !plan.submodules.is_empty() {
        git.submodules_reinit(&staged, &repo.toplevel, final_path)?;
        check_interrupt(env)?;
    }
    let report = relocate_tree(
        env,
        &staged,
        &repo.toplevel,
        &exclusions,
        request
            .relocate
            .then(|| Relocation::new(&repo.toplevel, final_path).excluding(exclusions.iter())),
    )?;
    check_interrupt(env)?;
    crate::failpoint!("before_rename");
    check_interrupt(env)?;
    // Record where the worktree is going *before* moving it: if the process dies right
    // after the rename, `doctor --fix` must repair the destination, not undo it.
    journal.publish_to = Some(final_path.to_path_buf());
    if let Some(route) = route {
        claim_link(env, plan, route, journal, &journal_path)?;
    } else {
        fs.write_file(&journal_path, &journal.encode())?;
    }
    fs.rename_exclusive(&staged, final_path)?;
    Ok(report)
}

/// Store runs, right before the publishing rename: the real directory exists, and the
/// requested path becomes a symlink to the real worktree. The link and its target are
/// journaled (so a crash is undone, and only this symlink is) and registered in the
/// store (so `doctor` can find it when the worktree is removed) *before* the symlink is
/// created; `symlink` fails on anything that exists, which is what makes the claim safe.
fn claim_link(
    env: &Env<'_>,
    plan: &Plan,
    route: &StoreRun,
    journal: &mut Journal,
    journal_path: &Path,
) -> Result<()> {
    let fs = env.fs;
    if let Some(dir) = route.real.parent() {
        ensure_dir(fs, dir)?;
    }
    journal.store_link = Some(StoreLink {
        path: plan.destination.clone(),
        target: route.real.clone(),
    });
    fs.write_file(journal_path, &journal.encode())?;
    let mut line = plan.destination.as_os_str().as_encoded_bytes().to_vec();
    line.push(b'\t');
    line.extend_from_slice(route.real.as_os_str().as_encoded_bytes());
    line.push(b'\n');
    fs.append_file(&route.links, &line)?;
    if let Err(e) = fs.symlink_exclusive(&route.real, &plan.destination) {
        // Nothing of ours is there. What is may be another run's link to this very
        // target (the same destination and name, raced): it is not ours to take back.
        journal.store_link = None;
        return Err(e);
    }
    crate::failpoint!("after_link");
    check_interrupt(env)
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
    let mut created = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        match op {
            CloneOp::CreateDir(rel) => {
                fs.create_dir(&staged.join(rel))?;
                created.push(rel);
            }
            CloneOp::Clone(rel) if copy => fs.copy_within(source, rel, &staged.join(rel))?,
            CloneOp::Clone(rel) => fs.clone_within(source, rel, &staged.join(rel))?,
        }
        if i == 0 {
            crate::failpoint!("mid_clone");
        }
        check_interrupt(env)?;
    }
    // The source's modes come last, deepest directory first: a read-only directory
    // could not be filled, and the plan lists parents before children.
    for rel in created.into_iter().rev() {
        fs.copy_mode(&source.join(rel), &staged.join(rel))?;
    }
    // The root (created by `git worktree add` with default permissions) takes the
    // source's once the worktree is published: see `publish`.
    Ok(())
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
    route: Option<&StoreRun>,
    staging: &Path,
    report: &RelocationReport,
) -> Result<AddOutcome> {
    crate::failpoint!("after_rename");
    let mut warnings = Vec::new();
    // Where the worktree really is: the destination, or its real path in the store.
    let at = StoreRun::final_path(route, plan);
    let staged = paths::staging_worktree_path(staging, &plan.destination);
    env.git
        .repoint_worktree(&plan.repo.toplevel, &staged, at)
        .map_err(|e| {
            Error::new(
                e.code,
                format!(
                    "{}; the worktree exists at `{}` but its registration is stale: run \
                 `git worktree repair {}`",
                    e.message,
                    at.display(),
                    at.display()
                ),
            )
        })?;
    // A worktree in the store has its registration in the repository's `.git` and its
    // files on the store's image: with the mount down, `git worktree prune` (which `git
    // gc` runs) sees a registration without a directory and drops it. A locked one is
    // skipped. The worktree is valid either way, so a lock that fails is a warning.
    if let Some(route) = route {
        if let Err(e) = env.git.worktree_lock(
            &plan.repo.toplevel,
            at,
            &format!("parhel store: {}", route.root.display()),
        ) {
            warnings.push(format!(
                "could not lock `{}` against `git worktree prune`, which would forget it \
                 while the store is not mounted: {}. Run `git worktree lock {}`",
                at.display(),
                e.message,
                at.display()
            ));
        }
    }
    // Last, because the root may be read-only: it could not be renamed (Linux wants
    // write access to a directory moved into another one) nor repaired before.
    if let Err(e) = env.fs.copy_mode(&plan.repo.toplevel, at) {
        warnings.push(format!(
            "could not give `{}` the permissions of the source's root: {}",
            at.display(),
            e.message
        ));
    }
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
        head: env.git.head(at)?,
        strategy: match (route, request.copy) {
            (Some(_), _) => Strategy::Store,
            (None, true) => Strategy::Copy,
            (None, false) => Strategy::CopyOnWrite,
        },
        warnings,
        relocated: report.rewritten + report.relinked,
        real_path: route.map(|r| r.real.clone()),
    })
}

/// [`DirLister`] over the real source tree through the [`CowFs`] port.
struct Lister<'a> {
    fs: &'a dyn CowFs,
    root: &'a Path,
}

impl DirLister for Lister<'_> {
    fn list(&self, relative: &Path) -> io::Result<Vec<OsString>> {
        // No-follow: a symlink is cloned as a link, never descended into.
        match self.fs.list_dir_within(self.root, relative) {
            Ok(Some(names)) => Ok(names),
            Ok(None) => Err(io::Error::from(io::ErrorKind::NotADirectory)),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{Call, FAKE_STAGING, SHA_A, SHA_B, Step, World, apfs, btrfs, ext4, p};
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
            store: None,
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
                real_path: None,
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
                        filesystem: 2,
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
                        device: 2,
                        filesystem: 2,
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
                        filesystem: 2,
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
                    filesystem: 1,
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
                    filesystem: 1,
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
    // paths) get the source's permission bits, never the defaults. (The root's, after
    // publishing: U-TXN-94.)
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
            ("/w/repo", DEST.to_owned()),
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
        let mode = w
            .position(
                |c| matches!(c, Call::CopyMode { to, .. } if *to == p(&format!("{STAGED_WT}/wt"))),
            )
            .unwrap();
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
            filesystem: device,
        }
    }

    /// Like [`volume`], on a filesystem of its own naming (a btrfs subvolume: another
    /// device, the same filesystem).
    fn volume_on(fs_type: &str, cow: bool, device: u64, filesystem: u64) -> VolumeInfo {
        VolumeInfo {
            filesystem,
            ..volume(fs_type, cow, device)
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
                real_path: None,
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
        vec![(p("/"), apfs(1)), (p("/w/repo"), volume("ext4", false, 2))],
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

    // U-TXN-92: on one filesystem the destination's probe decides. A source directory
    // the probe could not run in (read-only) is not a reason to refuse.
    #[test]
    fn same_filesystem_is_judged_by_the_destination() {
        let w = World {
            volumes: vec![(p("/"), apfs(1)), (p("/w/repo"), volume("xfs", false, 1))],
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap().strategy,
            Strategy::CopyOnWrite
        );
    }

    // ... and the destination's refusal names the destination.
    #[test]
    fn same_filesystem_without_cow_names_the_destination() {
        let w = World {
            volumes: vec![(p("/"), volume("ext4", false, 1))],
            ..World::default()
        };
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotCow);
        assert!(err.message.contains("`/w`"), "{}", err.message);
    }

    // U-TXN-93: two devices of one filesystem (btrfs subvolumes) can share storage.
    #[test]
    fn subvolumes_of_one_filesystem_are_not_cross_volume() {
        let w = World {
            volumes: vec![
                (p("/"), volume_on("btrfs", true, 5, 100)),
                (p("/w/repo"), volume_on("btrfs", true, 6, 100)),
            ],
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap().strategy,
            Strategy::CopyOnWrite
        );
    }

    // ... while another filesystem is, whatever the devices say.
    #[test]
    fn distinct_filesystems_are_cross_volume_even_on_one_device_number() {
        let w = World {
            volumes: vec![
                (p("/"), volume_on("btrfs", true, 5, 100)),
                (p("/w/repo"), volume_on("btrfs", true, 5, 200)),
            ],
            ..World::default()
        };
        assert_eq!(
            run(&w, &new_branch()).unwrap_err().code,
            ErrorCode::CrossVolume
        );
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
        assert!(deep < wt, "deepest first: {calls:#?}");
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

    // U-TXN-94: the worktree's root takes the source's mode once it is published and
    // repaired, not before: a read-only root cannot be moved (Linux wants write access
    // to a directory to rename it into another one) nor have its `.git` file rewritten.
    #[rstest::rstest]
    fn root_mode_is_applied_after_publishing(#[values(false, true)] copy: bool) {
        let w = World::default();
        let mut req = new_branch();
        req.copy = copy;
        run(&w, &req).unwrap();
        let calls = w.calls();
        let position = |wanted: &dyn Fn(&Call) -> bool| calls.iter().position(wanted);
        let rename = position(&|c| matches!(c, Call::Rename { .. })).unwrap();
        let repoint = position(&|c| matches!(c, Call::Repair { .. })).unwrap();
        let mode = position(&|c| matches!(c, Call::CopyMode { to, .. } if *to == p(DEST)))
            .unwrap_or_else(|| panic!("the destination root got no mode: {calls:#?}"));
        assert!(rename < repoint && repoint < mode, "{calls:#?}");
        assert!(
            position(&|c| matches!(c, Call::CopyMode { to, .. } if *to == p(STAGED_WT))).is_none(),
            "the staged root keeps its writable mode until then: {calls:#?}"
        );
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

    // ------------------------------------------------------------ the store

    const BASELINE_DIR: &str = "/store/baselines/7005185596c2e32bd0688ebe72725f92";
    const STORE_STAGING: &str = "/store/worktrees/.parhel-staging-test";
    const STORE_STAGED_WT: &str = "/store/worktrees/.parhel-staging-test/feat";

    /// Where the worktree called `name` really lives.
    fn real(name: &str) -> PathBuf {
        p(&format!(
            "/store/worktrees/7005185596c2e32bd0688ebe72725f92/{name}"
        ))
    }

    fn baseline() -> PathBuf {
        p(&format!("{BASELINE_DIR}/tree"))
    }

    fn store_world() -> World {
        World::default().with_store()
    }

    /// `add` with the store at `/store` configured.
    fn run_store(w: &World, req: &AddRequest) -> Result<AddOutcome> {
        run_with(w, req, Some(Path::new("/store")))
    }

    fn run_with(w: &World, req: &AddRequest, store: Option<&Path>) -> Result<AddOutcome> {
        let env = Env {
            git: w,
            fs: w,
            cwd: Path::new("/w/repo"),
            interrupted: &w.interrupted,
            store,
        };
        add(&env, req)
    }

    fn journal_path() -> PathBuf {
        p(&format!("{STORE_STAGING}/{JOURNAL_FILE}"))
    }

    // U-STO-20: when the store is taken and when the normal path (or an error)
    // applies. The store is for sources that cannot clone into the destination.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Route {
        Store,
        Clone,
        Copy,
        Fallback,
        Refuse(ErrorCode),
    }

    #[rstest]
    #[case::source_and_destination_on_ext4(
        vec![(p("/"), ext4(1)), (p("/store"), btrfs(2))], "../feat", false, false, Route::Store)]
    #[case::cow_source_and_destination_on_one_filesystem(
        vec![(p("/"), btrfs(3)), (p("/store"), btrfs(2))], "../feat", false, false, Route::Clone)]
    #[case::cow_source_destination_without_cow(
        vec![(p("/"), ext4(1)), (p("/w/repo"), btrfs(3)), (p("/store"), btrfs(2))],
        "../feat", false, false, Route::Store)]
    #[case::source_without_cow_destination_with_cow(
        vec![(p("/"), ext4(1)), (p("/d"), btrfs(3)), (p("/store"), btrfs(2))],
        "/d/feat", false, false, Route::Store)]
    #[case::two_cow_filesystems(
        vec![(p("/"), btrfs(3)), (p("/d"), btrfs(4)), (p("/store"), btrfs(2))],
        "/d/feat", false, false, Route::Store)]
    // The store on the source's own filesystem would not help: the normal answer.
    #[case::store_on_the_sources_filesystem(
        vec![(p("/"), btrfs(3)), (p("/d"), btrfs(4)), (p("/store"), btrfs(3))],
        "/d/feat", false, false, Route::Refuse(ErrorCode::CrossVolume))]
    #[case::copy_bypasses_the_store(
        vec![(p("/"), ext4(1)), (p("/store"), btrfs(2))], "../feat", true, false, Route::Copy)]
    #[case::fallback_bypasses_the_store(
        vec![(p("/"), ext4(1)), (p("/store"), btrfs(2))], "../feat", false, true, Route::Fallback)]
    fn decision_table_for_taking_the_store(
        #[case] volumes: Vec<(PathBuf, VolumeInfo)>,
        #[case] destination: &str,
        #[case] copy: bool,
        #[case] fallback: bool,
        #[case] want: Route,
    ) {
        let mut w = store_world();
        w.volumes = volumes;
        w.put_dir(Path::new("/d"));
        let req = AddRequest {
            destination: p(destination),
            copy,
            fallback,
            ..new_branch()
        };
        let result = run_store(&w, &req);
        let linked = p(destination);
        match want {
            Route::Store => {
                let out = result.unwrap();
                assert_eq!(out.strategy, Strategy::Store);
                assert_eq!(
                    out.destination,
                    linked.canonicalize_like(),
                    "requested path"
                );
                assert_eq!(out.real_path, Some(real("feat")));
                assert_eq!(
                    w.links.borrow().get(&out.destination),
                    Some(&real("feat")),
                    "the requested path is a symlink to the real worktree"
                );
            }
            Route::Clone | Route::Copy | Route::Fallback => {
                let out = result.unwrap();
                assert_eq!(
                    out.strategy,
                    match want {
                        Route::Clone => Strategy::CopyOnWrite,
                        Route::Copy => Strategy::Copy,
                        _ => Strategy::Fallback,
                    }
                );
                assert_eq!(out.real_path, None);
                assert!(!w.touched(Path::new("/store")), "{:#?}", w.calls());
                assert!(w.links.borrow().is_empty());
            }
            Route::Refuse(code) => {
                assert_eq!(result.unwrap_err().code, code);
                assert_no_mutation(&w);
            }
        }
    }

    /// `p("../feat")` resolved against `/w/repo`, `p("/d/feat")` as is.
    trait CanonicalizeLike {
        fn canonicalize_like(&self) -> PathBuf;
    }

    impl CanonicalizeLike for PathBuf {
        fn canonicalize_like(&self) -> PathBuf {
            if self.is_absolute() {
                self.clone()
            } else {
                p("/w").join(self.strip_prefix("..").unwrap())
            }
        }
    }

    // U-STO-21: no store set up: the usual refusal, plus the way to get one.
    #[test]
    fn not_configured_store_adds_the_setup_hint_to_not_cow() {
        let w = World {
            volumes: vec![(p("/"), ext4(1))],
            ..World::default()
        };
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotCow);
        for needle in ["sudo git parhel setup", "--copy", "--fallback"] {
            assert!(
                err.message.contains(needle),
                "lacks `{needle}`: {}",
                err.message
            );
        }
        assert_no_mutation(&w);
    }

    // U-STO-21b: where stores do not exist (no store path at all) nothing is suggested.
    #[test]
    fn without_a_store_path_nothing_is_suggested() {
        let w = World {
            volumes: vec![(p("/"), ext4(1))],
            ..World::default()
        };
        let err = run_with(&w, &new_branch(), None).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotCow);
        assert!(!err.message.contains("setup"), "{}", err.message);
    }

    // U-STO-22: a store that exists but cannot be used is an error of its own, never
    // a silent fallback to a regular worktree or a full copy.
    #[rstest]
    #[case::marker_missing(|w: &World| { w.files.borrow_mut().remove(&p("/store/.parhel-store")); })]
    #[case::marker_unreadable(|w: &World| { w.files.borrow_mut().insert(p("/store/.parhel-store"), b"???".to_vec()); })]
    #[case::not_writable(|w: &World| { w.unwritable.borrow_mut().insert(p("/store")); })]
    fn configured_but_unavailable_store_is_an_error(#[case] break_it: fn(&World)) {
        let w = store_world();
        break_it(&w);
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert!(err.message.contains("git parhel doctor"), "{}", err.message);
        assert_no_mutation(&w);
    }

    // U-STO-22b: ... including a store on a filesystem that cannot clone.
    #[test]
    fn store_without_cow_is_unavailable() {
        let mut w = store_world();
        w.volumes = vec![(p("/"), ext4(1)), (p("/store"), ext4(2))];
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert_no_mutation(&w);
    }

    // U-STO-23: the escape hatches do not look at the store at all.
    #[rstest]
    #[case::copy(true, false, Strategy::Copy)]
    #[case::fallback(false, true, Strategy::Fallback)]
    fn escape_hatches_ignore_an_unavailable_store(
        #[case] copy: bool,
        #[case] fallback: bool,
        #[case] strategy: Strategy,
    ) {
        let w = store_world();
        w.files.borrow_mut().remove(&p("/store/.parhel-store"));
        let req = AddRequest {
            fallback,
            copy,
            ..new_branch()
        };
        assert_eq!(run_store(&w, &req).unwrap().strategy, strategy);
        assert!(!w.touched(Path::new("/store")), "{:#?}", w.calls());
    }

    // U-STO-24: every refusal about the source or the destination is decided before
    // the store is touched: no lock, no sync.
    #[rstest]
    #[case::dirty_source(|w: &mut World| w.dirty = true, ErrorCode::DirtySource)]
    #[case::operation_in_progress(|w: &mut World| w.operation = Some("merge".into()), ErrorCode::OperationInProgress)]
    #[case::sparse(|w: &mut World| w.sparse = true, ErrorCode::SparseCheckout)]
    #[case::destination_exists(|w: &mut World| { w.existing.borrow_mut().insert(p("/w/feat")); }, ErrorCode::DestExists)]
    #[case::destination_is_a_dangling_symlink(|w: &mut World| { w.links.borrow_mut().insert(p("/w/feat"), p("/gone")); }, ErrorCode::DestExists)]
    #[case::parent_missing(|w: &mut World| drop(w.tree.borrow_mut().remove(&p("/w"))), ErrorCode::DestParentMissing)]
    fn refusals_come_before_the_store(#[case] change: fn(&mut World), #[case] code: ErrorCode) {
        let mut w = store_world();
        change(&mut w);
        assert_eq!(run_store(&w, &new_branch()).unwrap_err().code, code);
        assert_no_mutation(&w);
    }

    // U-STO-45: the store route, step by step.
    #[test]
    fn store_route_runs_steps_in_order() {
        let w = store_world();
        let out = run_store(&w, &new_branch()).unwrap();
        assert_eq!(
            out,
            AddOutcome {
                destination: p(DEST),
                head: SHA_A.into(),
                strategy: Strategy::Store,
                warnings: vec![],
                relocated: 0,
                real_path: Some(real("feat")),
            }
        );
        let pos = |pred: &dyn Fn(&Call) -> bool| {
            w.position(|c| pred(c))
                .unwrap_or_else(|| panic!("missing call in {:#?}", w.calls()))
        };
        let lock = pos(&|c| *c == Call::Lock(p(&format!("{BASELINE_DIR}/lock"))));
        let first_mirror = pos(&|c| matches!(c, Call::Mirror { .. }));
        let last_mirror = w
            .calls()
            .iter()
            .rposition(|c| matches!(c, Call::Mirror { .. } | Call::MirrorDirMeta { .. }))
            .unwrap();
        let staging = pos(&|c| {
            *c == Call::CreateStaging {
                parent: p("/store/worktrees"),
            }
        });
        let add = pos(&|c| {
            *c == Call::WorktreeAdd(WorktreeAddSpec {
                path: p(STORE_STAGED_WT),
                branch: BranchMode::New("feat".into()),
                reference: None,
                no_checkout: true,
            })
        });
        let first_clone = pos(&|c| matches!(c, Call::Clone { .. }));
        let index = pos(&|c| matches!(c, Call::ResetIndex { .. }));
        let registry = pos(&|c| *c == Call::Append(p("/store/links")));
        let symlink = pos(&|c| {
            *c == Call::Symlink {
                target: real("feat"),
                link: p(DEST),
            }
        });
        let rename = pos(&|c| {
            *c == Call::Rename {
                from: p(STORE_STAGED_WT),
                to: real("feat"),
            }
        });
        let repair = pos(&|c| *c == Call::Repair { path: real("feat") });
        let cleanup = pos(&|c| *c == Call::RemoveDirAll(p(STORE_STAGING)));
        assert!(
            lock < first_mirror,
            "locked before the first baseline write"
        );
        assert!(
            last_mirror < staging,
            "the baseline is complete before the clone starts"
        );
        assert!(staging < add && add < first_clone && first_clone < index);
        assert!(index < registry && registry < symlink && symlink < rename);
        assert!(rename < repair && repair < cleanup);
        assert_eq!(
            w.count(|c| matches!(c, Call::RemoveForce { .. } | Call::BranchDelete(_))),
            0
        );
        // The worktree's directory exists in the store before the exclusive rename.
        let parent = pos(&|c| *c == Call::CreateDir(real("feat").parent().unwrap().to_path_buf()));
        assert!(parent < rename);
    }

    // U-STO-46: the lock lives next to the baseline it protects; the metadata names the source.
    #[test]
    fn baseline_has_a_lock_and_a_meta_naming_its_source() {
        let w = store_world();
        run_store(&w, &new_branch()).unwrap();
        assert_eq!(
            w.count(|c| *c == Call::Lock(p(&format!("{BASELINE_DIR}/lock")))),
            1
        );
        let meta = w.files.borrow()[&p(&format!("{BASELINE_DIR}/meta"))].clone();
        let meta = String::from_utf8(meta).unwrap();
        assert!(meta.lines().any(|l| l == "source=/w/repo"), "{meta}");
    }

    // U-STO-47: the worktree is cloned from the baseline, never from the live source,
    // and never copied in full: that is what makes it cheap.
    #[test]
    fn worktree_is_cloned_from_the_baseline() {
        let w = store_world();
        run_store(&w, &new_branch()).unwrap();
        let clones: Vec<_> = w
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Clone { .. } | Call::Copy { .. }))
            .collect();
        assert_eq!(
            clones,
            vec![
                Call::Clone {
                    src: baseline().join("node_modules"),
                    dst: p(&format!("{STORE_STAGED_WT}/node_modules")),
                },
                Call::Clone {
                    src: baseline().join("src"),
                    dst: p(&format!("{STORE_STAGED_WT}/src")),
                },
            ]
        );
    }

    // U-STO-48: the journal knows the link and the real destination.
    #[test]
    fn journal_records_the_link_and_the_real_destination() {
        let w = store_world();
        run_store(&w, &new_branch()).unwrap();
        let j = Journal::decode(&w.files.borrow()[&journal_path()]).unwrap();
        assert_eq!(j.publish_to, Some(real("feat")));
        assert_eq!(j.worktree, Some(p(STORE_STAGED_WT)));
        assert_eq!(
            j.store_link,
            Some(crate::app::rollback::StoreLink {
                path: p(DEST),
                target: real("feat"),
            })
        );
    }

    // U-STO-49: the link is journaled and registered before it is created: a crash
    // between the two leaves a record, never a symlink nobody knows about.
    #[test]
    fn link_is_journaled_and_registered_before_it_is_created() {
        let w = store_world();
        run_store(&w, &new_branch()).unwrap();
        let symlink = w.position(|c| matches!(c, Call::Symlink { .. })).unwrap();
        let journal_write = w
            .calls()
            .iter()
            .rposition(|c| *c == Call::WriteFile(journal_path()))
            .unwrap();
        let registry = w
            .position(|c| *c == Call::Append(p("/store/links")))
            .unwrap();
        assert!(
            journal_write < symlink && registry < symlink,
            "{:#?}",
            w.calls()
        );
        // One line per link: where it is and where it points.
        let registry = String::from_utf8(w.files.borrow()[&p("/store/links")].clone()).unwrap();
        assert_eq!(registry, format!("/w/feat\t{}\n", real("feat").display()));
    }

    // U-STO-50: somebody took the requested path since the checks: `dest-exists`, and
    // since this run never created anything there, nothing is removed from there.
    #[test]
    fn symlink_that_cannot_be_created_is_dest_exists_and_rolls_back() {
        let w = store_world().failing(Step::Symlink, ErrorCode::DestExists);
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::DestExists);
        assert_eq!(w.count(|c| matches!(c, Call::Rename { .. })), 0);
        assert_eq!(w.count(|c| *c == Call::RemoveFile(p(DEST))), 0);
        assert_eq!(
            w.count(|c| *c
                == Call::RemoveForce {
                    path: p(STORE_STAGED_WT)
                }),
            1
        );
        assert_eq!(w.count(|c| *c == Call::BranchDelete("feat".into())), 1);
        assert_eq!(
            w.calls().last(),
            Some(&Call::RemoveDirAll(p(STORE_STAGING)))
        );
    }

    // U-STO-51: the exclusive rename loses a race: the symlink this run made goes away.
    #[test]
    fn failed_publish_removes_the_symlink() {
        let w = store_world().failing(Step::Rename, ErrorCode::DestExists);
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::DestExists);
        assert_eq!(
            w.count(|c| *c == Call::RemoveFile(p(DEST))),
            1,
            "{:#?}",
            w.calls()
        );
        assert!(!w.links.borrow().contains_key(&p(DEST)));
        assert_eq!(
            w.count(|c| *c
                == Call::RemoveForce {
                    path: p(STORE_STAGED_WT)
                }),
            1
        );
        assert_eq!(
            w.calls().last(),
            Some(&Call::RemoveDirAll(p(STORE_STAGING)))
        );
    }

    // U-STO-52: two worktrees with one name in the same repository get distinct real
    // paths; the requested path is always the one the user asked for.
    #[rstest]
    #[case::free(&[], "feat")]
    #[case::taken(&["feat"], "feat-2")]
    #[case::two_taken(&["feat", "feat-2"], "feat-3")]
    #[case::gap_is_filled_from_the_start(&["feat", "feat-3"], "feat-2")]
    #[case::only_a_suffixed_one(&["feat-2"], "feat")]
    fn real_name_gets_a_suffix_when_taken(#[case] existing: &[&str], #[case] want: &str) {
        let w = store_world();
        for name in existing {
            w.put_dir(&real(name));
        }
        let out = run_store(&w, &new_branch()).unwrap();
        assert_eq!(out.destination, p(DEST));
        assert_eq!(out.real_path, Some(real(want)));
        assert_eq!(w.links.borrow().get(&p(DEST)), Some(&real(want)));
        assert_eq!(
            w.count(|c| *c
                == Call::Rename {
                    from: p(STORE_STAGED_WT),
                    to: real(want)
                }),
            1
        );
        assert_eq!(w.count(|c| *c == Call::Repair { path: real(want) }), 1);
    }

    // U-STO-53: references to the source are rewritten to the REAL path: it is valid
    // however the user reaches the worktree, and survives the symlink being replaced.
    #[test]
    fn relocation_points_at_the_real_path() {
        let w = World {
            staged_files: vec![(p("env/bin/tool"), b"#!/w/repo/env/bin/python\n".to_vec())],
            staged_links: vec![(p("env/link"), p("/w/repo/env/bin"))],
            ..store_world()
        };
        let out = run_store(&w, &new_branch()).unwrap();
        let tool = p(&format!("{STORE_STAGED_WT}/env/bin/tool"));
        let expected = format!("#!{}/env/bin/python\n", real("feat").display());
        assert_eq!(w.files.borrow()[&tool], expected.as_bytes());
        assert_eq!(
            w.count(|c| *c
                == Call::Relink {
                    path: p(&format!("{STORE_STAGED_WT}/env/link")),
                    target: real("feat").join("env/bin"),
                }),
            1
        );
        assert_eq!(out.relocated, 2);
    }

    /// Whatever failed, nothing of this run remains: no symlink, no registration, no
    /// branch, no staging, and the baseline was not touched by the rollback.
    fn assert_store_run_undone(w: &World, created_branch: Option<&str>) {
        let calls = w.calls();
        assert!(
            !w.links.borrow().contains_key(&p(DEST)),
            "symlink left: {calls:#?}"
        );
        let created_link = calls.iter().any(|c| matches!(c, Call::Symlink { .. }))
            && !w.fail.borrow().contains_key(&Step::Symlink);
        assert_eq!(
            w.count(|c| *c == Call::RemoveFile(p(DEST))),
            usize::from(created_link),
            "{calls:#?}"
        );
        let registered = calls.iter().any(|c| matches!(c, Call::WorktreeAdd(_)))
            && !w.fail.borrow().contains_key(&Step::WorktreeAdd);
        assert_eq!(
            w.count(|c| *c
                == Call::RemoveForce {
                    path: p(STORE_STAGED_WT)
                }),
            usize::from(registered),
            "{calls:#?}"
        );
        match created_branch {
            Some(b) if registered => {
                assert_eq!(
                    w.count(|c| *c == Call::BranchDelete(b.into())),
                    1,
                    "{calls:#?}"
                );
            }
            _ => assert_eq!(
                w.count(|c| matches!(c, Call::BranchDelete(_))),
                0,
                "{calls:#?}"
            ),
        }
        // (A staging directory that could not be created is nothing to remove.)
        if calls
            .iter()
            .any(|c| matches!(c, Call::CreateStaging { .. }))
            && !w.fail.borrow().contains_key(&Step::CreateStaging)
        {
            assert_eq!(
                calls.last(),
                Some(&Call::RemoveDirAll(p(STORE_STAGING))),
                "{calls:#?}"
            );
        }
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                Call::RemoveDirAll(path) | Call::RemoveFile(path) if path.starts_with("/store/baselines")
            )),
            "the baseline was not touched by the rollback: {calls:#?}"
        );
        assert_eq!(
            w.count(|c| *c == Call::RemoveDirAll(p(DEST))),
            0,
            "dest never deleted"
        );
    }

    // U-STO-54: a failure at any step of the store route leaves no trace.
    #[rstest]
    #[case::create_staging(Step::CreateStaging, ErrorCode::Io)]
    #[case::journal(Step::WriteJournal, ErrorCode::Io)]
    #[case::worktree_add(Step::WorktreeAdd, ErrorCode::GitFailed)]
    #[case::clone(Step::Clone, ErrorCode::CloneFailed)]
    #[case::index(Step::ResetIndex, ErrorCode::GitFailed)]
    #[case::registry(Step::Append, ErrorCode::Io)]
    #[case::symlink(Step::Symlink, ErrorCode::DestExists)]
    #[case::rename(Step::Rename, ErrorCode::Io)]
    fn failure_at_each_step_leaves_no_trace(#[case] step: Step, #[case] code: ErrorCode) {
        let w = store_world().failing(step, code);
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, code, "{err}");
        assert_store_run_undone(&w, Some("feat"));
    }

    // U-STO-55: a sync that fails stops the run before anything is registered, and the
    // baseline stays as the sync left it (the next sync repairs it).
    #[test]
    fn failing_sync_registers_nothing() {
        let w = store_world().failing(Step::Mirror, ErrorCode::CloneFailed);
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
        assert_eq!(
            w.count(|c| matches!(
                c,
                Call::CreateStaging { .. } | Call::WorktreeAdd(_) | Call::Symlink { .. }
            )),
            0
        );
        assert_store_run_undone(&w, None);
    }

    // U-STO-56: Ctrl-C during the sync and later: rolled back, never half-published.
    #[rstest]
    #[case::during_sync(Step::Mirror)]
    #[case::after_registration(Step::WorktreeAdd)]
    #[case::after_clone(Step::Clone)]
    #[case::after_index(Step::ResetIndex)]
    fn interrupt_in_the_store_route_rolls_back(#[case] step: Step) {
        let w = store_world();
        w.interrupt_after.set(Some(step));
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Interrupted, "{err}");
        assert_eq!(
            w.count(|c| matches!(c, Call::Symlink { .. } | Call::Rename { .. })),
            0
        );
        assert_store_run_undone(&w, Some("feat"));
    }

    // U-STO-57: the source moved while the worktree was cloned from the baseline:
    // the check runs against the live source, as in the normal route.
    #[test]
    fn source_changed_during_the_run_is_refused_without_a_trace() {
        let w = World {
            source_head_after_clone: Some(SHA_B.into()),
            ..store_world()
        };
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::SourceChanged);
        assert_eq!(w.count(|c| matches!(c, Call::Symlink { .. })), 0);
        assert_store_run_undone(&w, Some("feat"));
    }

    // U-STO-58: a branch that existed before is never removed by the rollback.
    #[test]
    fn existing_branch_survives_a_failed_store_run() {
        let w = store_world().failing(Step::Rename, ErrorCode::Io);
        w.branches.borrow_mut().insert("topic".into());
        let req = request(BranchMode::Existing("topic".into()));
        run_store(&w, &req).unwrap_err();
        assert_store_run_undone(&w, None);
    }

    // U-STO-59: the same repository twice reuses its baseline: the second run's sync
    // finds nothing to copy.
    #[test]
    fn second_run_for_the_same_repository_copies_nothing() {
        let w = store_world();
        run_store(&w, &new_branch()).unwrap();
        let first = w.count(|c| matches!(c, Call::Mirror { .. }));
        assert!(first > 0);
        let again = AddRequest {
            destination: p("../feat2"),
            branch: BranchMode::New("feat2".into()),
            ..new_branch()
        };
        run_store(&w, &again).unwrap();
        assert_eq!(
            w.count(|c| matches!(c, Call::Mirror { .. })),
            first,
            "no entry copied the second time"
        );
        assert_eq!(
            w.count(|c| *c == Call::Lock(p(&format!("{BASELINE_DIR}/lock")))),
            2
        );
    }

    fn baseline_meta(w: &World) -> String {
        let meta = w.files.borrow()[&p(&format!("{BASELINE_DIR}/meta"))].clone();
        String::from_utf8(meta).unwrap()
    }

    fn sync_start(meta: &str) -> Option<i128> {
        crate::domain::store::last_sync_of(meta.as_bytes())
    }

    fn second_request() -> AddRequest {
        AddRequest {
            destination: p("../feat2"),
            branch: BranchMode::New("feat2".into()),
            ..new_branch()
        }
    }

    // U-STO-59b: a finished sync records when it began, and the next one is judged
    // against it: a file whose status changed after that moment is copied again even
    // though its size, mtime and mode are what the baseline has.
    #[test]
    fn a_finished_sync_records_its_start_and_the_next_uses_it() {
        let w = store_world();
        w.put_file(&p("/w/repo/src/main.rs"), 20, 20);
        run_store(&w, &new_branch()).unwrap();
        let meta = baseline_meta(&w);
        assert!(meta.lines().any(|l| l == "state=synced"), "{meta}");
        let started = sync_start(&meta).expect("the start of the sync is recorded");
        assert!(started > 1_600_000_000_000_000_000, "{started}");

        // The file as the baseline holds it, then rewritten after the sync, with the
        // same size, mtime and mode: only its ctime shows.
        w.copy_facts(&p("/w/repo/src/main.rs"), &baseline().join("src/main.rs"));
        let mut rewritten = w
            .stat_within(Path::new("/w/repo"), Path::new("src/main.rs"))
            .unwrap()
            .unwrap();
        rewritten.ctime_ns = started + 1_000_000_000;
        w.meta
            .borrow_mut()
            .insert(p("/w/repo/src/main.rs"), rewritten);
        let before = w.count(|c| matches!(c, Call::Mirror { .. }));
        run_store(&w, &second_request()).unwrap();
        let copied: Vec<_> = w
            .calls()
            .into_iter()
            .skip_while(|c| !matches!(c, Call::Rename { .. }))
            .filter_map(|c| match c {
                Call::Mirror { dst, .. } => Some(dst),
                _ => None,
            })
            .collect();
        assert_eq!(copied, vec![baseline().join("src/main.rs")], "{before}");
        assert!(sync_start(&baseline_meta(&w)).unwrap() > started);
    }

    // U-STO-59c: a sync that did not finish does not move the mark: the entries it did
    // not get to are judged against the last sync that did. One that fails keeps the
    // mark of the one before it; a first sync that fails leaves none.
    #[test]
    fn a_sync_that_fails_keeps_the_last_finished_marks() {
        let w = store_world();
        w.put_file(&p("/w/repo/src/main.rs"), 20, 20);
        w.fail
            .borrow_mut()
            .insert(Step::Mirror, ErrorCode::CloneFailed);
        run_store(&w, &new_branch()).unwrap_err();
        let meta = baseline_meta(&w);
        assert!(meta.lines().any(|l| l == "state=synced"), "{meta}");
        assert_eq!(sync_start(&meta), None, "{meta}");
        w.fail.borrow_mut().clear();

        run_store(&w, &second_request()).unwrap();
        let first = sync_start(&baseline_meta(&w)).expect("a finished sync records its start");

        // A change in the source that the next sync would have to copy, and it fails.
        w.copy_facts(&p("/w/repo/src/main.rs"), &baseline().join("src/main.rs"));
        w.put_file(&p("/w/repo/src/main.rs"), 21, 20);
        w.fail
            .borrow_mut()
            .insert(Step::Mirror, ErrorCode::CloneFailed);
        let third = AddRequest {
            destination: p("../feat3"),
            branch: BranchMode::New("feat3".into()),
            ..new_branch()
        };
        run_store(&w, &third).unwrap_err();
        assert_eq!(
            sync_start(&baseline_meta(&w)),
            Some(first),
            "{}",
            baseline_meta(&w)
        );
    }

    // U-STO-59d: a baseline whose `meta` has no mark (written before the field existed,
    // or damaged) is not trusted: everything in it is copied again.
    #[test]
    fn a_baseline_without_a_mark_is_copied_again() {
        let w = store_world();
        w.put_file(&p("/w/repo/src/main.rs"), 20, 20);
        run_store(&w, &new_branch()).unwrap();
        w.copy_facts(&p("/w/repo/src/main.rs"), &baseline().join("src/main.rs"));
        let meta = baseline_meta(&w)
            .lines()
            .filter(|l| !l.starts_with("sync_start_ns="))
            .map(|l| format!("{l}\n"))
            .collect::<String>();
        w.files
            .borrow_mut()
            .insert(p(&format!("{BASELINE_DIR}/meta")), meta.into_bytes());
        let first = w.count(
            |c| matches!(c, Call::Mirror { dst, .. } if *dst == baseline().join("src/main.rs")),
        );
        run_store(&w, &second_request()).unwrap();
        let after = w.count(
            |c| matches!(c, Call::Mirror { dst, .. } if *dst == baseline().join("src/main.rs")),
        );
        assert_eq!(after, first + 1, "the file was copied again");
    }

    // U-STO-91: a worktree published in the store is locked in git, with a reason that
    // names the store: `git worktree prune` (and the `gc` that runs it) drops the
    // registration of a worktree whose directory is not there, and while the store's
    // mount is down none of them is. The lock follows the repair of the registration,
    // at the real path. Nothing else is ever locked.
    #[test]
    fn a_published_store_worktree_is_locked_against_prune() {
        let w = store_world();
        run_store(&w, &new_branch()).unwrap();
        let lock = Call::WorktreeLock {
            path: real("feat"),
            reason: "parhel store: /store".into(),
        };
        assert_eq!(w.count(|c| matches!(c, Call::WorktreeLock { .. })), 1);
        let repair = w
            .position(|c| *c == Call::Repair { path: real("feat") })
            .unwrap();
        let locked = w.position(|c| *c == lock).expect("locked at the real path");
        let cleanup = w
            .position(|c| *c == Call::RemoveDirAll(p(STORE_STAGING)))
            .unwrap();
        assert!(repair < locked && locked < cleanup, "{:#?}", w.calls());

        let plain = World::default();
        run(&plain, &new_branch()).unwrap();
        assert_eq!(
            plain.count(|c| matches!(c, Call::WorktreeLock { .. })),
            0,
            "only the store's worktrees"
        );
    }

    // U-STO-92: the worktree is there and valid once published: a lock that could not be
    // placed is a warning that says how to place it, never a rollback.
    #[test]
    fn a_lock_that_fails_is_a_warning_and_the_worktree_stays() {
        let w = store_world().failing(Step::WorktreeLock, ErrorCode::GitFailed);
        let out = run_store(&w, &new_branch()).unwrap();
        assert!(
            out.warnings.iter().any(|m| m.contains("git worktree lock")
                && m.contains(&real("feat").display().to_string())),
            "{:?}",
            out.warnings
        );
        assert_eq!(out.real_path, Some(real("feat")));
        assert_eq!(
            w.count(|c| matches!(c, Call::RemoveForce { .. } | Call::BranchDelete(_))),
            0
        );
    }

    // U-STO-22c: the message of a store that cannot be used names `git parhel doctor`
    // and `systemctl status`, with the mount unit when the marker still says which.
    #[rstest]
    #[case::unit_known(|w: &World| { w.unwritable.borrow_mut().insert(p("/store")); }, true)]
    #[case::unit_unknown(|w: &World| { w.files.borrow_mut().remove(&p("/store/.parhel-store")); }, false)]
    fn unavailable_store_message_names_the_way_to_look(
        #[case] break_it: fn(&World),
        #[case] unit_known: bool,
    ) {
        let w = store_world();
        break_it(&w);
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable);
        for needle in ["git parhel doctor", "systemctl status", "/store"] {
            assert!(
                err.message.contains(needle),
                "lacks `{needle}`: {}",
                err.message
            );
        }
        assert_eq!(
            err.message.contains("parhel-alice.mount"),
            unit_known,
            "{}",
            err.message
        );
    }

    // U-STO-85: a baseline whose last sync never reported its end (power loss, SIGKILL)
    // may hold files with the right metadata and no data: it is made again, not trusted.
    // One that finished, or that was stopped and said so, is only brought up to date.
    #[rstest]
    #[case::died_syncing("state=syncing\n", true)]
    #[case::finished("state=synced\n", false)]
    #[case::no_state("", false)]
    fn an_unfinished_baseline_is_rebuilt(#[case] state: &str, #[case] rebuilt: bool) {
        let w = store_world();
        w.put_dir(&baseline());
        w.files.borrow_mut().insert(
            p(&format!("{BASELINE_DIR}/meta")),
            format!("source=/w/repo\n{state}").into_bytes(),
        );
        run_store(&w, &new_branch()).unwrap();
        let removed = w.count(|c| *c == Call::RemoveDirAll(baseline()));
        assert_eq!(removed, usize::from(rebuilt), "{:#?}", w.calls());
        if rebuilt {
            let wiped = w
                .position(|c| *c == Call::RemoveDirAll(baseline()))
                .unwrap();
            let first = w.position(|c| matches!(c, Call::Mirror { .. })).unwrap();
            assert!(wiped < first, "before the first copy");
        }
        let meta = w.files.borrow()[&p(&format!("{BASELINE_DIR}/meta"))].clone();
        assert!(String::from_utf8(meta).unwrap().contains("state=synced"));
    }

    // U-STO-86: the sync is flushed to disk before the baseline counts as finished, and
    // a flush that fails (a full disk shows itself only here) fails the run with that
    // message, publishes nothing, and leaves the baseline marked unfinished.
    #[test]
    fn a_failed_flush_fails_the_run_and_leaves_the_baseline_unfinished() {
        let w = store_world();
        run_store(&w, &new_branch()).unwrap();
        assert_eq!(w.flushes.get(), 1, "flushed once, before the clone");

        let w = store_world();
        w.fail_flush.set(true);
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert!(err.message.contains("No space left"), "{}", err.message);
        assert_eq!(
            w.count(|c| matches!(c, Call::CreateStaging { .. } | Call::Symlink { .. })),
            0
        );
        let meta = w.files.borrow()[&p(&format!("{BASELINE_DIR}/meta"))].clone();
        assert!(String::from_utf8(meta).unwrap().contains("state=syncing"));
        assert_store_run_undone(&w, None);
    }

    // U-STO-87: the disk of the store fills up while mirroring or while cloning: the
    // run fails with the system's words, and nothing of it remains.
    #[rstest]
    #[case::while_mirroring(Step::Mirror)]
    #[case::while_cloning(Step::Clone)]
    #[case::while_writing_the_registry(Step::Append)]
    fn a_full_store_rolls_back_cleanly(#[case] step: Step) {
        let w = store_world().failing(step, ErrorCode::Io);
        *w.fail_message.borrow_mut() = "No space left on device (os error 28)".into();
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert!(err.message.contains("No space left"), "{}", err.message);
        assert_store_run_undone(&w, Some("feat"));
    }

    // U-STO-88: a path the registry (tab separated lines) could not hold is refused
    // before anything is created in the store; the same name is fine where no registry
    // is involved.
    #[rstest]
    #[case::tab("../fe\tat")]
    #[case::newline("../fe\nat")]
    fn destinations_the_registry_cannot_hold_are_refused(#[case] destination: &str) {
        let w = store_world();
        let req = AddRequest {
            destination: p(destination),
            ..new_branch()
        };
        let err = run_store(&w, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage, "{err}");
        assert!(err.message.contains("tab or a newline"), "{}", err.message);
        assert_no_mutation(&w);

        // Copy-on-write in place has no registry.
        let w = World {
            volumes: vec![(p("/"), btrfs(3))],
            ..World::default()
        };
        assert_eq!(run(&w, &req).unwrap().strategy, Strategy::CopyOnWrite);
    }

    // U-STO-89: a symlink the registry knows inside the source (a worktree requested
    // there) is not mirrored, so no later worktree is born with a link to a sibling.
    #[test]
    fn registered_links_inside_the_source_are_not_mirrored() {
        let w = store_world();
        w.put_dir(&p("/w/repo/.worktrees"));
        w.put_link(&p("/w/repo/.worktrees/old"), &real("old"));
        w.put_link(&p("/w/repo/.worktrees/unrelated"), &p("/elsewhere"));
        w.files.borrow_mut().insert(
            p("/store/links"),
            format!(
                "/w/repo/.worktrees/old\t{}\n/w/repo/.worktrees/unrelated\t{}\n",
                real("old").display(),
                real("other").display()
            )
            .into_bytes(),
        );
        run_store(&w, &new_branch()).unwrap();
        let mirrored: Vec<_> = w
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Mirror { dst, .. } => Some(dst),
                _ => None,
            })
            .collect();
        assert!(
            !mirrored.contains(&baseline().join(".worktrees/old")),
            "{mirrored:?}"
        );
        // The registry is a record, not a license: a link that no longer points where it
        // was registered is the user's, and is mirrored like any other.
        assert!(
            mirrored.contains(&baseline().join(".worktrees/unrelated")),
            "{mirrored:?}"
        );
    }

    // U-STO-50b: another run got the same path first, and by the same name its link points
    // at the very same real path. The loser's rollback must not take that link for its own.
    #[test]
    fn a_rival_link_to_the_same_target_is_never_removed() {
        let w = store_world();
        *w.rival_link.borrow_mut() = Some((p(DEST), real("feat")));
        let err = run_store(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::DestExists, "{err}");
        assert_eq!(
            w.links.borrow().get(&p(DEST)),
            Some(&real("feat")),
            "still there"
        );
        assert_eq!(
            w.count(|c| matches!(c, Call::RemoveFile(path) | Call::RemoveDirAll(path) if *path == p(DEST))),
            0,
            "{:#?}",
            w.calls()
        );
        assert_eq!(
            w.count(|c| *c
                == Call::RemoveForce {
                    path: p(STORE_STAGED_WT)
                }),
            1
        );
    }
}
