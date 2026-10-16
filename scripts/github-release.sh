#!/bin/bash
# Publishes a release on GitHub Releases, where the app looks for updates
# (releases/latest/download/feed.json): bumps the workspace version, builds the app and the DMG,
# signs the DMG into dist/feed.json, commits "Release <version>", tags v<version>, pushes both,
# uploads the DMG and the feed, then reads the feed back from GitHub and checks it as the app
# would (version, size, signature with the app's public key).
#
#   scripts/github-release.sh 0.2.1 "Faster grid scrolling. SQL Server table DDL."
#   scripts/github-release.sh 0.2.1 ~/notes-0.2.1.txt     # the notes from a file
#
# Needs: a clean tree on main, up to date with origin; gh, logged in; the update key
# (scripts/keygen.sh: ~/.config/dataman-release/update_ed25519.key, or DATAMAN_UPDATE_KEY /
# DATAMAN_UPDATE_PRIVATE_KEY). The notes are never written into the repository.
set -euo pipefail

repo="brunocapri/dataman"
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
usage="usage: github-release.sh <version> [notes text or file]"
version="${1:?$usage}"
notes_arg="${2:-}"
die() { echo "github-release: $*" >&2; exit 1; }
step() { echo "==> $*"; }

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "'$version' is not a version (x.y.z)"
tag="v$version"

# The notes: a file's text, or the text itself.
if [ -n "$notes_arg" ] && [ -f "$notes_arg" ]; then
  notes="$(cat "$notes_arg")"
else
  notes="$notes_arg"
fi
[ -n "$notes" ] || notes="Dataman $version"

# --- Preconditions -----------------------------------------------------------------------------
command -v gh > /dev/null || die "gh is not installed"
gh auth status > /dev/null 2>&1 || die "gh is not logged in (gh auth login)"
[ -z "$(git status --porcelain)" ] || die "the working tree is not clean"
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die "not on main"
git fetch --quiet --tags origin
[ -z "$(git rev-list HEAD..origin/main)" ] || die "main is behind origin/main (pull first)"
git rev-parse -q --verify "refs/tags/$tag" > /dev/null && die "the tag $tag exists"
[ -z "$(git ls-remote --tags origin "refs/tags/$tag")" ] || die "the tag $tag exists on origin"
gh release view "$tag" --repo "$repo" > /dev/null 2>&1 && die "the release $tag exists"
for v in DATAMAN_UPDATE_FEED DATAMAN_UPDATE_PUBKEY DATAMAN_VERSION DATAMAN_BUILD DATAMAN_DIST DATAMAN_SKIP_BUILD; do
  [ -z "${!v:-}" ] || die "$v is set: a release builds with the defaults"
done

current="$(grep -m1 '^version = "' Cargo.toml | cut -d'"' -f2)"
newest="$(printf '%s\n%s\n' "$current" "$version" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
[ "$newest" = "$version" ] || die "$version is older than the current $current"

# The key that signs must be the one the app trusts (DEFAULT_PUBKEY in updates.rs).
pubkey="$(grep -m1 'pub const DEFAULT_PUBKEY' crates/dataman/src/updates.rs | cut -d'"' -f2)"
[ -n "$pubkey" ] || die "no DEFAULT_PUBKEY in crates/dataman/src/updates.rs"
signing="$(cargo run --release --quiet -p xtask -- pubkey)" || die "no update key (scripts/keygen.sh)"
[ "$signing" = "$pubkey" ] || die "the update key does not match the app's DEFAULT_PUBKEY"

work="$(mktemp -d)"
bumped=""
committed=""
cleanup() {
  local rc=$?
  rm -rf "$work"
  # A failure before the commit leaves the tree as it was.
  if [ $rc -ne 0 ] && [ -n "$bumped" ] && [ -z "$committed" ]; then
    git checkout -- Cargo.toml Cargo.lock 2> /dev/null || true
    echo "github-release: failed; Cargo.toml and Cargo.lock restored" >&2
  fi
}
trap cleanup EXIT

# --- The version -------------------------------------------------------------------------------
if [ "$current" != "$version" ]; then
  step "version $current -> $version"
  # Only the `version = "…"` line of the [workspace.package] table.
  awk -v v="$version" '
    /^\[/ { section = $0 }
    section == "[workspace.package]" && /^version = "/ && !done { print "version = \"" v "\""; done = 1; next }
    { print }
  ' Cargo.toml > "$work/Cargo.toml"
  cp "$work/Cargo.toml" Cargo.toml
  bumped=1
  [ "$(grep -m1 '^version = "' Cargo.toml | cut -d'"' -f2)" = "$version" ] || die "the version did not change in Cargo.toml"
  cargo update --workspace --offline --quiet 2> /dev/null || cargo update --workspace --quiet
fi

# --- Build, DMG, signed feed -------------------------------------------------------------------
base="https://github.com/$repo/releases/download/$tag"
printf '%s\n' "$notes" > "$work/notes.txt"
step "building Dataman $version"
"$root/scripts/release.sh" "$base" "$work/notes.txt"
dmg="$root/dist/Dataman-$version.dmg"
feed="$root/dist/feed.json"
[ -f "$dmg" ] || die "no $dmg"
[ "$(plutil -extract version raw -o - "$feed")" = "$version" ] || die "dist/feed.json is not for $version"
[ "$(plutil -extract url raw -o - "$feed")" = "$base/Dataman-$version.dmg" ] || die "dist/feed.json points elsewhere"
cargo run --release --quiet -p xtask -- verify "$feed" "$dmg" --pubkey "$pubkey" --version "$version" > /dev/null

# --- Commit, tag, push -------------------------------------------------------------------------
if [ -n "$bumped" ]; then
  step "commit and tag $tag"
  git add Cargo.toml Cargo.lock
  git commit --quiet -m "Release $version"
  committed=1
fi
git tag -a "$tag" -m "Dataman $version"
git push --quiet origin main
git push --quiet origin "$tag"

# --- The release -------------------------------------------------------------------------------
step "GitHub release $tag"
gh release create "$tag" "$dmg" "$feed" --repo "$repo" --title "Dataman $version" --notes "$notes" --latest > /dev/null

# --- What the app will read --------------------------------------------------------------------
step "checking releases/latest/download/feed.json"
latest="https://github.com/$repo/releases/latest/download/feed.json"
ok=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if curl -fsSL --proto '=https' -o "$work/feed.json" "$latest" \
    && [ "$(plutil -extract version raw -o - "$work/feed.json" 2> /dev/null)" = "$version" ]; then
    ok=1
    break
  fi
  sleep 3
done
[ -n "$ok" ] || die "the latest feed does not offer $version yet ($latest)"
url="$(plutil -extract url raw -o - "$work/feed.json")"
curl -fsSL --proto '=https' -o "$work/Dataman.dmg" "$url"
checked="$(cargo run --release --quiet -p xtask -- verify "$work/feed.json" "$work/Dataman.dmg" --pubkey "$pubkey" --version "$version")"

page="$(gh release view "$tag" --repo "$repo" --json url --jq .url)"
echo
echo "Dataman $version released"
echo "  release  $page"
echo "  commit   $(git rev-parse --short HEAD) (tag $tag)"
echo "  assets   Dataman-$version.dmg ($(du -h "$dmg" | cut -f1 | tr -d ' ')), feed.json"
echo "  feed     $latest"
echo "  check    $checked (downloaded from GitHub)"
