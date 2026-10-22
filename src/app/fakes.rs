//! Recording fakes of the [`Git`] and [`CowFs`] ports for application-layer unit tests.
//!
//! One [`World`] implements both ports so every side effect lands in a single,
//! ordered log. Reads are answered from configurable facts and are not logged;
//! every mutation is logged *before* its injected failure is evaluated, so tests
//! can assert both "what was attempted" and "what was undone".

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::domain::{BranchMode, Error, ErrorCode, Result};
use crate::git::parse::{GitVersion, WorktreeEntry};
use crate::ports::{CowFs, FileLock, Git, RepoInfo, VolumeInfo, WorktreeAddSpec};

pub const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// Name the fake gives to every staging dir: `<parent>/.parhel-staging-test`.
pub const FAKE_STAGING: &str = ".parhel-staging-test";

/// A logged mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    WorktreeAdd(WorktreeAddSpec),
    ResetIndex {
        worktree: PathBuf,
        commit: String,
    },
    CheckoutTwoWay {
        worktree: PathBuf,
        from: String,
        to: String,
    },
    SubmodulesReinit {
        worktree: PathBuf,
    },
    Repair {
        path: PathBuf,
    },
    RemoveForce {
        path: PathBuf,
    },
    Prune,
    BranchDelete(String),
    CreateStaging {
        parent: PathBuf,
    },
    CreateDir(PathBuf),
    Clone {
        src: PathBuf,
        dst: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    RemoveDirAll(PathBuf),
    WriteFile(PathBuf),
    Lock(PathBuf),
}

impl Call {
    /// Every path this call mutates.
    pub fn touched_paths(&self) -> Vec<&Path> {
        match self {
            Call::WorktreeAdd(spec) => vec![&spec.path],
            Call::ResetIndex { worktree, .. }
            | Call::CheckoutTwoWay { worktree, .. }
            | Call::SubmodulesReinit { worktree } => vec![worktree],
            Call::Repair { path }
            | Call::RemoveForce { path }
            | Call::CreateDir(path)
            | Call::RemoveDirAll(path)
            | Call::WriteFile(path)
            | Call::Lock(path) => vec![path],
            Call::CreateStaging { parent } => vec![parent],
            Call::Clone { dst, .. } => vec![dst],
            Call::Rename { from, to } => vec![from, to],
            Call::Prune | Call::BranchDelete(_) => vec![],
        }
    }
}

/// Mutation kinds that can be made to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Step {
    CreateStaging,
    WriteJournal,
    WorktreeAdd,
    Clone,
    ResetIndex,
    CheckoutTwoWay,
    SubmodulesReinit,
    Rename,
    Repair,
    RemoveForce,
    BranchDelete,
    RemoveDirAll,
}

/// Configurable fake world. Defaults describe a healthy repo at `/w/repo` on APFS,
/// HEAD `SHA_A` on `main`, containing `src/` and `node_modules/`.
#[derive(Debug)]
pub struct World {
    pub log: RefCell<Vec<Call>>,
    pub fail: RefCell<HashMap<Step, ErrorCode>>,
    /// Raise the interrupt flag right after the first successful call of this kind.
    pub interrupt_after: Cell<Option<Step>>,
    pub interrupted: AtomicBool,

    pub version: GitVersion,
    pub discover_error: Option<ErrorCode>,
    pub repo: RepoInfo,
    pub dirty: bool,
    pub sparse: bool,
    pub flagged: Vec<PathBuf>,
    pub operation: Option<String>,
    /// Simulates a commit in the source while `add` runs: HEAD reported for the
    /// source once any clone happened.
    pub source_head_after_clone: Option<String>,
    /// Simulates an edit in the source while `add` runs.
    pub dirty_after_clone: bool,
    pub submodules: Vec<PathBuf>,
    pub worktrees: Vec<WorktreeEntry>,
    pub branches: RefCell<HashSet<String>>,
    /// `rev -> oid` for `resolve_commit`.
    pub commits: HashMap<String, String>,
    /// HEAD reported for the staging worktree after `worktree add` (the target commit).
    pub staging_head: String,

    /// Longest-prefix match: path prefix -> volume.
    pub volumes: Vec<(PathBuf, VolumeInfo)>,
    /// Paths that exist (any type), beyond the directories in `tree`.
    pub existing: RefCell<HashSet<PathBuf>>,
    /// Absolute directory -> entries.
    pub tree: RefCell<BTreeMap<PathBuf, Vec<OsString>>>,
    pub files: RefCell<HashMap<PathBuf, Vec<u8>>>,
    /// Paths whose `flock` is held by another (live) process.
    pub locked: RefCell<HashSet<PathBuf>>,
    /// Directories in `tree` that are really symlinks to directories.
    pub symlinks: RefCell<HashSet<PathBuf>>,
}

pub fn apfs(device: u64) -> VolumeInfo {
    VolumeInfo {
        fs_type: "apfs".into(),
        device,
    }
}

pub fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

