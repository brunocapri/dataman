#!/bin/bash
# Self-update end to end on Linux with real AppImages: Dataman 0.0.1 finds 0.0.2 in a feed
# served from 127.0.0.1, downloads it, verifies it, replaces its own AppImage and relaunches;
# first, a tampered AppImage is refused and the file stays as it is. Never touches the user's
# config or keyring (XDG_CONFIG_HOME and DATAMAN_LOG_DIR point into the scratch folder; the
# signing key is a throwaway one). Needs a display (Xvfb will do); runs the AppImages
# extracted, so no FUSE is needed.
#
#   scripts/linux/test-update.sh [scratch-dir]
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; exit 1; }
[ "$(uname)" = Linux ] || fail "Linux only"
[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || fail "no display (run it under xvfb-run)"
case "$(uname -m)" in aarch64|arm64) arch=aarch64 ;; *) arch=x86_64 ;; esac
work="${1:-$(mktemp -d)}"
mkdir -p "$work"
work="$(cd "$work" && pwd)"
www="$work/www"
apps="$work/Apps"
app="$apps/Dataman.AppImage"
old=0.0.1
new=0.0.2
mkdir -p "$www" "$apps" "$work/config/dataman" "$work/logs"
export APPIMAGE_EXTRACT_AND_RUN=1

export DATAMAN_UPDATE_KEY="$work/update.key"
rm -f "$DATAMAN_UPDATE_KEY"
pub="$(cd "$root" && cargo run --release --quiet -p xtask -- keygen 2> /dev/null)"
port="$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
base="http://127.0.0.1:$port"
export DATAMAN_UPDATE_FEED="$base/feed.json" DATAMAN_UPDATE_PUBKEY="$pub" DATAMAN_NO_NOTIFY=1

version_of() { "$1" --version 2> /dev/null || true; }

DATAMAN_VERSION=$new DATAMAN_DIST="$work/v2" "$root/scripts/linux/build.sh" "$arch" > /dev/null
img="$work/v2/linux/Dataman-$new-$arch.AppImage"
cp "$img" "$www/Dataman-$new-$arch.AppImage"
cp "$img" "$www/tampered.AppImage"
size="$(stat -c %s "$img")"
dd if=/dev/urandom of="$www/tampered.AppImage" bs=1 count=16 seek=$((size / 2)) conv=notrunc 2> /dev/null
feed() { (cd "$root" && cargo run --release --quiet -p xtask -- feed --version $new --asset "linux-$arch" "$img" "$1"); }
feed "$base/Dataman-$new-$arch.AppImage" > "$www/feed.json"
feed "$base/tampered.AppImage" > "$www/tampered.json"
pass "$new built, AppImage $(du -h "$img" | cut -f1), feed signed"

DATAMAN_VERSION=$old DATAMAN_DIST="$work/v1" "$root/scripts/linux/build.sh" "$arch" > /dev/null
cp "$work/v1/linux/Dataman-$old-$arch.AppImage" "$app"
[ "$(version_of "$app")" = $old ] || fail "$old not installed"
pass "$old installed in $apps"

(cd "$www" && exec python3 -m http.server "$port" --bind 127.0.0.1 > "$work/http.log" 2>&1) &
server=$!
trap 'kill $server 2>/dev/null || true; pkill -f "$work/Apps" 2>/dev/null || true' EXIT
sleep 1

run_app() {
  XDG_CONFIG_HOME="$work/config" DATAMAN_LOG_DIR="$work/logs" DATAMAN_LOG=1 DATAMAN_UPDATE_AUTORESTART=1 \
    "$app" > "$work/$1.log" 2>&1 &
  echo $!
}
wait_for() { # seconds, command…
  local n=$(($1 * 5)); shift
  for _ in $(seq "$n"); do "$@" && return 0; sleep 0.2; done
  return 1
}
# The extracted app's processes (the AppImage runtime execs into them).
app_pids() { pgrep -f "appimage_extracted_[0-9a-f]*/usr/bin/dataman( |$)" || true; }

echo "update_feed = \"$base/tampered.json\"" > "$work/config/dataman/settings.toml"
pid="$(run_app tampered)"
wait_for 60 grep -q "update rejected" "$work/tampered.log" || { cat "$work/tampered.log"; fail "tampered AppImage not rejected"; }
kill $(app_pids) "$pid" 2> /dev/null || true
wait "$pid" 2> /dev/null || true
grep "update rejected" "$work/tampered.log" | sed 's/^/      /'
[ "$(version_of "$app")" = $old ] || fail "the AppImage changed after a tampered download"
pass "tampered AppImage refused, the app still $old"

rm -f "$work/config/dataman/settings.toml"
pid="$(run_app update)"
sleep 2
first="$(app_pids | head -1)"
version_is() { [ "$(version_of "$app")" = "$1" ]; }
wait_for 120 version_is $new || { cat "$work/update.log"; fail "not replaced"; }
wait_for 30 sh -c "! kill -0 ${first:-$pid} 2>/dev/null" || fail "$old did not quit"
grep -E "update:" "$work/update.log" | sed 's/^/      /'
pass "replaced by $new, $old quit"
relaunched() { app_pids | grep -vx "${first:-none}" | head -1 | grep -q .; }
wait_for 60 relaunched || fail "$new not relaunched"
pass "$new relaunched (pid $(app_pids | grep -vx "${first:-none}" | head -1))"
[ -x "$app" ] || fail "the new AppImage is not executable"
left="$(ls -A "$apps")"
[ "$left" = "Dataman.AppImage" ] || fail "leftovers next to the app: $left"
pass "no staged copy left; GET log:"
grep GET "$work/http.log" | sed 's/^/      /' || true
