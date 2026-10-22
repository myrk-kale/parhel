//! `git parhel doctor`: diagnose the environment and repair leftovers of crashed runs.

use std::path::PathBuf;

use crate::app::add::Env;
use crate::domain::{DoctorRequest, Result};

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
    let _ = (env, request);
    todo!("app::doctor")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{Call, SHA_A, World, p};
    use crate::app::rollback::{JOURNAL_FILE, Journal};
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
}
