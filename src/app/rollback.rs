//! Undo log of an in-flight `add`.
//!
//! The journal is persisted inside the staging dir *before* each mutation it
//! describes, so a crashed run (SIGKILL, power loss) leaves enough information
//! for `doctor --fix` to undo exactly what this run created and nothing else.

use std::path::{Path, PathBuf};

use crate::domain::Result;
use crate::ports::{CowFs, Git};

/// File name of the journal inside the staging dir.
pub const JOURNAL_FILE: &str = "journal";

/// What an `add` run has created so far.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Journal {
    /// Repository the worktree was registered in (source toplevel).
    pub repo: PathBuf,
    /// Staging worktree path once `git worktree add` succeeded.
    pub worktree: Option<PathBuf>,
    /// Branch created by this run (`--new-branch`, or the implicit basename branch).
    /// Never set for a branch that existed before the run.
    pub created_branch: Option<String>,
}

impl Journal {
    /// Serialize; must round-trip any path (spaces, newlines, non-UTF-8 bytes).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        todo!("Journal::encode")
    }

    /// Errors: `io` on malformed content.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let _ = bytes;
        todo!("Journal::decode")
    }
}

/// Undo everything recorded in `journal`, then delete `staging_dir`:
///
/// 1. `git worktree remove --force <worktree>`; if that fails, delete the directory
///    and `git worktree prune`;
/// 2. `git branch -D <created_branch>`;
/// 3. remove the staging dir.
///
/// Every step is attempted even if an earlier one fails. Any failure yields
/// `rollback-failed` with the manual commands needed to finish the cleanup.
pub fn rollback(
    git: &dyn Git,
    fs: &dyn CowFs,
    staging_dir: &Path,
    journal: &Journal,
) -> Result<()> {
    let _ = (git, fs, staging_dir, journal);
    todo!("rollback")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{Call, Step, World, p};
    use crate::domain::ErrorCode;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn full_journal() -> Journal {
        Journal {
            repo: p("/w/repo"),
            worktree: Some(p("/w/.parhel-staging-test/feat")),
            created_branch: Some("feat".into()),
        }
    }

    // U-RB-01
    #[test]
    fn journal_round_trips() {
        for j in [
            Journal {
                repo: p("/w/repo"),
                ..Journal::default()
            },
            full_journal(),
        ] {
            assert_eq!(Journal::decode(&j.encode()).unwrap(), j);
        }
    }

    // U-RB-02
    #[test]
    fn journal_round_trips_hostile_paths() {
        let weird = PathBuf::from(OsStr::from_bytes(b"/w/a b\nc=d\0e\xff"));
        let j = Journal {
            repo: weird.clone(),
            worktree: Some(weird.join("x")),
            created_branch: Some("feat/with=equals".into()),
        };
        assert_eq!(Journal::decode(&j.encode()).unwrap(), j);
    }

    // U-RB-03
    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(
            Journal::decode(b"\x00\x01garbage").unwrap_err().code,
            ErrorCode::Io
        );
    }

    // U-RB-04
    #[test]
    fn undoes_worktree_then_branch_then_staging() {
        let w = World::default();
        rollback(&w, &w, &p("/w/.parhel-staging-test"), &full_journal()).unwrap();
        assert_eq!(
            w.calls(),
            vec![
                Call::RemoveForce {
                    path: p("/w/.parhel-staging-test/feat")
                },
                Call::BranchDelete("feat".into()),
                Call::RemoveDirAll(p("/w/.parhel-staging-test")),
            ]
        );
    }

    // U-RB-05
    #[test]
    fn empty_journal_only_removes_staging() {
        let w = World::default();
        let j = Journal {
            repo: p("/w/repo"),
            ..Journal::default()
        };
        rollback(&w, &w, &p("/w/.parhel-staging-test"), &j).unwrap();
        assert_eq!(
            w.calls(),
            vec![Call::RemoveDirAll(p("/w/.parhel-staging-test"))]
        );
    }

    // U-RB-06
    #[test]
    fn failed_worktree_remove_falls_back_to_delete_and_prune() {
        let w = World::default().failing(Step::RemoveForce, ErrorCode::GitFailed);
        rollback(&w, &w, &p("/w/.parhel-staging-test"), &full_journal()).unwrap();
        let wt = p("/w/.parhel-staging-test/feat");
        let rm = w
            .position(|c| *c == Call::RemoveDirAll(wt.clone()))
            .expect("dir removed");
        let prune = w.position(|c| *c == Call::Prune).expect("pruned");
        assert!(rm < prune, "prune must run after the directory is gone");
        assert_eq!(w.count(|c| *c == Call::BranchDelete("feat".into())), 1);
    }

    // U-RB-07
    #[test]
    fn keeps_going_and_reports_when_a_step_fails() {
        let w = World::default().failing(Step::BranchDelete, ErrorCode::GitFailed);
        let err = rollback(&w, &w, &p("/w/.parhel-staging-test"), &full_journal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::RollbackFailed);
        assert!(
            err.message.contains("git branch -D feat"),
            "manual hint: {}",
            err.message
        );
        assert_eq!(
            w.count(|c| *c == Call::RemoveDirAll(p("/w/.parhel-staging-test"))),
            1
        );
    }

    // U-RB-08
    #[test]
    fn staging_removal_failure_names_the_path() {
        let w = World::default().failing(Step::RemoveDirAll, ErrorCode::Io);
        let j = Journal {
            repo: p("/w/repo"),
            ..Journal::default()
        };
        let err = rollback(&w, &w, &p("/w/.parhel-staging-test"), &j).unwrap_err();
        assert_eq!(err.code, ErrorCode::RollbackFailed);
        assert!(
            err.message.contains("/w/.parhel-staging-test"),
            "{}",
            err.message
        );
    }

    // U-RB-P1
    mod props {
        use super::*;
        use proptest::prelude::*;
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        fn path() -> impl Strategy<Value = PathBuf> {
            prop::collection::vec(any::<u8>(), 1..40)
                .prop_map(|b| PathBuf::from(OsStr::from_bytes(&b)))
        }

        proptest! {
            #[test]
            fn journal_round_trips_arbitrary_bytes(
                repo in path(),
                worktree in prop::option::of(path()),
                branch in prop::option::of("[^\\x00]{1,30}"),
            ) {
                let j = Journal { repo, worktree, created_branch: branch };
                prop_assert_eq!(Journal::decode(&j.encode()).unwrap(), j);
            }

            #[test]
            fn decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
                let _ = Journal::decode(&bytes);
            }
        }
    }
}
