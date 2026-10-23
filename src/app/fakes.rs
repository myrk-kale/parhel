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

use crate::domain::store::{EntryKind, EntryMeta};
use crate::domain::{BranchMode, Error, ErrorCode, Result};
use crate::git::parse::{GitVersion, WorktreeEntry};
use crate::ports::{
    CowFs, FileLock, Git, Host, ImageUsage, Owner, RepoInfo, Scan, Space, UserInfo, VolumeInfo,
    WorktreeAddSpec,
};

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
    /// `worktree_lock`.
    WorktreeLock {
        path: PathBuf,
        reason: String,
    },
    Unregister {
        path: PathBuf,
    },
    Rewrite(PathBuf),
    RemoveFile(PathBuf),
    CopyMode {
        from: PathBuf,
        to: PathBuf,
    },
    Relink {
        path: PathBuf,
        target: PathBuf,
    },
    BranchDelete(String),
    CreateStaging {
        parent: PathBuf,
    },
    CreateDir(PathBuf),
    Clone {
        src: PathBuf,
        dst: PathBuf,
    },
    /// `copy_entry`: a full copy, as opposed to [`Call::Clone`].
    Copy {
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
    /// `mirror_entry`: one entry copied into the baseline.
    Mirror {
        src: PathBuf,
        dst: PathBuf,
    },
    /// `mirror_dir_meta`: a baseline directory given its source's metadata.
    MirrorDirMeta {
        src: PathBuf,
        dst: PathBuf,
    },
    /// `symlink_exclusive`.
    Symlink {
        target: PathBuf,
        link: PathBuf,
    },
    Append(PathBuf),
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
            | Call::WorktreeLock { path, .. }
            | Call::Unregister { path }
            | Call::Rewrite(path)
            | Call::RemoveFile(path)
            | Call::CopyMode { to: path, .. }
            | Call::Relink { path, .. }
            | Call::CreateDir(path)
            | Call::RemoveDirAll(path)
            | Call::WriteFile(path)
            | Call::Append(path)
            | Call::Lock(path) => vec![path],
            Call::Symlink { link, .. } => vec![link],
            Call::CreateStaging { parent } => vec![parent],
            Call::Clone { dst, .. }
            | Call::Copy { dst, .. }
            | Call::Mirror { dst, .. }
            | Call::MirrorDirMeta { dst, .. } => vec![dst],
            Call::Rename { from, to } => vec![from, to],
            Call::BranchDelete(_) => vec![],
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
    WorktreeLock,
    BranchDelete,
    RemoveDirAll,
    RemoveFile,
    Mirror,
    Symlink,
    Append,
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
    /// Worktrees (e.g. submodule checkouts) reported dirty, beyond `dirty`.
    pub dirty_paths: HashSet<PathBuf>,
    /// Simulates an edit in the source while `add` runs.
    pub dirty_after_clone: bool,
    pub submodules: Vec<PathBuf>,
    pub worktrees: Vec<WorktreeEntry>,
    pub branches: RefCell<HashSet<String>>,
    /// Tip of each branch; branches without an entry point at `staging_head`.
    pub branch_tips: RefCell<HashMap<String, String>>,
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
    /// Symlink targets (absolute paths of links -> target).
    pub links: RefCell<HashMap<PathBuf, PathBuf>>,
    /// Files reported as larger than the relocation limit.
    pub big: RefCell<HashSet<PathBuf>>,
    /// Message of injected failures (default: a generic one).
    pub fail_message: RefCell<String>,
    /// With a failing `WorktreeAdd`: create the `-b` branch before failing, as git does.
    pub fail_after_branch: Cell<bool>,
    /// Index of the staged worktree (relative paths).
    pub tracked: Vec<PathBuf>,
    /// Result of `parhelignored` for the source.
    pub parhelignored: Vec<PathBuf>,
    /// Untracked content that "appears" in the staged worktree once registered,
    /// standing in for what the clone would bring: (relative path, bytes).
    pub staged_files: Vec<(PathBuf, Vec<u8>)>,
    /// Symlinks that appear in the staged worktree: (relative path, target).
    pub staged_links: Vec<(PathBuf, PathBuf)>,
    /// `lstat` facts that differ from the defaults derived from `tree`, `files` and
    /// `links` (size, mtime, mode of an entry).
    pub meta: RefCell<HashMap<PathBuf, EntryMeta>>,
    /// Directories the current user cannot write to.
    pub unwritable: RefCell<HashSet<PathBuf>>,
    /// Longest-prefix match: path prefix -> filesystem space.
    pub spaces: RefCell<Vec<(PathBuf, Space)>>,
    /// How many times `flush` was called, and whether it reports a full disk.
    pub flushes: Cell<usize>,
    pub fail_flush: Cell<bool>,
    /// A symlink another run creates at the moment `symlink_exclusive` is called
    /// (link, target): the race that `symlink_exclusive` exists to lose cleanly.
    pub rival_link: RefCell<Option<(PathBuf, PathBuf)>>,
    /// A symlink another process (a `doctor --fix`) removes (`None`) or points elsewhere
    /// (`Some(target)`) at the moment `rename_exclusive` is called: (link, what becomes
    /// of it).
    pub link_tampered_at_rename: RefCell<Option<(PathBuf, Option<PathBuf>)>>,
    /// `space` fails (a path that cannot be measured).
    pub space_unavailable: Cell<bool>,
}

