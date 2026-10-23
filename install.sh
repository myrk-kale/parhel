#!/bin/sh
# Install git-parhel from GitHub releases.
#
#   curl -fsSL https://raw.githubusercontent.com/myrk-kale/parhel/main/install.sh | sh
#
# Environment:
#   PARHEL_VERSION      version to install, e.g. 0.1.0 (default: latest release)
#   PARHEL_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   PARHEL_MAN_DIR      where to put man pages (default: ~/.local/share/man/man1)
set -eu

repo="myrk-kale/parhel"
bin_dir="${PARHEL_INSTALL_DIR:-$HOME/.local/bin}"
man_dir="${PARHEL_MAN_DIR:-$HOME/.local/share/man/man1}"

say() { printf 'parhel: %s\n' "$*"; }
die() { printf 'parhel: error: %s\n' "$*" >&2; exit 1; }

# The release archive for this system: a universal binary on macOS, a static (musl)
# binary on Linux.
case "$(uname -s)" in
  Darwin) target="universal-apple-darwin"; rc=".zshrc" ;;
  Linux)
    rc=".profile"
    case "$(uname -m)" in
      x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
      aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
      *) die "no prebuilt binary for Linux on $(uname -m) (x86_64 and aarch64 only); build with 'cargo install parhel'" ;;
    esac
    ;;
  *) die "parhel runs on macOS and Linux. On other systems use 'git worktree add'." ;;
esac
command -v curl >/dev/null || die "curl is required"
command -v git >/dev/null || die "git is required"
if command -v sha256sum >/dev/null; then
  verify_sha256() { sha256sum -c "$1" >/dev/null; }
elif command -v shasum >/dev/null; then
  verify_sha256() { shasum -a 256 -c "$1" >/dev/null; }
else
  die "sha256sum or shasum is required"
fi

if [ -n "${PARHEL_VERSION:-}" ]; then
  tag="v${PARHEL_VERSION#v}"
else
  tag="$(curl -fsSL "https://api.github.com/repos/${repo}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$tag" ] || die "could not determine the latest release"
fi

archive="parhel-${tag}-${target}.tar.gz"
base="https://github.com/${repo}/releases/download/${tag}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading ${tag}"
curl -fsSL -o "$tmp/$archive" "$base/$archive" || die "download failed: $base/$archive"
curl -fsSL -o "$tmp/$archive.sha256" "$base/$archive.sha256" || die "checksum download failed"
(cd "$tmp" && verify_sha256 "$archive.sha256") || die "checksum mismatch"

mkdir -p "$tmp/pkg" "$bin_dir" "$man_dir"
tar -xzf "$tmp/$archive" -C "$tmp/pkg"
install -m 0755 "$tmp/pkg/git-parhel" "$bin_dir/git-parhel"
for page in "$tmp"/pkg/man/*.1; do install -m 0644 "$page" "$man_dir/"; done

say "installed $("$bin_dir/git-parhel" --version) to $bin_dir"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) say "add $bin_dir to your PATH, e.g.: echo 'export PATH=\"$bin_dir:\$PATH\"' >> ~/$rc" ;;
esac
case ":${MANPATH:-}:" in
  *":${man_dir%/man1}:"*) ;;
  *) say "for 'git parhel --help', add: export MANPATH=\"${man_dir%/man1}:\$MANPATH\"" ;;
esac
if [ "$(uname -s)" = "Linux" ]; then
  say "copy-on-write needs Btrfs, XFS (reflink), bcachefs or ZFS 2.2+; elsewhere use --copy or --fallback"
fi
say "try: git parhel add ../my-feature --new-branch my-feature"
