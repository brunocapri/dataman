//! Importing a file into a table: CSV, TSV, JSON (an array of objects), NDJSON and Parquet.
//!
//! [`preview`] reads the start of a file (never all of it): the format, the CSV dialect
//! (delimiter, quote, header, encoding), the source columns with types inferred from a sample,
//! and the first rows. [`Connection::import`] loads it on a connection of its own (never an
//! editor tab's session): a producer thread streams the file, turns each row into `COPY` text
//! (or `INSERT` tuples) and hands chunks of rows over a small bounded channel to the loader,
//! which sends them with `COPY … FROM STDIN` (tokio-postgres's `copy_in`, text format) or, where
//! the server has no `COPY FROM STDIN`, with multi-row `INSERT … VALUES` built by `sqlgen`.
//! Memory stays flat whatever the file's size: at most a few chunks are alive at a time.
//!
//! Options: truncate first, stop at the first bad row or skip bad rows (a count and the first
//! hundred, with their lines), and one transaction for everything (otherwise each chunk commits
//! on its own, so a failure keeps what was already loaded). Errors carry the server's message
//! and the line of the file. A read-only connection is refused before anything is sent.

pub mod csv;
pub mod infer;
mod load;
mod source;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;
use tokio::runtime::Handle;

pub use infer::{Infer, PgType};

use crate::Task;
use crate::result::ErrorInfo;
use crate::sqlgen::{self, quote_ident};
use crate::value::Kind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportFormat {
    Csv,
    /// Tab-separated, `COPY … (FORMAT text)` style: `\t \n \r \\` escapes and `\N` for NULL
    /// (what the export writes; a plain TSV without backslashes reads the same).
    Tsv,
    /// One JSON array of objects.
    Json,
    /// One JSON object per line (a `.json` file that starts with `{` reads as this).
    Ndjson,
    Parquet,
}

impl ImportFormat {
    pub const ALL: [ImportFormat; 5] = [ImportFormat::Csv, ImportFormat::Tsv, ImportFormat::Json, ImportFormat::Ndjson, ImportFormat::Parquet];

    pub fn label(self) -> &'static str {
        match self {
            ImportFormat::Csv => "CSV",
            ImportFormat::Tsv => "TSV",
            ImportFormat::Json => "JSON",
            ImportFormat::Ndjson => "NDJSON",
            ImportFormat::Parquet => "Parquet",
        }
    }

    pub fn is_delimited(self) -> bool {
        matches!(self, ImportFormat::Csv | ImportFormat::Tsv)
    }

    /// What a bad row's position counts: lines of the file, or rows (JSON arrays, Parquet).
    pub fn location_word(self) -> &'static str {
        match self {
            ImportFormat::Json | ImportFormat::Parquet => "row",
            _ => "line",
        }
    }

    /// The format a file name suggests.
    pub fn from_path(p: &Path) -> Option<ImportFormat> {
        let ext = p.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "csv" => ImportFormat::Csv,
            "tsv" | "tab" => ImportFormat::Tsv,
            "json" => ImportFormat::Json,
            "ndjson" | "jsonl" => ImportFormat::Ndjson,
            "parquet" | "pq" => ImportFormat::Parquet,
            _ => return None,
        })
    }

    /// A file this sheet can import (by its extension).
    pub fn is_importable(p: &Path) -> bool {
        ImportFormat::from_path(p).is_some() || p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("txt"))
    }
}

/// The format of a file: its first bytes (Parquet's magic, a JSON bracket) over its name.
pub fn detect_format(path: &Path, head: &[u8]) -> ImportFormat {
    if head.starts_with(b"PAR1") {
        return ImportFormat::Parquet;
    }
    let first = head.iter().copied().skip_while(|c| c.is_ascii_whitespace() || *c == 0xef || *c == 0xbb || *c == 0xbf).next();
    match (ImportFormat::from_path(path), first) {
        (Some(ImportFormat::Json), Some(b'{')) => ImportFormat::Ndjson,
        (Some(ImportFormat::Ndjson), Some(b'[')) => ImportFormat::Json,
        (Some(f), _) => f,
        (None, Some(b'[')) => ImportFormat::Json,
        (None, Some(b'{')) => ImportFormat::Ndjson,
        (None, _) => ImportFormat::Csv,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Utf8,
    Latin1,
    Windows1252,
}

impl Encoding {
    pub const ALL: [Encoding; 3] = [Encoding::Utf8, Encoding::Latin1, Encoding::Windows1252];

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Latin1 => "Latin-1",
            Encoding::Windows1252 => "Windows-1252",
        }
    }
}

