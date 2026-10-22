//! I-54: proof that the new worktree shares storage with the source.

mod common;

use std::fs;

use common::cow::{free_bytes, shares_storage, write_blob};
use common::volume::{DiskImage, ImageFs};
use common::*;
use serial_test::serial;

const MIB: usize = 1 << 20;

// I-54a: tracked and ignored files are clones (same physical extents).
#[test]
fn i54a_files_share_physical_storage() {
    let sb = Sandbox::new();
    let src = project(&sb);
    write_blob(&src.path("assets/tracked.bin"), 2 * MIB, 1);
    src.commit_all("tracked blob");
    write_blob(&src.path("target/ignored.bin"), 4 * MIB, 2);
    let dest = sb.path("cow");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "c"]),
    ));

    for rel in ["assets/tracked.bin", "target/ignored.bin"] {
        assert!(
            shares_storage(&src.path(rel), &dest.join(rel)),
            "{rel} was copied, not cloned"
        );
    }
}

// I-54b: writes diverge (copy-on-write, not hard links).
#[test]
fn i54b_writes_in_destination_do_not_reach_source() {
    let sb = Sandbox::new();
    let src = project(&sb);
    write_blob(&src.path("target/ignored.bin"), MIB, 3);
    let original = fs::read(src.path("target/ignored.bin")).unwrap();
    let dest = sb.path("cow");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "c"]),
    ));
    fs::write(dest.join("target/ignored.bin"), b"rewritten").unwrap();
    fs::write(dest.join("README.md"), b"rewritten").unwrap();
    assert_eq!(fs::read(src.path("target/ignored.bin")).unwrap(), original);
    assert_eq!(src.read("README.md"), "# test repo\n");
}

// I-54c: free space barely moves when cloning a large tree (isolated volume).
#[test]
#[serial(hdiutil)]
fn i54c_free_space_barely_changes() {
    let sb = Sandbox::new();
    let img = DiskImage::new(ImageFs::Apfs, 512);
    let src = TestRepo::init_at(&sb.env, &img.mount.join("src"));
    src.ignore(&["target/"]);
    for i in 0..8 {
        write_blob(&src.path(format!("target/blob{i}.bin")), 16 * MIB, 10 + i);
    }
    let before = free_bytes(&img.mount);
    let dest = img.mount.join("dest");
    assert_success(&parhel(
        &sb.env,
        &src.root,
        &add_args(&dest, &["--new-branch", "c"]),
    ));
    let used = before.saturating_sub(free_bytes(&img.mount));
    assert!(
        used < (8 * MIB) as u64,
        "cloning 128 MiB consumed {} MiB",
        used / MIB as u64
    );
    assert!(dest.join("target/blob7.bin").is_file());
}
