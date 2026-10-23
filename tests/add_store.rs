//! I-S01..I-S29: worktrees of sources without copy-on-write, through the store.
//!
//! The source and the requested destination are on ext4 (`PARHEL_TEST_EXT4`); the store
//! is a fresh directory on btrfs (`PARHEL_TEST_BTRFS`) with the marker `git parhel setup`
//! writes, announced through `PARHEL_STORE` (`common::store`). A missing mount skips the
//! test with a notice, except under `CI` / `PARHEL_REQUIRE_FS=1` where it fails (see
//! `common::volume::test_mount`). The real `setup` and the default store location are
//! tested in `tests/setup.rs`.
#![cfg(target_os = "linux")]

mod common;

use std::fs::{self, File, FileTimes};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, UNIX_EPOCH};

use common::cow::{shares_storage, write_blob};
use common::store::*;
use common::volume::{MountedFs, test_mount};
use common::*;
use rstest::rstest;

const MIB: usize = 1 << 20;
const SIGINT: i32 = 2;

/// Blobs big enough to have extents of their own: tracked, ignored, nested.
const BLOBS: [&str; 3] = [
    "assets/tracked.bin",
    "target/ignored.bin",
    "node_modules/pkg/big.bin",
];

/// A source on ext4, a store on btrfs.
struct World {
    sb: Sandbox,
    store: TestStore,
    src: TestRepo,
}

/// `None` (after a notice) when the mounts are not available.
fn world() -> Option<World> {
    let ext4 = test_mount(MountedFs::Ext4)?;
    let btrfs = test_mount(MountedFs::Btrfs)?;
    let sb = Sandbox::new_in(&ext4);
    let store = TestStore::new(&btrfs);
    let src = project(&sb);
    write_blob(&src.path("assets/tracked.bin"), 2 * MIB, 1);
    src.commit_all("tracked blob");
    write_blob(&src.path("target/ignored.bin"), 4 * MIB, 2);
    write_blob(&src.path("node_modules/pkg/big.bin"), MIB, 3);
    src.assert_clean();
    Some(World { sb, store, src })
}

impl World {
    /// `add <dest> --new-branch <branch> <extra...>` against the store.
    fn add(&self, dest: &Path, branch: &str, extra: &[&str]) -> Output {
        let mut args = vec!["--new-branch", branch];
        args.extend(extra);
        parhel_store(
            &self.sb.env,
            &self.src.root,
            &add_args(dest, &args),
            &self.store,
        )
    }

    fn add_ok(&self, dest: &Path, branch: &str) -> PathBuf {
        let out = self.add(dest, branch, &[]);
        assert_success(&out);
        fs::canonicalize(dest).unwrap()
    }

    /// The branch's worktree through the path the user asked for.
    fn open(&self, dest: &Path) -> TestRepo {
        TestRepo::open(&self.sb.env, dest)
    }

    fn baseline(&self, rel: &str) -> PathBuf {
        self.store.baseline_tree().join(rel)
    }

    /// Nothing of a failed or refused run remains outside the source repository.
    fn assert_no_trace(&self, dest: &Path) {
        assert!(
            fs::symlink_metadata(dest).is_err(),
            "{} was left behind",
            dest.display()
        );
        assert_eq!(
            self.src.worktree_paths(),
            vec![self.src.root.clone()],
            "no registration left"
        );
        assert!(
            self.store.worktrees().is_empty(),
            "{:?}",
            self.store.worktrees()
        );
        assert_no_staging(&[&self.sb.root]);
        self.store.assert_no_staging();
    }
}

/// Let the fixture age past the sync's margin. A file changed within 2 s of the start of a
/// sync (its status-change time cannot tell whether that was before or after the sync
/// looked) is copied again by the next one, which is right, and which the tests that
/// count what a second `add` copies, or that look at what the worktrees still share with
/// the baseline, must not be confused by.
fn settle() {
    std::thread::sleep(Duration::from_millis(2_200));
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Inode and modification time (ns) of a file: unchanged when nothing rewrote it.
fn fingerprint(path: &Path) -> (u64, i64, i64) {
    let md = fs::metadata(path).unwrap();
    (md.ino(), md.mtime(), md.mtime_nsec())
}

fn assert_same_content(src: &TestRepo, dest: &Path) {
    for rel in BLOBS {
        assert_eq!(
            fs::read(src.path(rel)).unwrap(),
            fs::read(dest.join(rel)).unwrap(),
            "{rel}"
        );
    }
}

// I-S01: from ext4 the worktree is created in the store and the requested path is a
// symlink to it.
#[test]
fn is01_add_from_ext4_creates_the_worktree_in_the_store() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let out = w.add(&dest, "f", &[]);
    assert_success(&out);
    assert!(!stderr(&out).contains("warning"), "{}", stderr(&out));

    assert!(is_symlink(&dest), "{} is not a symlink", dest.display());
    let real = w.store.only_worktree();
    assert_eq!(
        fs::read_link(&dest).unwrap(),
        real,
        "points at the real path"
    );
    assert!(real.starts_with(w.store.worktrees_dir()));
    let said = stdout(&out);
    assert!(said.contains("store"), "{said}");
    assert!(said.contains(dest.to_str().unwrap()), "{said}");
    assert!(said.contains(real.to_str().unwrap()), "{said}");

    let wt = w.open(&dest);
    wt.assert_clean();
    assert_eq!(wt.current_branch().as_deref(), Some("f"));
    assert_eq!(
        wt.root, real,
        "git resolves the symlink to the real worktree"
    );
    assert_eq!(wt.read("target/debug/app"), "binary");
    assert_eq!(
        wt.read("node_modules/pkg/index.js"),
        "module.exports = 1;\n"
    );
    assert_same_content(&w.src, &dest);
    // git registered the real path, so the registration survives the symlink.
    assert!(w.src.worktree_paths().contains(&real));
    w.store.assert_no_staging();
    assert_no_staging(&[&w.sb.root]);
}

// I-S02: the source is untouched.
#[test]
fn is02_the_source_is_not_modified() {
    let Some(w) = world() else { return };
    let before = w.src.snapshot();
    w.add_ok(&w.sb.path("wt"), "f");
    let mut after = w.src.snapshot();
    // The new registration and branch are the only differences.
    assert!(after.worktrees.lines().count() > before.worktrees.lines().count());
    after.worktrees = before.worktrees.clone();
    after.admin_dirs = before.admin_dirs.clone();
    after.branches = before.branches.clone();
    before.assert_same(&after);
}

