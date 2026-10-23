#!/bin/sh
# Fill packaging/parhel.rb.in and print the formula.
#
#   packaging/render-formula.sh VERSION SHA256_MACOS SHA256_LINUX_X86_64 SHA256_LINUX_AARCH64
#
# Used by the Homebrew workflow (real checksums) and by the formula audit in CI
# (placeholder checksums). Fails if a placeholder is left unfilled.
set -eu

[ "$#" -eq 4 ] || { echo "usage: $0 VERSION SHA256_MACOS SHA256_LINUX_X86_64 SHA256_LINUX_AARCH64" >&2; exit 2; }
# The version goes into `sed` and into URLs: digits first, then only what a semver
# (with a pre-release or build suffix) is made of. No `v`, `/`, `&`, spaces or newlines.
case "$1" in
  [0-9]*) ;;
  *) echo "not a version: $1" >&2; exit 1 ;;
esac
case "$1" in
  *[!0-9A-Za-z.+-]*) echo "not a version: $1" >&2; exit 1 ;;
esac
for sha in "$2" "$3" "$4"; do
  case "$sha" in
    *[!0-9a-f]* | "") echo "not a SHA-256: $sha" >&2; exit 1 ;;
  esac
  [ "${#sha}" -eq 64 ] || { echo "not a SHA-256: $sha" >&2; exit 1; }
done

here=$(cd "$(dirname "$0")" && pwd)
sed -e "s/@VERSION@/$1/g" \
    -e "s/@SHA256_MACOS@/$2/g" \
    -e "s/@SHA256_LINUX_X86_64@/$3/g" \
    -e "s/@SHA256_LINUX_AARCH64@/$4/g" \
    "$here/parhel.rb.in" > "${TMPDIR:-/tmp}/parhel.rb.$$"
if grep -q '@[A-Z0-9_]*@' "${TMPDIR:-/tmp}/parhel.rb.$$"; then
  echo "unfilled placeholder in the formula" >&2
  rm -f "${TMPDIR:-/tmp}/parhel.rb.$$"
  exit 1
fi
cat "${TMPDIR:-/tmp}/parhel.rb.$$"
rm -f "${TMPDIR:-/tmp}/parhel.rb.$$"
