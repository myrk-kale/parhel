//! The store: discovery and the baseline sync.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::domain::exclusions::Exclusions;
use crate::domain::store::{
    EntryKind, EntryMeta, MARKER_FILE, Marker, Store, StoreState, SyncAction, sync_action,
};
use crate::domain::{Error, ErrorCode, Result};
use crate::ports::CowFs;

/// What a sync did to the baseline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Files, symlinks and FIFOs created or rewritten.
    pub copied: usize,
    /// Entries removed because the source no longer has them (or has another kind).
    pub deleted: usize,
    /// Entries found identical and left alone.
    pub unchanged: usize,
    /// Bytes of file data copied.
    pub bytes: u64,
}

/// Look at the configured store path.
///
/// - missing: [`StoreState::NotConfigured`];
/// - present but without a readable `.parhel-store` marker of format 1, not on a
///   copy-on-write filesystem, or not writable: [`StoreState::Unavailable`] with the
///   reason;
/// - otherwise [`StoreState::Available`].
///
/// Only looks: nothing is created or changed.
pub fn discover(fs: &dyn CowFs, path: &Path) -> Result<StoreState> {
    if !fs.exists_no_follow(path)? {
        return Ok(StoreState::NotConfigured);
    }
    let unavailable = |reason: String| {
        Ok(StoreState::Unavailable {
            root: path.to_path_buf(),
            reason,
        })
    };
    // The marker lives inside the mounted image: without it the path is an empty mount
    // point (the unit is not running) or somebody else's directory.
    let marker_path = path.join(MARKER_FILE);
    let text = match fs.read_file(&marker_path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => {
            return unavailable(format!(
                "`{}` has no `{MARKER_FILE}` marker: the store is not mounted or was not set \
                 up by `git parhel setup`",
                path.display()
            ));
        }
    };
    let marker = match Marker::parse(&text) {
        Ok(marker) => marker,
        Err(e) => return unavailable(e.message),
    };
    let volume = fs.volume(path)?;
    if !volume.cow {
        return unavailable(format!(
            "`{}` is on {}, which has no copy-on-write; a store must be on btrfs (or another \
             filesystem that can reflink)",
            path.display(),
            volume.fs_type
        ));
    }
    if !fs.is_writable(path)? {
        return unavailable(format!(
            "`{}` is not writable by the current user",
            path.display()
        ));
    }
    Ok(StoreState::Available(Store {
        root: path.to_path_buf(),
        marker,
    }))
}

/// Size from which the first sync of a repository announces itself.
pub const NOTICE_BYTES: u64 = 1 << 30;

/// Bytes the first sync of `source` into an empty or missing `baseline` would copy, or
/// `None` when the baseline already holds something (later syncs copy only the
/// difference). Only `stat`s: nothing is read.
pub fn first_sync_size(
    fs: &dyn CowFs,
    source: &Path,
    baseline: &Path,
    exclusions: &Exclusions,
) -> Result<Option<u64>> {
    let occupied = fs
        .stat_within(baseline, Path::new(""))?
        .is_some_and(|meta| meta.kind == EntryKind::Dir)
        && fs
            .list_dir_within(baseline, Path::new(""))?
            .is_some_and(|names| !names.is_empty());
    if occupied {
        return Ok(None);
    }
    let mut total = 0u64;
    let mut pending = vec![PathBuf::new()];
    while let Some(dir) = pending.pop() {
        for name in fs.list_dir_within(source, &dir)?.unwrap_or_default() {
            let rel = dir.join(name);
            if exclusions.is_excluded(&rel) {
                continue;
            }
            match fs.stat_within(source, &rel)? {
                Some(EntryMeta {
                    kind: EntryKind::Dir,
                    ..
                }) => pending.push(rel),
                Some(EntryMeta {
                    kind: EntryKind::File,
                    size,
                    ..
                }) => total = total.saturating_add(size),
                _ => {}
            }
        }
    }
    Ok(Some(total))
}

