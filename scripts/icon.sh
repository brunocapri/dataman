#!/bin/bash
# packaging/icon/app-icon.html → packaging/AppIcon.icns and packaging/AppIcon.png (the 1024),
# and its day variant (white squircle, the Dock icon while the app is light) →
# packaging/AppIconLight.icns and packaging/AppIconLight.png.
# Every size is rendered natively by a headless Chromium browser (never scaled down from the
# 1024), so small sizes get the reduced grain and shake the source computes for them.
#
#   DISKMAN_CHROME      the browser binary (default: Google Chrome, Chromium, Brave or Edge)
#   DISKMAN_ICON_WORK   keep the PNGs and the iconset in this folder (default: a temp dir, removed)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
html="$root/packaging/icon/app-icon.html"

chrome="${DISKMAN_CHROME:-}"
if [ -z "$chrome" ]; then
  for c in \
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    "/Applications/Chromium.app/Contents/MacOS/Chromium" \
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser" \
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"; do
    if [ -x "$c" ]; then chrome="$c"; break; fi
  done
fi
[ -n "$chrome" ] && [ -x "$chrome" ] || { echo "icon: no Chromium-based browser (set DISKMAN_CHROME)" >&2; exit 1; }

if [ -n "${DISKMAN_ICON_WORK:-}" ]; then
  work="$DISKMAN_ICON_WORK"
  mkdir -p "$work"
else
  work="$(mktemp -d "${TMPDIR:-/tmp}/diskman-icon.XXXXXX")"
  trap 'rm -rf "$work"' EXIT
fi
profile="$work/profile"

# Headless Chrome stalls on very small windows, so a size under 256 renders in a 256 window
# (?pad= puts the icon in its middle) and the middle is cropped out: a crop, not a scale.
render() {
  local n="$1" out="$2" variant="$3" w shot="$work/shot.png"
  w=$(( n < 256 ? 256 : n ))
  rm -f "$shot"
  perl -e 'alarm 60; exec @ARGV' "$chrome" --headless=new --disable-gpu --user-data-dir="$profile" \
    --force-device-scale-factor=1 --hide-scrollbars --default-background-color=00000000 \
    --window-size="$w,$w" --virtual-time-budget=2000 --screenshot="$shot" \
    "file://$html?size=$n&pad=$(( (w - n) / 2 ))&variant=$variant" > /dev/null 2>&1
  [ -s "$shot" ] || { echo "icon: no screenshot at $n px" >&2; exit 1; }
  if [ "$w" -ne "$n" ]; then
    sips -c "$n" "$n" "$shot" --out "$out" > /dev/null
  else
    mv "$shot" "$out"
  fi
}

# One variant (`night` or `day`) → packaging/<name>.icns and packaging/<name>.png.
build() {
  local variant="$1" name="$2" set="$work/$2.iconset"
  rm -rf "$set" "$profile"
  mkdir -p "$set"
  for n in 16 32 64 128 256 512 1024; do
    render "$n" "$work/$variant-$n.png" "$variant"
  done
  rm -rf "$profile" "$work/shot.png"
  for n in 16 32 128 256 512; do
    cp "$work/$variant-$n.png" "$set/icon_${n}x${n}.png"
    cp "$work/$variant-$(( n * 2 )).png" "$set/icon_${n}x${n}@2x.png"
  done
  iconutil -c icns "$set" -o "$root/packaging/$name.icns"
  cp "$work/$variant-1024.png" "$root/packaging/$name.png"
  echo "$root/packaging/$name.icns"
  echo "  $(du -h "$root/packaging/$name.icns" | cut -f1) · rendered with $(basename "$chrome")"
}

build night AppIcon
build day AppIconLight
