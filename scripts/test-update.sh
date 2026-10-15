#!/bin/bash
# Self-update end to end with the real app: Diskman 0.1.0, installed in a scratch folder,
# finds 0.1.1 in a feed served from 127.0.0.1, downloads it, verifies it, swaps itself and
# relaunches; first, a tampered DMG is refused and the app stays as it is. Never touches
# /Applications, the user's config, Keychain or logs (XDG_CONFIG_HOME and DISKMAN_LOG_DIR point
# into the scratch folder; the signing key is a throwaway one). Needs a desktop session.
#
#   scripts/test-update.sh [scratch-dir]
#
# Leaves target/release built with the test feed and key: rebuild afterwards.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="${1:-$(mktemp -d)}"
mkdir -p "$work"
work="$(cd "$work" && pwd)"
www="$work/www"
apps="$work/Applications"
app="$apps/Diskman.app"
mkdir -p "$www" "$apps" "$work/config/diskman" "$work/logs"
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; exit 1; }

export DISKMAN_UPDATE_KEY="$work/update.key"
rm -f "$DISKMAN_UPDATE_KEY"
pub="$("$root/scripts/keygen.sh" 2>/dev/null)"
port="$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
base="http://127.0.0.1:$port"
export DISKMAN_UPDATE_FEED="$base/feed.json" DISKMAN_UPDATE_PUBKEY="$pub"

# 0.1.1: the update, its signed feed, and a tampered copy under a feed signed for the real one.
DISKMAN_VERSION=0.1.1 DISKMAN_DIST="$work/v2" "$root/scripts/bundle.sh" > /dev/null
DISKMAN_DIST="$work/v2" "$root/scripts/dmg.sh" > /dev/null
dmg="$work/v2/Diskman-0.1.1.dmg"
cp "$dmg" "$www/Diskman-0.1.1.dmg"
cp "$dmg" "$www/tampered.dmg"
size="$(stat -f %z "$dmg")"
dd if=/dev/urandom of="$www/tampered.dmg" bs=1 count=16 seek=$((size / 2)) conv=notrunc 2> /dev/null
DISKMAN_DIST="$work/v2" "$root/scripts/sign-update.sh" "$dmg" "$base/Diskman-0.1.1.dmg" > "$www/feed.json"
DISKMAN_DIST="$work/v2" "$root/scripts/sign-update.sh" "$dmg" "$base/tampered.dmg" > "$www/tampered.json"
pass "0.1.1 bundled, DMG $(du -h "$dmg" | cut -f1), feed signed"

# 0.1.0: the installed app, with the same feed and key.
DISKMAN_DIST="$work/v1" "$root/scripts/bundle.sh" > /dev/null
rm -rf "$app"
ditto "$work/v1/Diskman.app" "$app"
[ "$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")" = 0.1.0 ] || fail "0.1.0 not installed"
pass "0.1.0 installed in $apps"

(cd "$www" && exec python3 -m http.server "$port" --bind 127.0.0.1 > "$work/http.log" 2>&1) &
server=$!
trap 'kill $server 2>/dev/null || true' EXIT
sleep 1

run_app() {
  XDG_CONFIG_HOME="$work/config" DISKMAN_LOG_DIR="$work/logs" DISKMAN_LOG=1 DISKMAN_UPDATE_AUTORESTART=1 \
    "$app/Contents/MacOS/diskman" > "$work/$1.log" 2>&1 &
  echo $!
}

wait_for() { # seconds, command…
  local n=$(($1 * 5)); shift
  for _ in $(seq "$n"); do "$@" && return 0; sleep 0.2; done
  return 1
}

# Tampered: refused, nothing swapped.
echo "update_feed = \"$base/tampered.json\"" > "$work/config/diskman/settings.toml"
pid="$(run_app tampered)"
wait_for 30 grep -q "update rejected" "$work/tampered.log" || { cat "$work/tampered.log"; fail "tampered DMG not rejected"; }
kill "$pid"
wait "$pid" 2> /dev/null || true
grep "update rejected" "$work/tampered.log" | sed 's/^/      /'
[ "$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")" = 0.1.0 ] || fail "app changed after a tampered DMG"
pass "tampered DMG refused, app still 0.1.0"

# The real one: detected, downloaded, verified, swapped, relaunched.
rm -f "$work/config/diskman/settings.toml" "$work/config/diskman/.update-check"
pid="$(run_app update)"
version_is() { [ "$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist" 2> /dev/null)" = "$1" ]; }
wait_for 60 version_is 0.1.1 || { cat "$work/update.log"; fail "not swapped"; }
wait_for 20 sh -c "! kill -0 $pid 2>/dev/null" || fail "0.1.0 did not quit"
grep -E "update:" "$work/update.log" | sed 's/^/      /'
pass "swapped to 0.1.1, 0.1.0 quit"
# Anchored: the pattern must not also match Contents/MacOS/diskman-mcp.
wait_for 20 pgrep -f "$app/Contents/MacOS/diskman( |$)" > /dev/null || fail "0.1.1 not relaunched"
new="$(pgrep -f "$app/Contents/MacOS/diskman( |$)" | head -1)"
[ "$("$app/Contents/MacOS/diskman" --version)" = 0.1.1 ] || fail "the new binary is not 0.1.1"
codesign --verify --deep --strict "$app" || fail "the swapped app's signature"
pass "0.1.1 relaunched (pid $new), signature valid"
kill "$new" 2> /dev/null || true
left="$(ls -A "$apps")"
[ "$left" = "Diskman.app" ] || fail "leftovers next to the app: $left"
pass "no staging copy or old app left; GET log:"
sed 's/^/      /' "$work/http.log" | grep GET || true
