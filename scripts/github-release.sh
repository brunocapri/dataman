#!/bin/bash
# Publishes a release on GitHub Releases, where the app looks for updates
# (releases/latest/download/feed.json): bumps the workspace version, builds the app and the DMG,
# commits "Release <version>", tags v<version> and pushes both. The tag runs the Linux and
# Windows workflows: once both pass, their AppImages, .debs, installers and .zips join the DMG,
# the feed is signed for every platform, everything is uploaded, and the feed is read back from
# GitHub and checked as the app would (version, size, signature with the app's public key).
#
#   scripts/github-release.sh 0.2.1 "Faster grid scrolling. SQL Server table DDL."
#   scripts/github-release.sh 0.2.1 ~/notes-0.2.1.txt     # the notes from a file
#
# Needs: a clean tree on main, up to date with origin; gh, logged in; the update key
# (scripts/keygen.sh: ~/.config/dataman-release/update_ed25519.key, or DATAMAN_UPDATE_KEY /
# DATAMAN_UPDATE_PRIVATE_KEY). The notes are never written into the repository. A workflow that
# fails leaves the tag pushed with no release: fix it and release the next version.
set -euo pipefail
[ "$(uname)" = Darwin ] || { echo "github-release: macOS only (the Linux release: scripts/linux/build.sh)" >&2; exit 1; }

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
# The Mac only: Linux comes from the tag's workflow, not from Docker here.
env -u DATAMAN_LINUX "$root/scripts/release.sh" "$base" "$work/notes.txt"
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

# --- Linux and Windows: what the tag's workflows built -----------------------------------------
sha="$(git rev-parse HEAD)"
# The run a workflow started for the tag's push, once it has passed.
run_for() {
  local wf="$1" run=""
  for _ in $(seq 1 60); do
    run="$(gh run list --repo "$repo" --workflow "$wf" --commit "$sha" --event push --json databaseId --jq '.[0].databaseId // empty')"
    [ -n "$run" ] && break
    sleep 10
  done
  [ -n "$run" ] || die "no $wf run for $tag: the tag is pushed with no release"
  gh run watch "$run" --repo "$repo" --exit-status > /dev/null \
    || die "$wf failed (run $run): the tag $tag is pushed with no release; fix it and release the next version"
  echo "$run"
}
step "the Linux and Windows workflows on $tag"
linux_run="$(run_for linux.yml)"
windows_run="$(run_for windows.yml)"

macarch="$([ "$(uname -m)" = arm64 ] && echo aarch64 || echo x86_64)"
feedargs=(feed --version "$version" --base "$feed" --asset "macos-$macarch" "$dmg" "$base/$(basename "$dmg")")
checks=()
linux=()
for a in x86_64; do
  d="$work/linux-$a"
  gh run download "$linux_run" --repo "$repo" -n "dataman-linux-$a" -D "$d" > /dev/null 2>&1 || die "run $linux_run has no Linux $a build"
  img="$d/Dataman-$version-$a.AppImage"
  deb="$d/dataman_${version}_amd64.deb"
  [ -f "$img" ] && [ -f "$deb" ] || die "run $linux_run has no Dataman $version AppImage and .deb for Linux $a: $(ls "$d" | tr '\n' ' ')"
  linux+=("$img" "$deb")
  feedargs+=(--asset "linux-$a" "$img" "$base/$(basename "$img")" --asset "linux-$a-deb" "$deb" "$base/$(basename "$deb")")
  checks+=("linux-$a:$img" "linux-$a-deb:$deb")
done
windows=()
winarch=()
for a in x86_64; do
  d="$work/windows-$a"
  gh run download "$windows_run" --repo "$repo" -n "dataman-windows-$a" -D "$d" > /dev/null 2>&1 || die "run $windows_run has no Windows $a build"
  setup="$d/Dataman-$version-windows-$a-setup.exe"
  zip="$d/Dataman-$version-windows-$a.zip"
  [ -f "$setup" ] && [ -f "$zip" ] || die "run $windows_run has no Dataman $version installer and .zip for Windows $a: $(ls "$d" | tr '\n' ' ')"
  windows+=("$setup" "$zip")
  winarch+=("$a")
  feedargs+=(--asset "windows-$a" "$setup" "$base/$(basename "$setup")")
  checks+=("windows-$a:$setup")
done

step "the feed, for every platform"
cargo run --release --quiet -p xtask -- "${feedargs[@]}" > "$work/feed-all.json"
cp "$work/feed-all.json" "$feed"
cargo run --release --quiet -p xtask -- verify "$feed" "$dmg" --pubkey "$pubkey" --version "$version" > /dev/null
for c in "${checks[@]}"; do
  cargo run --release --quiet -p xtask -- verify "$feed" "${c#*:}" --pubkey "$pubkey" --version "$version" --asset "${c%%:*}" > /dev/null
done

# --- The release -------------------------------------------------------------------------------
step "GitHub release $tag"
gh release create "$tag" "$dmg" "${linux[@]}" "${windows[@]}" "$feed" --repo "$repo" --title "Dataman $version" --notes "$notes" --latest > /dev/null

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
for a in x86_64; do
  url="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["assets"]["linux-" + sys.argv[2]]["url"])' "$work/feed.json" "$a")"
  curl -fsSL --proto '=https' -o "$work/Dataman-$a.AppImage" "$url"
  cargo run --release --quiet -p xtask -- verify "$work/feed.json" "$work/Dataman-$a.AppImage" --pubkey "$pubkey" --version "$version" --asset "linux-$a" > /dev/null
  url="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["assets"]["linux-" + sys.argv[2] + "-deb"]["url"])' "$work/feed.json" "$a")"
  curl -fsSL --proto '=https' -o "$work/dataman-$a.deb" "$url"
  cargo run --release --quiet -p xtask -- verify "$work/feed.json" "$work/dataman-$a.deb" --pubkey "$pubkey" --version "$version" --asset "linux-$a-deb" > /dev/null
done
checked="$checked; the Linux x86_64 AppImage and .deb too"
for a in ${winarch[@]+"${winarch[@]}"}; do
  url="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["assets"]["windows-" + sys.argv[2]]["url"])' "$work/feed.json" "$a")"
  curl -fsSL --proto '=https' -o "$work/Dataman-$a-setup.exe" "$url"
  cargo run --release --quiet -p xtask -- verify "$work/feed.json" "$work/Dataman-$a-setup.exe" --pubkey "$pubkey" --version "$version" --asset "windows-$a" > /dev/null
  checked="$checked; the Windows $a installer too"
done

page="$(gh release view "$tag" --repo "$repo" --json url --jq .url)"
echo
echo "Dataman $version released"
echo "  release  $page"
echo "  commit   $(git rev-parse --short HEAD) (tag $tag)"
echo "  assets   Dataman-$version.dmg ($(du -h "$dmg" | cut -f1 | tr -d ' ')), feed.json"
echo "  feed     $latest"
echo "  check    $checked (downloaded from GitHub)"
