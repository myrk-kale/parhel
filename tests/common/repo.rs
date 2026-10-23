//! Isolated git sandboxes, repository builders and invariant snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use tempfile::TempDir;

/// Variables that could leak the developer's git setup into tests.
const LEAKY_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_EXEC_PATH",
    "GIT_TEMPLATE_DIR",
    "PARHEL_FAILPOINTS",
];

/// Hermetic git configuration: own HOME, global config, no system config.
#[derive(Debug)]
pub struct GitEnv {
    pub home: PathBuf,
    pub global_config: PathBuf,
    /// Extra entries prepended to PATH for every command (e.g. fake git, installed binary).
    pub path_prefix: Vec<PathBuf>,
}

impl GitEnv {
    pub fn vars(&self) -> Vec<(OsString, OsString)> {
        let mut path = OsString::new();
        for p in &self.path_prefix {
            path.push(p);
            path.push(":");
        }
        path.push(std::env::var_os("PATH").unwrap_or_default());
        vec![
            ("HOME".into(), self.home.clone().into()),
            (
                "GIT_CONFIG_GLOBAL".into(),
                self.global_config.clone().into(),
            ),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            // A store that does not exist, so a developer's own `/parhel/<user>` can
            // never change what a test sees. Tests of the store override it.
            ("PARHEL_STORE".into(), self.home.join("no-store").into()),
            ("LC_ALL".into(), "C".into()),
            ("TZ".into(), "UTC".into()),
            ("PATH".into(), path),
        ]
    }

    /// Apply the hermetic environment to any command.
    pub fn apply(&self, cmd: &mut Command) {
        for var in LEAKY_ENV {
            cmd.env_remove(var);
        }
        cmd.envs(self.vars());
    }

    pub fn git_command(&self, cwd: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd);
        self.apply(&mut cmd);
        cmd
    }
}

/// A temp directory with its own git identity. Everything a test creates lives here.
pub struct Sandbox {
    _dir: TempDir,
    /// Canonical root (macOS: `/private/var/...`).
    pub root: PathBuf,
    pub env: Arc<GitEnv>,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("parhel-it-")
            .tempdir()
            .unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        Self::with_root(dir, root)
    }

    /// A sandbox whose directory lives in `base` (e.g. a mount of a specific
    /// filesystem on Linux) instead of `$TMPDIR`.
    pub fn new_in(base: &Path) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("parhel-it-")
            .tempdir_in(base)
            .unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        Self::with_root(dir, root)
    }

    fn with_root(dir: TempDir, root: PathBuf) -> Self {
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let global_config = home.join(".gitconfig");
        fs::write(
            &global_config,
            "[user]\n\tname = Parhel Test\n\temail = test@parhel.invalid\n\
             [init]\n\tdefaultBranch = main\n\
             [commit]\n\tgpgsign = false\n\
             [tag]\n\tgpgsign = false\n\
             [protocol \"file\"]\n\tallow = always\n\
             [advice]\n\tdetachedHead = false\n\
             [core]\n\tautocrlf = false\n",
        )
        .unwrap();
        let env = Arc::new(GitEnv {
            home,
            global_config,
            path_prefix: vec![],
        });
        Self {
            _dir: dir,
            root,
            env,
        }
    }

    /// Same sandbox identity but with extra PATH entries (fake git, installed bin).
    pub fn with_path_prefix(&mut self, dirs: Vec<PathBuf>) {
        let env = Arc::get_mut(&mut self.env).expect("set PATH before creating repos");
        env.path_prefix = dirs;
    }

    pub fn path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(rel)
    }

    /// `git init` a repo at `<sandbox>/<rel>` with one commit containing `README.md`.
    pub fn repo(&self, rel: &str) -> TestRepo {
        TestRepo::init_at(&self.env, &self.path(rel))
    }

    /// `git init` with no commits (unborn HEAD).
    pub fn empty_repo(&self, rel: &str) -> TestRepo {
        TestRepo::init_empty_at(&self.env, &self.path(rel))
    }
}

impl Drop for Sandbox {
    /// Tests create immutable (`uchg`, macOS) and unreadable fixtures; make everything
    /// deletable before `TempDir` cleans up.
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        let _ = Command::new("chflags")
            .args(["-R", "nouchg"])
            .arg(&self.root)
            .status();
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.root)
            .status();
    }
}

/// Run git, panic with full output on failure, return trimmed stdout.
pub fn git_in(env: &GitEnv, cwd: &Path, args: &[&str]) -> String {
    let out = env.git_command(cwd).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}\nstdout: {}\nstderr: {}",
        cwd.display(),
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
}

#[derive(Clone)]
pub struct TestRepo {
    pub root: PathBuf,
    pub env: Arc<GitEnv>,
}

