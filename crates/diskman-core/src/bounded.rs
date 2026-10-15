//! Bounded statements for agents (the MCP server, `diskman-mcp`): statements on a backend of their
//! own, inside a transaction, with a row cap, a byte cap, a time limit and cancellation.
//!
//! - [`Mode::Read`]: one statement, after [`guard::agent_read_refusal`], in `BEGIN READ ONLY` …
//!   `ROLLBACK`. LAR accepts `READ ONLY` without enforcing it, so the client-side check is what
//!   keeps a read a read on LAR; PostgreSQL enforces both.
//! - [`Mode::Write`]: one or more statements in `BEGIN` … `COMMIT` (all or nothing). With
//!   [`Connection::bounded_held`] the transaction stays open after the statements, their counts
//!   and RETURNING rows in hand, until a decision commits it or rolls it back (a human's answer
//!   in the app, the agents' row limit).
//! - [`Mode::Rollback`]: the same, always rolled back (EXPLAIN ANALYZE of a write).
//! - [`Mode::Autocommit`]: one statement that cannot run in a transaction (`VACUUM`, `CREATE
//!   INDEX CONCURRENTLY`), on its own, after a human allowed it.
//!
//! The time limit is `SET LOCAL statement_timeout` on the server and a timer here (LAR may not
//! enforce the setting); when it fires, or the caller cancels, a CancelRequest goes out and the
//! backend is dropped. A read that hits the row or byte cap stops reading and drops its backend
//! instead of draining the rest. Backends that end cleanly go back to a small pool per database.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::Value;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, SimpleQueryMessage};

// The statements run on PostgreSQL's protocol (prepare, then the simple protocol when the server
// cannot prepare); MySQL's version of `in_transaction` is `my_in_transaction`.
use crate::backend::pg::{TextParam, column_infos, error_info, push_row};
use crate::backend::{Dedicated, RowSource, ms, my};
use crate::conn::Connection;
use crate::guard;
use crate::result::{Cell, ChunkBuilder, ColumnInfo, ErrorInfo, format_cell};
use crate::sql::Dialect;
use crate::value::{Kind, Storage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `BEGIN READ ONLY` … `ROLLBACK`, after the agent read check.
    Read,
    /// `BEGIN` … `COMMIT` (`ROLLBACK` on error).
    Write,
    /// `BEGIN` … `ROLLBACK`: writes that must leave nothing behind.
    Rollback,
    /// No transaction: one statement that cannot run inside one, committed as it runs.
    Autocommit,
}

/// Decides, once a write's statements ran and before anything is committed, whether it commits
/// (`true`) or rolls back: it sees their counts and RETURNING rows.
pub type Decide = Box<dyn FnOnce(&Outcome) -> futures_util::future::BoxFuture<'static, bool> + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_rows: usize,
    /// Approximate size of the rows returned (their text).
    pub max_bytes: usize,
    /// Longer text values are cut, ending in `…`.
    pub max_cell: usize,
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_rows: 200, max_bytes: 256 * 1024, max_cell: 2000, timeout: Duration::from_secs(30) }
    }
}

/// Why rows stopped short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncation {
    Rows,
    Bytes,
}

