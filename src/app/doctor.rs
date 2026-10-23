//! `git parhel doctor`: diagnose the environment and repair leftovers of crashed runs.

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::app::add::Env;
use crate::app::rollback::{JOURNAL_FILE, Journal, rollback};
use crate::app::store::discover;
use crate::domain::paths::{self, is_staging_name};
use crate::domain::store::{
    BASELINES_DIR, LINKS_FILE, Store, StoreState, WORKTREES_DIR, is_repo_id, repo_id,
};
use crate::domain::{DoctorRequest, ErrorCode, Result};
use crate::git::parse::WorktreeEntry;
use crate::ports::{CowFs, RepoInfo};

/// Something worth reporting. `code()` is printed and is a stable contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// The source worktree's volume has no copy-on-write: `add` would refuse (or need `--fallback`).
    NotCow { path: PathBuf, fs_type: String },
    /// A `.parhel-staging-*` dir left by a crashed `add`.
    OrphanStaging { path: PathBuf },
    /// A worktree registration whose path is inside a staging dir.
    OrphanRegistration { path: PathBuf },
    /// A `.git` *file* inside the source pointing to another repository's git dir:
    /// `add` would clone it and the copy would share that repository's state.
    ForeignGitfile { path: PathBuf },
    /// A staging dir whose journal is locked by a live `add`: not a problem, never touched.
    InProgress { path: PathBuf },
    /// The store is configured but unusable (not mounted, marker missing, ...).
    StoreUnavailable { path: PathBuf, reason: String },
    /// The store is mounted but its backing image was deleted: copy work out before
    /// unmounting, the data disappears with the last reference.
    StoreImageDeleted { path: PathBuf, image: String },
    /// The store or the filesystem holding its image is nearly full. `path` is
    /// the one that is (the store root, or the directory of the image).
    StoreLowSpace {
        path: PathBuf,
        free: u64,
        total: u64,
    },
    /// A directory under `worktrees/<id>/` that git does not know. Reported only:
    /// its contents are never deleted.
    StoreOrphan { path: PathBuf },
    /// A symlink recorded in the store's registry that points into the store at
    /// something that no longer exists.
    StoreDanglingLink { link: PathBuf, target: PathBuf },
    /// A baseline whose source is no longer a git worktree.
    StoreStaleBaseline { path: PathBuf },
    /// The store cannot be reached and git lists worktrees of this repository that live
    /// in it. `git worktree prune` (and the `gc` that runs it) forgets the registration
    /// of a worktree whose directory is not there, and with it the branch checked out
    /// there, unless the worktree is locked: `unlocked` are those that are not.
    StoreWorktreesUnreachable {
        root: PathBuf,
        worktrees: Vec<PathBuf>,
        unlocked: Vec<PathBuf>,
    },
}