impl TestRepo {
    pub fn init_at(env: &Arc<GitEnv>, root: &Path) -> Self {
        let repo = Self::init_empty_at(env, root);
        repo.write("README.md", "# test repo\n");
        repo.commit_all("initial");
        repo
    }

    pub fn init_empty_at(env: &Arc<GitEnv>, root: &Path) -> Self {
        fs::create_dir_all(root).unwrap();
        git_in(env, root, &["init", "-q", "-b", "main"]);
        Self {
            root: fs::canonicalize(root).unwrap(),
            env: Arc::clone(env),
        }
    }

    /// Handle to an existing worktree (e.g. one created by the product).
    pub fn open(env: &Arc<GitEnv>, root: &Path) -> Self {
        Self {
            root: fs::canonicalize(root).unwrap(),
            env: Arc::clone(env),
        }
    }

    pub fn path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(rel)
    }

    pub fn git(&self, args: &[&str]) -> String {
        git_in(&self.env, &self.root, args)
    }

    pub fn git_output(&self, args: &[&str]) -> Output {
        self.env
            .git_command(&self.root)
            .args(args)
            .output()
            .unwrap()
    }

    // ---- file helpers

    pub fn write(&self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    pub fn read(&self, rel: impl AsRef<Path>) -> String {
        fs::read_to_string(self.path(rel)).unwrap()
    }

    pub fn mkdir(&self, rel: impl AsRef<Path>) {
        fs::create_dir_all(self.path(rel)).unwrap();
    }

    pub fn symlink(&self, rel: impl AsRef<Path>, target: impl AsRef<Path>) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        symlink(target, p).unwrap();
    }

    pub fn chmod(&self, rel: impl AsRef<Path>, mode: u32) {
        fs::set_permissions(self.path(rel), fs::Permissions::from_mode(mode)).unwrap();
    }

    pub fn remove(&self, rel: impl AsRef<Path>) {
        let p = self.path(rel);
        if fs::symlink_metadata(&p).unwrap().is_dir() {
            fs::remove_dir_all(p).unwrap();
        } else {
            fs::remove_file(p).unwrap();
        }
    }

    pub fn exists(&self, rel: impl AsRef<Path>) -> bool {
        fs::symlink_metadata(self.path(rel)).is_ok()
    }

    /// Append patterns to `.gitignore` and commit it.
    pub fn ignore(&self, patterns: &[&str]) {
        let mut text = fs::read_to_string(self.path(".gitignore")).unwrap_or_default();
        for p in patterns {
            text.push_str(p);
            text.push('\n');
        }
        self.write(".gitignore", text);
        self.git(&["add", ".gitignore"]);
        self.git(&["commit", "-q", "-m", "ignore"]);
    }

    // ---- git helpers

    pub fn commit_all(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Short symbolic HEAD (`main`) or `None` when detached.
    pub fn current_branch(&self) -> Option<String> {
        let out = self.git_output(&["symbolic-ref", "-q", "--short", "HEAD"]);
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
    }

    pub fn branch_tip(&self, name: &str) -> Option<String> {
        let out = self.git_output(&["rev-parse", "-q", "--verify", &format!("refs/heads/{name}")]);
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
    }

    pub fn branches(&self) -> BTreeMap<String, String> {
        self.git(&[
            "for-each-ref",
            "--format=%(refname:short) %(objectname)",
            "refs/heads",
        ])
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, s)| (n.to_owned(), s.to_owned()))
        .collect()
    }

    /// `git status --porcelain=v1 --untracked-files=all` (ignored excluded).
    pub fn status(&self) -> String {
        self.git(&["status", "--porcelain=v1", "--untracked-files=all"])
    }

    pub fn assert_clean(&self) {
        let s = self.status();
        assert!(
            s.is_empty(),
            "worktree {} not clean:\n{s}",
            self.root.display()
        );
    }

    /// Registered worktree paths (canonical as git prints them).
    pub fn worktree_paths(&self) -> Vec<PathBuf> {
        self.git(&["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect()
    }

    // Without `--path-format=absolute` (git 2.31): the suite also runs on git 2.29.
    pub fn common_dir(&self) -> PathBuf {
        let p = PathBuf::from(self.git(&["rev-parse", "--git-common-dir"]));
        fs::canonicalize(if p.is_absolute() {
            p
        } else {
            self.root.join(p)
        })
        .unwrap()
    }

    pub fn git_dir(&self) -> PathBuf {
        fs::canonicalize(self.git(&["rev-parse", "--absolute-git-dir"])).unwrap()
    }

    /// Plain `git worktree add -b <branch> <path>`; returns a handle to it.
    pub fn linked_worktree(&self, path: &Path, branch: &str) -> TestRepo {
        self.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            path.to_str().unwrap(),
        ]);
        Self::open(&self.env, path)
    }

    /// `git submodule add <sub> <path>` + commit.
    pub fn add_submodule(&self, sub: &TestRepo, path: &str) {
        self.git(&["submodule", "add", "-q", sub.root.to_str().unwrap(), path]);
        self.git(&["commit", "-q", "-m", &format!("add submodule {path}")]);
    }

    // ---- invariants

    /// Everything a failed `add` must leave unchanged.
    pub fn snapshot(&self) -> RepoSnapshot {
        let admin = self.common_dir().join("worktrees");
        let admin_dirs = fs::read_dir(&admin)
            .map(|rd| rd.map(|e| e.unwrap().file_name()).collect())
            .unwrap_or_default();
        RepoSnapshot {
            head: self.head(),
            status: self.status(),
            worktrees: self.git(&["worktree", "list", "--porcelain"]),
            branches: self.branches(),
            admin_dirs,
            tree: tree_digest(&self.root),
        }
    }
}

