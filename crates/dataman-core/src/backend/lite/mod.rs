//! SQLite, compiled in (rusqlite, `bundled`). Each connection is a thread of its own
//! ([`Worker`]); a session, the explorer's catalog reads and an agent's reads each open their own
//! connection to the file, as with a server.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use fallible_iterator_03::FallibleIterator;
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Batch, OpenFlags, ffi};

use super::embed::{self, Columns, Sink, Val, Worker};
use super::Ran;
use crate::config::ConnectionProfile;
use crate::result::{ColumnInfo, ErrorInfo, NamedOrigin, ResultSet};
use crate::server::{ServerExtension, ServerInfo};
use crate::tx::{Seen, TxWatch};
use crate::value::Kind;

pub(crate) type LiteError = rusqlite::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

fn closed_error() -> LiteError {
    rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_MISUSE), Some("the connection is closed".into()))
}

/// How a connection is opened.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Open {
    pub read_only: bool,
    /// An agent's connection: escapes refused by SQLite itself (see [`harden`]).
    pub agent: Option<Agent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Agent {
    Read,
    Write,
}

/// SQLite itself refuses an agent what would leave the database or change it behind the
/// classifier's back: no ATTACH (and no attached database at all), no extension or file
/// function, no PRAGMA that sets anything for a reader, no write for a reader, no virtual table;
/// defensive mode keeps `writable_schema` and the like from corrupting the file.
fn harden(db: &rusqlite::Connection, agent: Agent) -> Result<()> {
    use rusqlite::config::DbConfig;
    use rusqlite::limits::Limit;
    db.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    db.authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| authorize(agent, &ctx.action)))?;
    Ok(())
}

pub(crate) fn authorize(agent: Agent, a: &rusqlite::hooks::AuthAction<'_>) -> rusqlite::hooks::Authorization {
    use rusqlite::hooks::AuthAction as A;
    use rusqlite::hooks::Authorization::{Allow, Deny};
    let read = agent == Agent::Read;
    match a {
        A::Attach { .. } | A::Detach { .. } | A::CreateVtable { .. } | A::DropVtable { .. } => Deny,
        A::Function { function_name } if crate::guard::SQLITE_EFFECT_FUNCTIONS.iter().any(|f| f.eq_ignore_ascii_case(function_name)) => Deny,
        // Settings that weaken the file or the checks are not an agent's at any level.
        A::Pragma { pragma_name, pragma_value } if pragma_value.is_some() && ["writable_schema", "trusted_schema", "journal_mode", "locking_mode", "query_only", "cell_size_check", "mmap_size"].iter().any(|p| p.eq_ignore_ascii_case(pragma_name)) => Deny,
        A::Pragma { pragma_name, pragma_value } if read && crate::guard::pragma_sets(pragma_name, pragma_value.is_some()) => Deny,
        A::Insert { .. }
        | A::Update { .. }
        | A::Delete { .. }
        | A::CreateIndex { .. }
        | A::CreateTable { .. }
        | A::CreateTempIndex { .. }
        | A::CreateTempTable { .. }
        | A::CreateTempTrigger { .. }
        | A::CreateTempView { .. }
        | A::CreateTrigger { .. }
        | A::CreateView { .. }
        | A::DropIndex { .. }
        | A::DropTable { .. }
        | A::DropTempIndex { .. }
        | A::DropTempTable { .. }
        | A::DropTempTrigger { .. }
        | A::DropTempView { .. }
        | A::DropTrigger { .. }
        | A::DropView { .. }
        | A::AlterTable { .. }
        | A::Reindex { .. }
        | A::Analyze { .. }
            if read =>
        {
            Deny
        }
        _ => Allow,
    }
}

pub(crate) struct Conn {
    db: rusqlite::Connection,
    watch: Option<Arc<TxWatch>>,
}

impl Conn {
    fn report(&self) {
        if let Some(w) = &self.watch {
            w.seen(Seen::Ready(if self.db.is_autocommit() { b'I' } else { b'T' }));
        }
    }
}

