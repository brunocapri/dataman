#!/bin/bash
# Notarizes and staples the app or the DMG (Developer ID signatures only).
#
#   scripts/notarize.sh dist/Dataman.app
#   scripts/notarize.sh dist/Dataman-0.2.0.dmg
#
#   DATAMAN_NOTARY_PROFILE  a notarytool keychain profile, made once with
#                           xcrun notarytool store-credentials <profile> --apple-id … --team-id …
#   or, as in CI, an App Store Connect API key:
#   DATAMAN_NOTARY_KEY      the AuthKey_<id>.p8 file
#   DATAMAN_NOTARY_KEY_ID   its key ID
#   DATAMAN_NOTARY_ISSUER   the issuer ID (App Store Connect → Users and Access → Integrations)
#
# Stapling changes the file: sign the update (scripts/sign-update.sh) after this, never before.
set -euo pipefail
[ "$(uname)" = Darwin ] || { echo "notarize: macOS only (the Linux release: scripts/linux/build.sh)" >&2; exit 1; }

target="${1:?usage: notarize.sh <app or dmg>}"
if [ -n "${DATAMAN_NOTARY_KEY:-}" ]; then
  auth=(--key "$DATAMAN_NOTARY_KEY" --key-id "${DATAMAN_NOTARY_KEY_ID:?set DATAMAN_NOTARY_KEY_ID}" --issuer "${DATAMAN_NOTARY_ISSUER:?set DATAMAN_NOTARY_ISSUER}")
else
  : "${DATAMAN_NOTARY_PROFILE:?set DATAMAN_NOTARY_PROFILE (xcrun notarytool store-credentials) or DATAMAN_NOTARY_KEY}"
  auth=(--keychain-profile "$DATAMAN_NOTARY_PROFILE")
fi

case "$target" in
  *.app)
    zip="$(mktemp -d)/$(basename "$target" .app).zip"
    trap 'rm -rf "$(dirname "$zip")"' EXIT
    ditto -c -k --keepParent "$target" "$zip"
    xcrun notarytool submit "$zip" "${auth[@]}" --wait
    ;;
  *.dmg)
    xcrun notarytool submit "$target" "${auth[@]}" --wait
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
