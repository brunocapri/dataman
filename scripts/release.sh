#!/bin/bash
# The whole release: app → (notarized) → DMG → (notarized) → signed feed.
#
#   scripts/release.sh                                   # ad hoc app and DMG, no feed
#   scripts/release.sh https://example.com/dataman [notes.md]
#
# With a URL, the DMG is expected at <url>/Dataman-<version>.dmg and dist/feed.json is
# written for <url>/feed.json (upload both; the DMG first). Notarizes when
# DATAMAN_NOTARY_PROFILE is set; see bundle.sh for the other variables.
set -euo pipefail

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
if [ -n "$base" ]; then
  "$here/sign-update.sh" "$dmg" "${base%/}/Dataman-$version.dmg" "$notes" > /dev/null
  echo "$dist/feed.json → ${base%/}/feed.json"
fi
