//! Running the `git-parhel` binary and coordinating with its failpoints.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::repo::GitEnv;

pub fn parhel_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_git-parhel"))
}

/// `git-parhel <args>` in `cwd` with the hermetic git environment.
pub fn parhel_cmd<S: AsRef<OsStr>>(env: &GitEnv, cwd: &Path, args: &[S]) -> Command {
    let mut cmd = Command::new(parhel_bin());
    cmd.current_dir(cwd).args(args);
    env.apply(&mut cmd);
    cmd
}

pub fn parhel<S: AsRef<OsStr>>(env: &GitEnv, cwd: &Path, args: &[S]) -> Output {
    parhel_cmd(env, cwd, args).output().unwrap()
}

/// Same, with `PARHEL_FAILPOINTS` set.
pub fn parhel_with_failpoints<S: AsRef<OsStr>>(
    env: &GitEnv,
    cwd: &Path,
    args: &[S],
    failpoints: &str,
) -> Output {
    parhel_cmd(env, cwd, args)
        .env("PARHEL_FAILPOINTS", failpoints)
        .output()
        .unwrap()
}

pub fn spawn_parhel<S: AsRef<OsStr>>(
    env: &GitEnv,
    cwd: &Path,
    args: &[S],
    failpoints: Option<&str>,
) -> Child {
    let mut cmd = parhel_cmd(env, cwd, args);
    if let Some(fp) = failpoints {
        cmd.env("PARHEL_FAILPOINTS", fp);
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn describe(out: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        stdout(out),
        stderr(out)
    )
}

pub fn assert_success(out: &Output) {
    assert!(out.status.success(), "expected success\n{}", describe(out));
}

/// Exit status 1 (2 for `usage`, 130 for `interrupted`) and `error[<code>]` on stderr.
pub fn assert_error(out: &Output, code: &str) {
    let want = match code {
        "usage" => 2,
        "interrupted" => 130,
        _ => 1,
    };
    assert_eq!(
        out.status.code(),
        Some(want),
        "wrong exit status for {code}\n{}",
        describe(out)
    );
    assert!(
        stderr(out).contains(&format!("error[{code}]")),
        "stderr lacks error[{code}]\n{}",
        describe(out)
    );
}

/// Freezes a child at a `wait:` failpoint until released.
pub struct Barrier {
    path: PathBuf,
}

impl Barrier {
    pub fn new(dir: &Path, name: &str) -> Self {
        Self {
            path: dir.join(format!("barrier-{name}")),
        }
    }

    /// Failpoint spec fragment: `<failpoint>=wait:<path>`.
    pub fn spec(&self, failpoint: &str) -> String {
        format!("{failpoint}=wait:{}", self.path.display())
    }

    fn reached_marker(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".reached");
        p.into()
    }

    /// Block until the child sits at the failpoint. Panics if it exits first: a stub
    /// that never reaches the failpoint must fail the test, not hang it.
    pub fn wait_reached(&self, child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.reached_marker().exists() {
            if let Some(status) = child.try_wait().unwrap() {
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    std::io::Read::read_to_string(&mut s, &mut err).unwrap();
                }
                panic!("process exited ({status}) before reaching the failpoint\nstderr:\n{err}");
            }
            assert!(Instant::now() < deadline, "timed out waiting for failpoint");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn release(&self) {
        fs::write(&self.path, b"go").unwrap();
    }
}

/// Wait for a child with a timeout, returning its full output.
pub fn finish(child: Child) -> Output {
    let start = Instant::now();
    let mut child = child;
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(120), "child hung");
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

/// Like [`finish`], but a child still running after `secs` seconds is killed and the
/// test fails: a product bug that blocks forever must not hang the suite.
pub fn finish_within(child: Child, secs: u64) -> Output {
    let mut child = child;
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(secs) {
            child.kill().unwrap();
            let out = child.wait_with_output().unwrap();
            panic!("still running after {secs}s (killed)\n{}", describe(&out));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

/// Directory containing a `git-parhel` symlink, to put on PATH so `git parhel` works.
pub fn install_extension(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(parhel_bin(), bin.join("git-parhel")).unwrap();
    bin
}

/// Exit status `128 + signal` and `error[interrupted]` on stderr.
pub fn assert_interrupted(out: &Output, signal: i32) {
    assert_eq!(out.status.code(), Some(128 + signal), "{}", describe(out));
    assert!(
        stderr(out).contains("error[interrupted]"),
        "{}",
        describe(out)
    );
}

pub fn send_signal(child: &Child, signal: i32) {
    // SAFETY: signalling our own child process.
    let rc = unsafe { libc::kill(i32::try_from(child.id()).unwrap(), signal) };
    assert_eq!(rc, 0, "kill({signal})");
}