impl Truncation {
    pub fn label(self) -> &'static str {
        match self {
            Truncation::Rows => "rows",
            Truncation::Bytes => "bytes",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

/// One statement's command and count (`UPDATE` 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub verb: String,
    pub affected: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// The columns and rows of the last statement that returned rows.
    pub columns: Vec<Column>,
    /// JSON values: numbers, booleans, `null` for NULL (an empty string stays `""`), embedded
    /// `json`, text for the rest (numeric exactly, times as PostgreSQL prints them).
    pub rows: Vec<Vec<Value>>,
    pub truncated: Option<Truncation>,
    /// Text values cut at `max_cell`.
    pub cut_cells: usize,
    pub bytes: usize,
    pub statements: Vec<Done>,
    pub elapsed: Duration,
    /// The write was committed (a [`Mode::Write`] its decision allowed, or [`Mode::Autocommit`]).
    pub committed: bool,
}

impl Outcome {
    /// Rows the statements changed, all together.
    pub fn affected(&self) -> u64 {
        self.statements.iter().filter_map(|d| d.affected).sum()
    }
}

/// Why a bounded run did not finish.
#[derive(Debug, thiserror::Error)]
pub enum Stop {
    /// Refused before anything reached the server.
    #[error("refused: {0}")]
    Refused(String),
    #[error("timed out after {}", fmt_secs(*.0))]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Server(ErrorInfo),
    #[error("{0:#}")]
    Other(anyhow::Error),
}

fn fmt_secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

/// The statements of `sql` (one at least), trimmed and without their `;`.
pub fn statements(sql: &str) -> Vec<String> {
    statements_in(sql, Dialect::Postgres)
}

/// [`statements`] of a script in `dialect`.
pub fn statements_in(sql: &str, dialect: Dialect) -> Vec<String> {
    crate::sql::split_in(sql, dialect).into_iter().map(|s| sql[s.range].trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// What refuses `stmts` in `mode` before anything is sent, if anything does.
pub fn refusal(stmts: &[String], params: usize, mode: Mode, read_only_profile: bool) -> Option<String> {
    refusal_in(stmts, params, mode, read_only_profile, Dialect::Postgres)
}

/// [`refusal`] of statements in `dialect`.
pub fn refusal_in(stmts: &[String], params: usize, mode: Mode, read_only_profile: bool, dialect: Dialect) -> Option<String> {
    if stmts.is_empty() {
        return Some("empty statement".into());
    }
    if mode == Mode::Read && stmts.len() > 1 {
        return Some("one statement at a time".into());
    }
    if params > 0 && stmts.len() > 1 {
        return Some("parameters need a single statement".into());
    }
    if mode == Mode::Autocommit && stmts.len() > 1 {
        return Some("a statement that runs outside a transaction runs alone".into());
    }
    for s in stmts {
        if mode == Mode::Read {
            if let Some(r) = guard::agent_read_refusal_in(s, dialect) {
                return Some(r);
            }
            continue;
        }
        let f = guard::classify_in(s, dialect);
        if f.verb.is_empty() {
            return Some("empty statement".into());
        }
        if f.tx.is_some() {
            return Some(format!("transaction control ({}) is not allowed: every call is its own transaction", f.verb));
        }
        if f.outside_tx && mode != Mode::Autocommit {
            return Some(format!("{} cannot run inside a transaction", f.verb));
        }
        if read_only_profile && let Some(r) = guard::read_only_refusal_in(s, dialect) {
            return Some(r);
        }
    }
    None
}

/// Extra time the local timer gives the server's own `statement_timeout` to answer first.
const GRACE: Duration = Duration::from_millis(500);
/// Idle backends kept per database.
const POOL_SIZE: usize = 2;
const POOL: &str = "agent-pool";

type Pool = Mutex<Vec<Dedicated>>;

impl Connection {
    /// The pool of `db`'s connections made while it was read-only (`ro`) or not: a toggle in
    /// the explorer never hands out a connection made for the other state.
    fn agent_pool(&self, db: &str, ro: bool) -> Arc<Pool> {
        let key = (POOL, db.to_string(), if ro { "read-only" } else { "" }.to_string());
        let mut g = self.caches.lock();
        if let Some(p) = g.get(&key).cloned().and_then(|a| a.downcast::<Pool>().ok()) {
            return p;
        }
        let p: Arc<Pool> = Arc::new(Mutex::new(Vec::new()));
        g.insert(key, p.clone());
        p
    }

    async fn agent_take(&self, db: &str, ro: bool) -> anyhow::Result<Dedicated> {
        let pool = self.agent_pool(db, ro);
        loop {
            let d = pool.lock().pop();
            match d {
                Some(d) if !d.db().is_closed() => return Ok(d),
                Some(_) => continue,
                None => break,
            }
        }
        let d = self.dedicated(Some(db)).await?;
        // Tells the activity monitor (and pg_stat_activity readers) who this is.
        if let Some(p) = d.pg() {
            let _ = p.client.simple_query("SET application_name = 'Diskman MCP'").await;
        }
        Ok(d)
    }

    fn agent_give(&self, db: &str, ro: bool, d: Dedicated) {
        if d.db().is_closed() {
            return;
        }
        let pool = self.agent_pool(db, ro);
        let mut g = pool.lock();
        if g.len() < POOL_SIZE {
            g.push(d);
        }
    }

    /// Runs `sql` bounded (see the module docs). `params` bind `$1 … $n` in text format (the
    /// server parses each as its parameter's type; `None` is NULL): values are never spliced into
    /// the SQL. `cancel` resolving stops the run (CancelRequest, backend dropped).
    pub async fn bounded(
        &self,
        database: Option<&str>,
        sql: &str,
        params: Vec<Option<String>>,
        mode: Mode,
        limits: Limits,
        cancel: impl Future<Output = ()> + Send,
    ) -> Result<Outcome, Stop> {
        self.bounded_held(database, sql, params, mode, limits, cancel, None).await
    }

    /// [`Connection::bounded`], and in [`Mode::Write`] the transaction held open after the
    /// statements until `decide` answers (`None`: commit): COMMIT when it says so, ROLLBACK
    /// otherwise, and ROLLBACK when `cancel` resolves meanwhile. The time limit covers the
    /// statements, not the wait. `elapsed` is the statements' time.
    #[allow(clippy::too_many_arguments)]
    pub async fn bounded_held(
        &self,
        database: Option<&str>,
        sql: &str,
        params: Vec<Option<String>>,
        mode: Mode,
        limits: Limits,
        cancel: impl Future<Output = ()> + Send,
        decide: Option<Decide>,
    ) -> Result<Outcome, Stop> {
        let dialect = self.dialect();
        let stmts = statements_in(sql, dialect);
        let db = database.unwrap_or(self.database()).to_string();
        let ro = self.read_only_for(&db);
        if let Some(r) = refusal_in(&stmts, params.len(), mode, ro, dialect) {
            return Err(Stop::Refused(r));
        }
        let started = Instant::now();
        let mut d = tokio::select! {
            d = self.agent_take(&db, ro) => d.map_err(Stop::Other)?,
            _ = tokio::time::sleep(limits.timeout) => return Err(Stop::Timeout(limits.timeout)),
        };
        tokio::pin!(cancel);
        let ran = {
            let work = async {
                match &d {
                    Dedicated::Pg(p) => in_transaction(&p.client, &stmts, &params, mode, &limits).await,
                    Dedicated::My(c) => my_in_transaction(c, &stmts, &params, mode, &limits).await,
                    Dedicated::Ms(c) => ms_in_transaction(c, &stmts, &params, mode, &limits).await,
                }
            };
            tokio::pin!(work);
            tokio::select! {
                r = &mut work => r,
                _ = tokio::time::sleep(limits.timeout + GRACE) => Err((Stop::Timeout(limits.timeout), false)),
                _ = &mut cancel => Err((Stop::Cancelled, false)),
            }
        };
        let elapsed = started.elapsed();
        let (r, reuse) = match ran {
            // A read cut short at a cap: the backend goes, its transaction with it.
            Ok((o, false)) => (Ok(o), false),
            Ok((mut o, true)) => {
                o.elapsed = elapsed;
                let mut cancelled = false;
                let commit = match (mode, decide) {
                    (Mode::Write, Some(f)) => {
                        let wait = f(&o);
                        tokio::select! {
                            ok = wait => ok,
                            _ = &mut cancel => {
                                cancelled = true;
                                false
                            }
                        }
                    }
                    (Mode::Write, None) => true,
                    _ => false,
                };
                match finish(&d, dialect, mode, commit).await {
                    Ok(()) => {
                        o.committed = mode == Mode::Autocommit || (mode == Mode::Write && commit);
                        // A session set up for one statement outside a transaction is not reused.
                        (if cancelled { Err(Stop::Cancelled) } else { Ok(o) }, mode != Mode::Autocommit)
                    }
                    Err((e, reuse)) => (Err(e), reuse),
                }
            }
            Err((e, reuse)) => (Err(e), reuse),
        };
        let r = match r {
            Ok(o) => Ok(o),
            // The server's own `statement_timeout` answered first (MySQL: MAX_EXECUTION_TIME,
            // MariaDB: max_statement_time).
            Err(Stop::Server(e)) if e.code.as_deref() == Some("57014") && e.message.contains("timeout") => Err(Stop::Timeout(limits.timeout)),
            Err(Stop::Server(e)) if matches!(e.code.as_deref(), Some("3024" | "1969")) => Err(Stop::Timeout(limits.timeout)),
            Err(e) => Err(e),
        };
        if reuse {
            self.agent_give(&db, ro, d);
        } else {
            // A statement may still be running: stop it on the server, then let the backend go.
            if matches!(r, Err(Stop::Timeout(_) | Stop::Cancelled)) {
                let _ = tokio::time::timeout(Duration::from_secs(2), d.cancel()).await;
            }
            if let Some(p) = d.pg_mut() {
                p.notifications.close();
            }
            drop(d);
        }
        r
    }
}

/// Ends what [`in_transaction`] (or its MySQL and SQL Server forms) left open: COMMIT
/// (`commit`) or ROLLBACK; nothing in [`Mode::Autocommit`]. The flag of an error says whether
/// the backend can be reused.
async fn finish(d: &Dedicated, dialect: Dialect, mode: Mode, commit: bool) -> Result<(), Failed> {
    if mode == Mode::Autocommit {
        return Ok(());
    }
    let sql = if commit { dialect.commit() } else { dialect.rollback() };
    match d {
        Dedicated::Pg(p) => p.client.simple_query(sql).await.map(|_| ()).map_err(|e| (Stop::Server(error_info(&e)), !p.client.is_closed())),
        Dedicated::My(c) => c.execute(sql).await.map(|_| ()).map_err(|e| (Stop::Server(my::error_info_at(&e, sql)), !c.is_closed())),
        Dedicated::Ms(c) => c.execute(sql).await.map(|_| ()).map_err(|e| (Stop::Server(ms::error_info_at(&e, sql, 0)), !c.is_closed())),
    }
}

/// [`in_transaction`] on MySQL: `START TRANSACTION [READ ONLY]` (none in [`Mode::Autocommit`]),
/// the session's statement time limit, the statements (prepared, or as text when the server will
/// not prepare them). DDL commits on its own in MySQL: a write that changes the schema is not
/// undone by a ROLLBACK (the agents' tiers ask before running it).
async fn my_in_transaction(c: &Arc<my::Client>, stmts: &[String], params: &[Option<String>], mode: Mode, limits: &Limits) -> Result<(Outcome, bool), Failed> {
    let server = |e: mysql_async::Error, sql: &str| Stop::Server(my::error_info_at(&e, sql));
    if mode != Mode::Autocommit {
        let begin = if mode == Mode::Read { "START TRANSACTION READ ONLY" } else { "START TRANSACTION" };
        c.execute(begin).await.map_err(|e| (server(e, begin), false))?;
    }
    // MySQL bounds SELECTs (MAX_EXECUTION_TIME, ms), MariaDB every statement (seconds); the
    // local timer covers the rest.
    let ms = limits.timeout.as_millis().max(1);
    if c.execute(&format!("SET SESSION MAX_EXECUTION_TIME = {ms}")).await.is_err() {
        let _ = c.execute(&format!("SET SESSION max_statement_time = {}", (ms as f64 / 1000.0).max(0.001))).await;
    }
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        match my_statement(c, s, p, mode, limits, &mut out).await {
            Ok(true) => {}
            // A read cut short: the server is still sending; stop it, drop the connection.
            Ok(false) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), c.cancel()).await;
                return Ok((out, false));
            }
            Err(e) => {
                let ok = mode == Mode::Autocommit || c.execute("ROLLBACK").await.is_ok();
                return Err((e, ok && !c.is_closed()));
            }
        }
    }
    Ok((out, true))
}

