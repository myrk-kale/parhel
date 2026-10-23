//! The store: discovery and the baseline sync.
//!
//! TDD state: both entry points answer `not-implemented`; the tests in this module
//! and in `add`, `doctor` and `rollback` describe what they must do.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::domain::exclusions::Exclusions;
use crate::domain::store::StoreState;
use crate::domain::{Error, Result};
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
pub fn discover(_fs: &dyn CowFs, _path: &Path) -> Result<StoreState> {
    Err(Error::not_implemented("store discovery"))
}

/// Bring the baseline tree `baseline` in line with the live source worktree `source`:
/// walk the source anchored on directory descriptors (`CowFs::list_dir_within`,
/// `stat_within`), skip `exclusions`, and for each entry apply
/// [`sync_action`](crate::domain::store::sync_action); delete baseline entries the
/// source does not have (or that are now excluded); set a directory's metadata after
/// its children. Stops with `interrupted` when `interrupted` is raised.
pub fn sync_baseline(
    _fs: &dyn CowFs,
    _source: &Path,
    _baseline: &Path,
    _exclusions: &Exclusions,
    _interrupted: &AtomicBool,
) -> Result<SyncReport> {
    Err(Error::not_implemented("baseline sync"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{Call, MARKER, REPO_ID, STORE, Step, World, ext4, p};
    use crate::domain::ErrorCode;
    use crate::domain::exclusions::ExclusionInput;
    use crate::domain::store::{Marker, Store};
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
    /// (20 bytes), and an empty baseline directory.
    fn world() -> World {
        let w = World::default().with_store();
        w.put_file(&src("node_modules/big"), 100, 10);
        w.put_file(&src("src/main.rs"), 20, 20);
        w.put_dir(&base());
        w
    }

    fn sync(w: &World, exclusions: &Exclusions) -> Result<SyncReport> {
        sync_baseline(w, Path::new("/w/repo"), &base(), exclusions, &w.interrupted)
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
}