// I-S03: two worktrees share extents with each other and with the baseline: that is
// what makes the second one nearly free.
#[test]
fn is03_worktrees_share_storage_with_each_other_and_the_baseline() {
    let Some(w) = world() else { return };
    settle();
    let a = w.add_ok(&w.sb.path("a"), "fa");
    let b = w.add_ok(&w.sb.path("b"), "fb");
    for rel in BLOBS {
        assert!(
            shares_storage(&w.baseline(rel), &a.join(rel)),
            "{rel}: a does not share with the baseline"
        );
        assert!(
            shares_storage(&w.baseline(rel), &b.join(rel)),
            "{rel}: b does not share with the baseline"
        );
        assert!(
            shares_storage(&a.join(rel), &b.join(rel)),
            "{rel}: a and b do not share"
        );
    }
}

// I-S04: writes diverge: a worktree changing a file changes neither the baseline, nor
// the source, nor another worktree.
#[test]
fn is04_writes_do_not_cross() {
    let Some(w) = world() else { return };
    let a = w.add_ok(&w.sb.path("a"), "fa");
    let b = w.add_ok(&w.sb.path("b"), "fb");
    let original = fs::read(w.src.path("target/ignored.bin")).unwrap();
    fs::write(a.join("target/ignored.bin"), b"rewritten").unwrap();
    assert_eq!(
        fs::read(w.src.path("target/ignored.bin")).unwrap(),
        original
    );
    assert_eq!(
        fs::read(w.baseline("target/ignored.bin")).unwrap(),
        original
    );
    assert_eq!(fs::read(b.join("target/ignored.bin")).unwrap(), original);
}

// I-S05: the second add copies only what changed in the source: untouched baseline
// files keep their inode and mtime, the changed one is rewritten.
#[test]
fn is05_second_add_copies_only_the_changed_files() {
    let Some(w) = world() else { return };
    settle();
    w.add_ok(&w.sb.path("a"), "fa");
    let unchanged: Vec<_> = ["assets/tracked.bin", "target/ignored.bin"]
        .iter()
        .map(|rel| (*rel, fingerprint(&w.baseline(rel))))
        .collect();
    let old_big = fingerprint(&w.baseline("node_modules/pkg/big.bin"));

    write_blob(&w.src.path("node_modules/pkg/big.bin"), MIB, 99);
    w.src.write("target/new.txt", "new\n");
    let b = w.add_ok(&w.sb.path("b"), "fb");

    for (rel, before) in unchanged {
        assert_eq!(fingerprint(&w.baseline(rel)), before, "{rel} was rewritten");
    }
    assert_ne!(
        fingerprint(&w.baseline("node_modules/pkg/big.bin")),
        old_big
    );
    assert_eq!(
        fs::read(w.baseline("node_modules/pkg/big.bin")).unwrap(),
        fs::read(w.src.path("node_modules/pkg/big.bin")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(w.baseline("target/new.txt")).unwrap(),
        "new\n"
    );
    assert_same_content(&w.src, &b);
}

// I-S06: what the source deletes, the baseline and the next worktree lose; the
// worktree made before keeps it.
#[test]
fn is06_deleting_in_the_source_deletes_in_the_baseline() {
    let Some(w) = world() else { return };
    let a = w.add_ok(&w.sb.path("a"), "fa");
    w.src.remove("target/ignored.bin");
    w.src.remove("node_modules");
    let b = w.add_ok(&w.sb.path("b"), "fb");
    assert!(!w.baseline("target/ignored.bin").exists());
    assert!(!w.baseline("node_modules").exists());
    assert!(!b.join("target/ignored.bin").exists());
    assert!(!b.join("node_modules").exists());
    assert!(
        a.join("target/ignored.bin").exists(),
        "the first worktree is independent"
    );
    assert!(a.join("node_modules/pkg/big.bin").exists());
}

// I-S07: a later sync cannot disturb a worktree that already exists.
#[test]
fn is07_existing_worktrees_are_unaffected_by_a_later_sync() {
    let Some(w) = world() else { return };
    let a = w.add_ok(&w.sb.path("a"), "fa");
    let originals: Vec<_> = BLOBS
        .iter()
        .map(|rel| fs::read(a.join(rel)).unwrap())
        .collect();
    for (i, rel) in BLOBS.iter().enumerate() {
        write_blob(&w.src.path(rel), MIB, 10 + i as u64);
    }
    w.src.commit_all("blobs changed");
    w.add_ok(&w.sb.path("b"), "fb");
    for (rel, original) in BLOBS.iter().zip(&originals) {
        assert_eq!(
            &fs::read(a.join(rel)).unwrap(),
            original,
            "{rel} changed under a"
        );
    }
    w.open(&a).assert_clean();
}

// I-S08: an injected failure at every step: no symlink, no registration, no branch,
// no staging, and the baseline is as the sync left it (a complete mirror of the source).
#[rstest]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is08_failure_at_every_step_leaves_no_trace(
    #[values(
        "after_validate",
        "after_worktree_add",
        "mid_clone",
        "after_clone",
        "after_index",
        "after_sync",
        "after_link",
        "before_rename"
    )]
    failpoint: &str,
) {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let out = parhel_store_with_failpoints(
        &w.sb.env,
        &w.src.root,
        &add_args(&dest, &["--new-branch", "f"]),
        &w.store,
        &format!("{failpoint}=error"),
    );
    assert_error(&out, "failpoint");
    w.assert_no_trace(&dest);
    assert_eq!(w.src.branch_tip("f"), None, "the branch was removed");
    if failpoint == "after_validate" {
        assert!(
            w.store.baselines().is_empty(),
            "nothing is synced before validation ends"
        );
    } else {
        assert_eq!(
            tree_digest(&w.store.baseline_tree()),
            tree_digest(&w.src.root),
            "the baseline mirrors the source"
        );
    }
    // The name is free again.
    assert_success(&w.add(&dest, "f", &[]));
}

// I-S08b: a panic (bug) with the symlink in place still rolls everything back.
#[test]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is08b_panic_after_the_link_still_rolls_back() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let out = parhel_store_with_failpoints(
        &w.sb.env,
        &w.src.root,
        &add_args(&dest, &["--new-branch", "f"]),
        &w.store,
        "after_link=panic",
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("failpoint `after_link` panicked"),
        "the run got as far as the link: {}",
        stderr(&out)
    );
    w.assert_no_trace(&dest);
    assert_eq!(w.src.branch_tip("f"), None);
}

