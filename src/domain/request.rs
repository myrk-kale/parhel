//! Validated requests produced by the CLI layer.

use std::path::PathBuf;

/// How the new worktree's HEAD is chosen. Mirrors `git worktree add` semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchMode {
    /// No branch flag: git creates a branch named after the destination's basename
    /// (or checks out `[ref]` when it names a local branch).
    Default,
    /// `--branch <name>`: check out an existing branch.
    Existing(String),
    /// `--new-branch <name>`: create the branch (`git worktree add -b`).
    New(String),
    /// `--detach`: detached HEAD at `[ref]` (or the source HEAD).
    Detach,
}

/// What to do with initialized submodules in the source worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubmodulePolicy {
    /// Refuse to create the worktree (`error[submodules-present]`).
    #[default]
    Refuse,
    /// Skip submodule directories during the clone and re-initialize them in the
    /// destination with their own git dirs, borrowing objects from the source.
    Reinit,
}

/// `git parhel add` after argument validation.
// Each bool is an independent command-line switch.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRequest {
    /// `--from`: any path inside the source worktree (default `.`), as typed.
    pub from: PathBuf,
    /// `<destination>`, as typed (relative paths resolve against the cwd).
    pub destination: PathBuf,
    /// Optional `[ref]` commit-ish.
    pub reference: Option<String>,
    pub branch: BranchMode,
    /// `--allow-dirty`: carry uncommitted changes from the source.
    pub allow_dirty: bool,
    /// `--fallback`: use a plain `git worktree add` when copy-on-write is impossible.
    pub fallback: bool,
    /// `--copy`: copy every file in full (no copy-on-write, uses disk space). Works on
    /// any filesystem and across volumes. Never combined with `fallback`.
    pub copy: bool,
    pub submodules: SubmodulePolicy,
    /// Rewrite the source's absolute path in copied untracked files (`--no-relocate`
    /// turns it off).
    pub relocate: bool,
}

/// `git parhel doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorRequest {
    /// `--from`: any path inside the repository to diagnose (default `.`).
    pub from: PathBuf,
    /// `--fix`: repair what can be repaired safely.
    pub fix: bool,
}

/// `git parhel setup`: create or remove the per-user store (Linux, needs root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupRequest {
    /// `--size`: apparent size of the sparse image, in bytes.
    pub size: u64,
    /// `--preallocate`: allocate the whole image up front.
    pub preallocate: bool,
    /// `--user`: whose store (default `$SUDO_USER`). `Some` is explicit, so
    /// `--user root` is honored where an inferred root is refused.
    pub user: Option<String>,
    /// `--remove`: take the store down instead of creating it.
    pub remove: bool,
}

/// Default image size: 100 GiB, sparse (nothing is allocated until used).
pub const DEFAULT_STORE_SIZE: u64 = 100 << 30;

/// Smallest image btrfs can be formatted on, with room for a baseline.
pub const MIN_STORE_SIZE: u64 = 256 << 20;

/// Parse a size such as `512M`, `100G` or `2T` (binary multiples; a bare number is
/// bytes). Errors: `usage` for empty, malformed, overflowing or too small values.
pub fn parse_size(text: &str) -> crate::domain::Result<u64> {
    use crate::domain::{Error, ErrorCode};
    let usage = |why: &str| Error::new(ErrorCode::Usage, format!("--size `{text}`: {why}"));
    let text = text.trim();
    let (digits, shift) = match text.chars().last() {
        Some('k' | 'K') => (&text[..text.len() - 1], 10),
        Some('m' | 'M') => (&text[..text.len() - 1], 20),
        Some('g' | 'G') => (&text[..text.len() - 1], 30),
        Some('t' | 'T') => (&text[..text.len() - 1], 40),
        _ => (text, 0),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(usage(
            "expected a number with an optional K, M, G or T suffix",
        ));
    }
    let bytes = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
        .ok_or_else(|| usage("too large"))?;
    if bytes < MIN_STORE_SIZE {
        return Err(usage("a store needs at least 256M"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ErrorCode;
    use rstest::rstest;

    // U-CLI-25: sizes take binary suffixes, any case; a bare number is bytes.
    #[rstest]
    #[case("512M", 512 << 20)]
    #[case("512m", 512 << 20)]
    #[case("100G", 100 << 30)]
    #[case("2T", 2 << 40)]
    #[case("1048576K", 1 << 30)]
    #[case("268435456", 256 << 20)]
    #[case("256M", MIN_STORE_SIZE)]
    fn parses_sizes(#[case] text: &str, #[case] want: u64) {
        assert_eq!(parse_size(text).unwrap(), want);
    }

    // U-CLI-26: anything else is a usage error that names the value.
    #[rstest]
    #[case::empty("")]
    #[case::only_a_suffix("G")]
    #[case::words("lots")]
    #[case::negative("-5G")]
    #[case::decimal("1.5G")]
    #[case::unknown_suffix("5P")]
    #[case::two_suffixes("5GG")]
    #[case::inner_space("5 G")]
    #[case::too_small("255M")]
    #[case::zero("0")]
    #[case::overflow("99999999999999999999T")]
    #[case::overflow_by_suffix("17179869184T")]
    fn rejects_sizes(#[case] text: &str) {
        let err = parse_size(text).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage, "{text:?}");
        assert!(err.message.contains("--size"), "{}", err.message);
    }
}
