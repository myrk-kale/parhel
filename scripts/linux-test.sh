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
FROM rust:1-trixie
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
    useradd --create-home --uid 1000 tester
    # Mounts, ownership and the PARHEL_TEST_* / TMPDIR variables: shared with CI.
    mount_env=$(/src/scripts/mount-test-filesystems.sh tester)

    chown tester:tester /cache/cargo /cache/target
    export HOME=/home/tester
    exec runuser -u tester -- env \
      HOME=/home/tester \
      RUSTUP_HOME=/usr/local/rustup \
      RUSTUP_TOOLCHAIN=stable \
      CARGO_HOME=/cache/cargo \
      CARGO_TARGET_DIR=/cache/target \
      PATH=/usr/local/cargo/bin:$PATH \
      $mount_env \
      bash -euo pipefail -c "
        cd /src
        git --version
        cargo build --all-targets --features failpoints
        cargo test --features failpoints --no-fail-fast \"\$@\"
      " _ "$@"
  ' _ "$@"
