#!/usr/bin/env bash
# Install a .deb or .rpm in a container and check it, as a normal user.
#
#   scripts/test-package.sh <image> <package file>
#   scripts/test-package.sh ubuntu:24.04 dist/parhel_0.3.0_amd64.deb
#   scripts/test-package.sh fedora:latest dist/parhel-0.3.0-1.x86_64.rpm
#
# The package manager resolves the dependencies (git) from the distribution's own
# repositories. The checks: the installed files, `git parhel --version` and
# scripts/smoke-test.sh, both run by a user without privileges. The image's
# architecture must match the package's (the container runs the static binary).
set -euo pipefail

image=${1:?usage: test-package.sh <image> <package file>}
package=$(realpath "${2:?usage: test-package.sh <image> <package file>}")
repo=$(realpath "$(dirname "$0")/..")

case "$package" in
  *.deb) kind=deb ;;
  *.rpm) kind=rpm ;;
  *) echo "not a .deb or .rpm: $package" >&2; exit 2 ;;
esac

# PARHEL_DOCKER_ARGS: extra `docker run` options (for example --dns on a host whose
# default resolver does not work in containers).
# shellcheck disable=SC2086
docker run --rm ${PARHEL_DOCKER_ARGS:-} \
  -v "$package:/pkg/$(basename "$package"):ro" \
  -v "$repo/scripts/smoke-test.sh:/pkg/smoke-test.sh:ro" \
  -e "KIND=$kind" -e "PACKAGE=/pkg/$(basename "$package")" \
  "$image" bash -euxo pipefail -c '
    if [ "$KIND" = deb ]; then
      export DEBIAN_FRONTEND=noninteractive
      # The Ubuntu image leaves out man pages and docs; install them, to check them.
      rm -f /etc/dpkg/dpkg.cfg.d/excludes
      apt-get update -qq
      apt-get install -y -qq "$PACKAGE"
      dpkg -s parhel | grep -E "^(Status|Version|Depends|Recommends):"
      dpkg -L parhel
      dpkg --verify parhel
    else
      # The Fedora image sets tsflags=nodocs, which skips man pages; install them, to check them.
      dnf install -y -q --setopt=tsflags= "$PACKAGE"
      rpm -qi parhel
      rpm -qR parhel
      rpm -ql parhel
      rpm --verify parhel
    fi

    test -x /usr/bin/git-parhel
    for page in git-parhel git-parhel-add git-parhel-doctor; do
      ls /usr/share/man/man1/"$page".1*
    done

    useradd --create-home tester
    runuser -u tester -- git parhel --version
    runuser -u tester -- bash /pkg/smoke-test.sh /usr/bin/git-parhel
  '
