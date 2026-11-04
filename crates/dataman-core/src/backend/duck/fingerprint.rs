// The ABI the engine (engines/duckdb) and the loader (engine.rs) agree on: the libduckdb-sys
// version and api_v1.rs's list, in order, hashed (FNV-1a). Both include this file: an engine built
// from another list or another DuckDB has another fingerprint, and is refused before its table is
// read.
macro_rules! fingerprint_of {
    ($($n:ident),* $(,)?) => {
        const FINGERPRINT: u64 = fingerprint(LIBDUCKDB_SYS, &[$(stringify!($n)),*]);
    };
}
duckdb_api_v1!(fingerprint_of);

const fn fingerprint(sys: &str, names: &[&str]) -> u64 {
    const fn eat(mut h: u64, s: &[u8]) -> u64 {
        let mut i = 0;
        while i < s.len() {
            h ^= s[i] as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
            i += 1;
        }
        // A separator: ["ab", "c"] is not ["a", "bc"].
        h ^= 0xff;
        h.wrapping_mul(0x0100_0000_01b3)
    }
    let mut h = eat(0xcbf2_9ce4_8422_2325, sys.as_bytes());
    let mut i = 0;
    while i < names.len() {
        h = eat(h, names[i].as_bytes());
        i += 1;
    }
    h
}
