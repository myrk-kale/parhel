//! Cross-process failpoints for integration tests.
//!
//! Compiled into the binary only with the `failpoints` feature. Configured through
//! `PARHEL_FAILPOINTS`, a `;`-separated list of `name=action`:
//!
//! - `error`: return `error[failpoint]` from that point;
//! - `panic`: panic (simulates a bug);
//! - `kill`: `SIGKILL` the process itself (simulates a crash, no cleanup runs);
//! - `wait:<path>`: create `<path>.reached`, then block until `<path>` exists.
//!   Lets a test freeze the process at a precise step, act, and release it.
//!
//! A failpoint placed once per entry of a tree walk is named `<name>:<path>`, the path
//! being the entry's, relative to the source root (`walk_entry:node_modules/pkg`):
//! a test freezes the walk at one entry only.
//!
//! The same build has one more test hook that is not a failpoint: `PARHEL_SPACE`, a
//! `;`-separated list of `<path prefix>=<total bytes>:<free bytes>`, which says how much
//! room the disk under a path has (the longest prefix wins), so that the checks of room
//! (`doctor`'s `store-low-space`, `add`'s `store-no-space`) can be tested without filling a
//! disk and without depending on the one the tests run on. See `fs::unix::space`.
//!
//! This module is test infrastructure, not product behavior: it is implemented
//! (not stubbed) so integration tests can drive the product deterministically.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::domain::{Error, ErrorCode, Result};

/// Environment variable holding the failpoint configuration.
pub const ENV_VAR: &str = "PARHEL_FAILPOINTS";

/// Names of every failpoint placed in the `add` transaction, in execution order.
pub const ADD_FAILPOINTS: &[&str] = &[
    "after_validate",
    "after_worktree_add",
    "mid_clone",
    "after_clone",
    "after_index",
    "before_rename",
];

/// Failpoints of the store route of `add`, in execution order: right after the store-wide
/// lock was taken (shared, before `after_validate`), after the baseline sync, and after the
/// symlink was claimed at the requested destination (both before the publishing rename).
/// Each also applies to [`ADD_FAILPOINTS`] steps the route shares.
pub const STORE_FAILPOINTS: &[&str] = &["store_locked", "after_sync", "after_link"];

/// Failpoints of `setup --remove`, in execution order: right after the store-wide lock was
/// taken exclusively (before anything in the store is looked at), and after the store was
/// sealed and the lock let go of, right before the unit is stopped. Frozen at either, a
/// test starts an `add` and sees what it makes of a store that is going away.
pub const REMOVE_FAILPOINTS: &[&str] = &["store_remove_locked", "store_remove_sealed"];

/// Failpoint of the baseline sync, evaluated as `sync_entry:<path>` just before each
/// entry below the source root is compared (the sync's twin of [`WALK_FAILPOINT`]).
pub const SYNC_FAILPOINT: &str = "sync_entry";

/// Failpoint of the tree walk (clone or copy), evaluated as `walk_entry:<path>` just
/// before each entry below the source root is opened.
pub const WALK_FAILPOINT: &str = "walk_entry";

/// Failpoint between the publishing rename and `git worktree repair`.
pub const PUBLISH_FAILPOINT: &str = "after_rename";

/// Failpoint inside rollback itself (lets tests signal a process that is undoing).
pub const ROLLBACK_FAILPOINT: &str = "during_rollback";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Error,
    Panic,
    Kill,
    Wait(PathBuf),
}

/// Parse a `PARHEL_FAILPOINTS` value.
pub fn parse_spec(spec: &str) -> Result<HashMap<String, Action>> {
    let mut map = HashMap::new();
    for item in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, action) = item
            .split_once('=')
            .ok_or_else(|| Error::new(ErrorCode::Usage, format!("bad failpoint `{item}`")))?;
        let action = match action {
            "error" => Action::Error,
            "panic" => Action::Panic,
            "kill" => Action::Kill,
            other => match other.strip_prefix("wait:") {
                Some(path) if !path.is_empty() => Action::Wait(PathBuf::from(path)),
                _ => {
                    return Err(Error::new(
                        ErrorCode::Usage,
                        format!("bad failpoint action `{other}`"),
                    ));
                }
            },
        };
        map.insert(name.to_owned(), action);
    }
    Ok(map)
}

/// Evaluate the failpoint `name` against `PARHEL_FAILPOINTS`.
#[cfg(feature = "failpoints")]
pub fn eval(name: &str) -> Result<()> {
    use std::time::{Duration, Instant};

    let Ok(spec) = std::env::var(ENV_VAR) else {
        return Ok(());
    };
    let Some(action) = parse_spec(&spec)?.remove(name) else {
        return Ok(());
    };
    match action {
        Action::Error => Err(Error::new(
            ErrorCode::Failpoint,
            format!("injected at `{name}`"),
        )),
        Action::Panic => panic!("failpoint `{name}` panicked"),
        Action::Kill => {
            // SAFETY: plain syscalls on our own pid; SIGKILL never returns.
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
            unreachable!("SIGKILL delivered")
        }
        Action::Wait(release) => {
            let mut reached = release.clone().into_os_string();
            reached.push(".reached");
            std::fs::write(&reached, std::process::id().to_string())
                .map_err(|e| Error::new(ErrorCode::Io, format!("failpoint `{name}`: {e}")))?;
            let deadline = Instant::now() + Duration::from_secs(120);
            while !release.exists() {
                if Instant::now() > deadline {
                    return Err(Error::new(
                        ErrorCode::Failpoint,
                        format!("`{name}` timed out"),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        }
    }
}

/// `failpoint!("name")` inside a function returning [`Result`]: no-op unless the
/// `failpoints` feature is enabled. `failpoint!("name", path)` is the per-entry form,
/// named `name:<path>`.
#[macro_export]
macro_rules! failpoint {
    ($name:literal) => {{
        #[cfg(feature = "failpoints")]
        {
            $crate::failpoint::eval($name)?;
        }
    }};
    ($name:literal, $path:expr) => {{
        #[cfg(feature = "failpoints")]
        {
            $crate::failpoint::eval(&format!("{}:{}", $name, $path.display()))?;
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    // Harness self-test: must pass.
    #[test]
    fn parses_all_actions() {
        let m = parse_spec("a=error; b=panic;c=kill;d=wait:/tmp/x y;").unwrap();
        assert_eq!(m["a"], Action::Error);
        assert_eq!(m["b"], Action::Panic);
        assert_eq!(m["c"], Action::Kill);
        assert_eq!(m["d"], Action::Wait(PathBuf::from("/tmp/x y")));
    }

    // Harness self-test: must pass.
    #[test]
    fn rejects_malformed_specs() {
        for bad in ["a", "a=boom", "a=wait:"] {
            assert_eq!(parse_spec(bad).unwrap_err().code, ErrorCode::Usage, "{bad}");
        }
    }
}
