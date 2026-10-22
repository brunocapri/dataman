//! DuckDB, compiled in (`bundled`, with json and parquet). A file is opened once per process: a
//! registry keeps each file's instance, and every connection to it (catalog reads, sessions,
//! agents) is a clone of the instance's own. Two `Connection::open` of one file in a process
//! make two databases writing the same file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use duckdb::arrow::datatypes::{DataType, TimeUnit as ArrowUnit};
use duckdb::types::Value;
use parking_lot::Mutex;

use super::Ran;
use super::embed::{self, Columns, Sink, Val, Worker};
use crate::config::ConnectionProfile;
use crate::guard::TxCmd;
use crate::result::{ColumnInfo, ErrorInfo, ResultSet};
use crate::server::{ServerExtension, ServerInfo};
use crate::sql::Dialect;
use crate::tx::{Seen, TxWatch};
use crate::value::Kind;

pub(crate) type DuckError = duckdb::Error;

/// The profile option that lets SQL read and write files (`read_csv`, `COPY … TO`, ATTACH…);
/// on unless the user turns it off.
pub const EXTERNAL_ACCESS: &str = "duckdb.external_access";

fn closed_error() -> DuckError {
    duckdb::Error::DuckDBFailure(duckdb::ffi::Error::new(duckdb::ffi::DuckDBError), Some("the connection is closed".into()))
}

/// One open database file: the connection every other one is cloned from.
pub(crate) struct Instance {
    root: Mutex<duckdb::Connection>,
    read_only: bool,
    external: bool,
}

static REGISTRY: Mutex<Option<HashMap<PathBuf, Weak<Instance>>>> = Mutex::new(None);

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The file's instance, opened now or shared: in the mode asked for, or an error that says which
/// mode holds it.
fn instance(path: &Path, read_only: bool, external: bool) -> Result<Arc<Instance>> {
    if !path.is_file() {
        bail!("{} does not exist", path.display());
    }
    let key = canonical(path);
    let mut g = REGISTRY.lock();
    let map = g.get_or_insert_with(HashMap::new);
    map.retain(|_, w| w.strong_count() > 0);
    if let Some(i) = map.get(&key).and_then(Weak::upgrade) {
        if i.read_only != read_only {
            let held = if i.read_only { "read-only" } else { "read-write" };
            bail!("{} is already open {held} in another connection: close that one first", path.display());
        }
        if i.external != external {
            bail!("{} is already open with file access {} in another connection: close that one first", path.display(), if i.external { "on" } else { "off" });
        }
        return Ok(i);
    }
    // An empty file (`touch shop.duckdb`) holds no data, but DuckDB refuses it as "not a valid
    // DuckDB database file": opened read-write it becomes a new database, as SQLite does with one.
    let empty = std::fs::metadata(&key).is_ok_and(|m| m.len() == 0);
    if empty && read_only {
        bail!("{} is empty: it holds no DuckDB database yet (open it once without read-only to create one)", path.display());
    }
    if !empty && crate::import::detect_database(&key) != Some(crate::config::Engine::DuckDb) {
        bail!("{} is not a DuckDB database file", path.display());
    }
    if empty {
        std::fs::remove_file(&key).with_context(|| format!("replacing the empty {}", path.display()))?;
    }
    let mut cfg = duckdb::Config::default()
        .access_mode(if read_only { duckdb::AccessMode::ReadOnly } else { duckdb::AccessMode::ReadWrite })?
        // Extensions not compiled in are never fetched nor loaded (the app's signature allows no
        // unsigned code, and agents would reach the network).
        .with("autoinstall_known_extensions", "false")?
        .with("autoload_known_extensions", "false")?;
    if !external {
        cfg = cfg.enable_external_access(false)?.with("lock_configuration", "true")?;
    }
    let root = duckdb::Connection::open_with_flags(&key, cfg).with_context(|| format!("opening {}", path.display()))?;
    root.query_row("SELECT 1", [], |_| Ok(())).with_context(|| format!("{} is not a DuckDB database", path.display()))?;
    if empty {
        root.execute_batch("CHECKPOINT").with_context(|| format!("creating {}", path.display()))?;
    }
    let i = Arc::new(Instance { root: Mutex::new(root), read_only, external });
    map.insert(key, Arc::downgrade(&i));
    Ok(i)
}

