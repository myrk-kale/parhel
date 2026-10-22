//! `git parhel doctor`: diagnose the environment and repair leftovers of crashed runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::app::add::Env;
use crate::app::rollback::{JOURNAL_FILE, Journal, rollback};
use crate::domain::paths::{self, is_staging_name};
use crate::domain::{DoctorRequest, Result};
use crate::git::parse::WorktreeEntry;
use crate::ports::RepoInfo;

/// Something worth reporting. `code()` is printed and is a stable contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// The source worktree is not on APFS: `add` would refuse (or need `--fallback`).
    NotApfs { path: PathBuf, fs_type: String },
    /// A `.parhel-staging-*` dir left by a crashed `add`.
    OrphanStaging { path: PathBuf },
    /// A worktree registration whose path is inside a staging dir.
    OrphanRegistration { path: PathBuf },
    /// A `.git` *file* inside the source pointing to another repository's git dir:
    /// `add` would clone it and the copy would share that repository's state.
    ForeignGitfile { path: PathBuf },
    /// A staging dir whose journal is locked by a live `add`: not a problem, never touched.
    InProgress { path: PathBuf },
}

impl Finding {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotApfs { .. } => "not-apfs",
            Self::OrphanStaging { .. } => "orphan-staging",
            Self::OrphanRegistration { .. } => "orphan-registration",
            Self::ForeignGitfile { .. } => "foreign-gitfile",
            Self::InProgress { .. } => "in-progress",
        }
    }

    /// Whether `--fix` can repair this finding.
    #[must_use]
    pub const fn fixable(&self) -> bool {
        matches!(
            self,
            Self::OrphanStaging { .. } | Self::OrphanRegistration { .. }
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

    let volume = env.fs.volume(&repo.toplevel)?;
    if volume.fs_type != "apfs" {
        report.findings.push(Finding::NotApfs {
            path: repo.toplevel.clone(),
            fs_type: volume.fs_type,
        });
    }

    let worktrees = env.git.worktrees(&repo.toplevel)?;
    let mut scan_dirs: BTreeSet<PathBuf> = worktrees
        .iter()
        .filter_map(|w| w.path.parent().map(Path::to_path_buf))
        .collect();
    scan_dirs.extend(repo.toplevel.parent().map(Path::to_path_buf));
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
    let in_staging = |path: &Path| path.components().any(|c| is_staging_name(c.as_os_str()));
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

    if request.fix {
        fix(
            env,
            &repo,
            &worktrees,
            &orphans,
            &registrations,
            &mut report,
        );
    }
    Ok(report)
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
    report: &mut DoctorReport,
) {
    let mut handled: Vec<PathBuf> = Vec::new();
    for staging in orphans {
        let journal = env
            .fs
            .read_file(&staging.join(JOURNAL_FILE))
            .ok()
            .and_then(|bytes| Journal::decode(&bytes).ok())
            .and_then(|journal| trusted(journal, repo, worktrees, staging));
        let ok = if let Some(journal) = journal {
            handled.extend(journal.worktree.clone());
            let published = journal.worktree.as_deref().and_then(|staged| {
                published_dest(env, staging, staged, journal.publish_to.as_deref())
                    .map(|dest| (staged, dest))
            });
            match published {
                Some((staged, dest)) => complete_publish(env, repo, staging, staged, &dest),
                None => rollback(env.git, env.fs, staging, &journal).is_ok(),
            }
        } else {
            let mut ok = true;
            for reg in registrations.iter().filter(|r| r.starts_with(staging)) {
                ok &= match published_dest(env, staging, reg, None) {
                    Some(dest) => env.git.repoint_worktree(&repo.toplevel, reg, &dest).is_ok(),
                    None => remove_registration(env, repo, reg),
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
            None => remove_registration(env, repo, reg),
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
/// Returns `None` when the journal points anywhere else.
fn trusted(
    journal: Journal,
    repo: &RepoInfo,
    worktrees: &[WorktreeEntry],
    staging: &Path,
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
    let publish_to = staged
        .as_ref()
        .and_then(|s| Some(staging.parent()?.join(s.file_name()?)));
    Some(Journal {
        repo: repo.toplevel.clone(),
        worktree: staged,
        created_branch,
        publish_to,
        branch_start: journal.branch_start,
    })
}

/// The `.parhel-staging-*` directory `path` lies in, if any.
fn staging_of(path: &Path) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        if is_staging_name(component.as_os_str()) {
            return Some(prefix);
        }
    }
    None
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

/// Finish what the crashed run was doing: point the registration at the destination.
fn complete_publish(
    env: &Env<'_>,
    repo: &RepoInfo,
    staging: &Path,
    staged: &Path,
    dest: &Path,
) -> bool {
    env.git
        .repoint_worktree(&repo.toplevel, staged, dest)
        .is_ok()
        && env.fs.remove_dir_all(staging).is_ok()
}

fn remove_registration(env: &Env<'_>, repo: &RepoInfo, path: &Path) -> bool {
    if env.git.worktree_remove_force(&repo.toplevel, path).is_ok() {
        return true;
    }
    let removed = env.fs.remove_dir_all(path).is_ok();
    removed && env.git.unregister_worktree(&repo.toplevel, path).is_ok()
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
    use crate::app::fakes::{Call, SHA_A, Step, World, p};
    use crate::app::rollback::{JOURNAL_FILE, Journal};
    use crate::domain::ErrorCode;
    use crate::git::parse::WorktreeEntry;
    use crate::ports::VolumeInfo;
    use std::ffi::OsString;
    use std::path::Path;

    fn run(w: &World, fix: bool) -> DoctorReport {
        let env = Env {
            git: w,
            fs: w,
            cwd: Path::new("/w/repo"),
            interrupted: &w.interrupted,
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
    fn reports_non_apfs_source() {
        let w = World {
            volumes: vec![(
                p("/"),
                VolumeInfo {
                    fs_type: "hfs".into(),
                    device: 1,
                },
            )],
            ..World::default()
        };
        let report = run(&w, false);
        assert_eq!(
            report.findings,
            vec![Finding::NotApfs {
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

    // U-DOC-14: orphan registration (not published) whose removal fails: only that
    // registration goes, never a global prune.
    #[test]
    fn orphan_registration_cleanup_is_scoped() {
        let w = crashed_world().failing(Step::RemoveForce, ErrorCode::GitFailed);
        let staging = p("/w/.parhel-staging-dead");
        w.files.borrow_mut().remove(&staging.join(JOURNAL_FILE));
        w.tree
            .borrow_mut()
            .get_mut(&staging)
            .unwrap()
            .retain(|n| n != JOURNAL_FILE);
        assert!(run(&w, true).healthy());
        assert_eq!(
            w.count(|c| *c
                == Call::Unregister {
                    path: staging.join("feat")
                }),
            1
        );
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
    fn forged(journal: Journal) -> World {
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
        let w = forged(Journal {
            repo: p("/w/repo"),
            worktree: Some(p("/w/victim")),
            created_branch: Some("victim-branch".into()),
            publish_to: Some(p("/w/victim")),
            branch_start: Some(SHA_A.into()),
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
        let w = forged(Journal {
            repo: p("/elsewhere/other-repo"),
            worktree: Some(p("/w/.parhel-staging-evil/feat")),
            created_branch: Some("victim-branch".into()),
            publish_to: None,
            branch_start: Some(SHA_A.into()),
        });
        run(&w, true);
        assert_eq!(
            w.count(|c| matches!(c, Call::BranchDelete(_))),
            0,
            "{:#?}",
            w.calls()
        );
    }
}