pub fn apfs(device: u64) -> VolumeInfo {
    VolumeInfo {
        fs_type: "apfs".into(),
        cow: true,
        device,
        filesystem: device,
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
            dirty_paths: HashSet::new(),
            submodules: vec![],
            worktrees: vec![WorktreeEntry {
                path: p("/w/repo"),
                head: Some(SHA_A.into()),
                branch: Some("refs/heads/main".into()),
                ..WorktreeEntry::default()
            }],
            branches: RefCell::new(HashSet::from(["main".to_owned()])),
            branch_tips: RefCell::default(),
            commits: HashMap::from([
                ("HEAD".to_owned(), SHA_A.to_owned()),
                ("main".to_owned(), SHA_A.to_owned()),
                ("v1".to_owned(), SHA_B.to_owned()),
            ]),
            staging_head: SHA_A.into(),
            volumes: vec![(p("/"), apfs(1))],
            existing: RefCell::default(),
            tree: RefCell::new(tree),
            files: RefCell::default(),
            locked: RefCell::default(),
            symlinks: RefCell::default(),
            links: RefCell::default(),
            big: RefCell::default(),
            fail_message: RefCell::default(),
            fail_after_branch: Cell::new(false),
            tracked: vec![],
            parhelignored: vec![],
            staged_files: vec![],
            staged_links: vec![],
            meta: RefCell::default(),
            unwritable: RefCell::default(),
            spaces: RefCell::default(),
            flushes: Cell::new(0),
            fail_flush: Cell::new(false),
            rival_link: RefCell::default(),
            link_tampered_at_rename: RefCell::default(),
            space_unavailable: Cell::new(false),
        }
    }
}

/// A btrfs volume (copy-on-write).
pub fn btrfs(device: u64) -> VolumeInfo {
    VolumeInfo {
        fs_type: "btrfs".into(),
        cow: true,
        device,
        filesystem: device,
    }
}

/// An ext4 volume (no copy-on-write).
pub fn ext4(device: u64) -> VolumeInfo {
    VolumeInfo {
        fs_type: "ext4".into(),
        cow: false,
        device,
        filesystem: device,
    }
}

/// The fake store's root.
pub const STORE: &str = "/store";

/// `repo_id("/w/repo")`: first 16 bytes of its SHA-256, in hex.
pub const REPO_ID: &str = "7005185596c2e32bd0688ebe72725f92";

/// A valid marker for the fake store.
pub const MARKER: &str =
    "format=1\nuser=alice\nimage=/var/lib/parhel/alice.img\nunit=parhel-alice.mount\n";

impl World {
    /// The healthy world of the store: sources and destinations on ext4 (device 1),
    /// a store with its marker, `baselines/` and `worktrees/` on btrfs (device 2) at
    /// [`STORE`], writable.
    pub fn with_store(mut self) -> Self {
        self.volumes = vec![(p("/"), ext4(1)), (p(STORE), btrfs(2))];
        for dir in [STORE, "/store/baselines", "/store/worktrees"] {
            self.put_dir(Path::new(dir));
        }
        self.files
            .borrow_mut()
            .insert(p("/store/.parhel-store"), MARKER.as_bytes().to_vec());
        self.add_entry(Path::new("/store/.parhel-store"));
        self
    }

    /// A directory (and its ancestors) in the fake tree.
    pub fn put_dir(&self, path: &Path) {
        self.add_entry(path);
        self.tree
            .borrow_mut()
            .entry(path.to_path_buf())
            .or_default();
    }

    /// A regular file with the given size and mtime (ns).
    pub fn put_file(&self, path: &Path, size: u64, mtime_ns: i128) {
        self.add_entry(path);
        self.meta.borrow_mut().insert(
            path.to_path_buf(),
            EntryMeta {
                kind: EntryKind::File,
                size,
                mtime_ns,
                ctime_ns: 0,
                mode: 0o644,
                link_target: None,
            },
        );
    }

    /// A symlink.
    pub fn put_link(&self, path: &Path, target: &Path) {
        self.add_entry(path);
        self.links
            .borrow_mut()
            .insert(path.to_path_buf(), target.to_path_buf());
    }