// I-S09: another run takes the requested path while this one is about to claim it: the
// loser reports `dest-exists` and leaves nothing; the winner is intact.
fn race_at_the_link(interfere: impl FnOnce(&World, &Path)) {
    let Some(w) = world() else { return };
    let dest = w.sb.path("contested");
    let barrier = Barrier::new(&w.sb.root, "a");
    let mut a = spawn_parhel_store(
        &w.sb.env,
        &w.src.root,
        &add_args(&dest, &["--new-branch", "from-a"]),
        &w.store,
        Some(&barrier.spec("before_rename")),
    );
    barrier.wait_reached(&mut a);
    interfere(&w, &dest);
    let digest = tree_digest(&dest);
    let link = fs::read_link(&dest).ok();
    barrier.release();
    assert_error(&finish(a), "dest-exists");

    assert_eq!(tree_digest(&dest), digest, "the destination is untouched");
    assert_eq!(fs::read_link(&dest).ok(), link, "and so is its link");
    assert_eq!(
        w.src.branch_tip("from-a"),
        None,
        "the loser removed its branch"
    );
    assert_no_staging(&[&w.sb.root]);
    w.store.assert_no_staging();
    // Only what the interference created is registered (the winner, if any).
    for registered in w.src.worktree_paths() {
        assert!(
            registered == w.src.root || registered.starts_with(w.store.worktrees_dir()),
            "{}",
            registered.display()
        );
    }
    assert!(w.store.worktrees().len() <= 1, "{:?}", w.store.worktrees());
}

// I-S09a: a second run wins and the user starts working in its worktree.
#[test]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is09a_losing_run_leaves_the_winner_alone() {
    race_at_the_link(|w, dest| {
        assert_success(&w.add(dest, "from-b", &[]));
        fs::write(dest.join("uncommitted.txt"), "do not lose me").unwrap();
    });
}

// I-S09b: somebody creates an empty directory at the requested path.
#[test]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is09b_a_directory_created_meanwhile_is_not_replaced() {
    race_at_the_link(|_, dest| fs::create_dir(dest).unwrap());
}

// I-S09c: ... or a dangling symlink.
#[test]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is09c_a_dangling_symlink_created_meanwhile_is_not_replaced() {
    race_at_the_link(|_, dest| symlink("nowhere", dest).unwrap());
}

// I-S10: Ctrl-C while the baseline is being synced: exit 130, nothing published, and
// the next run repairs the half-built baseline.
#[test]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is10_sigint_during_the_sync_rolls_back_and_the_next_run_repairs() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let barrier = Barrier::new(&w.sb.root, "sync");
    let mut child = spawn_parhel_store(
        &w.sb.env,
        &w.src.root,
        &add_args(&dest, &["--new-branch", "f"]),
        &w.store,
        Some(&barrier.spec("sync_entry:target/debug/app")),
    );
    barrier.wait_reached(&mut child);
    send_signal(&child, SIGINT);
    barrier.release();
    assert_interrupted(&finish_within(child, 60), SIGINT);
    w.assert_no_trace(&dest);
    assert_eq!(w.src.branch_tip("f"), None);

    let b = w.add_ok(&dest, "f");
    assert_eq!(
        tree_digest(&w.store.baseline_tree()),
        tree_digest(&w.src.root),
        "the baseline was repaired"
    );
    assert_same_content(&w.src, &b);
}

// I-S11: untracked files that mention the source's path are rewritten to the REAL path
// of the worktree: it stays valid however the user reaches it.
#[test]
fn is11_relocation_points_at_the_real_path() {
    let Some(w) = world() else { return };
    w.src.ignore(&["venv/"]);
    w.src.write(
        "venv/pyvenv.cfg",
        format!("home = {}/venv/bin\n", w.src.root.display()),
    );
    w.src.symlink("venv/current", w.src.path("venv/bin"));
    let dest = w.sb.path("wt");
    let out = w.add(&dest, "f", &[]);
    assert_success(&out);
    let real = w.store.only_worktree();
    assert_eq!(
        fs::read_to_string(real.join("venv/pyvenv.cfg")).unwrap(),
        format!("home = {}/venv/bin\n", real.display())
    );
    assert_eq!(
        fs::read_link(real.join("venv/current")).unwrap(),
        real.join("venv/bin")
    );
    assert!(stdout(&out).contains("Relocated 2"), "{}", stdout(&out));
    assert!(
        !fs::read_to_string(real.join("venv/pyvenv.cfg"))
            .unwrap()
            .contains(dest.to_str().unwrap()),
        "the symlink path is not written anywhere"
    );
}

// I-S12: no store: the usual refusal plus the way to get one; nothing created.
#[test]
fn is12_without_a_store_not_cow_suggests_setup() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    // The default environment points PARHEL_STORE at a directory that does not exist.
    let out = parhel(
        &w.sb.env,
        &w.src.root,
        &add_args(&dest, &["--new-branch", "f"]),
    );
    assert_error(&out, "not-cow");
    for needle in ["sudo git parhel setup", "--copy", "--fallback"] {
        assert!(
            stderr(&out).contains(needle),
            "lacks `{needle}`: {}",
            stderr(&out)
        );
    }
    assert!(!dest.exists());
    assert_eq!(w.store.baselines(), Vec::<PathBuf>::new());
}

// I-S13: a store that is configured but unusable is an error, never a silent fallback.
#[rstest]
#[case::no_marker(None)]
#[case::newer_format(Some("format=2\nuser=x\nimage=/i\nunit=u.mount\n"))]
#[case::garbage(Some("garbage"))]
fn is13_unavailable_store_is_an_error(#[case] marker: Option<&str>) {
    let Some(w) = world() else { return };
    let marker_path = w.store.root.join(".parhel-store");
    match marker {
        None => fs::remove_file(&marker_path).unwrap(),
        Some(text) => fs::write(&marker_path, text).unwrap(),
    }
    let dest = w.sb.path("wt");
    let before = w.src.snapshot();
    let out = w.add(&dest, "f", &[]);
    assert_error(&out, "store-unavailable");
    assert!(
        stderr(&out).contains("git parhel doctor"),
        "{}",
        stderr(&out)
    );
    assert!(!dest.exists());
    assert_eq!(w.store.baselines(), Vec::<PathBuf>::new());
    before.assert_same(&w.src.snapshot());
}

// I-S13b: `--copy` and `--fallback` do not need the store, so they keep working when
// it is unavailable.
#[test]
fn is13b_copy_and_fallback_bypass_the_store() {
    let Some(w) = world() else { return };
    fs::remove_file(w.store.root.join(".parhel-store")).unwrap();
    let copy = w.sb.path("copy");
    assert_success(&w.add(&copy, "c", &["--copy"]));
    assert!(!is_symlink(&copy) && copy.join("target/debug/app").exists());
    let plain = w.sb.path("plain");
    assert_success(&w.add(&plain, "p", &["--fallback"]));
    assert!(!is_symlink(&plain));
    assert!(w.store.baselines().is_empty() && w.store.worktrees().is_empty());
}

