//! The database drivers behind [`Connection`](crate::Connection) and [`Session`](crate::Session).
//!
//! The rest of the core reaches a server only through the handles here, one enum variant per
//! driver (static dispatch: no `dyn` where rows go by):
//!
//! - [`Db`]: a client for Dataman's own statements (catalog reads, edits, counts, exports): text
//!   rows, typed rows decoded like the grid's, statements that only return a count.
//! - [`Dedicated`]: a [`Db`] on a backend of its own, with what cancels its statement.
//! - [`Canceller`]: cancels the statement running on an editor session's backend.
//! - [`RowSource`] and [`pump`]: a driver's typed rows, decoded straight into the grid's chunks
//!   and paced by the reader's demand (pauses, flushes, draining after an abort).
//! - [`run`]: one editor statement, from prepare to the last row ([`Ran`]).
//! - [`Caps`]: what the engine can do; the UI hides what it cannot.
//!
//! Connecting dispatches on the profile's [`Engine`]; everything after that on the handle's
//! variant. PostgreSQL (and LAR) is [`pg`], MySQL (and MariaDB) [`my`], SQL Server (and Azure
//! SQL) [`ms`].

pub(crate) mod mongo;
pub(crate) mod ms;
pub(crate) mod my;
pub(crate) mod pg;
pub(crate) mod redis;
pub(crate) mod search;

/// rustls picks the process' crypto provider: ring, for every engine (two providers are
/// compiled in: tiberius names aws-lc-rs as its fallback).
pub(crate) fn install_tls_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::config::{ConnectionProfile, Engine};
use crate::result::{CHUNK, ChunkBuilder, ColumnInfo, Demand, ErrorInfo, Phase, ResultSet};
use crate::server::{ServerExtension, ServerInfo};
use crate::ssh::Tunnel;
use crate::tx::TxWatch;

// ---------------------------------------------------------------------------------------------
// Capabilities

/// What an engine can do beyond running statements and browsing tables. Features read it before
/// they start, and the UI leaves out what is false (no "not supported" text anywhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Databases hold schemas (PostgreSQL, SQL Server); MySQL's databases are its schemas.
    pub schemas: bool,
    /// DDL inside a transaction rolls back with it (imports' CREATE TABLE, EXPLAIN ANALYZE).
    pub transactional_ddl: bool,
    /// Importing files (CSV, JSON, Parquet) into tables.
    pub import: bool,
    /// Bulk loads through `COPY … FROM STDIN`; without it imports insert in batches.
    pub copy_import: bool,
    /// `EXPLAIN (FORMAT JSON)`: the visual plan, EXPLAIN ANALYZE rolled back.
    pub explain: bool,
    /// The activity monitor: sessions, locks, table health, statement statistics, cancel and
    /// terminate.
    pub activity: bool,
    /// `LISTEN` / `NOTIFY` on a connection (LAR's live queries build on it).
    pub notifications: bool,
    /// The explorer's objects besides tables and views, and their definitions.
    pub functions: bool,
    pub sequences: bool,
    /// Enums, domains and composite types.
    pub types: bool,
    pub triggers: bool,
    /// Triggers can be disabled and enabled again (`ALTER TABLE … DISABLE TRIGGER`).
    pub toggle_triggers: bool,
    pub extensions: bool,
    pub roles: bool,
    /// The privileges panel (grants per role).
    pub privileges: bool,
}

impl Caps {
    pub const POSTGRES: Caps = Caps {
        schemas: true,
        transactional_ddl: true,
        import: true,
        copy_import: true,
        explain: true,
        activity: true,
        notifications: true,
        functions: true,
        sequences: true,
        types: true,
        triggers: true,
        toggle_triggers: true,
        extensions: true,
        roles: true,
        privileges: true,
    };

    /// Nothing beyond statements and tables: what an engine has before its features land.
    const BASIC: Caps = Caps {
        schemas: true,
        transactional_ddl: false,
        import: false,
        copy_import: false,
        explain: false,
        activity: false,
        notifications: false,
        functions: false,
        sequences: false,
        types: false,
        triggers: false,
        toggle_triggers: false,
        extensions: false,
        roles: false,
        privileges: false,
    };

    /// An engine without SQL (`crate::nosql`): no schemas either.
    pub const NONE: Caps = Caps { schemas: false, ..Caps::BASIC };