fn open_file(path: &Path, how: Open) -> Result<rusqlite::Connection> {
    if !path.is_file() {
        anyhow::bail!("{} does not exist", path.display());
    }
    // No URI filenames: a path is a path.
    let flags = if how.read_only { OpenFlags::SQLITE_OPEN_READ_ONLY } else { OpenFlags::SQLITE_OPEN_READ_WRITE } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let db = rusqlite::Connection::open_with_flags(path, flags).with_context(|| format!("opening {}", path.display()))?;
    db.busy_timeout(BUSY_TIMEOUT)?;
    // Per connection, and what users expect of their schema's REFERENCES.
    db.pragma_update(None, "foreign_keys", true)?;
    if how.read_only {
        // The file is opened read-only too; this says so in the error of a write.
        db.pragma_update(None, "query_only", true)?;
    }
    if let Some(agent) = how.agent {
        harden(&db, agent)?;
    }
    // SQLite opens lazily: a file that is not a database fails here, not on the first query.
    db.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(())).with_context(|| format!("{} is not a SQLite database", path.display()))?;
    Ok(db)
}

pub(crate) struct Client {
    worker: Worker<Conn>,
    interrupt: Arc<rusqlite::InterruptHandle>,
    busy: Arc<AtomicBool>,
    watch: Option<Arc<TxWatch>>,
}

pub(crate) async fn connect(p: &ConnectionProfile, how: Open, watch: Option<&Arc<TxWatch>>) -> Result<Arc<Client>> {
    let file = p.file_path();
    let w = watch.cloned();
    let worker = Worker::start("sqlite", move || Ok(Conn { db: open_file(&file, how)?, watch: w })).await?;
    let interrupt = worker.call(|c| c.db.get_interrupt_handle()).await.ok_or_else(|| anyhow!("the SQLite connection closed"))?;
    Ok(Arc::new(Client { worker, interrupt: Arc::new(interrupt), busy: Arc::new(AtomicBool::new(false)), watch: watch.cloned() }))
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(w) = &self.watch {
            w.closed();
        }
    }
}

/// Marks the connection busy while a statement runs: a cancel interrupts only then.
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

    async fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut Conn) -> Result<R, LiteError> + Send + 'static) -> Result<R, LiteError> {
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

    pub(crate) async fn execute(&self, sql: &str) -> Result<Option<u64>, LiteError> {
        let sql = sql.to_string();
        self.call(move |c| execute_on(&c.db, &sql)).await
    }

    pub(crate) async fn text_rows(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>, LiteError> {
        let sql = sql.to_string();
        self.call(move |c| text_rows_on(&c.db, &sql)).await
    }

    /// The columns a statement returns, as its declared types say (before any row).
    pub(crate) async fn prepare(&self, sql: &str) -> Result<Vec<ColumnInfo>, LiteError> {
        let sql = sql.to_string();
        self.call(move |c| {
            let stmt = c.db.prepare(&sql)?;
            Ok(columns_of(&stmt, None).0)
        })
        .await
    }

    /// Runs `sql` (one statement or several), its rows coming through the returned channel.
    pub(crate) async fn stream(&self, sql: &str, params: &[Option<String>], kinds: Option<Vec<Kind>>) -> Result<(Columns, embed::Rows<LiteError>), LiteError> {
        let (sink, first, rx) = Sink::new();
        let sql = sql.to_string();
        let params: Vec<Val> = params.iter().map(embed::param_val).collect();
        let busy = self.busy.clone();
        if !self.worker.post(move |c| {
            let _b = Busy::new(&busy);
            stream_on(c, &sql, &params, kinds, sink);
        }) {
            return Err(closed_error());
        }
        let cols = first.await.unwrap_or_else(|_| Err(closed_error()))?;
        Ok((cols, embed::Rows::new(rx, None, closed_error)))
    }
}

struct Bind<'a>(&'a Val);

impl rusqlite::ToSql for Bind<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self.0 {
            Val::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Val::Bool(b) => ToSqlOutput::Borrowed(ValueRef::Integer(*b as i64)),
            Val::Int(v) => ToSqlOutput::Borrowed(ValueRef::Integer(*v)),
            Val::Float(f) => ToSqlOutput::Borrowed(ValueRef::Real(*f)),
            Val::Text(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            Val::Blob(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
        })
    }
}

fn bind(stmt: &mut rusqlite::Statement<'_>, params: &[Val]) -> Result<(), LiteError> {
    let n = stmt.parameter_count();
    if n > params.len() {
        return Err(rusqlite::Error::InvalidParameterCount(params.len(), n));
    }
    for (i, v) in params.iter().take(n).enumerate() {
        stmt.raw_bind_parameter(i + 1, Bind(v))?;
    }
    Ok(())
}

