//! DuckDB is not compiled into Dataman: its C API comes from the engine, a library built apart
//! (`engines/duckdb`, from the same DuckDB sources, with json and parquet) and loaded the first
//! time a DuckDB database is opened. The `duckdb` crate is built as for a loadable extension,
//! every C call going through a table of pointers; the engine hands over that table.
//!
//! The engine is looked for in `DATAMAN_DUCKDB` (a folder: the tests, a build of one's own), then
//! beside the executable, where every package ships it. On macOS it is signed with the app's own
//! Developer ID: the hardened runtime's library validation loads nothing else.

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use duckdb::ffi;
use parking_lot::Mutex;

include!("api_v1.rs");
include!("fingerprint.rs");

/// The DuckDB the bindings were made from: the engine must be this one.
pub const VERSION: &str = "v1.5.6";

/// A folder that holds the engine, looked in first.
pub const ENV: &str = "DATAMAN_DUCKDB";

/// The engine's file name.
pub const FILE: &str = if cfg!(target_os = "macos") {
    "libdataman_duckdb.dylib"
} else if cfg!(windows) {
    "dataman_duckdb.dll"
} else {
    "libdataman_duckdb.so"
};

type Table = unsafe extern "C" fn(out: *mut *const c_void, cap: usize) -> usize;
type Fingerprint = unsafe extern "C" fn() -> u64;

macro_rules! api {
    ($($n:ident),* $(,)?) => {
        const COUNT: usize = [$(stringify!($n)),*].len();

        /// The struct, from the engine's table in the list's order.
        #[allow(clippy::missing_transmute_annotations)] // The target is each field's type, which the macro does not name.
        unsafe fn api(t: &[*const c_void; COUNT]) -> ffi::duckdb_ext_api_v1 {
            let mut i = 0;
            // SAFETY: each entry is the engine's own function of that name, the field's type.
            ffi::duckdb_ext_api_v1 { $($n: { i += 1; unsafe { std::mem::transmute::<*const c_void, _>(t[i - 1]) } },)* }
        }
    };
}
duckdb_api_v1!(api);

/// What to do about an engine that is not this one.
const REBUILD: &str = "reinstall Dataman; a build of one's own: scripts/duckdb-engine.sh";

/// Where the engine came from, once loaded.
static LOADED: Mutex<Option<PathBuf>> = parking_lot::const_mutex(None);

/// The engine, loaded now or already; the next connection tries again after a failure.
pub(crate) fn load() -> Result<()> {
    let mut g = LOADED.lock();
    if g.is_some() {
        return Ok(());
    }
    let path = places().into_iter().map(|d| d.join(FILE)).find(|p| p.is_file()).ok_or_else(|| {
        anyhow!("DuckDB's engine ({FILE}) is missing: it ships beside Dataman (a build of one's own: scripts/duckdb-engine.sh)")
    })?;
    open(&path)?;
    log::info!("DuckDB {VERSION} from {}", path.display());
    *g = Some(path);
    Ok(())
}

/// The folders looked in, in order.
fn places() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var_os(ENV).filter(|d| !d.is_empty()).map(PathBuf::from).into_iter().collect();
    if let Some(exe) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        #[cfg(target_os = "macos")]
        v.push(exe.join("../Frameworks"));
        // The AppImage's and the .deb's usr/lib/dataman.
        #[cfg(target_os = "linux")]
        v.push(exe.join("../lib/dataman"));
        v.push(exe);
    }
    v
}

/// `info` is the struct itself: no other extension info is asked for.
unsafe extern "C" fn get_api(info: ffi::duckdb_extension_info, _version: *const c_char) -> *const c_void {
    info as *const c_void
}

/// The engine at `path` loaded for the life of the process, its C API the one every call takes.
fn open(path: &Path) -> Result<()> {
    let what = || format!("{} is not this Dataman's DuckDB {VERSION}", path.display());
    // SAFETY: the engine runs no code of its own when loaded (no constructors but DuckDB's).
    let lib = unsafe { libloading::Library::new(path) }.with_context(|| format!("loading DuckDB from {}", path.display()))?;
    // Before anything of its table is read: the engine was built from this list and this
    // libduckdb-sys (an older one, in another order, would be called at the wrong functions).
    // SAFETY: the engine's export, with this signature (engines/duckdb/src/lib.rs).
    let fp = unsafe { lib.get::<Fingerprint>(b"dataman_duckdb_fingerprint\0") }.map_err(|_| anyhow!("{}: it is an older build, with no ABI fingerprint ({REBUILD})", what()))?;
    // SAFETY: as above.
    let fp = unsafe { fp() };
    if fp != FINGERPRINT {
        bail!("{}: it was built for another C API ({fp:016x}, not {FINGERPRINT:016x}; {REBUILD})", what());
    }
    let mut t = [std::ptr::null(); COUNT];
    // SAFETY: the engine's export, with this signature (engines/duckdb/src/lib.rs).
    let n = unsafe { lib.get::<Table>(b"dataman_duckdb_api_v1\0").with_context(what)?(t.as_mut_ptr(), COUNT) };
    if n != COUNT || t.iter().any(|p| p.is_null()) {
        bail!("{}: its C API has {n} functions, not {COUNT}", what());
    }
    // SAFETY: the table is complete and in the list's order.
    let api = unsafe { api(&t) };
    // SAFETY: a static string.
    let version = unsafe { CStr::from_ptr(api.duckdb_library_version.context("no duckdb_library_version")?()) }.to_string_lossy().into_owned();
    if version != VERSION {
        bail!("{}: it is DuckDB {version}", what());
    }
    let access = ffi::duckdb_extension_access { set_error: None, get_database: None, get_api: Some(get_api) };
    // SAFETY: get_api returns `api`, alive for the call; the pointers are copied out of it.
    let ok = unsafe { ffi::duckdb_rs_extension_api_init(&api as *const ffi::duckdb_ext_api_v1 as ffi::duckdb_extension_info, &access, VERSION) }.map_err(|e| anyhow!("{}: {e}", what()))?;
    if !ok {
        bail!(what());
    }
    // Its functions are called until the process ends.
    std::mem::forget(lib);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_library_that_is_not_the_engine_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, b"not a library").unwrap();
        let e = open(&path).unwrap_err();
        assert!(format!("{e:#}").contains("loading DuckDB"), "{e:#}");
    }

    #[test]
    fn the_api_list_is_this_libduckdb_sys() {
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")).unwrap();
        let at = lock.find("name = \"libduckdb-sys\"").unwrap();
        assert_eq!(lock[at..].lines().nth(1), Some(format!("version = \"{LIBDUCKDB_SYS}\"").as_str()), "api_v1.rs: scripts/duckdb-engine.sh --api");
        assert_ne!(FINGERPRINT, fingerprint(LIBDUCKDB_SYS, &["duckdb_open"]));
        assert_ne!(fingerprint("1", &["ab", "c"]), fingerprint("1", &["a", "bc"]));
    }
}
