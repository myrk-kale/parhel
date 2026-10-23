#!/usr/bin/env bash
# Smoke test of a built git-parhel binary on Linux: it runs, and a real `add` works.
#
#   scripts/smoke-test.sh path/to/git-parhel
#
# Used by the release workflow (on the archive's binary), by CI (`build linux musl`)
# and for local container runs. The scratch repository lives under $TMPDIR (default
# /tmp). Without a flag, `add` has to refuse with `not-cow` on a filesystem that
# cannot reflink (ext4 on the CI runners, overlayfs in a container) and has to
# succeed on one that can (btrfs, XFS with reflink, bcachefs, ZFS); `--copy` has to
# succeed on both.
set -euo pipefail

bin=$(realpath "${1:?usage: smoke-test.sh <git-parhel binary>}")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

"$bin" --version

git init -q -b main "$tmp/src"
git -C "$tmp/src" config user.name smoke
git -C "$tmp/src" config user.email smoke@example.com
printf 'target/\n' > "$tmp/src/.gitignore"
git -C "$tmp/src" add .gitignore
git -C "$tmp/src" commit -q -m init
mkdir "$tmp/src/target"
printf 'built' > "$tmp/src/target/artifact"

fstype=$(stat -f -c %T "$tmp")
case "$fstype" in
  btrfs | xfs | bcachefs | zfs) reflink=yes ;;
  *) reflink=no ;;
esac
echo "smoke test on $fstype (reflink expected: $reflink)"

if [ "$reflink" = no ]; then
  # No copy-on-write here: parhel must say so and leave nothing behind.
  if (cd "$tmp/src" && "$bin" add ../cow --new-branch smoke-cow) 2> "$tmp/err"; then
    echo "add succeeded on $fstype, which cannot reflink" >&2
    exit 1
  fi
  cat "$tmp/err"
  grep -q 'not-cow' "$tmp/err" || { echo "expected the not-cow error" >&2; exit 1; }
  test ! -e "$tmp/cow"
  test -z "$(git -C "$tmp/src" branch --list smoke-cow)"
else
  (cd "$tmp/src" && "$bin" add ../cow --new-branch smoke-cow)
  test "$(cat "$tmp/cow/target/artifact")" = built
  test -z "$(git -C "$tmp/cow" status --porcelain)"
fi

(cd "$tmp/src" && "$bin" add ../copy --copy --new-branch smoke-copy)
test "$(cat "$tmp/copy/target/artifact")" = built
test -z "$(git -C "$tmp/copy" status --porcelain)"
echo "smoke test passed"
