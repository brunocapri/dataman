#!/bin/bash
# The whole release: app → (notarized) → DMG → (notarized) → signed feed.
#
#   scripts/release.sh                                   # ad hoc app and DMG, no feed
#   scripts/release.sh https://example.com/dataman [notes.md]
#
# With a URL, the DMG is expected at <url>/Dataman-<version>.dmg and dist/feed.json is
# written for <url>/feed.json (upload both; the DMG first). Notarizes when
# DATAMAN_NOTARY_PROFILE is set; see bundle.sh for the other variables. DATAMAN_LINUX=1 also
# builds the Linux AppImages and .debs (scripts/linux/build.sh, in Docker) into the same feed.
set -euo pipefail
[ "$(uname)" = Darwin ] || { echo "release: macOS only (the Linux release: scripts/linux/build.sh)" >&2; exit 1; }

here="$(cd "$(dirname "$0")" && pwd)"
dist="${DATAMAN_DIST:-$here/../dist}"
base="${1:-}"
notes="${2:-}"

"$here/bundle.sh"
app="$dist/Dataman.app"
if [ -n "${DATAMAN_NOTARY_PROFILE:-}" ]; then
  "$here/notarize.sh" "$app"
fi
"$here/dmg.sh"
version="$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")"
dmg="$dist/Dataman-$version.dmg"
if [ -n "${DATAMAN_NOTARY_PROFILE:-}" ]; then
  "$here/notarize.sh" "$dmg"
fi
if [ -n "$base" ] && [ -n "${DATAMAN_LINUX:-}" ]; then
  # Every platform in one feed: the DMG stays the top-level download (what released Macs read).
  DATAMAN_VERSION="$version" "$here/linux/build.sh" all
  minos="$(plutil -extract LSMinimumSystemVersion raw -o - "$app/Contents/Info.plist")"
  macarch="$([ "$(uname -m)" = arm64 ] && echo aarch64 || echo x86_64)"
  args=(feed --version "$version" --min-macos "$minos" --asset "macos-$macarch" "$dmg" "${base%/}/Dataman-$version.dmg")
  for a in x86_64 aarch64; do
    f="$dist/linux/Dataman-$version-$a.AppImage"
    [ -f "$f" ] || { echo "release: no $f" >&2; exit 1; }
    args+=(--asset "linux-$a" "$f" "${base%/}/$(basename "$f")")
  done
  [ -n "$notes" ] && args+=(--notes "$notes")
  (cd "$here/.." && cargo run --release --quiet -p xtask -- "${args[@]}") > "$dist/feed.json"
  echo "$dist/feed.json → ${base%/}/feed.json (macOS and Linux)"
elif [ -n "$base" ]; then
  "$here/sign-update.sh" "$dmg" "${base%/}/Dataman-$version.dmg" "$notes" > /dev/null
  echo "$dist/feed.json → ${base%/}/feed.json"
fi
