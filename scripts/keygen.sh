#!/bin/bash
# A new ed25519 key for update signatures, kept outside the repository:
# ~/.config/diskman-release/update_ed25519.key (0600), or DISKMAN_UPDATE_KEY. Prints the public
# key to build with (DISKMAN_UPDATE_PUBKEY). --force replaces an existing key.
#
#   scripts/keygen.sh
#   scripts/keygen.sh --pubkey     # the public key of the current one
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
if [ "${1:-}" = "--pubkey" ]; then
  exec cargo run --release --quiet -p xtask -- pubkey
fi
exec cargo run --release --quiet -p xtask -- keygen "$@"
