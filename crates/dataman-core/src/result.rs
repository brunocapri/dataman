//! A query result, filled in batches by the core and read by the UI.
//!
//! Rows live in immutable, columnar chunks. The fetch task appends chunks; readers take a
//! [`Snapshot`] (an `Arc` clone, no lock held while drawing) and only format the cells they
//! show. Fetching is driven by demand: the fetch task pauses once it is `PREFETCH` rows ahead of
//! the last row the UI asked for, keeping the server-side stream open.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::sync::watch;

use crate::value::{self, Kind, Storage};

/// Rows per chunk.
pub const CHUNK: usize = 1024;
/// How far past the last requested row the fetcher reads before pausing.
pub const PREFETCH: usize = 10_000;

static PREFETCH_ROWS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(PREFETCH);

/// Rows fetched past what is requested (the Settings sheet changes it; new demands use it).
pub fn prefetch() -> usize {
    PREFETCH_ROWS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sets [`prefetch`] (clamped to 1,024 – 1,000,000).
pub fn set_prefetch(rows: usize) {
    PREFETCH_ROWS.store(rows.clamp(CHUNK, 1_000_000), std::sync::atomic::Ordering::Relaxed);
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub type_oid: u32,
    pub kind: Kind,
}

#[derive(Debug)]
pub enum ColData {
    Bool(Vec<bool>),
    I64(Vec<i64>),
    F64(Vec<f64>),
    /// All values of the chunk in one string; `ends[i]` is where value `i` ends.
    Text { text: String, ends: Vec<u32> },
}

#[derive(Debug)]
pub struct ColChunk {
    /// Bit `i` set = row `i` is NULL.
    pub nulls: Vec<u64>,
    pub data: ColData,
}

#[derive(Debug)]
pub struct Chunk {
    pub rows: usize,
    pub cols: Vec<ColChunk>,
}

impl Chunk {
    /// Cell `row` (0-based within the chunk) of column `col`.
    pub fn cell(&self, row: usize, col: usize) -> Cell<'_> {
        self.cols[col].get(row)
    }
}

/// Where a result column comes from, as the server's RowDescription says (PostgreSQL fills
/// these for plain table columns; computed columns and servers that do not say leave `None`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ColumnOrigin {
    pub table_oid: Option<u32>,
    /// The column's `attnum` in that table.
    pub column_id: Option<i16>,
}

/// The table column a result column shows, by name (MySQL reports it in each column's
/// definition: database, original table and original column).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct NamedOrigin {
    pub schema: String,
    pub table: String,
    pub column: String,
}

/// A borrowed cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cell<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(&'a str),
}

impl ColChunk {
    fn is_null(&self, i: usize) -> bool {
        self.nulls[i / 64] >> (i % 64) & 1 == 1
    }

    fn get(&self, i: usize) -> Cell<'_> {
        if self.is_null(i) {
            return Cell::Null;
        }
        match &self.data {
            ColData::Bool(v) => Cell::Bool(v[i]),
            ColData::I64(v) => Cell::Int(v[i]),
            ColData::F64(v) => Cell::Float(v[i]),
            ColData::Text { text, ends } => {
                let s = if i == 0 { 0 } else { ends[i - 1] as usize };
                Cell::Text(&text[s..ends[i] as usize])
            }
        }
    }
}

/// Builds one chunk row by row.
pub struct ChunkBuilder {
    kinds: Vec<Kind>,
    rows: usize,
    cols: Vec<ColChunk>,
}

impl ChunkBuilder {
    pub fn new(columns: &[ColumnInfo]) -> Self {
        let kinds: Vec<Kind> = columns.iter().map(|c| c.kind).collect();
        let cols = kinds.iter().map(|k| Self::empty_col(*k)).collect();
        ChunkBuilder { kinds, rows: 0, cols }
    }