/// One MySQL statement into `out`. `Ok(false)`: a read stopped at a cap with rows left.
async fn my_statement(c: &Arc<my::Client>, sql: &str, params: &[Option<String>], mode: Mode, limits: &Limits, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = guard::classify_in(sql, Dialect::MySql).verb;
    let (cols, mut rows) = my::query(c, sql, params).await.map_err(|e| Stop::Server(my::error_info_at(&e, sql)))?;
    let returns_rows = !cols.is_empty();
    if returns_rows {
        out.columns = cols.iter().map(|c| Column { name: c.name.clone(), type_name: c.type_name.clone() }).collect();
        out.rows.clear();
        out.truncated = None;
        out.bytes = 0;
    }
    let mut b = ChunkBuilder::new(&cols);
    loop {
        let more = rows.next_into(&mut b).await.map_err(|e| Stop::Server(my::error_info(&e)))?;
        if !more {
            break;
        }
        let chunk = b.finish();
        match keep_row(&cols, &chunk, mode, limits, out) {
            Keep::Yes | Keep::Skip => {}
            Keep::Stop => return Ok(false),
        }
    }
    let affected = rows.affected();
    out.statements.push(Done { verb, affected: if returns_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

/// [`in_transaction`] on SQL Server: `BEGIN TRANSACTION` (none in [`Mode::Autocommit`]), the
/// statements (bound through `sp_executesql` when they have parameters). SQL Server has neither
/// read-only transactions nor a statement time limit: a read is a read by the client-side check,
/// and the local timer cancels (an attention) and drops the connection.
async fn ms_in_transaction(c: &Arc<ms::Client>, stmts: &[String], params: &[Option<String>], mode: Mode, limits: &Limits) -> Result<(Outcome, bool), Failed> {
    let server = |e: ms::MsError, sql: &str| Stop::Server(ms::error_info_at(&e, sql, 0));
    if mode != Mode::Autocommit {
        let begin = "BEGIN TRANSACTION";
        c.execute(begin).await.map_err(|e| (server(e, begin), false))?;
    }
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        match ms_statement(c, s, p, mode, limits, &mut out).await {
            Ok(true) => {}
            // A read cut short: the server is still sending; stop it, drop the connection.
            Ok(false) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), c.cancel()).await;
                return Ok((out, false));
            }
            Err(e) => {
                let ok = c.execute("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await.is_ok();
                return Err((e, ok && !c.is_closed()));
            }
        }
    }
    Ok((out, true))
}

