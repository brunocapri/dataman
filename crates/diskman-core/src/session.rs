//! The workspace kept between launches: the query tabs (their SQL, selection, scroll, title,
//! connection and database, a table's data view), the explorer and history panel, and each
//! tab's last results.
//!
//! On disk, in the session directory (`~/.config/diskman/session/`, next to the history):
//!
//! - `session.json`: the workspace ([`Session`]), small, written last;
//! - one `r<tab>-<generation>.bin` per tab with results: a header (the statements of the run,
//!   each result's columns, outcome, error and row counts, as JSON) and the rows, cell by
//!   cell in a tagged binary form. At most [`ROW_CAP`] rows and [`BYTE_CAP`] bytes of cells
//!   per tab; past them the result keeps the rows that fit (marked partial).
//!
//! Every file is written to a temporary name and renamed into place; result files no longer
//! named by `session.json` are removed after it is written. No passwords: a tab names its
//! connection by id. A missing, old or damaged file reads as no session (or a tab without its
//! results): it never keeps the app from starting.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::batch::Key;
use crate::dataview::TableView;
use crate::result::{CHUNK, Cell, ChunkBuilder, ColumnInfo, ColumnOrigin, ErrorInfo, NamedOrigin, Phase, ResultSet, SortDir, Status};

/// The most rows of results kept per tab.
pub const ROW_CAP: usize = 10_000;
/// The most bytes of cells kept per tab (about; checked row by row).
pub const BYTE_CAP: usize = 20 * 1024 * 1024;
/// The workspace file in the session directory.
pub const SESSION_FILE: &str = "session.json";
/// The format of `session.json` (a newer one is not read).
pub const VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"LARSRS01";

/// The workspace.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub version: u32,
    pub tabs: Vec<TabState>,
    /// The tab in front (an index into `tabs`).
    pub active: usize,
    pub explorer_open: bool,
    pub explorer_w: f32,
    /// The history panel is open on the right.
    pub history_open: bool,
    pub side_w: f32,
    /// The editor's share of the work column (`None`: the default height).
    pub editor_frac: Option<f32>,
}

/// A query tab.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabState {
    pub sql: String,
    /// The selection as byte offsets (anchor, head; equal for a cursor).
    pub selection: (usize, usize),
    /// The editor's scroll (x, y), in points at 100 %.
    pub scroll: (f32, f32),
    /// A title given with the SQL (a followed reference, a table), kept while the text is
    /// `title_sql`.
    pub title: Option<String>,
    pub title_sql: Option<String>,
    /// The connection (profile id) and database the tab was on.
    pub conn: Option<String>,
    pub database: Option<String>,
    /// A table's data view (the statement in `sql` is its page).
    pub table: Option<TableView>,
    /// The results file (in the session directory), if the tab had run something.
    pub results: Option<String>,
    /// The result tab on screen and the ones the user closed.
    pub shown: Option<Key>,
    pub closed: Vec<Key>,
}

/// How a statement of the last run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemState {
    Pending,
    Ok,
    Failed,
    Cancelled,
}

/// A statement of a tab's last run, as saved (the header of a results file).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Item {
    at: usize,
    sql: String,
    state: ItemState,
    elapsed_ms: Option<f64>,
    label: Option<String>,
    params: Vec<Option<String>>,
    result: Option<Meta>,
}

/// A result, less its rows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Meta {
    sql: String,
    columns: Vec<ColumnInfo>,
    origins: Option<Vec<ColumnOrigin>>,
    named: Option<Vec<Option<NamedOrigin>>>,
    phase: Phase,
    affected: Option<u64>,
    first_row_ms: Option<f64>,
    elapsed_ms: Option<f64>,
    error: Option<ErrorInfo>,
    partial: bool,
    /// Rows stored (in arrival order) and rows the result had when saved.
    rows: usize,
    total: usize,
    sort: Option<(usize, SortDir)>,
    params: Vec<Option<String>>,
    /// The server's messages (`PRINT`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    messages: Vec<String>,
}