impl Finding {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotCow { .. } => "not-cow",
            Self::OrphanStaging { .. } => "orphan-staging",
            Self::OrphanRegistration { .. } => "orphan-registration",
            Self::ForeignGitfile { .. } => "foreign-gitfile",
            Self::InProgress { .. } => "in-progress",
            Self::StoreUnavailable { .. } => "store-unavailable",
            Self::StoreImageDeleted { .. } => "store-image-deleted",
            Self::StoreLowSpace { .. } => "store-low-space",
            Self::StoreOrphan { .. } => "store-orphan",
            Self::StoreDanglingLink { .. } => "store-dangling-link",
            Self::StoreStaleBaseline { .. } => "store-stale-baseline",
            Self::StoreWorktreesUnreachable { .. } => "store-worktrees-unreachable",
        }
    }

    /// Whether `--fix` can repair this finding.
    #[must_use]
    pub const fn fixable(&self) -> bool {
        matches!(
            self,
            Self::OrphanStaging { .. }
                | Self::OrphanRegistration { .. }
                | Self::StoreDanglingLink { .. }
                | Self::StoreStaleBaseline { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DoctorReport {
    pub findings: Vec<Finding>,
    /// Subset of `findings` repaired by `--fix`.
    pub fixed: Vec<Finding>,
}

impl DoctorReport {
    /// Healthy when nothing is left unrepaired (runs in progress are not problems).
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.findings
            .iter()
            .all(|f| self.fixed.contains(f) || matches!(f, Finding::InProgress { .. }))
    }
}

/// Scan the repository of `request.from`:
/// - source volume type;
/// - staging dirs next to every registered worktree and the source;
/// - registrations pointing into staging dirs;
/// - foreign gitfiles anywhere under the source (full walk; `doctor` may be slow).
///
/// With `fix`, orphans are undone using their journal (same steps as rollback).
pub fn doctor(env: &Env<'_>, request: &DoctorRequest) -> Result<DoctorReport> {
    let repo = env
        .git
        .discover(&paths::resolve_destination(env.cwd, &request.from))?;
    let mut report = DoctorReport::default();
    // Looking at the store is reading only; a store that is not there is not a finding.
    let mut store_state = match env.store {
        Some(path) => discover(env.fs, path)?,
        None => StoreState::NotConfigured,
    };
    if let StoreState::Available(store) = &mut store_state {
        // Registrations name canonical paths; compare like with like.
        store.root = env.fs.canonicalize(&store.root)?;
    }

    report
        .findings
        .extend(not_cow_finding(env, &repo, &store_state)?);

    let worktrees = env.git.worktrees(&repo.toplevel)?;
    let mut scan_dirs: BTreeSet<PathBuf> = worktrees
        .iter()
        .filter_map(|w| w.path.parent().map(Path::to_path_buf))
        .collect();
    scan_dirs.extend(repo.toplevel.parent().map(Path::to_path_buf));
    // The staging dirs of store runs sit in the store, with no registration to lead here
    // when the run died before registering anything.
    let store = match &store_state {
        StoreState::Available(store) => Some(store),
        _ => None,
    };
    scan_dirs.extend(store.map(|s| s.root.join(WORKTREES_DIR)));
    // A crashed run's registration points inside its staging dir, which sits next
    // to the destination, possibly far from the source: look there too.
    scan_dirs.extend(
        worktrees
            .iter()
            .filter_map(|w| staging_of(&w.path))
            .filter_map(|s| s.parent().map(Path::to_path_buf)),
    );
    let mut live = Vec::new();
    let mut orphans = Vec::new();
    for dir in &scan_dirs {
        let Ok(names) = env.fs.list_dir(dir) else {
            continue;
        };
        for name in names.iter().filter(|n| is_staging_name(n)) {
            let staging = dir.join(name);
            if env.fs.is_locked(&staging.join(JOURNAL_FILE))? {
                live.push(staging);
            } else {
                orphans.push(staging);
            }
        }
    }
    // parhel stages a worktree as `<staging dir>/<name>`, nothing else.
    let in_staging = |path: &Path| staging_of(path).is_some();
    let registrations: Vec<PathBuf> = worktrees
        .iter()
        .map(|w| w.path.clone())
        .filter(|p| in_staging(p) && !live.iter().any(|l| p.starts_with(l)))
        .collect();
    report
        .findings
        .extend(live.iter().map(|p| Finding::InProgress { path: p.clone() }));
    report.findings.extend(
        orphans
            .iter()
            .map(|p| Finding::OrphanStaging { path: p.clone() }),
    );
    report.findings.extend(
        registrations
            .iter()
            .map(|p| Finding::OrphanRegistration { path: p.clone() }),
    );
    find_foreign_gitfiles(env, &repo, &repo.toplevel, &mut report);

    // Where crashed store runs were going (before the fix removes their staging dirs).
    let claimed = store.map_or_else(Vec::new, |s| claimed_destinations(env.fs, s));
    if request.fix {
        fix(
            env,
            &repo,
            &worktrees,
            &orphans,
            &registrations,
            store.map(|s| s.root.as_path()),
            &mut report,
        );
    }
    // After the fix: a crashed run's symlink is gone by now and is not reported twice.
    match &store_state {
        StoreState::Unavailable { root, reason } => {
            report.findings.push(Finding::StoreUnavailable {
                path: root.clone(),
                reason: reason.clone(),
            });
            unreachable_worktrees(env, root, &worktrees, &mut report);
        }
        StoreState::Available(store) => {
            check_store(
                env,
                &repo,
                store,
                &worktrees,
                &claimed,
                request.fix,
                &mut report,
            );
        }
        StoreState::NotConfigured => {
            if let Some(root) = env.store {
                unreachable_worktrees(env, root, &worktrees, &mut report);
            }
        }
    }
    Ok(report)
}

/// Worktrees git lists inside the store at `root`, which cannot be reached: a `git
/// worktree prune` (the `gc` runs one) would forget their registrations, and the
/// branches checked out in them would lose their worktree. Locked ones are skipped by
/// prune; `unlocked` are the ones that are not. Reported only.
fn unreachable_worktrees(
    env: &Env<'_>,
    root: &Path,
    worktrees: &[WorktreeEntry],
    report: &mut DoctorReport,
) {
    // Registrations name canonical paths, and the store's own directory is the only
    // part of the path that may be reached through a link.
    let mut bases = vec![root.join(WORKTREES_DIR)];
    if let Ok(canonical) = env.fs.canonicalize(root) {
        bases.push(canonical.join(WORKTREES_DIR));
    }
    let inside: Vec<&WorktreeEntry> = worktrees
        .iter()
        .filter(|w| bases.iter().any(|base| w.path.starts_with(base)))
        .collect();
    if inside.is_empty() {
        return;
    }
    report.findings.push(Finding::StoreWorktreesUnreachable {
        root: root.to_path_buf(),
        worktrees: inside.iter().map(|w| w.path.clone()).collect(),
        unlocked: inside
            .iter()
            .filter(|w| w.locked.is_none())
            .map(|w| w.path.clone())
            .collect(),
    });
}

/// `not-cow` for the source's volume, unless the store can serve it.
fn not_cow_finding(
    env: &Env<'_>,
    repo: &RepoInfo,
    store_state: &StoreState,
) -> Result<Option<Finding>> {
    let mut volume = env.fs.volume(&repo.toplevel)?;
    if !volume.cow {
        // A read-only top directory cannot be probed for reflink support; the git
        // directory on the same filesystem can.
        if let Ok(git) = env.fs.volume(&repo.git_dir) {
            if git.cow && git.filesystem == volume.filesystem {
                volume = git;
            }
        }
    }
    // `add` serves a source without copy-on-write through the store, so while the store
    // can do that the source's filesystem is no problem.
    let served = match store_state {
        StoreState::Available(store) => env
            .fs
            .volume(&store.root)
            .is_ok_and(|held| held.filesystem != volume.filesystem),
        _ => false,
    };
    Ok((!volume.cow && !served).then(|| Finding::NotCow {
        path: repo.toplevel.clone(),
        fs_type: volume.fs_type,
    }))
}

/// Undo crashed runs: with a journal, exactly what the run created; without one,
/// only registrations inside the staging dir and the dir itself (branches are kept:
/// nothing proves parhel created them).
fn fix(
    env: &Env<'_>,
    repo: &RepoInfo,
    worktrees: &[WorktreeEntry],
    orphans: &[PathBuf],
    registrations: &[PathBuf],
    store: Option<&Path>,
    report: &mut DoctorReport,
) {
    let mut handled: Vec<PathBuf> = Vec::new();
    for staging in orphans {
        let journal = env
            .fs
            .read_file(&staging.join(JOURNAL_FILE))
            .ok()
            .and_then(|bytes| Journal::decode(&bytes).ok())
            .and_then(|journal| trusted(journal, repo, worktrees, staging, store));
        let ok = if let Some(journal) = journal {
            handled.extend(journal.worktree.clone());
            let published = journal.worktree.as_deref().and_then(|staged| {
                published_dest(env, staging, staged, journal.publish_to.as_deref())
                    .map(|dest| (staged, dest))
            });
            match published {
                Some((staged, dest)) => complete_publish(env, repo, staging, staged, &dest, store),
                None => rollback(env.git, env.fs, staging, &journal).is_ok(),
            }
        } else {
            let mut ok = true;
            for reg in registrations.iter().filter(|r| r.starts_with(staging)) {
                ok &= match published_dest(env, staging, reg, None) {
                    Some(dest) => env.git.repoint_worktree(&repo.toplevel, reg, &dest).is_ok(),
                    None => forget_missing(env, repo, reg),
                };
                handled.push(reg.clone());
            }
            // Without a readable journal nothing proves the remaining contents are
            // parhel's: only a folder holding nothing but parhel's (unreadable)
            // journal file is removed.
            let empty = env
                .fs
                .list_dir(staging)
                .is_ok_and(|names| names.iter().all(|n| n == JOURNAL_FILE));
            ok && empty && env.fs.remove_dir_all(staging).is_ok()
        };
        if ok {
            report.fixed.push(Finding::OrphanStaging {
                path: staging.clone(),
            });
            for reg in registrations.iter().filter(|r| r.starts_with(staging)) {
                report
                    .fixed
                    .push(Finding::OrphanRegistration { path: reg.clone() });
            }
        }
    }
    for reg in registrations.iter().filter(|r| !handled.contains(r)) {
        // The staging dir itself may be gone; the worktree may still have been
        // published next to where it was.
        let published = staging_of(reg).and_then(|s| published_dest(env, &s, reg, None));
        let fixed = match published {
            Some(dest) => env.git.repoint_worktree(&repo.toplevel, reg, &dest).is_ok(),
            None => forget_missing(env, repo, reg),
        };
        if fixed {
            report
                .fixed
                .push(Finding::OrphanRegistration { path: reg.clone() });
        }
    }
}

/// A journal is plain data in a folder anyone could have created: keep only what
/// the filesystem and git confirm. The staged worktree must be a direct child of
/// this staging dir; the repository is always the one doctor runs in; the
/// destination is the staging dir's sibling, never read from the journal; and the
/// branch counts only if the staged registration really has it checked out.
///
/// In a store (the staging dir is directly under `<store>/worktrees`) the destination
/// is not a sibling: it is the journaled one, accepted only when it is exactly
/// `<store>/worktrees/<id>/<name>`, and the journaled symlink only when it points at that
/// destination (it is removed only while it still does).
/// Returns `None` when the journal points anywhere else.
fn trusted(
    journal: Journal,
    repo: &RepoInfo,
    worktrees: &[WorktreeEntry],
    staging: &Path,
    store: Option<&Path>,
) -> Option<Journal> {
    let staged = match journal.worktree {
        Some(staged) if staged.parent() == Some(staging) && staged.file_name().is_some() => {
            Some(staged)
        }
        Some(_) => return None,
        None => None,
    };
    let created_branch = journal.created_branch.filter(|name| {
        let checked_out = format!("refs/heads/{name}");
        staged.as_ref().is_some_and(|staged| {
            worktrees
                .iter()
                .any(|w| &w.path == staged && w.branch.as_deref() == Some(checked_out.as_str()))
        })
    });
    let worktrees_dir = store
        .filter(|root| staging.parent() == Some(root.join(WORKTREES_DIR).as_path()))
        .map(|root| root.join(WORKTREES_DIR));
    let (publish_to, store_link) = match &worktrees_dir {
        Some(dir) => {
            let publish_to = journal
                .publish_to
                .filter(|dest| is_store_destination(dest, dir));
            let link = journal
                .store_link
                .filter(|link| publish_to.as_ref() == Some(&link.target));
            (publish_to, link)
        }
        None => (
            staged
                .as_ref()
                .and_then(|s| Some(staging.parent()?.join(s.file_name()?))),
            None,
        ),
    };
    Some(Journal {
        repo: repo.toplevel.clone(),
        worktree: staged,
        created_branch,
        publish_to,
        branch_start: journal.branch_start,
        store_link,
    })
}

/// `dest` is `<worktrees>/<id>/<name>` with nothing else in it (no `..`, no extra
/// levels) and `<id>` shaped like a repository id.
fn is_store_destination(dest: &Path, worktrees: &Path) -> bool {
    use std::path::Component;
    let Ok(rest) = dest.strip_prefix(worktrees) else {
        return false;
    };
    let parts: Vec<Component<'_>> = rest.components().collect();
    matches!(parts.as_slice(), [Component::Normal(id), Component::Normal(_)] if is_repo_id(id))
}

// ------------------------------------------------------------------ the store

/// A host filesystem or a store is low on space below this many free bytes (the host)
/// or this share of its size (both).
const LOW_FREE_BYTES: u64 = 2 << 30;

/// Everything about a store that can be checked while it is available. Reports only,
/// except for what `fix` can repair without deleting anybody's work: a symlink the
/// registry knows that dangles, and a baseline (a cache) whose source is gone. A
/// worktree directory is never deleted.
fn check_store(
    env: &Env<'_>,
    repo: &RepoInfo,
    store: &Store,
    worktrees: &[WorktreeEntry],
    claimed: &[PathBuf],
    fix: bool,
    report: &mut DoctorReport,
) {
    if let Some(image) = deleted_image(env.fs, &store.root) {
        report.findings.push(Finding::StoreImageDeleted {
            path: store.root.clone(),
            image,
        });
    }
    check_space(env.fs, store, report);
    store_orphans(env, repo, store, worktrees, claimed, report);
    dangling_links(env, store, fix, report);
    stale_baselines(env, store, worktrees, fix, report);
}

/// The image file backing the loop device the store is mounted from, when that file was
/// deleted: the data lives on only until the store is unmounted.
fn deleted_image(fs: &dyn CowFs, root: &Path) -> Option<String> {
    let info = fs.read_file(Path::new("/proc/self/mountinfo")).ok()?;
    let info = String::from_utf8_lossy(&info);
    // The deepest mount point that holds the store: the store itself in production.
    let mut best: Option<(usize, &str)> = None;
    for line in info.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let (Some(mount), Some(source)) = (before.split(' ').nth(4), after.split(' ').nth(1))
        else {
            continue;
        };
        let mount = unescape_mount(mount);
        if root.starts_with(&mount) && best.is_none_or(|(len, _)| mount.as_os_str().len() >= len) {
            best = Some((mount.as_os_str().len(), source));
        }
    }
    let device = Path::new(best?.1).file_name()?.to_str()?;
    if !device.starts_with("loop") {
        return None;
    }
    let backing = fs
        .read_file(Path::new(&format!("/sys/block/{device}/loop/backing_file")))
        .ok()?;
    let backing = String::from_utf8_lossy(&backing);
    let image = backing.trim_end().strip_suffix(" (deleted)")?;
    Some(image.to_owned())
}

/// `/proc/self/mountinfo` writes a space, tab, newline or backslash in a path as `\040`
/// (octal).
fn unescape_mount(field: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = (bytes[i] == b'\\' && i + 3 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 4]).ok())
            .flatten()
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        if let Some(byte) = octal {
            out.push(byte);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// The filesystem holding the image grows with the store (under 10% free or under 2 GiB
/// is low), and the store itself should keep a tenth free.
fn check_space(fs: &dyn CowFs, store: &Store, report: &mut DoctorReport) {
    let host = store
        .marker
        .image
        .parent()
        .map(Path::to_path_buf)
        .and_then(|dir| fs.space(&dir).ok().map(|space| (dir, space)));
    let mut low = Vec::new();
    if let Some((dir, space)) = host {
        if space.free < LOW_FREE_BYTES || space.free.saturating_mul(10) < space.total {
            low.push((dir, space));
        }
    }
    if let Ok(space) = fs.space(&store.root) {
        if space.free.saturating_mul(10) < space.total {
            low.push((store.root.clone(), space));
        }
    }
    for (path, space) in low {
        report.findings.push(Finding::StoreLowSpace {
            path,
            free: space.free,
            total: space.total,
        });
    }
}

/// Where the store's runs in flight or crashed (a `.parhel-staging-*` dir with a
/// journal) were going to publish their worktree: a directory there is theirs to
/// repair, not an orphan.
fn claimed_destinations(fs: &dyn CowFs, store: &Store) -> Vec<PathBuf> {
    let dir = store.root.join(WORKTREES_DIR);
    fs.list_dir(&dir)
        .unwrap_or_default()
        .iter()
        .filter(|name| is_staging_name(name))
        .filter_map(|name| fs.read_file(&dir.join(name).join(JOURNAL_FILE)).ok())
        .filter_map(|bytes| Journal::decode(&bytes).ok())
        .filter_map(|journal| journal.publish_to)
        .filter(|dest| is_store_destination(dest, &dir))
        .collect()
}

/// The directories this repository has under `worktrees/<id>/` that git does not know.
/// Reported only: they may hold work, and the user decides.
fn store_orphans(
    env: &Env<'_>,
    repo: &RepoInfo,
    store: &Store,
    worktrees: &[WorktreeEntry],
    claimed: &[PathBuf],
    report: &mut DoctorReport,
) {
    let fs = env.fs;
    let dir = store.root.join(WORKTREES_DIR);
    let Ok(mut ids) = fs.list_dir(&dir) else {
        return;
    };
    ids.sort();
    let own_id = repo_id(&repo.toplevel);
    for id in ids.iter().filter(|id| is_repo_id(id)) {
        // Another repository's id (its baseline names a source this repository does not
        // have) is none of this run's business.
        let ours = *id == *own_id
            || baseline_source(fs, store, id)
                .is_some_and(|source| worktrees.iter().any(|w| w.path == source));
        if !ours {
            continue;
        }
        let id_dir = dir.join(id);
        let Ok(mut names) = fs.list_dir(&id_dir) else {
            continue;
        };
        names.sort();
        for name in names {
            let path = id_dir.join(name);
            let known = worktrees.iter().any(|w| w.path == path) || claimed.contains(&path);
            if !known && fs.is_dir_no_follow(&path).unwrap_or(false) {
                report.findings.push(Finding::StoreOrphan { path });
            }
        }
    }
}

/// The `source=` line of a baseline's `meta`.
fn baseline_source(fs: &dyn CowFs, store: &Store, id: &std::ffi::OsStr) -> Option<PathBuf> {
    let meta = fs
        .read_file(&store.root.join(BASELINES_DIR).join(id).join("meta"))
        .ok()?;
    meta.split(|&b| b == b'\n').find_map(|line| {
        let value = line.strip_prefix(b"source=")?;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(value)))
    })
}