/// How a CSV or TSV file is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvDialect {
    pub delimiter: u8,
    /// `None`: no quoting (TSV).
    pub quote: Option<u8>,
    /// The first record names the columns.
    pub header: bool,
    pub encoding: Encoding,
    /// An unquoted field equal to this is NULL (`""`: empty unquoted fields; a quoted `""` is
    /// always an empty string). Empty values going into non-text columns are NULL anyway.
    pub null_text: String,
    /// `COPY` text escapes (`\t \n \r \\`); with them `\N` means NULL.
    pub escapes: bool,
}

impl Default for CsvDialect {
    fn default() -> Self {
        CsvDialect { delimiter: b',', quote: Some(b'"'), header: true, encoding: Encoding::Utf8, null_text: String::new(), escapes: false }
    }
}

impl CsvDialect {
    pub fn tsv() -> Self {
        CsvDialect { delimiter: b'\t', quote: None, header: true, encoding: Encoding::Utf8, null_text: "\\N".into(), escapes: true }
    }
}

/// A column of the file.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceColumn {
    pub name: String,
    /// Inferred from the sample (Parquet: from the file's schema, refined by the sample for
    /// strings).
    pub ty: PgType,
}

/// What [`preview`] read.
#[derive(Clone, Debug)]
pub struct Preview {
    pub path: PathBuf,
    pub format: ImportFormat,
    /// The dialect used (detected, or the one given); meaningful for CSV and TSV.
    pub dialect: CsvDialect,
    pub columns: Vec<SourceColumn>,
    /// The first rows (at most [`PREVIEW_ROWS`]), one value per column.
    pub rows: Vec<Vec<Option<String>>>,
    pub file_bytes: u64,
    /// Rows in the file: exact when the sample covered it (or from Parquet's metadata), else
    /// estimated from the sample's bytes per row.
    pub total_rows: Option<u64>,
    pub rows_exact: bool,
    /// Rows the inference looked at.
    pub sampled: usize,
}

pub const PREVIEW_ROWS: usize = 50;
/// The inference looks at this many rows at most…
pub const SAMPLE_ROWS: usize = 2000;
/// …within this many bytes of the start (CSV, TSV, JSON, NDJSON).
pub const SAMPLE_BYTES: usize = 1 << 20;

/// Reads the start of `path`: the format (given, or detected), the dialect (given, or
/// detected for CSV/TSV), the columns with inferred types and the first rows. Blocking: call it
/// off the UI thread ([`preview_task`]).
pub fn preview(path: &Path, format: Option<ImportFormat>, dialect: Option<CsvDialect>) -> Result<Preview> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.is_dir() {
        bail!("{} is a folder", path.display());
    }
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut head = Vec::with_capacity(SAMPLE_BYTES.min(meta.len() as usize));
    (&mut f).take(SAMPLE_BYTES as u64).read_to_end(&mut head)?;
    let complete = (head.len() as u64) >= meta.len();
    let format = format.unwrap_or_else(|| detect_format(path, &head));
    let mut p = match format {
        ImportFormat::Csv | ImportFormat::Tsv => source::preview_delimited(&head, complete, format, dialect)?,
        ImportFormat::Json | ImportFormat::Ndjson => source::preview_json(path, format)?,
        ImportFormat::Parquet => source::preview_parquet(path)?,
    };
    p.path = path.to_path_buf();
    p.file_bytes = meta.len();
    if p.total_rows.is_none() && p.sampled > 0 {
        // Bytes per row in the sample → rows in the file.
        let per_row = (head.len() as f64 / p.sampled.max(1) as f64).max(1.);
        p.total_rows = Some((meta.len() as f64 / per_row).round() as u64);
    }
    Ok(p)
}

