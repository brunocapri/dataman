#!/bin/bash
# Starts a release built in CI (.github/workflows/release.yml): bumps the workspace version,
# commits "Release <version>", tags v<version> (annotated; its message is the release notes) and
# pushes both. CI then builds, signs, notarizes and publishes the release and its update feed.
# (scripts/github-release.sh does the same from this Mac instead.)
#
#   scripts/tag-release.sh 0.3.7 "Faster grid scrolling. SQL Server table DDL."
#   scripts/tag-release.sh 0.3.7 ~/notes-0.3.7.md      # the notes from a file
#
# Needs: a clean tree on main, up to date with origin.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
usage="usage: tag-release.sh <version> [notes text or file]"
version="${1:?$usage}"
notes_arg="${2:-}"
die() { echo "tag-release: $*" >&2; exit 1; }

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "'$version' is not a version (x.y.z)"
tag="v$version"
if [ -n "$notes_arg" ] && [ -f "$notes_arg" ]; then
  notes="$(cat "$notes_arg")"
else
  notes="$notes_arg"
fi
[ -n "$notes" ] || notes="Dataman $version"

[ -z "$(git status --porcelain)" ] || die "the working tree is not clean"
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die "not on main"
git fetch --quiet --tags origin
[ -z "$(git rev-list HEAD..origin/main)" ] || die "main is behind origin/main (pull first)"
git rev-parse -q --verify "refs/tags/$tag" > /dev/null && die "the tag $tag exists"
[ -z "$(git ls-remote --tags origin "refs/tags/$tag")" ] || die "the tag $tag exists on origin"

current="$(grep -m1 '^version = "' Cargo.toml | cut -d'"' -f2)"
newest="$(printf '%s\n%s\n' "$current" "$version" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
[ "$newest" = "$version" ] || die "$version is older than the current $current"

if [ "$current" != "$version" ]; then
  echo "==> version $current -> $version"
  tmp="$(mktemp)"
  awk -v v="$version" '
    /^\[/ { section = $0 }
    section == "[workspace.package]" && /^version = "/ && !done { print "version = \"" v "\""; done = 1; next }
    { print }
  ' Cargo.toml > "$tmp"
  cp "$tmp" Cargo.toml
  rm -f "$tmp"
  [ "$(grep -m1 '^version = "' Cargo.toml | cut -d'"' -f2)" = "$version" ] || die "the version did not change in Cargo.toml"
  cargo update --workspace --offline --quiet 2> /dev/null || cargo update --workspace --quiet
  git add Cargo.toml Cargo.lock
  git commit --quiet -m "Release $version"
fi
printf '%s\n' "$notes" | git tag -a "$tag" -F -
git push --quiet origin main
git push --quiet origin "$tag"
echo "pushed $tag: https://github.com/brunocapri/dataman/actions/workflows/release.yml"