/// One SQL Server statement into `out` (its first result set; a write's count). `Ok(false)`: a
/// read stopped at a cap with rows left.
async fn ms_statement(c: &Arc<ms::Client>, sql: &str, params: &[Option<String>], mode: Mode, limits: &Limits, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = guard::classify_in(sql, Dialect::TSql).verb;
    let (cols, mut rows) = ms::query(c, sql, params).await.map_err(|e| Stop::Server(ms::error_info_at(&e, sql, 0)))?;
    let returns_rows = !cols.is_empty();
    if returns_rows {
        out.columns = cols.iter().map(|c| Column { name: c.name.clone(), type_name: c.type_name.clone() }).collect();
        out.rows.clear();
        out.truncated = None;
        out.bytes = 0;
    }
    let mut b = ChunkBuilder::new(&cols);
    let mut n = 0u64;
    while rows.next_into(&mut b).await.map_err(|e| Stop::Server(ms::error_info(&e)))? {
        n += 1;
        let chunk = b.finish();
        match keep_row(&cols, &chunk, mode, limits, out) {
            Keep::Yes | Keep::Skip => {}
            Keep::Stop => return Ok(false),
        }
    }
    // Later result sets are read and dropped; the batch's error (if any) comes after them.
    let affected = loop {
        match rows.next_result().await {
            ms::Next::Columns(_) => continue,
            ms::Next::End { affected, .. } => break affected,
            ms::Next::Fail(e) => return Err(Stop::Server(ms::error_info_at(&e, sql, 0))),
        }
    };
    out.statements.push(Done { verb, affected: if returns_rows { (mode != Mode::Read).then_some(n) } else { affected } });
    Ok(true)
}

