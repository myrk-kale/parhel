//! The `add` transaction: validate, assemble in staging, publish atomically.
//!
//! Invariants (each covered by a test below and by integration tests):
//! - every refusal happens before the first mutation;
//! - the destination path is only ever written by the final exclusive rename, so
//!   an existing destination (including another run's worktree) is never touched;
//! - any failure before publish undoes the registration, the branch this run
//!   created, and the staging dir.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::domain::{AddRequest, Result};
use crate::ports::{CowFs, Git};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    pub destination: PathBuf,
    /// Commit checked out in the new worktree.
    pub head: String,
    pub strategy: Strategy,
    /// Printed to stderr as `warning: ...`.
    pub warnings: Vec<String>,
}

/// Run `git parhel add`.
pub fn add(env: &Env<'_>, request: &AddRequest) -> Result<AddOutcome> {
    let _ = (env, request);
    todo!("app::add")
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
            submodules: SubmodulePolicy::Refuse,
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
    fn refuses_source_not_on_apfs() {
        let w = World {
            volumes: vec![
                (p("/"), apfs(1)),
                (
                    p("/w/repo"),
                    VolumeInfo {
                        fs_type: "hfs".into(),
                        device: 2,
                    },
                ),
            ],
            ..World::default()
        };
        let err = run(&w, &new_branch()).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotApfs);
        assert!(err.message.contains("/w/repo") && err.message.contains("hfs"));
        assert_no_mutation(&w);
    }

    #[test]
    fn refuses_destination_parent_not_on_apfs() {
        let w = World {
            volumes: vec![
                (
                    p("/"),
                    VolumeInfo {
                        fs_type: "exfat".into(),
                        device: 2,
                    },
                ),
                (p("/w/repo"), apfs(1)),
            ],
            ..World::default()
        };
        assert_eq!(run(&w, &new_branch()).unwrap_err().code, ErrorCode::NotApfs);
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
    fn fallback_uses_plain_worktree_add_when_not_apfs() {
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
    #[test]
    fn refuses_non_utf8_destination_before_mutation() {
        use std::os::unix::ffi::OsStrExt;
        let w = World::default();
        let mut req = new_branch();
        req.destination = Path::new("/w").join(std::ffi::OsStr::from_bytes(b"feat-\xff"));
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
}
