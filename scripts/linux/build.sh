#!/bin/bash
# Builds the Linux release: dist/linux/Dataman-<version>-<arch>.AppImage and
# dataman_<version>_<debarch>.deb, for x86_64, aarch64 or both.
#
#   scripts/linux/build.sh [x86_64|aarch64|all]      # default: this machine's architecture
#
# On macOS (or any non-Linux host) each architecture builds in Docker (`--platform linux/...`;
# on Apple silicon x86_64 runs under Rosetta). On Linux it builds here, for this
# architecture only.
#
#   DATAMAN_VERSION         version baked in (default: the workspace's)
#   DATAMAN_UPDATE_FEED     update feed URL baked in (unset: the latest GitHub release)
#   DATAMAN_UPDATE_PUBKEY   update public key baked in (unset: the release key)
#   DATAMAN_LINUX_IMAGE     the build image (default: built from packaging/linux/Dockerfile)
#   DATAMAN_DIST            output folder (default dist; the files go to <dist>/linux)
#   DATAMAN_SKIP_BUILD=1    package target/release as it is (Linux only)
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
want="${1:-}"
die() { echo "linux/build: $*" >&2; exit 1; }
here_arch() { case "$(uname -m)" in arm64|aarch64) echo aarch64 ;; x86_64|amd64) echo x86_64 ;; *) uname -m ;; esac; }
[ -n "$want" ] || want="$(here_arch)"
case "$want" in x86_64|aarch64) archs=("$want") ;; all) archs=(x86_64 aarch64) ;; *) die "unknown architecture '$want' (x86_64, aarch64, all)" ;; esac
version="${DATAMAN_VERSION:-$(grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2)}"
dist="${DATAMAN_DIST:-$root/dist}/linux"

if [ "$(uname)" != Linux ]; then
  command -v docker > /dev/null || die "not on Linux, and no docker to build in"
  image="${DATAMAN_LINUX_IMAGE:-}"
  mkdir -p "$dist"
  for arch in "${archs[@]}"; do
    platform="linux/$([ "$arch" = x86_64 ] && echo amd64 || echo arm64)"
    img="$image"
    if [ -z "$img" ]; then
      img="dataman-linux-build:$arch"
      docker build --platform "$platform" -t "$img" -f "$root/packaging/linux/Dockerfile" "$root/packaging/linux" > /dev/null
    fi
    echo "==> $arch in $img"
    docker run --rm --platform "$platform" \
      -e DATAMAN_IN_DOCKER=1 -e DATAMAN_VERSION="$version" \
      -e DATAMAN_UPDATE_FEED="${DATAMAN_UPDATE_FEED:-}" -e DATAMAN_UPDATE_PUBKEY="${DATAMAN_UPDATE_PUBKEY:-}" \
      -e CARGO_TARGET_DIR=/target -e DATAMAN_DIST=/dist \
      -v "$root:/src:ro" -v "$(cd "$dist/.." && pwd):/dist" \
      -v "dataman-linux-target-$arch:/target" -v "dataman-linux-cargo-$arch:/usr/local/cargo/registry" \
      -v "dataman-linux-git-$arch:/usr/local/cargo/git" \
      -w /src "$img" bash scripts/linux/build.sh "$arch"
  done
  ls -la "$dist"
  exit 0
fi

# --- On Linux ------------------------------------------------------------------------------------
[ "${#archs[@]}" = 1 ] && [ "${archs[0]}" = "$(here_arch)" ] || die "on Linux this builds for $(here_arch) only"
arch="${archs[0]}"
target="${CARGO_TARGET_DIR:-$root/target}"
if [ -z "${DATAMAN_SKIP_BUILD:-}" ]; then
  (cd "$root" && DATAMAN_VERSION="$version" cargo build --release --locked -p dataman -p dataman-mcp)
fi
baked="$("$target/release/dataman" --version 2> /dev/null || true)"
[ "$baked" = "$version" ] || die "the binary says '$baked', expected '$version' (rebuild with DATAMAN_VERSION=$version)"
mkdir -p "$dist"
DATAMAN_TARGET="$target" "$root/scripts/linux/appimage.sh" "$arch" "$version" "$dist"
DATAMAN_TARGET="$target" "$root/scripts/linux/deb.sh" "$arch" "$version" "$dist"