/// Symlinks the registry says we created, that still point at their worktree's real
/// path inside the store, and the worktree is gone (`git worktree remove` given the
/// symlink removes the real worktree and leaves the link). The registry is a record,
/// not a license: a path that is no longer a symlink to exactly that target is none of
/// our business.
fn dangling_links(env: &Env<'_>, store: &Store, fix: bool, report: &mut DoctorReport) {
    let fs = env.fs;
    let Ok(registry) = fs.read_file(&store.root.join(LINKS_FILE)) else {
        return;
    };
    let worktrees = store.root.join(WORKTREES_DIR);
    let mut seen = BTreeSet::new();
    for line in registry.split(|&b| b == b'\n') {
        let Some(tab) = line.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let link = PathBuf::from(std::ffi::OsStr::from_bytes(&line[..tab]));
        let target = PathBuf::from(std::ffi::OsStr::from_bytes(&line[tab + 1..]));
        if !seen.insert((link.clone(), target.clone())) {
            continue;
        }
        let inside = target.strip_prefix(&worktrees).is_ok_and(|rest| {
            rest.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        });
        let still_ours = matches!(fs.read_link(&link), Ok(Some(t)) if t == target);
        if !inside || !still_ours || fs.exists_no_follow(&target).unwrap_or(true) {
            continue;
        }
        let finding = Finding::StoreDanglingLink {
            link: link.clone(),
            target,
        };
        report.findings.push(finding.clone());
        if fix && fs.remove_file(&link).is_ok() {
            report.fixed.push(finding);
        }
    }
}

/// Baselines whose source is no longer a git worktree. A baseline is a cache: `--fix`
/// deletes it (the next `add` mirrors the source again) unless a run holds its lock.
fn stale_baselines(
    env: &Env<'_>,
    store: &Store,
    worktrees: &[WorktreeEntry],
    fix: bool,
    report: &mut DoctorReport,
) {
    let fs = env.fs;
    let dir = store.root.join(BASELINES_DIR);
    let Ok(mut ids) = fs.list_dir(&dir) else {
        return;
    };
    ids.sort();
    for id in ids {
        let Some(source) = baseline_source(fs, store, &id) else {
            continue;
        };
        let registered = worktrees.iter().any(|w| w.path == source);
        let alive = registered
            || (fs.exists_no_follow(&source).unwrap_or(true)
                && env.git.discover(&source).map_or_else(
                    |e| e.code != ErrorCode::NotARepo,
                    |info| info.toplevel == source,
                ));
        if alive {
            continue;
        }
        let path = dir.join(&id);
        let finding = Finding::StoreStaleBaseline { path: path.clone() };
        report.findings.push(finding.clone());
        if fix
            && !fs.is_locked(&path.join("lock")).unwrap_or(true)
            && fs.remove_dir_all(&path).is_ok()
        {
            report.fixed.push(finding);
        }
    }
}

/// The `.parhel-staging-*` directory `path` lies in, if any.
fn staging_of(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    parent
        .file_name()
        .is_some_and(is_staging_name)
        .then(|| parent.to_path_buf())
}

/// Where the crashed run published `staged`, if the exclusive rename happened.
///
/// The destination is the journaled `publish_to`, or else the staging dir's sibling
/// with the staged worktree's name (staging is always created next to the
/// destination). Without a journal entry the disk must prove it: the destination's
/// `.git` file points to an admin dir whose `gitdir` still names `staged`, i.e. the
/// worktree was moved but `git worktree repair` never ran. The journal alone is not
/// trusted to be on disk after a power loss.
fn published_dest(
    env: &Env<'_>,
    staging: &Path,
    staged: &Path,
    publish_to: Option<&Path>,
) -> Option<PathBuf> {
    if env.fs.exists_no_follow(staged).unwrap_or(true) {
        return None;
    }
    let dest = match publish_to {
        Some(dest) => dest.to_path_buf(),
        None => staging.parent()?.join(staged.file_name()?),
    };
    if !env.fs.exists_no_follow(&dest).unwrap_or(false) {
        return None;
    }
    // Always ask the disk, even with a journaled destination: journals are data.
    let gitfile = env.fs.read_file(&dest.join(".git")).ok()?;
    let admin = String::from_utf8_lossy(&gitfile)
        .trim()
        .strip_prefix("gitdir:")?
        .trim()
        .to_owned();
    let backlink = env.fs.read_file(&Path::new(&admin).join("gitdir")).ok()?;
    let backlink = String::from_utf8_lossy(&backlink).trim().to_owned();
    (Path::new(&backlink) == staged.join(".git")).then_some(dest)
}

