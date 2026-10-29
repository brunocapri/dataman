#!/bin/bash
# Builds release, signs target/release/dataman as the released app and runs it.
#
# The Keychain trusts a binary by its designated requirement. An ad hoc build changes it on
# every build, so each saved password asks again; signed with the Developer ID and the release's
# bundle identifier, every build is the same app and "Always Allow" holds across rebuilds (and
# for the passwords the released Dataman saved).
#
#   DATAMAN_SIGN_IDENTITY   e.g. "Developer ID Application: Name (TEAMID)" (unset: the first
#                           Developer ID Application identity in the keychain, else ad hoc)
#   DATAMAN_BUNDLE_ID       identifier signed in (default dev.dataman, as scripts/bundle.sh)
#   DATAMAN_SKIP_BUILD=1    sign and run target/release as it is
#
#   scripts/dev.sh [args for dataman]
set -euo pipefail
[ "$(uname)" = Darwin ] || { echo "dev: macOS only" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
bundle_id="${DATAMAN_BUNDLE_ID:-dev.dataman}"
identity="${DATAMAN_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -1)}"
if [ -z "$identity" ]; then
  echo "dev: no Developer ID Application identity: signing ad hoc, the Keychain will ask again after each build" >&2
  identity=-
fi

# Same settings as a plain `cargo build --release`, so the two share what they compiled.
if [ -z "${DATAMAN_SKIP_BUILD:-}" ]; then
  cargo build --release -p dataman
fi

bin=target/release/dataman
codesign --force --options runtime --timestamp=none --identifier "$bundle_id" \
  --entitlements packaging/entitlements.plist --sign "$identity" "$bin"
codesign --verify --strict "$bin"
exec "$bin" "$@"
