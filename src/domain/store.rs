//! The store: a per-user btrfs image mounted at a fixed place, where worktrees of
//! sources without copy-on-write live (see `docs/` and the README).
//!
//! Layout inside the store root:
//!
//! ```text
//! .parhel-store                 marker (format=1, user, image, unit)
//! baselines/<id>/tree           mirror of one repository's source worktree
//! baselines/<id>/meta           source=<path>, last sync time
//! baselines/<id>/lock           flock held for sync AND clone
//! worktrees/<id>/<name>         the real worktrees (reflink clones of the baseline)
//! worktrees/.parhel-staging-*   staging of a run in progress
//! links                         append-only registry of the symlinks created
//! ```
//!
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::domain::{Error, ErrorCode, Result};

/// Marker file at the root of a store.
pub const MARKER_FILE: &str = ".parhel-store";
/// The only marker format this version understands.
pub const MARKER_FORMAT: u32 = 1;
pub const BASELINES_DIR: &str = "baselines";
pub const WORKTREES_DIR: &str = "worktrees";
/// Registry of the symlinks created at requested destinations.
pub const LINKS_FILE: &str = "links";

/// Contents of `.parhel-store`: `key=value` lines, `format=1` first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub user: String,
    /// The backing image (`/var/lib/parhel/<user>.img`).
    pub image: PathBuf,
    /// The systemd mount unit (`parhel-<user>.mount`).
    pub unit: String,
}

impl Marker {
    /// The text written by `setup`.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "format={MARKER_FORMAT}\nuser={}\nimage={}\nunit={}\n",
            self.user,
            self.image.display(),
            self.unit
        )
    }

    /// Errors: `store-unavailable` for an unknown format, a missing key or garbage.
    pub fn parse(text: &str) -> Result<Self> {
        let unusable = |why: String| Error::new(ErrorCode::StoreUnavailable, why);
        let (mut format, mut user, mut image, mut unit) = (None, None, None, None);
        for line in text.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
            // Anything that is not `key=value` is not ours: unknown keys are skipped
            // (a later version may add some), a line without `=` is not a marker.
            let (key, value) = line.split_once('=').ok_or_else(|| {
                unusable(format!(
                    "`{MARKER_FILE}` is not a parhel store marker (found `{line}`)"
                ))
            })?;
            match key {
                "format" => format = Some(value),
                "user" => user = Some(value),
                "image" => image = Some(value),
                "unit" => unit = Some(value),
                _ => {}
            }
        }
        match format {
            Some(f) if f.trim() == MARKER_FORMAT.to_string() => {}
            Some(other) => {
                return Err(unusable(format!(
                    "`{MARKER_FILE}` has format {other}, but this parhel understands format \
                     {MARKER_FORMAT}: update parhel (or run `sudo git parhel setup` with the \
                     version that made the store)"
                )));
            }
            None => {
                return Err(unusable(format!(
                    "`{MARKER_FILE}` is not a parhel store marker (no format)"
                )));
            }
        }
        let key = |name: &str, value: Option<&str>| match value {
            Some(v) if !v.is_empty() => Ok(v.to_owned()),
            _ => Err(unusable(format!(
                "`{MARKER_FILE}` lacks `{name}`: the store was not set up by `git parhel setup`"
            ))),
        };
        Ok(Self {
            user: key("user", user)?,
            image: PathBuf::from(key("image", image)?),
            unit: key("unit", unit)?,
        })
    }
}

/// A store that passed every availability check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    pub root: PathBuf,
    pub marker: Marker,
}

/// What discovery found at the configured store path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreState {
    /// The path does not exist: the user never ran `setup`.
    NotConfigured,
    /// The path exists but cannot be used; `reason` says why (not mounted, marker
    /// missing, no copy-on-write, not writable).
    Unavailable {
        root: PathBuf,
        reason: String,
    },
    Available(Store),
}

/// The store path: `PARHEL_STORE` when set and non-empty, else `/parhel/<user>`.
#[must_use]
pub fn store_path(env_override: Option<&OsStr>, user: &str) -> PathBuf {
    match env_override {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => Path::new("/parhel").join(user),
    }
}

