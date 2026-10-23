//! A store for integration tests: a fresh directory on btrfs holding the marker
//! `git parhel setup` writes, which is all `add` and `doctor` need to use it.
//!
//! The directory is announced to the binary through `PARHEL_STORE`; the tests that
//! exercise the real default location (`/parhel/<user>`) live in `tests/setup.rs`.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use super::cli::parhel_cmd;
use super::repo::GitEnv;

/// What `setup` writes into `.parhel-store` for a user.
pub fn marker_text(user: &str) -> String {
    marker_text_for(user, Path::new(&format!("/var/lib/parhel/{user}.img")))
}

/// ... for an image kept somewhere else.
pub fn marker_text_for(user: &str, image: &Path) -> String {
    format!(
        "format=1\nuser={user}\nimage={}\nunit=parhel-{user}.mount\n",
        image.display()
    )
}

/// What the binary is told about the room on every disk (`PARHEL_SPACE`, a hook that only
/// exists with the `failpoints` feature): a terabyte with half of it free, wherever. The
/// checks of how much room there is - `store-low-space` in `doctor`, `store-no-space` in
/// `add` - would otherwise depend on the disk the tests happen to run on (the btrfs of the
/// test container is 1 GiB). A test of what happens on a real small disk takes this off
/// with `env_remove("PARHEL_SPACE")`.
pub const PLENTY_OF_ROOM: &str = "/=1099511627776:549755813888";

/// Name of the user running the tests.
pub fn current_user() -> String {
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    let out = Command::new("id").args(["-un", &uid.to_string()]).output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => format!("uid{uid}"),
    }
}

#[derive(Debug)]
pub struct TestStore {
    _dir: TempDir,
    /// Canonical root of the store (what `PARHEL_STORE` names).
    pub root: PathBuf,
}

impl TestStore {
    /// A store inside `base` (a directory on btrfs): marker, `baselines/`, `worktrees/`.
    pub fn new(base: &Path) -> Self {
        Self::with_marker(base, None)
    }

    /// ... whose marker says that the image lives at `image`.
    pub fn with_image(base: &Path, image: &Path) -> Self {
        Self::with_marker(base, Some(image))
    }

    fn with_marker(base: &Path, image: Option<&Path>) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("parhel-store-")
            .tempdir_in(base)
            .unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let marker = match image {
            Some(image) => marker_text_for(&current_user(), image),
            None => marker_text(&current_user()),
        };
        fs::write(root.join(".parhel-store"), marker).unwrap();
        fs::create_dir(root.join("baselines")).unwrap();
        fs::create_dir(root.join("worktrees")).unwrap();
        Self { _dir: dir, root }
    }

    pub fn baselines_dir(&self) -> PathBuf {
        self.root.join("baselines")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    /// Directory names directly under `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The baselines present (`baselines/<id>` directories).
    pub fn baselines(&self) -> Vec<PathBuf> {
        Self::names(&self.baselines_dir())
            .into_iter()
            .map(|n| self.baselines_dir().join(n))
            .collect()
    }

    /// The only baseline (tests use one repository), its `tree/` directory.
    pub fn baseline_tree(&self) -> PathBuf {
        let all = self.baselines();
        assert_eq!(all.len(), 1, "expected one baseline, found {all:?}");
        all[0].join("tree")
    }

    /// The real worktrees: `worktrees/<id>/<name>`, sorted by path. Staging
    /// directories (`.parhel-staging-*`) are not worktrees.
    pub fn worktrees(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for id in Self::names(&self.worktrees_dir()) {
            if id.starts_with(".parhel-staging-") {
                continue;
            }
            for name in Self::names(&self.worktrees_dir().join(&id)) {
                found.push(self.worktrees_dir().join(&id).join(name));
            }
        }
        found
    }

    /// The only real worktree.
    pub fn only_worktree(&self) -> PathBuf {
        let all = self.worktrees();
        assert_eq!(all.len(), 1, "expected one worktree, found {all:?}");
        all[0].clone()
    }

    /// Everything that may not be left behind by a failed run.
    pub fn assert_no_staging(&self) {
        super::repo::assert_no_staging(&[&self.worktrees_dir(), &self.root]);
    }
}

/// `git-parhel <args>` in `cwd` using `store`.
pub fn parhel_store_cmd<S: AsRef<OsStr>>(
    env: &GitEnv,
    cwd: &Path,
    args: &[S],
    store: &TestStore,
) -> Command {
    let mut cmd = parhel_cmd(env, cwd, args);
    cmd.env("PARHEL_STORE", &store.root)
        .env("PARHEL_SPACE", PLENTY_OF_ROOM);
    cmd
}

pub fn parhel_store<S: AsRef<OsStr>>(
    env: &GitEnv,
    cwd: &Path,
    args: &[S],
    store: &TestStore,
) -> Output {
    parhel_store_cmd(env, cwd, args, store).output().unwrap()
}

/// Same, with `PARHEL_FAILPOINTS` set.
pub fn parhel_store_with_failpoints<S: AsRef<OsStr>>(
    env: &GitEnv,
    cwd: &Path,
    args: &[S],
    store: &TestStore,
    failpoints: &str,
) -> Output {
    parhel_store_cmd(env, cwd, args, store)
        .env("PARHEL_FAILPOINTS", failpoints)
        .output()
        .unwrap()
}

/// Same, spawned (for races and signals).
pub fn spawn_parhel_store<S: AsRef<OsStr>>(
    env: &GitEnv,
    cwd: &Path,
    args: &[S],
    store: &TestStore,
    failpoints: Option<&str>,
) -> std::process::Child {
    let mut cmd = parhel_store_cmd(env, cwd, args, store);
    if let Some(fp) = failpoints {
        cmd.env("PARHEL_FAILPOINTS", fp);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}
