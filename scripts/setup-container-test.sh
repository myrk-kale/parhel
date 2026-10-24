#!/usr/bin/env bash
# Run tests/setup.rs - the tests of `git parhel setup`, which need root, a loop-mounted
# btrfs and systemd - in a privileged container, from macOS (or any Docker host).
#
# CI runs them for real in the job `setup (Linux)`. A container has no systemd as PID 1, so
# here `systemctl` is scripts/fake-systemctl.sh: it understands what `setup` and the tests
# use (enable, disable, start, stop, is-active, is-enabled, cat, daemon-reload) and really
# mounts and unmounts the unit's image with `mount` / `umount`, which is what matters: an
# open file on the store makes `umount` fail here as it does under systemd. It is installed
# in /usr/bin, where root looks for programs (root ignores PATH: README, "Security").
#
# The tests run as an unprivileged user with passwordless sudo, like on the CI runner. Extra
# arguments go to `cargo test`:
#
#   scripts/setup-container-test.sh                       # all of tests/setup.rs
#   scripts/setup-container-test.sh -- iset23 iset24      # some of them
#
# Needs Docker and a kernel with the btrfs, xfs and loop modules (see scripts/linux-test.sh).
# The cargo registry and the target directory are the named volumes scripts/linux-test.sh
# uses, so a run after it is incremental.
set -euo pipefail

cd "$(dirname "$0")/.."
repo=$PWD
image=parhel-setup-container

command -v docker >/dev/null || { echo "docker not found" >&2; exit 1; }

docker build -q -t "$image" - >/dev/null <<'DOCKERFILE'
FROM rust:1-trixie
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git btrfs-progs xfsprogs e2fsprogs python3 python3-venv util-linux sudo systemd \
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE

docker run --rm --privileged \
  -v "$repo":/src:ro \
  -v parhel-linux-cargo:/cache/cargo \
  -v parhel-linux-target:/cache/target \
  "$image" bash -euo pipefail -c '
    useradd --create-home --uid 1000 tester
    echo "tester ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/tester
    chmod 0440 /etc/sudoers.d/tester

    # What `sd_booted()` looks for, and the stand-in for systemctl.
    mkdir -p /run/systemd/system /run/fake-systemd/enabled
    install -m 0755 /src/scripts/fake-systemctl.sh /usr/bin/systemctl

    # Mounts, ownership and the PARHEL_TEST_* / TMPDIR variables: shared with CI.
    mount_env=$(/src/scripts/mount-test-filesystems.sh tester)

    chown tester:tester /cache/cargo /cache/target
    exec runuser -u tester -- env \
      HOME=/home/tester \
      RUSTUP_HOME=/usr/local/rustup \
      RUSTUP_TOOLCHAIN=stable \
      CARGO_HOME=/cache/cargo \
      CARGO_TARGET_DIR=/cache/target \
      PATH=/usr/local/cargo/bin:$PATH \
      PARHEL_TEST_SETUP=1 \
      $mount_env \
      bash -euo pipefail -c "
        cd /src
        cargo build --all-targets --features failpoints
        cargo test --features failpoints --test setup --no-fail-fast -- --test-threads=1 \"\$@\"
      " _ "$@"
  ' _ "$@"