/// Identity of a repository inside the store: the first 16 bytes of the SHA-256 of
/// the canonical source toplevel (as bytes), in lowercase hex (32 characters).
#[must_use]
pub fn repo_id(toplevel: &Path) -> String {
    let digest = Sha256::digest(toplevel.as_os_str().as_bytes());
    digest
        .iter()
        .take(16)
        .fold(String::with_capacity(32), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Whether `name` looks like a [`repo_id`]: 32 lowercase hexadecimal characters.
#[must_use]
pub fn is_repo_id(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 32 && bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What a directory entry is, as far as syncing the baseline cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Fifo,
}

/// What `lstat` says about an entry, for comparing the live source with the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMeta {
    pub kind: EntryKind,
    /// Length for files (0 otherwise).
    pub size: u64,
    /// Modification time in nanoseconds since the epoch.
    pub mtime_ns: i128,
    /// Permission bits (`st_mode & 0o7777`).
    pub mode: u32,
    /// Target for symlinks.
    pub link_target: Option<PathBuf>,
}

/// What syncing does with one source entry given what the baseline holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// Identical (directories: same mode; their mtime is set last anyway).
    Keep,
    /// Not in the baseline.
    Create,
    /// Same kind, different content or metadata: rewrite in place.
    Update,
    /// Different kind: remove the baseline entry, then create.
    Replace,
}