/// A statement to save: its outcome and its result as it is now.
pub struct SaveItem {
    pub at: usize,
    pub sql: String,
    pub state: ItemState,
    pub elapsed: Option<Duration>,
    pub label: Option<String>,
    pub params: Vec<Option<String>>,
    pub rs: Option<Arc<ResultSet>>,
}

/// A statement read back, with its result rebuilt (finished, read-only:
/// [`ResultSet::is_restored`]).
pub struct RestoredItem {
    pub at: usize,
    pub sql: String,
    pub state: ItemState,
    pub elapsed: Option<Duration>,
    pub label: Option<String>,
    pub params: Vec<Option<String>>,
    pub rs: Option<Arc<ResultSet>>,
}

fn ms(d: Option<Duration>) -> Option<f64> {
    d.map(|d| d.as_secs_f64() * 1e3)
}

fn dur(ms: Option<f64>) -> Option<Duration> {
    ms.filter(|m| m.is_finite() && *m >= 0.).map(|m| Duration::from_secs_f64(m / 1e3))
}

const NULL: u8 = 0;
const FALSE: u8 = 1;
const TRUE: u8 = 2;
const INT: u8 = 3;
const FLOAT: u8 = 4;
const TEXT: u8 = 5;

fn put_cell(out: &mut Vec<u8>, c: Cell<'_>) {
    match c {
        Cell::Null => out.push(NULL),
        Cell::Bool(false) => out.push(FALSE),
        Cell::Bool(true) => out.push(TRUE),
        Cell::Int(v) => {
            out.push(INT);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Cell::Float(f) => {
            out.push(FLOAT);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Cell::Text(s) => {
            out.push(TEXT);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
    }
}

/// A tab's last run as a results file: every statement and the rows of its result (arrival
/// order; the sort is saved apart), within [`ROW_CAP`] rows and [`BYTE_CAP`] bytes for the
/// whole tab. A result still running or paused is saved as it is now, finished.
pub fn encode_results(items: &[SaveItem]) -> Vec<u8> {
    let (mut rows_left, mut bytes_left) = (ROW_CAP, BYTE_CAP);
    let mut data = Vec::new();
    let mut header = Vec::with_capacity(items.len());
    for it in items {
        let result = it.rs.as_ref().map(|rs| {
            let st = rs.status();
            let snap = rs.snapshot();
            let ncols = snap.columns.len();
            let total = if ncols == 0 { 0 } else { snap.source_rows() };
            let mut stored = 0;
            while stored < total && rows_left > 0 {
                let mark = data.len();
                for c in 0..ncols {
                    put_cell(&mut data, snap.source_cell(stored, c));
                }
                let used = data.len() - mark;
                if used > bytes_left {
                    data.truncate(mark);
                    bytes_left = 0;
                    break;
                }
                bytes_left -= used;
                rows_left -= 1;
                stored += 1;
            }
            let phase = match rs.phase() {
                // The stream is gone with the app: what arrived is the result.
                Phase::Running | Phase::Paused => Phase::Done,
                p => p,
            };
            Meta {
                sql: rs.sql.clone(),
                columns: snap.columns.to_vec(),
                origins: rs.origins().map(|o| o.to_vec()),
                named: rs.named_origins().map(|o| o.to_vec()),
                phase,
                affected: st.affected,
                first_row_ms: ms(st.first_row),
                elapsed_ms: ms(st.elapsed),
                error: st.error,
                partial: st.partial || (phase == Phase::Done && (stored < total || rs.phase() != Phase::Done)),
                rows: stored,
                total,
                sort: rs.sort_state().filter(|(c, _)| *c < ncols),
                params: rs.params().to_vec(),
                messages: rs.messages().0.to_vec(),
            }
        });
        header.push(Item {
            at: it.at,
            sql: it.sql.clone(),
            state: it.state,
            elapsed_ms: ms(it.elapsed),
            label: it.label.clone(),
            params: it.params.clone(),
            result,
        });
    }
    let json = serde_json::to_vec(&header).unwrap_or_default();
    let mut out = Vec::with_capacity(MAGIC.len() + 8 + json.len() + data.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(json.len() as u64).to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(&data);
    out
}

/// Reads bytes off a slice; `None` past its end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
}

/// A results file read back; `None` when it is not one or is damaged (the tab then opens
/// without its results).
pub fn decode_results(bytes: &[u8]) -> Option<Vec<RestoredItem>> {
    let mut r = Reader(bytes);
    if r.take(MAGIC.len())? != MAGIC {
        return None;
    }
    let len = usize::try_from(r.u64()?).ok()?;
    let header: Vec<Item> = serde_json::from_slice(r.take(len)?).ok()?;
    let mut out = Vec::with_capacity(header.len());
    for it in header {
        let rs = match it.result {
            None => None,
            Some(m) => {
                let ncols = m.columns.len();
                // Never more rows than bytes left (each row has at least one tag per column).
                if m.rows > 0 && (ncols == 0 || m.rows.saturating_mul(ncols) > r.0.len()) {
                    return None;
                }
                let mut chunks = Vec::new();
                let mut b = ChunkBuilder::new(&m.columns);
                for _ in 0..m.rows {
                    for c in 0..ncols {
                        match r.u8()? {
                            NULL => b.push_null(c),
                            FALSE => b.push_bool(c, false),
                            TRUE => b.push_bool(c, true),
                            INT => b.push_i64(c, i64::from_le_bytes(r.take(8)?.try_into().ok()?)),
                            FLOAT => b.push_f64(c, f64::from_bits(u64::from_le_bytes(r.take(8)?.try_into().ok()?))),
                            TEXT => {
                                let n = r.u32()? as usize;
                                b.push_text(c, std::str::from_utf8(r.take(n)?).ok()?);
                            }
                            _ => return None,
                        }
                    }
                    b.end_row();
                    if b.len() == CHUNK {
                        chunks.push(b.finish());
                    }
                }
                if !b.is_empty() {
                    chunks.push(b.finish());
                }
                let status = Status {
                    phase: Some(m.phase),
                    affected: m.affected,
                    first_row: dur(m.first_row_ms),
                    elapsed: dur(m.elapsed_ms),
                    error: m.error,
                    text_mode: false,
                    partial: m.partial,
                    ..Default::default()
                };
                let origins = m.origins.filter(|o| o.len() == ncols);
                let named = m.named.filter(|o| o.len() == ncols);
                let rs = ResultSet::restored(m.sql, m.columns, origins, named, m.params, chunks, status);
                if let Some(s) = m.sort.filter(|(c, _)| *c < ncols) {
                    rs.sort(Some(s));
                }
                for msg in m.messages {
                    rs.push_message(msg);
                }
                Some(rs)
            }
        };
        out.push(RestoredItem { at: it.at, sql: it.sql, state: it.state, elapsed: dur(it.elapsed_ms), label: it.label, params: it.params, rs });
    }
    Some(out)
}

/// The session directory: reads and writes the files above, one save at a time.
pub struct SessionStore {
    dir: PathBuf,
    /// The newest save written: an older one that finishes later is dropped.
    written: Mutex<u64>,
    next: AtomicU64,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> SessionStore {
        SessionStore { dir: dir.into(), written: Mutex::new(0), next: AtomicU64::new(1) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A number for the next save (saves apply in this order).
    pub fn ticket(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// The saved workspace; `None` (logged) when there is none or it cannot be read.
    pub fn load(&self) -> Option<Session> {
        let path = self.dir.join(SESSION_FILE);
        let text = match std::fs::read(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                log::warn!("session: cannot read {}: {e}", path.display());
                return None;
            }
        };
        match serde_json::from_slice::<Session>(&text) {
            Ok(s) if s.version == VERSION => Some(s),
            Ok(s) => {
                log::warn!("session: {} has version {}, not {VERSION}: starting fresh", path.display(), s.version);
                None
            }
            Err(e) => {
                log::warn!("session: {} is damaged ({e}): starting fresh", path.display());
                None
            }
        }
    }

    /// A tab's results; `None` (logged) when the file is missing or damaged.
    pub fn load_results(&self, name: &str) -> Option<Vec<RestoredItem>> {
        if !is_results_name(name) {
            return None;
        }
        let path = self.dir.join(name);
        let bytes = std::fs::read(&path).map_err(|e| log::warn!("session: cannot read {}: {e}", path.display())).ok()?;
        let r = decode_results(&bytes);
        if r.is_none() {
            log::warn!("session: {} is damaged: the tab opens without its results", path.display());
        }
        r
    }

    /// Writes save number `ticket`: the new results files, then `session.json`, then removes the
    /// results files it no longer names. A save older than one already written does nothing.
    pub fn save(&self, ticket: u64, session: &Session, files: &[(String, Vec<u8>)]) -> Result<()> {
        let mut written = self.written.lock();
        if ticket < *written {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        for (name, bytes) in files {
            anyhow::ensure!(is_results_name(name), "not a results file name: {name}");
            write_atomic(&self.dir, name, bytes)?;
        }
        let json = serde_json::to_vec_pretty(session)?;
        write_atomic(&self.dir, SESSION_FILE, &json)?;
        *written = ticket;
        let keep: std::collections::HashSet<&str> = session.tabs.iter().filter_map(|t| t.results.as_deref()).collect();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if is_results_name(&name) && !keep.contains(name.as_str()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(())
    }
}

/// `r<digits>-<hex>.bin`: the only names the store writes, reads or removes.
pub fn is_results_name(name: &str) -> bool {
    let Some(stem) = name.strip_prefix('r').and_then(|s| s.strip_suffix(".bin")) else { return false };
    let Some((tab, generation)) = stem.split_once('-') else { return false };
    !tab.is_empty() && tab.bytes().all(|b| b.is_ascii_digit()) && !generation.is_empty() && generation.bytes().all(|b| b.is_ascii_hexdigit())
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(".{name}.{}.{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(name)).with_context(|| format!("writing {}", dir.join(name).display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Kind;

    fn cols() -> Vec<ColumnInfo> {
        vec![
            ColumnInfo { name: "id".into(), type_name: "int8".into(), type_oid: 20, kind: Kind::Int },
            ColumnInfo { name: "name".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text },
            ColumnInfo { name: "price".into(), type_name: "float8".into(), type_oid: 701, kind: Kind::Float },
            ColumnInfo { name: "ok".into(), type_name: "bool".into(), type_oid: 16, kind: Kind::Bool },
        ]
    }

    fn result(rows: usize, text: &str) -> Arc<ResultSet> {
        let mut b = ChunkBuilder::new(&cols());
        let mut chunks = Vec::new();
        for i in 0..rows {
            b.push_i64(0, i as i64);
            if i % 7 == 3 {
                b.push_null(1);
            } else {
                b.push_text(1, &format!("{text} {i} — ünïcode"));
            }
            b.push_f64(2, i as f64 * 0.5);
            b.push_bool(3, i % 2 == 0);
            b.end_row();
            if b.len() == CHUNK {
                chunks.push(b.finish());
            }
        }
        chunks.push(b.finish());
        let status = Status { phase: Some(Phase::Done), elapsed: Some(Duration::from_millis(12)), ..Default::default() };
        ResultSet::restored("SELECT * FROM t".into(), cols(), Some(vec![ColumnOrigin { table_oid: Some(42), column_id: Some(1) }; 4]), None, Vec::new(), chunks, status)
    }

    fn failed() -> Arc<ResultSet> {
        let err = ErrorInfo { code: Some("42P01".into()), message: "relation \"nope\" does not exist".into(), position: Some(15), ..Default::default() };
        ResultSet::refused("SELECT * FROM nope", err)
    }

    fn item(at: usize, state: ItemState, rs: Option<Arc<ResultSet>>) -> SaveItem {
        SaveItem { at, sql: rs.as_ref().map_or("SET x = 1".into(), |r| r.sql.clone()), state, elapsed: Some(Duration::from_millis(3)), label: None, params: Vec::new(), rs }
    }

    #[test]
    fn results_round_trip() {
        let ok = result(2500, "row");
        ok.sort(Some((0, SortDir::Desc)));
        ok.push_message("Updated 3 rows".into());
        ok.push_message("done".into());
        let bytes = encode_results(&[item(0, ItemState::Ok, Some(ok.clone())), item(30, ItemState::Ok, None), item(40, ItemState::Failed, Some(failed()))]);
        let back = decode_results(&bytes).expect("decodes");
        assert_eq!(back.len(), 3);
        let r = back[0].rs.as_ref().unwrap();
        assert!(r.is_restored() && !ResultSet::new("SELECT 1").is_restored());
        assert_eq!((r.rows(), r.phase(), r.sql.as_str()), (2500, Phase::Done, "SELECT * FROM t"));
        assert_eq!(r.columns().unwrap().as_slice(), cols().as_slice());
        assert_eq!(r.sort_state(), Some((0, SortDir::Desc)));
        assert_eq!(r.origins().unwrap()[0].table_oid, Some(42));
        assert_eq!(r.status().elapsed, Some(Duration::from_millis(12)));
        assert!(!r.status().partial);
        let (a, b) = (ok.snapshot(), r.snapshot());
        for row in [0, 1, 3, 10, 1023, 1024, 2499] {
            for c in 0..4 {
                assert_eq!(a.text(row, c), b.text(row, c), "row {row} col {c}");
            }
        }
        assert_eq!((back[1].at, back[1].state, back[1].rs.is_none()), (30, ItemState::Ok, true));
        let e = back[2].rs.as_ref().unwrap();
        assert_eq!((e.phase(), back[2].state), (Phase::Failed, ItemState::Failed));
        let err = e.status().error.unwrap();
        assert_eq!((err.code.as_deref(), err.position), (Some("42P01"), Some(15)));
        assert_eq!(back[0].elapsed, Some(Duration::from_millis(3)));
        assert_eq!(r.messages().0.as_slice(), ["Updated 3 rows", "done"]);
        assert!(!e.has_messages());
    }

    #[test]
    fn the_row_cap_keeps_the_first_rows_and_marks_them_partial() {
        let big = result(ROW_CAP + 500, "x");
        let second = result(10, "y");
        let bytes = encode_results(&[item(0, ItemState::Ok, Some(big)), item(9, ItemState::Ok, Some(second))]);
        let back = decode_results(&bytes).unwrap();
        let r = back[0].rs.as_ref().unwrap();
        assert_eq!(r.rows(), ROW_CAP);
        assert!(r.status().partial);
        assert_eq!(r.snapshot().text(ROW_CAP - 1, 0).as_deref(), Some(&*(ROW_CAP - 1).to_string()));
        // The cap is the tab's: nothing is left for the next result.
        let s = back[1].rs.as_ref().unwrap();
        assert_eq!((s.rows(), s.status().partial, s.columns().map(|c| c.len())), (0, true, Some(4)));
        // The byte cap: a few rows of huge text.
        let huge = "z".repeat(BYTE_CAP / 3);
        let mut b = ChunkBuilder::new(&cols());
        for i in 0..5 {
            b.push_i64(0, i);
            b.push_text(1, &huge);
            b.push_f64(2, 0.);
            b.push_bool(3, true);
            b.end_row();
        }
        let rs = ResultSet::restored("SELECT".into(), cols(), None, None, Vec::new(), vec![b.finish()], Status { phase: Some(Phase::Done), ..Default::default() });
        let bytes = encode_results(&[item(0, ItemState::Ok, Some(rs))]);
        assert!(bytes.len() <= BYTE_CAP + 64 * 1024, "{}", bytes.len());
        let back = decode_results(&bytes).unwrap();
        assert_eq!(back[0].rs.as_ref().unwrap().rows(), 2);
    }

    #[test]
    fn damaged_files_read_as_nothing() {
        let bytes = encode_results(&[item(0, ItemState::Ok, Some(result(50, "a")))]);
        assert!(decode_results(&bytes).is_some());
        assert!(decode_results(&[]).is_none());
        assert!(decode_results(b"not a results file at all").is_none());
        assert!(decode_results(&bytes[..bytes.len() - 3]).is_none(), "cut short");
        let mut bad = bytes.clone();
        let n = bad.len();
        bad[n - 40] = 99; // an unknown tag, or a length past the end
        let _ = decode_results(&bad);
        let mut huge = bytes.clone();
        huge[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_results(&huge).is_none());
    }

    #[test]
    fn store_saves_atomically_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("session"));
        assert!(store.load().is_none(), "no session yet");
        let tab = |sql: &str, results: Option<&str>| TabState { sql: sql.into(), conn: Some("c1".into()), results: results.map(Into::into), shown: Some(Key::Stmt(0)), ..Default::default() };
        let s = Session { version: VERSION, tabs: vec![tab("SELECT 1;", Some("r1-a1.bin")), tab("SELECT 2;", None)], active: 1, explorer_open: true, explorer_w: 260., ..Default::default() };
        let bytes = encode_results(&[item(0, ItemState::Ok, Some(result(3, "q")))]);
        store.save(store.ticket(), &s, &[("r1-a1.bin".into(), bytes)]).unwrap();
        let back = store.load().unwrap();
        assert_eq!(back, s);
        assert_eq!(store.load_results("r1-a1.bin").unwrap()[0].rs.as_ref().unwrap().rows(), 3);
        // Next save: tab 1 has a new generation; the old file goes, nothing temporary stays.
        let mut s2 = s.clone();
        s2.tabs[0].results = Some("r1-b2.bin".into());
        let t_old = store.ticket();
        let t_new = store.ticket();
        store.save(t_new, &s2, &[("r1-b2.bin".into(), encode_results(&[item(0, ItemState::Ok, Some(result(4, "q")))]))]).unwrap();
        let names: Vec<String> = std::fs::read_dir(store.dir()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"r1-b2.bin".to_string()) && !names.contains(&"r1-a1.bin".to_string()), "{names:?}");
        assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");
        // An older save finishing late does not undo the newer one.
        store.save(t_old, &s, &[]).unwrap();
        assert_eq!(store.load().unwrap().tabs[0].results.as_deref(), Some("r1-b2.bin"));
        // Damaged, foreign or newer files: no session, no panic.
        for text in ["{ not json", "[]", "{\"version\": 99}"] {
            std::fs::write(store.dir().join(SESSION_FILE), text).unwrap();
            assert!(store.load().is_none(), "{text}");
        }
        std::fs::write(store.dir().join("r1-b2.bin"), b"garbage").unwrap();
        assert!(store.load_results("r1-b2.bin").is_none());
        assert!(store.load_results("../settings.toml").is_none());
        assert!(!is_results_name("settings.toml") && !is_results_name("r-1.bin") && is_results_name("r12-ff0a.bin"));
        // Unknown keys are ignored, missing ones take their defaults.
        std::fs::write(store.dir().join(SESSION_FILE), "{\"version\": 1, \"tabs\": [{\"sql\": \"SELECT 3\", \"future\": true}]}").unwrap();
        let s = store.load().unwrap();
        assert_eq!((s.tabs.len(), s.tabs[0].sql.as_str(), s.tabs[0].conn.as_deref()), (1, "SELECT 3", None));
    }
}
