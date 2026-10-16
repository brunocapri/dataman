//! Exporting a result to a file: CSV, TSV, JSON, NDJSON, SQL `INSERT`s and Parquet.
//!
//! Two sources:
//!
//! - [`export_result`]: a result already in the grid (optionally only the selected rows). When
//!   the result is paused with rows left, the export asks for more as it writes, so the rest is
//!   fetched on demand; those rows stay in the result (it is the grid's), so memory grows with it.
//! - [`Connection::export_query`]: runs the statement again on a dedicated connection and streams
//!   it straight to the file, one chunk at a time: flat memory for 10 M rows and more.
//!
//! The writer runs on a blocking thread fed through a small bounded channel (back-pressure all
//! the way to the server socket). It writes to a temporary file next to the target and renames it
//! at the end; a cancelled or failed export removes the temporary file and leaves the target
//! untouched. Progress is published in [`ExportProgress`] (atomics the UI polls) and through an
//! optional callback.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use tokio::runtime::Handle;

use crate::backend::RowSource;
use crate::conn::Connection;
use crate::result::{CHUNK, Cell, Chunk, ChunkBuilder, ColumnInfo, Phase, ResultSet, Snapshot, format_cell};
use crate::sqlgen::{self, quote_ident};
use crate::value::{Kind, Storage};
use crate::{Task, spawn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    /// RFC 4180 (delimiter, quoting and NULL text configurable).
    Csv,
    /// `COPY … (FORMAT text)`: tab-separated, `\t \n \r \\` escaped, NULL as `\N` by default.
    Tsv,
    /// One JSON array of objects.
    Json,
    /// One JSON object per line.
    Ndjson,
    /// `INSERT INTO table (…) VALUES (…), (…);` in batches.
    SqlInsert,
    /// Apache Parquet (Snappy), columns typed from the result's kinds.
    Parquet,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 6] =
        [ExportFormat::Csv, ExportFormat::Tsv, ExportFormat::Json, ExportFormat::Ndjson, ExportFormat::SqlInsert, ExportFormat::Parquet];

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::Ndjson => "ndjson",
            ExportFormat::SqlInsert => "sql",
            ExportFormat::Parquet => "parquet",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::Ndjson => "NDJSON",
            ExportFormat::SqlInsert => "SQL INSERT",
            ExportFormat::Parquet => "Parquet",
        }
    }

    /// The format a file name suggests (`x.csv` → CSV).
    pub fn from_path(p: &Path) -> Option<ExportFormat> {
        let ext = p.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "csv" => ExportFormat::Csv,
            "tsv" | "tab" | "txt" => ExportFormat::Tsv,
            "json" => ExportFormat::Json,
            "ndjson" | "jsonl" => ExportFormat::Ndjson,
            "sql" => ExportFormat::SqlInsert,
            "parquet" | "pq" => ExportFormat::Parquet,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuoteStyle {
    /// Only fields that need it (delimiter, quote, line break, leading/trailing space, empty).
    #[default]
    Necessary,
    Always,
    /// Everything but numbers and booleans.
    NonNumeric,
    /// Never quote (the caller knows the data is clean).
    Never,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// CSV/TSV: a first line with the column names.
    pub header: bool,
    /// CSV only (TSV is always a tab).
    pub delimiter: char,
    pub quote: QuoteStyle,
    /// How NULL is written in CSV/TSV. `None`: CSV writes nothing (unquoted), TSV writes `\N`.
    pub null_text: Option<String>,
    pub line_ending: LineEnding,
    /// Columns to write, in this order (indexes into the result's columns). `None`: all.
    pub columns: Option<Vec<usize>>,
    /// Only these display rows of the result (the grid's selection). Only for [`export_result`].
    pub rows: Option<Vec<usize>>,
    /// SQL INSERT: the target table (quoted as needed; `schema.table` allowed).
    pub table: String,
    /// SQL INSERT: rows per statement.
    pub batch_size: usize,
    /// SQL INSERT: the dialect of the statements (the source connection's by default in the UI).
    pub dialect: crate::sql::Dialect,
    /// Parquet: rows per row group (memory held by the writer before flushing a group).
    pub row_group_rows: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            format: ExportFormat::Csv,
            header: true,
            delimiter: ',',
            quote: QuoteStyle::Necessary,
            null_text: None,
            line_ending: LineEnding::Lf,
            columns: None,
            rows: None,
            table: "exported".into(),
            batch_size: 500,
            dialect: crate::sql::Dialect::Postgres,
            row_group_rows: 64 * 1024,
        }
    }
}

impl ExportOptions {
    pub fn new(format: ExportFormat) -> Self {
        ExportOptions { format, ..Default::default() }
    }
}

/// Live progress of an export, shared with the UI.
#[derive(Debug, Default)]
pub struct ExportProgress {
    rows: AtomicU64,
    bytes: AtomicU64,
    /// Rows expected (0 = unknown).
    total: AtomicU64,
    cancel: AtomicBool,
    finished: AtomicBool,
}