enum Keep {
    Yes,
    /// Past a cap in a write: read on, keep nothing.
    Skip,
    /// Past a cap in a read: stop reading.
    Stop,
}

/// Adds the one row of `chunk` to `out` within the caps.
fn keep_row(cols: &[ColumnInfo], chunk: &crate::result::Chunk, mode: Mode, limits: &Limits, out: &mut Outcome) -> Keep {
    let past = |out: &mut Outcome, why: Truncation| {
        out.truncated.get_or_insert(why);
        if mode == Mode::Read { Keep::Stop } else { Keep::Skip }
    };
    if out.truncated.is_some() || out.rows.len() >= limits.max_rows {
        return past(out, Truncation::Rows);
    }
    let mut vals = Vec::with_capacity(cols.len());
    let mut size = 0;
    for (i, c) in cols.iter().enumerate() {
        let (v, n, cut) = json_cell(c.kind, chunk.cell(0, i), limits.max_cell);
        size += n;
        out.cut_cells += cut as usize;
        vals.push(v);
    }
    if out.bytes + size > limits.max_bytes && !out.rows.is_empty() {
        return past(out, Truncation::Bytes);
    }
    out.bytes += size;
    out.rows.push(vals);
    Keep::Yes
}

type Failed = (Stop, bool);

/// BEGIN and the statements, the transaction left open for [`finish`]; `Ok((_, false))`: a
/// read stopped at a cap (the backend must go). On an error the transaction is rolled back and
/// the flag says whether the backend can be reused (it is idle, nothing in flight).
async fn in_transaction(client: &Client, stmts: &[String], params: &[Option<String>], mode: Mode, limits: &Limits) -> Result<(Outcome, bool), Failed> {
    begin(client, mode, limits).await?;
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        match statement(client, s, p, mode, limits, &mut out).await {
            Ok(true) => {}
            // A read cut short: the rest is never read; the backend is dropped (rolled back).
            Ok(false) => return Ok((out, false)),
            Err(e) => {
                let ok = mode == Mode::Autocommit || client.simple_query("ROLLBACK").await.is_ok();
                return Err((e, ok && !client.is_closed()));
            }
        }
    }
    Ok((out, true))
}

async fn begin(client: &Client, mode: Mode, limits: &Limits) -> Result<(), Failed> {
    if mode == Mode::Autocommit {
        // No transaction: the session's own limit (the backend is not reused after).
        let timeout = format!("SET statement_timeout = {}", limits.timeout.as_millis().max(1));
        let _ = client.simple_query(&timeout).await;
        return Ok(());
    }
    let begin = if mode == Mode::Read { "BEGIN READ ONLY" } else { "BEGIN" };
    client.simple_query(begin).await.map_err(|e| (Stop::Server(error_info(&e)), false))?;
    let timeout = format!("SET LOCAL statement_timeout = {}", limits.timeout.as_millis().max(1));
    if client.simple_query(&timeout).await.is_err() {
        // A server without the setting: the local timer alone.
        client.simple_query("ROLLBACK").await.map_err(|e| (Stop::Server(error_info(&e)), false))?;
        client.simple_query(begin).await.map_err(|e| (Stop::Server(error_info(&e)), false))?;
    }
    Ok(())
}

