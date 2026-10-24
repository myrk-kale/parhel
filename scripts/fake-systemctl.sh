#!/bin/bash
# Stand-in for systemctl in a container without systemd (scripts/setup-container-test.sh
# installs it as /usr/bin/systemctl). Mounts and unmounts the unit's image for real.
echo "systemctl $*" >> /tmp/systemctl.log
unitdir=/etc/systemd/system
state=/run/fake-systemd
cmd=${1:-}; shift || true
now=0; units=()
for a in "$@"; do case "$a" in --now) now=1;; --*) ;; *) units+=("$a");; esac; done
u=${units[0]:-}
f=$unitdir/$u
field() { sed -n "s/^$1=//p" "$f" | head -1; }
case $cmd in
  daemon-reload) exit 0 ;;
  --version) echo "systemd 257 (fake)"; exit 0 ;;
  list-units) exit 0 ;;
  is-enabled) [ -e "$state/enabled/$u" ] && { echo enabled; exit 0; } || { echo disabled; exit 1; } ;;
  is-active) if [ -f "$f" ] && mountpoint -q "$(field Where)"; then echo active; exit 0; else echo inactive; exit 3; fi ;;
  cat) [ -f "$f" ] && { echo "# $f"; cat "$f"; exit 0; } || { echo "No files found for $u." >&2; exit 1; } ;;
  enable|start)
    [ -f "$f" ] || { echo "Unit $u does not exist" >&2; exit 5; }
    [ "$cmd" = enable ] && touch "$state/enabled/$u"
    if [ "$cmd" = start ] || [ $now = 1 ]; then
      where=$(field Where); what=$(field What); opts=$(field Options)
      mountpoint -q "$where" && exit 0
      mount -t "$(field Type)" -o "$opts" "$what" "$where" || exit 1
    fi ;;
  disable|stop)
    if [ ! -f "$f" ]; then echo "Unit $u does not exist" >&2; exit 5; fi
    [ "$cmd" = disable ] && rm -f "$state/enabled/$u"
    if [ "$cmd" = stop ] || [ $now = 1 ]; then
      where=$(field Where)
      if mountpoint -q "$where"; then umount "$where" || exit 1; fi
    fi ;;
  *) exit 0 ;;
esac