    fn empty_col(k: Kind) -> ColChunk {
        let data = match k.storage() {
            Storage::Bool => ColData::Bool(Vec::with_capacity(CHUNK)),
            Storage::I64 => ColData::I64(Vec::with_capacity(CHUNK)),
            Storage::F64 => ColData::F64(Vec::with_capacity(CHUNK)),
            Storage::Text => ColData::Text { text: String::new(), ends: Vec::with_capacity(CHUNK) },
        };
        ColChunk { nulls: Vec::with_capacity(CHUNK / 64), data }
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn kind(&self, col: usize) -> Kind {
        self.kinds[col]
    }

    fn mark(&mut self, col: usize, null: bool) {
        let c = &mut self.cols[col];
        let r = self.rows;
        if r % 64 == 0 {
            c.nulls.push(0);
        }
        if null {
            *c.nulls.last_mut().unwrap() |= 1 << (r % 64);
        }
    }

    pub fn push_null(&mut self, col: usize) {
        self.mark(col, true);
        match &mut self.cols[col].data {
            ColData::Bool(v) => v.push(false),
            ColData::I64(v) => v.push(0),
            ColData::F64(v) => v.push(0.0),
            ColData::Text { text, ends } => ends.push(text.len() as u32),
        }
    }

    pub fn push_bool(&mut self, col: usize, x: bool) {
        self.mark(col, false);
        match &mut self.cols[col].data {
            ColData::Bool(v) => v.push(x),
            _ => self.push_text_fallback(col, if x { "true" } else { "false" }),
        }
    }

    pub fn push_i64(&mut self, col: usize, x: i64) {
        self.mark(col, false);
        match &mut self.cols[col].data {
            ColData::I64(v) => v.push(x),
            _ => self.push_text_fallback(col, &x.to_string()),
        }
    }

    pub fn push_f64(&mut self, col: usize, x: f64) {
        self.mark(col, false);
        match &mut self.cols[col].data {
            ColData::F64(v) => v.push(x),
            _ => self.push_text_fallback(col, &x.to_string()),
        }
    }

    /// Appends a text value; `f` writes it into the chunk's string directly.
    pub fn push_text_with(&mut self, col: usize, f: impl FnOnce(&mut String)) {
        self.mark(col, false);
        match &mut self.cols[col].data {
            ColData::Text { text, ends } => {
                f(text);
                ends.push(text.len() as u32);
            }
            // Only reachable if a text value lands in a native column: store a default.
            ColData::Bool(v) => v.push(false),
            ColData::I64(v) => v.push(0),
            ColData::F64(v) => v.push(0.0),
        }
    }

    pub fn push_text(&mut self, col: usize, s: &str) {
        self.push_text_with(col, |t| t.push_str(s));
    }

    fn push_text_fallback(&mut self, col: usize, s: &str) {
        if let ColData::Text { text, ends } = &mut self.cols[col].data {
            text.push_str(s);
            ends.push(text.len() as u32);
        }
    }

    /// Call after pushing one value into every column.
    pub fn end_row(&mut self) {
        self.rows += 1;
    }

    pub fn finish(&mut self) -> Chunk {
        let cols = self.kinds.iter().map(|k| Self::empty_col(*k)).collect();
        let cols = std::mem::replace(&mut self.cols, cols);
        let rows = std::mem::take(&mut self.rows);
        Chunk { rows, cols }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Phase {
    /// Waiting for the server (prepare, first rows).
    Running,
    /// Enough rows loaded for now; the stream is open and more can be fetched.
    Paused,
    Done,
    Cancelled,
    Failed,
}

impl Phase {
    pub fn is_finished(self) -> bool {
        matches!(self, Phase::Done | Phase::Cancelled | Phase::Failed)
    }
}

/// A server error, split into its parts.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ErrorInfo {
    pub code: Option<String>,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based character position in the statement, when the server reports one.
    pub position: Option<u32>,
    /// `ERROR`, `FATAL`…, as the server says it.
    pub severity: Option<String>,
}

impl std::fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(c) = &self.code {
            write!(f, "[{c}] ")?;
        }
        f.write_str(&self.message)?;
        if let Some(d) = &self.detail {
            write!(f, "\nDETAIL: {d}")?;
        }
        if let Some(h) = &self.hint {
            write!(f, "\nHINT: {h}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub phase: Option<Phase>,
    /// `INSERT 0 5` and friends: rows affected by a statement without result rows.
    pub affected: Option<u64>,
    pub first_row: Option<Duration>,
    pub elapsed: Option<Duration>,
    pub error: Option<ErrorInfo>,
    /// The statement was run through the simple protocol (text results).
    pub text_mode: bool,
    /// Only the rows loaded before [`ResultSet::stop_fetching`] were kept; the server finished
    /// the statement and the rest was read and dropped.
    pub partial: bool,
    /// Time spent [`Phase::Paused`] (waiting for the reader), not counted as running time.
    pub paused_for: Duration,
    /// Since when it is paused, while it is.
    pub paused_since: Option<std::time::Instant>,
}

impl Status {
    /// All the time spent paused, the current pause included.
    pub fn paused_time(&self) -> Duration {
        self.paused_for + self.paused_since.map_or(Duration::ZERO, |p| p.elapsed())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Demand {
    /// Keep at least this many rows loaded.
    pub rows: usize,
    pub all: bool,
    pub abort: bool,
}

#[derive(Default)]
struct Chunks {
    list: Vec<Arc<Chunk>>,
    /// `starts[i]` = global index of the first row of chunk `i`.
    starts: Vec<usize>,
    rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortDir {
    Asc,
    Desc,
}

pub struct ResultSet {
    pub sql: String,
    columns: OnceLock<Arc<Vec<ColumnInfo>>>,
    origins: OnceLock<Arc<Vec<ColumnOrigin>>>,
    named_origins: OnceLock<Arc<Vec<Option<NamedOrigin>>>>,
    chunks: RwLock<Arc<Chunks>>,
    order: RwLock<Option<(usize, SortDir, Arc<Vec<u32>>)>>,
    status: Mutex<Status>,
    demand: watch::Sender<Demand>,
    /// Bumped on every change readers may care about (rows, phase, order).
    version: watch::Sender<u64>,
    /// Written to the history already (at the first pause, or at the end).
    pub(crate) recorded: std::sync::atomic::AtomicBool,
    /// The statement reached the server for execution (a CancelRequest can only matter then).
    pub(crate) executing: std::sync::atomic::AtomicBool,
    /// Values bound to `$1 … $n` (text format; `None` is NULL).
    params: OnceLock<Vec<Option<String>>>,
    /// Runs inside a transaction (or a savepoint) that is rolled back: what it changes is undone
    /// (Explain Analyze of a write).
    pub(crate) rollback: std::sync::atomic::AtomicBool,
    /// That transaction is a read-only one (an agent's read in its tab).
    pub(crate) read_only: std::sync::atomic::AtomicBool,
    /// Run for an agent (its client's name): the history says so.
    agent: OnceLock<String>,
    /// The result set the same statement returned after this one (a T-SQL batch's): set before
    /// this one finishes.
    next: OnceLock<Arc<ResultSet>>,
    /// Read back from the saved session (`crate::session`): rows as they were, no stream
    /// behind them, never edited in place.
    restored: bool,
    /// What the server printed while this result was the statement's latest (SQL Server's
    /// `PRINT`), in order, up to [`MESSAGE_CAP`]; and how many more came.
    messages: Mutex<(Arc<Vec<String>>, usize)>,
    /// The planner's estimate of the statement's rows (asked once, when it first pauses:
    /// [`ResultSet::ask_estimate`]); `None` inside when there is none.
    estimate: OnceLock<Option<u64>>,
    estimate_asked: std::sync::atomic::AtomicBool,
}

/// Server messages a result keeps (a `PRINT` in a loop must not fill the memory).
pub const MESSAGE_CAP: usize = 10_000;

/// An immutable view of the rows loaded so far (and of the sort order at that moment).
#[derive(Clone)]
pub struct Snapshot {
    pub columns: Arc<Vec<ColumnInfo>>,
    chunks: Arc<Chunks>,
    order: Option<Arc<Vec<u32>>>,
}

impl Snapshot {
    pub fn rows(&self) -> usize {
        match &self.order {
            // Rows that arrived after the sort are not in the permutation; show them after it.
            Some(o) => self.chunks.rows.max(o.len()),
            None => self.chunks.rows,
        }
    }

    /// The storage row behind a display row.
    pub fn source_row(&self, row: usize) -> usize {
        match &self.order {
            Some(o) if row < o.len() => o[row] as usize,
            _ => row,
        }
    }

    pub(crate) fn locate(&self, row: usize) -> (&Chunk, usize) {
        let c = &self.chunks;
        let i = match c.starts.binary_search(&row) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        (&c.list[i], row - c.starts[i])
    }

    /// Cell at display row `row`, column `col`.
    pub fn cell(&self, row: usize, col: usize) -> Cell<'_> {
        let (chunk, i) = self.locate(self.source_row(row));
        chunk.cols[col].get(i)
    }

    /// Cell at storage row `row` (arrival order, ignoring any sort).
    pub fn source_cell(&self, row: usize, col: usize) -> Cell<'_> {
        let (chunk, i) = self.locate(row);
        chunk.cols[col].get(i)
    }

    /// Rows loaded, in arrival order (the sort permutation may lag behind them).
    pub fn source_rows(&self) -> usize {
        self.chunks.rows
    }

    /// Display text (`None` = NULL).
    pub fn text(&self, row: usize, col: usize) -> Option<Cow<'_, str>> {
        format_cell(self.columns[col].kind, self.cell(row, col))
    }
}

pub fn format_cell(kind: Kind, cell: Cell<'_>) -> Option<Cow<'_, str>> {
    Some(match cell {
        Cell::Null => return None,
        Cell::Bool(b) => Cow::Borrowed(if b { "true" } else { "false" }),
        Cell::Text(s) => Cow::Borrowed(s),
        Cell::Float(_) | Cell::Int(_) => {
            let mut s = String::new();
            format_cell_into(kind, cell, &mut s);
            Cow::Owned(s)
        }
    })
}

/// [`format_cell`] written into `out` (cleared first), for loops over many cells that reuse one
/// buffer; false for NULL.
pub fn format_cell_into(kind: Kind, cell: Cell<'_>, out: &mut String) -> bool {
    out.clear();
    match cell {
        Cell::Null => return false,
        Cell::Bool(b) => out.push_str(if b { "true" } else { "false" }),
        Cell::Text(s) => out.push_str(s),
        Cell::Float(f) => value::format_float(f, kind == Kind::Float4, out),
        Cell::Int(v) => match kind {
            Kind::Date | Kind::Timestamp if value::is_odd_date(v) => value::format_odd_date(v, kind == Kind::Timestamp, out),
            Kind::Date => value::format_date(v as i32, out),
            Kind::Time => value::format_time(v, out),
            Kind::Timestamp => value::format_timestamp(v, out),
            Kind::TimestampTz => value::format_timestamptz(v, out),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "{v}");
            }
        },
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFormat {
    /// Tab-separated; NULL as `\N`, like `COPY … (FORMAT text)`.
    Tsv,
    /// RFC 4180; NULL unquoted and empty, the empty string as `""`, like `COPY … (FORMAT csv)`.
    Csv,
}

impl ResultSet {
    pub fn new(sql: impl Into<String>) -> Arc<Self> {
        Arc::new(ResultSet {
            sql: sql.into(),
            columns: OnceLock::new(),
            origins: OnceLock::new(),
            named_origins: OnceLock::new(),
            chunks: RwLock::new(Arc::new(Chunks::default())),
            order: RwLock::new(None),
            status: Mutex::new(Status { phase: Some(Phase::Running), ..Default::default() }),
            demand: watch::Sender::new(Demand { rows: prefetch(), all: false, abort: false }),
            version: watch::Sender::new(0),
            recorded: Default::default(),
            executing: Default::default(),
            params: OnceLock::new(),
            rollback: Default::default(),
            read_only: Default::default(),
            agent: OnceLock::new(),
            next: OnceLock::new(),
            restored: false,
            messages: Mutex::new((Arc::new(Vec::new()), 0)),
            estimate: OnceLock::new(),
            estimate_asked: Default::default(),
        })
    }

    /// The planner's estimate of the rows the statement returns, once known.
    pub fn row_estimate(&self) -> Option<u64> {
        self.estimate.get().copied().flatten()
    }

    /// The estimate was answered (with a number or without).
    pub fn estimate_known(&self) -> bool {
        self.estimate.get().is_some()
    }

    /// `true` the first time only: whoever gets it asks the server for the estimate.
    pub fn ask_estimate(&self) -> bool {
        !self.estimate_asked.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// The estimate's answer (`None`: there is none); readers are told.
    pub fn set_estimate(&self, rows: Option<u64>) {
        if self.estimate.set(rows).is_ok() {
            self.bump();
        }
    }

    /// A result of the saved session read back: its columns (and where they come from), its
    /// rows in `chunks` and its final `status`. A snapshot: nothing streams into it and grid
    /// edits refuse it until it runs again.
    pub(crate) fn restored(
        sql: String,
        columns: Vec<ColumnInfo>,
        origins: Option<Vec<ColumnOrigin>>,
        named: Option<Vec<Option<NamedOrigin>>>,
        params: Vec<Option<String>>,
        chunks: Vec<Chunk>,
        status: Status,
    ) -> Arc<Self> {
        let mut rs = Arc::into_inner(Self::new(sql)).expect("a new result has one owner");
        rs.restored = true;
        let rs = Arc::new(rs);
        if !columns.is_empty() {
            let _ = rs.columns.set(Arc::new(columns));
        }
        if let Some(o) = origins {
            rs.set_origins(o);
        }
        if let Some(o) = named {
            rs.set_named_origins(o);
        }
        rs.set_params(params);
        rs.recorded.store(true, std::sync::atomic::Ordering::Relaxed);
        for c in chunks {
            rs.push_chunk(c);
        }
        rs.update_status(|s| *s = status);
        rs
    }

    /// A result read back from the saved session (see [`ResultSet::restored`]).
    pub fn is_restored(&self) -> bool {
        self.restored
    }

    /// The result set that followed this one in the same statement (a T-SQL batch returns any
    /// number): known before this one's phase is final, so whoever sees it finish finds it.
    pub fn next_result(&self) -> Option<Arc<ResultSet>> {
        self.next.get().cloned()
    }

    pub(crate) fn set_next(&self, next: Arc<ResultSet>) {
        let _ = self.next.set(next);
    }

    /// The server's messages (`PRINT`), and how many more came past [`MESSAGE_CAP`].
    pub fn messages(&self) -> (Arc<Vec<String>>, usize) {
        self.messages.lock().clone()
    }

    pub fn has_messages(&self) -> bool {
        !self.messages.lock().0.is_empty()
    }

    /// A message the server sent while this result was the statement's latest.
    pub fn push_message(&self, m: String) {
        {
            let mut g = self.messages.lock();
            if g.0.len() < MESSAGE_CAP {
                Arc::make_mut(&mut g.0).push(m);
            } else {
                g.1 += 1;
            }
        }
        self.bump();
    }

    /// It ran inside a transaction (or savepoint) that was rolled back.
    pub fn rolled_back(&self) -> bool {
        self.rollback.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The values bound to the statement's parameters (empty: none).
    pub fn params(&self) -> &[Option<String>] {
        self.params.get().map_or(&[], Vec::as_slice)
    }

    pub(crate) fn set_params(&self, p: Vec<Option<String>>) {
        let _ = self.params.set(p);
    }

    pub fn columns(&self) -> Option<Arc<Vec<ColumnInfo>>> {
        self.columns.get().cloned()
    }

    /// Table and column of each result column, when the server reported them.
    pub fn origins(&self) -> Option<Arc<Vec<ColumnOrigin>>> {
        self.origins.get().cloned()
    }

    pub(crate) fn set_origins(&self, o: Vec<ColumnOrigin>) {
        let _ = self.origins.set(Arc::new(o));
    }

    /// Database (schema), table and column of each result column by name, when the server
    /// reported them so (MySQL's column definitions); `None` for computed columns.
    pub fn named_origins(&self) -> Option<Arc<Vec<Option<NamedOrigin>>>> {
        self.named_origins.get().cloned()
    }

    pub(crate) fn set_named_origins(&self, o: Vec<Option<NamedOrigin>>) {
        let _ = self.named_origins.set(Arc::new(o));
    }

    pub fn set_columns(&self, cols: Vec<ColumnInfo>) {
        let _ = self.columns.set(Arc::new(cols));
        self.bump();
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            columns: self.columns().unwrap_or_default(),
            chunks: self.chunks.read().clone(),
            order: self.order.read().as_ref().map(|o| o.2.clone()),
        }
    }

    pub fn rows(&self) -> usize {
        self.chunks.read().rows
    }

    pub fn status(&self) -> Status {
        self.status.lock().clone()
    }

    pub fn phase(&self) -> Phase {
        self.status.lock().phase.unwrap_or(Phase::Running)
    }

    pub fn sort_state(&self) -> Option<(usize, SortDir)> {
        self.order.read().as_ref().map(|o| (o.0, o.1))
    }

    /// A receiver that changes whenever rows, phase or order change.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    pub(crate) fn push_chunk(&self, chunk: Chunk) {
        if chunk.rows == 0 {
            return;
        }
        {
            let mut g = self.chunks.write();
            let old: &Chunks = &g;
            let mut list = Vec::with_capacity(old.list.len() + 1);
            list.extend(old.list.iter().cloned());
            let mut starts = Vec::with_capacity(old.starts.len() + 1);
            starts.extend_from_slice(&old.starts);
            starts.push(old.rows);
            let rows = old.rows + chunk.rows;
            list.push(Arc::new(chunk));
            *g = Arc::new(Chunks { list, starts, rows });
        }
        self.bump();
    }

    pub(crate) fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock());
        self.bump();
    }

    pub(crate) fn demand(&self) -> watch::Receiver<Demand> {
        self.demand.subscribe()
    }

    /// The UI shows rows up to `upto`: make sure the fetcher reads past it.
    pub fn request_rows(&self, upto: usize) {
        let want = upto.saturating_add(prefetch());
        self.demand.send_if_modified(|d| {
            if want > d.rows {
                d.rows = want;
                true
            } else {
                false
            }
        });
    }

    /// Read everything (used before sorting, and by "fetch all").
    pub fn fetch_all(&self) {
        self.demand.send_if_modified(|d| !std::mem::replace(&mut d.all, true));
    }

    /// Keeps the rows loaded so far and lets the statement finish without storing the rest: the
    /// stream is read to its end and dropped, with no CancelRequest (a transaction stays
    /// intact), and the result ends `Done` with `partial` set. Used when the next statement of a
    /// script needs the session while this one is paused.
    pub fn stop_fetching(&self) {
        if self.phase().is_finished() {
            return;
        }
        self.status.lock().partial = true;
        self.abort();
    }

    /// A finished result built in memory (the activity monitor's lists): `columns` and the rows
    /// of `chunk`, done.
    pub fn from_chunk(sql: impl Into<String>, columns: Vec<ColumnInfo>, chunk: Chunk) -> Arc<Self> {
        let rs = Self::new(sql);
        let _ = rs.columns.set(Arc::new(columns));
        rs.push_chunk(chunk);
        rs.update_status(|s| {
            s.phase = Some(Phase::Done);
            s.elapsed = Some(Duration::ZERO);
        });
        rs
    }

    /// The agent (its client's name) this statement ran for, if one did.
    pub fn agent(&self) -> Option<&str> {
        self.agent.get().map(String::as_str)
    }

    pub(crate) fn set_agent(&self, client: &str) {
        let _ = self.agent.set(client.to_string());
    }

    /// A result that ran elsewhere (an agent's write, run on the agent's own backend) shown
    /// here: still running until [`ResultSet::finish_with`] ends it. Never written to the
    /// history by a session (whoever ran it records it).
    pub fn placeholder(sql: impl Into<String>, agent: Option<&str>) -> Arc<Self> {
        let rs = Self::new(sql);
        rs.recorded.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(a) = agent {
            rs.set_agent(a);
        }
        rs
    }

    /// Ends a [`ResultSet::placeholder`]: its rows (`columns` and `chunk`), the rows the
    /// statements changed, what to say about it (messages), or its error.
    pub fn finish_with(&self, columns: Option<Vec<ColumnInfo>>, chunk: Option<Chunk>, affected: Option<u64>, messages: Vec<String>, error: Option<ErrorInfo>, elapsed: Duration) {
        if let Some(c) = columns {
            let _ = self.columns.set(Arc::new(c));
        }
        if let Some(ch) = chunk {
            self.push_chunk(ch);
        }
        for m in messages {
            self.push_message(m);
        }
        self.update_status(|s| {
            s.phase = Some(if error.is_some() { Phase::Failed } else { Phase::Done });
            s.affected = affected;
            s.error = error;
            s.elapsed = Some(elapsed);
        });
    }

    /// A statement refused before it reached the server: a finished, failed result.
    pub fn refused(sql: impl Into<String>, error: ErrorInfo) -> Arc<Self> {
        let rs = Self::new(sql);
        rs.update_status(|s| {
            s.phase = Some(Phase::Failed);
            s.elapsed = Some(Duration::ZERO);
            s.error = Some(error);
        });
        rs
    }

    /// Stop fetching and drop the rest of the result (the stream is drained after a cancel).
    pub fn abort(&self) {
        self.demand.send_if_modified(|d| !std::mem::replace(&mut d.abort, true));
    }

    pub async fn wait_finished(&self) -> Phase {
        let mut rx = self.subscribe();
        loop {
            let p = self.phase();
            if p.is_finished() {
                return p;
            }
            if rx.changed().await.is_err() {
                return self.phase();
            }
        }
    }

    /// Rows the current sort covers (`None`: not sorted). Rows that arrived after it show after
    /// them, in arrival order, until the next [`ResultSet::sort`].
    pub fn sorted_rows(&self) -> Option<usize> {
        self.order.read().as_ref().map(|o| o.2.len())
    }

    /// Sorts the loaded rows by `col`, stably, by the column's type: numbers by value, dates and
    /// times in time order, text case-insensitively with digit runs by value
    /// ([`value::cmp_natural`]; JSON and bytes byte by byte), NULLs last either way. `None`
    /// restores the arrival order. Nothing is read from the server. CPU-bound: run it off the
    /// UI thread.
    pub fn sort(&self, by: Option<(usize, SortDir)>) {
        let Some((col, dir)) = by else {
            *self.order.write() = None;
            self.bump();
            return;
        };
        let snap = Snapshot { order: None, ..self.snapshot() };
        let n = snap.rows();
        let kind = snap.columns[col].kind;
        let mut perm: Vec<u32> = (0..n as u32).collect();
        // Pre-extract keys: much faster than locating chunks inside the comparator.
        enum Keys<'a> {
            I(Vec<Option<i64>>),
            F(Vec<Option<f64>>),
            B(Vec<Option<bool>>),
            T(Vec<Option<&'a str>>),
        }
        let cells = (0..n).map(|r| snap.cell(r, col));
        let keys = match kind.storage() {
            Storage::I64 => Keys::I(cells.map(|c| if let Cell::Int(x) = c { Some(x) } else { None }).collect()),
            Storage::F64 => Keys::F(cells.map(|c| if let Cell::Float(x) = c { Some(x) } else { None }).collect()),
            Storage::Bool => Keys::B(cells.map(|c| if let Cell::Bool(x) = c { Some(x) } else { None }).collect()),
            Storage::Text => Keys::T(cells.map(|c| if let Cell::Text(x) = c { Some(x) } else { None }).collect()),
        };
        // NULLs last in both directions; only the values turn around.
        fn by_opt<T>(a: &Option<T>, b: &Option<T>, dir: SortDir, f: impl Fn(&T, &T) -> Ordering) -> Ordering {
            match (a, b) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => match dir {
                    SortDir::Asc => f(a, b),
                    SortDir::Desc => f(b, a),
                },
            }
        }
        let numeric_text = kind == Kind::Numeric;
        let natural = matches!(kind, Kind::Text | Kind::Other);
        let cmp = |a: &u32, b: &u32| -> Ordering {
            let (a, b) = (*a as usize, *b as usize);
            match &keys {
                Keys::I(v) => by_opt(&v[a], &v[b], dir, |x, y| x.cmp(y)),
                Keys::F(v) => by_opt(&v[a], &v[b], dir, |x, y| x.total_cmp(y)),
                Keys::B(v) => by_opt(&v[a], &v[b], dir, |x, y| x.cmp(y)),
                Keys::T(v) if numeric_text => by_opt(&v[a], &v[b], dir, |x, y| value::cmp_decimal(x, y)),
                Keys::T(v) if natural => by_opt(&v[a], &v[b], dir, |x, y| value::cmp_natural(x, y)),
                Keys::T(v) => by_opt(&v[a], &v[b], dir, |x, y| x.cmp(y)),
            }
        };
        perm.sort_by(cmp);
        *self.order.write() = Some((col, dir, Arc::new(perm)));
        self.bump();
    }

    /// Copies a rectangle of display rows × columns.
    pub fn copy(&self, rows: impl IntoIterator<Item = usize>, cols: &[usize], fmt: CopyFormat, header: bool) -> String {
        let snap = self.snapshot();
        let mut out = String::new();
        let sep = match fmt {
            CopyFormat::Tsv => '\t',
            CopyFormat::Csv => ',',
        };
        if header {
            for (i, &c) in cols.iter().enumerate() {
                if i > 0 {
                    out.push(sep);
                }
                push_field(&mut out, Some(&snap.columns[c].name), fmt);
            }
            out.push('\n');
        }
        for r in rows {
            if r >= snap.rows() {
                break;
            }
            for (i, &c) in cols.iter().enumerate() {
                if i > 0 {
                    out.push(sep);
                }
                push_field(&mut out, snap.text(r, c).as_deref(), fmt);
            }
            out.push('\n');
        }
        out
    }
}

fn push_field(out: &mut String, v: Option<&str>, fmt: CopyFormat) {
    match (fmt, v) {
        (CopyFormat::Tsv, None) => out.push_str("\\N"),
        (CopyFormat::Tsv, Some(s)) => {
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    c => out.push(c),
                }
            }
        }
        (CopyFormat::Csv, None) => {}
        (CopyFormat::Csv, Some(s)) => {
            if s.is_empty() || s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
                out.push('"');
                out.push_str(&s.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<ColumnInfo> {
        vec![
            ColumnInfo { name: "id".into(), type_name: "int8".into(), type_oid: 20, kind: Kind::Int },
            ColumnInfo { name: "name".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text },
            ColumnInfo { name: "amount".into(), type_name: "numeric".into(), type_oid: 1700, kind: Kind::Numeric },
        ]
    }

    fn sample() -> Arc<ResultSet> {
        let rs = ResultSet::new("select");
        rs.set_columns(cols());
        let mut b = ChunkBuilder::new(&cols());
        let data: &[(i64, Option<&str>, Option<&str>)] = &[
            (3, Some("c"), Some("10")),
            (1, None, Some("9.5")),
            (2, Some(""), None),
            (4, Some("a\tb"), Some("-1")),
        ];
        for (i, (id, name, amount)) in data.iter().enumerate() {
            b.push_i64(0, *id);
            match name {
                Some(n) => b.push_text(1, n),
                None => b.push_null(1),
            }
            match amount {
                Some(a) => b.push_text(2, a),
                None => b.push_null(2),
            }
            b.end_row();
            if i == 1 {
                rs.push_chunk(b.finish()); // chunks of uneven sizes
            }
        }
        rs.push_chunk(b.finish());
        rs
    }

    #[test]
    fn null_differs_from_empty() {
        let rs = sample();
        let s = rs.snapshot();
        assert_eq!(s.rows(), 4);
        assert_eq!(s.text(1, 1), None);
        assert_eq!(s.text(2, 1).as_deref(), Some(""));
        assert_eq!(s.cell(3, 0), Cell::Int(4));
    }

    #[test]
    fn copy_formats() {
        let rs = sample();
        let tsv = rs.copy(0..4, &[0, 1], CopyFormat::Tsv, true);
        assert_eq!(tsv, "id\tname\n3\tc\n1\t\\N\n2\t\n4\ta\\tb\n");
        let csv = rs.copy(0..4, &[1, 2], CopyFormat::Csv, false);
        assert_eq!(csv, "c,10\n,9.5\n\"\",\na\tb,-1\n");
    }

    #[test]
    fn sorting() {
        let rs = sample();
        rs.sort(Some((2, SortDir::Asc)));
        let s = rs.snapshot();
        let ids: Vec<_> = (0..4).map(|r| s.cell(r, 0)).collect();
        // -1, 9.5, 10, NULL
        assert_eq!(ids, vec![Cell::Int(4), Cell::Int(1), Cell::Int(3), Cell::Int(2)]);
        assert_eq!(rs.sorted_rows(), Some(4));
        rs.sort(Some((1, SortDir::Desc)));
        let s = rs.snapshot();
        // NULLs last when descending too.
        assert_eq!(s.text(0, 1).as_deref(), Some("c"));
        assert_eq!(s.text(3, 1), None);
        rs.sort(Some((2, SortDir::Desc)));
        let s = rs.snapshot();
        let ids: Vec<_> = (0..4).map(|r| s.cell(r, 0)).collect();
        assert_eq!(ids, vec![Cell::Int(3), Cell::Int(1), Cell::Int(4), Cell::Int(2)]);
        rs.sort(None);
        assert_eq!(rs.snapshot().cell(0, 0), Cell::Int(3));
        assert_eq!(rs.sorted_rows(), None);
    }

    /// A million rows sort off the UI thread in well under a second or two (numbers, then text
    /// with the natural order).
    #[test]
    fn sorting_a_million_rows() {
        let c = vec![
            ColumnInfo { name: "n".into(), type_name: "int8".into(), type_oid: 20, kind: Kind::Int },
            ColumnInfo { name: "t".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text },
        ];
        let rs = ResultSet::new("x");
        rs.set_columns(c.clone());
        let mut b = ChunkBuilder::new(&c);
        for i in 0..1_000_000i64 {
            let k = (i * 7_919) % 1_000_003;
            b.push_i64(0, k);
            b.push_text(1, &format!("Item {k}"));
            b.end_row();
            if b.len() == CHUNK {
                rs.push_chunk(b.finish());
            }
        }
        rs.push_chunk(b.finish());
        let t0 = std::time::Instant::now();
        rs.sort(Some((0, SortDir::Desc)));
        let ints = t0.elapsed();
        let t1 = std::time::Instant::now();
        rs.sort(Some((1, SortDir::Asc)));
        let text = t1.elapsed();
        eprintln!("sort 1M rows: int {ints:?}, natural text {text:?}");
        let s = rs.snapshot();
        assert_eq!(s.text(0, 1).as_deref(), Some("Item 0"));
        assert!(ints < std::time::Duration::from_secs(3) && text < std::time::Duration::from_secs(6), "{ints:?} {text:?}");
    }

    /// Text sorts case-insensitively with numbers inside by value (a case-only tie by bytes);
    /// equal values keep their arrival order both ways; rows that arrive after a sort follow it
    /// until the next one.
    #[test]
    fn sorting_text_stable_and_late_rows() {
        let c = vec![
            ColumnInfo { name: "k".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text },
            ColumnInfo { name: "n".into(), type_name: "int8".into(), type_oid: 20, kind: Kind::Int },
        ];
        let rs = ResultSet::new("x");
        rs.set_columns(c.clone());
        let push = |rows: &[(&str, i64)]| {
            let mut b = ChunkBuilder::new(&c);
            for (k, n) in rows {
                b.push_text(0, k);
                b.push_i64(1, *n);
                b.end_row();
            }
            rs.push_chunk(b.finish());
        };
        push(&[("item10", 0), ("Item2", 1), ("b", 2), ("item2", 3), ("B", 4), ("a", 5)]);
        let order = |rs: &ResultSet| {
            let s = rs.snapshot();
            (0..s.rows()).map(|r| match s.cell(r, 1) { Cell::Int(n) => n, _ => -1 }).collect::<Vec<_>>()
        };
        rs.sort(Some((0, SortDir::Asc)));
        assert_eq!(order(&rs), [5, 4, 2, 1, 3, 0]);
        rs.sort(Some((0, SortDir::Desc)));
        assert_eq!(order(&rs), [0, 3, 1, 2, 4, 5]);
        push(&[("0", 6), ("a", 7)]);
        assert_eq!(rs.sorted_rows(), Some(6));
        assert_eq!(order(&rs), [0, 3, 1, 2, 4, 5, 6, 7]);
        rs.sort(rs.sort_state());
        assert_eq!(order(&rs), [0, 3, 1, 2, 4, 5, 7, 6]);
        rs.sort(Some((0, SortDir::Asc)));
        assert_eq!(order(&rs), [6, 5, 7, 4, 2, 1, 3, 0]);
    }

    #[test]
    fn many_rows_locate() {
        let c = vec![ColumnInfo { name: "n".into(), type_name: "int8".into(), type_oid: 20, kind: Kind::Int }];
        let rs = ResultSet::new("x");
        rs.set_columns(c.clone());
        let mut b = ChunkBuilder::new(&c);
        for i in 0..100_000i64 {
            b.push_i64(0, i);
            b.end_row();
            if b.len() == CHUNK || i % 777 == 0 {
                rs.push_chunk(b.finish());
            }
        }
        rs.push_chunk(b.finish());
        let s = rs.snapshot();
        assert_eq!(s.rows(), 100_000);
        for r in [0, 1, 776, 777, 778, 50_000, 99_999] {
            assert_eq!(s.cell(r, 0), Cell::Int(r as i64));
        }
        rs.sort(Some((0, SortDir::Desc)));
        assert_eq!(rs.snapshot().cell(0, 0), Cell::Int(99_999));
    }
}