    /// Give `to` the facts of `from` (a fixture standing in for a copy that already
    /// happened, without going through the code under test).
    pub fn copy_facts(&self, from: &Path, to: &Path) {
        let meta = self.entry_meta(from).expect("the source entry exists");
        if meta.kind == EntryKind::Dir {
            self.put_dir(to);
        } else {
            self.add_entry(to);
        }
        if let Some(target) = &meta.link_target {
            self.links
                .borrow_mut()
                .insert(to.to_path_buf(), target.clone());
        }
        self.meta.borrow_mut().insert(to.to_path_buf(), meta);
    }

    /// What `lstat` says about `path`, from the explicit facts or the defaults.
    fn entry_meta(&self, path: &Path) -> Option<EntryMeta> {
        if let Some(meta) = self.meta.borrow().get(path) {
            return Some(meta.clone());
        }
        let listed = path.parent().is_some_and(|parent| {
            self.tree
                .borrow()
                .get(parent)
                .zip(path.file_name())
                .is_some_and(|(entries, name)| entries.iter().any(|e| e == name))
        });
        if let Some(target) = self.links.borrow().get(path) {
            return Some(EntryMeta {
                kind: EntryKind::Symlink,
                size: 0,
                mtime_ns: 0,
                ctime_ns: 0,
                mode: 0o777,
                link_target: Some(target.clone()),
            });
        }
        if self.tree.borrow().contains_key(path) {
            return Some(EntryMeta {
                kind: EntryKind::Dir,
                size: 0,
                mtime_ns: 0,
                ctime_ns: 0,
                mode: 0o755,
                link_target: None,
            });
        }
        listed.then(|| EntryMeta {
            kind: EntryKind::File,
            size: self.files.borrow().get(path).map_or(0, |b| b.len() as u64),
            mtime_ns: 0,
            ctime_ns: 0,
            mode: 0o644,
            link_target: None,
        })
    }

