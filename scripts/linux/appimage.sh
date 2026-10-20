#!/bin/bash
# The built binaries → <dist>/Dataman-<version>-<arch>.AppImage. Linux only; called by build.sh.
#
#   scripts/linux/appimage.sh <arch> <version> <dist>
#
# The binaries link only libraries every desktop has (checked); GPU, Wayland and the rest are
# loaded at run time.
# appimagetool and the AppImage runtime are pinned and checked by hash, cached in
# ~/.cache/dataman-release (DATAMAN_TOOLS).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:?usage: appimage.sh <arch> <version> <dist>}"
version="${2:?usage: appimage.sh <arch> <version> <dist>}"
dist="${3:?usage: appimage.sh <arch> <version> <dist>}"
target="${DATAMAN_TARGET:-$root/target}"
die() { echo "appimage: $*" >&2; exit 1; }
[ "$(uname)" = Linux ] || die "Linux only (scripts/linux/build.sh builds in Docker elsewhere)"

tool_version=1.9.1
runtime_version=20251108
case "$arch" in
  x86_64)
    tool_sha=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    runtime_sha=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    ;;
  aarch64)
    tool_sha=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    runtime_sha=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
    ;;
  *) die "unknown architecture $arch" ;;
esac
tools="${DATAMAN_TOOLS:-$HOME/.cache/dataman-release}"
mkdir -p "$tools"
fetch() { # url sha file
  if [ ! -f "$3" ] || ! echo "$2  $3" | sha256sum -c --quiet 2> /dev/null; then
    curl -fsSL --proto '=https' -o "$3.part" "$1"
    echo "$2  $3.part" | sha256sum -c --quiet || die "$1: unexpected sha256"
    mv "$3.part" "$3"
  fi
}
tool="$tools/appimagetool-$tool_version-$arch.AppImage"
runtime="$tools/runtime-$runtime_version-$arch"
fetch "https://github.com/AppImage/appimagetool/releases/download/$tool_version/appimagetool-$arch.AppImage" "$tool_sha" "$tool"
fetch "https://github.com/AppImage/type2-runtime/releases/download/$runtime_version/runtime-$arch" "$runtime_sha" "$runtime"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
app="$work/Dataman.AppDir"
mkdir -p "$app/usr/bin" "$app/usr/lib" "$app/usr/share/applications" "$app/usr/share/metainfo"
cp "$target/release/dataman" "$target/release/dataman-mcp" "$app/usr/bin/"
strip "$app/usr/bin/dataman" "$app/usr/bin/dataman-mcp"
cp "$root/packaging/linux/AppRun" "$app/AppRun"
chmod +x "$app/AppRun"
cp "$root/packaging/linux/dev.dataman.Dataman.desktop" "$app/"
cp "$root/packaging/linux/dev.dataman.Dataman.desktop" "$app/usr/share/applications/"
cp "$root/packaging/linux/dev.dataman.Dataman.metainfo.xml" "$app/usr/share/metainfo/"
for n in 128 256 512; do
  mkdir -p "$app/usr/share/icons/hicolor/${n}x${n}/apps"
  convert "$root/packaging/AppIcon.png" -resize "${n}x${n}" "$app/usr/share/icons/hicolor/${n}x${n}/apps/dev.dataman.Dataman.png"
done
cp "$app/usr/share/icons/hicolor/256x256/apps/dev.dataman.Dataman.png" "$app/dev.dataman.Dataman.png"
ln -s dev.dataman.Dataman.png "$app/.DirIcon"

# What the binaries link is on every X11 or Wayland desktop (the pure-Rust core brings the
# rest): nothing is bundled, and a new link outside this list stops the release.
system='^(libc|libm|libdl|libpthread|librt|libgcc_s|libxcb|libxkbcommon|libxkbcommon-x11|libfontconfig|libfreetype|ld-linux-[a-z0-9_-]+)\.so'
for bin in dataman dataman-mcp; do
  for lib in $(readelf -d "$app/usr/bin/$bin" | awk '/NEEDED/ {gsub(/[][]/, "", $5); print $5}'); do
    echo "$lib" | grep -Eq "$system" || die "$bin links $lib, not one every desktop has (bundle it, or drop it)"
  done
done
rmdir "$app/usr/lib"

# An AppImage marks its ELF header (bytes 8-10, "AI\2"), which emulators (Rosetta, qemu) refuse
# to run: an unmarked copy runs anywhere. Only copies are unmarked, never what ships.
runnable() {
  local copy
  copy="$work/run-$(basename "$1")"
  cp "$1" "$copy"
  printf '\000\000\000' | dd of="$copy" bs=1 seek=8 count=3 conv=notrunc 2> /dev/null
  chmod +x "$copy"
  echo "$copy"
}

out="$dist/Dataman-$version-$arch.AppImage"
rm -f "$out"
ARCH="$arch" APPIMAGE_EXTRACT_AND_RUN=1 "$(runnable "$tool")" --no-appstream --runtime-file "$runtime" "$app" "$out" > "$work/tool.log" 2>&1 || { cat "$work/tool.log"; die "appimagetool failed"; }
chmod +x "$out"
said="$(APPIMAGE_EXTRACT_AND_RUN=1 "$(runnable "$out")" --version 2> /dev/null || true)"
[ "$said" = "$version" ] || die "$out says '$said', expected $version"
echo "$out"
echo "  $(du -h "$out" | cut -f1)"
