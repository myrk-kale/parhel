//! Undo log of an in-flight `add`.
//!
//! The journal is persisted inside the staging dir *before* each mutation it
//! describes, so a crashed run (SIGKILL, power loss) leaves enough information
//! for `doctor --fix` to undo exactly what this run created and nothing else.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::domain::{Error, ErrorCode, Result};
use crate::ports::{CowFs, Git};

/// File name of the journal inside the staging dir.
pub const JOURNAL_FILE: &str = "journal";

const MAGIC: &[u8] = b"parhel-journal-v1\0";

fn put(out: &mut Vec<u8>, field: Option<&[u8]>) {
    match field {
        None => out.push(0),
        Some(bytes) => {
            out.push(1);
            let len = u32::try_from(bytes.len()).expect("journal field under 4 GiB");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(Error::new(ErrorCode::Io, "truncated parhel journal"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn field(&mut self) -> Result<Option<&'a [u8]>> {
        match self.take(1)?[0] {
            0 => Ok(None),
            1 => {
                let len = u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes"));
                Ok(Some(self.take(len as usize)?))
            }
            _ => Err(Error::new(ErrorCode::Io, "malformed parhel journal")),
        }
    }
}

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
    /// Final destination, recorded right before the publishing rename. After a crash,
    /// a missing staged worktree plus an existing `publish_to` means the rename
    /// happened: the worktree must be repaired, never rolled back.
    pub publish_to: Option<PathBuf>,
    /// Commit `created_branch` pointed to when this run created it. The branch is
    /// deleted on rollback only while it still points there, so commits made on it
    /// after a crash are never lost. Missing (older journals): never deleted.
    pub branch_start: Option<String>,
    /// Store runs: the symlink placed at the requested destination and the real
    /// worktree path it points to, recorded before the symlink is created. Rollback
    /// removes the symlink only while it still is a symlink to exactly this target.
    /// Missing (journals of runs without the store, and older ones): none.
    pub store_link: Option<StoreLink>,
}

/// The symlink a store run claims at the requested destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLink {
    /// Where the symlink lives (the destination the user asked for).
    pub path: PathBuf,
    /// What it points to (the worktree's real path inside the store).
    pub target: PathBuf,
}

impl Journal {
    /// Serialize; must round-trip any path (spaces, newlines, non-UTF-8 bytes).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        put(&mut out, Some(self.repo.as_os_str().as_bytes()));
        put(
            &mut out,
            self.worktree.as_ref().map(|p| p.as_os_str().as_bytes()),
        );
        put(&mut out, self.created_branch.as_ref().map(String::as_bytes));
        put(
            &mut out,
            self.publish_to.as_ref().map(|p| p.as_os_str().as_bytes()),
        );
        put(&mut out, self.branch_start.as_ref().map(String::as_bytes));
        // One field, `path NUL target` (neither can contain NUL), added in 0.4.
        let link = self.store_link.as_ref().map(|l| {
            let mut bytes = l.path.as_os_str().as_bytes().to_vec();
            bytes.push(0);
            bytes.extend_from_slice(l.target.as_os_str().as_bytes());
            bytes
        });
        put(&mut out, link.as_deref());
        out
    }

    /// Errors: `io` on malformed content.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        if r.take(MAGIC.len()).ok() != Some(MAGIC) {
            return Err(Error::new(ErrorCode::Io, "not a parhel journal"));
        }
        let path = |b: &[u8]| PathBuf::from(OsStr::from_bytes(b));
        let repo = r
            .field()?
            .map(path)
            .ok_or_else(|| Error::new(ErrorCode::Io, "journal without repo"))?;
        let worktree = r.field()?.map(path);
        let created_branch = r
            .field()?
            .map(|b| String::from_utf8(b.to_vec()))
            .transpose()
            .map_err(|_| Error::new(ErrorCode::Io, "journal branch is not UTF-8"))?;
        let publish_to = r.field()?.map(path);
        // Added in 0.2: absent from journals written by older versions.
        let branch_start = if r.0.is_empty() {
            None
        } else {
            r.field()?.map(|b| String::from_utf8_lossy(b).into_owned())
        };
        // Added in 0.4: absent from journals written by older versions.
        let store_link = if r.0.is_empty() {
            None
        } else {
            r.field()?
                .map(|b| {
                    let split = b.iter().position(|&c| c == 0).ok_or_else(|| {
                        Error::new(ErrorCode::Io, "malformed store link in parhel journal")
                    })?;
                    Ok(StoreLink {
                        path: path(&b[..split]),
                        target: path(&b[split + 1..]),
                    })
                })
                .transpose()?
        };
        if !r.0.is_empty() {
            return Err(Error::new(
                ErrorCode::Io,
                "trailing bytes in parhel journal",
            ));
        }
        Ok(Self {
            repo,
            worktree,
            created_branch,
            publish_to,
            branch_start,
            store_link,
        })
    }
}