    pub fn of(engine: Engine) -> Caps {
        match engine {
            Engine::Postgres => Caps::POSTGRES,
            // A MySQL database is its schema; routines and triggers are in the explorer.
            Engine::MySql => Caps { schemas: false, functions: true, triggers: true, ..Caps::BASIC },
            // Procedures and functions, triggers (they can be disabled), sequences and alias
            // and table types are in the explorer.
            Engine::SqlServer => Caps {
                transactional_ddl: true,
                functions: true,
                sequences: true,
                types: true,
                triggers: true,
                toggle_triggers: true,
                ..Caps::BASIC
            },
            // The engines of `nosql`: none of the SQL features.
            Engine::MongoDb | Engine::Elasticsearch | Engine::OpenSearch | Engine::Redis => Caps::NONE,
        }
    }
}

/// The server's error inside `e` (a statement or a login refused), in its parts; `None` when the
/// failure is not the server's (unreachable, TLS, a dropped connection).
pub fn server_error(e: &anyhow::Error) -> Option<ErrorInfo> {
    e.chain().find_map(|c| {
        if let Some(e) = c.downcast_ref::<tokio_postgres::Error>() {
            return e.as_db_error().is_some().then(|| pg::error_info(e));
        }
        if let Some(e @ ms::MsError::Server(_)) = c.downcast_ref::<ms::MsError>() {
            return Some(ms::error_info(e));
        }
        match c.downcast_ref::<mysql_async::Error>() {
            Some(e @ mysql_async::Error::Server(_)) => Some(my::error_info(e)),
            _ => None,
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Handles

/// A client for Dataman's own statements.
#[derive(Clone)]
pub(crate) enum Db {
    Pg(Arc<tokio_postgres::Client>),
    My(Arc<my::Client>),
    Ms(Arc<ms::Client>),
}

impl Db {
    pub(crate) fn is_closed(&self) -> bool {
        match self {
            Db::Pg(c) => c.is_closed(),
            Db::My(c) => c.is_closed(),
            Db::Ms(c) => c.is_closed(),
        }
    }

    /// Runs `sql` (one or more statements, text protocol) and returns the count the first one
    /// reported (`UPDATE 3` → 3; `None` when it has none). SQL Server: a batch, and the count of
    /// its first result set's rows, else of its last statement.
    pub(crate) async fn execute(&self, sql: &str) -> Result<Option<u64>, ErrorInfo> {
        match self {
            Db::Pg(c) => pg::execute(c, sql).await.map_err(|e| pg::error_info(&e)),
            Db::My(c) => c.execute(sql).await.map_err(|e| my::error_info_at(&e, sql)),
            Db::Ms(c) => c.execute(sql).await.map_err(|e| ms::error_info_at(&e, sql, 0)),
        }
    }

    /// Prepares one statement for the typed protocol: its columns, before any row.
    pub(crate) async fn prepare(&self, sql: &str) -> Result<Prepared, ErrorInfo> {
        match self {
            Db::Pg(c) => {
                let stmt = c.prepare(sql).await.map_err(|e| pg::error_info(&e))?;
                Ok(Prepared { columns: pg::column_infos(&stmt), stmt: PreparedStmt::Pg(c.clone(), stmt, sql.to_string()) })
            }
            Db::My(c) => {
                let stmt = c.prepare(sql).await.map_err(|e| my::error_info_at(&e, sql))?;
                Ok(Prepared { columns: my::column_infos(&stmt), stmt: PreparedStmt::My(c.clone(), stmt) })
            }
            // SQL Server prepares nothing ahead: the batch starts, and its first result set's
            // columns arrive before its rows.
            Db::Ms(c) => {
                let (columns, rows) = ms::start(c, sql).await.map_err(|e| ms::error_info_at(&e, sql, 0))?;
                Ok(Prepared { columns, stmt: PreparedStmt::Ms(parking_lot::Mutex::new(Some(rows))) })
            }
        }
    }
}

/// Rows of a text query (every statement's). Catalog queries use it: text results are the most
/// portable across servers of one family.
pub(crate) async fn text_rows(db: &Db, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    match db {
        Db::Pg(c) => pg::text_rows(c, sql).await,
        Db::My(c) => Ok(c.text_rows(sql).await?),
        Db::Ms(c) => Ok(c.text_rows(sql).await?),
    }
}

/// The first of `queries` that succeeds.
pub(crate) async fn first_ok(db: &Db, queries: &[&str]) -> Result<Vec<Vec<Option<String>>>> {
    let mut last = None;
    for q in queries {
        match text_rows(db, q).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                log::debug!("catalog query failed ({e}): {q}");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("no query")))
}

/// A client on a backend of its own (exports, edits, counts, live queries): never shared with
/// an editor tab, so its transactions (and notifications) are its own.
pub(crate) enum Dedicated {
    Pg(pg::Dedicated),
    My(Arc<my::Client>),
    Ms(Arc<ms::Client>),
}

impl Dedicated {
    pub(crate) fn db(&self) -> Db {
        match self {
            Dedicated::Pg(d) => Db::Pg(d.client.clone()),
            Dedicated::My(c) => Db::My(c.clone()),
            Dedicated::Ms(c) => Db::Ms(c.clone()),
        }
    }

    /// The connection negotiated TLS.
    pub(crate) fn tls_active(&self) -> bool {
        match self {
            Dedicated::Pg(d) => d.tls_active,
            Dedicated::My(c) => c.tls_active,
            Dedicated::Ms(c) => c.tls_active,
        }
    }

    /// Cancels the statement running on it.
    pub(crate) async fn cancel(&self) -> Result<()> {
        match self {
            Dedicated::Pg(d) => d.cancel().await,
            Dedicated::My(c) => c.cancel().await,
            Dedicated::Ms(c) => c.cancel().await,
        }
    }

    /// The PostgreSQL connection, for what only PostgreSQL has (COPY, LISTEN, LAR's SUBSCRIBE).
    /// `None` on other engines: callers check [`Caps`] first.
    pub(crate) fn pg(&self) -> Option<&pg::Dedicated> {
        match self {
            Dedicated::Pg(d) => Some(d),
            Dedicated::My(_) | Dedicated::Ms(_) => None,
        }
    }

    pub(crate) fn pg_mut(&mut self) -> Option<&mut pg::Dedicated> {
        match self {
            Dedicated::Pg(d) => Some(d),
            Dedicated::My(_) | Dedicated::Ms(_) => None,
        }
    }

    pub(crate) fn into_pg(self) -> Result<pg::Dedicated> {
        match self {
            Dedicated::Pg(d) => Ok(d),
            Dedicated::My(_) | Dedicated::Ms(_) => Err(anyhow!("this needs a PostgreSQL connection")),
        }
    }
}

/// Stops the statement running on an editor session's backend.
#[derive(Clone)]
pub(crate) enum Canceller {
    Pg(pg::Canceller),
    My(my::Canceller),
    Ms(ms::Canceller),
}

impl Canceller {
    pub(crate) async fn cancel(&self) -> Result<()> {
        match self {
            Canceller::Pg(c) => c.cancel().await,
            Canceller::My(c) => c.cancel().await,
            Canceller::Ms(c) => c.cancel().await,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Connecting

/// What a SQL path says on an engine without SQL (callers check `Engine::is_sql` first).
fn no_sql(e: Engine) -> anyhow::Error {
    anyhow!("{} has no SQL sessions", e.label())
}

/// A [`Dedicated`] client for the profile (through its SSH tunnel, if any), on `database` or the
/// profile's own.
pub(crate) async fn dedicated(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, tunnel: Option<&Tunnel>) -> Result<Dedicated> {
    match p.engine {
        Engine::Postgres => Ok(Dedicated::Pg(pg::connect_through(p, password, database, tunnel).await?)),
        Engine::MySql => Ok(Dedicated::My(my::connect(p, password, database, tunnel, None).await?)),
        Engine::SqlServer => Ok(Dedicated::Ms(ms::connect(p, password, database, tunnel, None).await?)),
        e => Err(no_sql(e)),
    }
}

/// An editor session's connection: its client and what cancels its statements. The driver
/// reports the session's transaction status (and backend id) to `watch`.
pub(crate) async fn session(
    p: &ConnectionProfile,
    password: Option<&str>,
    database: Option<&str>,
    tunnel: Option<&Tunnel>,
    watch: &Arc<TxWatch>,
) -> Result<(Db, Canceller)> {
    match p.engine {
        Engine::Postgres => {
            let (client, cancel) = pg::connect_session_through(p, password, database, tunnel, watch).await?;
            Ok((Db::Pg(Arc::new(client)), Canceller::Pg(cancel)))
        }
        Engine::MySql => {
            let c = my::connect(p, password, database, tunnel, Some(watch)).await?;
            Ok((Db::My(c.clone()), Canceller::My(c.canceller())))
        }
        Engine::SqlServer => {
            let c = ms::connect(p, password, database, tunnel, Some(watch)).await?;
            Ok((Db::Ms(c.clone()), Canceller::Ms(c.canceller())))
        }
        e => Err(no_sql(e)),
    }
}

/// Version, database, user, search path and the extensions that recognize the server.
pub(crate) async fn server_info(db: &Db, exts: &[Arc<dyn ServerExtension>]) -> Result<ServerInfo> {
    match db {
        Db::Pg(c) => pg::server_info(c, exts).await,
        Db::My(c) => my::server_info(c, exts).await,
        Db::Ms(c) => ms::server_info(c, exts).await,
    }
}

/// The schemas searched for unqualified names, in order (MySQL: the current database; SQL
/// Server: the user's default schema).
pub(crate) async fn search_path(db: &Db, user: &str) -> Vec<String> {
    match db {
        Db::Pg(c) => pg::search_path(c, user).await,
        Db::My(c) => my::search_path(c).await,
        Db::Ms(c) => ms::search_path(c).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Rows

/// One statement's typed rows on their way into the grid's columnar chunks.
pub(crate) trait RowSource: Send {
    type Error;
    /// Reads the next row into `b`; `Ok(false)` at the end. Cancel-safe: dropped before it
    /// returns, no row is lost.
    fn next_into(&mut self, b: &mut ChunkBuilder) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    /// Reads the next row and drops it; `Ok(false)` at the end.
    fn skip(&mut self) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    /// What the command reported once its rows ended (`UPDATE 3`; a SELECT's row count).
    fn affected(&self) -> Option<u64>;
}

/// A prepared statement ([`Db::prepare`]) and its columns.
pub(crate) struct Prepared {
    pub columns: Vec<ColumnInfo>,
    stmt: PreparedStmt,
}

enum PreparedStmt {
    Pg(Arc<tokio_postgres::Client>, tokio_postgres::Statement, String),
    My(Arc<my::Client>, mysql_async::Statement),
    /// The batch already runs: its rows, taken by [`Prepared::start`].
    Ms(parking_lot::Mutex<Option<ms::MsRows>>),
}

impl Prepared {
    /// Runs it: its rows to read. Waiting here may last until the statement's first rows (the
    /// server sends nothing before): race it with whatever cancels.
    pub(crate) async fn start(&self) -> Result<Rows, ErrorInfo> {
        match &self.stmt {
            PreparedStmt::Pg(c, stmt, sql) => pg::start(c, stmt, &self.columns, sql).await.map_err(|e| pg::error_info(&e)),
            PreparedStmt::My(c, stmt) => Ok(Rows::My(my::start(c, stmt, &self.columns).await.map_err(|e| my::error_info(&e))?)),
            PreparedStmt::Ms(rows) => match rows.lock().take() {
                Some(r) => Ok(Rows::Ms(r)),
                None => Err(ErrorInfo { message: "the statement already ran".into(), ..Default::default() }),
            },
        }
    }
}

pub(crate) enum Rows {
    Pg(pg::PgRows),
    My(my::MyRows),
    Ms(ms::MsRows),
}

impl RowSource for Rows {
    type Error = ErrorInfo;

    async fn next_into(&mut self, b: &mut ChunkBuilder) -> Result<bool, ErrorInfo> {
        match self {
            Rows::Pg(r) => r.next_into(b).await.map_err(|e| pg::error_info(&e)),
            Rows::My(r) => r.next_into(b).await.map_err(|e| my::error_info(&e)),
            Rows::Ms(r) => r.next_into(b).await.map_err(|e| ms::error_info(&e)),
        }
    }

    async fn skip(&mut self) -> Result<bool, ErrorInfo> {
        match self {
            Rows::Pg(r) => r.skip().await.map_err(|e| pg::error_info(&e)),
            Rows::My(r) => r.skip().await.map_err(|e| my::error_info(&e)),
            Rows::Ms(r) => r.skip().await.map_err(|e| ms::error_info(&e)),
        }
    }

    fn affected(&self) -> Option<u64> {
        match self {
            Rows::Pg(r) => r.affected(),
            Rows::My(r) => r.affected(),
            Rows::Ms(_) => None,
        }
    }
}

/// How long decoded rows may wait before they are published to the UI.
pub(crate) const FLUSH_EVERY: Duration = Duration::from_millis(20);

pub(crate) fn wants_more(demand: &tokio::sync::watch::Receiver<Demand>, have: usize) -> bool {
    let d = *demand.borrow();
    !d.abort && (d.all || have < d.rows)
}

/// Waits until the reader wants more rows (or abort). Returns false on abort.
pub(crate) async fn wait_for_demand(
    rs: &ResultSet,
    demand: &mut tokio::sync::watch::Receiver<Demand>,
    have: usize,
    on_pause: &(dyn Fn() + Sync),
) -> bool {
    loop {
        let d = *demand.borrow_and_update();
        if d.abort {
            return false;
        }
        if d.all || have < d.rows {
            return true;
        }
        on_pause();
        rs.update_status(|s| {
            s.phase = Some(Phase::Paused);
            s.paused_since.get_or_insert_with(std::time::Instant::now);
        });
        let changed = demand.changed().await;
        // The pause ends here (fetching again, aborted or dropped): its time is not running time.
        rs.update_status(|s| {
            if let Some(p) = s.paused_since.take() {
                s.paused_for += p.elapsed();
            }
            if s.phase == Some(Phase::Paused) && changed.is_ok() {
                s.phase = Some(Phase::Running);
            }
        });
        if changed.is_err() {
            return false;
        }
    }
}

/// Reads a statement's rows into `rs` (its columns already set): chunks of at most
/// [`CHUNK`](crate::result::CHUNK) rows, published at least every [`FLUSH_EVERY`]; pauses when
/// the reader has what it asked for; after an abort reads the rest without keeping it. Returns
/// the command's count for statements without columns.
pub(crate) async fn pump<S: RowSource>(
    src: &mut S,
    rs: &Arc<ResultSet>,
    cols: &[ColumnInfo],
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
) -> Result<Option<u64>, S::Error> {
    let mut b = ChunkBuilder::new(cols);
    let mut demand = rs.demand();
    let mut last_flush = Instant::now();
    let mut first = true;
    let mut draining = false;
    loop {
        if !draining && !wants_more(&demand, rs.rows() + b.len()) {
            // Publish what we have before pausing (or before draining after an abort).
            rs.push_chunk(b.finish());
            last_flush = Instant::now();
            if !wait_for_demand(rs, &mut demand, rs.rows(), on_pause).await {
                draining = true;
            }
        }
        if draining {
            match src.skip().await {
                Ok(true) => continue,
                Err(e) => return Err(e),
                Ok(false) => return Ok(src.affected()),
            }
        }
        let was_empty = b.is_empty();
        let next = if was_empty {
            src.next_into(&mut b).await
        } else {
            let left = FLUSH_EVERY.saturating_sub(last_flush.elapsed());
            match tokio::time::timeout(left, src.next_into(&mut b)).await {
                Ok(n) => n,
                Err(_) => {
                    rs.push_chunk(b.finish());
                    last_flush = Instant::now();
                    continue;
                }
            }
        };
        match next {
            Ok(true) => {
                if first {
                    first = false;
                    let t = started.elapsed();
                    rs.update_status(|s| s.first_row = Some(t));
                }
                if was_empty {
                    last_flush = Instant::now();
                }
                if b.len() >= CHUNK || last_flush.elapsed() >= FLUSH_EVERY {
                    rs.push_chunk(b.finish());
                    last_flush = Instant::now();
                }
            }
            Err(e) => {
                rs.push_chunk(b.finish());
                return Err(e);
            }
            Ok(false) => {
                rs.push_chunk(b.finish());
                let affected = src.affected();
                // A SELECT reports its row count here too; only keep it for row-less commands.
                return Ok(if cols.is_empty() { affected } else { None });
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Editor statements

/// How an editor statement went.
pub(crate) enum Ran {
    /// Stopped before it reached the server: it never ran. `settle`: an error is on its way
    /// (wait for the transaction status after it).
    NeverRan { settle: bool },
    /// It completed; the count of a statement without rows.
    Done(Option<u64>),
    /// The server cancelled it (a CancelRequest).
    Cancelled,
    Failed(ErrorInfo),
}

/// Runs `rs`'s statement (with its parameters) on an editor session's client, its rows into
/// `rs`. `aborted` says whether the user stopped it meanwhile. A text that returns several
/// result sets (a T-SQL batch, a MySQL procedure) puts each after the first into the result `next(prev)` returns
/// (which finishes `prev`); [`Ran`] is then about the last one.
pub(crate) async fn run(
    db: &Db,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
    aborted: &(dyn Fn() -> bool + Sync),
    next: &(dyn Fn(&Arc<ResultSet>) -> Arc<ResultSet> + Sync),
) -> Ran {
    match db {
        Db::Pg(c) => pg::run(c, rs, started, on_pause, aborted).await,
        Db::My(c) => my::run(c, rs, started, on_pause, aborted, next).await,
        Db::Ms(c) => ms::run(c, rs, started, on_pause, aborted, next).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_per_engine() {
        assert_eq!(Caps::of(Engine::Postgres), Caps::POSTGRES);
        assert!(!Caps::of(Engine::MySql).schemas && !Caps::of(Engine::MySql).transactional_ddl);
        let ms = Caps::of(Engine::SqlServer);
        assert!(ms.schemas && ms.functions && ms.triggers && !ms.copy_import && !ms.explain && !ms.roles);
    }
}