fn val(v: ValueRef<'_>) -> Val {
    match v {
        ValueRef::Null => Val::Null,
        ValueRef::Integer(i) => Val::Int(i),
        ValueRef::Real(f) => Val::Float(f),
        ValueRef::Text(t) => Val::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Val::Blob(b.to_vec()),
    }
}

/// The kind a declared type gives, by SQLite's own affinity rules (with BOOLEAN, JSON and the
/// exact NUMERIC/DECIMAL kept apart). Dates stay text: SQLite has no date type.
pub(crate) fn kind_of(decl: &str) -> Kind {
    let d = decl.to_ascii_uppercase();
    if d.contains("BOOL") {
        Kind::Bool
    } else if d.contains("INT") {
        Kind::Int
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        if d.contains("JSON") { Kind::Json } else { Kind::Text }
    } else if d.contains("BLOB") {
        Kind::Bytes
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        Kind::Float
    } else if d == "JSON" || d == "JSONB" {
        Kind::Json
    } else if d.contains("NUMERIC") || d.contains("DECIMAL") {
        Kind::Numeric
    } else if d.is_empty() {
        Kind::Other
    } else {
        Kind::Text
    }
}

/// A cell's own class, for a column with no declared type (an expression).
fn kind_of_val(v: &Val) -> Kind {
    match v {
        Val::Int(_) => Kind::Int,
        Val::Float(_) => Kind::Float,
        Val::Blob(_) => Kind::Bytes,
        Val::Text(_) | Val::Bool(_) => Kind::Text,
        Val::Null => Kind::Other,
    }
}

fn columns_of(stmt: &rusqlite::Statement<'_>, first: Option<&[Val]>) -> Columns {
    let mut infos = Vec::new();
    let mut origins = Vec::new();
    for (i, (c, m)) in stmt.columns().iter().zip(stmt.columns_with_metadata()).enumerate() {
        let decl = c.decl_type().unwrap_or("").to_string();
        let kind = match (decl.is_empty(), first.and_then(|f| f.get(i))) {
            (true, Some(v)) => kind_of_val(v),
            _ => kind_of(&decl),
        };
        infos.push(ColumnInfo { name: c.name().to_string(), type_name: decl, type_oid: 0, kind });
        origins.push(match (m.database_name(), m.table_name(), m.origin_name()) {
            (Some(db), Some(t), Some(col)) => Some(NamedOrigin { schema: db.to_string(), table: t.to_string(), column: col.to_string() }),
            _ => None,
        });
    }
    (infos, origins)
}

fn execute_on(db: &rusqlite::Connection, sql: &str) -> Result<Option<u64>, LiteError> {
    let mut batch = Batch::new(db, sql);
    let mut n = None;
    while let Some(mut stmt) = batch.next()? {
        n = Some(if stmt.column_count() == 0 {
            stmt.raw_execute()?;
            db.changes()
        } else {
            let mut rows = stmt.raw_query();
            let mut k = 0;
            while rows.next()?.is_some() {
                k += 1;
            }
            k
        });
    }
    Ok(n)
}

fn text_rows_on(db: &rusqlite::Connection, sql: &str) -> Result<Vec<Vec<Option<String>>>, LiteError> {
    let mut batch = Batch::new(db, sql);
    let mut out = Vec::new();
    while let Some(mut stmt) = batch.next()? {
        let n = stmt.column_count();
        let mut rows = stmt.raw_query();
        while let Some(r) = rows.next()? {
            out.push((0..n).map(|i| r.get_ref(i).map(|v| val(v).text()).unwrap_or(None)).collect());
        }
    }
    Ok(out)
}