// I-S14: a source that can clone does not use the store.
#[test]
fn is14_a_source_with_cow_does_not_use_the_store() {
    let Some(btrfs) = test_mount(MountedFs::Btrfs) else {
        return;
    };
    let sb = Sandbox::new_in(&btrfs);
    let store = TestStore::new(&btrfs);
    let src = project(&sb);
    write_blob(&src.path("target/ignored.bin"), 2 * MIB, 5);
    let dest = sb.path("wt");
    let out = parhel_store(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "f"]),
        &store,
    );
    assert_success(&out);
    assert!(!is_symlink(&dest), "a plain directory");
    assert!(shares_storage(
        &src.path("target/ignored.bin"),
        &dest.join("target/ignored.bin")
    ));
    assert!(store.baselines().is_empty() && store.worktrees().is_empty());
}

// I-S15: refusals about the source or the destination come before the store is touched.
#[test]
fn is15_refusals_create_nothing_in_the_store() {
    let Some(w) = world() else { return };
    // Existing destination.
    let taken = w.sb.path("taken");
    fs::create_dir(&taken).unwrap();
    fs::write(taken.join("work.txt"), "precious").unwrap();
    assert_error(&w.add(&taken, "f", &[]), "dest-exists");
    assert_eq!(
        fs::read_to_string(taken.join("work.txt")).unwrap(),
        "precious"
    );
    // Dirty source.
    w.src.write("README.md", "changed\n");
    assert_error(&w.add(&w.sb.path("wt"), "f", &[]), "dirty-source");
    assert!(
        w.store.baselines().is_empty(),
        "no sync before the refusals"
    );
    assert_eq!(w.store.worktrees(), Vec::<PathBuf>::new());
    assert!(!w.sb.path("wt").exists());
}

// I-S16: `git worktree remove` given the symlink removes the real worktree (git
// resolves the path) and leaves the symlink dangling; `doctor --fix` clears it and the
// path can be used again. This is the documented way to remove a store worktree.
#[test]
fn is16_git_worktree_remove_through_the_symlink() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let real = w.add_ok(&dest, "f");
    let out = w
        .src
        .git_output(&["worktree", "remove", "--force", dest.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!real.exists(), "the real worktree is gone");
    assert!(is_symlink(&dest), "the symlink is left behind");
    assert!(!dest.exists(), "and it dangles");
    assert_eq!(w.src.worktree_paths(), vec![w.src.root.clone()]);

    // The path is still taken until the link is cleared.
    assert_error(&w.add(&dest, "f2", &[]), "dest-exists");
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor", "--fix"], &w.store);
    assert_success(&out);
    assert!(
        fs::symlink_metadata(&dest).is_err(),
        "doctor removed the dangling link"
    );
    assert_success(&w.add(&dest, "f2", &[]));
}

// I-S17: two worktrees with one name get distinct real paths; each requested path
// points at its own.
#[test]
fn is17_same_name_twice_gets_a_suffix() {
    let Some(w) = world() else { return };
    fs::create_dir(w.sb.path("one")).unwrap();
    fs::create_dir(w.sb.path("two")).unwrap();
    let a = w.sb.path("one/feat");
    let b = w.sb.path("two/feat");
    w.add_ok(&a, "fa");
    w.add_ok(&b, "fb");
    let names: Vec<String> = w
        .store
        .worktrees()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["feat", "feat-2"]);
    assert_ne!(fs::read_link(&a).unwrap(), fs::read_link(&b).unwrap());
    assert_eq!(w.open(&a).current_branch().as_deref(), Some("fa"));
    assert_eq!(w.open(&b).current_branch().as_deref(), Some("fb"));
}

// I-S18: `--allow-dirty` carries the uncommitted work; the baseline follows the
// source back when the work is gone.
#[test]
fn is18_allow_dirty_and_the_baseline_following_the_source() {
    let Some(w) = world() else { return };
    w.src.write("src/main.rs", "fn main() { dirty(); }\n");
    w.src.write("scratch.txt", "untracked\n");
    let a = w.sb.path("a");
    assert_success(&w.add(&a, "fa", &["--allow-dirty"]));
    let wt = w.open(&a);
    assert_eq!(wt.read("src/main.rs"), "fn main() { dirty(); }\n");
    assert_eq!(wt.read("scratch.txt"), "untracked\n");

    w.src.git(&["checkout", "--", "src/main.rs"]);
    w.src.remove("scratch.txt");
    w.src.assert_clean();
    let b = w.add_ok(&w.sb.path("b"), "fb");
    assert_eq!(
        fs::read_to_string(b.join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(!b.join("scratch.txt").exists());
    assert_eq!(
        wt.read("scratch.txt"),
        "untracked\n",
        "the first worktree keeps its work"
    );
}

// I-S19: a ref other than the source's HEAD: only the tracked files that differ are
// rewritten, the ignored ones arrive.
#[test]
fn is19_checking_out_another_commit() {
    let Some(w) = world() else { return };
    let older = w.src.head();
    w.src.write("Cargo.toml", "[package]\nname = \"newer\"\n");
    w.src.commit_all("newer");
    let dest = w.sb.path("old");
    let out = parhel_store(
        &w.sb.env,
        &w.src.root,
        &add_args(&dest, &["--detach", &older]),
        &w.store,
    );
    assert_success(&out);
    let wt = w.open(&dest);
    wt.assert_clean();
    assert_eq!(wt.head(), older);
    assert_eq!(wt.read("Cargo.toml"), "[package]\nname = \"demo\"\n");
    assert_eq!(wt.read("target/debug/app"), "binary");
}

// I-S20: fidelity from the source through the baseline: modes, nanosecond mtimes,
// symlinks.
#[test]
fn is20_modes_mtimes_and_symlinks_survive() {
    let Some(w) = world() else { return };
    w.src.write("target/run.sh", "#!/bin/sh\n");
    w.src.chmod("target/run.sh", 0o755);
    w.src.write("target/ro.txt", "read only\n");
    w.src.chmod("target/ro.txt", 0o444);
    w.src.symlink("target/rel-link", "run.sh");
    w.src.symlink("target/abs-elsewhere", "/etc/hostname");
    w.src.symlink("target/dangling", "nowhere");
    let when = UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_789);
    for rel in ["target/run.sh", "target/ro.txt"] {
        File::open(w.src.path(rel))
            .unwrap()
            .set_times(FileTimes::new().set_modified(when))
            .unwrap();
    }
    let real = w.add_ok(&w.sb.path("wt"), "f");
    for rel in ["target/run.sh", "target/ro.txt"] {
        let (s, d) = (
            fs::metadata(w.src.path(rel)).unwrap(),
            fs::metadata(real.join(rel)).unwrap(),
        );
        assert_eq!(
            s.permissions().mode() & 0o7777,
            d.permissions().mode() & 0o7777,
            "{rel}"
        );
        assert_eq!(
            (s.mtime(), s.mtime_nsec()),
            (d.mtime(), d.mtime_nsec()),
            "{rel}"
        );
    }
    for rel in ["target/rel-link", "target/abs-elsewhere", "target/dangling"] {
        assert_eq!(
            fs::read_link(w.src.path(rel)).unwrap(),
            fs::read_link(real.join(rel)).unwrap(),
            "{rel}"
        );
    }
}

// I-S21: the worktree works through the symlink like any other: status, commits,
// `git worktree list` showing the real path.
#[test]
fn is21_working_through_the_symlink() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let real = w.add_ok(&dest, "f");
    let wt = w.open(&dest);
    wt.write("work.txt", "work\n");
    wt.commit_all("work through the link");
    assert_eq!(w.src.branch_tip("f"), Some(wt.head()));
    assert!(
        fs::read_to_string(real.join("work.txt"))
            .unwrap()
            .contains("work")
    );
    let listed = w.src.worktree_paths();
    assert!(
        listed.contains(&real) && !listed.contains(&dest),
        "{listed:?}"
    );
}