/// The one-line notice printed before a first sync that copies `bytes`, if it is big
/// enough to be worth one.
#[must_use]
pub fn first_sync_notice(bytes: u64) -> Option<String> {
    (bytes > NOTICE_BYTES).then(|| {
        #[allow(clippy::cast_precision_loss)]
        let gib = bytes as f64 / f64::from(1u32 << 30);
        format!(
            "copying {gib:.1} GiB into the store: the first worktree of a repository mirrors it \
             once, the next ones copy only what changed"
        )
    })
}

/// Bring the baseline tree `baseline` in line with the live source worktree `source`:
/// walk the source anchored on directory descriptors (`CowFs::list_dir_within`,
/// `stat_within`), skip `exclusions`, and for each entry apply
/// [`sync_action`]; delete baseline entries the
/// source does not have (or that are now excluded); set a directory's metadata after
/// its children. Stops with `interrupted` when `interrupted` is raised.
///
/// `last_sync` is when the last sync that finished began (ns since the epoch): entries
/// changed since then are copied whatever their size, times and mode say (see
/// [`sync_action`]); `None` when there is no such record, and then everything is copied.
///
/// Safe to repeat and to run after a crash: it always compares everything, so a
/// half-synced baseline is repaired by the next run. Existing worktrees are
/// independent reflink clones, so rewriting baseline files in place never reaches them.
pub fn sync_baseline(
    fs: &dyn CowFs,
    source: &Path,
    baseline: &Path,
    exclusions: &Exclusions,
    interrupted: &AtomicBool,
    last_sync: Option<i128>,
) -> Result<SyncReport> {
    let mut sync = Sync {
        fs,
        source,
        baseline,
        exclusions,
        interrupted,
        since: last_sync,
        report: SyncReport::default(),
    };
    sync.check_interrupt()?;
    sync.entry(Path::new(""))?;
    Ok(sync.report)
}

struct Sync<'a> {
    fs: &'a dyn CowFs,
    source: &'a Path,
    baseline: &'a Path,
    exclusions: &'a Exclusions,
    interrupted: &'a AtomicBool,
    since: Option<i128>,
    report: SyncReport,
}

