#!/usr/bin/env bash
# Build the .deb and .rpm for one musl target from the binary that is already built.
#
#   scripts/build-packages.sh <target> <output dir>
#
#   cargo build --release --locked --target x86_64-unknown-linux-musl --bin git-parhel
#   scripts/build-packages.sh x86_64-unknown-linux-musl dist
#
# Writes parhel_<version>-1_<amd64|arm64>.deb and parhel-<version>-1.<x86_64|aarch64>.rpm
# to the output directory, each with a .sha256 file. The packages are described in
# Cargo.toml ([package.metadata.deb] and [package.metadata.generate-rpm]). Used by
# the release workflow and by CI (`build linux musl`).
set -euo pipefail

# Pinned: the tools run in the release job, so a new version is a reviewed change.
CARGO_DEB_VERSION=3.8.0
CARGO_GENERATE_RPM_VERSION=0.21.1

target=${1:?usage: build-packages.sh <musl target> <output dir>}
out=${2:?usage: build-packages.sh <musl target> <output dir>}

case "$target" in
  x86_64-unknown-linux-musl) deb_arch=amd64 rpm_arch=x86_64 ;;
  aarch64-unknown-linux-musl) deb_arch=arm64 rpm_arch=aarch64 ;;
  *) echo "unsupported target: $target" >&2; exit 2 ;;
esac

cd "$(dirname "$0")/.."
bin="target/$target/release/git-parhel"
test -x "$bin" || { echo "build $bin first" >&2; exit 1; }

version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')

# Install a pinned tool unless exactly that version is already there.
ensure_tool() { # <subcommand> <crate> <version>
  if ! cargo "$1" --version 2>/dev/null | grep -qF "$3"; then
    cargo install --locked "$2" --version "=$3"
  fi
}
ensure_tool deb cargo-deb "$CARGO_DEB_VERSION"
ensure_tool generate-rpm cargo-generate-rpm "$CARGO_GENERATE_RPM_VERSION"

mkdir -p "$out"
out=$(realpath "$out")
deb="parhel_${version}-1_${deb_arch}.deb"
rpm="parhel-${version}-1.${rpm_arch}.rpm"

# A Debian package that is not native carries a changelog in Debian's format.
# The project's history is CHANGELOG.md (shipped as changelog.gz); this one
# entry points to it.
mkdir -p target/debian
cat > target/debian/changelog <<CHANGELOG
parhel (${version}-1) unstable; urgency=medium

  * Release ${version}. See /usr/share/doc/parhel/changelog.gz for the changes.

 -- Kale <parhel-release@users.noreply.github.com>  $(LC_ALL=C date -R)
CHANGELOG

# The binary is stripped by the release profile already, and the build is not
# repeated.
cargo deb --no-build --no-strip --target "$target" --deb-revision 1 --output "$out/$deb"
# gzip, not the default zstd: every rpm that can read a package at all can read it.
cargo generate-rpm --target "$target" --auto-req disabled --payload-compress gzip \
  --output "$out/$rpm"

cd "$out"
for package in "$deb" "$rpm"; do
  sha256sum "$package" > "$package.sha256"
  echo "built $package"
done
