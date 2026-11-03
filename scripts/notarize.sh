#!/bin/bash
# Notarizes and staples the app or the DMG (Developer ID signatures only); a library (the DuckDB
# engine) is notarized only: it cannot be stapled, Gatekeeper looks its ticket up online.
#
#   scripts/notarize.sh dist/Dataman.app
#   scripts/notarize.sh dist/Dataman-0.2.0.dmg
#   scripts/notarize.sh target/duckdb/libdataman_duckdb.dylib
#
#   DATAMAN_NOTARY_PROFILE  a notarytool keychain profile, made once with
#                           xcrun notarytool store-credentials <profile> --apple-id … --team-id …
#
# Stapling changes the file: sign the update (scripts/sign-update.sh) after this, never before.
set -euo pipefail
[ "$(uname)" = Darwin ] || { echo "notarize: macOS only (the Linux release: scripts/linux/build.sh)" >&2; exit 1; }

target="${1:?usage: notarize.sh <app, dmg or dylib>}"
: "${DATAMAN_NOTARY_PROFILE:?set DATAMAN_NOTARY_PROFILE (xcrun notarytool store-credentials)}"

case "$target" in
  *.app)
    zip="$(mktemp -d)/$(basename "$target" .app).zip"
    trap 'rm -rf "$(dirname "$zip")"' EXIT
    ditto -c -k --keepParent "$target" "$zip"
    xcrun notarytool submit "$zip" --keychain-profile "$DATAMAN_NOTARY_PROFILE" --wait
    ;;
  *.dmg)
    xcrun notarytool submit "$target" --keychain-profile "$DATAMAN_NOTARY_PROFILE" --wait
    ;;
  *.dylib)
    zip="$(mktemp -d)/$(basename "$target").zip"
    trap 'rm -rf "$(dirname "$zip")"' EXIT
    ditto -c -k "$target" "$zip"
    out="$(xcrun notarytool submit "$zip" --keychain-profile "$DATAMAN_NOTARY_PROFILE" --wait)"
    echo "$out"
    grep -q 'status: Accepted' <<< "$out" || exit 1
    spctl --assess --type open --context context:primary-signature --verbose=2 "$target"
    exit 0
    ;;
  *)
    echo "notarize: expected a .app, a .dmg or a .dylib" >&2
    exit 1
    ;;
esac
xcrun stapler staple "$target"
xcrun stapler validate "$target"
case "$target" in
  *.app) spctl --assess --type execute --verbose=2 "$target" ;;
  *.dmg) spctl --assess --type open --context context:primary-signature --verbose=2 "$target" ;;
esac