impl Default for World {
    fn default() -> Self {
        let tree = BTreeMap::from([
            (p("/w"), vec![OsString::from("repo")]),
            (
                p("/w/repo"),
                vec![
                    OsString::from(".git"),
                    OsString::from("node_modules"),
                    OsString::from("src"),
                ],
            ),
            (p("/w/repo/.git"), vec![]),
            (p("/w/repo/src"), vec![OsString::from("main.rs")]),
            (p("/w/repo/node_modules"), vec![OsString::from("big")]),
        ]);
        Self {
            log: RefCell::default(),
            fail: RefCell::default(),
            interrupt_after: Cell::new(None),
            interrupted: AtomicBool::new(false),
            version: GitVersion {
                major: 2,
                minor: 47,
                patch: 0,
            },
            discover_error: None,
            repo: RepoInfo {
                toplevel: p("/w/repo"),
                git_dir: p("/w/repo/.git"),
                common_dir: p("/w/repo/.git"),
                head: SHA_A.into(),
            },
            dirty: false,
            sparse: false,
            flagged: vec![],
            operation: None,
            source_head_after_clone: None,
            dirty_after_clone: false,
            submodules: vec![],
            worktrees: vec![WorktreeEntry {
                path: p("/w/repo"),
                head: Some(SHA_A.into()),
                branch: Some("refs/heads/main".into()),
                ..WorktreeEntry::default()
            }],
            branches: RefCell::new(HashSet::from(["main".to_owned()])),
            commits: HashMap::from([
                ("HEAD".to_owned(), SHA_A.to_owned()),
                ("main".to_owned(), SHA_A.to_owned()),
            ]),
            staging_head: SHA_A.into(),
            volumes: vec![(p("/"), apfs(1))],
            existing: RefCell::default(),
            tree: RefCell::new(tree),
            files: RefCell::default(),
            locked: RefCell::default(),
            symlinks: RefCell::default(),
        }
    }
}

impl World {
    pub fn failing(self, step: Step, code: ErrorCode) -> Self {
        self.fail.borrow_mut().insert(step, code);
        self
    }

    pub fn calls(&self) -> Vec<Call> {
        self.log.borrow().clone()
    }

    /// Index of the first logged call matching `pred`.
    pub fn position(&self, pred: impl Fn(&Call) -> bool) -> Option<usize> {
        self.log.borrow().iter().position(pred)
    }

    pub fn count(&self, pred: impl Fn(&Call) -> bool) -> usize {
        self.log.borrow().iter().filter(|c| pred(c)).count()
    }

    /// Whether any logged call mutates `path` or something below it.
    pub fn touched(&self, path: &Path) -> bool {
        self.log
            .borrow()
            .iter()
            .any(|c| c.touched_paths().iter().any(|t| t.starts_with(path)))
    }

    fn record(&self, call: Call, step: Step) -> Result<()> {
        self.log.borrow_mut().push(call);
        if let Some(code) = self.fail.borrow().get(&step) {
            return Err(Error::new(*code, format!("injected failure at {step:?}")));
        }
        if self.interrupt_after.get() == Some(step) {
            self.interrupted.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn cloned_anything(&self) -> bool {
        self.log
            .borrow()
            .iter()
            .any(|c| matches!(c, Call::Clone { .. }))
    }

    fn is_dir_path(&self, path: &Path) -> bool {
        self.tree.borrow().contains_key(path)
    }
}

impl Git for World {
    fn version(&self) -> Result<GitVersion> {
        Ok(self.version)
    }

    fn discover(&self, _from: &Path) -> Result<RepoInfo> {
        match self.discover_error {
            Some(code) => Err(Error::new(code, "fake discover failure")),
            None => Ok(self.repo.clone()),
        }
    }

    fn is_dirty(&self, _: &Path, _exclude: &[PathBuf]) -> Result<bool> {
        Ok(self.dirty || (self.dirty_after_clone && self.cloned_anything()))
    }

    fn flagged_index_entries(&self, _: &Path) -> Result<Vec<PathBuf>> {
        Ok(self.flagged.clone())
    }

    fn operation_in_progress(&self, _: &Path) -> Result<Option<String>> {
        Ok(self.operation.clone())
    }

    fn sparse_checkout_enabled(&self, _: &Path) -> Result<bool> {
        Ok(self.sparse)
    }

    fn initialized_submodules(&self, _: &Path) -> Result<Vec<PathBuf>> {
        Ok(self.submodules.clone())
    }

    fn worktrees(&self, _: &Path) -> Result<Vec<WorktreeEntry>> {
        Ok(self.worktrees.clone())
    }

    fn branch_exists(&self, _: &Path, name: &str) -> Result<bool> {
        Ok(self.branches.borrow().contains(name))
    }

    fn resolve_commit(&self, _: &Path, rev: &str) -> Result<String> {
        self.commits
            .get(rev)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::GitFailed, format!("unknown revision {rev}")))
    }

    fn head(&self, worktree: &Path) -> Result<String> {
        if worktree == self.repo.toplevel {
            match &self.source_head_after_clone {
                Some(moved) if self.cloned_anything() => Ok(moved.clone()),
                _ => Ok(self.repo.head.clone()),
            }
        } else {
            Ok(self.staging_head.clone())
        }
    }