impl ExportProgress {
    pub fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    /// Rows expected, when known (an export of a selection or of a finished result).
    pub fn total_rows(&self) -> Option<u64> {
        Some(self.total.load(Ordering::Relaxed)).filter(|t| *t > 0)
    }
    /// Asks the export to stop: the temporary file is removed and the target left untouched.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

/// Called with (rows, bytes) at most every 100 ms, from the writer thread.
pub type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[derive(Clone, Debug, PartialEq)]
pub struct ExportSummary {
    pub path: PathBuf,
    pub rows: u64,
    pub bytes: u64,
    pub elapsed: Duration,
    /// Stopped by [`ExportProgress::cancel`]: nothing was written to `path`.
    pub cancelled: bool,
    /// All rows of the source were written (false when the result had been cancelled with rows
    /// never fetched: the loaded ones were exported).
    pub complete: bool,
}

/// A running export: poll `progress`, await `task`.
pub struct Export {
    pub progress: Arc<ExportProgress>,
    pub task: Task<ExportSummary>,
}

// ---------------------------------------------------------------------------------------------
// Sources

enum Batch {
    /// Display rows `range` of a snapshot.
    Range(Snapshot, std::ops::Range<usize>),
    /// Selected display rows of a snapshot.
    Picked(Snapshot, Vec<usize>),
    /// Storage rows `range` of a snapshot (arrival order, whatever the sort).
    Stored(Snapshot, std::ops::Range<usize>),
    Chunk(Chunk),
}

const CHANNEL: usize = 4;

/// Exports a result shown in the grid. Rows not fetched yet are requested as the writer goes.
pub fn export_result(rt: &Handle, rs: Arc<ResultSet>, path: impl Into<PathBuf>, opts: ExportOptions) -> Export {
    export_result_with(rt, rs, path, opts, None)
}

pub fn export_result_with(
    rt: &Handle,
    rs: Arc<ResultSet>,
    path: impl Into<PathBuf>,
    opts: ExportOptions,
    on_progress: Option<ProgressFn>,
) -> Export {
    let progress = Arc::new(ExportProgress::default());
    let path = path.into();
    let p = progress.clone();
    let rt2 = rt.clone();
    let task = spawn(rt, async move {
        let t0 = Instant::now();
        // The columns are known once the statement has been described.
        let mut rx = rs.subscribe();
        while rs.columns().is_none() && !rs.phase().is_finished() {
            if rx.changed().await.is_err() {
                break;
            }
        }
        let cols = rs.columns().unwrap_or_default();
        if cols.is_empty() {
            if let Some(e) = rs.status().error {
                bail!("the statement failed: {e}");
            }
            bail!("the statement returned no columns");
        }
        let (tx, writer) = start_writer(&rt2, path.clone(), cols.to_vec(), opts.clone(), p.clone(), on_progress);
        let mut complete = true;
        if let Some(sel) = opts.rows.clone() {
            p.total.store(sel.len() as u64, Ordering::Relaxed);
            let snap = rs.snapshot();
            for part in sel.chunks(CHUNK) {
                if p.is_cancelled() || tx.send(Batch::Picked(snap.clone(), part.to_vec())).await.is_err() {
                    break;
                }
            }
        } else if rs.sort_state().is_some() {
            // A sorted result is shown in sort order: wait for every row, then write them.
            rs.fetch_all();
            let ph = wait_or_cancel(&rs, &p).await;
            complete = ph != Some(Phase::Cancelled);
            let snap = rs.snapshot();
            p.total.store(snap.rows() as u64, Ordering::Relaxed);
            let mut at = 0;
            while at < snap.rows() && !p.is_cancelled() {
                let end = (at + CHUNK * 8).min(snap.rows());
                if tx.send(Batch::Range(snap.clone(), at..end)).await.is_err() {
                    break;
                }
                at = end;
            }
        } else {
            // Unsorted when the export started: stream in arrival (storage) order, so sorting the
            // grid meanwhile changes nothing here.
            let mut sent = 0usize;
            loop {
                if p.is_cancelled() {
                    break;
                }
                let snap = rs.snapshot();
                let n = snap.source_rows();
                if n > sent {
                    if tx.send(Batch::Stored(snap, sent..n)).await.is_err() {
                        break;
                    }
                    sent = n;
                }
                let ph = rs.phase();
                if ph.is_finished() && rs.rows() <= sent {
                    complete = ph == Phase::Done;
                    if ph == Phase::Failed {
                        // Cancel first: a writer that just sees the channel close finishes the
                        // file and renames the partial temporary over the target.
                        p.cancel();
                        drop(tx);
                        let _ = writer.await;
                        let e = rs.status().error.map(|e| e.to_string()).unwrap_or_default();
                        bail!("the statement failed: {e}");
                    }
                    break;
                }
                // Keep the fetcher a little ahead of the writer, not the whole result ahead.
                rs.request_rows(sent + CHUNK * 4);
                let _ = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
            }
        }
        drop(tx);
        let (rows, bytes, cancelled) = writer.await.map_err(|e| anyhow!("export writer: {e}"))??;
        Ok(ExportSummary { path, rows, bytes, elapsed: t0.elapsed(), cancelled, complete: complete && !cancelled })
    });
    Export { progress, task }
}

/// Resolves once the export is cancelled (polled: the flag is a plain atomic).
async fn cancelled(p: &ExportProgress) {
    while !p.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_or_cancel(rs: &ResultSet, p: &ExportProgress) -> Option<Phase> {
    let mut rx = rs.subscribe();
    loop {
        let ph = rs.phase();
        if ph.is_finished() {
            return Some(ph);
        }
        if p.is_cancelled() {
            return None;
        }
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
    }
}

impl Connection {
    /// Runs `sql` on a dedicated connection to `database` and streams its rows to `path`, with
    /// flat memory. Only for statements that are safe to run again: SELECT, WITH, TABLE, VALUES.
    /// `opts.rows` (a selection) does not apply here.
    pub fn export_query(
        self: &Arc<Self>,
        database: Option<&str>,
        sql: impl Into<String>,
        path: impl Into<PathBuf>,
        opts: ExportOptions,
        on_progress: Option<ProgressFn>,
    ) -> Export {
        let progress = Arc::new(ExportProgress::default());
        let (this, db, sql, path) = (self.clone(), database.map(str::to_string), sql.into(), path.into());
        let p = progress.clone();
        let task = self.spawn(async move {
            let t0 = Instant::now();
            if !is_rerunnable(&sql) {
                bail!("only SELECT, WITH, TABLE and VALUES statements can be exported by running them again");
            }
            let d = this.dedicated(db.as_deref()).await?;
            let stmt = d.db().prepare(&sql).await.map_err(|e| anyhow!(e.to_string()))?;
            let cols = stmt.columns.clone();
            if cols.is_empty() {
                bail!("the statement returns no rows");
            }
            let (tx, writer) = start_writer(this.handle(), path.clone(), cols.clone(), opts, p.clone(), on_progress);
            let run = async {
                // Waiting for the first row of a slow statement (or for room in the channel)
                // must not delay a cancel: this whole future races the cancel flag below.
                let mut rows = match stmt.start().await {
                    Ok(r) => r,
                    Err(e) => return Some(e.to_string()),
                };
                let mut b = ChunkBuilder::new(&cols);
                loop {
                    match rows.next_into(&mut b).await {
                        Ok(true) => {
                            if b.len() >= CHUNK && tx.send(Batch::Chunk(b.finish())).await.is_err() {
                                return None;
                            }
                        }
                        Err(e) => return Some(e.to_string()),
                        Ok(false) => {
                            if !b.is_empty() {
                                let _ = tx.send(Batch::Chunk(b.finish())).await;
                            }
                            return None;
                        }
                    }
                }
            };
            let failure = tokio::select! {
                f = run => f,
                _ = cancelled(&p) => {
                    // The statement may still be running on the server: stop it there too.
                    let _ = d.cancel().await;
                    None
                }
            };
            if failure.is_some() {
                // The writer removes its temporary file when cancelled.
                p.cancel();
            }
            drop(tx);
            let r = writer.await.map_err(|e| anyhow!("export writer: {e}"))?;
            if let Some(f) = failure {
                bail!("the query failed: {f}");
            }
            let (rows, bytes, cancelled) = r?;
            Ok(ExportSummary { path, rows, bytes, elapsed: t0.elapsed(), cancelled, complete: !cancelled })
        });
        Export { progress, task }
    }
}

/// SELECT, WITH (without data-modifying parts), TABLE or VALUES.
pub fn is_rerunnable(sql: &str) -> bool {
    use crate::sql::lexer::{TokenKind, tokenize};
    let toks: Vec<_> = tokenize(sql).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let Some(first) = toks.first() else { return false };
    let w = first.text(sql).to_ascii_uppercase();
    if !matches!(w.as_str(), "SELECT" | "WITH" | "TABLE" | "VALUES") && first.text(sql) != "(" {
        return false;
    }
    !toks.iter().any(|t| {
        matches!(t.kind, TokenKind::Keyword | TokenKind::Ident)
            && matches!(t.text(sql).to_ascii_uppercase().as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "INTO" | "SUBSCRIBE" | "LISTEN")
    })
}

// ---------------------------------------------------------------------------------------------
// Writer

type WriterResult = Result<(u64, u64, bool)>;

fn start_writer(
    rt: &Handle,
    path: PathBuf,
    cols: Vec<ColumnInfo>,
    opts: ExportOptions,
    progress: Arc<ExportProgress>,
    on_progress: Option<ProgressFn>,
) -> (tokio::sync::mpsc::Sender<Batch>, tokio::task::JoinHandle<WriterResult>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Batch>(CHANNEL);
    let join = rt.spawn_blocking(move || write_file(&path, &cols, &opts, rx, &progress, on_progress));
    (tx, join)
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("export");
    let tag = uuid::Uuid::new_v4().simple().to_string();
    path.with_file_name(format!(".{name}.{}.tmp", &tag[..8]))
}

/// Counts bytes written, into the shared progress.
struct Counting<W> {
    inner: W,
    progress: Arc<ExportProgress>,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.progress.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn write_file(
    path: &Path,
    cols: &[ColumnInfo],
    opts: &ExportOptions,
    mut rx: tokio::sync::mpsc::Receiver<Batch>,
    progress: &Arc<ExportProgress>,
    on_progress: Option<ProgressFn>,
) -> WriterResult {
    let pick: Vec<usize> = match &opts.columns {
        Some(c) => {
            if let Some(bad) = c.iter().find(|&&i| i >= cols.len()) {
                bail!("column {bad} is not in the result");
            }
            c.clone()
        }
        None => (0..cols.len()).collect(),
    };
    if pick.is_empty() {
        bail!("no columns to export");
    }
    let picked: Vec<ColumnInfo> = pick.iter().map(|&i| cols[i].clone()).collect();
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !dir.is_dir() {
        bail!("folder {} does not exist", dir.display());
    }
    let tmp = temp_path(path);
    let file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let out = Counting { inner: BufWriter::with_capacity(1 << 20, file), progress: progress.clone() };
    let r = (|| -> Result<bool> {
        let mut sink = make_sink(out, &picked, opts)?;
        let mut last_report = Instant::now();
        let mut rows = 0u64;
        let mut report = |rows: u64, force: bool| {
            progress.rows.store(rows, Ordering::Relaxed);
            if let Some(f) = &on_progress
                && (force || last_report.elapsed() >= Duration::from_millis(100))
            {
                last_report = Instant::now();
                f(rows, progress.bytes());
            }
        };
        while let Some(batch) = rx.blocking_recv() {
            if progress.is_cancelled() {
                return Ok(true);
            }
            let mut cells: Vec<Cell<'_>> = Vec::with_capacity(pick.len());
            match &batch {
                Batch::Range(snap, range) => {
                    for r in range.clone() {
                        cells.clear();
                        cells.extend(pick.iter().map(|&c| snap.cell(r, c)));
                        sink.row(&cells)?;
                        rows += 1;
                    }
                }
                Batch::Stored(snap, range) => {
                    for r in range.clone() {
                        cells.clear();
                        cells.extend(pick.iter().map(|&c| snap.source_cell(r, c)));
                        sink.row(&cells)?;
                        rows += 1;
                    }
                }
                Batch::Picked(snap, list) => {
                    for &r in list {
                        if r >= snap.rows() {
                            continue;
                        }
                        cells.clear();
                        cells.extend(pick.iter().map(|&c| snap.cell(r, c)));
                        sink.row(&cells)?;
                        rows += 1;
                    }
                }
                Batch::Chunk(ch) => {
                    for r in 0..ch.rows {
                        cells.clear();
                        cells.extend(pick.iter().map(|&c| ch.cell(r, c)));
                        sink.row(&cells)?;
                        rows += 1;
                    }
                }
            }
            report(rows, false);
        }
        if progress.is_cancelled() {
            return Ok(true);
        }
        sink.finish()?;
        report(rows, true);
        Ok(false)
    })();
    match r {
        Ok(false) => {
            std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
            progress.finished.store(true, Ordering::Release);
            Ok((progress.rows(), progress.bytes(), false))
        }
        Ok(true) => {
            let _ = std::fs::remove_file(&tmp);
            progress.finished.store(true, Ordering::Release);
            Ok((progress.rows(), progress.bytes(), true))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            progress.finished.store(true, Ordering::Release);
            Err(e)
        }
    }
}

trait Sink {
    fn row(&mut self, cells: &[Cell<'_>]) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
}

fn make_sink<'a, W: Write + Send + 'a>(out: W, cols: &[ColumnInfo], o: &ExportOptions) -> Result<Box<dyn Sink + 'a>> {
    let kinds: Vec<Kind> = cols.iter().map(|c| c.kind).collect();
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let eol = o.line_ending.as_str();
    Ok(match o.format {
        ExportFormat::Csv | ExportFormat::Tsv => {
            let tsv = o.format == ExportFormat::Tsv;
            let mut s = DelimitedSink {
                out,
                kinds,
                tsv,
                delim: if tsv { '\t' } else { o.delimiter },
                quote: o.quote,
                null: o.null_text.clone().unwrap_or_else(|| if tsv { "\\N".into() } else { String::new() }),
                eol,
                line: String::new(),
            };
            if o.header {
                let h: Vec<Cell<'_>> = names.iter().map(|n| Cell::Text(n)).collect();
                let header_kinds = vec![Kind::Text; names.len()];
                let k = std::mem::replace(&mut s.kinds, header_kinds);
                s.row(&h)?;
                s.kinds = k;
            }
            Box::new(s)
        }
        ExportFormat::Json | ExportFormat::Ndjson => {
            let keys = names.iter().map(|n| serde_json::to_string(n).unwrap_or_default()).collect();
            Box::new(JsonSink { out, kinds, keys, array: o.format == ExportFormat::Json, first: true, eol, line: String::new() })
        }
        ExportFormat::SqlInsert => {
            let d = o.dialect;
            let table = table_sql(&o.table, d);
            let cols = names.iter().map(|n| quoted(d, n)).collect::<Vec<_>>().join(", ");
            Box::new(InsertSink {
                out,
                kinds,
                dialect: d,
                head: format!("INSERT INTO {table} ({cols}) VALUES"),
                // T-SQL takes at most 1000 rows in a VALUES list.
                batch: if d == crate::sql::Dialect::TSql { o.batch_size.clamp(1, 1000) } else { o.batch_size.max(1) },
                in_batch: 0,
                eol,
                line: String::new(),
            })
        }
        ExportFormat::Parquet => Box::new(parquet_sink::ParquetSink::new(out, cols, o.row_group_rows.max(1024))?),
    })
}

/// `schema.table` or `table`, quoting each part when needed; already-quoted parts kept.
fn table_sql(t: &str, d: crate::sql::Dialect) -> String {
    let t = t.trim();
    if t.contains(['"', '`', '[']) {
        return t.to_string();
    }
    match t.split_once('.') {
        Some((s, n)) => format!("{}.{}", quoted(d, s), quoted(d, n)),
        None => quoted(d, t),
    }
}

/// An identifier quoted for `d` (PostgreSQL's as before: only where it folds or is reserved).
fn quoted(d: crate::sql::Dialect, name: &str) -> String {
    if d == crate::sql::Dialect::Postgres { quote_ident(name) } else { d.quote_ident(name) }
}

fn cell_text(kind: Kind, cell: Cell<'_>) -> Option<String> {
    sqlgen::cell_text_utc(kind, cell)
}

struct DelimitedSink<W> {
    out: W,
    kinds: Vec<Kind>,
    tsv: bool,
    delim: char,
    quote: QuoteStyle,
    null: String,
    eol: &'static str,
    line: String,
}

impl<W: Write> DelimitedSink<W> {
    fn field(&mut self, kind: Kind, cell: Cell<'_>) {
        let Some(s) = cell_text(kind, cell) else {
            self.line.push_str(&self.null);
            return;
        };
        if self.tsv {
            for c in s.chars() {
                match c {
                    '\\' => self.line.push_str("\\\\"),
                    '\t' => self.line.push_str("\\t"),
                    '\n' => self.line.push_str("\\n"),
                    '\r' => self.line.push_str("\\r"),
                    c => self.line.push(c),
                }
            }
            return;
        }
        let numeric = matches!(cell, Cell::Int(_) | Cell::Float(_) | Cell::Bool(_)) && kind.storage() != Storage::Text
            && !matches!(kind, Kind::Date | Kind::Time | Kind::Timestamp | Kind::TimestampTz)
            || (kind == Kind::Numeric && sqlgen::is_decimal(&s));
        let needs = s.is_empty()
            || s.contains(self.delim)
            || s.contains(['"', '\n', '\r'])
            || s.starts_with(' ')
            || s.ends_with(' ')
            || (!self.null.is_empty() && s == self.null);
        let q = match self.quote {
            QuoteStyle::Always => true,
            QuoteStyle::Never => false,
            QuoteStyle::NonNumeric => !numeric,
            QuoteStyle::Necessary => needs,
        };
        if q {
            self.line.push('"');
            for c in s.chars() {
                if c == '"' {
                    self.line.push('"');
                }
                self.line.push(c);
            }
            self.line.push('"');
        } else {
            self.line.push_str(&s);
        }
    }
}

impl<W: Write> Sink for DelimitedSink<W> {
    fn row(&mut self, cells: &[Cell<'_>]) -> Result<()> {
        self.line.clear();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                self.line.push(self.delim);
            }
            let k = self.kinds[i];
            self.field(k, *c);
        }
        self.line.push_str(self.eol);
        self.out.write_all(self.line.as_bytes())?;
        Ok(())
    }
    fn finish(mut self: Box<Self>) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// A JSON value for a cell: numbers and booleans as such, `json` columns embedded, timestamps
/// with zone in RFC 3339 UTC, everything else as a string.
pub(crate) fn json_value(kind: Kind, cell: Cell<'_>, out: &mut String) {
    use std::fmt::Write as _;
    match (kind, cell) {
        (_, Cell::Null) => out.push_str("null"),
        (_, Cell::Bool(b)) => out.push_str(if b { "true" } else { "false" }),
        (Kind::Int, Cell::Int(v)) => {
            let _ = write!(out, "{v}");
        }
        (Kind::TimestampTz, Cell::Int(v)) => push_json_str(out, &sqlgen::timestamptz_rfc3339(v)),
        (_, Cell::Float(f)) if f.is_finite() => {
            let mut s = String::new();
            crate::value::format_float(f, kind == Kind::Float4, &mut s);
            // JSON numbers need a digit before the exponent sign, which format_float has.
            out.push_str(&s);
        }
        (Kind::Json, Cell::Text(s)) if serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok() => out.push_str(s),
        (Kind::Numeric, Cell::Text(s)) if sqlgen::is_decimal(s) && !s.starts_with('.') && !s.starts_with("-.") => out.push_str(s),
        (k, c) => push_json_str(out, &format_cell(k, c).unwrap_or_default()),
    }
}

fn push_json_str(out: &mut String, s: &str) {
    out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()));
}

struct JsonSink<W> {
    out: W,
    kinds: Vec<Kind>,
    keys: Vec<String>,
    array: bool,
    first: bool,
    eol: &'static str,
    line: String,
}

impl<W: Write> Sink for JsonSink<W> {
    fn row(&mut self, cells: &[Cell<'_>]) -> Result<()> {
        self.line.clear();
        if self.array {
            self.line.push_str(if self.first { "[" } else { "," });
            self.line.push_str(self.eol);
        }
        self.first = false;
        self.line.push('{');
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                self.line.push(',');
            }
            self.line.push_str(&self.keys[i]);
            self.line.push(':');
            json_value(self.kinds[i], *c, &mut self.line);
        }
        self.line.push('}');
        if !self.array {
            self.line.push_str(self.eol);
        }
        self.out.write_all(self.line.as_bytes())?;
        Ok(())
    }
    fn finish(mut self: Box<Self>) -> Result<()> {
        if self.array {
            let end = if self.first { format!("[]{}", self.eol) } else { format!("{}]{}", self.eol, self.eol) };
            self.out.write_all(end.as_bytes())?;
        }
        self.out.flush()?;
        Ok(())
    }
}

struct InsertSink<W> {
    out: W,
    kinds: Vec<Kind>,
    dialect: crate::sql::Dialect,
    head: String,
    batch: usize,
    in_batch: usize,
    eol: &'static str,
    line: String,
}

impl<W: Write> Sink for InsertSink<W> {
    fn row(&mut self, cells: &[Cell<'_>]) -> Result<()> {
        self.line.clear();
        if self.in_batch == 0 {
            self.line.push_str(&self.head);
            self.line.push_str(self.eol);
            self.line.push_str("  (");
        } else {
            self.line.push(',');
            self.line.push_str(self.eol);
            self.line.push_str("  (");
        }
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                self.line.push_str(", ");
            }
            self.line.push_str(&sqlgen::literal_in(self.dialect, self.kinds[i], *c));
        }
        self.line.push(')');
        self.in_batch += 1;
        if self.in_batch == self.batch {
            self.line.push(';');
            self.line.push_str(self.eol);
            self.in_batch = 0;
        }
        self.out.write_all(self.line.as_bytes())?;
        Ok(())
    }
    fn finish(mut self: Box<Self>) -> Result<()> {
        if self.in_batch > 0 {
            self.out.write_all(format!(";{}", self.eol).as_bytes())?;
        }
        self.out.flush()?;
        Ok(())
    }
}