// I-S22: `.parhelignore` is honored: the paths are in neither the baseline nor the
// worktree.
#[test]
fn is22_parhelignore_paths_are_not_copied() {
    let Some(w) = world() else { return };
    w.src.write(".parhelignore", "node_modules/\n");
    w.src.commit_all("parhelignore");
    let real = w.add_ok(&w.sb.path("wt"), "f");
    assert!(!w.baseline("node_modules").exists());
    assert!(!real.join("node_modules").exists());
    assert!(real.join("target/debug/app").exists());
}

// I-S23: nested worktrees of the repository inside the source are not part of the baseline.
#[test]
fn is23_nested_worktrees_are_not_mirrored() {
    let Some(w) = world() else { return };
    let nested = w.src.path(".worktrees/other");
    w.src.linked_worktree(&nested, "other");
    w.add_ok(&w.sb.path("wt"), "f");
    assert!(!w.baseline(".worktrees/other").exists());
    assert!(!w.baseline(".git").exists());
}

// I-S24: doctor is quiet about a healthy store, and a source on ext4 is no longer a
// problem while the store serves it.
#[test]
fn is24_doctor_is_quiet_about_a_healthy_store() {
    let Some(w) = world() else { return };
    w.add_ok(&w.sb.path("wt"), "f");
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor"], &w.store);
    assert_success(&out);
    assert!(stdout(&out).contains("no issues found"), "{}", stdout(&out));
}

// I-S25: a dangling link is reported; `--fix` removes it and nothing else.
#[test]
fn is25_doctor_reports_and_fixes_a_dangling_link() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let real = w.add_ok(&dest, "f");
    w.src
        .git(&["worktree", "remove", "--force", real.to_str().unwrap()]);
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor"], &w.store);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("store-dangling-link"),
        "{}",
        stdout(&out)
    );
    assert!(is_symlink(&dest), "read-only without --fix");

    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor", "--fix"], &w.store);
    assert_success(&out);
    assert!(stdout(&out).contains("(fixed)"), "{}", stdout(&out));
    assert!(fs::symlink_metadata(&dest).is_err());
    assert!(
        w.baseline("target/ignored.bin").exists(),
        "the baseline stays"
    );
}

// I-S26: a directory in worktrees/<id>/ that git does not know is reported and kept.
#[test]
fn is26_doctor_orphan_directory_is_reported_and_never_deleted() {
    let Some(w) = world() else { return };
    w.add_ok(&w.sb.path("wt"), "f");
    let id_dir = w.store.only_worktree().parent().unwrap().to_path_buf();
    let ghost = id_dir.join("ghost");
    fs::create_dir(&ghost).unwrap();
    fs::write(ghost.join("work.txt"), "mine").unwrap();
    for args in [&["doctor"][..], &["doctor", "--fix"][..]] {
        let out = parhel_store(&w.sb.env, &w.src.root, args, &w.store);
        assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
        assert!(stdout(&out).contains("store-orphan"), "{}", stdout(&out));
        assert_eq!(fs::read_to_string(ghost.join("work.txt")).unwrap(), "mine");
    }
}

// I-S27: a baseline whose source is gone is stale; `--fix` deletes it (and only it).
#[test]
fn is27_doctor_stale_baseline() {
    let Some(w) = world() else { return };
    w.add_ok(&w.sb.path("wt"), "f");
    let baseline = w.store.baselines().remove(0);
    fs::write(baseline.join("meta"), "source=/gone/repo\n").unwrap();
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor"], &w.store);
    assert!(
        stdout(&out).contains("store-stale-baseline"),
        "{}",
        stdout(&out)
    );
    assert!(baseline.exists());
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor", "--fix"], &w.store);
    assert_success(&out);
    assert!(!baseline.exists());
    assert!(
        w.store.only_worktree().exists(),
        "worktrees are never deleted"
    );
}

// I-S28: a store that cannot be used is reported by doctor.
#[test]
fn is28_doctor_unavailable_store() {
    let Some(w) = world() else { return };
    fs::remove_file(w.store.root.join(".parhel-store")).unwrap();
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor"], &w.store);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("store-unavailable"),
        "{}",
        stdout(&out)
    );
}

// I-S29: killed after the rename and before the registration was repaired (SIGKILL at
// `after_rename`): `doctor --fix` repairs the worktree at its real path and keeps the
// symlink; never removes either.
#[test]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is29_sigkill_after_publishing_is_repaired_not_removed() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let out = parhel_store_with_failpoints(
        &w.sb.env,
        &w.src.root,
        &add_args(&dest, &["--new-branch", "f"]),
        &w.store,
        "after_rename=kill",
    );
    assert!(!out.status.success());
    let real = w.store.only_worktree();
    assert_eq!(fs::read_link(&dest).unwrap(), real, "published and linked");
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor", "--fix"], &w.store);
    assert_success(&out);
    assert!(real.exists() && is_symlink(&dest), "kept");
    assert!(
        w.src.worktree_paths().contains(&real),
        "registered at the real path"
    );
    w.open(&dest).assert_clean();
    assert_eq!(
        w.src.branch_tip("f"),
        Some(w.src.head()),
        "the branch is kept"
    );
}

// I-S30: SIGKILL before the rename: `doctor --fix` removes the symlink the run created,
// the registration, the branch and the staging directory.
#[test]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is30_sigkill_after_the_link_is_cleaned_up_by_doctor() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    let out = parhel_store_with_failpoints(
        &w.sb.env,
        &w.src.root,
        &add_args(&dest, &["--new-branch", "f"]),
        &w.store,
        "after_link=kill",
    );
    assert!(!out.status.success());
    assert!(is_symlink(&dest), "the run died holding the link");
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor", "--fix"], &w.store);
    assert_success(&out);
    w.assert_no_trace(&dest);
    assert_eq!(w.src.branch_tip("f"), None);
}