fn stream_on(c: &mut Conn, sql: &str, params: &[Val], kinds: Option<Vec<Kind>>, mut sink: Sink<LiteError>) {
    let r = (|| -> Result<Option<u64>, LiteError> {
        let mut batch = Batch::new(&c.db, sql);
        let (mut shown, mut affected, mut n) = (false, None, 0u64);
        while let Some(mut stmt) = batch.next()? {
            if !params.is_empty() {
                bind(&mut stmt, params)?;
            }
            if stmt.column_count() == 0 {
                stmt.raw_execute()?;
                if !shown {
                    affected = Some(c.db.changes());
                }
                continue;
            }
            let width = stmt.column_count();
            let (mut infos, origins) = columns_of(&stmt, None);
            let mut rows = stmt.raw_query();
            let first: Option<Vec<Val>> = rows.next()?.map(|r| (0..width).map(|i| r.get_ref(i).map(val).unwrap_or(Val::Null)).collect());
            match (&kinds, &first) {
                (Some(k), _) if !shown && k.len() == infos.len() => infos.iter_mut().zip(k).for_each(|(c, k)| c.kind = *k),
                (_, Some(f)) => infos.iter_mut().zip(f).filter(|(c, _)| c.type_name.is_empty()).for_each(|(c, v)| c.kind = kind_of_val(v)),
                _ => {}
            }
            sink.columns((infos, origins));
            shown = true;
            n = 0;
            if let Some(f) = first {
                n += 1;
                if !sink.row(f) {
                    continue;
                }
            }
            while let Some(r) = rows.next()? {
                n += 1;
                // A reader that went away (an export stopped): the statement just ends.
                if !sink.row((0..width).map(|i| r.get_ref(i).map(val).unwrap_or(Val::Null)).collect()) {
                    break;
                }
            }
        }
        Ok(if shown { Some(n) } else { affected })
    })();
    c.report();
    sink.end(r);
}

#[derive(Clone)]
pub(crate) struct Canceller {
    interrupt: Arc<rusqlite::InterruptHandle>,
    busy: Arc<AtomicBool>,
}

impl Canceller {
    /// Interrupts what runs now; between statements there is nothing to interrupt, and SQLite
    /// lets no later one fail for it.
    pub(crate) async fn cancel(&self) -> Result<()> {
        if self.busy.load(Ordering::Acquire) {
            self.interrupt.interrupt();
        }
        Ok(())
    }
}

pub(crate) async fn server_info(client: &Client, exts: &[Arc<dyn ServerExtension>]) -> Result<ServerInfo> {
    let rows = client.text_rows("SELECT sqlite_version()").await?;
    let v = rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()).unwrap_or_default();
    let mut info = ServerInfo { version: format!("SQLite {v}"), database: "main".into(), user: String::new(), ..Default::default() };
    info.search_path = vec!["main".into()];
    info.extensions = exts.iter().filter(|e| e.detect(&info)).map(|e| e.id()).collect();
    Ok(info)
}

/// SQLite's name for a result code, as `SQLITE_CONSTRAINT_UNIQUE`.
fn code_name(e: &ffi::Error) -> String {
    let base = match e.code {
        ffi::ErrorCode::ConstraintViolation => "SQLITE_CONSTRAINT",
        ffi::ErrorCode::DatabaseBusy => "SQLITE_BUSY",
        ffi::ErrorCode::DatabaseLocked => "SQLITE_LOCKED",
        ffi::ErrorCode::ReadOnly => "SQLITE_READONLY",
        ffi::ErrorCode::OperationInterrupted => "SQLITE_INTERRUPT",
        ffi::ErrorCode::TypeMismatch => "SQLITE_MISMATCH",
        ffi::ErrorCode::DatabaseCorrupt => "SQLITE_CORRUPT",
        ffi::ErrorCode::NotADatabase => "SQLITE_NOTADB",
        ffi::ErrorCode::PermissionDenied => "SQLITE_PERM",
        ffi::ErrorCode::OperationAborted => "SQLITE_ABORT",
        ffi::ErrorCode::TooBig => "SQLITE_TOOBIG",
        ffi::ErrorCode::CannotOpen => "SQLITE_CANTOPEN",
        ffi::ErrorCode::AuthorizationForStatementDenied => "SQLITE_AUTH",
        _ => "SQLITE_ERROR",
    };
    let ext = match e.extended_code {
        ffi::SQLITE_CONSTRAINT_UNIQUE => "_UNIQUE",
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY => "_PRIMARYKEY",
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => "_FOREIGNKEY",
        ffi::SQLITE_CONSTRAINT_NOTNULL => "_NOTNULL",
        ffi::SQLITE_CONSTRAINT_CHECK => "_CHECK",
        ffi::SQLITE_CONSTRAINT_TRIGGER => "_TRIGGER",
        ffi::SQLITE_CONSTRAINT_DATATYPE => "_DATATYPE",
        _ => "",
    };
    format!("{base}{ext}")
}

