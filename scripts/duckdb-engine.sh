#!/bin/bash
# Builds the DuckDB engine (engines/duckdb) into target/duckdb, the folder the tests and
# `cargo run` load it from (DATAMAN_DUCKDB, .cargo/config.toml). The C++ builds once per DuckDB
# version and target, in its own target/duckdb-engine; later runs only copy. Under
# CARGO_TARGET_DIR both go there instead.
#
# The packages carry it beside the binary: bundle.sh (Contents/Frameworks, signed with the app),
# scripts/linux/build.sh, scripts/windows/package.ps1.
#
#   scripts/duckdb-engine.sh              build for this machine
#   scripts/duckdb-engine.sh --api        remake backend/duck/api_v1.rs from libduckdb-sys
#
#   CARGO_BUILD_TARGET      another target (e.g. x86_64-pc-windows-msvc)
#   DATAMAN_DUCKDB_CACHED=true   the engine in target/duckdb is current (CI restored it from
#                           its cache, keyed on engines/duckdb and this script): no build
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
tdir="${CARGO_TARGET_DIR:-target}"
build="$tdir/duckdb-engine"
out="$tdir/duckdb"
if [ "${DATAMAN_DUCKDB_CACHED:-}" = true ] && [ -f "$out/$file" ]; then
  echo "$out/$file ($(du -h "$out/$file" | cut -f1), DuckDB v$version, cached)"
  exit 0
fi
[ "$os" = macos ] && export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
# On Linux libstdc++ goes in statically: the AppImage links nothing outside its allowlist (the
# `cc` crate would link it dynamically), and the tests load the engine the packages carry.
if [ "$os" = linux ]; then
  export CXXSTDLIB=""
  export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-Bstatic -C link-arg=-lstdc++ -C link-arg=-Wl,-Bdynamic"
fi
cargo build --release --locked --manifest-path engines/duckdb/Cargo.toml --target-dir "$build" ${target:+--target "$target"}
lib="$build/${target:+$target/}release/$file"

mkdir -p "$out"
cp "$lib" "$out/$file.tmp"
# Ad hoc: bundle.sh signs it again with the app's identity.
[ "$os" = macos ] && codesign --force --options runtime --timestamp=none --sign - "$out/$file.tmp"
mv "$out/$file.tmp" "$out/$file"
echo "$out/$file ($(du -h "$out/$file" | cut -f1), DuckDB v$version)"

