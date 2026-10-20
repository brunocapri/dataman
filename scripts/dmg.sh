#!/bin/bash
# dist/Dataman.app → dist/Dataman-<version>.dmg: the app and an /Applications link in a
# compressed (UDZO) image, signed with DATAMAN_SIGN_IDENTITY when it is set.
#
#   DATAMAN_DIST            the folder with the app (default dist)
#   DATAMAN_SIGN_IDENTITY   signs the image too (needed to notarize it)
set -euo pipefail
[ "$(uname)" = Darwin ] || { echo "dmg: macOS only (the Linux release: scripts/linux/build.sh)" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
dist="${DATAMAN_DIST:-$root/dist}"
app="$dist/Dataman.app"
[ -d "$app" ] || { echo "dmg: no $app (scripts/bundle.sh first)" >&2; exit 1; }
version="$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")"
out="$dist/Dataman-$version.dmg"

stage="$(mktemp -d "$dist/.dmg-stage.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
ditto "$app" "$stage/Dataman.app"
ln -s /Applications "$stage/Applications"
rm -f "$out"
hdiutil create -quiet -volname "Dataman" -srcfolder "$stage" -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov "$out"
if [ -n "${DATAMAN_SIGN_IDENTITY:-}" ] && [ "$DATAMAN_SIGN_IDENTITY" != "-" ]; then
  codesign --force --timestamp --sign "$DATAMAN_SIGN_IDENTITY" "$out"
fi
hdiutil verify -quiet "$out"
echo "$out"
echo "  $(du -h "$out" | cut -f1) · $(shasum -a 256 "$out" | cut -c1-16)…"