/// Undo everything recorded in `journal`, then delete `staging_dir`:
///
/// 1. `git worktree remove --force <worktree>`; if that fails, delete the directory
///    and remove only its registration (never a repository-wide `worktree prune`,
///    which would also drop worktrees that are merely unreachable);
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
    let mut failures: Vec<(String, Error)> = Vec::new();
    let repo = &journal.repo;
    if let Some(worktree) = &journal.worktree {
        if git.worktree_remove_force(repo, worktree).is_err() {
            if let Err(e) = fs.remove_dir_all(worktree) {
                failures.push((format!("rm -rf '{}'", worktree.display()), e));
            }
            if let Err(e) = git.unregister_worktree(repo, worktree) {
                failures.push((
                    format!(
                        "rm -rf \"$(git rev-parse --git-common-dir)/worktrees/{}\"",
                        worktree.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    e,
                ));
            }
        }
    }
    if let Some(branch) = &journal.created_branch {
        // Only while the branch is exactly as this run created it: commits made on
        // it since (e.g. after a crash) must never be lost.
        let tip = git
            .resolve_commit(repo, &format!("refs/heads/{branch}"))
            .ok();
        if let (Some(tip), Some(start)) = (&tip, &journal.branch_start) {
            if tip == start {
                if let Err(e) = git.branch_delete(repo, branch, start) {
                    failures.push((format!("git branch -D {branch}"), e));
                }
            }
        }
    }
    if let Err(e) = fs.remove_dir_all(staging_dir) {
        failures.push((format!("rm -rf '{}'", staging_dir.display()), e));
    }
    if failures.is_empty() {
        return Ok(());
    }
    let mut message = format!(
        "rollback incomplete; finish the cleanup manually in `{}`:",
        repo.display()
    );
    for (command, cause) in &failures {
        let _ = write!(message, "\n    {command}    # {cause}");
    }
    Err(Error::new(ErrorCode::RollbackFailed, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{Call, SHA_A, SHA_B, Step, World, p};
    use crate::domain::ErrorCode;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn full_journal() -> Journal {
        Journal {
            repo: p("/w/repo"),
            worktree: Some(p("/w/.parhel-staging-test/feat")),
            created_branch: Some("feat".into()),
            publish_to: Some(p("/w/feat")),
            branch_start: Some(SHA_A.into()),
            store_link: None,
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
            publish_to: Some(weird.join("dest")),
            branch_start: Some(SHA_A.into()),
            store_link: None,
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
        w.branches.borrow_mut().insert("feat".into());
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

    // U-RB-06: when `worktree remove` fails, only this run's registration is
    // removed. A global `worktree prune` would also drop unrelated worktrees whose
    // directories are temporarily unreachable (unmounted disks).
    #[test]
    fn failed_worktree_remove_unregisters_only_this_worktree() {
        let w = World::default().failing(Step::RemoveForce, ErrorCode::GitFailed);
        w.branches.borrow_mut().insert("feat".into());
        rollback(&w, &w, &p("/w/.parhel-staging-test"), &full_journal()).unwrap();
        let wt = p("/w/.parhel-staging-test/feat");
        let rm = w
            .position(|c| *c == Call::RemoveDirAll(wt.clone()))
            .expect("dir removed");
        let unregister = w
            .position(|c| *c == Call::Unregister { path: wt.clone() })
            .expect("own registration removed");
        assert!(rm < unregister);
        assert_eq!(w.count(|c| *c == Call::BranchDelete("feat".into())), 1);
    }

    // U-RB-07
    #[test]
    fn keeps_going_and_reports_when_a_step_fails() {
        let w = World::default().failing(Step::BranchDelete, ErrorCode::GitFailed);
        w.branches.borrow_mut().insert("feat".into());
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
                publish_to in prop::option::of(path()),
                branch_start in prop::option::of("[0-9a-f]{40}"),
            ) {
                let j = Journal { repo, worktree, created_branch: branch, publish_to, branch_start, store_link: None };
                prop_assert_eq!(Journal::decode(&j.encode()).unwrap(), j);
            }

            #[test]
            fn decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
                let _ = Journal::decode(&bytes);
            }
        }
    }

    // U-RB-10: commits made on the branch after it was created keep it alive.
    #[test]
    fn keeps_branch_that_moved_since_creation() {
        let w = World::default();
        w.branches.borrow_mut().insert("feat".into());
        w.branch_tips
            .borrow_mut()
            .insert("feat".into(), SHA_B.into());
        rollback(&w, &w, &p("/w/.parhel-staging-test"), &full_journal()).unwrap();
        assert_eq!(w.count(|c| matches!(c, Call::BranchDelete(_))), 0);
    }

    // U-RB-11: an older journal without the start commit cannot prove the branch
    // is unused: keep it.
    #[test]
    fn keeps_branch_without_recorded_start() {
        let w = World::default();
        w.branches.borrow_mut().insert("feat".into());
        let j = Journal {
            branch_start: None,
            ..full_journal()
        };
        rollback(&w, &w, &p("/w/.parhel-staging-test"), &j).unwrap();
        assert_eq!(w.count(|c| matches!(c, Call::BranchDelete(_))), 0);
    }

    // U-RB-12: journals written before `branch_start` existed still decode.
    #[test]
    fn decodes_journal_without_branch_start() {
        let mut bytes = Journal {
            branch_start: None,
            ..full_journal()
        }
        .encode();
        // Drop the trailing "absent" markers of the two newest fields.
        bytes.truncate(bytes.len() - 2);
        let j = Journal::decode(&bytes).unwrap();
        assert_eq!(j.branch_start, None);
        assert_eq!(j.created_branch.as_deref(), Some("feat"));
    }

    const REAL: &str = "/store/worktrees/7005185596c2e32bd0688ebe72725f92/feat";
    const STORE_STAGING: &str = "/store/worktrees/.parhel-staging-test";

    fn store_journal() -> Journal {
        Journal {
            worktree: Some(p(STORE_STAGING).join("feat")),
            publish_to: Some(p(REAL)),
            store_link: Some(StoreLink {
                path: p("/w/feat"),
                target: p(REAL),
            }),
            ..full_journal()
        }
    }

    // U-STO-60: the link field is data like the rest (real: pure encoding).
    #[test]
    fn journal_round_trips_the_store_link() {
        let j = store_journal();
        assert_eq!(Journal::decode(&j.encode()).unwrap(), j);
        let weird = PathBuf::from(OsStr::from_bytes(b"/w/a b\nc=d\xff"));
        let j = Journal {
            store_link: Some(StoreLink {
                path: weird.clone(),
                target: weird.join("t\te"),
            }),
            ..store_journal()
        };
        assert_eq!(Journal::decode(&j.encode()).unwrap(), j);
    }

    // U-STO-61: journals written before the link existed (every journal of 0.3 and
    // earlier) still decode, with no link.
    #[test]
    fn decodes_journal_without_store_link() {
        let mut bytes = full_journal().encode();
        bytes.pop(); // the "absent" marker of the link field
        let j = Journal::decode(&bytes).unwrap();
        assert_eq!(j.store_link, None);
        assert_eq!(j, full_journal());
    }

    // U-STO-62: a half-written link field is corruption, not "no link".
    #[test]
    fn decode_rejects_a_link_without_a_target() {
        let mut bytes = full_journal().encode();
        bytes.pop();
        bytes.extend_from_slice(&[1, 3, 0, 0, 0, b'/', b'w', b'x']); // no NUL separator
        assert_eq!(Journal::decode(&bytes).unwrap_err().code, ErrorCode::Io);
    }

    fn rollback_store(w: &World) {
        rollback(w, w, &p(STORE_STAGING), &store_journal()).unwrap();
    }

    // U-STO-63: the symlink this run created is removed, and nothing at its place
    // is removed with a recursive delete.
    #[test]
    fn rollback_removes_the_symlink_it_created() {
        let w = World::default();
        w.put_link(Path::new("/w/feat"), Path::new(REAL));
        rollback_store(&w);
        assert_eq!(
            w.count(|c| *c == Call::RemoveFile(p("/w/feat"))),
            1,
            "{:#?}",
            w.calls()
        );
        assert_eq!(w.count(|c| *c == Call::RemoveDirAll(p("/w/feat"))), 0);
        assert_eq!(
            w.calls().last(),
            Some(&Call::RemoveDirAll(p(STORE_STAGING))),
            "staging still goes last"
        );
    }

    // U-STO-64: a link that now points elsewhere is somebody else's.
    #[test]
    fn rollback_keeps_a_symlink_that_was_retargeted() {
        let w = World::default();
        w.put_link(Path::new("/w/feat"), Path::new("/somewhere/else"));
        rollback_store(&w);
        assert!(!w.touched(Path::new("/w/feat")), "{:#?}", w.calls());
    }

    // U-STO-65: a regular file or directory at the path is never removed.
    #[test]
    fn rollback_keeps_a_directory_or_file_in_its_place() {
        let w = World::default();
        w.put_dir(Path::new("/w/feat"));
        w.put_file(Path::new("/w/feat/precious"), 1, 1);
        rollback_store(&w);
        assert!(!w.touched(Path::new("/w/feat")), "{:#?}", w.calls());

        let w = World::default();
        w.put_file(Path::new("/w/feat"), 1, 1);
        rollback_store(&w);
        assert!(!w.touched(Path::new("/w/feat")), "{:#?}", w.calls());
    }

    // U-STO-66: nothing there (the run died before claiming it) is fine.
    #[test]
    fn rollback_without_the_symlink_is_not_an_error() {
        let w = World::default();
        rollback_store(&w);
        assert!(!w.touched(Path::new("/w/feat")), "{:#?}", w.calls());
    }

    // U-STO-67: a symlink that cannot be removed is reported with the command to run.
    #[test]
    fn rollback_names_the_symlink_it_could_not_remove() {
        let w = World::default().failing(Step::RemoveFile, ErrorCode::Io);
        w.put_link(Path::new("/w/feat"), Path::new(REAL));
        let err = rollback(&w, &w, &p(STORE_STAGING), &store_journal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::RollbackFailed);
        assert!(err.message.contains("/w/feat"), "{}", err.message);
    }
}