    /// Drop `path` from its parent's listing and from the per-entry facts.
    fn forget_entry(&self, path: &Path) {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            if let Some(entries) = self.tree.borrow_mut().get_mut(parent) {
                entries.retain(|e| e != name);
            }
        }
        self.meta.borrow_mut().remove(path);
        self.links.borrow_mut().remove(path);
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
            let message = self.fail_message.borrow();
            let message = if message.is_empty() {
                format!("injected failure at {step:?}")
            } else {
                message.clone()
            };
            return Err(Error::new(*code, message));
        }
        if self.interrupt_after.get() == Some(step) {
            self.interrupted.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Register `path` in its parent directory listing, creating parents as needed.
    fn add_entry(&self, path: &Path) {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        if !self.tree.borrow().contains_key(parent) {
            self.add_entry(parent);
        }
        let mut tree = self.tree.borrow_mut();
        let entries = tree.entry(parent.to_path_buf()).or_default();
        if !entries.iter().any(|e| e == name) {
            entries.push(name.to_owned());
        }
    }

    fn cloned_anything(&self) -> bool {
        self.log
            .borrow()
            .iter()
            .any(|c| matches!(c, Call::Clone { .. } | Call::Copy { .. }))
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

    fn is_dirty(&self, worktree: &Path, _exclude: &[PathBuf]) -> Result<bool> {
        if self.dirty_paths.contains(worktree) {
            return Ok(true);
        }
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

    fn tracked_files(&self, _: &Path) -> Result<Vec<PathBuf>> {
        Ok(self.tracked.clone())
    }

    fn parhelignored(&self, _: &Path) -> Result<Vec<PathBuf>> {
        Ok(self.parhelignored.clone())
    }

    fn branch_exists(&self, _: &Path, name: &str) -> Result<bool> {
        Ok(self.branches.borrow().contains(name))
    }

    fn resolve_commit(&self, _: &Path, rev: &str) -> Result<String> {
        if let Some(name) = rev.strip_prefix("refs/heads/") {
            if self.branches.borrow().contains(name) {
                return Ok(self
                    .branch_tips
                    .borrow()
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| self.staging_head.clone()));
            }
        }
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
        if self.fail_after_branch.get() {
            if let BranchMode::New(name) = &spec.branch {
                self.branches.borrow_mut().insert(name.clone());
            }
        }
        self.record(Call::WorktreeAdd(spec.clone()), Step::WorktreeAdd)?;
        match (&spec.branch, &spec.reference) {
            (BranchMode::New(name), _) => {
                self.branches.borrow_mut().insert(name.clone());
            }
            // Like git: no branch flag and no ref -> a branch named after the path.
            (BranchMode::Default, None) => {
                if let Some(name) = spec.path.file_name() {
                    self.branches
                        .borrow_mut()
                        .insert(name.to_string_lossy().into_owned());
                }
            }
            _ => {}
        }
        self.tree
            .borrow_mut()
            .insert(spec.path.clone(), vec![OsString::from(".git")]);
        for (rel, bytes) in &self.staged_files {
            let abs = spec.path.join(rel);
            self.add_entry(&abs);
            self.files.borrow_mut().insert(abs, bytes.clone());
        }
        for (rel, target) in &self.staged_links {
            let abs = spec.path.join(rel);
            self.add_entry(&abs);
            self.links.borrow_mut().insert(abs, target.clone());
        }
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

    fn repoint_worktree(&self, _: &Path, _old: &Path, new: &Path) -> Result<()> {
        self.record(Call::Repair { path: new.into() }, Step::Repair)
    }

    fn worktree_remove_force(&self, _: &Path, path: &Path) -> Result<()> {
        self.record(Call::RemoveForce { path: path.into() }, Step::RemoveForce)
    }

    fn worktree_lock(&self, _: &Path, path: &Path, reason: &str) -> Result<()> {
        self.record(
            Call::WorktreeLock {
                path: path.into(),
                reason: reason.into(),
            },
            Step::WorktreeLock,
        )
    }

    fn unregister_worktree(&self, _: &Path, path: &Path) -> Result<()> {
        self.log
            .borrow_mut()
            .push(Call::Unregister { path: path.into() });
        Ok(())
    }

    fn branch_delete(&self, _: &Path, name: &str, _expected: &str) -> Result<()> {
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
            || self.files.borrow().contains_key(path)
            || self.links.borrow().contains_key(path))
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

    fn list_dir_within(&self, root: &Path, rel: &Path) -> Result<Option<Vec<OsString>>> {
        let path = root.join(rel);
        if !self.is_dir_no_follow(&path)? {
            return Ok(None);
        }
        self.list_dir(&path).map(Some)
    }

    fn clone_within(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()> {
        self.record(
            Call::Clone {
                src: root.join(rel),
                dst: dst.into(),
            },
            Step::Clone,
        )
    }

    /// Shares `Step::Clone` with `clone_within`: a failure injected there hits whichever
    /// flavour the run uses.
    fn copy_within(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()> {
        self.record(
            Call::Copy {
                src: root.join(rel),
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
        if let Some((link, now)) = self.link_tampered_at_rename.borrow_mut().take() {
            match now {
                Some(target) => self.links.borrow_mut().insert(link, target),
                None => self.links.borrow_mut().remove(&link),
            };
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        self.record(Call::RemoveDirAll(path.into()), Step::RemoveDirAll)?;
        self.tree.borrow_mut().retain(|k, _| !k.starts_with(path));
        self.meta.borrow_mut().retain(|k, _| !k.starts_with(path));
        self.forget_entry(path);
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
        // Like `flock(LOCK_NB)`: refused while another live process holds it.
        if self.locked.borrow().contains(path) {
            return Err(Error::new(
                ErrorCode::Io,
                format!("{}: Resource temporarily unavailable", path.display()),
            ));
        }
        Ok(FileLock::detached())
    }

    fn is_locked(&self, path: &Path) -> Result<bool> {
        Ok(self.locked.borrow().contains(path))
    }

    fn replace_contents(&self, path: &Path, contents: &[u8]) -> Result<()> {
        self.record(Call::Rewrite(path.into()), Step::Clone)?;
        self.files
            .borrow_mut()
            .insert(path.into(), contents.to_vec());
        Ok(())
    }

    fn replace_symlink(&self, path: &Path, target: &Path) -> Result<()> {
        self.record(
            Call::Relink {
                path: path.into(),
                target: target.into(),
            },
            Step::Clone,
        )?;
        self.links.borrow_mut().insert(path.into(), target.into());
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        self.record(Call::RemoveFile(path.into()), Step::RemoveFile)?;
        self.files.borrow_mut().remove(path);
        self.forget_entry(path);
        Ok(())
    }

    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        self.record(
            Call::CopyMode {
                from: from.into(),
                to: to.into(),
            },
            Step::Clone,
        )
    }

    fn stat_within(&self, root: &Path, rel: &Path) -> Result<Option<EntryMeta>> {
        Ok(self.entry_meta(&root.join(rel)))
    }

    fn mirror_entry(&self, root: &Path, rel: &Path, dst: &Path) -> Result<u64> {
        let src = root.join(rel);
        let meta = self.entry_meta(&src).ok_or_else(|| {
            Error::new(ErrorCode::CloneFailed, format!("{} is gone", src.display()))
        })?;
        self.record(
            Call::Mirror {
                src,
                dst: dst.into(),
            },
            Step::Mirror,
        )?;
        // The contract: same kind or nothing there. A different kind must have been
        // removed first.
        if let Some(old) = self.entry_meta(dst) {
            if old.kind != meta.kind {
                return Err(Error::new(
                    ErrorCode::Io,
                    format!("{} holds another kind of entry", dst.display()),
                ));
            }
        }
        match meta.kind {
            EntryKind::Dir => {
                self.tree.borrow_mut().entry(dst.to_path_buf()).or_default();
            }
            EntryKind::Symlink => {
                let target = meta.link_target.clone().unwrap_or_default();
                self.links.borrow_mut().insert(dst.to_path_buf(), target);
            }
            EntryKind::File | EntryKind::Fifo => {}
        }
        self.add_entry(dst);
        let copied = if meta.kind == EntryKind::File {
            meta.size
        } else {
            0
        };
        self.meta.borrow_mut().insert(dst.to_path_buf(), meta);
        Ok(copied)
    }

    fn mirror_dir_meta(&self, root: &Path, rel: &Path, dst: &Path) -> Result<()> {
        let src = root.join(rel);
        self.record(
            Call::MirrorDirMeta {
                src: src.clone(),
                dst: dst.into(),
            },
            Step::Mirror,
        )?;
        if let Some(meta) = self.entry_meta(&src) {
            self.meta.borrow_mut().insert(dst.to_path_buf(), meta);
        }
        Ok(())
    }

    fn symlink_exclusive(&self, target: &Path, link: &Path) -> Result<()> {
        self.record(
            Call::Symlink {
                target: target.into(),
                link: link.into(),
            },
            Step::Symlink,
        )?;
        if let Some((rival, target)) = self.rival_link.borrow_mut().take() {
            self.links.borrow_mut().insert(rival.clone(), target);
            self.add_entry(&rival);
        }
        if self.exists_no_follow(link)? {
            return Err(Error::new(
                ErrorCode::DestExists,
                "fake: symlink path exists",
            ));
        }
        self.links
            .borrow_mut()
            .insert(link.to_path_buf(), target.to_path_buf());
        self.add_entry(link);
        Ok(())
    }

    fn read_link(&self, path: &Path) -> Result<Option<PathBuf>> {
        Ok(self.links.borrow().get(path).cloned())
    }

    fn append_file(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.record(Call::Append(path.into()), Step::Append)?;
        self.files
            .borrow_mut()
            .entry(path.to_path_buf())
            .or_default()
            .extend_from_slice(bytes);
        self.add_entry(path);
        Ok(())
    }

    /// Not logged: a flush changes nothing a test could observe (see `flushed`).
    fn flush(&self, _: &Path) -> Result<()> {
        self.flushes.set(self.flushes.get() + 1);
        if self.fail_flush.get() {
            return Err(Error::new(
                ErrorCode::Io,
                "No space left on device (os error 28)",
            ));
        }
        Ok(())
    }

    fn is_writable(&self, path: &Path) -> Result<bool> {
        Ok(!self.unwritable.borrow().contains(path))
    }

    fn space(&self, path: &Path) -> Result<Space> {
        if self.space_unavailable.get() {
            return Err(Error::new(
                ErrorCode::Io,
                format!("{}: cannot measure", path.display()),
            ));
        }
        Ok(self
            .spaces
            .borrow()
            .iter()
            .filter(|(prefix, _)| path.starts_with(prefix))
            .max_by_key(|(prefix, _)| prefix.components().count())
            .map_or(
                Space {
                    total: 1 << 40,
                    free: 1 << 39,
                },
                |(_, space)| *space,
            ))
    }

    fn scan(
        &self,
        root: &Path,
        needles: &[Vec<u8>],
        max_len: u64,
        skip: &(dyn Fn(&Path) -> bool + Sync),
    ) -> Result<Scan> {
        let skipped = |rel: &Path| {
            rel.ancestors()
                .any(|a| !a.as_os_str().is_empty() && skip(a))
        };
        let contains = |bytes: &[u8]| {
            needles
                .iter()
                .any(|n| bytes.windows(n.len()).any(|w| w == n.as_slice()))
        };
        let mut scan = Scan::default();
        for (path, bytes) in self.files.borrow().iter() {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            if rel.as_os_str().is_empty() || skipped(rel) {
                continue;
            }
            if rel.file_name().is_some_and(|n| n == ".git") {
                scan.gitfiles.push(rel.to_path_buf());
            } else if !self.big.borrow().contains(path)
                && bytes.len() as u64 <= max_len
                && contains(bytes)
            {
                scan.matches.push(rel.to_path_buf());
            }
        }
        for (path, target) in self.links.borrow().iter() {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            if !skipped(rel)
                && needles
                    .iter()
                    .any(|n| target.as_os_str().as_encoded_bytes().starts_with(n))
            {
                scan.links.push((rel.to_path_buf(), target.clone()));
            }
        }
        scan.matches.sort();
        scan.gitfiles.sort();
        scan.links.sort();
        Ok(scan)
    }
}

// ------------------------------------------------------------------------ the host

/// A logged mutation of the machine, for `setup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCall {
    Run {
        program: String,
        args: Vec<String>,
    },
    CreateDir {
        path: PathBuf,
        mode: u32,
        owner: Owner,
    },
    WriteFile {
        path: PathBuf,
        mode: u32,
        owner: Owner,
    },
    CreateImage {
        path: PathBuf,
        bytes: u64,
        preallocate: bool,
    },
    VerifyImmutable {
        dir: PathBuf,
        image: PathBuf,
    },
    RemoveFile(PathBuf),
    RemoveDir(PathBuf),
}

impl HostCall {
    /// One line per call, the way the tests spell the expected sequences.
    pub fn line(&self) -> String {
        match self {
            Self::Run { program, args } => format!("run {program} {}", args.join(" ")),
            Self::CreateDir { path, .. } => format!("mkdir {}", path.display()),
            Self::WriteFile { path, .. } => format!("write {}", path.display()),
            Self::CreateImage {
                path,
                bytes,
                preallocate: false,
            } => format!("image {} {bytes} sparse", path.display()),
            Self::CreateImage {
                path,
                bytes,
                preallocate: true,
            } => format!("image {} {bytes} allocated", path.display()),
            Self::VerifyImmutable { dir, image } => {
                format!("verify-immutable {} {}", dir.display(), image.display())
            }
            Self::RemoveFile(path) => format!("rm {}", path.display()),
            Self::RemoveDir(path) => format!("rmdir {}", path.display()),
        }
    }
}

/// The unit directory the fake systemd reads.
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// A fake machine for `setup`: an ordered log of mutations and just enough
/// simulation (files, mounts, immutable directories, units) for the answers to stay
/// consistent across a re-run. Defaults: Linux, root via `sudo` as `alice`, systemd,
/// btrfs in the kernel, every tool installed, nothing set up.
// Each bool is an independent fact of the machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct FakeHost {
    pub log: RefCell<Vec<HostCall>>,
    pub linux: bool,
    pub root: bool,
    pub env: HashMap<String, String>,
    pub users: Vec<UserInfo>,
    pub tools: RefCell<HashSet<String>>,
    pub systemd: bool,
    /// Text files by path, including `/proc/filesystems` and the units.
    pub files: RefCell<HashMap<PathBuf, String>>,
    pub dirs: RefCell<HashSet<PathBuf>>,
    pub images: RefCell<HashMap<PathBuf, ImageUsage>>,
    /// Images `mkfs.btrfs` ran on.
    pub formatted: RefCell<HashSet<PathBuf>>,
    /// Images that hold some other filesystem, by type (not made by `setup`).
    pub foreign: RefCell<HashMap<PathBuf, String>>,
    pub mounted: RefCell<HashSet<PathBuf>>,
    pub immutable: RefCell<HashSet<PathBuf>>,
    /// Modes and owners last given to a path.
    pub attrs: RefCell<HashMap<PathBuf, (u32, Owner)>>,
    /// Programs that fail when run.
    pub fail_run: RefCell<HashMap<String, ErrorCode>>,
    /// A `chattr` that succeeds without setting anything (a filesystem that ignores it).
    pub chattr_does_nothing: bool,
}

impl Default for FakeHost {
    fn default() -> Self {
        let user = |name: &str, id: u32| UserInfo {
            name: name.into(),
            uid: id,
            gid: id,
        };
        let tools = [
            "mkfs.btrfs",
            "chattr",
            "systemctl",
            "systemd-escape",
            "modprobe",
        ];
        Self {
            log: RefCell::default(),
            linux: true,
            root: true,
            env: HashMap::from([("SUDO_USER".to_owned(), "alice".to_owned())]),
            users: vec![user("root", 0), user("alice", 1000), user("bob", 1001)],
            tools: RefCell::new(tools.iter().map(|t| (*t).to_owned()).collect()),
            systemd: true,
            files: RefCell::new(HashMap::from([(
                p("/proc/filesystems"),
                "nodev\tsysfs\nnodev\ttmpfs\n\text4\n\tbtrfs\n".to_owned(),
            )])),
            dirs: RefCell::new(
                ["/", "/etc", "/etc/systemd", UNIT_DIR, "/var", "/var/lib"]
                    .iter()
                    .map(|d| p(d))
                    .collect(),
            ),
            images: RefCell::default(),
            formatted: RefCell::default(),
            foreign: RefCell::default(),
            mounted: RefCell::default(),
            immutable: RefCell::default(),
            attrs: RefCell::default(),
            fail_run: RefCell::default(),
            chattr_does_nothing: false,
        }
    }
}

impl FakeHost {
    pub fn calls(&self) -> Vec<HostCall> {
        self.log.borrow().clone()
    }

    pub fn lines(&self) -> Vec<String> {
        self.log.borrow().iter().map(HostCall::line).collect()
    }

    pub fn clear_log(&self) {
        self.log.borrow_mut().clear();
    }

    pub fn without_tool(self, tool: &str) -> Self {
        self.tools.borrow_mut().remove(tool);
        self
    }

    pub fn failing(self, program: &str, code: ErrorCode) -> Self {
        self.fail_run.borrow_mut().insert(program.to_owned(), code);
        self
    }

    pub fn text(&self, path: &str) -> Option<String> {
        self.files.borrow().get(Path::new(path)).cloned()
    }

    /// A mutation is refused inside an immutable directory, or inside a mount point
    /// that is not mounted (it would land on the host filesystem underneath).
    fn guard(&self, path: &Path, creating_or_removing: bool) -> Result<()> {
        let parent = path.parent().unwrap_or(Path::new("/"));
        if creating_or_removing && self.immutable.borrow().contains(parent) {
            return Err(Error::new(
                ErrorCode::Io,
                format!("{}: Operation not permitted", path.display()),
            ));
        }
        let unmounted_mount_point = self.files.borrow().iter().any(|(unit, text)| {
            unit.starts_with(UNIT_DIR)
                && mount_point_of(text).is_some_and(|w| {
                    path.starts_with(&w) && path != w && !self.mounted.borrow().contains(&w)
                })
        });
        if unmounted_mount_point {
            return Err(Error::new(
                ErrorCode::Io,
                format!(
                    "{} is below a mount point that is not mounted",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    fn log(&self, call: HostCall) {
        self.log.borrow_mut().push(call);
    }

    /// Whether the contents of the store's filesystem are out of reach at `path`: the
    /// root of a store is the directory that holds its marker, and while it is not
    /// mounted whatever is below it lives on the image, not on the host. `include_root`
    /// counts the root's own entries (its listing), which the image holds too.
    fn out_of_reach(&self, path: &Path, include_root: bool) -> bool {
        let mounted = self.mounted.borrow();
        self.files
            .borrow()
            .keys()
            .filter(|f| f.file_name().is_some_and(|n| n == ".parhel-store"))
            .filter_map(|f| f.parent())
            .any(|root| {
                !mounted.contains(root) && path.starts_with(root) && (include_root || path != root)
            })
    }

    fn remember(&self, path: &Path, mode: u32, owner: Owner) {
        self.attrs
            .borrow_mut()
            .insert(path.to_path_buf(), (mode, owner));
    }
}

/// `Where=` of a unit file.
fn mount_point_of(unit: &str) -> Option<PathBuf> {
    unit.lines()
        .find_map(|l| l.strip_prefix("Where="))
        .map(PathBuf::from)
}

impl Host for FakeHost {
    fn is_linux(&self) -> bool {
        self.linux
    }

    fn is_root(&self) -> bool {
        self.root
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }

    fn user(&self, name: &str) -> Result<Option<UserInfo>> {
        Ok(self.users.iter().find(|u| u.name == name).cloned())
    }

    fn tool_exists(&self, tool: &str) -> bool {
        self.tools.borrow().contains(tool)
    }

    fn systemd_running(&self) -> bool {
        self.systemd
    }

    fn read_text(&self, path: &Path) -> Result<Option<String>> {
        if self.out_of_reach(path, false) {
            return Ok(None);
        }
        Ok(self.files.borrow().get(path).cloned())
    }

    fn exists(&self, path: &Path) -> bool {
        !self.out_of_reach(path, false)
            && (self.files.borrow().contains_key(path)
                || self.dirs.borrow().contains(path)
                || self.images.borrow().contains_key(path))
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<OsString>> {
        if self.out_of_reach(path, true) {
            return Ok(Vec::new());
        }
        let files = self.files.borrow();
        let dirs = self.dirs.borrow();
        let images = self.images.borrow();
        let mut names: Vec<OsString> = files
            .keys()
            .chain(dirs.iter())
            .chain(images.keys())
            .filter(|c| c.parent() == Some(path))
            .filter_map(|c| c.file_name().map(OsString::from))
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn is_mount_point(&self, path: &Path) -> bool {
        self.mounted.borrow().contains(path)
    }

    fn is_immutable(&self, path: &Path) -> bool {
        self.immutable.borrow().contains(path)
    }

    fn filesystem_of(&self, image: &Path) -> Result<Option<String>> {
        if !self.images.borrow().contains_key(image) {
            return Err(Error::new(
                ErrorCode::Io,
                format!("{}: no such file", image.display()),
            ));
        }
        if let Some(kind) = self.foreign.borrow().get(image) {
            return Ok(Some(kind.clone()));
        }
        Ok(self
            .formatted
            .borrow()
            .contains(image)
            .then(|| "btrfs".to_owned()))
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        // Queries are not mutations: not logged.
        if program == "systemd-escape" {
            let name = args.last().copied().unwrap_or_default();
            // Like systemd: `/` becomes `-`, a literal `-` becomes `\x2d`.
            let escaped = name
                .trim_matches('/')
                .split('/')
                .map(|part| part.replace('-', "\\x2d"))
                .collect::<Vec<_>>()
                .join("-");
            let suffix = args
                .iter()
                .find_map(|a| a.strip_prefix("--suffix="))
                .map_or(String::new(), |s| format!(".{s}"));
            return Ok(format!("{escaped}{suffix}\n"));
        }
        self.log(HostCall::Run {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        });
        if let Some(code) = self.fail_run.borrow().get(program) {
            return Err(Error::new(*code, format!("fake: {program} failed")));
        }
        match (program, args) {
            ("modprobe", ["btrfs"]) => {
                self.files
                    .borrow_mut()
                    .entry(p("/proc/filesystems"))
                    .or_default()
                    .push_str("\tbtrfs\n");
            }
            ("mkfs.btrfs", [.., image]) => {
                if !self.images.borrow().contains_key(Path::new(image)) {
                    return Err(Error::new(ErrorCode::Io, format!("{image}: no such file")));
                }
                self.formatted.borrow_mut().insert(p(image));
            }
            ("systemctl", ["enable", "--now", unit] | ["start", unit]) => {
                let text = self
                    .files
                    .borrow()
                    .get(&Path::new(UNIT_DIR).join(unit))
                    .cloned()
                    .ok_or_else(|| Error::new(ErrorCode::Io, format!("unit {unit} not found")))?;
                if let Some(at) = mount_point_of(&text) {
                    self.mounted.borrow_mut().insert(at);
                }
            }
            ("umount", [at]) => {
                self.mounted.borrow_mut().remove(Path::new(at));
            }
            ("systemctl", ["disable", "--now", unit] | ["stop", unit]) => {
                let text = self
                    .files
                    .borrow()
                    .get(&Path::new(UNIT_DIR).join(unit))
                    .cloned();
                if let Some(at) = text.as_deref().and_then(mount_point_of) {
                    self.mounted.borrow_mut().remove(&at);
                }
            }
            ("chattr", ["+i", path]) => {
                if !self.chattr_does_nothing {
                    self.immutable.borrow_mut().insert(p(path));
                }
            }
            ("chattr", ["-i", path]) => {
                self.immutable.borrow_mut().remove(Path::new(path));
            }
            _ => {}
        }
        Ok(String::new())
    }

    fn create_dir(&self, path: &Path, mode: u32, owner: Owner) -> Result<()> {
        self.log(HostCall::CreateDir {
            path: path.into(),
            mode,
            owner,
        });
        if !self.dirs.borrow().contains(path) {
            self.guard(path, true)?;
            self.dirs.borrow_mut().insert(path.into());
        }
        self.remember(path, mode, owner);
        Ok(())
    }

    fn write_file(&self, path: &Path, contents: &[u8], mode: u32, owner: Owner) -> Result<()> {
        self.log(HostCall::WriteFile {
            path: path.into(),
            mode,
            owner,
        });
        self.guard(path, !self.files.borrow().contains_key(path))?;
        self.files
            .borrow_mut()
            .insert(path.into(), String::from_utf8_lossy(contents).into_owned());
        self.remember(path, mode, owner);
        Ok(())
    }

    fn create_image(&self, path: &Path, bytes: u64, preallocate: bool) -> Result<()> {
        self.log(HostCall::CreateImage {
            path: path.into(),
            bytes,
            preallocate,
        });
        self.guard(path, true)?;
        self.images.borrow_mut().insert(
            path.into(),
            ImageUsage {
                apparent: bytes,
                allocated: if preallocate { bytes } else { 4096 },
            },
        );
        self.remember(path, 0o600, None);
        Ok(())
    }

    fn image_usage(&self, path: &Path) -> Result<ImageUsage> {
        self.images
            .borrow()
            .get(path)
            .copied()
            .ok_or_else(|| Error::new(ErrorCode::Io, format!("no image {}", path.display())))
    }

    fn verify_immutable(&self, dir: &Path, image: &Path) -> Result<()> {
        self.log(HostCall::VerifyImmutable {
            dir: dir.into(),
            image: image.into(),
        });
        if self.immutable.borrow().contains(dir) {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::Io,
                format!("{} can be modified: it is not immutable", image.display()),
            ))
        }
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        self.log(HostCall::RemoveFile(path.into()));
        self.guard(path, true)?;
        self.files.borrow_mut().remove(path);
        self.images.borrow_mut().remove(path);
        // A new file at that path starts blank.
        self.formatted.borrow_mut().remove(path);
        self.foreign.borrow_mut().remove(path);
        Ok(())
    }

    fn remove_dir(&self, path: &Path) -> Result<()> {
        self.log(HostCall::RemoveDir(path.into()));
        self.guard(path, true)?;
        if self.mounted.borrow().contains(path) || !self.list_dir(path)?.is_empty() {
            return Err(Error::new(
                ErrorCode::Io,
                format!("{}: Directory not empty or busy", path.display()),
            ));
        }
        // What was below lived on the filesystem the directory hid: gone with its image.
        self.files.borrow_mut().retain(|f, _| !f.starts_with(path));
        self.dirs
            .borrow_mut()
            .retain(|d| d != path && !d.starts_with(path));
        Ok(())
    }
}
