fn main() {
    // The libduckdb-sys it is built with, for lib.rs to check api_v1.rs against.
    let lock = std::fs::read_to_string("Cargo.lock").expect("Cargo.lock");
    let at = lock.find("name = \"libduckdb-sys\"").expect("libduckdb-sys in Cargo.lock");
    let version = lock[at..].lines().nth(1).and_then(|l| l.strip_prefix("version = \"")).and_then(|v| v.strip_suffix('"')).expect("its version");
    println!("cargo:rustc-env=DATAMAN_LIBDUCKDB_SYS={version}");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=../../crates/dataman-core/src/backend/duck/api_v1.rs");
    println!("cargo:rerun-if-changed=../../crates/dataman-core/src/backend/duck/fingerprint.rs");
    // No build machine's path in the library: the app loads it by its own path.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libdataman_duckdb.dylib");
    }
}