pub(crate) fn error_info(e: &LiteError) -> ErrorInfo {
    match e {
        rusqlite::Error::SqliteFailure(err, msg) => ErrorInfo {
            code: Some(code_name(err)),
            message: msg.clone().unwrap_or_else(|| err.to_string()),
            hint: busy_hint(err),
            severity: Some("ERROR".into()),
            ..Default::default()
        },
        rusqlite::Error::SqlInputError { error, msg, .. } => {
            ErrorInfo { code: Some(code_name(error)), message: msg.clone(), severity: Some("ERROR".into()), ..Default::default() }
        }
        e => ErrorInfo { message: e.to_string(), ..Default::default() },
    }
}

fn busy_hint(e: &ffi::Error) -> Option<String> {
    matches!(e.code, ffi::ErrorCode::DatabaseBusy | ffi::ErrorCode::DatabaseLocked)
        .then(|| "the database is locked: another tab or program holds a write transaction on this file".into())
}

/// With the position of the token SQLite points at, counted in characters of the whole text.
pub(crate) fn error_info_at(e: &LiteError, sql: &str) -> ErrorInfo {
    let mut info = error_info(e);
    if let rusqlite::Error::SqlInputError { sql: rest, offset, .. } = e
        && *offset >= 0
    {
        let at = sql.len().saturating_sub(rest.len()) + *offset as usize;
        if sql.is_char_boundary(at.min(sql.len())) {
            info.position = Some(sql[..at.min(sql.len())].chars().count() as u32 + 1);
        }
    }
    info
}

pub(crate) fn interrupted(e: &LiteError) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(err, _) if err.code == ffi::ErrorCode::OperationInterrupted)
}

pub(crate) async fn run(
    client: &Arc<Client>,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
    aborted: &(dyn Fn() -> bool + Sync),
    next: &(dyn Fn(&Arc<ResultSet>) -> Arc<ResultSet> + Sync),
) -> Ran {
    let fail = |e: &LiteError| if interrupted(e) { Ran::Cancelled } else { Ran::Failed(error_info_at(e, &rs.sql)) };
    if aborted() {
        return Ran::NeverRan { settle: false };
    }
    rs.executing.store(true, Ordering::Release);
    let ((infos, origins), mut rows) = match client.stream(&rs.sql, rs.params(), None).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// SQLite's own refusals for agents, under the classifier: what a reader's connection
    /// and a writer's let through.
    #[test]
    fn agents_connections_refuse_escapes() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sqlite");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1)").unwrap();
        drop(db);
        let p = ConnectionProfile::new_file(crate::config::Engine::Sqlite, path.to_string_lossy());
        let other = dir.path().join("b.sqlite");
        rt.block_on(async {
            for (agent, ro) in [(Agent::Read, true), (Agent::Write, false)] {
                let c = connect(&p, Open { read_only: ro, agent: Some(agent) }, None).await.unwrap();
                for sql in [
                    format!("ATTACH '{}' AS x", other.display()),
                    "PRAGMA writable_schema = ON".into(),
                    "PRAGMA trusted_schema = 1".into(),
                    "SELECT load_extension('x')".into(),
                    "CREATE VIRTUAL TABLE v USING fts5(a)".into(),
                ] {
                    assert!(c.execute(&sql).await.is_err(), "{agent:?}: {sql}");
                }
                assert!(c.text_rows("SELECT a FROM t").await.is_ok());
                assert!(c.text_rows("PRAGMA table_info(t)").await.is_ok(), "{agent:?}");
                let write = c.execute("INSERT INTO t VALUES (2)").await;
                assert_eq!(write.is_ok(), agent == Agent::Write, "{agent:?}: {write:?}");
                assert_eq!(c.execute("PRAGMA user_version = 7").await.is_ok(), agent == Agent::Write, "{agent:?}");
                assert_eq!(c.execute("CREATE TEMP TABLE scratch (x)").await.is_ok(), agent == Agent::Write, "{agent:?}");
            }
        });
        assert!(!other.exists());
    }

    #[test]
    fn declared_types() {
        for (d, k) in [
            ("INTEGER", Kind::Int),
            ("bigint", Kind::Int),
            ("BOOLEAN", Kind::Bool),
            ("VARCHAR(20)", Kind::Text),
            ("TEXT", Kind::Text),
            ("BLOB", Kind::Bytes),
            ("REAL", Kind::Float),
            ("DOUBLE PRECISION", Kind::Float),
            ("NUMERIC(10,2)", Kind::Numeric),
            ("DATETIME", Kind::Text),
            ("JSON", Kind::Json),
            ("", Kind::Other),
        ] {
            assert_eq!(kind_of(d), k, "{d}");
        }
    }
}
