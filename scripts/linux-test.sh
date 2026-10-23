#!/usr/bin/env bash
# Run parhel's test suite on Linux, from macOS (or any Docker host).
#
# A privileged container loop-mounts a btrfs, an XFS (reflink=1) and an ext4
# image, puts TMPDIR on the btrfs one and exports their mount points as
# PARHEL_TEST_BTRFS / PARHEL_TEST_XFS / PARHEL_TEST_EXT4. Tests run as an
# unprivileged user (permission tests rely on not being root). Extra arguments
# go to `cargo test`:
#
#   scripts/linux-test.sh                       # whole suite
#   scripts/linux-test.sh --lib                 # unit tests only
#   scripts/linux-test.sh --test add_filesystems -- --nocapture
#
# Needs Docker and a kernel with the btrfs, xfs and loop modules. With colima:
#   colima ssh -- sudo modprobe btrfs xfs loop
#
# The cargo registry and the target directory live in named Docker volumes, so
# repeated runs are incremental. Reset them with:
#   docker volume rm parhel-linux-cargo parhel-linux-target
set -euo pipefail

cd "$(dirname "$0")/.."
repo=$PWD
image=parhel-linux-test

command -v docker >/dev/null || { echo "docker not found" >&2; exit 1; }

docker build -q -t "$image" - >/dev/null <<'DOCKERFILE'
FROM rust:1-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git btrfs-progs xfsprogs e2fsprogs python3 python3-venv util-linux \
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE

docker run --rm --privileged \
  -v "$repo":/src:ro \
  -v parhel-linux-cargo:/cache/cargo \
  -v parhel-linux-target:/cache/target \
  "$image" bash -euo pipefail -c '
    img=/var/lib/parhel-images
    mkdir -p "$img"

    # Loop device nodes made by the host after the container started are not
    # visible in its /dev: create a few so losetup works.
    [ -e /dev/loop-control ] || mknod /dev/loop-control c 10 237
    for i in $(seq 0 15); do [ -e /dev/loop$i ] || mknod /dev/loop$i b 7 $i; done

    useradd --create-home --uid 1000 tester
    mkdir -p /mnt/btrfs /mnt/xfs /mnt/ext4

    for fs in btrfs xfs ext4; do
      truncate -s 1G "$img/$fs.img"
      case $fs in
        btrfs) mkfs.btrfs -q -f "$img/$fs.img" ;;
        xfs)   mkfs.xfs -q -f -m reflink=1 "$img/$fs.img" ;;
        ext4)  mkfs.ext4 -q -F "$img/$fs.img" ;;
      esac
      mount -o loop "$img/$fs.img" "/mnt/$fs"
      mkdir -p "/mnt/$fs/tmp"
      chown -R tester:tester "/mnt/$fs"
    done
    for fs in btrfs xfs ext4; do findmnt -o TARGET,SOURCE,FSTYPE,OPTIONS "/mnt/$fs"; done

    chown tester:tester /cache/cargo /cache/target
    export HOME=/home/tester
    exec runuser -u tester -- env \
      HOME=/home/tester \
      RUSTUP_HOME=/usr/local/rustup \
      RUSTUP_TOOLCHAIN=stable \
      CARGO_HOME=/cache/cargo \
      CARGO_TARGET_DIR=/cache/target \
      PATH=/usr/local/cargo/bin:$PATH \
      TMPDIR=/mnt/btrfs/tmp \
      PARHEL_TEST_BTRFS=/mnt/btrfs/tmp \
      PARHEL_TEST_XFS=/mnt/xfs/tmp \
      PARHEL_TEST_EXT4=/mnt/ext4/tmp \
      PARHEL_REQUIRE_FS=1 \
      bash -euo pipefail -c "
        cd /src
        git --version
        cargo build --all-targets --features failpoints
        cargo test --features failpoints --no-fail-fast \"\$@\"
      " _ "$@"
  ' _ "$@"