/// `\x0aff` → bytes (the grid's text for `bytea`); anything else → its UTF-8 bytes.
fn unhex(s: &str) -> Vec<u8> {
    let Some(h) = s.strip_prefix("\\x") else { return s.as_bytes().to_vec() };
    if h.len() % 2 != 0 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
        return s.as_bytes().to_vec();
    }
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0)).collect()
}

mod parquet_sink {
    use std::io::Write;
    use std::sync::Arc;

    use anyhow::Result;
    use arrow_array::builder::{
        BinaryBuilder, BooleanBuilder, Date32Builder, Float32Builder, Float64Builder, Int64Builder, StringBuilder,
        Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
    };
    use arrow_array::{ArrayRef, RecordBatch};
    use arrow_schema::{DataType, Field, Schema, TimeUnit};
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::properties::WriterProperties;

    use super::{Sink, unhex};
    use crate::result::{Cell, ColumnInfo, format_cell};
    use crate::value::Kind;

    /// Days from 1970-01-01 to 2000-01-01 (PostgreSQL's epoch).
    const PG_EPOCH_DAYS: i32 = 10_957;
    const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;
    const BATCH: usize = 8192;

    enum Col {
        Bool(BooleanBuilder),
        I64(Int64Builder),
        F64(Float64Builder),
        F32(Float32Builder),
        Str(StringBuilder),
        Bin(BinaryBuilder),
        Date(Date32Builder),
        Time(Time64MicrosecondBuilder),
        Ts(TimestampMicrosecondBuilder),
        TsTz(TimestampMicrosecondBuilder),
    }

