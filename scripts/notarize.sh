#!/bin/bash
# Notarizes and staples the app or the DMG (Developer ID signatures only).
#
#   scripts/notarize.sh dist/Diskman.app
#   scripts/notarize.sh dist/Diskman-0.2.0.dmg
#
#   DISKMAN_NOTARY_PROFILE  a notarytool keychain profile, made once with
#                           xcrun notarytool store-credentials <profile> --apple-id … --team-id …
#
# Stapling changes the file: sign the update (scripts/sign-update.sh) after this, never before.
set -euo pipefail

target="${1:?usage: notarize.sh <app or dmg>}"
: "${DISKMAN_NOTARY_PROFILE:?set DISKMAN_NOTARY_PROFILE (xcrun notarytool store-credentials)}"

case "$target" in
  *.app)
    zip="$(mktemp -d)/$(basename "$target" .app).zip"
    trap 'rm -rf "$(dirname "$zip")"' EXIT
    ditto -c -k --keepParent "$target" "$zip"
    xcrun notarytool submit "$zip" --keychain-profile "$DISKMAN_NOTARY_PROFILE" --wait
    ;;
  *.dmg)
    xcrun notarytool submit "$target" --keychain-profile "$DISKMAN_NOTARY_PROFILE" --wait
    ;;
  *)
    echo "notarize: expected a .app or a .dmg" >&2
    exit 1
    ;;
esac
xcrun stapler staple "$target"
xcrun stapler validate "$target"
case "$target" in
  *.app) spctl --assess --type execute --verbose=2 "$target" ;;
  *.dmg) spctl --assess --type open --context context:primary-signature --verbose=2 "$target" ;;
esac
