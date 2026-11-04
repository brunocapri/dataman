//! DuckDB's C API as one table, in the order of duckdb_ext_api_v1's fields (the list the app's
//! loader fills that struct from), and the fingerprint of that list the loader checks first.
//! Nothing else is exported.

use std::ffi::c_void;

use libduckdb_sys as ffi;

include!("../../../crates/dataman-core/src/backend/duck/api_v1.rs");
include!("../../../crates/dataman-core/src/backend/duck/fingerprint.rs");

// The list was made from the libduckdb-sys this engine is built with (build.rs reads Cargo.lock).
const _: () = assert!(same(LIBDUCKDB_SYS, env!("DATAMAN_LIBDUCKDB_SYS")), "api_v1.rs is not this libduckdb-sys's: scripts/duckdb-engine.sh --api");

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

macro_rules! table {
    ($($n:ident),* $(,)?) => {
        const TABLE: &[*const c_void] = &[$(ffi::$n as *const c_void),*];
    };
}
duckdb_api_v1!(table);

/// Writes up to `cap` entries of the table to `out`; returns how many it has.
///
/// # Safety
/// `out` holds `cap` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dataman_duckdb_api_v1(out: *mut *const c_void, cap: usize) -> usize {
    for (i, p) in TABLE.iter().take(cap).enumerate() {
        // SAFETY: i < cap.
        unsafe { *out.add(i) = *p };
    }
    TABLE.len()
}

/// The ABI this engine was built for (fingerprint.rs): the loader reads it before the table.
#[unsafe(no_mangle)]
pub extern "C" fn dataman_duckdb_fingerprint() -> u64 {
    FINGERPRINT
}