    /// Kind → Arrow type: integers Int64, floats Float64 (real: Float32), bool Boolean, bytea
    /// Binary, date Date32, time Time64(µs), timestamp Timestamp(µs), timestamptz
    /// Timestamp(µs, UTC); numeric, json, text and everything else Utf8 (exact text).
    pub(super) fn arrow_type(k: Kind) -> DataType {
        match k {
            Kind::Bool => DataType::Boolean,
            Kind::Int => DataType::Int64,
            Kind::Float => DataType::Float64,
            Kind::Float4 => DataType::Float32,
            Kind::Bytes => DataType::Binary,
            Kind::Date => DataType::Date32,
            Kind::Time => DataType::Time64(TimeUnit::Microsecond),
            Kind::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Kind::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            Kind::Numeric | Kind::Text | Kind::Json | Kind::Other => DataType::Utf8,
        }
    }

    fn builder(k: Kind) -> Col {
        match k {
            Kind::Bool => Col::Bool(BooleanBuilder::with_capacity(BATCH)),
            Kind::Int => Col::I64(Int64Builder::with_capacity(BATCH)),
            Kind::Float => Col::F64(Float64Builder::with_capacity(BATCH)),
            Kind::Float4 => Col::F32(Float32Builder::with_capacity(BATCH)),
            Kind::Bytes => Col::Bin(BinaryBuilder::new()),
            Kind::Date => Col::Date(Date32Builder::with_capacity(BATCH)),
            Kind::Time => Col::Time(Time64MicrosecondBuilder::with_capacity(BATCH)),
            Kind::Timestamp => Col::Ts(TimestampMicrosecondBuilder::with_capacity(BATCH)),
            Kind::TimestampTz => Col::TsTz(TimestampMicrosecondBuilder::with_capacity(BATCH).with_timezone("UTC")),
            _ => Col::Str(StringBuilder::new()),
        }
    }

