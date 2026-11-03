fn main() {
    // No build machine's path in the library: the app loads it by its own path.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libdataman_duckdb.dylib");
    }
}
