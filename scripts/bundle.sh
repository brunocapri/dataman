#!/bin/bash
# Builds release and assembles dist/Diskman.app, signed (ad hoc unless told otherwise).
#
#   DISKMAN_BUNDLE_ID       bundle identifier (default dev.diskman)
#   DISKMAN_VERSION         version (default: the workspace's, from Cargo.toml); baked into the app
#   DISKMAN_BUILD           CFBundleVersion (default: the version)
#   DISKMAN_SIGN_IDENTITY   e.g. "Developer ID Application: Name (TEAMID)"; unset: ad hoc ("-")
#   DISKMAN_UPDATE_FEED     update feed URL baked into the app (unset: the latest GitHub release)
#   DISKMAN_UPDATE_PUBKEY   the ed25519 public key (base64) that signs updates, baked in (unset:
#                           the release key, DEFAULT_PUBKEY in crates/diskman/src/updates.rs)
#   DISKMAN_DIST            output folder (default dist)
#   DISKMAN_SKIP_BUILD=1    package target/release as it is
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
dist="${DISKMAN_DIST:-$root/dist}"
bundle_id="${DISKMAN_BUNDLE_ID:-dev.diskman}"
version="${DISKMAN_VERSION:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
build="${DISKMAN_BUILD:-$version}"
identity="${DISKMAN_SIGN_IDENTITY:--}"

if [ -z "${DISKMAN_SKIP_BUILD:-}" ]; then
  cargo build --release -p diskman -p diskman-mcp
fi

app="$dist/Diskman.app"
mkdir -p "$dist"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/diskman "$app/Contents/MacOS/diskman"
cp target/release/diskman-mcp "$app/Contents/MacOS/diskman-mcp"
cp packaging/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"
# The Dock icon while the app is light (`dock_icon`).
cp packaging/AppIconLight.icns "$app/Contents/Resources/AppIconLight.icns"
cp packaging/Info.plist "$app/Contents/Info.plist"

# The binary must say the same version the bundle does (the updater compares them).
baked="$("$app/Contents/MacOS/diskman" --version 2>/dev/null || true)"
if [ "$baked" != "$version" ]; then
  echo "bundle: the binary says '$baked', the bundle would say '$version' (rebuild with DISKMAN_VERSION=$version)" >&2
  exit 1
fi

minos="$(otool -l "$app/Contents/MacOS/diskman" | awk '/LC_BUILD_VERSION/ {f = 1} f && $1 == "minos" {print $2; exit}')"
plist="$app/Contents/Info.plist"
plutil -replace CFBundleIdentifier -string "$bundle_id" "$plist"
plutil -replace CFBundleShortVersionString -string "$version" "$plist"
plutil -replace CFBundleVersion -string "$build" "$plist"
plutil -replace LSMinimumSystemVersion -string "${minos:-11.0}" "$plist"
plutil -lint -s "$plist"

# Inside out: the helper, then the bundle (which signs the main executable). The helper shares
# the app's identifier, so with a Developer ID both satisfy the same designated requirement and
# the passwords the app saved open in the MCP server without a Keychain prompt.
xattr -cr "$app"
if [ "$identity" = "-" ]; then ts=(--timestamp=none); else ts=(--timestamp); fi
sign=(codesign --force --options runtime "${ts[@]}" --entitlements packaging/entitlements.plist --sign "$identity")
"${sign[@]}" --identifier "$bundle_id" "$app/Contents/MacOS/diskman-mcp"
"${sign[@]}" "$app"
codesign --verify --deep --strict "$app"

echo "$app"
echo "  $version ($build) · $bundle_id · macOS ${minos:-11.0}+ · $(du -sh "$app" | cut -f1) · signed: $( [ "$identity" = "-" ] && echo "ad hoc" || echo "$identity")"
echo "  updates: ${DISKMAN_UPDATE_FEED:-GitHub Releases (brunocapri/diskman)}"