/// Runs one statement into `out`. `Ok(false)`: a read stopped at a cap with rows left.
async fn statement(client: &Client, sql: &str, params: &[Option<String>], mode: Mode, limits: &Limits, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = crate::guard::classify(sql).verb;
    let stmt = match client.prepare(sql).await {
        Ok(s) => s,
        // Statements the server cannot prepare run through the simple protocol (text results).
        // A failed prepare aborted the transaction: start it again first. Never with parameters.
        Err(e) if e.as_db_error().is_some() && params.is_empty() && out.statements.is_empty() => {
            if mode != Mode::Autocommit {
                client.simple_query("ROLLBACK").await.map_err(|e| Stop::Server(error_info(&e)))?;
                begin(client, mode, limits).await.map_err(|(e, _)| e)?;
            }
            return simple(client, sql, verb, mode, limits, out).await.map_err(|e2| {
                // The prepare's error is the better one when both say the same thing.
                let first = error_info(&e);
                if e2.as_db_error().is_some_and(|d| Some(d.code().code()) == first.code.as_deref()) { Stop::Server(first) } else { Stop::Server(error_info(&e2)) }
            });
        }
        Err(e) => return Err(Stop::Server(error_info(&e))),
    };
    let cols = column_infos(&stmt);
    let types: Vec<Type> = stmt.columns().iter().map(|c| c.type_().clone()).collect();
    let storage: Vec<Storage> = cols.iter().map(|c| c.kind.storage()).collect();
    let binds: Vec<TextParam> = params.iter().map(|p| TextParam(p.as_deref())).collect();
    let stream = client.query_raw(&stmt, binds.iter().map(|p| p as &(dyn ToSql + Sync))).await.map_err(|e| Stop::Server(error_info(&e)))?;
    futures_util::pin_mut!(stream);
    let returns_rows = !cols.is_empty();
    if returns_rows {
        out.columns = cols.iter().map(|c| Column { name: c.name.clone(), type_name: c.type_name.clone() }).collect();
        out.rows.clear();
        out.truncated = None;
        out.bytes = 0;
    }
    let mut b = ChunkBuilder::new(&cols);
    while let Some(row) = stream.as_mut().next().await {
        let row = row.map_err(|e| Stop::Server(error_info(&e)))?;
        if out.truncated.is_some() {
            if mode == Mode::Read {
                return Ok(false);
            }
            continue; // a write's RETURNING: read to the end for the count, keep nothing more
        }
        if out.rows.len() >= limits.max_rows {
            out.truncated = Some(Truncation::Rows);
            if mode == Mode::Read {
                return Ok(false);
            }
            continue;
        }
        push_row(&mut b, &row, &types, &storage);
        let chunk = b.finish();
        let mut vals = Vec::with_capacity(cols.len());
        let mut size = 0;
        for (i, c) in cols.iter().enumerate() {
            let (v, n, cut) = json_cell(c.kind, chunk.cell(0, i), limits.max_cell);
            size += n;
            out.cut_cells += cut as usize;
            vals.push(v);
        }
        if out.bytes + size > limits.max_bytes && !out.rows.is_empty() {
            out.truncated = Some(Truncation::Bytes);
            if mode == Mode::Read {
                return Ok(false);
            }
            continue;
        }
        out.bytes += size;
        out.rows.push(vals);
    }
    let affected = stream.rows_affected();
    out.statements.push(Done { verb, affected: if returns_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

/// The simple protocol: text values only.
async fn simple(client: &Client, sql: &str, verb: String, mode: Mode, limits: &Limits, out: &mut Outcome) -> Result<bool, tokio_postgres::Error> {
    let stream = client.simple_query_raw(sql).await?;
    futures_util::pin_mut!(stream);
    let mut affected = None;
    let mut first = true;
    while let Some(m) = stream.as_mut().next().await {
        match m? {
            SimpleQueryMessage::RowDescription(cols) if first => {
                out.columns = cols.iter().map(|c| Column { name: c.name().to_string(), type_name: "text".into() }).collect();
            }
            SimpleQueryMessage::Row(r) if first => {
                if out.truncated.is_some() || out.rows.len() >= limits.max_rows {
                    out.truncated.get_or_insert(Truncation::Rows);
                    if mode == Mode::Read {
                        return Ok(false);
                    }
                    continue;
                }
                let mut vals = Vec::with_capacity(r.len());
                let mut size = 0;
                for i in 0..r.len() {
                    let (v, n, cut) = match r.get(i) {
                        Some(s) => json_cell(Kind::Text, Cell::Text(s), limits.max_cell),
                        None => (Value::Null, 4, false),
                    };
                    size += n;
                    out.cut_cells += cut as usize;
                    vals.push(v);
                }
                if out.bytes + size > limits.max_bytes && !out.rows.is_empty() {
                    out.truncated = Some(Truncation::Bytes);
                    if mode == Mode::Read {
                        return Ok(false);
                    }
                    continue;
                }
                out.bytes += size;
                out.rows.push(vals);
            }
            SimpleQueryMessage::CommandComplete(n) => {
                if first {
                    affected = Some(n);
                }
                first = false;
            }
            _ => {}
        }
    }
    let has_rows = !out.columns.is_empty();
    out.statements.push(Done { verb, affected: if has_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

/// The agent's bounded view of a result that runs elsewhere (its agent tab's session): the
/// first rows within `limits`, as [`Connection::bounded`] would return them, in the server's
/// order (a sort in the grid does not count). `truncated` when more rows exist or may (the
/// result paused, still running, or kept partly).
pub fn outcome_of(rs: &crate::result::ResultSet, limits: &Limits) -> Outcome {
    let snap = rs.snapshot();
    let st = rs.status();
    let mut out = Outcome::default();
    let cols: Vec<ColumnInfo> = rs.columns().map(|c| c.as_ref().clone()).unwrap_or_default();
    out.columns = cols.iter().map(|c| Column { name: c.name.clone(), type_name: c.type_name.clone() }).collect();
    let n = snap.source_rows();
    for r in 0..n {
        if out.rows.len() >= limits.max_rows {
            out.truncated = Some(Truncation::Rows);
            break;
        }
        let mut vals = Vec::with_capacity(cols.len());
        let mut size = 0;
        for (i, c) in cols.iter().enumerate() {
            let (v, len, cut) = json_cell(c.kind, snap.source_cell(r, i), limits.max_cell);
            size += len;
            out.cut_cells += cut as usize;
            vals.push(v);
        }
        if out.bytes + size > limits.max_bytes && !out.rows.is_empty() {
            out.truncated = Some(Truncation::Bytes);
            break;
        }
        out.bytes += size;
        out.rows.push(vals);
    }
    let unfinished = st.phase.is_some_and(|p| !p.is_finished()) || st.partial;
    if out.truncated.is_none() && unfinished && !cols.is_empty() {
        out.truncated = Some(Truncation::Rows);
    }
    let verb = crate::sql::statement_verb(&rs.sql);
    out.statements.push(Done { verb, affected: if cols.is_empty() { st.affected } else { None } });
    out.elapsed = st.elapsed.unwrap_or_default();
    out
}

/// A cell as JSON, its approximate size, and whether its text was cut.
pub fn json_cell(kind: Kind, cell: Cell<'_>, max_cell: usize) -> (Value, usize, bool) {
    match cell {
        Cell::Null => (Value::Null, 4, false),
        Cell::Bool(b) => (Value::Bool(b), 5, false),
        Cell::Int(v) if !matches!(kind, Kind::Date | Kind::Time | Kind::Timestamp | Kind::TimestampTz) => (Value::from(v), 20, false),
        Cell::Float(_) | Cell::Int(_) => {
            let text = format_cell(kind, cell).map(|s| s.into_owned()).unwrap_or_default();
            if matches!(cell, Cell::Float(_))
                && let Some(n) = text.parse::<f64>().ok().and_then(serde_json::Number::from_f64)
            {
                return (Value::Number(n), text.len(), false);
            }
            let n = text.len() + 2;
            (Value::String(text), n, false)
        }
        Cell::Text(s) => {
            if kind == Kind::Json
                && s.len() <= max_cell
                && let Ok(v) = serde_json::from_str::<Value>(s)
            {
                return (v, s.len(), false);
            }
            let (text, cut) = cut(s, max_cell);
            let n = text.len() + 2;
            (Value::String(text), n, cut)
        }
    }
}

/// `s` up to `max` characters, ending in `…` when cut.
fn cut(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((at, _)) => (format!("{}…", &s[..at]), true),
        None => (s.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_cells() {
        assert_eq!(json_cell(Kind::Int, Cell::Null, 10).0, Value::Null);
        assert_eq!(json_cell(Kind::Text, Cell::Text(""), 10).0, Value::String(String::new()), "empty is not NULL");
        assert_eq!(json_cell(Kind::Int, Cell::Int(42), 10).0, Value::from(42));
        assert_eq!(json_cell(Kind::Bool, Cell::Bool(true), 10).0, Value::Bool(true));
        assert_eq!(json_cell(Kind::Float4, Cell::Float(0.1f32 as f64), 10).0.to_string(), "0.1");
        assert_eq!(json_cell(Kind::Float, Cell::Float(f64::NAN), 10).0, Value::String("NaN".into()));
        assert_eq!(json_cell(Kind::Numeric, Cell::Text("12.3400"), 10).0, Value::String("12.3400".into()), "numeric stays exact");
        assert_eq!(json_cell(Kind::Json, Cell::Text("{\"a\":[1]}"), 100).0, serde_json::json!({"a": [1]}));
        assert_eq!(json_cell(Kind::Date, Cell::Int(0), 10).0, Value::String("2000-01-01".into()));
        let (v, _, c) = json_cell(Kind::Text, Cell::Text("café au lait"), 4);
        assert_eq!((v, c), (Value::String("café…".into()), true));
    }

    /// The agent's view of a tab's result: the caps, NULL as null, cut text, more rows.
    #[test]
    fn outcome_of_a_result() {
        use crate::result::{ChunkBuilder, ResultSet};
        let cols = vec![
            ColumnInfo { name: "id".into(), type_name: "int4".into(), type_oid: 23, kind: Kind::Int },
            ColumnInfo { name: "t".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text },
        ];
        let mut b = ChunkBuilder::new(&cols);
        for i in 0..5 {
            b.push_i64(0, i);
            if i == 2 {
                b.push_null(1);
            } else {
                b.push_text(1, &"x".repeat(10));
            }
            b.end_row();
        }
        let rs = ResultSet::from_chunk("SELECT id, t FROM x", cols, b.finish());
        let o = outcome_of(&rs, &Limits { max_rows: 3, ..Limits::default() });
        assert_eq!((o.rows.len(), o.truncated), (3, Some(Truncation::Rows)));
        assert_eq!(o.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "t"]);
        assert_eq!(o.rows[2], vec![Value::from(2), Value::Null]);
        assert_eq!((o.statements[0].verb.as_str(), o.statements[0].affected), ("SELECT", None));
        let o = outcome_of(&rs, &Limits { max_rows: 10, max_cell: 4, ..Limits::default() });
        assert_eq!((o.rows.len(), o.truncated, o.cut_cells), (5, None, 4));
        assert_eq!(o.rows[0][1], Value::String("xxxx…".into()));
        // Still running: more may come.
        let live = ResultSet::new("SELECT 1");
        assert_eq!(outcome_of(&live, &Limits::default()).truncated, None, "no columns yet: nothing to cut");
    }

    #[test]
    fn refusals() {
        let s = |x: &str| statements(x);
        assert_eq!(s("select 1; ; select 2;"), vec!["select 1", "select 2"]);
        assert!(refusal(&s("SELECT 1; SELECT 2"), 0, Mode::Read, false).unwrap().contains("one statement"));
        assert!(refusal(&s("DELETE FROM t WHERE id = 1"), 0, Mode::Read, false).unwrap().contains("not a read"));
        assert!(refusal(&s(""), 0, Mode::Write, false).unwrap().contains("empty"));
        assert!(refusal(&s("BEGIN; INSERT INTO t VALUES (1)"), 0, Mode::Write, false).unwrap().contains("transaction control"));
        assert!(refusal(&s("CREATE INDEX CONCURRENTLY i ON t (x)"), 0, Mode::Write, false).unwrap().contains("inside a transaction"));
        assert!(refusal(&s("INSERT INTO t VALUES (1)"), 0, Mode::Write, true).unwrap().contains("read-only connection"));
        assert!(refusal(&s("INSERT INTO t VALUES ($1); SELECT 1"), 1, Mode::Write, false).unwrap().contains("single statement"));
        assert_eq!(refusal(&s("INSERT INTO t VALUES (1); UPDATE t SET a = 2 WHERE b"), 0, Mode::Write, false), None);
        assert_eq!(refusal(&s("SELECT * FROM t WHERE id = $1"), 1, Mode::Read, false), None);
        // Outside a transaction: one statement, never transaction control, never on a
        // read-only database.
        assert_eq!(refusal(&s("VACUUM t"), 0, Mode::Autocommit, false), None);
        assert_eq!(refusal(&s("CREATE INDEX CONCURRENTLY i ON t (x)"), 0, Mode::Autocommit, false), None);
        assert!(refusal(&s("VACUUM a; VACUUM b"), 0, Mode::Autocommit, false).unwrap().contains("alone"));
        assert!(refusal(&s("COMMIT"), 0, Mode::Autocommit, false).unwrap().contains("transaction control"));
        assert!(refusal(&s("CREATE INDEX CONCURRENTLY i ON t (x)"), 0, Mode::Autocommit, true).unwrap().contains("read-only"));
    }
}
