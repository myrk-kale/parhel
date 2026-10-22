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

[ "$(uname -s)" = "Darwin" ] || die "parhel needs macOS (APFS). On other systems use 'git worktree add'."
command -v curl >/dev/null || die "curl is required"
command -v git >/dev/null || die "git is required"

if [ -n "${PARHEL_VERSION:-}" ]; then
  tag="v${PARHEL_VERSION#v}"
else
  tag="$(curl -fsSL "https://api.github.com/repos/${repo}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$tag" ] || die "could not determine the latest release"
fi

archive="parhel-${tag}-universal-apple-darwin.tar.gz"
base="https://github.com/${repo}/releases/download/${tag}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading ${tag}"
curl -fsSL -o "$tmp/$archive" "$base/$archive" || die "download failed: $base/$archive"
curl -fsSL -o "$tmp/$archive.sha256" "$base/$archive.sha256" || die "checksum download failed"
(cd "$tmp" && shasum -a 256 -c "$archive.sha256" >/dev/null) || die "checksum mismatch"

mkdir -p "$tmp/pkg" "$bin_dir" "$man_dir"
tar -xzf "$tmp/$archive" -C "$tmp/pkg"
install -m 0755 "$tmp/pkg/git-parhel" "$bin_dir/git-parhel"
for page in "$tmp"/pkg/man/*.1; do install -m 0644 "$page" "$man_dir/"; done

say "installed $("$bin_dir/git-parhel" --version) to $bin_dir"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) say "add $bin_dir to your PATH, e.g.: echo 'export PATH=\"$bin_dir:\$PATH\"' >> ~/.zshrc" ;;
esac
case ":${MANPATH:-}:" in
  *":${man_dir%/man1}:"*) ;;
  *) say "for 'git parhel --help', add: export MANPATH=\"${man_dir%/man1}:\$MANPATH\"" ;;
esac
say "try: git parhel add ../my-feature --new-branch my-feature"
