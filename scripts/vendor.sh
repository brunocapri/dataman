#!/bin/bash
# vendor/<crate> is a crates.io release plus vendor/<crate>.patch, Dataman's changes to it.
#
#   scripts/vendor.sh apply [crate]   recreates vendor/<crate> from the release and the patch
#   scripts/vendor.sh diff [crate]    rewrites vendor/<crate>.patch from vendor/<crate> as edited
#   scripts/vendor.sh check [crate]   fails unless vendor/<crate> is exactly release + patch (CI)
#
# To change a vendored crate, edit vendor/<crate> and run `diff`. To upgrade one, set its version
# and checksum below (the `cksum` of that version in https://index.crates.io) and run `apply`:
# the hunks that no longer apply are left in vendor/<crate> as *.rej files. Make those changes by
# hand, delete the .rej files, then run `diff`.
set -euo pipefail
cd "$(dirname "$0")/.."

# name, version, sha256 of the .crate (the crates.io registry checksum), and the release's
# files Dataman leaves out: upstream's CI, dev environment and docs, unused by the build.
CRATES=(
  "tiberius 0.13.0 e07324791de2bdaed058af4aa433a0b5ec5c0beac407a9797419c19805df2415 .envrc .github .gitleaks.toml CHANGELOG.md Cargo.lock README.md deny.toml docker docker-compose.yml docs flake.lock flake.nix rust-toolchain"
  "tokio-postgres 0.7.18 a528f7d280f6d5b9cd149635c8705b0dd049754bc67d81d31fa25169a93809d3 Cargo.lock"
)

cmd=${1:-}
only=${2:-}
case "$cmd" in apply | diff | check) ;; *) sed -n '2,11p' "$0" >&2; exit 2 ;; esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

# Extracts the verified release into $tmp/<name>/a, without the excluded files.
release() {
  local name=$1 version=$2 sum=$3
  local file="$tmp/$name.crate"
  shift 3
  curl -fsSL -o "$file" "https://static.crates.io/crates/$name/$name-$version.crate"
  local got
  got=$(sha256 "$file")
  [ "$got" = "$sum" ] || { echo "$name $version: sha256 $got, expected $sum" >&2; exit 1; }
  mkdir -p "$tmp/$name"
  tar -xzf "$file" -C "$tmp/$name"
  mv "$tmp/$name/$name-$version" "$tmp/$name/a"
  local x
  for x in "$@"; do
    [ -e "$tmp/$name/a/$x" ] || { echo "$name $version: excluded $x is not in the release" >&2; exit 1; }
    rm -rf "${tmp:?}/$name/a/$x"
  done
}

# Release + patch in $tmp/<name>/b. Outside any git repository, `git apply` works on plain files
# and refuses a hunk that does not apply exactly; extra arguments go to it.
patched() {
  local name=$1
  shift
  cp -R "$tmp/$name/a" "$tmp/$name/b"
  (cd "$tmp/$name/b" && git apply --whitespace=nowarn "$@" "$OLDPWD/vendor/$name.patch")
}

status=0
for row in "${CRATES[@]}"; do
  read -r -a f <<<"$row"
  name=${f[0]}
  [ -z "$only" ] || [ "$only" = "$name" ] || continue
  release "${f[@]}"
  case "$cmd" in
    apply)
      patched "$name" --reject || status=1
      rm -rf "vendor/$name"
      cp -R "$tmp/$name/b" "vendor/$name"
      if [ $status = 0 ]; then echo "vendor/$name: ${f[1]} + vendor/$name.patch"; else echo "vendor/$name: hunks rejected, see the .rej files" >&2; fi
      ;;
    diff)
      ! find "vendor/$name" -name '*.rej' | grep . || { echo "vendor/$name: rejected hunks left" >&2; exit 1; }
      rm -rf "$tmp/$name/b"
      cp -R "vendor/$name" "$tmp/$name/b"
      # The trees are named a and b, so without prefixes the paths read a/src/… and b/src/…. Fixed
      # options, so the user's git config cannot change the patch. Exit status 1 means differences.
      (cd "$tmp/$name" && git diff --no-index --no-prefix --no-color --no-ext-diff --no-renames \
        --diff-algorithm=myers a b) >"vendor/$name.patch" || [ $? = 1 ]
      echo "vendor/$name.patch: $(grep -c '^diff --git' "vendor/$name.patch") files"
      ;;
    check)
      patched "$name"
      if diff -r "$tmp/$name/b" "vendor/$name" >"$tmp/$name.diff"; then
        echo "vendor/$name: ${f[1]} + vendor/$name.patch"
      else
        cat "$tmp/$name.diff"
        echo "vendor/$name is not ${f[1]} + vendor/$name.patch: run scripts/vendor.sh diff $name (or apply, to drop the edits)" >&2
        status=1
      fi
      ;;
  esac
done
exit $status
