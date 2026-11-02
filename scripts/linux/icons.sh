#!/bin/bash
# The app icon at 128, 256 and 512 → <root>/usr/share/icons/hicolor/<n>x<n>/apps/dev.dataman.Dataman.png.
# Called by appimage.sh and deb.sh.
#
#   scripts/linux/icons.sh <root>
#
# The PNGs are the ones the committed packaging/AppIcon.icns holds (ic07, ic08, ic09): each size
# rendered natively by scripts/icon.sh, never scaled down here.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
dest="${1:?usage: icons.sh <root>}"
icns="$root/packaging/AppIcon.icns"
die() { echo "icons: $*" >&2; exit 1; }

# An .icns is "icns" and its length, then entries: a 4-byte type, a 4-byte big-endian length
# (header included), the data.
at() { { tail -c +$(($2 + 1)) "$icns" || true; } | head -c "$1"; } # n off (tail stops on head's SIGPIPE)
size=$(wc -c < "$icns")
png() { # type → stdout
  local off=8 type n
  while [ "$off" -lt "$size" ]; do
    type="$(at 4 "$off")"
    n=$(at 4 $((off + 4)) | od -An -tu1 | awk '{print $1 * 16777216 + $2 * 65536 + $3 * 256 + $4}')
    [ "$n" -ge 8 ] || die "$icns: bad entry at $off"
    if [ "$type" = "$1" ]; then at $((n - 8)) $((off + 8)); return; fi
    off=$((off + n))
  done
  die "$icns has no $1"
}

for e in ic07:128 ic08:256 ic09:512; do
  n="${e#*:}"
  out="$dest/usr/share/icons/hicolor/${n}x${n}/apps/dev.dataman.Dataman.png"
  mkdir -p "$(dirname "$out")"
  png "${e%:*}" > "$out"
  [ "$(head -c 4 "$out" | od -An -c | tr -d ' ')" = "211PNG" ] || die "$icns: ${e%:*} is not a PNG"
done