/// Repository state that must survive a failed or refused `add` untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSnapshot {
    pub head: String,
    pub status: String,
    pub worktrees: String,
    pub branches: BTreeMap<String, String>,
    pub admin_dirs: BTreeSet<OsString>,
    /// Source files outside `.git`: path -> (kind, mode, content hash / link target).
    pub tree: BTreeMap<PathBuf, String>,
}

impl RepoSnapshot {
    pub fn assert_same(&self, after: &RepoSnapshot) {
        let mut diff = String::new();
        if self.head != after.head {
            writeln!(diff, "HEAD: {} -> {}", self.head, after.head).unwrap();
        }
        if self.status != after.status {
            writeln!(diff, "status:\n{}\n->\n{}", self.status, after.status).unwrap();
        }
        if self.worktrees != after.worktrees {
            writeln!(
                diff,
                "worktrees:\n{}\n->\n{}",
                self.worktrees, after.worktrees
            )
            .unwrap();
        }
        if self.branches != after.branches {
            writeln!(
                diff,
                "branches: {:?} -> {:?}",
                self.branches, after.branches
            )
            .unwrap();
        }
        if self.admin_dirs != after.admin_dirs {
            writeln!(
                diff,
                ".git/worktrees: {:?} -> {:?}",
                self.admin_dirs, after.admin_dirs
            )
            .unwrap();
        }
        for (k, v) in &self.tree {
            match after.tree.get(k) {
                None => writeln!(diff, "removed {}", k.display()).unwrap(),
                Some(a) if a != v => writeln!(diff, "changed {}: {v} -> {a}", k.display()).unwrap(),
                Some(_) => {}
            }
        }
        for k in after.tree.keys().filter(|k| !self.tree.contains_key(*k)) {
            writeln!(diff, "added {}", k.display()).unwrap();
        }
        assert!(diff.is_empty(), "repository changed:\n{diff}");
    }
}

/// Digest of every entry under `root` except the top-level `.git`.
pub fn tree_digest(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    walk(root, Path::new(""), &mut out);
    out
}

fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, String>) {
    let Ok(rd) = fs::read_dir(root.join(rel)) else {
        out.insert(rel.to_path_buf(), "unreadable-dir".into());
        return;
    };
    for entry in rd {
        let entry = entry.unwrap();
        let child = rel.join(entry.file_name());
        if child == Path::new(".git") {
            continue;
        }
        let md = fs::symlink_metadata(entry.path()).unwrap();
        let mode = md.permissions().mode() & 0o7777;
        let desc = if md.file_type().is_symlink() {
            format!("link {}", fs::read_link(entry.path()).unwrap().display())
        } else if md.is_dir() {
            walk(root, &child, out);
            format!("dir {mode:o}")
        } else {
            match fs::read(entry.path()) {
                Ok(bytes) => {
                    let mut h = DefaultHasher::new();
                    bytes.hash(&mut h);
                    format!("file {mode:o} {:016x}", h.finish())
                }
                Err(_) => format!("file {mode:o} unreadable"),
            }
        };
        out.insert(child, desc);
    }
}

/// `.parhel-staging-*` entries directly inside any of `dirs`.
pub fn staging_leftovers(dirs: &[&Path]) -> Vec<PathBuf> {
    dirs.iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flat_map(|rd| rd.map(|e| e.unwrap().path()))
        .filter(|p| {
            p.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|n| n.starts_with(".parhel-staging-"))
        })
        .collect()
}

pub fn assert_no_staging(dirs: &[&Path]) {
    let left = staging_leftovers(dirs);
    assert!(left.is_empty(), "staging leftovers: {left:?}");
}

/// Restores permissions on drop so `TempDir` cleanup can delete chmod-000 fixtures.
pub struct RestoreMode(pub PathBuf, pub u32);

impl Drop for RestoreMode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
    }
}
