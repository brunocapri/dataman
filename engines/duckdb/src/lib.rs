//! DuckDB's C API as one table, in the order of duckdb_ext_api_v1's fields (the list the app's
//! loader fills that struct from). Nothing else is exported.

use std::ffi::c_void;

use libduckdb_sys as ffi;

include!("../../../crates/dataman-core/src/backend/duck/api_v1.rs");

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