/// A new, empty database file (the sheet's New…).
pub fn create(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("{} exists already", path.display());
    }
    duckdb::Connection::open(path).with_context(|| format!("creating {}", path.display()))?.execute_batch("CHECKPOINT")?;
    Ok(())
}

pub(crate) fn external_access(p: &ConnectionProfile) -> bool {
    p.options.get(EXTERNAL_ACCESS).is_none_or(|v| v != "false")
}

pub(crate) struct Conn {
    db: duckdb::Connection,
    watch: Option<Arc<TxWatch>>,
    /// DuckDB does not say: the transaction's state as the statements run left it.
    tx: u8,
}

impl Conn {
    fn report(&self) {
        if let Some(w) = &self.watch {
            w.seen(Seen::Ready(self.tx));
        }
    }

    fn track(&mut self, sql: &str, ok: bool) {
        let cmd = crate::guard::classify_in(sql, Dialect::DuckDb).tx;
        self.tx = match (cmd, ok) {
            (Some(TxCmd::Begin), true) => b'T',
            (Some(TxCmd::Commit | TxCmd::Rollback), _) => b'I',
            (_, false) if self.tx != b'I' => b'E',
            _ => self.tx,
        };
    }
}

pub(crate) struct Client {
    worker: Worker<Conn>,
    interrupt: Arc<duckdb::InterruptHandle>,
    busy: Arc<AtomicBool>,
    watch: Option<Arc<TxWatch>>,
    _instance: Arc<Instance>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(w) = &self.watch {
            w.closed();
        }
    }
}

struct Busy(Arc<AtomicBool>);

impl Busy {
    fn new(b: &Arc<AtomicBool>) -> Busy {
        b.store(true, Ordering::Release);
        Busy(b.clone())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A connection to `database` (a catalog of the instance; `None`: its default). The instance's
/// mode is the profile's own read-only, whatever the database overrides say (one mode per file).
pub(crate) async fn connect(p: &ConnectionProfile, database: Option<&str>, watch: Option<&Arc<TxWatch>>) -> Result<Arc<Client>> {
    let path = p.file_path();
    let (read_only, external) = (p.read_only, external_access(p));
    let inst = tokio::task::spawn_blocking(move || instance(&path, read_only, external)).await??;
    let root = inst.root.lock().try_clone()?;
    let catalog = database.filter(|d| !d.is_empty()).map(str::to_string);
    let w = watch.cloned();
    let worker = Worker::start("duckdb", move || {
        if let Some(c) = &catalog {
            let current: String = root.query_row("SELECT current_database()", [], |r| r.get(0))?;
            if *c != current {
                root.execute_batch(&format!("USE {}", Dialect::DuckDb.quote_ident(c)))?;
            }
        }
        Ok(Conn { db: root, watch: w, tx: b'I' })
    })
    .await?;
    let interrupt = worker.call(|c| c.db.interrupt_handle()).await.ok_or_else(|| anyhow!("the DuckDB connection closed"))?;
    Ok(Arc::new(Client { worker, interrupt, busy: Arc::new(AtomicBool::new(false)), watch: watch.cloned(), _instance: inst }))
}

impl Client {
    pub(crate) fn is_closed(&self) -> bool {
        self.worker.is_closed()
    }

    pub(crate) fn canceller(&self) -> Canceller {
        Canceller { interrupt: self.interrupt.clone(), busy: self.busy.clone() }
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        self.canceller().cancel().await
    }

    async fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut Conn) -> Result<R, DuckError> + Send + 'static) -> Result<R, DuckError> {
        let busy = self.busy.clone();
        self.worker
            .call(move |c| {
                let _b = Busy::new(&busy);
                let r = f(c);
                c.report();
                r
            })
            .await
            .unwrap_or_else(|| Err(closed_error()))
    }

    pub(crate) async fn execute(&self, sql: &str) -> Result<Option<u64>, DuckError> {
        let sql = sql.to_string();
        self.call(move |c| {
            let mut n = None;
            for s in statements(&sql) {
                let r = run_one(&c.db, &s, &[], &mut |_| true);
                c.track(&s, r.is_ok());
                n = Some(r?.1);
            }
            Ok(n)
        })
        .await
    }