    fn worktree_add(&self, _: &Path, spec: &WorktreeAddSpec) -> Result<()> {
        self.record(Call::WorktreeAdd(spec.clone()), Step::WorktreeAdd)?;
        if let BranchMode::New(name) = &spec.branch {
            self.branches.borrow_mut().insert(name.clone());
        }
        self.tree
            .borrow_mut()
            .insert(spec.path.clone(), vec![OsString::from(".git")]);
        Ok(())
    }

    fn reset_index(&self, worktree: &Path, commit: &str) -> Result<()> {
        self.record(
            Call::ResetIndex {
                worktree: worktree.into(),
                commit: commit.into(),
            },
            Step::ResetIndex,
        )
    }

    fn checkout_two_way(&self, worktree: &Path, from: &str, to: &str) -> Result<()> {
        self.record(
            Call::CheckoutTwoWay {
                worktree: worktree.into(),
                from: from.into(),
                to: to.into(),
            },
            Step::CheckoutTwoWay,
        )
    }

    fn submodules_reinit(&self, worktree: &Path, _source: &Path, _final: &Path) -> Result<()> {
        self.record(
            Call::SubmodulesReinit {
                worktree: worktree.into(),
            },
            Step::SubmodulesReinit,
        )
    }

    fn worktree_repair(&self, _: &Path, path: &Path) -> Result<()> {
        self.record(Call::Repair { path: path.into() }, Step::Repair)
    }

    fn worktree_remove_force(&self, _: &Path, path: &Path) -> Result<()> {
        self.record(Call::RemoveForce { path: path.into() }, Step::RemoveForce)
    }

    fn worktree_prune(&self, _: &Path) -> Result<()> {
        self.log.borrow_mut().push(Call::Prune);
        Ok(())
    }

    fn branch_delete(&self, _: &Path, name: &str) -> Result<()> {
        self.record(Call::BranchDelete(name.into()), Step::BranchDelete)?;
        self.branches.borrow_mut().remove(name);
        Ok(())
    }
}

impl CowFs for World {
    fn volume(&self, path: &Path) -> Result<VolumeInfo> {
        self.volumes
            .iter()
            .filter(|(prefix, _)| path.starts_with(prefix))
            .max_by_key(|(prefix, _)| prefix.components().count())
            .map(|(_, v)| v.clone())
            .ok_or_else(|| Error::new(ErrorCode::Io, format!("no volume for {}", path.display())))
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        Ok(path.to_path_buf())
    }

    fn exists_no_follow(&self, path: &Path) -> Result<bool> {
        Ok(self.existing.borrow().contains(path)
            || self.is_dir_path(path)
            || self.files.borrow().contains_key(path))
    }

    fn is_dir(&self, path: &Path) -> Result<bool> {
        Ok(self.is_dir_path(path))
    }

    fn is_dir_no_follow(&self, path: &Path) -> Result<bool> {
        Ok(self.is_dir_path(path) && !self.symlinks.borrow().contains(path))
    }

    fn create_staging_dir(&self, parent: &Path) -> Result<PathBuf> {
        self.record(
            Call::CreateStaging {
                parent: parent.into(),
            },
            Step::CreateStaging,
        )?;
        let dir = parent.join(FAKE_STAGING);
        self.tree.borrow_mut().insert(dir.clone(), vec![]);
        Ok(dir)
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<OsString>> {
        self.tree
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::Io, format!("not a dir: {}", path.display())))
    }

    fn create_dir(&self, path: &Path) -> Result<()> {
        self.record(Call::CreateDir(path.into()), Step::Clone)
    }

    fn clone_entry(&self, src: &Path, dst: &Path) -> Result<()> {
        self.record(
            Call::Clone {
                src: src.into(),
                dst: dst.into(),
            },
            Step::Clone,
        )
    }

    fn rename_exclusive(&self, from: &Path, to: &Path) -> Result<()> {
        self.record(
            Call::Rename {
                from: from.into(),
                to: to.into(),
            },
            Step::Rename,
        )?;
        if self.existing.borrow().contains(to) || self.is_dir_path(to) {
            return Err(Error::new(
                ErrorCode::DestExists,
                "fake: destination exists",
            ));
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        self.record(Call::RemoveDirAll(path.into()), Step::RemoveDirAll)?;
        self.tree.borrow_mut().retain(|k, _| !k.starts_with(path));
        Ok(())
    }

    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()> {
        self.record(Call::WriteFile(path.into()), Step::WriteJournal)?;
        self.files
            .borrow_mut()
            .insert(path.into(), contents.to_vec());
        Ok(())
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        self.files
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::Io, format!("no file {}", path.display())))
    }

    fn lock_exclusive(&self, path: &Path) -> Result<FileLock> {
        self.record(Call::Lock(path.into()), Step::WriteJournal)?;
        Ok(FileLock::detached())
    }

    fn is_locked(&self, path: &Path) -> Result<bool> {
        Ok(self.locked.borrow().contains(path))
    }
}
