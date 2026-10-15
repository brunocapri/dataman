#!/bin/bash
# dist/Diskman.app → dist/Diskman-<version>.dmg: the app and an /Applications link in a
# compressed (UDZO) image, signed with DISKMAN_SIGN_IDENTITY when it is set.
#
#   DISKMAN_DIST            the folder with the app (default dist)
#   DISKMAN_SIGN_IDENTITY   signs the image too (needed to notarize it)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dist="${DISKMAN_DIST:-$root/dist}"
app="$dist/Diskman.app"
[ -d "$app" ] || { echo "dmg: no $app (scripts/bundle.sh first)" >&2; exit 1; }
version="$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")"
out="$dist/Diskman-$version.dmg"

stage="$(mktemp -d "$dist/.dmg-stage.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
ditto "$app" "$stage/Diskman.app"
ln -s /Applications "$stage/Applications"
rm -f "$out"
hdiutil create -quiet -volname "Diskman" -srcfolder "$stage" -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov "$out"
if [ -n "${DISKMAN_SIGN_IDENTITY:-}" ] && [ "$DISKMAN_SIGN_IDENTITY" != "-" ]; then
  codesign --force --timestamp --sign "$DISKMAN_SIGN_IDENTITY" "$out"
fi
hdiutil verify -quiet "$out"
echo "$out"
echo "  $(du -h "$out" | cut -f1) · $(shasum -a 256 "$out" | cut -c1-16)…"