    pub(crate) async fn text_rows(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>, DuckError> {
        let sql = sql.to_string();
        self.call(move |c| {
            let mut out = Vec::new();
            for s in statements(&sql) {
                let r = run_one(&c.db, &s, &[], &mut |row| {
                    out.push(row.iter().map(Val::text).collect());
                    true
                });
                c.track(&s, r.is_ok());
                r?;
            }
            Ok(out)
        })
        .await
    }

    /// The columns of a query: DuckDB knows them once it has run, so this runs it (a read).
    pub(crate) async fn prepare(&self, sql: &str) -> Result<Vec<ColumnInfo>, DuckError> {
        let sql = sql.to_string();
        self.call(move |c| {
            let mut stmt = c.db.prepare(&sql)?;
            let rows = stmt.query([])?;
            Ok(rows.as_ref().map(|s| columns_of(s).0).unwrap_or_default())
        })
        .await
    }

    pub(crate) async fn stream(&self, sql: &str, params: &[Option<String>]) -> Result<(Columns, embed::Rows<DuckError>), DuckError> {
        let (sink, first, rx) = Sink::new();
        let sql = sql.to_string();
        let params: Vec<Val> = params.iter().map(embed::param_val).collect();
        let busy = self.busy.clone();
        if !self.worker.post(move |c| {
            let _b = Busy::new(&busy);
            stream_on(c, &sql, &params, sink);
        }) {
            return Err(closed_error());
        }
        let cols = first.await.unwrap_or_else(|_| Err(closed_error()))?;
        Ok((cols, embed::Rows::new(rx, None, closed_error)))
    }
}

/// One statement at a time (DuckDB prepares one), as the dialect's splitter reads them.
fn statements(sql: &str) -> Vec<String> {
    crate::sql::split_in(sql, Dialect::DuckDb).into_iter().map(|s| sql[s.range].to_string()).filter(|s| !s.trim().is_empty()).collect()
}

fn bind_values(params: &[Val]) -> Vec<Value> {
    params
        .iter()
        .map(|v| match v {
            Val::Null => Value::Null,
            Val::Bool(b) => Value::Boolean(*b),
            Val::Int(i) => Value::BigInt(*i),
            Val::Float(f) => Value::Double(*f),
            Val::Text(s) => Value::Text(s.clone()),
            Val::Blob(b) => Value::Blob(b.clone()),
        })
        .collect()
}

/// A DML statement's result is one `Count` column: what it changed, not rows to show.
fn is_count(sql: &str, cols: &[ColumnInfo]) -> bool {
    cols.len() == 1 && cols[0].name == "Count" && crate::guard::classify_in(sql, Dialect::DuckDb).is_write()
}

/// Runs one statement, each row to `row` (false stops reading); its columns, and the rows read
/// or changed.
fn run_one(db: &duckdb::Connection, sql: &str, params: &[Val], row: &mut dyn FnMut(Vec<Val>) -> bool) -> Result<(Columns, u64), DuckError> {
    let mut stmt = db.prepare(sql)?;
    let values = bind_values(params);
    let mut rows = stmt.query(duckdb::params_from_iter(values.iter().take(stmt_params(sql, params.len()))))?;
    let cols = rows.as_ref().map(columns_of).unwrap_or_default();
    let width = cols.0.len();
    let count = is_count(sql, &cols.0);
    let mut n = 0u64;
    while let Some(r) = rows.next()? {
        let vals: Vec<Val> = (0..width).map(|i| r.get::<_, Value>(i).map(val).unwrap_or(Val::Null)).collect();
        if count {
            n = match vals.first() {
                Some(Val::Int(c)) => *c as u64,
                _ => 0,
            };
            continue;
        }
        n += 1;
        if !row(vals) {
            break;
        }
    }
    Ok((if count { Default::default() } else { cols }, n))
}

/// How many of the values the statement takes (a script's other statements take none).
fn stmt_params(sql: &str, given: usize) -> usize {
    let n = crate::sql::tokenize_in(sql, Dialect::DuckDb).iter().filter(|t| t.kind == crate::sql::TokenKind::Param).count();
    if n == 0 { 0 } else { given }
}

fn stream_on(c: &mut Conn, sql: &str, params: &[Val], mut sink: Sink<DuckError>) {
    let stmts = statements(sql);
    let (mut shown, mut affected, mut last, mut err) = (false, None, 0, None);
    for s in &stmts {
        let p = if stmts.len() == 1 { params } else { &[] };
        let r = run_one_streaming(&c.db, s, p, &mut sink);
        c.track(s, r.is_ok());
        match r {
            Ok((false, n)) => {
                if !shown {
                    affected = Some(n);
                }
            }
            Ok((true, n)) => {
                shown = true;
                last = n;
            }
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    c.report();
    sink.end(match err {
        Some(e) => Err(e),
        None => Ok(if shown { Some(last) } else { affected }),
    });
}

/// As [`run_one`], into the sink: whether a result was sent (a DML's count is none: the number
/// of changed rows comes back), and its rows.
fn run_one_streaming(db: &duckdb::Connection, sql: &str, params: &[Val], sink: &mut Sink<DuckError>) -> Result<(bool, u64), DuckError> {
    let mut stmt = db.prepare(sql)?;
    let values = bind_values(params);
    let mut rows = stmt.query(duckdb::params_from_iter(values.iter().take(stmt_params(sql, params.len()))))?;
    let cols = rows.as_ref().map(columns_of).unwrap_or_default();
    let width = cols.0.len();
    if is_count(sql, &cols.0) {
        return Ok((false, match rows.next()? {
            Some(r) => r.get::<_, i64>(0).unwrap_or(0) as u64,
            None => 0,
        }));
    }
    if width == 0 {
        while rows.next()?.is_some() {}
        return Ok((false, 0));
    }
    sink.columns(cols);
    let mut n = 0u64;
    while let Some(r) = rows.next()? {
        n += 1;
        // A reader that went away (an export stopped): the statement just ends.
        if !sink.row((0..width).map(|i| r.get::<_, Value>(i).map(val).unwrap_or(Val::Null)).collect()) {
            break;
        }
    }
    Ok((true, n))
}

const EPOCH_DAYS: i32 = 10_957;
const EPOCH_MICROS: i64 = 946_684_800_000_000;


/// A column's kind and DuckDB's name for its type, from the result's (arrow) schema.
pub(crate) fn kind_of(t: &DataType) -> (Kind, String) {
    let name = |s: &str| s.to_string();
    match t {
        DataType::Boolean => (Kind::Bool, name("BOOLEAN")),
        DataType::Int8 => (Kind::Int, name("TINYINT")),
        DataType::Int16 => (Kind::Int, name("SMALLINT")),
        DataType::Int32 => (Kind::Int, name("INTEGER")),
        DataType::Int64 => (Kind::Int, name("BIGINT")),
        DataType::UInt8 => (Kind::Int, name("UTINYINT")),
        DataType::UInt16 => (Kind::Int, name("USMALLINT")),
        DataType::UInt32 => (Kind::Int, name("UINTEGER")),
        // Past i64::MAX a value turns the column to text, as it is.
        DataType::UInt64 => (Kind::Int, name("UBIGINT")),
        DataType::Float16 | DataType::Float32 => (Kind::Float4, name("FLOAT")),
        DataType::Float64 => (Kind::Float, name("DOUBLE")),
        DataType::Decimal128(38, 0) => (Kind::Numeric, name("HUGEINT")),
        DataType::Decimal128(p, s) | DataType::Decimal256(p, s) => (Kind::Numeric, format!("DECIMAL({p},{s})")),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => (Kind::Text, name("VARCHAR")),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => (Kind::Bytes, name("BLOB")),
        DataType::FixedSizeBinary(16) => (Kind::Text, name("UUID")),
        DataType::Date32 | DataType::Date64 => (Kind::Date, name("DATE")),
        DataType::Time32(_) | DataType::Time64(_) => (Kind::Time, name("TIME")),
        DataType::Timestamp(u, None) => (Kind::Timestamp, match u {
            ArrowUnit::Second => name("TIMESTAMP_S"),
            ArrowUnit::Millisecond => name("TIMESTAMP_MS"),
            ArrowUnit::Microsecond => name("TIMESTAMP"),
            ArrowUnit::Nanosecond => name("TIMESTAMP_NS"),
        }),
        DataType::Timestamp(_, Some(_)) => (Kind::TimestampTz, name("TIMESTAMP WITH TIME ZONE")),
        DataType::Interval(_) | DataType::Duration(_) => (Kind::Other, name("INTERVAL")),
        DataType::Dictionary(_, _) => (Kind::Text, name("ENUM")),
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(_, _) | DataType::ListView(_) => (Kind::Json, name("LIST")),
        DataType::Struct(_) => (Kind::Json, name("STRUCT")),
        DataType::Map(_, _) => (Kind::Json, name("MAP")),
        DataType::Union(_, _) => (Kind::Json, name("UNION")),
        DataType::Null => (Kind::Other, name("NULL")),
        other => (Kind::Other, format!("{other}")),
    }
}

fn columns_of(stmt: &duckdb::Statement<'_>) -> Columns {
    let n = stmt.column_count();
    let infos = (0..n)
        .map(|i| {
            let (kind, type_name) = kind_of(&stmt.column_type(i));
            ColumnInfo { name: stmt.column_name(i).cloned().unwrap_or_default(), type_name, type_oid: 0, kind }
        })
        .collect();
    (infos, vec![None; n])
}

/// A cell as stored, dates and times in the kinds' epochs (2000-01-01, µs).
fn val(v: Value) -> Val {
    match v {
        Value::Null => Val::Null,
        Value::Boolean(b) => Val::Bool(b),
        Value::TinyInt(x) => Val::Int(x as i64),
        Value::SmallInt(x) => Val::Int(x as i64),
        Value::Int(x) => Val::Int(x as i64),
        Value::BigInt(x) => Val::Int(x),
        Value::UTinyInt(x) => Val::Int(x as i64),
        Value::USmallInt(x) => Val::Int(x as i64),
        Value::UInt(x) => Val::Int(x as i64),
        Value::UBigInt(x) => i64::try_from(x).map(Val::Int).unwrap_or_else(|_| Val::Text(x.to_string())),
        Value::HugeInt(x) => Val::Text(x.to_string()),
        Value::UHugeInt(x) => Val::Text(x.to_string()),
        Value::Float(f) => Val::Float(f as f64),
        Value::Double(f) => Val::Float(f),
        Value::Decimal(d) => Val::Text(d.to_string()),
        Value::Timestamp(u, x) => Val::Int(u.to_micros(x) - EPOCH_MICROS),
        Value::Text(s) => Val::Text(s),
        Value::Blob(b) | Value::Geometry(b) => Val::Blob(b),
        Value::Date32(d) => Val::Int((d - EPOCH_DAYS) as i64),
        Value::Time64(u, x) => Val::Int(u.to_micros(x)),
        Value::Interval { months, days, nanos } => {
            let mut s = String::new();
            crate::value::format_interval(months, days, nanos / 1000, &mut s);
            Val::Text(s)
        }
        Value::Enum(s) => Val::Text(s),
        nested => Val::Text(json(&nested).to_string()),
        #[allow(unreachable_patterns)]
        _ => Val::Null,
    }
}

/// LIST, ARRAY, STRUCT, MAP and UNION as JSON (a MAP with text keys as an object).
fn json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Float(f) => serde_json::Number::from_f64(*f as f64).map_or(J::Null, J::Number),
        Value::Double(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        Value::List(xs) | Value::Array(xs) => J::Array(xs.iter().map(json).collect()),
        Value::Struct(m) => J::Object(m.iter().map(|(k, v)| (k.clone(), json(v))).collect()),
        Value::Map(m) => {
            let text_keys = m.keys().all(|k| matches!(k, Value::Text(_)));
            if text_keys {
                J::Object(m.iter().map(|(k, v)| (if let Value::Text(t) = k { t.clone() } else { String::new() }, json(v))).collect())
            } else {
                J::Array(m.iter().map(|(k, v)| serde_json::json!({"key": json(k), "value": json(v)})).collect())
            }
        }
        Value::Union(x) => json(x),
        other => match val(other.clone()) {
            Val::Null => J::Null,
            Val::Bool(b) => J::Bool(b),
            Val::Int(i) => match other {
                Value::Date32(_) | Value::Timestamp(..) | Value::Time64(..) => J::String(scalar_text(other)),
                _ => J::from(i),
            },
            Val::Float(f) => serde_json::Number::from_f64(f).map_or(J::Null, J::Number),
            Val::Text(s) => J::String(s),
            Val::Blob(b) => {
                let mut s = String::new();
                embed::hex(&b, &mut s);
                J::String(s)
            }
        },
    }
}

/// A date or time inside a nested value, as the grid would show it.
fn scalar_text(v: &Value) -> String {
    let mut s = String::new();
    match v {
        Value::Date32(d) => crate::value::format_date(d - EPOCH_DAYS, &mut s),
        Value::Time64(u, x) => crate::value::format_time(u.to_micros(*x), &mut s),
        Value::Timestamp(u, x) => crate::value::format_timestamp(u.to_micros(*x) - EPOCH_MICROS, &mut s),
        _ => {}
    }
    s
}

#[derive(Clone)]
pub(crate) struct Canceller {
    interrupt: Arc<duckdb::InterruptHandle>,
    busy: Arc<AtomicBool>,
}

impl Canceller {
    pub(crate) async fn cancel(&self) -> Result<()> {
        if self.busy.load(Ordering::Acquire) {
            self.interrupt.interrupt();
        }
        Ok(())
    }
}

pub(crate) async fn server_info(client: &Client, exts: &[Arc<dyn ServerExtension>]) -> Result<ServerInfo> {
    let rows = client.text_rows("SELECT version(), current_database(), current_schema()").await?;
    let row = rows.into_iter().next().context("version() returned no row")?;
    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let v = get(0);
    let mut info = ServerInfo { version: format!("DuckDB {}", v.trim_start_matches('v')), database: get(1), user: String::new(), ..Default::default() };
    info.search_path = vec![Some(get(2)).filter(|s| !s.is_empty()).unwrap_or_else(|| "main".into())];
    info.extensions = exts.iter().filter(|e| e.detect(&info)).map(|e| e.id()).collect();
    Ok(info)
}

pub(crate) async fn search_path(client: &Client) -> Vec<String> {
    match client.text_rows("SELECT current_schema()").await {
        Ok(rows) => rows.first().and_then(|r| r.first().cloned().flatten()).into_iter().collect(),
        Err(_) => vec!["main".into()],
    }
}

fn message(e: &DuckError) -> String {
    match e {
        duckdb::Error::DuckDBFailure(_, Some(m)) => m.clone(),
        e => e.to_string(),
    }
}

/// DuckDB's errors carry their class in front (`Constraint Error: Duplicate key …`): the
/// constraint ones get codes the edit's explanations know.
pub(crate) fn error_info(e: &DuckError) -> ErrorInfo {
    let m = message(e);
    let (class, rest) = m.split_once(": ").filter(|(c, _)| c.ends_with("Error") && !c.contains('\n')).unwrap_or(("", m.as_str()));
    let code = if class == "Constraint Error" {
        let r = rest.to_ascii_lowercase();
        Some(
            if r.contains("duplicate key") {
                "DUCKDB_CONSTRAINT_UNIQUE"
            } else if r.contains("not null") {
                "DUCKDB_CONSTRAINT_NOTNULL"
            } else if r.contains("foreign key") {
                "DUCKDB_CONSTRAINT_FOREIGNKEY"
            } else if r.contains("check constraint") {
                "DUCKDB_CONSTRAINT_CHECK"
            } else {
                "DUCKDB_CONSTRAINT"
            }
            .to_string(),
        )
    } else if class.is_empty() {
        None
    } else {
        Some(class.to_string())
    };
    ErrorInfo { code, message: if class.is_empty() { m.clone() } else { rest.to_string() }, severity: Some("ERROR".into()), ..Default::default() }
}

pub(crate) fn error_info_at(e: &DuckError, _sql: &str) -> ErrorInfo {
    error_info(e)
}

pub(crate) fn interrupted(e: &DuckError) -> bool {
    message(e).starts_with("INTERRUPT Error")
}

pub(crate) async fn run(
    client: &Arc<Client>,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
    aborted: &(dyn Fn() -> bool + Sync),
    next: &(dyn Fn(&Arc<ResultSet>) -> Arc<ResultSet> + Sync),
) -> Ran {
    let fail = |e: &DuckError| if interrupted(e) { Ran::Cancelled } else { Ran::Failed(error_info_at(e, &rs.sql)) };
    if aborted() {
        return Ran::NeverRan { settle: false };
    }
    rs.executing.store(true, Ordering::Release);
    let ((infos, origins), mut rows) = match client.stream(&rs.sql, rs.params()).await {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    rs.set_named_origins(origins);
    rs.set_columns(infos);
    rows.show_in(rs);
    let mut cur = rs.clone();
    let mut cols = rs.columns().unwrap_or_default();
    loop {
        let n = match super::pump(&mut rows, &cur, &cols, started, on_pause).await {
            Ok(n) => n,
            Err(e) => return fail(&e),
        };
        let Some((infos, origins)) = rows.next_set() else { return Ran::Done(n) };
        if cur.demand().borrow().abort && !cur.status().partial {
            let _ = client.cancel().await;
            return Ran::Cancelled;
        }
        cur = next(&cur);
        cur.set_named_origins(origins);
        cur.set_columns(infos);
        rows.show_in(&cur);
        cols = cur.columns().unwrap_or_default();
    }
}
