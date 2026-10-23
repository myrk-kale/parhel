#!/usr/bin/env bash
# Create and mount the filesystems parhel's Linux tests need, and print the
# environment the test suite expects (one KEY=VALUE per line, on stdout):
#
#   PARHEL_TEST_BTRFS  a directory on btrfs
#   PARHEL_TEST_XFS    a directory on XFS formatted with reflink=1
#   PARHEL_TEST_EXT4   a directory on ext4 (no reflink)
#   PARHEL_TEST_BTRFS_SMALL  a directory on a small btrfs (a full store, I-S33)
#   PARHEL_REQUIRE_FS  1: a missing filesystem fails the tests instead of skipping
#   TMPDIR             the btrfs directory (the suite runs on a reflink filesystem)
#
# Used by scripts/linux-test.sh (inside a privileged container) and by the Linux CI
# job (on the runner, with sudo). Everything else goes to stderr.
#
#   sudo scripts/mount-test-filesystems.sh <owner> [image-dir]
#
# <owner> is the unprivileged user who will run the tests: the mounts are chowned to
# it, because the permission tests rely on not being root. Needs btrfs-progs,
# xfsprogs, e2fsprogs and the btrfs, xfs and loop kernel modules. Sparse images of
# $PARHEL_IMAGE_SIZE (default 1G) go to [image-dir] (default /var/lib/parhel-images),
# mounts to $PARHEL_MOUNT_ROOT/{btrfs,xfs,ext4} (default /mnt); a fourth btrfs of
# $PARHEL_SMALL_IMAGE_SIZE (default 128M) is mounted at $PARHEL_MOUNT_ROOT/btrfs-small.
set -euo pipefail

# stdout is the environment only: the tools' chatter (mkfs notes) goes to stderr.
exec 3>&1 1>&2

owner=${1:?usage: mount-test-filesystems.sh <owner> [image-dir]}
img=${2:-/var/lib/parhel-images}
root=${PARHEL_MOUNT_ROOT:-/mnt}
size=${PARHEL_IMAGE_SIZE:-1G}
small=${PARHEL_SMALL_IMAGE_SIZE:-128M}

[ "$(id -u)" = 0 ] || { echo "mount-test-filesystems.sh must run as root (sudo)" >&2; exit 1; }
id "$owner" >/dev/null

mkdir -p "$img"

# Loop device nodes made by the host after a container started are not visible in
# its /dev: create a few so losetup works. A normal host has them already.
if [ ! -e /dev/loop-control ]; then
  mknod /dev/loop-control c 10 237
  for i in $(seq 0 15); do [ -e "/dev/loop$i" ] || mknod "/dev/loop$i" b 7 "$i"; done
fi

for fs in btrfs xfs ext4; do
  mkdir -p "$root/$fs"
  if mountpoint -q "$root/$fs"; then umount "$root/$fs"; fi
  rm -f "$img/$fs.img"
  truncate -s "$size" "$img/$fs.img"
  case $fs in
    btrfs) mkfs.btrfs -q -f "$img/$fs.img" ;;
    xfs)   mkfs.xfs -q -f -m reflink=1 "$img/$fs.img" ;;
    ext4)  mkfs.ext4 -q -F "$img/$fs.img" ;;
  esac
  mount -o loop "$img/$fs.img" "$root/$fs"
  mkdir -p "$root/$fs/tmp"
  chown -R "$owner:$(id -gn "$owner")" "$root/$fs"
done
# A btrfs too small for the tests' data, to fill up: what a store does when it is full.
mkdir -p "$root/btrfs-small"
if mountpoint -q "$root/btrfs-small"; then umount "$root/btrfs-small"; fi
rm -f "$img/btrfs-small.img"
truncate -s "$small" "$img/btrfs-small.img"
mkfs.btrfs -q -f "$img/btrfs-small.img"
mount -o loop "$img/btrfs-small.img" "$root/btrfs-small"
mkdir -p "$root/btrfs-small/tmp"
chown -R "$owner:$(id -gn "$owner")" "$root/btrfs-small"
for fs in btrfs xfs ext4 btrfs-small; do
  findmnt -o TARGET,SOURCE,FSTYPE,OPTIONS "$root/$fs"
done

cat >&3 <<ENV
PARHEL_TEST_BTRFS=$root/btrfs/tmp
PARHEL_TEST_XFS=$root/xfs/tmp
PARHEL_TEST_EXT4=$root/ext4/tmp
PARHEL_TEST_BTRFS_SMALL=$root/btrfs-small/tmp
PARHEL_REQUIRE_FS=1
TMPDIR=$root/btrfs/tmp
ENV