/// Finish what the crashed run was doing: point the registration at the destination,
/// and in a store lock it as `add` does (the run may have died before it got to).
fn complete_publish(
    env: &Env<'_>,
    repo: &RepoInfo,
    staging: &Path,
    staged: &Path,
    dest: &Path,
    store: Option<&Path>,
) -> bool {
    if env
        .git
        .repoint_worktree(&repo.toplevel, staged, dest)
        .is_err()
    {
        return false;
    }
    if let Some(root) = store.filter(|root| dest.starts_with(root.join(WORKTREES_DIR))) {
        // A worktree that is valid but unlocked is the lesser harm: the cleanup goes on.
        let _ = env.git.worktree_lock(
            &repo.toplevel,
            dest,
            &format!("parhel store: {}", root.display()),
        );
    }
    env.fs.remove_dir_all(staging).is_ok()
}

/// Without a trusted journal nothing proves a registered worktree is parhel's
/// (its name alone proves nothing): its files are never deleted. Only a
/// registration whose directory is gone is dropped.
fn forget_missing(env: &Env<'_>, repo: &RepoInfo, path: &Path) -> bool {
    !env.fs.exists_no_follow(path).unwrap_or(true)
        && env.git.unregister_worktree(&repo.toplevel, path).is_ok()
}