/// [`preview`] on a blocking thread of the core's runtime.
pub fn preview_task(rt: &Handle, path: PathBuf, format: Option<ImportFormat>, dialect: Option<CsvDialect>) -> Task<Preview> {
    let rt2 = rt.clone();
    crate::spawn(rt, async move { rt2.spawn_blocking(move || preview(&path, format, dialect)).await? })
}

// ---------------------------------------------------------------------------------------------
// Options

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnError {
    /// The first bad row stops the import (one transaction: nothing stays).
    #[default]
    Stop,
    /// Bad rows are left out and counted (the first hundred kept with their lines).
    Skip,
}

/// How rows reach the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Method {
    /// `COPY … FROM STDIN` when the server accepts it (checked first), else `INSERT`.
    #[default]
    Auto,
    Copy,
    Insert,
}

/// One column of the target, fed by one source column.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnMap {
    /// Index into the preview's columns.
    pub source: usize,
    /// The target column's name.
    pub target: String,
    /// Its type: the catalog's (existing table) or the one to create (new table).
    pub type_name: String,
}

#[derive(Clone, Debug)]
pub struct ImportOptions {
    pub format: ImportFormat,
    pub dialect: CsvDialect,
    /// The source column names, in file order (JSON keys, Parquet fields, CSV header).
    pub source_names: Vec<String>,
    pub schema: String,
    pub table: String,
    /// `CREATE TABLE` it first, with the columns' types.
    pub create: bool,
    /// The target columns, in order. Target columns left out get their defaults.
    pub columns: Vec<ColumnMap>,
    pub truncate: bool,
    pub on_error: OnError,
    pub one_transaction: bool,
    pub method: Method,
    /// `INSERT`: rows per statement (0: up to 1,000 rows or 512 KB of text).
    pub insert_rows: usize,
}

impl ImportOptions {
    pub fn qualified(&self) -> String {
        sqlgen::qualified(Some(&self.schema), &self.table)
    }

    fn column_list(&self) -> String {
        self.columns.iter().map(|c| quote_ident(&c.target)).collect::<Vec<_>>().join(", ")
    }

    pub fn create_sql(&self) -> String {
        let cols: Vec<String> = self.columns.iter().map(|c| format!("{} {}", quote_ident(&c.target), c.type_name)).collect();
        format!("CREATE TABLE {} ({})", self.qualified(), cols.join(", "))
    }

    pub fn truncate_sql(&self) -> String {
        format!("TRUNCATE TABLE {}", self.qualified())
    }

    pub fn copy_sql(&self) -> String {
        format!("COPY {} ({}) FROM STDIN", self.qualified(), self.column_list())
    }

    fn insert_head(&self) -> String {
        format!("INSERT INTO {} ({}) VALUES ", self.qualified(), self.column_list())
    }

    /// The statements the import runs, for the guard sheet (`guard::review`): CREATE TABLE,
    /// TRUNCATE, then the load (shown as its COPY).
    pub fn statements(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.create {
            v.push(self.create_sql());
        }
        if self.truncate && !self.create {
            v.push(self.truncate_sql());
        }
        v.push(self.copy_sql());
        v
    }

