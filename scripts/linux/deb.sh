#!/bin/bash
# The built binaries → <dist>/dataman_<version>_<amd64|arm64>.deb. Linux only; called by build.sh.
# Installed under /usr, the app leaves updates to the package manager (`update::installed_app`).
#
#   scripts/linux/deb.sh <arch> <version> <dist>
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:?usage: deb.sh <arch> <version> <dist>}"
version="${2:?usage: deb.sh <arch> <version> <dist>}"
dist="${3:?usage: deb.sh <arch> <version> <dist>}"
target="${DATAMAN_TARGET:-$root/target}"
die() { echo "deb: $*" >&2; exit 1; }
[ "$(uname)" = Linux ] || die "Linux only (scripts/linux/build.sh builds in Docker elsewhere)"
command -v dpkg-deb > /dev/null || die "no dpkg-deb"
case "$arch" in x86_64) debarch=amd64 ;; aarch64) debarch=arm64 ;; *) die "unknown architecture $arch" ;; esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
pkg="$work/dataman"
mkdir -p "$pkg/DEBIAN" "$pkg/usr/bin" "$pkg/usr/share/applications" "$pkg/usr/share/metainfo" "$pkg/usr/share/doc/dataman" "$pkg/usr/share/mime/packages"
install -m 0755 "$target/release/dataman" "$target/release/dataman-mcp" "$pkg/usr/bin/"
strip "$pkg/usr/bin/dataman" "$pkg/usr/bin/dataman-mcp"
install -m 0644 "$root/packaging/linux/dev.dataman.Dataman.desktop" "$pkg/usr/share/applications/"
install -m 0644 "$root/packaging/linux/dev.dataman.Dataman.metainfo.xml" "$pkg/usr/share/metainfo/"
install -m 0644 "$root/packaging/linux/dev.dataman.Dataman.xml" "$pkg/usr/share/mime/packages/"
install -m 0644 "$root/LICENSE" "$pkg/usr/share/doc/dataman/copyright"
for n in 128 256 512; do
  mkdir -p "$pkg/usr/share/icons/hicolor/${n}x${n}/apps"
  convert "$root/packaging/AppIcon.png" -resize "${n}x${n}" "$pkg/usr/share/icons/hicolor/${n}x${n}/apps/dev.dataman.Dataman.png"
done
size="$(du -sk "$pkg/usr" | cut -f1)"
cat > "$pkg/DEBIAN/control" << EOF
Package: dataman
Version: $version
Architecture: $debarch
Maintainer: brunocapri <brunocapri@users.noreply.github.com>
Installed-Size: $size
Depends: libc6 (>= 2.35), libgcc-s1, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libfontconfig1, libfreetype6
Recommends: libvulkan1 | libegl1, xdg-desktop-portal, gnome-keyring | kwalletmanager | keepassxc
Section: database
Priority: optional
Homepage: https://github.com/brunocapri/dataman
Description: Database client for PostgreSQL, MySQL, SQL Server, SQLite, DuckDB, MongoDB, Redis and search
 Dataman queries and edits PostgreSQL, MySQL, SQL Server, SQLite and DuckDB files,
 MongoDB, Redis, Elasticsearch and OpenSearch, with an MCP server (dataman-mcp) that
 lets agents use the saved connections under read-only defaults.
EOF
out="$dist/dataman_${version}_${debarch}.deb"
rm -f "$out"
dpkg-deb --build --root-owner-group -Zxz "$pkg" "$out" > /dev/null
echo "$out"
echo "  $(du -h "$out" | cut -f1)"
