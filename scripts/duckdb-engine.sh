#!/bin/bash
# Builds the DuckDB engine (engines/duckdb) into target/duckdb, the folder the tests and
# `cargo run` load it from (DATAMAN_DUCKDB, .cargo/config.toml). The C++ builds once per DuckDB
# version and target, in its own target/duckdb-engine; later runs only copy. Under
# CARGO_TARGET_DIR both go there instead.
#
#   scripts/duckdb-engine.sh              build for this machine
#   scripts/duckdb-engine.sh --package    also gzip it into dist/duckdb and print the entry
#                                         for DOWNLOADS (crates/dataman-core/src/backend/duck/engine.rs)
#   scripts/duckdb-engine.sh --api        remake backend/duck/api_v1.rs from libduckdb-sys
#
#   DATAMAN_SIGN_IDENTITY   macOS: the Developer ID the app is signed with (unset: ad hoc); the
#                           hardened runtime loads only a library signed by the app's own team
#   DATAMAN_NOTARY_PROFILE  macOS, --package: notarizes it too (scripts/notarize.sh); Gatekeeper
#                           refuses an unnotarized one put in place by hand from a browser
#   CARGO_BUILD_TARGET      another target (e.g. x86_64-pc-windows-msvc)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="$(grep -m1 '^version' engines/duckdb/Cargo.toml | cut -d'"' -f2)"
sys="$(grep -m1 'libduckdb-sys' engines/duckdb/Cargo.toml | sed -E 's/.*version = "=([^"]+)".*/\1/')"

if [ "${1:-}" = "--api" ]; then
  src="$(cargo metadata --format-version 1 --manifest-path engines/duckdb/Cargo.toml |
    python3 -c "import json,sys; print([p['manifest_path'] for p in json.load(sys.stdin)['packages'] if p['name'] == 'libduckdb-sys'][0])")"
  bindings="$(dirname "$src")/src/bindgen_bundled_version_loadable.rs"
  out=crates/dataman-core/src/backend/duck/api_v1.rs
  {
    echo "// The fields of libduckdb-sys $sys's duckdb_ext_api_v1, in order: the table the engine"
    echo "// (engines/duckdb) hands over and the struct the loader fills. Remade on a DuckDB upgrade by"
    echo "// scripts/duckdb-engine.sh --api; a list that is off fails to compile."
    echo "const LIBDUCKDB_SYS: &str = \"$sys\";"
    echo 'macro_rules! duckdb_api_v1 {'
    echo '    ($m:ident) => {'
    echo '        $m! {'
    awk '/^pub struct duckdb_ext_api_v1 \{/ {f = 1; next} f && /^\}/ {exit} f && /^    pub [a-z_0-9]+:/ {sub(/^    pub /, ""); sub(/:.*/, ""); print "            " $0 ","}' "$bindings"
    echo '        }'
    echo '    };'
    echo '}'
  } > "$out"
  echo "$out: $(grep -c '^            duckdb' "$out") functions"
  exit 0
fi

case "$(uname -s)" in
  Darwin) file=libdataman_duckdb.dylib os=macos ;;
  Linux) file=libdataman_duckdb.so os=linux ;;
  *) file=dataman_duckdb.dll os=windows ;;
esac
target="${CARGO_BUILD_TARGET:-}"
arch="${target%%-*}"
arch="${arch:-$(uname -m | sed 's/arm64/aarch64/; s/amd64/x86_64/')}"
tdir="${CARGO_TARGET_DIR:-target}"
build="$tdir/duckdb-engine"
out="$tdir/duckdb"
[ "$os" = macos ] && export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
cargo build --release --locked --manifest-path engines/duckdb/Cargo.toml --target-dir "$build" ${target:+--target "$target"}
lib="$build/${target:+$target/}release/$file"

mkdir -p "$out"
cp "$lib" "$out/$file.tmp"
if [ "$os" = macos ]; then
  identity="${DATAMAN_SIGN_IDENTITY:--}"
  if [ "$identity" = "-" ]; then ts=(--timestamp=none); else ts=(--timestamp); fi
  codesign --force --options runtime "${ts[@]}" --sign "$identity" "$out/$file.tmp"
fi
mv "$out/$file.tmp" "$out/$file"
echo "$out/$file ($(du -h "$out/$file" | cut -f1), DuckDB v$version)"

if [ "${1:-}" = "--package" ]; then
  if [ "$os" = macos ] && [ -n "${DATAMAN_NOTARY_PROFILE:-}" ]; then
    scripts/notarize.sh "$out/$file"
  fi
  mkdir -p dist/duckdb
  name="${file%.*}-v$version-$os-$arch.${file##*.}.gz"
  gzip -9 -n -c "$out/$file" > "dist/duckdb/$name"
  size="$(wc -c < "dist/duckdb/$name" | tr -d ' ')"
  sum="$( (shasum -a 256 2> /dev/null || sha256sum) < "dist/duckdb/$name" | cut -d' ' -f1)"
  echo "dist/duckdb/$name"
  cat << EOF
    Download {
        platform: "$os-$arch",
        url: "https://github.com/brunocapri/dataman/releases/download/duckdb-v$version/$name",
        size: $size,
        sha256: "$sum",
    },
EOF
fi