    /// What is wrong with the options before anything runs.
    pub fn check(&self) -> Result<(), String> {
        if self.table.trim().is_empty() {
            return Err("the table has no name".into());
        }
        if self.columns.is_empty() {
            return Err("no column is imported".into());
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.columns {
            if c.target.trim().is_empty() {
                return Err("a column has no name".into());
            }
            if !seen.insert(c.target.as_str()) {
                return Err(format!("{} is fed twice", quote_ident(&c.target)));
            }
            if c.source >= self.source_names.len() {
                return Err(format!("column {} is not in the file", c.source + 1));
            }
        }
        Ok(())
    }
}

/// For each source column, the target column it feeds by default: the same name (exactly, then
/// ignoring case and punctuation: `Customer ID` → `customer_id`), each target used once. A
/// file without names (`column_1`…) maps by position.
pub fn auto_map(source: &[SourceColumn], target: &[String], named: bool) -> Vec<Option<usize>> {
    if !named {
        return (0..source.len()).map(|i| (i < target.len()).then_some(i)).collect();
    }
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let mut used = vec![false; target.len()];
    let mut out = vec![None; source.len()];
    for exact in [true, false] {
        for (i, s) in source.iter().enumerate() {
            if out[i].is_some() {
                continue;
            }
            let hit = target.iter().enumerate().position(|(j, t)| !used[j] && if exact { *t == s.name } else { norm(t) == norm(&s.name) && !norm(t).is_empty() });
            if let Some(j) = hit {
                used[j] = true;
                out[i] = Some(j);
            }
        }
    }
    out
}

/// A name for a new table from a file name: `Sales 2024.csv` → `sales_2024`.
pub fn table_name_for(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("imported");
    let mut s: String = stem.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_').to_string();
    match s.chars().next() {
        None => "imported".into(),
        Some(c) if c.is_ascii_digit() => format!("t_{s}"),
        _ => s,
    }
}

/// A column type name → the kind its literals follow (`INSERT`) and whether an empty value is
/// text (anything else gets NULL for it).
pub fn kind_of_type(type_name: &str) -> Kind {
    let t = type_name.trim().to_ascii_lowercase();
    if t.ends_with("[]") {
        return Kind::Other;
    }
    let base = t.split('(').next().unwrap_or("").trim().trim_matches('"');
    match base {
        "smallint" | "integer" | "bigint" | "int" | "int2" | "int4" | "int8" | "smallserial" | "serial" | "bigserial" | "serial2" | "serial4"
        | "serial8" | "oid" => Kind::Int,
        "real" | "float4" => Kind::Float4,
        "double precision" | "float8" | "float" => Kind::Float,
        "numeric" | "decimal" => Kind::Numeric,
        "boolean" | "bool" => Kind::Bool,
        "text" | "character varying" | "varchar" | "character" | "char" | "bpchar" | "name" | "citext" | "string" => Kind::Text,
        "bytea" => Kind::Bytes,
        "date" => Kind::Date,
        "time" | "time without time zone" | "timetz" | "time with time zone" => Kind::Time,
        "timestamp" | "timestamp without time zone" => Kind::Timestamp,
        "timestamptz" | "timestamp with time zone" => Kind::TimestampTz,
        "json" | "jsonb" => Kind::Json,
        _ => Kind::Other,
    }
}

// ---------------------------------------------------------------------------------------------
// Progress and outcome

/// Live progress of an import, shared with the UI (atomics it polls).
#[derive(Debug, Default)]
pub struct ImportProgress {
    /// Rows the server took (committed, or inside the open transaction).
    rows: AtomicU64,
    /// Rows read from the file so far.
    read: AtomicU64,
    skipped: AtomicU64,
    /// Bytes of the file consumed.
    bytes: AtomicU64,
    total_bytes: AtomicU64,
    /// Rows in the file when known up front (Parquet).
    total_rows: AtomicU64,
    cancel: AtomicBool,
    finished: AtomicBool,
    issues: Mutex<Vec<RowIssue>>,
}

impl ImportProgress {
    pub fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }
    pub fn read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }
    pub fn total_rows(&self) -> Option<u64> {
        Some(self.total_rows.load(Ordering::Relaxed)).filter(|t| *t > 0)
    }
    /// How far along, 0…1: by bytes, or by rows when only those are known.
    pub fn fraction(&self) -> Option<f64> {
        if let Some(t) = self.total_rows() {
            return Some((self.read() as f64 / t as f64).min(1.));
        }
        let tb = self.total_bytes();
        (tb > 0).then(|| (self.bytes() as f64 / tb as f64).min(1.))
    }
    /// Stops the import: the running chunk is abandoned; one transaction rolls back.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
    pub fn issues(&self) -> Vec<RowIssue> {
        self.issues.lock().clone()
    }

    /// A progress object with fixed numbers (screenshots of the progress view).
    pub fn fixed(rows: u64, bytes: u64, total_bytes: u64) -> Arc<ImportProgress> {
        let p = ImportProgress::default();
        p.rows.store(rows, Ordering::Relaxed);
        p.read.store(rows, Ordering::Relaxed);
        p.bytes.store(bytes, Ordering::Relaxed);
        p.total_bytes.store(total_bytes, Ordering::Relaxed);
        Arc::new(p)
    }

    pub(crate) fn skip(&self, line: u64, message: String) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
        let mut g = self.issues.lock();
        if g.len() < MAX_ISSUES {
            g.push(RowIssue { line, message });
        }
    }
}