/// Compare a source entry with the baseline's, by kind, size, mtime (ns), mode and
/// symlink target.
#[must_use]
pub fn sync_action(source: &EntryMeta, baseline: Option<&EntryMeta>) -> SyncAction {
    let Some(baseline) = baseline else {
        return SyncAction::Create;
    };
    if source.kind != baseline.kind {
        return SyncAction::Replace;
    }
    let same = match source.kind {
        // A directory's mtime moves whenever a child does; it is set last anyway.
        EntryKind::Dir => source.mode == baseline.mode,
        // A symlink is its target; its own mode and times mean nothing.
        EntryKind::Symlink => source.link_target == baseline.link_target,
        EntryKind::File | EntryKind::Fifo => {
            source.size == baseline.size
                && source.mtime_ns == baseline.mtime_ns
                && source.mode == baseline.mode
        }
    };
    if same {
        SyncAction::Keep
    } else {
        SyncAction::Update
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ErrorCode;
    use rstest::rstest;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn marker() -> Marker {
        Marker {
            user: "alice".into(),
            image: PathBuf::from("/var/lib/parhel/alice.img"),
            unit: "parhel-alice.mount".into(),
        }
    }

    const MARKER_TEXT: &str =
        "format=1\nuser=alice\nimage=/var/lib/parhel/alice.img\nunit=parhel-alice.mount\n";

    // U-STO-01: what setup writes is what discovery reads.
    #[test]
    fn marker_round_trips() {
        assert_eq!(Marker::parse(&marker().render()).unwrap(), marker());
    }

    // U-STO-02: the format is part of the contract (other tools may read it).
    #[test]
    fn marker_text_is_key_value_lines_with_the_format_first() {
        assert_eq!(marker().render(), MARKER_TEXT);
    }

    // U-STO-03: order and unknown keys do not matter within format 1.
    #[test]
    fn marker_parse_ignores_order_blank_lines_and_unknown_keys() {
        let text = "\nunit=parhel-alice.mount\nfuture=whatever\nimage=/var/lib/parhel/alice.img\n\
                    user=alice\nformat=1\n";
        assert_eq!(Marker::parse(text).unwrap(), marker());
    }

    // U-STO-04: a marker we cannot vouch for makes the store unavailable.
    #[rstest]
    #[case::empty("")]
    #[case::garbage("not a marker at all")]
    #[case::newer_format("format=2\nuser=alice\nimage=/i\nunit=u.mount\n")]
    #[case::no_format("user=alice\nimage=/i\nunit=u.mount\n")]
    #[case::no_user("format=1\nimage=/i\nunit=u.mount\n")]
    #[case::no_image("format=1\nuser=alice\nunit=u.mount\n")]
    #[case::no_unit("format=1\nuser=alice\nimage=/i\n")]
    #[case::empty_user("format=1\nuser=\nimage=/i\nunit=u.mount\n")]
    fn marker_parse_rejects(#[case] text: &str) {
        let err = Marker::parse(text).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{text:?}: {err}");
    }

    // U-STO-05: `PARHEL_STORE` wins; an empty value is no value.
    #[rstest]
    #[case(Some("/mnt/other-store"), "alice", "/mnt/other-store")]
    #[case(Some(""), "alice", "/parhel/alice")]
    #[case(None, "alice", "/parhel/alice")]
    #[case(None, "mary-ann", "/parhel/mary-ann")]
    fn store_path_resolution(#[case] over: Option<&str>, #[case] user: &str, #[case] want: &str) {
        assert_eq!(
            store_path(over.map(OsStr::new), user),
            Path::new(want).to_path_buf()
        );
    }

    // U-STO-06: the repository id is a stable function of the path's bytes.
    #[rstest]
    #[case(b"/w/repo", "7005185596c2e32bd0688ebe72725f92")]
    #[case(b"/home/alice/projects/api", "6ab40a992eabf73a7ed8514118384f08")]
    #[case(b"/", "8a5edab282632443219e051e4ade2d1d")]
    // Not UTF-8, with a space and a newline: hashed as bytes.
    #[case(b"/w/a b\n\xff", "f000c057239489a793ae67a6e114c41f")]
    fn repo_id_is_the_sha256_prefix_of_the_path_bytes(#[case] path: &[u8], #[case] want: &str) {
        assert_eq!(repo_id(Path::new(OsStr::from_bytes(path))), want);
    }

    fn meta(kind: EntryKind, size: u64, mtime_ns: i128, mode: u32) -> EntryMeta {
        EntryMeta {
            kind,
            size,
            mtime_ns,
            mode,
            link_target: None,
        }
    }

    fn file(size: u64, mtime_ns: i128, mode: u32) -> EntryMeta {
        meta(EntryKind::File, size, mtime_ns, mode)
    }

    fn dir(mtime_ns: i128, mode: u32) -> EntryMeta {
        meta(EntryKind::Dir, 0, mtime_ns, mode)
    }

    fn link(target: &str) -> EntryMeta {
        EntryMeta {
            link_target: Some(PathBuf::from(target)),
            ..meta(EntryKind::Symlink, 0, 5, 0o777)
        }
    }

    // U-STO-07: what changed decides what is copied (kind, size, mtime to the
    // nanosecond, mode, symlink target); everything else is left alone.
    #[rstest]
    #[case::identical_file(file(10, 100, 0o644), Some(file(10, 100, 0o644)), SyncAction::Keep)]
    #[case::new_file(file(10, 100, 0o644), None, SyncAction::Create)]
    #[case::bigger(file(11, 100, 0o644), Some(file(10, 100, 0o644)), SyncAction::Update)]
    #[case::same_size_newer(file(10, 101, 0o644), Some(file(10, 100, 0o644)), SyncAction::Update)]
    #[case::one_nanosecond(
        file(10, 100_000_000_001, 0o644),
        Some(file(10, 100_000_000_000, 0o644)),
        SyncAction::Update
    )]
    #[case::older_too(file(10, 99, 0o644), Some(file(10, 100, 0o644)), SyncAction::Update)]
    #[case::mode_only(file(10, 100, 0o755), Some(file(10, 100, 0o644)), SyncAction::Update)]
    #[case::file_became_dir(dir(100, 0o755), Some(file(10, 100, 0o644)), SyncAction::Replace)]
    #[case::dir_became_file(file(10, 100, 0o644), Some(dir(100, 0o755)), SyncAction::Replace)]
    #[case::file_became_link(link("a"), Some(file(10, 100, 0o644)), SyncAction::Replace)]
    #[case::link_became_file(file(10, 100, 0o644), Some(link("a")), SyncAction::Replace)]
    #[case::same_link(link("a"), Some(link("a")), SyncAction::Keep)]
    #[case::link_retargeted(link("b"), Some(link("a")), SyncAction::Update)]
    #[case::fifo_became_file(
        file(0, 100, 0o644),
        Some(meta(EntryKind::Fifo, 0, 100, 0o644)),
        SyncAction::Replace
    )]
    #[case::same_fifo(
        meta(EntryKind::Fifo, 0, 100, 0o644),
        Some(meta(EntryKind::Fifo, 0, 100, 0o644)),
        SyncAction::Keep
    )]
    #[case::new_dir(dir(100, 0o755), None, SyncAction::Create)]
    // A directory's mtime moves whenever a child does and is set last anyway.
    #[case::dir_mtime_ignored(dir(200, 0o755), Some(dir(100, 0o755)), SyncAction::Keep)]
    #[case::dir_mode(dir(100, 0o700), Some(dir(100, 0o755)), SyncAction::Update)]
    fn sync_action_table(
        #[case] source: EntryMeta,
        #[case] baseline: Option<EntryMeta>,
        #[case] want: SyncAction,
    ) {
        assert_eq!(sync_action(&source, baseline.as_ref()), want);
    }
}
