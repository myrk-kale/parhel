//! I-W01..I-W06: the walk over the source tree cannot be turned against us while it
//! runs. Another process swaps a directory for a symlink, a regular file for a FIFO,
//! or the user presses Ctrl-C; the walk is frozen at one entry with the `walk_entry`
//! failpoint (`walk_entry:<path relative to the source>`) or at `mid_clone`, so every
//! test is deterministic. macOS and Linux.

mod common;

use std::fs;
use std::os::unix::fs::{FileTypeExt, symlink};
use std::path::Path;
use std::process::Output;

use common::*;

/// Full copy everywhere; reflink also walks the tree on Linux (macOS clones a whole
/// directory in one call and never walks it).
fn walking_modes() -> Vec<&'static [&'static str]> {
    if cfg!(target_os = "linux") {
        vec![&[], &["--copy"]]
    } else {
        vec![&["--copy"]]
    }
}

/// Both modes, on both platforms.
fn all_modes() -> [&'static [&'static str]; 2] {
    [&[], &["--copy"]]
}

fn start(
    sb: &Sandbox,
    src: &TestRepo,
    dest: &Path,
    mode: &[&str],
    failpoints: &str,
) -> std::process::Child {
    let mut args = vec!["--new-branch", "w"];
    args.extend(mode);
    spawn_parhel(&sb.env, &src.root, &add_args(dest, &args), Some(failpoints))
}

fn assert_failed_without_trace(sb: &Sandbox, src: &TestRepo, dest: &Path, out: &Output) {
    assert_error(out, "clone-failed");
    assert!(!dest.exists(), "nothing published");
    assert_eq!(src.branch_tip("w"), None, "the branch was removed");
    assert_no_staging(&[&sb.root]);
}

/// A directory outside the source with a file nobody asked to copy.
fn outside_with_secret(sb: &Sandbox) -> std::path::PathBuf {
    let outside = sb.path("outside");
    fs::create_dir_all(outside.join("gen")).unwrap();
    fs::write(outside.join("secret.txt"), "not yours").unwrap();
    // The same names as the source directory it replaces.
    fs::write(outside.join("main.rs"), "fn evil() {}\n").unwrap();
    fs::write(outside.join("gen/out.txt"), "not yours either").unwrap();
    outside
}

// I-W01: a directory below an ignored one is swapped for a symlink to the outside
// while the walk is about to enter it: the walk fails, nothing outside is copied.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn iw01_directory_swapped_for_a_symlink_during_the_walk_is_not_followed() {
    for mode in walking_modes() {
        let sb = Sandbox::new();
        let src = project(&sb);
        src.ignore(&["deps/"]);
        src.write("deps/inner/real.txt", "mine");
        src.write("deps/other.txt", "also mine");
        let outside = outside_with_secret(&sb);
        let barrier = Barrier::new(&sb.root, "walk");
        let dest = sb.path("dest");
        let mut child = start(
            &sb,
            &src,
            &dest,
            mode,
            &barrier.spec("walk_entry:deps/inner"),
        );
        barrier.wait_reached(&mut child);
        fs::rename(src.path("deps/inner"), src.path("deps/inner.moved")).unwrap();
        symlink(&outside, src.path("deps/inner")).unwrap();
        barrier.release();
        let out = finish_within(child, 60);
        assert_failed_without_trace(&sb, &src, &dest, &out);
        assert!(stderr(&out).contains("deps/inner"), "{}", stderr(&out));
    }
}

// I-W02: the same swap one level up, in an ancestor of what is cloned: `src` holds
// an entry of `.parhelignore`, so the plan creates `src` and clones its other entries
// one by one (`src/gen`); `src` becomes a symlink between the plan and that clone.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn iw02_ancestor_directory_swapped_between_plan_and_clone_is_not_followed() {
    for mode in all_modes() {
        let sb = Sandbox::new();
        let src = project(&sb);
        src.ignore(&["src/gen/", "src/skipped/"]);
        src.write(".parhelignore", "src/skipped/\n");
        src.commit_all("parhelignore");
        src.write("src/gen/out.txt", "generated");
        src.write("src/skipped/log.txt", "left out");
        let outside = outside_with_secret(&sb);
        let barrier = Barrier::new(&sb.root, "plan");
        let dest = sb.path("dest");
        let mut child = start(&sb, &src, &dest, mode, &barrier.spec("mid_clone"));
        barrier.wait_reached(&mut child);
        fs::rename(src.path("src"), src.path("src.moved")).unwrap();
        symlink(&outside, src.path("src")).unwrap();
        barrier.release();
        let out = finish_within(child, 60);
        assert_failed_without_trace(&sb, &src, &dest, &out);
    }
}

// I-W03: a regular file is swapped for a FIFO after it was listed: opening it would
// block forever (and Ctrl-C would not stop it). The walk fails promptly instead.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn iw03_file_swapped_for_a_fifo_fails_instead_of_blocking() {
    for mode in walking_modes() {
        let sb = Sandbox::new();
        let src = project(&sb);
        src.ignore(&["deps/"]);
        src.write("deps/big.log", "log line\n");
        let barrier = Barrier::new(&sb.root, "fifo");
        let dest = sb.path("dest");
        let mut child = start(
            &sb,
            &src,
            &dest,
            mode,
            &barrier.spec("walk_entry:deps/big.log"),
        );
        barrier.wait_reached(&mut child);
        fs::remove_file(src.path("deps/big.log")).unwrap();
        let fifo = std::ffi::CString::new(src.path("deps/big.log").to_str().unwrap()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            fs::symlink_metadata(src.path("deps/big.log"))
                .unwrap()
                .file_type()
                .is_fifo()
        );
        barrier.release();
        let out = finish_within(child, 30);
        assert_failed_without_trace(&sb, &src, &dest, &out);
        assert!(stderr(&out).contains("deps/big.log"), "{}", stderr(&out));
    }
}

// I-W04: Ctrl-C while a long copy is under way stops the walk at the next entry (it
// does not go on to the end of the tree) and rolls everything back.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn iw04_sigint_during_the_walk_stops_it_and_rolls_back() {
    for mode in walking_modes() {
        let sb = Sandbox::new();
        let src = project(&sb);
        src.ignore(&["deps/"]);
        src.write("deps/a/b/c/leaf.txt", "deep");
        let before = src.snapshot();
        let here = Barrier::new(&sb.root, "here");
        let later = Barrier::new(&sb.root, "later");
        let dest = sb.path("dest");
        let spec = format!(
            "{};{}",
            here.spec("walk_entry:deps/a/b"),
            later.spec("walk_entry:deps/a/b/c")
        );
        let mut child = start(&sb, &src, &dest, mode, &spec);
        here.wait_reached(&mut child);
        send_signal(&child, libc::SIGINT);
        here.release();
        let out = finish_within(child, 30);
        assert_interrupted(&out, libc::SIGINT);
        assert!(
            !sb.path("barrier-later.reached").exists(),
            "the walk went on after the signal"
        );
        assert!(!dest.exists());
        assert_eq!(src.branch_tip("w"), None);
        before.assert_same(&src.snapshot());
        assert_no_staging(&[&sb.root]);
    }
}