impl Sync<'_> {
    fn check_interrupt(&self) -> Result<()> {
        if self.interrupted.load(Ordering::SeqCst) {
            return Err(Error::new(
                ErrorCode::Interrupted,
                "interrupted; rolled back",
            ));
        }
        Ok(())
    }

    /// Compare and fix the entry `rel` and, for a directory, everything below it.
    /// Returns whether the entry itself was created, rewritten or replaced (which
    /// changes its parent directory).
    fn entry(&mut self, rel: &Path) -> Result<bool> {
        self.check_interrupt()?;
        if !rel.as_os_str().is_empty() {
            crate::failpoint!("sync_entry", rel);
        }
        let Some(source) = self.fs.stat_within(self.source, rel)? else {
            // Gone since it was listed (or a socket, which a worktree never holds):
            // the baseline must not keep it.
            return self.remove_if_present(rel);
        };
        let dst = self.baseline.join(rel);
        let held = self.fs.stat_within(self.baseline, rel)?;
        let action = sync_action(&source, held.as_ref(), self.since);
        match action {
            SyncAction::Keep => self.report.unchanged += 1,
            SyncAction::Create | SyncAction::Update => {}
            SyncAction::Replace => {
                self.remove(rel, held.as_ref())?;
            }
        }
        if action != SyncAction::Keep {
            self.report.bytes += self.fs.mirror_entry(self.source, rel, &dst)?;
            if source.kind != EntryKind::Dir {
                self.report.copied += 1;
            }
        }
        let mut touched = action != SyncAction::Keep;
        if source.kind == EntryKind::Dir {
            touched |=
                self.children(rel, held.as_ref().filter(|_| action != SyncAction::Replace))?;
            if touched {
                self.check_interrupt()?;
                self.fs.mirror_dir_meta(self.source, rel, &dst)?;
            }
        }
        Ok(action != SyncAction::Keep)
    }

    /// Sync the entries of the directory `rel`; whether any of them changed.
    fn children(&mut self, rel: &Path, held: Option<&EntryMeta>) -> Result<bool> {
        let wanted: BTreeSet<OsString> = self
            .fs
            .list_dir_within(self.source, rel)?
            .unwrap_or_default()
            .into_iter()
            .filter(|name| !self.exclusions.is_excluded(&rel.join(name)))
            .collect();
        let mut touched = false;
        // What the baseline has and the source does not (or no longer wants).
        if held.is_some_and(|meta| meta.kind == EntryKind::Dir) {
            let mut extra: Vec<OsString> = self
                .fs
                .list_dir_within(self.baseline, rel)?
                .unwrap_or_default()
                .into_iter()
                .filter(|name| !wanted.contains(name))
                .collect();
            extra.sort();
            for name in extra {
                self.check_interrupt()?;
                touched |= self.remove_if_present(&rel.join(name))?;
            }
        }
        for name in wanted {
            touched |= self.entry(&rel.join(name))?;
        }
        Ok(touched)
    }

    fn remove_if_present(&mut self, rel: &Path) -> Result<bool> {
        let held = self.fs.stat_within(self.baseline, rel)?;
        if held.is_none() {
            return Ok(false);
        }
        self.remove(rel, held.as_ref())?;
        Ok(true)
    }

    /// Remove the baseline entry `rel`: a directory whole, anything else by name.
    fn remove(&mut self, rel: &Path, held: Option<&EntryMeta>) -> Result<()> {
        let Some(held) = held else { return Ok(()) };
        let path = self.baseline.join(rel);
        if held.kind == EntryKind::Dir {
            self.fs.remove_dir_all(&path)?;
        } else {
            self.fs.remove_file(&path)?;
        }
        self.report.deleted += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{Call, MARKER, REPO_ID, STORE, Step, World, ext4, p};
    use crate::domain::ErrorCode;
    use crate::domain::exclusions::ExclusionInput;
    use crate::domain::store::{Marker, Store};
    use rstest::rstest;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    fn base() -> PathBuf {
        p(&format!("{STORE}/baselines/{REPO_ID}/tree"))
    }

    fn at(rel: &str) -> PathBuf {
        if rel.is_empty() {
            base()
        } else {
            base().join(rel)
        }
    }

    fn src(rel: &str) -> PathBuf {
        p("/w/repo").join(rel)
    }

    fn assert_no_mutation(w: &World) {
        assert!(
            w.calls().is_empty(),
            "expected no side effects: {:#?}",
            w.calls()
        );
    }

    // ---------------------------------------------------------------- discovery

    // U-STO-10: no directory, no store: the user never ran `setup`.
    #[test]
    fn missing_path_is_not_configured() {
        let w = World::default();
        assert_eq!(
            discover(&w, Path::new("/nowhere/store")).unwrap(),
            StoreState::NotConfigured
        );
        assert_no_mutation(&w);
    }

    // U-STO-11
    #[test]
    fn healthy_store_is_available_with_its_marker() {
        let w = World::default().with_store();
        assert_eq!(
            discover(&w, Path::new(STORE)).unwrap(),
            StoreState::Available(Store {
                root: p(STORE),
                marker: Marker {
                    user: "alice".into(),
                    image: p("/var/lib/parhel/alice.img"),
                    unit: "parhel-alice.mount".into(),
                },
            })
        );
        assert_no_mutation(&w);
    }

    fn unavailable(w: &World) -> String {
        match discover(w, Path::new(STORE)).unwrap() {
            StoreState::Unavailable { root, reason } => {
                assert_eq!(root, p(STORE));
                reason
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // U-STO-12: a configured path without the marker is an unmounted mount point (or
    // someone else's directory): never used, never silently skipped.
    #[test]
    fn directory_without_marker_is_unavailable() {
        let w = World::default().with_store();
        w.files.borrow_mut().remove(&p("/store/.parhel-store"));
        let reason = unavailable(&w);
        assert!(reason.contains(".parhel-store"), "{reason}");
        assert_no_mutation(&w);
    }

    // U-STO-13
    #[test]
    fn marker_of_another_format_is_unavailable() {
        let w = World::default().with_store();
        w.files.borrow_mut().insert(
            p("/store/.parhel-store"),
            MARKER.replace("format=1", "format=2").into_bytes(),
        );
        let reason = unavailable(&w);
        assert!(reason.contains("format"), "{reason}");
    }

    // U-STO-14
    #[test]
    fn garbage_marker_is_unavailable() {
        let w = World::default().with_store();
        w.files
            .borrow_mut()
            .insert(p("/store/.parhel-store"), b"\xff\xfe nonsense".to_vec());
        let _ = unavailable(&w);
    }

    // U-STO-15: a store whose filesystem cannot clone is useless, and says why.
    #[test]
    fn store_on_a_filesystem_without_cow_is_unavailable() {
        let mut w = World::default().with_store();
        w.volumes = vec![(p("/"), ext4(1)), (p(STORE), ext4(2))];
        let reason = unavailable(&w);
        assert!(
            reason.contains("copy-on-write") && reason.contains("ext4"),
            "{reason}"
        );
    }

    // U-STO-16
    #[test]
    fn unwritable_store_is_unavailable() {
        let w = World::default().with_store();
        w.unwritable.borrow_mut().insert(p(STORE));
        let reason = unavailable(&w);
        assert!(reason.contains("writable"), "{reason}");
    }

    // ----------------------------------------------------------------- the sync

    fn exclusions() -> Exclusions {
        Exclusions::compute(&ExclusionInput {
            source_root: p("/w/repo"),
            ..ExclusionInput::default()
        })
    }

    /// `/w/repo` with `.git/`, `node_modules/big` (100 bytes) and `src/main.rs`
    /// (20 bytes), and no baseline yet: the first sync creates the tree itself, root
    /// included (`baselines/<id>` exists, `tree` does not).
    fn world() -> World {
        let w = World::default().with_store();
        w.put_file(&src("node_modules/big"), 100, 10);
        w.put_file(&src("src/main.rs"), 20, 20);
        w.put_dir(base().parent().unwrap());
        w
    }

    /// The start of the last finished sync in these tests: well after the entries'
    /// default status-change time (the epoch), so nothing looks recently changed.
    const LAST_SYNC: i128 = 100_000_000_000;

    fn sync(w: &World, exclusions: &Exclusions) -> Result<SyncReport> {
        sync_since(w, exclusions, Some(LAST_SYNC))
    }

    fn sync_since(
        w: &World,
        exclusions: &Exclusions,
        last_sync: Option<i128>,
    ) -> Result<SyncReport> {
        sync_baseline(
            w,
            Path::new("/w/repo"),
            &base(),
            exclusions,
            &w.interrupted,
            last_sync,
        )
    }

    fn mirrors(w: &World) -> Vec<PathBuf> {
        w.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Mirror { dst, .. } => Some(dst),
                _ => None,
            })
            .collect()
    }

    fn dir_metas(w: &World) -> Vec<PathBuf> {
        w.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::MirrorDirMeta { dst, .. } => Some(dst),
                _ => None,
            })
            .collect()
    }

    /// A world whose baseline already mirrors the source, as a finished sync leaves it.
    fn synced() -> World {
        let w = world();
        mirror_by_hand(
            &w,
            &["", "node_modules", "node_modules/big", "src", "src/main.rs"],
        );
        w
    }

    /// Fixture: the baseline holds a copy of each of these source entries (parents first).
    fn mirror_by_hand(w: &World, rels: &[&str]) {
        for rel in rels {
            w.copy_facts(&src(rel), &at(rel));
        }
    }

    // U-STO-30: the first sync copies everything but the exclusions, parents first,
    // and gives each directory its metadata after its children.
    #[test]
    fn first_sync_creates_the_tree_without_the_exclusions() {
        let w = world();
        let report = sync(&w, &exclusions()).unwrap();
        assert_eq!(
            report,
            SyncReport {
                copied: 2,
                deleted: 0,
                unchanged: 0,
                bytes: 120,
            }
        );
        let mirrored = mirrors(&w);
        assert_eq!(mirrored.len(), 5, "root, 2 dirs, 2 files: {mirrored:?}");
        assert!(
            mirrored.iter().all(|m| m.starts_with(base())),
            "{mirrored:?}"
        );
        assert!(
            !mirrored.iter().any(|m| m.starts_with(at(".git"))),
            "the root .git is never mirrored: {mirrored:?}"
        );
        let pos = |path: PathBuf| {
            w.position(|c| matches!(c, Call::Mirror { dst, .. } if *dst == path))
                .unwrap()
        };
        assert!(pos(at("")) < pos(at("node_modules")));
        assert!(pos(at("node_modules")) < pos(at("node_modules/big")));
        assert!(pos(at("src")) < pos(at("src/main.rs")));
        // Directory metadata last: after the children, the root's after everything.
        let meta_pos = |path: PathBuf| {
            w.position(|c| matches!(c, Call::MirrorDirMeta { dst, .. } if *dst == path))
                .unwrap()
        };
        assert!(pos(at("node_modules/big")) < meta_pos(at("node_modules")));
        assert!(pos(at("src/main.rs")) < meta_pos(at("src")));
        assert_eq!(meta_pos(at("")), w.calls().len() - 1);
        assert_eq!(dir_metas(&w).len(), 3);
    }

    // U-STO-31: a second sync with nothing changed touches nothing.
    #[test]
    fn second_sync_without_changes_does_nothing() {
        let w = synced();
        let report = sync(&w, &exclusions()).unwrap();
        assert_no_mutation(&w);
        assert_eq!(
            report,
            SyncReport {
                copied: 0,
                deleted: 0,
                unchanged: 5,
                bytes: 0,
            }
        );
    }

    // U-STO-32: only the changed file is copied, and its directory gets its mtime back.
    #[test]
    fn a_changed_file_is_the_only_thing_copied() {
        let w = synced();
        w.put_file(&src("src/main.rs"), 25, 99);
        let report = sync(&w, &exclusions()).unwrap();
        assert_eq!(
            w.calls(),
            vec![
                Call::Mirror {
                    src: src("src/main.rs"),
                    dst: at("src/main.rs")
                },
                Call::MirrorDirMeta {
                    src: src("src"),
                    dst: at("src")
                },
            ]
        );
        assert_eq!((report.copied, report.unchanged, report.bytes), (1, 4, 25));
    }

    // U-STO-33: same size and mtime but another mode is a change (chmod +x).
    #[test]
    fn a_mode_change_alone_is_copied() {
        let w = synced();
        let mut meta = w
            .stat_within(Path::new("/w/repo"), Path::new("src/main.rs"))
            .unwrap()
            .unwrap();
        meta.mode = 0o755;
        w.meta.borrow_mut().insert(src("src/main.rs"), meta);
        sync(&w, &exclusions()).unwrap();
        assert_eq!(mirrors(&w), vec![at("src/main.rs")]);
    }

    // U-STO-33b: a rewrite that restored the size, the mtime and the mode (`touch -d`,
    // `cp -p`, rsync -t, a tool that puts times back) looks identical to a comparison of
    // those three; the status-change time does not lie, so the file is copied.
    #[test]
    fn a_same_size_rewrite_with_the_mtime_restored_is_copied() {
        let w = synced();
        let mut meta = w
            .stat_within(Path::new("/w/repo"), Path::new("src/main.rs"))
            .unwrap()
            .unwrap();
        meta.ctime_ns = LAST_SYNC + 5_000_000_000;
        w.meta.borrow_mut().insert(src("src/main.rs"), meta);
        let report = sync(&w, &exclusions()).unwrap();
        assert_eq!(mirrors(&w), vec![at("src/main.rs")]);
        assert_eq!((report.copied, report.unchanged, report.bytes), (1, 4, 20));
    }

    // U-STO-33c: no finished sync on record (the meta was lost, or never said): the
    // baseline is not trusted and everything is copied again, files and directories.
    #[test]
    fn without_a_sync_on_record_the_whole_tree_is_copied_again() {
        let w = synced();
        let report = sync_since(&w, &exclusions(), None).unwrap();
        assert_eq!(report.copied, 2, "{report:?}");
        assert_eq!(report.unchanged, 0);
        let mirrored = mirrors(&w);
        for rel in ["node_modules/big", "src/main.rs"] {
            assert!(mirrored.contains(&at(rel)), "{rel}: {mirrored:?}");
        }
    }

    // U-STO-34
    #[test]
    fn a_new_file_is_created_and_its_directory_updated() {
        let w = synced();
        w.put_file(&src("src/lib.rs"), 7, 30);
        let report = sync(&w, &exclusions()).unwrap();
        assert_eq!(mirrors(&w), vec![at("src/lib.rs")]);
        assert_eq!(dir_metas(&w), vec![at("src")]);
        assert_eq!((report.copied, report.deleted), (1, 0));
    }

    // U-STO-35: a file gone from the source is gone from the baseline.
    #[test]
    fn a_deleted_file_is_removed_from_the_baseline() {
        let w = synced();
        w.tree
            .borrow_mut()
            .get_mut(&src("src"))
            .unwrap()
            .retain(|n| n != "main.rs");
        w.meta.borrow_mut().remove(&src("src/main.rs"));
        let report = sync(&w, &exclusions()).unwrap();
        assert_eq!(
            w.calls(),
            vec![
                Call::RemoveFile(at("src/main.rs")),
                Call::MirrorDirMeta {
                    src: src("src"),
                    dst: at("src")
                },
            ]
        );
        assert_eq!((report.copied, report.deleted), (0, 1));
    }

    // U-STO-36: a deleted directory goes in one removal, not entry by entry.
    #[test]
    fn a_deleted_directory_is_removed_whole() {
        let w = synced();
        w.tree.borrow_mut().remove(&src("node_modules"));
        w.tree
            .borrow_mut()
            .get_mut(&p("/w/repo"))
            .unwrap()
            .retain(|n| n != "node_modules");
        w.meta.borrow_mut().remove(&src("node_modules/big"));
        sync(&w, &exclusions()).unwrap();
        assert_eq!(
            w.calls(),
            vec![
                Call::RemoveDirAll(at("node_modules")),
                Call::MirrorDirMeta {
                    src: p("/w/repo"),
                    dst: at("")
                },
            ]
        );
    }

    // U-STO-37: a file that became a directory is replaced: removed, then created.
    #[test]
    fn a_file_that_became_a_directory_is_replaced() {
        let w = synced();
        // `src/main.rs` is now a directory with one file in it.
        w.meta.borrow_mut().remove(&src("src/main.rs"));
        w.put_dir(&src("src/main.rs"));
        w.put_file(&src("src/main.rs/mod.rs"), 3, 40);
        sync(&w, &exclusions()).unwrap();
        let removed = w
            .position(|c| *c == Call::RemoveFile(at("src/main.rs")))
            .expect("old file removed");
        let created = w
            .position(|c| matches!(c, Call::Mirror { dst, .. } if *dst == at("src/main.rs")))
            .expect("directory created");
        let child = w
            .position(|c| matches!(c, Call::Mirror { dst, .. } if *dst == at("src/main.rs/mod.rs")))
            .expect("its file copied");
        assert!(removed < created && created < child, "{:#?}", w.calls());
    }

    // U-STO-38: ... and the other way round.
    #[test]
    fn a_directory_that_became_a_file_is_replaced() {
        let w = synced();
        w.tree.borrow_mut().remove(&src("node_modules"));
        w.meta.borrow_mut().remove(&src("node_modules/big"));
        w.put_file(&src("node_modules"), 9, 50);
        sync(&w, &exclusions()).unwrap();
        let removed = w
            .position(|c| *c == Call::RemoveDirAll(at("node_modules")))
            .expect("old directory removed");
        let created = w
            .position(|c| matches!(c, Call::Mirror { dst, .. } if *dst == at("node_modules")))
            .expect("file created");
        assert!(removed < created, "{:#?}", w.calls());
    }

    // U-STO-39: symlinks are compared by target, never followed.
    #[test]
    fn symlinks_are_recreated_only_when_their_target_changes() {
        let w = synced();
        w.put_link(&src("current"), Path::new("src"));
        mirror_by_hand(&w, &["current"]);

        sync(&w, &exclusions()).unwrap();
        assert_no_mutation(&w);

        w.links
            .borrow_mut()
            .insert(src("current"), p("node_modules"));
        sync(&w, &exclusions()).unwrap();
        assert_eq!(mirrors(&w), vec![at("current")]);
        assert_eq!(
            w.count(|c| matches!(c, Call::Mirror { dst, .. } if dst.starts_with(at("src")))),
            0,
            "the link's target is not walked"
        );
    }

    // U-STO-40: excluded paths are never copied, and are dropped from a baseline
    // that already has them (a path added to .parhelignore since the last sync).
    #[test]
    fn exclusions_are_not_copied_and_are_removed_when_new() {
        let w = world();
        w.put_dir(&src(".worktrees/other"));
        w.put_file(&src(".worktrees/other/file"), 1, 1);
        w.put_dir(&src(".parhel-staging-test"));
        let ex = Exclusions::compute(&ExclusionInput {
            source_root: p("/w/repo"),
            staging_dir: Some(src(".parhel-staging-test")),
            other_worktrees: vec![src(".worktrees/other")],
            user_excluded: vec![PathBuf::from("node_modules")],
            ..ExclusionInput::default()
        });
        sync(&w, &ex).unwrap();
        let mirrored = mirrors(&w);
        for excluded in [
            ".git",
            ".worktrees/other",
            ".parhel-staging-test",
            "node_modules",
        ] {
            assert!(
                !mirrored.iter().any(|m| m.starts_with(at(excluded))),
                "{excluded} was mirrored: {mirrored:?}"
            );
        }
        assert!(mirrored.contains(&at("src/main.rs")));

        // The baseline has node_modules from an earlier sync (before the exclusion).
        let w = synced();
        w.put_dir(&at("node_modules"));
        w.put_file(&at("node_modules/big"), 100, 10);
        let ex = Exclusions::compute(&ExclusionInput {
            source_root: p("/w/repo"),
            user_excluded: vec![PathBuf::from("node_modules")],
            ..ExclusionInput::default()
        });
        sync(&w, &ex).unwrap();
        assert!(
            w.calls().contains(&Call::RemoveDirAll(at("node_modules"))),
            "{:#?}",
            w.calls()
        );
    }

    // U-STO-41: Ctrl-C stops the walk at the next entry.
    #[test]
    fn interrupt_stops_the_sync() {
        let w = world();
        w.interrupt_after.set(Some(Step::Mirror));
        let err = sync(&w, &exclusions()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Interrupted, "{err}");
        assert_eq!(mirrors(&w).len(), 1, "stopped after the first entry");
        assert_eq!(
            w.count(|c| matches!(c, Call::RemoveDirAll(_) | Call::RemoveFile(_))),
            0
        );
    }

    // U-STO-42: an interrupt that arrived before the sync started changes nothing.
    #[test]
    fn interrupt_before_the_sync_changes_nothing() {
        let w = world();
        w.interrupted.store(true, Ordering::SeqCst);
        assert_eq!(
            sync(&w, &exclusions()).unwrap_err().code,
            ErrorCode::Interrupted
        );
        assert_no_mutation(&w);
    }

    // U-STO-43: a copy that fails is reported with its own code and nothing else runs.
    #[test]
    fn a_failing_copy_stops_the_sync_with_its_code() {
        let w = world().failing(Step::Mirror, ErrorCode::CloneFailed);
        assert_eq!(
            sync(&w, &exclusions()).unwrap_err().code,
            ErrorCode::CloneFailed
        );
        assert_eq!(mirrors(&w).len(), 1);
        assert_eq!(dir_metas(&w), Vec::<PathBuf>::new());
    }

    // U-STO-44: a crashed sync leaves a baseline the next one repairs: whatever it
    // finds that differs is copied, and the leftovers of the crash are removed.
    #[test]
    fn a_half_synced_baseline_is_repaired() {
        let w = world();
        // The crashed run got `node_modules/big` wrong (right size, wrong mtime) and left a file
        // the source does not have.
        w.put_dir(&base());
        w.put_dir(&at("node_modules"));
        w.put_file(&at("node_modules/big"), 100, 11);
        w.put_dir(&at("src"));
        w.put_file(&at("src/stale.rs"), 5, 5);
        sync(&w, &exclusions()).unwrap();
        let calls = w.calls();
        assert!(
            calls.contains(&Call::RemoveFile(at("src/stale.rs"))),
            "{calls:#?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::Mirror { dst, .. } if *dst == at("node_modules/big"))),
            "{calls:#?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::Mirror { dst, .. } if *dst == at("src/main.rs"))),
            "{calls:#?}"
        );
    }

    const GIB: u64 = 1 << 30;

    // U-STO-90: a first sync of more than a GiB announces itself, with the size; a
    // smaller one, or any later one, says nothing. The size comes from `stat`s alone and
    // counts what would be copied (not the exclusions).
    #[test]
    fn a_big_first_sync_is_announced_with_its_size() {
        let w = world();
        w.put_file(&src("big/disk.img"), 3 * GIB, 5);
        w.put_file(&src("node_modules/huge"), 40 * GIB, 6);
        let ex = Exclusions::compute(&ExclusionInput {
            source_root: p("/w/repo"),
            user_excluded: vec![PathBuf::from("node_modules")],
            ..ExclusionInput::default()
        });
        let size = first_sync_size(&w, Path::new("/w/repo"), &base(), &ex).unwrap();
        assert_eq!(size, Some(3 * GIB + 20), "disk.img and src/main.rs");
        let notice = first_sync_notice(size.unwrap()).expect("over a GiB");
        assert!(notice.contains("3.0 GiB"), "{notice}");
        assert_no_mutation(&w);

        // Later syncs copy a difference, whatever the tree weighs.
        let w = synced();
        w.put_file(&src("big/disk.img"), 3 * GIB, 5);
        assert_eq!(
            first_sync_size(&w, Path::new("/w/repo"), &base(), &exclusions()).unwrap(),
            None
        );
    }

    // U-STO-90b: the threshold is a GiB, exclusive.
    #[rstest]
    #[case(0, false)]
    #[case(GIB - 1, false)]
    #[case(GIB, false)]
    #[case(GIB + 1, true)]
    #[case(500 * GIB, true)]
    fn the_notice_starts_above_a_gibibyte(#[case] bytes: u64, #[case] announced: bool) {
        assert_eq!(first_sync_notice(bytes).is_some(), announced);
    }
}
