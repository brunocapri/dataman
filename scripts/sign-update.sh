#!/bin/bash
# Signs a DMG with the update key and writes the feed (stdout and dist/feed.json).
#
#   scripts/sign-update.sh dist/Dataman-0.2.0.dmg https://example.com/Dataman-0.2.0.dmg [notes.md]
#
#   DATAMAN_UPDATE_PRIVATE_KEY  the key's base64 (CI secrets), or
#   DATAMAN_UPDATE_KEY          the key file (default ~/.config/dataman-release/update_ed25519.key)
#   DATAMAN_UPDATE_PUBKEY       when set, the key must match it
#
# The version and the minimum macOS are read from the app inside the DMG.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dmg="${1:?usage: sign-update.sh <dmg> <url> [notes.md]}"
url="${2:?usage: sign-update.sh <dmg> <url> [notes.md]}"
notes="${3:-}"
dist="${DATAMAN_DIST:-$root/dist}"

mnt="$(mktemp -d)"
trap 'hdiutil detach -quiet -force "$mnt" 2>/dev/null || true; rmdir "$mnt" 2>/dev/null || true' EXIT
hdiutil attach -quiet -nobrowse -readonly -noautoopen -mountpoint "$mnt" "$dmg"
plist="$mnt/Dataman.app/Contents/Info.plist"
version="$(plutil -extract CFBundleShortVersionString raw -o - "$plist")"
minos="$(plutil -extract LSMinimumSystemVersion raw -o - "$plist")"
hdiutil detach -quiet "$mnt"

args=(sign "$dmg" --url "$url" --version "$version" --min-macos "$minos")
[ -n "$notes" ] && args+=(--notes "$notes")
mkdir -p "$dist"
(cd "$root" && cargo run --release --quiet -p xtask -- "${args[@]}") > "$dist/feed.json"
cat "$dist/feed.json"