/// The first skipped rows kept.
pub const MAX_ISSUES: usize = 100;

/// A row left out: where it is (a line, or a row for JSON arrays and Parquet) and why.
#[derive(Clone, Debug, PartialEq)]
pub struct RowIssue {
    pub line: u64,
    pub message: String,
}

/// Why an import failed.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportFailure {
    pub error: ErrorInfo,
    /// The bad row's line (or row, see [`ImportFormat::location_word`]), when known…
    pub line: Option<u64>,
    /// …else the lines of the batch it was in.
    pub lines: Option<(u64, u64)>,
}

impl ImportFailure {
    fn plain(message: impl Into<String>) -> ImportFailure {
        ImportFailure { error: ErrorInfo { message: message.into(), ..Default::default() }, line: None, lines: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Done,
    Cancelled,
    Failed(ImportFailure),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportReport {
    pub outcome: Outcome,
    /// Rows in the table from this import (0 when the transaction rolled back).
    pub rows: u64,
    pub skipped: u64,
    /// The first skipped rows.
    pub issues: Vec<RowIssue>,
    /// How the rows went (`Copy` or `Insert`; `Auto` when nothing was sent).
    pub method: Method,
    pub elapsed: Duration,
    /// Bytes of the file read.
    pub bytes: u64,
    /// Everything was rolled back (one transaction that failed or was cancelled).
    pub rolled_back: bool,
}

/// A running import: poll `progress`, await `task`.
pub struct Import {
    pub progress: Arc<ImportProgress>,
    pub task: Task<ImportReport>,
}

/// The `TRUNCATE`/`CREATE` errors and read-only refusals use SQLSTATEs like the server's.
fn refused(code: &str, message: impl Into<String>) -> ImportFailure {
    ImportFailure { error: ErrorInfo { code: Some(code.into()), message: message.into(), ..Default::default() }, line: None, lines: None }
}

impl crate::Connection {
    /// Imports `path` into `opts.schema.opts.table` of `database` (the connection's own when
    /// `None`), on a connection of its own. Returns at once; poll the progress, await the task.
    pub fn import(self: &Arc<Self>, database: Option<&str>, path: impl Into<PathBuf>, opts: ImportOptions) -> Import {
        let progress = Arc::new(ImportProgress::default());
        let (this, db, path, p) = (self.clone(), database.map(str::to_string), path.into(), progress.clone());
        let task = self.spawn(async move { Ok(load::run(this, db, path, opts, p).await) });
        Import { progress, task }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_from_names_and_bytes() {
        let p = Path::new;
        assert_eq!(detect_format(p("a.csv"), b"id,name\n"), ImportFormat::Csv);
        assert_eq!(detect_format(p("a.TSV"), b"id\tname\n"), ImportFormat::Tsv);
        assert_eq!(detect_format(p("a.json"), b"  [{\"a\":1}]"), ImportFormat::Json);
        assert_eq!(detect_format(p("a.json"), b"{\"a\":1}\n{\"a\":2}\n"), ImportFormat::Ndjson);
        assert_eq!(detect_format(p("a.jsonl"), b"{\"a\":1}\n"), ImportFormat::Ndjson);
        assert_eq!(detect_format(p("data"), b"PAR1\x15\x04"), ImportFormat::Parquet);
        assert_eq!(detect_format(p("a.txt"), b"x;y\n"), ImportFormat::Csv);
        assert_eq!(detect_format(p("dump"), b"\xef\xbb\xbf[{}]"), ImportFormat::Json);
        assert!(ImportFormat::is_importable(p("x.txt")) && !ImportFormat::is_importable(p("x.sql")));
    }

    #[test]
    fn mapping_and_names() {
        let src = |names: &[&str]| names.iter().map(|n| SourceColumn { name: n.to_string(), ty: PgType::Text }).collect::<Vec<_>>();
        let tgt = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(auto_map(&src(&["id", "Customer ID", "junk", "total"]), &tgt(&["total", "customer_id", "id"]), true), vec![Some(2), Some(1), None, Some(0)]);
        // An exact name wins over a loose one, each target once.
        assert_eq!(auto_map(&src(&["Name", "name"]), &tgt(&["name"]), true), vec![None, Some(0)]);
        assert_eq!(auto_map(&src(&["column_1", "column_2", "column_3"]), &tgt(&["a", "b"]), false), vec![Some(0), Some(1), None]);
        assert_eq!(table_name_for(Path::new("/x/Sales 2024 (final).csv")), "sales_2024_final");
        assert_eq!(table_name_for(Path::new("2024.csv")), "t_2024");
        assert_eq!(table_name_for(Path::new("---.csv")), "imported");
    }

    #[test]
    fn type_names() {
        assert_eq!(kind_of_type("integer"), Kind::Int);
        assert_eq!(kind_of_type("numeric(12,2)"), Kind::Numeric);
        assert_eq!(kind_of_type("character varying(40)"), Kind::Text);
        assert_eq!(kind_of_type("timestamp with time zone"), Kind::TimestampTz);
        assert_eq!(kind_of_type("TIMESTAMP"), Kind::Timestamp);
        assert_eq!(kind_of_type("jsonb"), Kind::Json);
        assert_eq!(kind_of_type("uuid"), Kind::Other);
        assert_eq!(kind_of_type("text[]"), Kind::Other);
    }

    fn opts() -> ImportOptions {
        ImportOptions {
            format: ImportFormat::Csv,
            dialect: CsvDialect::default(),
            source_names: vec!["id".into(), "Name".into()],
            schema: "Sales".into(),
            table: "order".into(),
            create: true,
            columns: vec![
                ColumnMap { source: 0, target: "id".into(), type_name: "int8".into() },
                ColumnMap { source: 1, target: "Name".into(), type_name: "text".into() },
            ],
            truncate: false,
            on_error: OnError::Stop,
            one_transaction: true,
            method: Method::Auto,
            insert_rows: 0,
        }
    }

    #[test]
    fn statements_quote_names() {
        let mut o = opts();
        assert_eq!(o.statements(), vec!["CREATE TABLE \"Sales\".\"order\" (id int8, \"Name\" text)", "COPY \"Sales\".\"order\" (id, \"Name\") FROM STDIN"]);
        o.create = false;
        o.truncate = true;
        assert_eq!(o.statements()[0], "TRUNCATE TABLE \"Sales\".\"order\"");
        assert!(o.check().is_ok());
        o.columns[1].target = "id".into();
        assert!(o.check().unwrap_err().contains("twice"));
        o.columns.clear();
        assert!(o.check().is_err());
    }

    #[test]
    fn the_guard_sees_what_the_import_runs() {
        use crate::Environment;
        use crate::guard::review;
        let mut o = opts();
        o.create = false;
        // A plain load on a normal connection: nothing to confirm.
        assert!(review(&o.statements(), Environment::None).is_empty());
        // Production: the COPY is a write.
        let f = review(&o.statements(), Environment::Production);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "write");
        // Truncate first: asked everywhere.
        o.truncate = true;
        let f = review(&o.statements(), Environment::None);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "truncate");
        // A new table on production: the DDL and the write.
        let f = review(&opts().statements(), Environment::Production);
        assert_eq!(f.iter().map(|f| f.rule).collect::<Vec<_>>(), vec!["ddl", "write"]);
    }
}