// I-S31: `--submodules=reinit` through the store. The submodule is not mirrored (it is
// initialized again with its own git dir in the new worktree), its links name the REAL
// path, and working in it never reaches the source. Without the flag the run is
// refused before the store is touched.
#[test]
fn is31_submodules_reinit_through_the_store() {
    let Some(w) = world() else { return };
    let lib = w.sb.repo("lib");
    lib.write("lib.rs", "v1\n");
    lib.commit_all("lib v1");
    lib.write("lib.rs", "v2\n");
    lib.commit_all("lib v2");
    w.src.add_submodule(&lib, "libs/lib");
    w.src.assert_clean();

    let refused = w.add(&w.sb.path("refused"), "r", &[]);
    assert_error(&refused, "submodules-present");
    assert!(stderr(&refused).contains("--submodules=reinit"));
    assert!(
        w.store.baselines().is_empty(),
        "nothing synced before the refusal"
    );

    let dest = w.sb.path("wt");
    let out = w.add(&dest, "s", &["--submodules=reinit"]);
    assert_success(&out);
    let real = w.store.only_worktree();
    let wt = w.open(&dest);
    wt.assert_clean();
    assert_eq!(wt.read("libs/lib/lib.rs"), "v2\n");
    assert!(
        !w.baseline("libs/lib").exists(),
        "the submodule is reinitialized, not mirrored"
    );
    assert!(w.baseline("target/ignored.bin").exists());

    let src_sub = TestRepo::open(&w.sb.env, &w.src.path("libs/lib"));
    let sub = TestRepo::open(&w.sb.env, &real.join("libs/lib"));
    assert_ne!(sub.git_dir(), src_sub.git_dir(), "its own git dir");
    assert!(
        sub.git_dir().starts_with(wt.git_dir()),
        "{} is not under {}",
        sub.git_dir().display(),
        wt.git_dir().display()
    );
    assert!(
        fs::read_to_string(real.join("libs/lib/.git"))
            .unwrap()
            .contains(wt.git_dir().to_str().unwrap()),
        "the submodule's gitfile leads into the new worktree's admin dir"
    );
    sub.git(&["checkout", "-q", "HEAD~1"]);
    assert_eq!(sub.read("lib.rs"), "v1\n");
    assert_eq!(
        src_sub.read("lib.rs"),
        "v2\n",
        "the source's submodule is unchanged"
    );
    w.src.assert_clean();
}

/// An access ACL naming user 4242 on top of the owner's `owner`.
fn acl_for(owner: u16, named: u16, mask: u16) -> Vec<u8> {
    use common::xattr::acl;
    acl::encode(&[
        (acl::USER_OBJ, owner, acl::NOBODY),
        (acl::USER, named, 4242),
        (acl::GROUP_OBJ, 0, acl::NOBODY),
        (acl::MASK, mask, acl::NOBODY),
        (acl::OTHER, 0, acl::NOBODY),
    ])
}

// I-S32: extended attributes, POSIX ACLs and nanosecond mtimes of files and directories
// make it through the mirror (ext4 to the baseline) and through the reflink clone (the
// baseline to the worktree): the clone of an entry is only as faithful as the baseline.
#[test]
fn is32_xattrs_acls_and_mtimes_survive_the_baseline_and_the_clone() {
    use common::xattr::{self, acl};
    let Some(w) = world() else { return };
    w.src.ignore(&["secrets/"]);
    w.src.write("target/tagged.bin", "t");
    w.src.mkdir("target/tagged-dir");
    w.src.write("target/tagged-dir/inner.txt", "i");
    w.src.write("secrets/.env", "TOKEN=1\n");
    xattr::set(&w.src.path("target/tagged.bin"), "user.parhel.one", b"1").unwrap();
    xattr::set(
        &w.src.path("target/tagged.bin"),
        "user.parhel.two",
        b"two words",
    )
    .unwrap();
    xattr::set(&w.src.path("target/tagged-dir"), "user.parhel.dir", b"d").unwrap();
    let file_acl = acl_for(0o6, 0o4, 0o4);
    let dir_acl = acl_for(0o7, 0o5, 0o5);
    xattr::set(&w.src.path("secrets/.env"), acl::ACCESS, &file_acl).unwrap();
    for dir in ["secrets", "target/tagged-dir"] {
        xattr::set(&w.src.path(dir), acl::ACCESS, &dir_acl).unwrap();
        xattr::set(&w.src.path(dir), acl::DEFAULT, &dir_acl).unwrap();
    }
    // Directories last: filling one moves its mtime.
    let when = UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_789);
    for rel in [
        "target/tagged.bin",
        "target/tagged-dir/inner.txt",
        "secrets/.env",
    ] {
        File::open(w.src.path(rel))
            .unwrap()
            .set_times(FileTimes::new().set_modified(when))
            .unwrap();
    }
    for rel in ["target/tagged-dir", "secrets", "target"] {
        File::open(w.src.path(rel))
            .unwrap()
            .set_times(FileTimes::new().set_modified(when))
            .unwrap();
    }

    let real = w.add_ok(&w.sb.path("wt"), "f");
    let baseline = w.store.baseline_tree();
    for (label, copy) in [("baseline", &baseline), ("worktree", &real)] {
        for rel in [
            "target/tagged.bin",
            "target/tagged-dir",
            "target/tagged-dir/inner.txt",
            "secrets/.env",
            "secrets",
            "target",
        ] {
            let (s, c) = (w.src.path(rel), copy.join(rel));
            assert_eq!(
                xattr::user_attrs(&c),
                xattr::user_attrs(&s),
                "user attributes of `{rel}` in the {label}"
            );
            for name in [acl::ACCESS, acl::DEFAULT] {
                assert_eq!(
                    xattr::get(&c, name).unwrap(),
                    xattr::get(&s, name).unwrap(),
                    "{name} of `{rel}` in the {label}"
                );
            }
            let (sm, cm) = (fs::metadata(&s).unwrap(), fs::metadata(&c).unwrap());
            assert_eq!(
                sm.permissions().mode() & 0o7777,
                cm.permissions().mode() & 0o7777,
                "mode of `{rel}` in the {label}"
            );
            assert_eq!(
                (sm.mtime(), sm.mtime_nsec()),
                (cm.mtime(), cm.mtime_nsec()),
                "mtime of `{rel}` in the {label}"
            );
        }
    }
    assert_eq!(
        xattr::get(&real.join("secrets/.env"), acl::ACCESS).unwrap(),
        Some(file_acl)
    );
    // The root of the worktree takes the source's too.
    assert_eq!(
        xattr::get(&real, acl::ACCESS).unwrap(),
        xattr::get(&w.src.root, acl::ACCESS).unwrap()
    );
}

