//! DuckDB is not compiled into Dataman: its C API comes from the engine, a library built apart
//! (`engines/duckdb`, from the same DuckDB sources, with json and parquet) and loaded the first
//! time a DuckDB database is opened. The `duckdb` crate is built as for a loadable extension,
//! every C call going through a table of pointers; the engine hands over that table.
//!
//! The engine is looked for in `DATAMAN_DUCKDB` (a folder: the tests, a build of one's own),
//! beside the executable (the Linux and Windows packages ship it), then in the engines folder
//! (the Mac app downloads it); missing there,
//! it is downloaded into it from the release whose SHA-256 is pinned below, so a file that
//! differs is never loaded. On macOS the engine is signed with the app's own Developer ID: the
//! hardened runtime's library validation loads nothing else.

use std::ffi::{CStr, c_char, c_void};
use std::io::Read as _;
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

/// An engine to download: gzipped, `size` and `sha256` those of the .gz.
pub(crate) struct Download {
    pub platform: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// Made by `scripts/duckdb-engine.sh --package`, published under the `duckdb-v1.5.6` release (a
/// prerelease: the feed's `releases/latest` stays the app's). Linux and Windows ship the engine.
const DOWNLOADS: &[Download] = &[Download {
    platform: "macos-aarch64",
    url: "https://github.com/brunocapri/dataman/releases/download/duckdb-v1.5.6/libdataman_duckdb-v1.5.6-macos-aarch64.dylib.gz",
    size: 11435547,
    sha256: "f0d9255c8afc3c2b06fced0607ef7b01a9069365b6cf5e2723e0a820ffd19a60",
}];

type Table = unsafe extern "C" fn(out: *mut *const c_void, cap: usize) -> usize;
type Fingerprint = unsafe extern "C" fn() -> u64;

macro_rules! api {
    ($($n:ident),* $(,)?) => {
        const COUNT: usize = [$(stringify!($n)),*].len();

        /// The struct, from the engine's table in the list's order.
        unsafe fn api(t: &[*const c_void; COUNT]) -> ffi::duckdb_ext_api_v1 {
            let mut i = 0;
            // SAFETY: each entry is the engine's own function of that name, the field's type.
            ffi::duckdb_ext_api_v1 { $($n: { i += 1; unsafe { std::mem::transmute::<*const c_void, _>(t[i - 1]) } },)* }
        }
    };
}
duckdb_api_v1!(api);

/// Where the engine came from, once loaded.
static LOADED: Mutex<Option<PathBuf>> = parking_lot::const_mutex(None);

/// The engine, loaded now (found, or downloaded first) or already; the next connection tries
/// again after a failure. Blocking: a download takes a while.
pub(crate) fn load() -> Result<()> {
    let mut g = LOADED.lock();
    if g.is_some() {
        return Ok(());
    }
    let path = match places().into_iter().map(|d| d.join(FILE)).find(|p| p.is_file()) {
        Some(p) => p,
        None => {
            let dir = dir().context("no folder to download DuckDB into")?;
            let d = DOWNLOADS.iter().find(|d| d.platform == crate::update::platform()).ok_or_else(|| {
                anyhow!("DuckDB is not part of this Dataman and has no download for {}: put {FILE} in {}", crate::update::platform(), dir.display())
            })?;
            log::info!("DuckDB {VERSION}: downloading {}", d.url);
            fetch(d, &dir).context("downloading DuckDB")?
        }
    };
    open(&path)?;
    log::info!("DuckDB {VERSION} from {}", path.display());
    *g = Some(path);
    Ok(())
}

/// The folders looked in, in order; the last one is where a download goes.
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
    v.extend(dir());
    v
}

/// The engines folder: per user and machine, and per DuckDB version and ABI (an engine of
/// another build is never found there, so a new one is downloaded beside it).
fn dir() -> Option<PathBuf> {
    let base = if crate::os::WINDOWS {
        crate::os::local_app_data()?.join("dataman")
    } else if crate::os::LINUX {
        crate::os::data_home()?.join("dataman")
    } else {
        crate::os::home()?.join("Library/Application Support/Dataman")
    };
    Some(base.join("engines").join(format!("duckdb-{VERSION}-{FINGERPRINT:016x}")))
}

/// `d` downloaded into `dir`, checked and unpacked: the engine's path.
fn fetch(d: &Download, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Per process: the app and its MCP server may both be downloading.
    let gz = dir.join(format!("{FILE}.gz.{}.part", std::process::id()));
    let r = crate::http::download(d.url, d.size, &gz, &|_| {}).and_then(|()| install(&gz, d, dir));
    let _ = std::fs::remove_file(&gz);
    r
}

fn install(gz: &Path, d: &Download, dir: &Path) -> Result<PathBuf> {
    let bytes = std::fs::read(gz)?;
    let sum = data_encoding::HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref());
    if bytes.len() as u64 != d.size || sum != d.sha256 {
        bail!("the download is not the published one ({} bytes, SHA-256 {sum})", bytes.len());
    }
    let mut lib = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut lib).context("unpacking")?;
    let part = dir.join(format!("{FILE}.{}.part", std::process::id()));
    std::fs::write(&part, &lib)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;
    }
    let path = dir.join(FILE);
    std::fs::rename(&part, &path)?;
    Ok(path)
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
    let fp = unsafe { lib.get::<Fingerprint>(b"dataman_duckdb_fingerprint\0") }.map_err(|_| anyhow!("{}: it is an older build, with no ABI fingerprint (the engine ships beside Dataman; rebuild it with scripts/duckdb-engine.sh)", what()))?;
    // SAFETY: as above.
    let fp = unsafe { fp() };
    if fp != FINGERPRINT {
        bail!("{}: it was built for another C API ({fp:016x}, not {FINGERPRINT:016x}); rebuild it with scripts/duckdb-engine.sh", what());
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

    fn gz(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        data_encoding::HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
    }

    #[test]
    fn a_download_is_installed_only_as_published() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("engine.gz");
        let packed = gz(b"the engine");
        std::fs::write(&src, &packed).unwrap();
        let url: &'static str = format!("file://{}", src.display()).leak();
        let into = dir.path().join("engines");

        let sum: &'static str = sha(b"something else").leak();
        let wrong = Download { platform: "test", url, size: packed.len() as u64, sha256: sum };
        let e = fetch(&wrong, &into).unwrap_err();
        assert!(format!("{e:#}").contains("not the published one"), "{e:#}");
        assert_eq!(std::fs::read_dir(&into).unwrap().count(), 0, "nothing left behind");

        let sum: &'static str = sha(&packed).leak();
        let right = Download { platform: "test", url, size: packed.len() as u64, sha256: sum };
        let path = fetch(&right, &into).unwrap();
        assert_eq!(path, into.join(FILE));
        assert_eq!(std::fs::read(&path).unwrap(), b"the engine");
        assert_eq!(std::fs::read_dir(&into).unwrap().count(), 1, "only the engine");
    }

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

    #[test]
    fn every_platform_has_its_own_download() {
        let mut seen: Vec<&str> = DOWNLOADS.iter().map(|d| d.platform).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), DOWNLOADS.len());
        for d in DOWNLOADS {
            assert!(d.url.starts_with("https://") && d.url.ends_with(".gz") && d.sha256.len() == 64, "{}", d.url);
        }
    }
}