    pub struct ParquetSink<W: Write + Send> {
        writer: Option<ArrowWriter<W>>,
        schema: Arc<Schema>,
        kinds: Vec<Kind>,
        cols: Vec<Col>,
        rows: usize,
    }

    impl<W: Write + Send> ParquetSink<W> {
        pub fn new(out: W, cols: &[ColumnInfo], row_group: usize) -> Result<Self> {
            let mut seen = std::collections::HashSet::new();
            let fields: Vec<Field> = cols
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    // Parquet column names must be unique.
                    let mut name = c.name.clone();
                    if !seen.insert(name.clone()) {
                        name = format!("{}_{}", c.name, i + 1);
                        seen.insert(name.clone());
                    }
                    Field::new(name, arrow_type(c.kind), true)
                })
                .collect();
            let schema = Arc::new(Schema::new(fields));
            let props = WriterProperties::builder()
                .set_compression(Compression::SNAPPY)
                .set_max_row_group_row_count(Some(row_group))
                .build();
            let writer = ArrowWriter::try_new(out, schema.clone(), Some(props))?;
            let kinds: Vec<Kind> = cols.iter().map(|c| c.kind).collect();
            let builders = kinds.iter().map(|k| builder(*k)).collect();
            Ok(ParquetSink { writer: Some(writer), schema, kinds, cols: builders, rows: 0 })
        }

        fn flush(&mut self) -> Result<()> {
            if self.rows == 0 {
                return Ok(());
            }
            let arrays: Vec<ArrayRef> = self
                .cols
                .iter_mut()
                .map(|c| -> ArrayRef {
                    match c {
                        Col::Bool(b) => Arc::new(b.finish()),
                        Col::I64(b) => Arc::new(b.finish()),
                        Col::F64(b) => Arc::new(b.finish()),
                        Col::F32(b) => Arc::new(b.finish()),
                        Col::Str(b) => Arc::new(b.finish()),
                        Col::Bin(b) => Arc::new(b.finish()),
                        Col::Date(b) => Arc::new(b.finish()),
                        Col::Time(b) => Arc::new(b.finish()),
                        Col::Ts(b) => Arc::new(b.finish()),
                        Col::TsTz(b) => Arc::new(b.finish()),
                    }
                })
                .collect();
            let batch = RecordBatch::try_new(self.schema.clone(), arrays)?;
            self.writer.as_mut().expect("open").write(&batch)?;
            self.rows = 0;
            Ok(())
        }
    }

    impl<W: Write + Send> Sink for ParquetSink<W> {
        fn row(&mut self, cells: &[Cell<'_>]) -> Result<()> {
            for (i, cell) in cells.iter().enumerate() {
                let k = self.kinds[i];
                match (&mut self.cols[i], *cell) {
                    (Col::Bool(b), Cell::Bool(v)) => b.append_value(v),
                    (Col::Bool(b), _) => b.append_null(),
                    (Col::I64(b), Cell::Int(v)) => b.append_value(v),
                    (Col::I64(b), Cell::Text(s)) => b.append_option(s.trim().parse().ok()),
                    (Col::I64(b), _) => b.append_null(),
                    (Col::F64(b), Cell::Float(v)) => b.append_value(v),
                    (Col::F64(b), Cell::Text(s)) => b.append_option(s.trim().parse().ok()),
                    (Col::F64(b), _) => b.append_null(),
                    (Col::F32(b), Cell::Float(v)) => b.append_value(v as f32),
                    (Col::F32(b), _) => b.append_null(),
                    (Col::Date(b), Cell::Int(v)) => match v {
                        // ±infinity and MySQL's zero dates have no Date32 value.
                        v if v == i32::MAX as i64 || v == i32::MIN as i64 || crate::value::is_odd_date(v) => b.append_null(),
                        v => b.append_value(v as i32 + PG_EPOCH_DAYS),
                    },
                    (Col::Date(b), _) => b.append_null(),
                    (Col::Time(b), Cell::Int(v)) => b.append_value(v),
                    (Col::Time(b), _) => b.append_null(),
                    (Col::Ts(b) | Col::TsTz(b), Cell::Int(v)) => match v {
                        i64::MAX | i64::MIN => b.append_null(),
                        v if crate::value::is_odd_date(v) => b.append_null(),
                        v => b.append_option(v.checked_add(PG_EPOCH_MICROS)),
                    },
                    (Col::Ts(b) | Col::TsTz(b), _) => b.append_null(),
                    (Col::Bin(b), Cell::Text(s)) => b.append_value(unhex(s)),
                    (Col::Bin(b), _) => b.append_null(),
                    (Col::Str(b), c) => b.append_option(format_cell(k, c).as_deref()),
                }
            }
            self.rows += 1;
            if self.rows >= BATCH {
                self.flush()?;
            }
            Ok(())
        }

        fn finish(mut self: Box<Self>) -> Result<()> {
            self.flush()?;
            if let Some(w) = self.writer.take() {
                let mut inner = w.into_inner()?;
                inner.flush()?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ChunkBuilder;

    fn cols() -> Vec<ColumnInfo> {
        let c = |n: &str, t: &str, k| ColumnInfo { name: n.into(), type_name: t.into(), type_oid: 0, kind: k };
        vec![
            c("id", "int8", Kind::Int),
            c("name", "text", Kind::Text),
            c("price", "numeric", Kind::Numeric),
            c("ok", "bool", Kind::Bool),
            c("at", "timestamptz", Kind::TimestampTz),
            c("doc", "jsonb", Kind::Json),
            c("raw", "bytea", Kind::Bytes),
            c("x", "float8", Kind::Float),
        ]
    }

    fn sample(n: usize) -> Arc<ResultSet> {
        let rs = ResultSet::new("select");
        rs.set_columns(cols());
        let mut b = ChunkBuilder::new(&cols());
        for i in 0..n {
            b.push_i64(0, i as i64);
            match i % 4 {
                0 => b.push_text(1, "plain"),
                1 => b.push_text(1, "a,\"b\"\nc"),
                2 => b.push_text(1, ""),
                _ => b.push_null(1),
            }
            b.push_text(2, "12.50");
            b.push_bool(3, i % 2 == 0);
            b.push_i64(4, 1_500_000);
            b.push_text(5, "{\"a\": [1, 2]}");
            b.push_text(6, "\\xdead");
            if i == 3 { b.push_f64(7, f64::NAN) } else { b.push_f64(7, 0.25) }
            b.end_row();
            if b.len() == CHUNK {
                rs.push_chunk(b.finish());
            }
        }
        rs.push_chunk(b.finish());
        rs.update_status(|s| s.phase = Some(Phase::Done));
        rs
    }

    fn run(rs: Arc<ResultSet>, opts: ExportOptions) -> (String, ExportSummary) {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("out.{}", opts.format.extension()));
        let e = export_result(rt.handle(), rs, &path, opts);
        let s = rt.block_on(e.task).unwrap();
        let text = std::fs::read(&path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        // No temporary file left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        (text, s)
    }

    #[test]
    fn csv_quoting_and_nulls() {
        let mut o = ExportOptions::new(ExportFormat::Csv);
        o.columns = Some(vec![0, 1, 2]);
        let (t, s) = run(sample(4), o.clone());
        assert_eq!(s.rows, 4);
        assert_eq!(t, "id,name,price\n0,plain,12.50\n1,\"a,\"\"b\"\"\nc\",12.50\n2,\"\",12.50\n3,,12.50\n");
        o.quote = QuoteStyle::NonNumeric;
        o.null_text = Some("NULL".into());
        o.delimiter = ';';
        o.line_ending = LineEnding::CrLf;
        o.header = false;
        let (t, _) = run(sample(4), o);
        assert_eq!(t.split("\r\n").nth(3).unwrap(), "3;NULL;12.50");
        assert!(t.starts_with("0;\"plain\";12.50\r\n"));
    }

    #[test]
    fn tsv_like_copy() {
        let mut o = ExportOptions::new(ExportFormat::Tsv);
        o.columns = Some(vec![1, 3, 4]);
        o.header = false;
        let (t, _) = run(sample(4), o);
        let l: Vec<&str> = t.lines().collect();
        assert_eq!(l[1], "a,\"b\"\\nc\tfalse\t2000-01-01 00:00:01.5+00");
        assert_eq!(l[3], "\\N\tfalse\t2000-01-01 00:00:01.5+00");
    }

    #[test]
    fn json_and_ndjson() {
        let (t, _) = run(sample(2), ExportOptions::new(ExportFormat::Json));
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v[0]["id"], 0);
        assert_eq!(v[0]["price"], serde_json::json!(12.50));
        assert_eq!(v[0]["doc"]["a"][1], 2);
        assert_eq!(v[1]["at"], "2000-01-01T00:00:01.5Z");
        assert_eq!(v[1]["name"], "a,\"b\"\nc");
        let (t, _) = run(sample(4), ExportOptions::new(ExportFormat::Ndjson));
        let lines: Vec<serde_json::Value> = t.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[3]["name"], serde_json::Value::Null);
        assert_eq!(lines[3]["x"], "NaN");
        let rs = ResultSet::new("x");
        rs.set_columns(cols());
        rs.update_status(|s| s.phase = Some(Phase::Done));
        let (t, _) = run(rs, ExportOptions::new(ExportFormat::Json));
        assert_eq!(t.trim(), "[]");
    }

    #[test]
    fn sql_inserts_in_batches() {
        let mut o = ExportOptions::new(ExportFormat::SqlInsert);
        o.table = "sales.Orders".into();
        o.batch_size = 2;
        o.columns = Some(vec![0, 1, 3, 6]);
        let (t, _) = run(sample(3), o);
        assert_eq!(
            t,
            "INSERT INTO sales.\"Orders\" (id, name, ok, raw) VALUES\n  (0, 'plain', TRUE, '\\xdead'),\n  (1, 'a,\"b\"\nc', FALSE, '\\xdead');\n\
             INSERT INTO sales.\"Orders\" (id, name, ok, raw) VALUES\n  (2, '', TRUE, '\\xdead');\n"
        );
    }

    #[test]
    fn selection_only() {
        let mut o = ExportOptions::new(ExportFormat::Csv);
        o.rows = Some(vec![2, 0, 99]);
        o.columns = Some(vec![0]);
        o.header = false;
        let (t, s) = run(sample(4), o);
        assert_eq!(t, "2\n0\n");
        assert_eq!(s.rows, 2);
    }

    #[test]
    fn parquet_roundtrip() {
        use parquet::file::reader::{FileReader, SerializedFileReader};
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.parquet");
        let mut o = ExportOptions::new(ExportFormat::Parquet);
        o.row_group_rows = 2048;
        let e = export_result(rt.handle(), sample(5000), &path, o);
        let s = rt.block_on(e.task).unwrap();
        assert_eq!(s.rows, 5000);
        let r = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        let md = r.metadata();
        assert_eq!(md.file_metadata().num_rows(), 5000);
        assert!(md.num_row_groups() >= 3, "{}", md.num_row_groups());
        let schema = md.file_metadata().schema_descr();
        assert_eq!(schema.num_columns(), 8);
        // Read back the first row through Arrow and check the types.
        let f = std::fs::File::open(&path).unwrap();
        let b = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f).unwrap();
        let mut rd = b.build().unwrap();
        let batch = rd.next().unwrap().unwrap();
        use arrow_array::Array;
        let ts = batch.column(4).as_any().downcast_ref::<arrow_array::TimestampMicrosecondArray>().unwrap();
        assert_eq!(ts.value(0), 946_684_801_500_000);
        let raw = batch.column(6).as_any().downcast_ref::<arrow_array::BinaryArray>().unwrap();
        assert_eq!(raw.value(0), &[0xde, 0xad]);
        let name = batch.column(1).as_any().downcast_ref::<arrow_array::StringArray>().unwrap();
        assert!(name.is_null(3));
        assert_eq!(name.value(2), "");
    }

    #[test]
    fn cancel_leaves_target_untouched() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        std::fs::write(&path, "old").unwrap();
        // A result that never finishes: the export waits for rows until cancelled.
        let rs = ResultSet::new("x");
        rs.set_columns(cols());
        let e = export_result(rt.handle(), rs, &path, ExportOptions::new(ExportFormat::Csv));
        std::thread::sleep(Duration::from_millis(50));
        e.progress.cancel();
        let s = rt.block_on(e.task).unwrap();
        assert!(s.cancelled);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_statement_leaves_target_untouched() {
        // Rows, then the statement fails: the export errors and the partial file never replaces
        // the target.
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        std::fs::write(&path, "old").unwrap();
        let rs = sample(10);
        rs.update_status(|s| {
            s.phase = Some(Phase::Failed);
            s.error = Some(crate::result::ErrorInfo { code: Some("22012".into()), message: "division by zero".into(), ..Default::default() });
        });
        let e = export_result(rt.handle(), rs, &path, ExportOptions::new(ExportFormat::Csv));
        let err = rt.block_on(e.task).unwrap_err().to_string();
        assert!(err.contains("division by zero"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn sorting_during_a_streaming_export_changes_nothing() {
        // A current-thread runtime runs the export only inside `block_on`, so the grid can sort
        // between two of its reads deterministically.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        let cols = cols();
        let rs = ResultSet::new("select");
        rs.set_columns(cols.clone());
        let chunk = |from: i64, n: i64| {
            let mut b = ChunkBuilder::new(&cols);
            for i in from..from + n {
                b.push_i64(0, i);
                for c in 1..cols.len() {
                    b.push_null(c);
                }
                b.end_row();
            }
            b.finish()
        };
        rs.push_chunk(chunk(0, 100));
        let mut o = ExportOptions::new(ExportFormat::Csv);
        o.columns = Some(vec![0]);
        o.header = false;
        let e = export_result(rt.handle(), rs.clone(), &path, o);
        let p = e.progress.clone();
        rt.block_on(async {
            while p.rows() < 100 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        // More rows arrive and the grid sorts them all (descending) before the export looks again.
        rs.push_chunk(chunk(100, 100));
        rs.sort(Some((0, crate::result::SortDir::Desc)));
        rs.update_status(|s| s.phase = Some(Phase::Done));
        let s = rt.block_on(e.task).unwrap();
        assert_eq!(s.rows, 200);
        let want: String = (0..200).map(|i| format!("{i}\n")).collect();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want);
    }

    #[test]
    fn rerunnable_statements() {
        assert!(is_rerunnable("select 1"));
        assert!(is_rerunnable("  WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(!is_rerunnable("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d"));
        assert!(!is_rerunnable("update t set a = 1"));
        assert!(!is_rerunnable("SELECT * INTO t2 FROM t"));
    }

    #[test]
    fn formats_from_paths() {
        assert_eq!(ExportFormat::from_path(Path::new("a/b.JSONL")), Some(ExportFormat::Ndjson));
        assert_eq!(ExportFormat::from_path(Path::new("x.parquet")), Some(ExportFormat::Parquet));
        assert_eq!(ExportFormat::from_path(Path::new("x")), None);
        assert_eq!(table_sql("public.order", crate::sql::Dialect::Postgres), "public.\"order\"");
        assert_eq!(table_sql("shop.order", crate::sql::Dialect::MySql), "shop.`order`");
        assert_eq!(table_sql("`a b`.c", crate::sql::Dialect::MySql), "`a b`.c");
    }
}