// I-S33: the store fills up while the first sync copies (a 128 MiB btrfs, 192 MiB of
// data): the run fails with the system's words, nothing of it remains, the baseline is
// left usable, and a smaller source works right after.
#[test]
fn is33_a_full_store_fails_cleanly_and_recovers() {
    let Some(ext4) = test_mount(MountedFs::Ext4) else {
        return;
    };
    let Some(small) = test_mount(MountedFs::SmallBtrfs) else {
        return;
    };
    let sb = Sandbox::new_in(&ext4);
    let store = TestStore::new(&small);
    let src = project(&sb);
    src.ignore(&["blobs/"]);
    for i in 0..3 {
        write_blob(&src.path(format!("blobs/b{i}.bin")), 64 * MIB, i);
    }
    let dest = sb.path("wt");
    let args = add_args(&dest, &["--new-branch", "f"]);
    let out = parhel_store(&sb.env, &src.root, &args, &store);
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("No space left on device"),
        "{}",
        stderr(&out)
    );
    assert!(fs::symlink_metadata(&dest).is_err());
    assert_eq!(src.worktree_paths(), vec![src.root.clone()]);
    assert_eq!(src.branch_tip("f"), None);
    assert_eq!(store.worktrees(), Vec::<PathBuf>::new());
    store.assert_no_staging();
    assert_no_staging(&[&sb.root]);

    // Less to copy: the earlier blobs in the baseline that are gone from the source are
    // removed by the sync, which is what makes room.
    fs::remove_dir_all(src.path("blobs")).unwrap();
    assert_success(&parhel_store(&sb.env, &src.root, &args, &store));
    TestRepo::open(&sb.env, &dest).assert_clean();
    assert!(!store.baseline_tree().join("blobs").exists());
}

// I-S34: a second `add` for the same repository while the first is still syncing waits
// for it (the baseline is one) instead of failing, and both succeed.
#[test]
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
fn is34_a_second_add_waits_for_the_first_to_finish_with_the_baseline() {
    let Some(w) = world() else { return };
    let barrier = Barrier::new(&w.sb.root, "lock");
    let (a_dest, b_dest) = (w.sb.path("a"), w.sb.path("b"));
    let mut a = spawn_parhel_store(
        &w.sb.env,
        &w.src.root,
        &add_args(&a_dest, &["--new-branch", "fa"]),
        &w.store,
        Some(&barrier.spec("sync_entry:target/debug/app")),
    );
    barrier.wait_reached(&mut a);
    let mut b = spawn_parhel_store(
        &w.sb.env,
        &w.src.root,
        &add_args(&b_dest, &["--new-branch", "fb"]),
        &w.store,
        None,
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        b.try_wait().unwrap().is_none(),
        "the second run should be waiting for the baseline"
    );
    assert!(!b_dest.exists());
    barrier.release();
    assert_success(&finish_within(a, 120));
    assert_success(&finish_within(b, 120));
    assert_eq!(w.store.worktrees().len(), 2);
    w.open(&a_dest).assert_clean();
    w.open(&b_dest).assert_clean();
    assert_same_content(&w.src, &fs::canonicalize(&b_dest).unwrap());
}

// I-S35: the store's registry is tab separated lines: a destination with a tab or a
// newline in its path is refused before anything is created.
#[rstest]
#[case::tab("tab\there")]
#[case::newline("new\nline")]
fn is35_destinations_the_registry_cannot_hold_are_refused(#[case] name: &str) {
    let Some(w) = world() else { return };
    let dest = w.sb.path(name);
    let out = w.add(&dest, "f", &[]);
    assert_error(&out, "usage");
    assert!(
        stderr(&out).contains("tab or a newline"),
        "{}",
        stderr(&out)
    );
    assert_eq!(w.store.baselines(), Vec::<PathBuf>::new());
    w.assert_no_trace(&dest);
}

// I-S36: a baseline whose last sync died without a word (marked `state=syncing`) is not
// trusted, because the sync compares metadata only: a file with the right size and
// mtime and wrong data (what a power loss leaves) is replaced, not kept.
#[test]
fn is36_an_unfinished_baseline_is_mirrored_again() {
    let Some(w) = world() else { return };
    w.add_ok(&w.sb.path("a"), "fa");
    let victim = w.baseline("target/ignored.bin");
    let (len, mtime) = {
        let md = fs::metadata(&victim).unwrap();
        (md.len(), md.modified().unwrap())
    };
    fs::write(&victim, vec![0u8; usize::try_from(len).unwrap()]).unwrap();
    File::options()
        .write(true)
        .open(&victim)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mtime))
        .unwrap();
    let meta = w.store.baselines().remove(0).join("meta");
    let text = fs::read_to_string(&meta).unwrap();
    assert!(text.contains("state=synced"), "{text}");
    fs::write(&meta, text.replace("state=synced", "state=syncing")).unwrap();

    let b = w.add_ok(&w.sb.path("b"), "fb");
    assert_same_content(&w.src, &b);
    assert_eq!(
        fs::read(&victim).unwrap(),
        fs::read(w.src.path("target/ignored.bin")).unwrap()
    );
    assert!(fs::read_to_string(&meta).unwrap().contains("state=synced"));
}

