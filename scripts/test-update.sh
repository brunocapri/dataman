#!/bin/bash
# Self-update end to end with the real app: Dataman 0.0.1, installed in a scratch folder,
# finds 0.0.2 in a feed served from 127.0.0.1, downloads it, verifies it, swaps itself and
# relaunches; first, a tampered DMG is refused and the app stays as it is. Never touches
# /Applications, the user's config, Keychain or logs (XDG_CONFIG_HOME and DATAMAN_LOG_DIR point
# into the scratch folder; the signing key is a throwaway one). Needs a desktop session.
#
#   scripts/test-update.sh [scratch-dir]
#
# Leaves target/release built with the test feed and key: rebuild afterwards.
set -euo pipefail
[ "$(uname)" = Darwin ] || { echo "test-update: macOS only (the Linux release: scripts/linux/build.sh)" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
work="${1:-$(mktemp -d)}"
mkdir -p "$work"
work="$(cd "$work" && pwd)"
www="$work/www"
apps="$work/Applications"
app="$apps/Dataman.app"
old=0.0.1
new=0.0.2
mkdir -p "$www" "$apps" "$work/config/dataman" "$work/logs"
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; exit 1; }

export DATAMAN_UPDATE_KEY="$work/update.key"
rm -f "$DATAMAN_UPDATE_KEY"
pub="$("$root/scripts/keygen.sh" 2>/dev/null)"
port="$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
[ "$port" != 7700 ] || fail "port 7700 picked"
base="http://127.0.0.1:$port"
export DATAMAN_UPDATE_FEED="$base/feed.json" DATAMAN_UPDATE_PUBKEY="$pub" DATAMAN_NO_NOTIFY=1

# The update, its signed feed, and a tampered copy under a feed signed for the real one.
DATAMAN_VERSION=$new DATAMAN_DIST="$work/v2" "$root/scripts/bundle.sh" > /dev/null
DATAMAN_DIST="$work/v2" "$root/scripts/dmg.sh" > /dev/null
dmg="$work/v2/Dataman-$new.dmg"
cp "$dmg" "$www/Dataman-$new.dmg"
cp "$dmg" "$www/tampered.dmg"
size="$(stat -f %z "$dmg")"
dd if=/dev/urandom of="$www/tampered.dmg" bs=1 count=16 seek=$((size / 2)) conv=notrunc 2> /dev/null
DATAMAN_DIST="$work/v2" "$root/scripts/sign-update.sh" "$dmg" "$base/Dataman-$new.dmg" > "$www/feed.json"
DATAMAN_DIST="$work/v2" "$root/scripts/sign-update.sh" "$dmg" "$base/tampered.dmg" > "$www/tampered.json"
pass "$new bundled, DMG $(du -h "$dmg" | cut -f1), feed signed"

# The installed app, with the same feed and key.
DATAMAN_VERSION=$old DATAMAN_DIST="$work/v1" "$root/scripts/bundle.sh" > /dev/null
rm -rf "$app"
ditto "$work/v1/Dataman.app" "$app"
[ "$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")" = $old ] || fail "$old not installed"
pass "$old installed in $apps"

(cd "$www" && exec python3 -m http.server "$port" --bind 127.0.0.1 > "$work/http.log" 2>&1) &
server=$!
trap 'kill $server 2>/dev/null || true' EXIT
sleep 1

run_app() {
  XDG_CONFIG_HOME="$work/config" DATAMAN_LOG_DIR="$work/logs" DATAMAN_LOG=1 DATAMAN_UPDATE_AUTORESTART=1 \
    "$app/Contents/MacOS/dataman" > "$work/$1.log" 2>&1 &
  echo $!
}

wait_for() { # seconds, command…
  local n=$(($1 * 5)); shift
  for _ in $(seq "$n"); do "$@" && return 0; sleep 0.2; done
  return 1
}

# Tampered: refused, nothing swapped.
echo "update_feed = \"$base/tampered.json\"" > "$work/config/dataman/settings.toml"
pid="$(run_app tampered)"
wait_for 30 grep -q "update rejected" "$work/tampered.log" || { cat "$work/tampered.log"; fail "tampered DMG not rejected"; }
kill "$pid"
wait "$pid" 2> /dev/null || true
grep "update rejected" "$work/tampered.log" | sed 's/^/      /'
[ "$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")" = $old ] || fail "app changed after a tampered DMG"
pass "tampered DMG refused, app still $old"

# The real one: detected, downloaded, verified, swapped, relaunched.
rm -f "$work/config/dataman/settings.toml"
pid="$(run_app update)"
version_is() { [ "$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist" 2> /dev/null)" = "$1" ]; }
wait_for 60 version_is $new || { cat "$work/update.log"; fail "not swapped"; }
wait_for 20 sh -c "! kill -0 $pid 2>/dev/null" || fail "$old did not quit"
grep -E "update:" "$work/update.log" | sed 's/^/      /'
pass "swapped to $new, $old quit"
# Anchored: the pattern must not also match Contents/MacOS/dataman-mcp.
wait_for 20 pgrep -f "$app/Contents/MacOS/dataman( |$)" > /dev/null || fail "$new not relaunched"
relaunched="$(pgrep -f "$app/Contents/MacOS/dataman( |$)" | head -1)"
[ "$("$app/Contents/MacOS/dataman" --version)" = $new ] || fail "the new binary is not $new"
codesign --verify --deep --strict "$app" || fail "the swapped app's signature"
[ -z "$(xattr -r "$app" | grep com.apple.quarantine || true)" ] || fail "the swapped app is quarantined"
pass "$new relaunched (pid $relaunched), signature valid, no quarantine"
kill "$relaunched" 2> /dev/null || true
left="$(ls -A "$apps")"
[ "$left" = "Dataman.app" ] || fail "leftovers next to the app: $left"
pass "no staging copy or old app left; GET log:"
sed 's/^/      /' "$work/http.log" | grep GET || true