/// Walk the source (not following symlinks, skipping the root `.git`) for `.git`
/// files that point outside this repository's common dir.
fn find_foreign_gitfiles(env: &Env<'_>, repo: &RepoInfo, dir: &Path, report: &mut DoctorReport) {
    let Ok(mut names) = env.fs.list_dir(dir) else {
        return;
    };
    names.sort();
    for name in names {
        let path = dir.join(&name);
        if dir == repo.toplevel && name == ".git" {
            continue;
        }
        let is_dir = env.fs.is_dir(&path).unwrap_or(false);
        if is_dir {
            // Skip symlinked dirs: loops, or trees that are not part of the source.
            if env.fs.canonicalize(&path).ok().as_deref() != Some(path.as_path()) {
                continue;
            }
            find_foreign_gitfiles(env, repo, &path, report);
        } else if name == ".git" {
            let Ok(bytes) = env.fs.read_file(&path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let Some(target) = text.trim().strip_prefix("gitdir:") else {
                continue;
            };
            let target = paths::resolve_destination(dir, Path::new(target.trim()));
            let own = target.starts_with(&repo.common_dir)
                || env
                    .fs
                    .canonicalize(&target)
                    .is_ok_and(|t| t.starts_with(&repo.common_dir));
            if !own {
                report.findings.push(Finding::ForeignGitfile { path });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{Call, SHA_A, World, btrfs, p};
    use crate::app::rollback::{JOURNAL_FILE, Journal};
    use crate::git::parse::WorktreeEntry;
    use crate::ports::{CowFs, Space, VolumeInfo};
    use rstest::rstest;
    use std::ffi::OsString;
    use std::path::Path;

    fn run(w: &World, fix: bool) -> DoctorReport {
        let env = Env {
            git: w,
            fs: w,
            cwd: Path::new("/w/repo"),
            interrupted: &w.interrupted,
            store: None,
        };
        doctor(&env, &DoctorRequest { from: p("."), fix }).unwrap()
    }

    /// Simulates a run killed after registering `/w/.parhel-staging-dead/feat`.
    fn crashed_world() -> World {
        let mut w = World::default();
        let staging = p("/w/.parhel-staging-dead");
        w.tree
            .borrow_mut()
            .get_mut(&p("/w"))
            .unwrap()
            .push(OsString::from(".parhel-staging-dead"));
        w.tree.borrow_mut().insert(
            staging.clone(),
            vec![OsString::from("feat"), OsString::from(JOURNAL_FILE)],
        );
        w.tree
            .borrow_mut()
            .insert(staging.join("feat"), vec![OsString::from(".git")]);
        let journal = Journal {
            repo: p("/w/repo"),
            worktree: Some(staging.join("feat")),
            created_branch: Some("feat".into()),
            publish_to: None,
            branch_start: Some(SHA_A.into()),
            store_link: None,
        };
        w.files
            .borrow_mut()
            .insert(staging.join(JOURNAL_FILE), journal.encode());
        w.files.borrow_mut().insert(
            staging.join("feat/.git"),
            b"gitdir: /w/repo/.git/worktrees/feat\n".to_vec(),
        );
        w.worktrees.push(WorktreeEntry {
            path: staging.join("feat"),
            head: Some(SHA_A.into()),
            branch: Some("refs/heads/feat".into()),
            ..WorktreeEntry::default()
        });
        w.branches.borrow_mut().insert("feat".into());
        w
    }

    // U-DOC-01
    #[test]
    fn healthy_repo_has_no_findings() {
        let report = run(&World::default(), false);
        assert_eq!(report, DoctorReport::default());
        assert!(report.healthy());
    }

    // U-DOC-02
    #[test]
    fn reports_source_without_cow() {
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
        let report = run(&w, false);
        assert_eq!(
            report.findings,
            vec![Finding::NotCow {
                path: p("/w/repo"),
                fs_type: "hfs".into()
            }]
        );
        assert!(!report.healthy());
    }

    // U-DOC-03
    #[test]
    fn reports_crash_leftovers_without_touching_them() {
        let w = crashed_world();
        let report = run(&w, false);
        assert!(report.findings.contains(&Finding::OrphanStaging {
            path: p("/w/.parhel-staging-dead")
        }));
        assert!(report.findings.contains(&Finding::OrphanRegistration {
            path: p("/w/.parhel-staging-dead/feat")
        }));
        assert_eq!(report.fixed, vec![]);
        assert!(
            w.calls().is_empty(),
            "read-only without --fix: {:#?}",
            w.calls()
        );
    }

    // U-DOC-04
    #[test]
    fn fix_undoes_crash_using_journal() {
        let w = crashed_world();
        let report = run(&w, true);
        assert!(report.healthy(), "{report:#?}");
        assert_eq!(
            w.count(|c| *c
                == Call::RemoveForce {
                    path: p("/w/.parhel-staging-dead/feat")
                }),
            1
        );
        assert_eq!(w.count(|c| *c == Call::BranchDelete("feat".into())), 1);
        assert_eq!(
            w.count(|c| *c == Call::RemoveDirAll(p("/w/.parhel-staging-dead"))),
            1
        );
    }

    // U-DOC-05: no journal (crash before it was written) -> only the dir is removed.
    #[test]
    fn fix_without_journal_removes_only_staging_dir() {
        let w = World::default();
        w.tree
            .borrow_mut()
            .get_mut(&p("/w"))
            .unwrap()
            .push(OsString::from(".parhel-staging-empty"));
        w.tree
            .borrow_mut()
            .insert(p("/w/.parhel-staging-empty"), vec![]);
        let report = run(&w, true);
        assert!(report.healthy());
        assert_eq!(
            w.calls(),
            vec![Call::RemoveDirAll(p("/w/.parhel-staging-empty"))]
        );
    }

    // U-DOC-06
    #[test]
    fn reports_foreign_gitfile_but_never_fixes_it() {
        let w = World::default();
        w.tree
            .borrow_mut()
            .get_mut(&p("/w/repo/node_modules"))
            .unwrap()
            .push(OsString::from("vendored"));
        w.tree.borrow_mut().insert(
            p("/w/repo/node_modules/vendored"),
            vec![OsString::from(".git")],
        );
        w.files.borrow_mut().insert(
            p("/w/repo/node_modules/vendored/.git"),
            b"gitdir: /elsewhere/other/.git/worktrees/vendored\n".to_vec(),
        );
        let report = run(&w, true);
        let finding = Finding::ForeignGitfile {
            path: p("/w/repo/node_modules/vendored/.git"),
        };
        assert!(report.findings.contains(&finding));
        assert!(!report.fixed.contains(&finding));
        assert!(!report.healthy());
        assert!(!w.touched(Path::new("/w/repo/node_modules")));
    }

    // U-DOC-07: a nested worktree of this same repo is not foreign.
    #[test]
    fn own_nested_worktree_gitfile_is_not_foreign() {
        let mut w = World::default();
        w.tree
            .borrow_mut()
            .get_mut(&p("/w/repo"))
            .unwrap()
            .push(OsString::from("wt"));
        w.tree
            .borrow_mut()
            .insert(p("/w/repo/wt"), vec![OsString::from(".git")]);
        w.files.borrow_mut().insert(
            p("/w/repo/wt/.git"),
            b"gitdir: /w/repo/.git/worktrees/wt\n".to_vec(),
        );
        w.worktrees.push(WorktreeEntry {
            path: p("/w/repo/wt"),
            ..WorktreeEntry::default()
        });
        assert_eq!(run(&w, false).findings, vec![]);
    }

    // U-DOC-08: a staging dir whose journal is locked belongs to a live run.
    #[test]
    fn live_run_is_reported_in_progress_and_never_fixed() {
        let w = crashed_world();
        w.locked
            .borrow_mut()
            .insert(p("/w/.parhel-staging-dead").join(JOURNAL_FILE));
        let report = run(&w, true);
        assert!(report.findings.contains(&Finding::InProgress {
            path: p("/w/.parhel-staging-dead")
        }));
        assert!(
            !report.findings.iter().any(|f| matches!(
                f,
                Finding::OrphanStaging { .. } | Finding::OrphanRegistration { .. }
            )),
            "{report:#?}"
        );
        assert!(report.healthy());
        assert_eq!(w.calls(), vec![], "live run must not be touched");
    }

    // U-DOC-09: unrelated prunable worktrees (not in a staging dir) are not parhel's business.
    #[test]
    fn foreign_prunable_registration_is_not_reported() {
        let mut w = World::default();
        w.worktrees.push(WorktreeEntry {
            path: p("/w/deleted-by-user"),
            prunable: Some("gitdir file points to non-existent location".into()),
            ..WorktreeEntry::default()
        });
        assert_eq!(run(&w, true).findings, vec![]);
        assert_eq!(w.calls(), vec![]);
    }

    /// `crashed_world`, but the run had recorded its destination before dying.
    fn crashed_with_publish_intent(renamed: bool) -> World {
        let w = crashed_world();
        let staging = p("/w/.parhel-staging-dead");
        let journal = Journal {
            repo: p("/w/repo"),
            worktree: Some(staging.join("feat")),
            created_branch: Some("feat".into()),
            publish_to: Some(p("/w/feat")),
            branch_start: Some(SHA_A.into()),
            store_link: None,
        };
        w.files
            .borrow_mut()
            .insert(staging.join(JOURNAL_FILE), journal.encode());
        if renamed {
            w.files.borrow_mut().insert(
                p("/w/feat/.git"),
                b"gitdir: /w/repo/.git/worktrees/feat\n".to_vec(),
            );
            w.files.borrow_mut().insert(
                p("/w/repo/.git/worktrees/feat/gitdir"),
                b"/w/.parhel-staging-dead/feat/.git\n".to_vec(),
            );
            w.tree.borrow_mut().remove(&staging.join("feat"));
            w.tree
                .borrow_mut()
                .get_mut(&staging)
                .unwrap()
                .retain(|n| n != "feat");
            w.tree
                .borrow_mut()
                .insert(p("/w/feat"), vec![OsString::from(".git")]);
            w.tree
                .borrow_mut()
                .get_mut(&p("/w"))
                .unwrap()
                .push(OsString::from("feat"));
        }
        w
    }

    // U-DOC-10: killed between the publishing rename and `worktree repair`: the
    // destination is a valid worktree and must be repaired, not rolled back.
    #[test]
    fn fix_repairs_worktree_published_before_crash() {
        let w = crashed_with_publish_intent(true);
        let report = run(&w, true);
        assert!(report.healthy(), "{report:#?}");
        assert_eq!(
            w.calls(),
            vec![
                Call::Repair { path: p("/w/feat") },
                Call::RemoveDirAll(p("/w/.parhel-staging-dead")),
            ],
            "repair, never remove the worktree or its branch"
        );
    }

    // U-DOC-11: intent recorded but the rename never happened -> plain rollback.
    #[test]
    fn fix_rolls_back_when_rename_did_not_happen() {
        let w = crashed_with_publish_intent(false);
        let report = run(&w, true);
        assert!(report.healthy(), "{report:#?}");
        assert_eq!(
            w.count(|c| *c
                == Call::RemoveForce {
                    path: p("/w/.parhel-staging-dead/feat")
                }),
            1
        );
        assert_eq!(w.count(|c| *c == Call::BranchDelete("feat".into())), 1);
        assert_eq!(w.count(|c| matches!(c, Call::Repair { .. })), 0);
    }

    /// Published worktree at `/w/feat` whose registration still points into the
    /// staging dir (killed before `worktree repair`), with the given journal state.
    fn published_before_repair(journal: Option<Journal>) -> World {
        let w = crashed_world();
        let staging = p("/w/.parhel-staging-dead");
        w.tree.borrow_mut().remove(&staging.join("feat"));
        w.tree
            .borrow_mut()
            .get_mut(&staging)
            .unwrap()
            .retain(|n| n != "feat");
        w.tree
            .borrow_mut()
            .insert(p("/w/feat"), vec![OsString::from(".git")]);
        w.tree
            .borrow_mut()
            .get_mut(&p("/w"))
            .unwrap()
            .push(OsString::from("feat"));
        w.files.borrow_mut().remove(&staging.join("feat/.git"));
        w.files.borrow_mut().insert(
            p("/w/feat/.git"),
            b"gitdir: /w/repo/.git/worktrees/feat\n".to_vec(),
        );
        w.files.borrow_mut().insert(
            p("/w/repo/.git/worktrees/feat/gitdir"),
            b"/w/.parhel-staging-dead/feat/.git\n".to_vec(),
        );
        if let Some(j) = journal {
            w.files
                .borrow_mut()
                .insert(staging.join(JOURNAL_FILE), j.encode());
        } else {
            w.files.borrow_mut().remove(&staging.join(JOURNAL_FILE));
            w.tree
                .borrow_mut()
                .get_mut(&staging)
                .unwrap()
                .retain(|n| n != JOURNAL_FILE);
        }
        w
    }

    fn assert_repaired_not_removed(w: &World) {
        assert_eq!(
            w.calls(),
            vec![
                Call::Repair { path: p("/w/feat") },
                Call::RemoveDirAll(p("/w/.parhel-staging-dead")),
            ],
            "the published worktree must be repaired, never unregistered"
        );
    }

    // U-DOC-12: journal written before the destination was recorded (e.g. the
    // last journal write never reached the disk): detect publication on disk.
    #[test]
    fn detects_publication_without_journal_publish_to() {
        let old = Journal {
            repo: p("/w/repo"),
            worktree: Some(p("/w/.parhel-staging-dead/feat")),
            created_branch: Some("feat".into()),
            publish_to: None,
            branch_start: Some(SHA_A.into()),
            store_link: None,
        };
        let w = published_before_repair(Some(old));
        assert!(run(&w, true).healthy());
        assert_repaired_not_removed(&w);
    }

    // U-DOC-13: no readable journal at all.
    #[test]
    fn detects_publication_without_any_journal() {
        let w = published_before_repair(None);
        assert!(run(&w, true).healthy());
        assert_repaired_not_removed(&w);
    }

    // U-DOC-14: without a journal a registration is parhel's only by its name,
    // which proves nothing: its files are never deleted; it is dropped (alone,
    // never with a global prune) only once its directory is gone.
    #[test]
    fn journal_less_registration_is_only_dropped_when_missing() {
        let w = crashed_world();
        let staging = p("/w/.parhel-staging-dead");
        w.files.borrow_mut().remove(&staging.join(JOURNAL_FILE));
        w.tree
            .borrow_mut()
            .get_mut(&staging)
            .unwrap()
            .retain(|n| n != JOURNAL_FILE);
        let unregister = Call::Unregister {
            path: staging.join("feat"),
        };
        assert!(!run(&w, true).healthy(), "present: kept and still reported");
        assert!(
            w.calls().iter().all(|c| matches!(c, Call::Lock(_))),
            "{:#?}",
            w.calls()
        );
        w.tree.borrow_mut().remove(&staging.join("feat"));
        w.tree
            .borrow_mut()
            .get_mut(&staging)
            .unwrap()
            .retain(|n| n != "feat");
        run(&w, true);
        assert_eq!(w.count(|c| *c == unregister), 1);
    }

    // U-DOC-15: the staging dir of a crashed run may sit next to a destination far
    // from the source; it is found through the registration.
    #[test]
    fn finds_staging_next_to_a_distant_destination() {
        let mut w = World::default();
        let staging = p("/x/.parhel-staging-far");
        w.tree
            .borrow_mut()
            .insert(p("/x"), vec![OsString::from(".parhel-staging-far")]);
        w.tree
            .borrow_mut()
            .insert(staging.clone(), vec![OsString::from(JOURNAL_FILE)]);
        let journal = Journal {
            repo: p("/w/repo"),
            worktree: Some(staging.join("feat")),
            created_branch: None,
            publish_to: Some(p("/x/feat")),
            branch_start: None,
            store_link: None,
        };
        w.files
            .borrow_mut()
            .insert(staging.join(JOURNAL_FILE), journal.encode());
        w.tree
            .borrow_mut()
            .insert(p("/x/feat"), vec![OsString::from(".git")]);
        w.files.borrow_mut().insert(
            p("/x/feat/.git"),
            b"gitdir: /w/repo/.git/worktrees/feat\n".to_vec(),
        );
        w.files.borrow_mut().insert(
            p("/w/repo/.git/worktrees/feat/gitdir"),
            b"/x/.parhel-staging-far/feat/.git\n".to_vec(),
        );
        w.tree
            .borrow_mut()
            .get_mut(&p("/x"))
            .unwrap()
            .push(OsString::from("feat"));
        w.worktrees.push(WorktreeEntry {
            path: staging.join("feat"),
            ..WorktreeEntry::default()
        });
        let report = run(&w, true);
        assert!(report.findings.contains(&Finding::OrphanStaging {
            path: staging.clone()
        }));
        assert!(report.healthy(), "{report:#?}");
        assert_eq!(
            w.calls(),
            vec![
                Call::Repair { path: p("/x/feat") },
                Call::RemoveDirAll(staging)
            ]
        );
    }

    // U-DOC-16: staging dir deleted entirely, worktree published: repair it.
    #[test]
    fn repairs_publication_whose_staging_dir_is_gone() {
        let w = published_before_repair(None);
        let staging = p("/w/.parhel-staging-dead");
        w.tree.borrow_mut().remove(&staging);
        w.tree
            .borrow_mut()
            .get_mut(&p("/w"))
            .unwrap()
            .retain(|n| n != ".parhel-staging-dead");
        assert!(run(&w, true).healthy());
        assert_eq!(w.calls(), vec![Call::Repair { path: p("/w/feat") }]);
    }

    // U-DOC-17: without a journal nothing proves parhel owns the folder's
    // contents: a non-empty lookalike is reported, never deleted.
    #[test]
    fn never_deletes_a_non_empty_folder_without_journal() {
        let w = World::default();
        let lookalike = p("/w/.parhel-staging-mine");
        w.tree
            .borrow_mut()
            .get_mut(&p("/w"))
            .unwrap()
            .push(OsString::from(".parhel-staging-mine"));
        w.tree
            .borrow_mut()
            .insert(lookalike.clone(), vec![OsString::from("important.txt")]);
        w.files
            .borrow_mut()
            .insert(lookalike.join("important.txt"), b"mine".to_vec());
        let report = run(&w, true);
        assert!(report.findings.contains(&Finding::OrphanStaging {
            path: lookalike.clone()
        }));
        assert!(
            !report
                .fixed
                .contains(&Finding::OrphanStaging { path: lookalike })
        );
        assert_eq!(w.calls(), vec![]);
    }

    /// A staging dir holding a journal written by someone else, pointing at
    /// arbitrary places.
    fn forged(journal: &Journal) -> World {
        let w = World::default();
        let staging = p("/w/.parhel-staging-evil");
        w.tree
            .borrow_mut()
            .get_mut(&p("/w"))
            .unwrap()
            .push(OsString::from(".parhel-staging-evil"));
        w.tree
            .borrow_mut()
            .insert(staging.clone(), vec![OsString::from(JOURNAL_FILE)]);
        w.files
            .borrow_mut()
            .insert(staging.join(JOURNAL_FILE), journal.encode());
        w.tree
            .borrow_mut()
            .insert(p("/w/victim"), vec![OsString::from("data")]);
        w.branches.borrow_mut().insert("victim-branch".into());
        w
    }

    // U-DOC-18: a journal naming a worktree outside its own staging dir is not
    // trusted: nothing outside the staging dir is touched.
    #[test]
    fn forged_journal_cannot_target_paths_outside_its_staging_dir() {
        let w = forged(&Journal {
            repo: p("/w/repo"),
            worktree: Some(p("/w/victim")),
            created_branch: Some("victim-branch".into()),
            publish_to: Some(p("/w/victim")),
            branch_start: Some(SHA_A.into()),
            store_link: None,
        });
        run(&w, true);
        assert!(!w.touched(Path::new("/w/victim")), "{:#?}", w.calls());
        assert_eq!(w.count(|c| matches!(c, Call::BranchDelete(_))), 0);
    }

    // U-DOC-19: the journal's repository is ignored; git only ever runs against the
    // repository doctor was started in, and a branch is deleted only if the staged
    // registration really had it checked out.
    #[test]
    fn forged_journal_cannot_delete_branches_it_did_not_check_out() {
        let w = forged(&Journal {
            repo: p("/elsewhere/other-repo"),
            worktree: Some(p("/w/.parhel-staging-evil/feat")),
            created_branch: Some("victim-branch".into()),
            publish_to: None,
            branch_start: Some(SHA_A.into()),
            store_link: None,
        });
        run(&w, true);
        assert_eq!(
            w.count(|c| matches!(c, Call::BranchDelete(_))),
            0,
            "{:#?}",
            w.calls()
        );
    }

    // ------------------------------------------------------------ the store

    const ID: &str = "7005185596c2e32bd0688ebe72725f92";

    fn real() -> PathBuf {
        p(&format!("/store/worktrees/{ID}/feat"))
    }

    fn baseline_dir() -> PathBuf {
        p(&format!("/store/baselines/{ID}"))
    }

    const MOUNTINFO: &str = "36 25 0:32 / /store rw,noatime shared:1 - btrfs /dev/loop3 rw,ssd\n";

    fn run_store(w: &World, fix: bool) -> DoctorReport {
        run_with(w, fix, Some(Path::new("/store")))
    }

    fn run_with(w: &World, fix: bool, store: Option<&Path>) -> DoctorReport {
        let env = Env {
            git: w,
            fs: w,
            cwd: Path::new("/w/repo"),
            interrupted: &w.interrupted,
            store,
        };
        doctor(&env, &DoctorRequest { from: p("."), fix }).unwrap()
    }

    /// A healthy store with one worktree of `/w/repo` at its real path, reached through
    /// the symlink `/w/feat` (recorded in the registry), the baseline of `/w/repo`,
    /// and a loop device backed by a file that exists.
    fn store_world() -> World {
        let mut w = World::default().with_store();
        // The source is on a filesystem with copy-on-write: `not-cow` is not what
        // these tests are about (see U-STO-84 for a source on ext4).
        w.volumes = vec![(p("/"), btrfs(3)), (p("/store"), btrfs(2))];
        w.put_dir(&real());
        w.worktrees.push(WorktreeEntry {
            path: real(),
            head: Some(SHA_A.into()),
            branch: Some("refs/heads/feat".into()),
            ..WorktreeEntry::default()
        });
        w.put_link(Path::new("/w/feat"), &real());
        let registry = format!("/w/feat\t{}\n", real().display());
        w.files
            .borrow_mut()
            .insert(p("/store/links"), registry.into_bytes());
        w.put_dir(&baseline_dir());
        w.files
            .borrow_mut()
            .insert(baseline_dir().join("meta"), b"source=/w/repo\n".to_vec());
        w.files
            .borrow_mut()
            .insert(p("/proc/self/mountinfo"), MOUNTINFO.as_bytes().to_vec());
        w.files.borrow_mut().insert(
            p("/sys/block/loop3/loop/backing_file"),
            b"/var/lib/parhel/alice.img\n".to_vec(),
        );
        w
    }

    fn store_findings(report: &DoctorReport) -> Vec<&Finding> {
        report
            .findings
            .iter()
            .filter(|f| f.code().starts_with("store-"))
            .collect()
    }

    // U-STO-70: a healthy store is silent.
    #[test]
    fn healthy_store_has_no_findings() {
        let w = store_world();
        let report = run_store(&w, false);
        assert_eq!(report, DoctorReport::default());
        assert!(w.calls().is_empty(), "{:#?}", w.calls());
    }

    // U-STO-71: no store configured: no store findings, whatever lies around, and no
    // store files are even looked at.
    #[test]
    fn no_store_means_no_store_findings() {
        let w = store_world();
        w.files.borrow_mut().remove(&p("/store/.parhel-store"));
        assert_eq!(run_with(&w, false, None), DoctorReport::default());
        // Configured path that does not exist: the user never ran `setup`.
        let w = World::default();
        assert_eq!(
            run_with(&w, false, Some(Path::new("/store"))),
            DoctorReport::default()
        );
    }

    // U-STO-72: a store that cannot be used is one finding, with the reason; the
    // deeper checks (which need the store) are skipped instead of piling up noise.
    #[rstest]
    #[case::marker_missing(|w: &World| { w.files.borrow_mut().remove(&p("/store/.parhel-store")); }, ".parhel-store")]
    #[case::not_writable(|w: &World| { w.unwritable.borrow_mut().insert(p("/store")); }, "writable")]
    fn unavailable_store_is_one_finding(#[case] break_it: fn(&World), #[case] reason: &str) {
        let w = store_world();
        break_it(&w);
        // Things that would be findings if the store were readable.
        w.files
            .borrow_mut()
            .insert(baseline_dir().join("meta"), b"source=/gone\n".to_vec());
        w.put_dir(&p(&format!("/store/worktrees/{ID}/ghost")));
        let report = run_store(&w, false);
        // (`store-worktrees-unreachable` is the one more thing that is reported with the
        // store down - the registrations of its worktrees; see U-STO-93.)
        let found: Vec<_> = store_findings(&report)
            .into_iter()
            .filter(|f| f.code() != "store-worktrees-unreachable")
            .collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        match found[0] {
            Finding::StoreUnavailable { path, reason: why } => {
                assert_eq!(path, &p("/store"));
                assert!(why.contains(reason), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!report.healthy());
        assert!(!found[0].fixable());
    }

    // U-STO-73: the backing image was deleted while the store is still mounted: the
    // data lives on only until unmount, so say it loudly.
    #[test]
    fn deleted_backing_image_is_reported() {
        let w = store_world();
        w.files.borrow_mut().insert(
            p("/sys/block/loop3/loop/backing_file"),
            b"/var/lib/parhel/alice.img (deleted)\n".to_vec(),
        );
        let report = run_store(&w, false);
        assert_eq!(
            store_findings(&report),
            vec![&Finding::StoreImageDeleted {
                path: p("/store"),
                image: "/var/lib/parhel/alice.img".into(),
            }]
        );
        assert!(!report.healthy());
    }

    const GIB: u64 = 1 << 30;

    // U-STO-74: low space on the host filesystem (where the image grows) or in the
    // store: under 10% free, or under 2 GiB.
    #[rstest]
    #[case::host_under_ten_percent("/var/lib/parhel", 1000 * GIB, 90 * GIB, true)]
    #[case::host_fine("/var/lib/parhel", 1000 * GIB, 120 * GIB, false)]
    #[case::host_under_two_gib("/var/lib/parhel", 20 * GIB, GIB + GIB / 2, true)]
    #[case::host_exactly_two_gib_is_fine("/var/lib/parhel", 20 * GIB, 2 * GIB, false)]
    #[case::store_under_ten_percent("/store", 100 * GIB, 9 * GIB, true)]
    #[case::store_fine("/store", 100 * GIB, 50 * GIB, false)]
    fn low_space_is_reported(
        #[case] at: &str,
        #[case] total: u64,
        #[case] free: u64,
        #[case] reported: bool,
    ) {
        let w = store_world();
        w.spaces.borrow_mut().push((p(at), Space { total, free }));
        let report = run_store(&w, false);
        let want: Vec<Finding> = if reported {
            vec![Finding::StoreLowSpace {
                path: p(at),
                free,
                total,
            }]
        } else {
            vec![]
        };
        assert_eq!(store_findings(&report), want.iter().collect::<Vec<_>>());
    }

    // U-STO-75: a directory under worktrees/<id>/ that git does not know is reported,
    // and nothing is ever deleted for it.
    #[test]
    fn unregistered_worktree_directory_is_an_orphan_and_is_kept() {
        let w = store_world();
        let ghost = p(&format!("/store/worktrees/{ID}/ghost"));
        w.put_dir(&ghost);
        w.put_file(&ghost.join("work.txt"), 10, 1);
        let report = run_store(&w, true);
        assert_eq!(
            store_findings(&report),
            vec![&Finding::StoreOrphan {
                path: ghost.clone()
            }]
        );
        assert!(report.fixed.is_empty(), "{report:#?}");
        assert!(!report.healthy());
        assert!(!w.touched(&ghost), "never deleted: {:#?}", w.calls());
    }

    // U-STO-76: a link whose worktree is gone is dangling; --fix removes the link.
    #[test]
    fn dangling_link_is_reported_and_fixed() {
        let mut w = store_world();
        w.tree.borrow_mut().remove(&real());
        w.tree
            .borrow_mut()
            .get_mut(real().parent().unwrap())
            .unwrap()
            .clear();
        w.worktrees.clear();
        w.worktrees.push(WorktreeEntry {
            path: p("/w/repo"),
            head: Some(SHA_A.into()),
            branch: Some("refs/heads/main".into()),
            ..WorktreeEntry::default()
        });
        let finding = Finding::StoreDanglingLink {
            link: p("/w/feat"),
            target: real(),
        };
        let report = run_store(&w, false);
        assert_eq!(store_findings(&report), vec![&finding]);
        assert!(finding.fixable());
        assert_eq!(w.calls(), vec![]);

        let report = run_store(&w, true);
        assert_eq!(report.fixed, vec![finding]);
        assert!(report.healthy());
        assert_eq!(w.calls(), vec![Call::RemoveFile(p("/w/feat"))]);
    }

    // U-STO-77: the registry is a record of what we created, not a license: a path
    // that is no longer a symlink to exactly that target is left alone and unreported.
    #[rstest]
    #[case::retargeted(|w: &World| { w.links.borrow_mut().insert(p("/w/feat"), p("/store/worktrees/other/x")); })]
    #[case::replaced_by_a_directory(|w: &World| { w.links.borrow_mut().remove(&p("/w/feat")); w.put_dir(&p("/w/feat")); })]
    #[case::gone(|w: &World| { w.links.borrow_mut().remove(&p("/w/feat")); })]
    fn registry_entries_that_are_not_our_link_are_ignored(#[case] change: fn(&World)) {
        let w = store_world();
        w.tree.borrow_mut().remove(&real());
        change(&w);
        let report = run_store(&w, true);
        assert!(
            !store_findings(&report)
                .iter()
                .any(|f| matches!(f, Finding::StoreDanglingLink { .. })),
            "{report:#?}"
        );
        assert_eq!(
            w.count(|c| matches!(c, Call::RemoveFile(_) | Call::RemoveDirAll(_))),
            0
        );
    }

    // U-STO-78: a baseline whose source is not a worktree any more is stale; --fix
    // deletes that baseline (it is a cache), and only it.
    #[test]
    fn stale_baseline_is_reported_and_fixed() {
        let w = store_world();
        w.files
            .borrow_mut()
            .insert(baseline_dir().join("meta"), b"source=/gone/repo\n".to_vec());
        let finding = Finding::StoreStaleBaseline {
            path: baseline_dir(),
        };
        let report = run_store(&w, false);
        assert_eq!(store_findings(&report), vec![&finding]);
        assert_eq!(w.calls(), vec![]);

        let report = run_store(&w, true);
        assert_eq!(report.fixed, vec![finding]);
        assert_eq!(w.calls(), vec![Call::RemoveDirAll(baseline_dir())]);
    }

    // U-STO-79: a baseline whose source is still a registered worktree (the main one
    // or a linked one) is not stale.
    #[test]
    fn baseline_of_a_linked_worktree_is_not_stale() {
        let w = store_world();
        w.files.borrow_mut().insert(
            baseline_dir().join("meta"),
            format!("source={}\n", real().display()).into_bytes(),
        );
        assert_eq!(store_findings(&run_store(&w, true)), Vec::<&Finding>::new());
        assert_eq!(w.calls(), vec![]);
    }

    // U-STO-80: whatever --fix repairs, no worktree directory is ever deleted.
    #[test]
    fn fix_never_deletes_a_worktree_directory() {
        let w = store_world();
        let ghost = p(&format!("/store/worktrees/{ID}/ghost"));
        w.put_dir(&ghost);
        w.files
            .borrow_mut()
            .insert(baseline_dir().join("meta"), b"source=/gone/repo\n".to_vec());
        w.links
            .borrow_mut()
            .insert(p("/w/dangling"), p("/store/worktrees/x/y"));
        let registry = format!(
            "/w/feat\t{}\n/w/dangling\t/store/worktrees/x/y\n",
            real().display()
        );
        w.files
            .borrow_mut()
            .insert(p("/store/links"), registry.into_bytes());
        run_store(&w, true);
        assert!(
            !w.touched(Path::new("/store/worktrees")),
            "{:#?}",
            w.calls()
        );
        assert!(!w.touched(&real()));
        assert_eq!(w.count(|c| *c == Call::RemoveFile(p("/w/dangling"))), 1);
        assert_eq!(w.count(|c| *c == Call::RemoveDirAll(baseline_dir())), 1);
    }

    /// A store run killed before (or after) its publishing rename.
    fn crashed_store_run(renamed: bool, link: Option<(&str, &str)>) -> World {
        let w = store_world();
        // The finished worktree of `store_world` is not part of this story.
        w.tree.borrow_mut().remove(&real());
        w.links.borrow_mut().clear();
        let staging = p("/store/worktrees/.parhel-staging-dead");
        let staged = staging.join("feat");
        w.put_dir(&staging);
        w.put_file(&staging.join(JOURNAL_FILE), 1, 1);
        let journal = Journal {
            repo: p("/w/repo"),
            worktree: Some(staged.clone()),
            created_branch: Some("feat".into()),
            publish_to: Some(real()),
            branch_start: Some(SHA_A.into()),
            store_link: link.map(|(path, target)| crate::app::rollback::StoreLink {
                path: p(path),
                target: p(target),
            }),
        };
        w.files
            .borrow_mut()
            .insert(staging.join(JOURNAL_FILE), journal.encode());
        w.branches.borrow_mut().insert("feat".into());
        if let Some((path, target)) = link {
            w.links.borrow_mut().insert(p(path), p(target));
        }
        let gitfile = b"gitdir: /w/repo/.git/worktrees/feat\n".to_vec();
        let backlink = format!("{}/.git\n", staged.display()).into_bytes();
        w.files
            .borrow_mut()
            .insert(p("/w/repo/.git/worktrees/feat/gitdir"), backlink);
        if renamed {
            w.put_dir(&real());
            w.files.borrow_mut().insert(real().join(".git"), gitfile);
        } else {
            w.put_dir(&staged);
            w.files.borrow_mut().insert(staged.join(".git"), gitfile);
        }
        let mut w = w;
        w.worktrees.retain(|e| e.path != real());
        w.worktrees.push(WorktreeEntry {
            path: staged,
            head: Some(SHA_A.into()),
            branch: Some("refs/heads/feat".into()),
            ..WorktreeEntry::default()
        });
        w
    }

    // U-STO-81: killed before the rename: the symlink this run created goes with the
    // rest, because it still points where the journal says.
    #[test]
    fn fix_removes_the_symlink_of_a_crashed_store_run() {
        let w = crashed_store_run(false, Some(("/w/feat", real().to_str().unwrap())));
        let report = run_store(&w, true);
        assert!(report.healthy(), "{report:#?}");
        assert_eq!(
            w.count(|c| *c == Call::RemoveFile(p("/w/feat"))),
            1,
            "{:#?}",
            w.calls()
        );
        assert_eq!(
            w.count(|c| *c
                == Call::RemoveForce {
                    path: p("/store/worktrees/.parhel-staging-dead/feat")
                }),
            1
        );
        assert_eq!(w.count(|c| *c == Call::BranchDelete("feat".into())), 1);
        assert_eq!(
            w.calls().last(),
            Some(&Call::RemoveDirAll(p(
                "/store/worktrees/.parhel-staging-dead"
            )))
        );
    }

    // U-STO-82: a journal is data in a folder anyone could write: it cannot make
    // doctor remove a symlink that does not point into this store, or that someone
    // retargeted.
    #[rstest]
    #[case::points_outside_the_store("/w/precious", "/w/other")]
    #[case::points_into_the_store_elsewhere("/w/precious", "/store/baselines/x")]
    fn fix_ignores_forged_links(#[case] link: &str, #[case] target: &str) {
        let w = crashed_store_run(false, Some((link, target)));
        run_store(&w, true);
        assert_eq!(
            w.count(|c| *c == Call::RemoveFile(p(link))),
            0,
            "{:#?}",
            w.calls()
        );
    }

    #[test]
    fn fix_keeps_a_retargeted_link_of_a_crashed_run() {
        let w = crashed_store_run(false, Some(("/w/feat", real().to_str().unwrap())));
        w.links
            .borrow_mut()
            .insert(p("/w/feat"), p("/somewhere/else"));
        run_store(&w, true);
        assert_eq!(
            w.count(|c| *c == Call::RemoveFile(p("/w/feat"))),
            0,
            "{:#?}",
            w.calls()
        );
    }

    // U-STO-83: killed after the rename, before the registration was repaired: the
    // worktree at its real path is repaired and the symlink stays.
    #[test]
    fn fix_repairs_a_store_worktree_published_before_the_crash() {
        let w = crashed_store_run(true, Some(("/w/feat", real().to_str().unwrap())));
        let report = run_store(&w, true);
        assert!(report.healthy(), "{report:#?}");
        assert_eq!(
            w.calls(),
            vec![
                Call::Repair { path: real() },
                Call::WorktreeLock {
                    path: real(),
                    reason: "parhel store: /store".into(),
                },
                Call::RemoveDirAll(p("/store/worktrees/.parhel-staging-dead")),
            ],
            "repair at the real path and lock it as `add` does; keep the worktree, its \
             branch and the symlink"
        );
    }

    /// `store_world` with the store gone: unmounted (the marker, which lives on the
    /// image, is out of reach) and the worktree directories with it. Git still lists
    /// the registrations.
    fn store_down(locked: Option<&str>) -> World {
        let mut w = store_world();
        w.files.borrow_mut().remove(&p("/store/.parhel-store"));
        w.tree.borrow_mut().remove(&real());
        for entry in &mut w.worktrees {
            if entry.path == real() {
                entry.locked = locked.map(str::to_owned);
            }
        }
        w
    }

    fn unreachable_findings(report: &DoctorReport) -> Vec<&Finding> {
        report
            .findings
            .iter()
            .filter(|f| f.code() == "store-worktrees-unreachable")
            .collect()
    }

    // U-STO-93: the store is down and git lists worktrees that live in it: say so,
    // naming those a `git worktree prune` would forget (not locked) apart from the
    // others. Reported only: doctor changes nothing about them.
    #[rstest]
    #[case::locked(Some("parhel store: /store"), vec![])]
    #[case::locked_without_a_reason(Some(""), vec![])]
    #[case::not_locked(None, vec![real()])]
    fn worktrees_of_an_unreachable_store_are_reported(
        #[case] locked: Option<&str>,
        #[case] unlocked: Vec<PathBuf>,
    ) {
        for fix in [false, true] {
            let w = store_down(locked);
            let report = run_store(&w, fix);
            assert_eq!(
                unreachable_findings(&report),
                vec![&Finding::StoreWorktreesUnreachable {
                    root: p("/store"),
                    worktrees: vec![real()],
                    unlocked: unlocked.clone(),
                }],
                "{report:#?}"
            );
            assert!(!report.healthy());
            assert!(!unreachable_findings(&report)[0].fixable());
            assert_eq!(w.calls(), vec![], "doctor changes nothing here");
        }
    }

    // U-STO-93b: ... but only when there is something to lose: no registration in the
    // store, a store that is up, or no store at all say nothing of the kind.
    #[test]
    fn no_unreachable_worktrees_no_finding() {
        // Down, and git lists none of its worktrees.
        let mut w = store_down(None);
        w.worktrees.retain(|e| e.path != real());
        assert_eq!(
            unreachable_findings(&run_store(&w, false)),
            Vec::<&Finding>::new()
        );
        // Up: the worktrees are there to be seen.
        let w = store_world();
        assert_eq!(
            unreachable_findings(&run_store(&w, false)),
            Vec::<&Finding>::new()
        );
        // The registrations of another place are none of the store's business.
        let mut w = store_down(None);
        for entry in &mut w.worktrees {
            if entry.path == real() {
                entry.path = p("/elsewhere/feat");
            }
        }
        assert_eq!(
            unreachable_findings(&run_store(&w, false)),
            Vec::<&Finding>::new()
        );
    }

    // U-STO-93c: ... also when the directory of the store itself is gone (a store that
    // was removed with its worktrees registered): the path does not exist, and git
    // still lists them.
    #[test]
    fn a_store_directory_that_is_gone_is_reported_with_its_registrations() {
        let w = World::default();
        let mut w = w;
        w.worktrees.push(WorktreeEntry {
            path: real(),
            head: Some(SHA_A.into()),
            branch: Some("refs/heads/feat".into()),
            ..WorktreeEntry::default()
        });
        let report = run_store(&w, false);
        assert_eq!(
            unreachable_findings(&report),
            vec![&Finding::StoreWorktreesUnreachable {
                root: p("/store"),
                worktrees: vec![real()],
                unlocked: vec![real()],
            }]
        );
    }

    // U-STO-84: a source on ext4 is not a problem while the store can take it: `add`
    // works, so `not-cow` would be a false alarm. Without a usable store it stays.
    #[test]
    fn not_cow_is_not_reported_while_the_store_can_serve_the_source() {
        let w = World::default().with_store();
        assert!(!w.volume(Path::new("/w/repo")).unwrap().cow);
        assert_eq!(run_store(&w, false), DoctorReport::default());
        // The store is configured but broken: the source's problem is real again.
        w.files.borrow_mut().remove(&p("/store/.parhel-store"));
        let report = run_store(&w, false);
        assert!(
            report.findings.iter().any(|f| f.code() == "not-cow"),
            "{report:#?}"
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code() == "store-unavailable"),
            "{report:#?}"
        );
        // And with no store configured at all.
        let w = World::default().with_store();
        let report = run_with(&w, false, None);
        assert!(
            report.findings.iter().any(|f| f.code() == "not-cow"),
            "{report:#?}"
        );
    }
}