// I-S37: a directory the source made read-only (a vendored tree, a Go module cache) is
// mirrored with its mode, and a later change inside it still reaches the baseline: a new
// subdirectory (`mkdir` in a 0555 directory is refused even for its owner), and a file
// that became a directory.
#[test]
fn is37_changes_inside_a_read_only_directory_reach_the_baseline() {
    let Some(w) = world() else { return };
    let locked = w.src.path("target/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("file"), "f\n").unwrap();
    fs::write(locked.join("was-a-file"), "w\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let a = w.add_ok(&w.sb.path("a"), "fa");
    let held = w.baseline("target/locked");
    assert_eq!(fs::metadata(&held).unwrap().mode() & 0o7777, 0o555);

    // The change, as its owner would make it: open the directory, change, close it.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(locked.join("fresh")).unwrap();
    fs::write(locked.join("fresh/inside"), "i\n").unwrap();
    fs::remove_file(locked.join("was-a-file")).unwrap();
    fs::create_dir(locked.join("was-a-file")).unwrap();
    fs::write(locked.join("was-a-file/now"), "n\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let out = w.add(&w.sb.path("b"), "fb", &[]);
    let restore = |path: &Path| {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    };
    restore(&locked);
    restore(&held);
    restore(&a.join("target/locked"));
    assert_success(&out);
    assert_eq!(
        fs::read_to_string(held.join("fresh/inside")).unwrap(),
        "i\n"
    );
    assert_eq!(
        fs::read_to_string(held.join("was-a-file/now")).unwrap(),
        "n\n"
    );
    let b = fs::canonicalize(w.sb.path("b")).unwrap();
    assert_eq!(
        fs::read_to_string(b.join("target/locked/fresh/inside")).unwrap(),
        "i\n"
    );
}

/// Rewrite `path` with `content` (the same length as before) and put its modification
/// time and mode back: the file is, to a comparison of size, mtime and mode, untouched.
fn rewrite_and_restore(path: &Path, content: &[u8]) {
    let before = fs::metadata(path).unwrap();
    assert_eq!(
        before.len(),
        content.len() as u64,
        "not a same-size rewrite"
    );
    fs::write(path, content).unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    fs::set_permissions(path, before.permissions()).unwrap();
    let after = fs::metadata(path).unwrap();
    assert_eq!(
        (after.len(), after.mtime(), after.mtime_nsec(), after.mode()),
        (
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.mode()
        ),
        "the rewrite did not restore the file's looks"
    );
}

// I-S38: the sync does not take size, mtime and mode on trust: a file rewritten with the
// same size and its times put back (a build tool, `cp -p`, `touch -d`) is still copied,
// because its status-change time cannot be put back. The worktree made next has the new
// content.
#[test]
fn is38_a_same_size_rewrite_with_the_mtime_restored_reaches_the_baseline() {
    let Some(w) = world() else { return };
    w.add_ok(&w.sb.path("a"), "fa");
    let victim = "target/ignored.bin";
    let old = fs::read(w.src.path(victim)).unwrap();
    let new: Vec<u8> = old.iter().map(|b| !b).collect();
    rewrite_and_restore(&w.src.path(victim), &new);
    w.src.assert_clean();

    let b = w.add_ok(&w.sb.path("b"), "fb");
    assert_eq!(
        fs::read(w.baseline(victim)).unwrap(),
        new,
        "baseline is stale"
    );
    assert_eq!(
        fs::read(b.join(victim)).unwrap(),
        new,
        "the worktree is stale"
    );
    // And only that file was rewritten: its neighbours kept their inodes.
    assert_same_content(&w.src, &b);
}

// I-S38b: a baseline whose `meta` has no mark of a finished sync (an older store, a
// damaged file) is not trusted at all: a file in it that is wrong with the right size and
// mtime is replaced.
#[test]
fn is38b_a_baseline_without_a_mark_is_copied_again() {
    let Some(w) = world() else { return };
    w.add_ok(&w.sb.path("a"), "fa");
    let victim = w.baseline("target/ignored.bin");
    let (len, mtime) = {
        let md = fs::metadata(&victim).unwrap();
        (md.len(), md.modified().unwrap())
    };
    fs::write(&victim, vec![0u8; usize::try_from(len).unwrap()]).unwrap();
    File::options()
        .write(true)
        .open(&victim)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mtime))
        .unwrap();
    let meta = w.store.baselines().remove(0).join("meta");
    let text = fs::read_to_string(&meta).unwrap();
    assert!(text.contains("sync_start_ns="), "{text}");
    fs::write(
        &meta,
        text.lines()
            .filter(|l| !l.starts_with("sync_start_ns="))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();

    let b = w.add_ok(&w.sb.path("b"), "fb");
    assert_same_content(&w.src, &b);
    assert!(
        fs::read_to_string(&meta)
            .unwrap()
            .contains("sync_start_ns=")
    );
}

/// The store as `systemctl stop` leaves it: its directory is there, empty, and what was
/// in it is out of reach. Here the whole directory is moved away and back.
struct StoreDown {
    root: PathBuf,
    away: PathBuf,
}

impl StoreDown {
    fn new(store: &TestStore) -> Self {
        let away = store.root.with_extension("away");
        fs::rename(&store.root, &away).unwrap();
        Self {
            root: store.root.clone(),
            away,
        }
    }

    fn up(&self) {
        if self.away.exists() {
            fs::rename(&self.away, &self.root).unwrap();
        }
    }
}

impl Drop for StoreDown {
    fn drop(&mut self) {
        self.up();
    }
}

fn registrations(src: &TestRepo) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(src.common_dir().join("worktrees"))
        .map(|rd| {
            rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

// I-S39: the store's mount is down (reboot gone wrong, `systemctl stop`) and git cleans
// up: `git worktree prune`, and the `gc` that runs it, forget the registration of a
// worktree whose directory is missing. The worktrees of the store are locked when they
// are published, so they survive it; `doctor` says what is at stake while the store is
// down; and the same worktree unlocked is forgotten (the control that makes this test
// able to fail).
#[test]
fn is39_pruning_while_the_store_is_down_keeps_the_registrations() {
    let Some(w) = world() else { return };
    let dest = w.sb.path("wt");
    w.add_ok(&dest, "fa");
    let real = w.store.only_worktree();
    let listed = w.src.git(&["worktree", "list", "--porcelain"]);
    assert!(
        listed
            .lines()
            .any(|l| l == format!("locked parhel store: {}", w.store.root.display())),
        "published worktrees are locked, with the store named:\n{listed}"
    );
    assert_eq!(registrations(&w.src).len(), 1);

    let down = StoreDown::new(&w.store);
    // doctor, with the store down, names the registrations at stake (and changes nothing).
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor", "--fix"], &w.store);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let said = stdout(&out);
    assert!(said.contains("store-worktrees-unreachable"), "{said}");
    assert!(said.contains(real.to_str().unwrap()), "{said}");
    assert!(said.contains("git worktree prune"), "{said}");
    assert!(!said.contains("(fixed)"), "{said}");

    w.src.git(&["worktree", "prune", "--expire", "now"]);
    w.src
        .git(&["-c", "gc.worktreePruneExpire=now", "gc", "--quiet"]);
    assert_eq!(
        registrations(&w.src).len(),
        1,
        "prune forgot a locked worktree"
    );
    down.up();
    // The worktree is as it was, through the symlink.
    let wt = w.open(&dest);
    wt.assert_clean();
    assert!(w.src.worktree_paths().contains(&real));
    let out = parhel_store(&w.sb.env, &w.src.root, &["doctor"], &w.store);
    assert_success(&out);

    // Control: unlocked, the same prune forgets it.
    w.src.git(&["worktree", "unlock", real.to_str().unwrap()]);
    let down = StoreDown::new(&w.store);
    w.src.git(&["worktree", "prune", "--expire", "now"]);
    assert!(
        registrations(&w.src).is_empty(),
        "an unlocked one should be pruned"
    );
    down.up();
}
