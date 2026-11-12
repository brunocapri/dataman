// Without their feature, the file engines connect to nothing (and say so).
#[cfg(feature = "duckdb")]
pub(crate) mod duck;
#[cfg(not(feature = "duckdb"))]
#[path = "duck/off.rs"]
pub(crate) mod duck;
pub(crate) mod ch;
pub mod class;
pub(crate) mod embed;
#[cfg(feature = "sqlite")]
pub(crate) mod lite;
#[cfg(not(feature = "sqlite"))]
#[path = "lite/off.rs"]
pub(crate) mod lite;
pub(crate) mod mongo;
pub(crate) mod ms;
pub(crate) mod my;
pub(crate) mod pg;
pub(crate) mod redis;
pub(crate) mod search;

/// Linux connects a socket to itself when nothing listens on a local port inside the ephemeral
/// range: the connect takes that very port as its own and succeeds (a TCP simultaneous open),
/// then the client waits for a server that is not there.
pub(crate) fn refuse_self_connect(t: &tokio::net::TcpStream) -> std::io::Result<()> {
    match (t.local_addr(), t.peer_addr()) {
        (Ok(a), Ok(b)) if a == b => Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, format!("nothing listens on {b}"))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod self_connect {
    #[test]
    fn a_socket_connected_to_itself_is_refused() {
        let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        s.bind(&"127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap().into()).unwrap();
        let me = s.local_addr().unwrap();
        // A simultaneous open with itself: what a busy ephemeral range does by chance.
        if s.connect(&me).is_err() {
            return;
        }
        let std: std::net::TcpStream = s.into();
        std.set_nonblocking(true).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = rt.enter();
        let t = tokio::net::TcpStream::from_std(std).unwrap();
        let err = super::refuse_self_connect(&t).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
        assert!(err.to_string().contains("nothing listens on 127.0.0.1:"), "{err}");
    }
}

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
use crate::result::{CHUNK, Chunk, ChunkBuilder, ColumnInfo, Demand, ErrorInfo, Phase, ResultSet};
use crate::server::ServerInfo;
use crate::ssh::Tunnel;
use crate::tx::TxWatch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub schemas: bool,
    pub transactional_ddl: bool,
    pub import: bool,
    pub copy_import: bool,
    pub explain: bool,
    pub activity: bool,
    pub notifications: bool,
    pub functions: bool,
    pub sequences: bool,
    pub types: bool,
    pub triggers: bool,
    pub toggle_triggers: bool,
    pub extensions: bool,
    pub roles: bool,
    pub privileges: bool,
    /// A database can be dropped (a file database's file is not).
    pub drop_database: bool,
    /// `TRUNCATE` exists (SQLite empties a table with `DELETE`).
    pub truncate: bool,
    /// The database is a file on this machine (it can be shown in Finder).
    pub file: bool,
    /// The editor's text is checked by the server as it is typed (SQL Server's `SET PARSEONLY`).
    pub server_parse_check: bool,
    /// One process holds the file at a time (DuckDB): another app's lock is said so.
    pub exclusive_lock: bool,
    /// Its names for a UUID column, in lower case. SQLite takes any type name: `UUID` as declared;
    /// MySQL has none of its own, MariaDB's is `uuid`.
    pub uuid_types: &'static [&'static str],
    pub uuid_upper: bool,
    /// An `INTEGER PRIMARY KEY` left out of an INSERT takes the next rowid.
    pub rowid_key: bool,
    /// A document field's values can be read by its path (`sample_field_values`).
    pub field_values: bool,
    /// A row can be told apart and changed from the grid. ClickHouse's primary key is a sort key:
    /// it does not name one row, and its mutations run later, on their own.
    pub row_edits: bool,
    /// Dataman can make an agent a login of its own on this engine.
    pub agent_login: bool,
    /// Read-only is how the connection was made: changing it makes a new one, between transactions.
    pub read_only_on_connect: bool,
    /// A table can read other tables, or be filled from them, whatever kind it says it is
    /// (ClickHouse's Merge, Buffer, Dictionary, a materialized view's target): what agents may
    /// reach through a relation is checked on every relation, not only on views.
    pub tables_read_others: bool,
    /// The server keeps a read-only connection from writing (a read-only session, or the way it
    /// was opened). Where it does not, Dataman's check is all there is: what is not known to read
    /// is refused, and a read-only tab's statements run in read-only transactions.
    pub read_only_held: bool,
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
        drop_database: true,
        truncate: true,
        file: false,
        server_parse_check: false,
        exclusive_lock: false,
        uuid_types: &["uuid"],
        uuid_upper: false,
        rowid_key: false,
        field_values: false,
        row_edits: true,
        agent_login: true,
        read_only_on_connect: false,
        tables_read_others: false,
        read_only_held: true,
    };

    pub(crate) const BASIC: Caps = Caps {
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
        drop_database: true,
        truncate: true,
        file: false,
        server_parse_check: false,
        exclusive_lock: false,
        uuid_types: &[],
        uuid_upper: false,
        rowid_key: false,
        field_values: false,
        row_edits: true,
        agent_login: true,
        read_only_on_connect: false,
        tables_read_others: false,
        read_only_held: true,
    };

    pub const NONE: Caps = Caps { schemas: false, ..Caps::BASIC };

    pub const MYSQL: Caps = Caps { schemas: false, functions: true, triggers: true, uuid_types: &["uuid"], ..Caps::BASIC };
    pub const SQL_SERVER: Caps =
        Caps { transactional_ddl: true, functions: true, sequences: true, types: true, triggers: true, toggle_triggers: true, server_parse_check: true, uuid_types: &["uniqueidentifier"], uuid_upper: true, read_only_held: false, ..Caps::BASIC };
    pub const SQLITE: Caps = Caps { schemas: false, transactional_ddl: true, explain: true, triggers: true, drop_database: false, truncate: false, file: true, uuid_types: &["uuid"], rowid_key: true, agent_login: false, read_only_on_connect: true, ..Caps::BASIC };
    pub const DUCKDB: Caps = Caps { transactional_ddl: true, explain: true, drop_database: false, file: true, exclusive_lock: true, uuid_types: &["uuid"], agent_login: false, read_only_on_connect: true, ..Caps::BASIC };
    pub const CLICKHOUSE: Caps = Caps { schemas: false, uuid_types: &["uuid"], row_edits: false, agent_login: false, read_only_on_connect: true, tables_read_others: true, ..Caps::BASIC };

    pub fn of(engine: Engine) -> Caps {
        engine.spec().caps
    }

    /// What the server that answered can do: its engine's, less what its flavor lacks.
    pub fn of_server(engine: Engine, flavor: crate::server::Flavor) -> Caps {
        let caps = Caps::of(engine);
        let spec = flavor.spec_on(engine);
        let held = spec.is_none_or(|f| f.read_only_held);
        let ddl = spec.is_none_or(|f| f.transactional_ddl);
        Caps { read_only_held: caps.read_only_held && held, transactional_ddl: caps.transactional_ddl && ddl, ..caps }
    }
}

pub fn server_error(e: &anyhow::Error) -> Option<ErrorInfo> {
    e.chain().find_map(|c| {
        if let Some(e) = c.downcast_ref::<tokio_postgres::Error>() {
            return e.as_db_error().is_some().then(|| pg::error_info(e));
        }
        if let Some(e @ ms::MsError::Server(_)) = c.downcast_ref::<ms::MsError>() {
            return Some(ms::error_info(e));
        }
        if let Some(e @ ch::ChError::Server { .. }) = c.downcast_ref::<ch::ChError>() {
            return Some(ch::error_info(e));
        }
        #[cfg(feature = "sqlite")]
        if let Some(e @ (rusqlite::Error::SqliteFailure(..) | rusqlite::Error::SqlInputError { .. })) = c.downcast_ref::<lite::LiteError>() {
            return Some(lite::error_info(e));
        }
        #[cfg(feature = "duckdb")]
        if let Some(e @ duckdb::Error::DuckDBFailure(..)) = c.downcast_ref::<duck::DuckError>() {
            return Some(duck::error_info(e));
        }
        match c.downcast_ref::<mysql_async::Error>() {
            Some(e @ mysql_async::Error::Server(_)) => Some(my::error_info(e)),
            _ => None,
        }
    })
}

pub fn server_error_class(e: &anyhow::Error) -> Option<crate::result::ErrorClass> {
    use crate::result::ErrorClass;
    e.chain().find_map(|c| {
        if let Some(e) = c.downcast_ref::<tokio_postgres::Error>() {
            return e.as_db_error().map(|d| class::of(Engine::Postgres, d.code().code()));
        }
        if let Some(e @ ms::MsError::Server(_)) = c.downcast_ref::<ms::MsError>() {
            return Some(ms::error_info(e).class(Engine::SqlServer));
        }
        if let Some(e @ ch::ChError::Server { .. }) = c.downcast_ref::<ch::ChError>() {
            return Some(ch::error_info(e).class(Engine::ClickHouse));
        }
        #[cfg(feature = "sqlite")]
        if let Some(e @ (rusqlite::Error::SqliteFailure(..) | rusqlite::Error::SqlInputError { .. })) = c.downcast_ref::<lite::LiteError>() {
            return Some(lite::error_info(e).class(Engine::Sqlite));
        }
        #[cfg(feature = "duckdb")]
        if let Some(e @ duckdb::Error::DuckDBFailure(..)) = c.downcast_ref::<duck::DuckError>() {
            return Some(duck::error_info(e).class(Engine::DuckDb));
        }
        if let Some(mysql_async::Error::Server(se)) = c.downcast_ref::<mysql_async::Error>() {
            return Some(class::of(Engine::MySql, &se.code.to_string()));
        }
        if let Some(e) = c.downcast_ref::<mongodb::error::Error>() {
            use mongodb::error::ErrorKind;
            return match &*e.kind {
                ErrorKind::Authentication { .. } => Some(ErrorClass::Auth),
                ErrorKind::Command(ce) => Some(class::of(Engine::MongoDb, &ce.code.to_string())),
                _ => None,
            };
        }
        if let Some(e) = c.downcast_ref::<redis::ReplyError>() {
            return Some(class::of(Engine::Redis, e.code()));
        }
        c.downcast_ref::<ConnectError>().map(|ConnectError::Auth(_)| ErrorClass::Auth)
    })
}

/// A typed connect failure where the driver gives no class of its own (the message as the
/// server put it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    Auth(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Auth(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ConnectError {}

/// The server refused the login: ask for a password (whatever language the server speaks). A
/// driver's own refusal before any server answer ("password missing") says so in its words,
/// which are the driver's, not the server's.
pub fn needs_password(e: &anyhow::Error) -> bool {
    match server_error_class(e) {
        Some(c) => c == crate::result::ErrorClass::Auth,
        None => format!("{e:#}").to_ascii_lowercase().contains("password"),
    }
}

#[derive(Clone)]
pub(crate) enum Db {
    Pg(Arc<tokio_postgres::Client>),
    My(Arc<my::Client>),
    Ms(Arc<ms::Client>),
    Lite(Arc<lite::Client>),
    Duck(Arc<duck::Client>),
    Ch(Arc<ch::Client>),
}

impl Db {
    pub(crate) fn is_closed(&self) -> bool {
        match self {
            Db::Pg(c) => c.is_closed(),
            Db::My(c) => c.is_closed(),
            Db::Ms(c) => c.is_closed(),
            Db::Lite(c) => c.is_closed(),
            Db::Duck(c) => c.is_closed(),
            Db::Ch(c) => c.is_closed(),
        }
    }

    pub(crate) async fn execute(&self, sql: &str) -> Result<Option<u64>, ErrorInfo> {
        match self {
            Db::Pg(c) => pg::execute(c, sql).await.map_err(|e| pg::error_info(&e)),
            Db::My(c) => c.execute(sql).await.map_err(|e| my::error_info_at(&e, sql)),
            Db::Ms(c) => c.execute(sql).await.map_err(|e| ms::error_info_at(&e, sql, 0)),
            Db::Lite(c) => c.execute(sql).await.map_err(|e| lite::error_info_at(&e, sql)),
            Db::Duck(c) => c.execute(sql).await.map_err(|e| duck::error_info_at(&e, sql)),
            Db::Ch(c) => c.execute(sql).await.map_err(|e| ch::error_info_at(&e, sql)),
        }
    }

    pub(crate) async fn prepare(&self, sql: &str) -> Result<Prepared, ErrorInfo> {
        match self {
            Db::Pg(c) => {
                let stmt = c.describe(sql).await.map_err(|e| pg::error_info(&e))?;
                Ok(Prepared { columns: pg::column_infos(&stmt), stmt: PreparedStmt::Pg(c.clone(), stmt, sql.to_string()) })
            }
            Db::My(c) => {
                let stmt = c.prepare(sql).await.map_err(|e| my::error_info_at(&e, sql))?;
                Ok(Prepared { columns: my::column_infos(&stmt), stmt: PreparedStmt::My(c.clone(), stmt) })
            }
            Db::Ms(c) => {
                let (columns, rows) = ms::start(c, sql).await.map_err(|e| ms::error_info_at(&e, sql, 0))?;
                Ok(Prepared { columns, stmt: PreparedStmt::Ms(parking_lot::Mutex::new(Some(rows))) })
            }
            Db::Lite(c) => {
                let columns = c.prepare(sql).await.map_err(|e| lite::error_info_at(&e, sql))?;
                Ok(Prepared { stmt: PreparedStmt::Lite(c.clone(), sql.to_string(), columns.iter().map(|c| c.kind).collect()), columns })
            }
            // DuckDB knows the columns once the query runs: it runs once, here, as SQL Server's.
            Db::Duck(c) => {
                let ((columns, _), rows) = c.stream(sql, &[]).await.map_err(|e| duck::error_info_at(&e, sql))?;
                Ok(Prepared { columns, stmt: PreparedStmt::Duck(parking_lot::Mutex::new(Some(rows))) })
            }
            // The columns come with the rows: it runs once, here, as SQL Server's.
            Db::Ch(c) => {
                let (columns, rows) = c.query(sql, &[]).await.map_err(|e| ch::error_info_at(&e, sql))?;
                Ok(Prepared { columns, stmt: PreparedStmt::Ch(parking_lot::Mutex::new(Some(rows))) })
            }
        }
    }
}

pub(crate) async fn text_rows(db: &Db, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    match db {
        Db::Pg(c) => pg::text_rows(c, sql).await,
        Db::My(c) => Ok(c.text_rows(sql).await?),
        Db::Ms(c) => Ok(c.text_rows(sql).await?),
        Db::Lite(c) => Ok(c.text_rows(sql).await?),
        Db::Duck(c) => Ok(c.text_rows(sql).await?),
        Db::Ch(c) => Ok(c.text_rows(sql).await?),
    }
}

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

pub(crate) enum Dedicated {
    Pg(pg::Dedicated),
    My(Arc<my::Client>),
    Ms(Arc<ms::Client>),
    Lite(Arc<lite::Client>),
    Duck(Arc<duck::Client>),
    Ch(Arc<ch::Client>),
}

impl Dedicated {
    pub(crate) fn db(&self) -> Db {
        match self {
            Dedicated::Pg(d) => Db::Pg(d.client.clone()),
            Dedicated::My(c) => Db::My(c.clone()),
            Dedicated::Ms(c) => Db::Ms(c.clone()),
            Dedicated::Lite(c) => Db::Lite(c.clone()),
            Dedicated::Duck(c) => Db::Duck(c.clone()),
            Dedicated::Ch(c) => Db::Ch(c.clone()),
        }
    }

    pub(crate) fn tls_active(&self) -> bool {
        match self {
            Dedicated::Pg(d) => d.tls_active,
            Dedicated::My(c) => c.tls_active,
            Dedicated::Ms(c) => c.tls_active,
            Dedicated::Lite(_) | Dedicated::Duck(_) => false,
            Dedicated::Ch(c) => c.tls_active,
        }
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        match self {
            Dedicated::Pg(d) => d.cancel().await,
            Dedicated::My(c) => c.cancel().await,
            Dedicated::Ms(c) => c.cancel().await,
            Dedicated::Lite(c) => c.cancel().await,
            Dedicated::Duck(c) => c.cancel().await,
            Dedicated::Ch(c) => c.cancel().await,
        }
    }

    pub(crate) fn pg(&self) -> Option<&pg::Dedicated> {
        match self {
            Dedicated::Pg(d) => Some(d),
            Dedicated::My(_) | Dedicated::Ms(_) | Dedicated::Lite(_) | Dedicated::Duck(_) | Dedicated::Ch(_) => None,
        }
    }

    /// A pooler between this connection and the server (PostgreSQL only).
    pub(crate) fn pooler(&self) -> Option<crate::pooler::Pooler> {
        match self {
            Dedicated::Pg(d) => d.pooler,
            Dedicated::My(_) | Dedicated::Ms(_) | Dedicated::Lite(_) | Dedicated::Duck(_) | Dedicated::Ch(_) => None,
        }
    }

    pub(crate) fn pg_mut(&mut self) -> Option<&mut pg::Dedicated> {
        match self {
            Dedicated::Pg(d) => Some(d),
            Dedicated::My(_) | Dedicated::Ms(_) | Dedicated::Lite(_) | Dedicated::Duck(_) | Dedicated::Ch(_) => None,
        }
    }
}

#[derive(Clone)]
pub(crate) enum Canceller {
    Pg(pg::Canceller),
    My(my::Canceller),
    Ms(ms::Canceller),
    Lite(lite::Canceller),
    Duck(duck::Canceller),
    Ch(ch::Canceller),
}

impl Canceller {
    pub(crate) async fn cancel(&self) -> Result<()> {
        match self {
            Canceller::Pg(c) => c.cancel().await,
            Canceller::My(c) => c.cancel().await,
            Canceller::Ms(c) => c.cancel().await,
            Canceller::Lite(c) => c.cancel().await,
            Canceller::Duck(c) => c.cancel().await,
            Canceller::Ch(c) => c.cancel().await,
        }
    }
}

fn no_sql(e: Engine) -> anyhow::Error {
    anyhow!("{} has no SQL sessions", e.label())
}

pub(crate) async fn dedicated(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, tunnel: Option<&Tunnel>) -> Result<Dedicated> {
    match p.engine {
        Engine::Postgres => Ok(Dedicated::Pg(pg::connect_through(p, password, database, tunnel).await?)),
        Engine::MySql => Ok(Dedicated::My(my::connect(p, password, database, tunnel, None).await?)),
        Engine::SqlServer => Ok(Dedicated::Ms(ms::connect(p, password, database, tunnel, None).await?)),
        Engine::Sqlite => Ok(Dedicated::Lite(lite::connect(p, lite::Open { read_only: p.read_only, ..Default::default() }, None).await?)),
        Engine::DuckDb => Ok(Dedicated::Duck(duck::connect(p, database, None).await?)),
        Engine::ClickHouse => Ok(Dedicated::Ch(ch::connect(p, password, database, tunnel, false).await?)),
        e => Err(no_sql(e)),
    }
}

pub(crate) async fn session(
    p: &ConnectionProfile,
    password: Option<&str>,
    database: Option<&str>,
    tunnel: Option<&Tunnel>,
    watch: &Arc<TxWatch>,
    pooler: Option<crate::pooler::Pooler>,
) -> Result<(Db, Canceller)> {
    match p.engine {
        Engine::Postgres => {
            let (client, cancel) = pg::connect_session_through(p, password, database, tunnel, watch, pooler).await?;
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
        Engine::Sqlite => {
            let c = lite::connect(p, lite::Open { read_only: p.read_only, ..Default::default() }, Some(watch)).await?;
            Ok((Db::Lite(c.clone()), Canceller::Lite(c.canceller())))
        }
        Engine::DuckDb => {
            let c = duck::connect(p, database, Some(watch)).await?;
            Ok((Db::Duck(c.clone()), Canceller::Duck(c.canceller())))
        }
        Engine::ClickHouse => {
            let c = ch::connect(p, password, database, tunnel, true).await?;
            Ok((Db::Ch(c.clone()), Canceller::Ch(c)))
        }
        e => Err(no_sql(e)),
    }
}

/// What the server is; one older than its engine's floor is refused here, so that neither a
/// connection nor its test reaches a catalog Dataman does not read.
pub(crate) async fn server_info(db: &Db) -> Result<ServerInfo> {
    let info = match db {
        Db::Pg(c) => pg::server_info(c).await,
        Db::My(c) => my::server_info(c).await,
        Db::Ms(c) => ms::server_info(c).await,
        Db::Lite(c) => lite::server_info(c).await,
        Db::Duck(c) => duck::server_info(c).await,
        Db::Ch(c) => ch::server_info(c).await,
    }?;
    match info.below_floor() {
        Some(why) => Err(anyhow!("{why}")),
        None => Ok(info),
    }
}

pub(crate) async fn search_path(db: &Db, user: &str) -> Vec<String> {
    match db {
        Db::Pg(c) => pg::search_path(c, user).await,
        Db::My(c) => my::search_path(c).await,
        Db::Ms(c) => ms::search_path(c).await,
        Db::Lite(_) => vec!["main".into()],
        Db::Duck(c) => duck::search_path(c).await,
        Db::Ch(c) => ch::search_path(c).await,
    }
}

pub(crate) trait RowSource: Send {
    type Error;
    /// Must be cancel-safe: dropped before it returns, no row is lost.
    fn next_into(&mut self, b: &mut ChunkBuilder) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn skip(&mut self) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn affected(&self) -> Option<u64>;
}

pub(crate) struct Prepared {
    pub columns: Vec<ColumnInfo>,
    stmt: PreparedStmt,
}

enum PreparedStmt {
    Pg(Arc<tokio_postgres::Client>, tokio_postgres::Statement, String),
    My(Arc<my::Client>, mysql_async::Statement),
    Ms(parking_lot::Mutex<Option<ms::MsRows>>),
    Lite(Arc<lite::Client>, String, Vec<crate::value::Kind>),
    Duck(parking_lot::Mutex<Option<embed::Rows<duck::DuckError>>>),
    Ch(parking_lot::Mutex<Option<ch::ChRows>>),
}

impl Prepared {
    /// Waiting here may last until the statement's first rows (the server sends nothing before):
    /// race it with whatever cancels.
    pub(crate) async fn start(&self) -> Result<Rows, ErrorInfo> {
        match &self.stmt {
            PreparedStmt::Pg(c, stmt, sql) => pg::start(c, stmt, &self.columns, sql).await.map_err(|e| pg::error_info(&e)),
            PreparedStmt::My(c, stmt) => Ok(Rows::My(my::start(c, stmt, &self.columns).await.map_err(|e| my::error_info(&e))?)),
            PreparedStmt::Ms(rows) => match rows.lock().take() {
                Some(r) => Ok(Rows::Ms(r)),
                None => Err(ErrorInfo { message: "the statement already ran".into(), ..Default::default() }),
            },
            PreparedStmt::Lite(c, sql, kinds) => match c.stream(sql, &[], Some(kinds.clone())).await {
                Ok((_, rows)) => Ok(Rows::Lite(rows)),
                Err(e) => Err(lite::error_info_at(&e, sql)),
            },
            PreparedStmt::Duck(rows) => match rows.lock().take() {
                Some(r) => Ok(Rows::Duck(r)),
                None => Err(ErrorInfo { message: "the statement already ran".into(), ..Default::default() }),
            },
            PreparedStmt::Ch(rows) => match rows.lock().take() {
                Some(r) => Ok(Rows::Ch(r)),
                None => Err(ErrorInfo { message: "the statement already ran".into(), ..Default::default() }),
            },
        }
    }
}

pub(crate) enum Rows {
    Pg(pg::PgRows),
    My(my::MyRows),
    Ms(ms::MsRows),
    Lite(embed::Rows<lite::LiteError>),
    Duck(embed::Rows<duck::DuckError>),
    Ch(ch::ChRows),
}

impl RowSource for Rows {
    type Error = ErrorInfo;

    async fn next_into(&mut self, b: &mut ChunkBuilder) -> Result<bool, ErrorInfo> {
        match self {
            Rows::Pg(r) => r.next_into(b).await.map_err(|e| pg::error_info(&e)),
            Rows::My(r) => r.next_into(b).await.map_err(|e| my::error_info(&e)),
            Rows::Ms(r) => r.next_into(b).await.map_err(|e| ms::error_info(&e)),
            Rows::Lite(r) => r.next_into(b).await.map_err(|e| lite::error_info(&e)),
            Rows::Duck(r) => r.next_into(b).await.map_err(|e| duck::error_info(&e)),
            Rows::Ch(r) => r.next_into(b).await.map_err(|e| ch::error_info(&e)),
        }
    }

    async fn skip(&mut self) -> Result<bool, ErrorInfo> {
        match self {
            Rows::Pg(r) => r.skip().await.map_err(|e| pg::error_info(&e)),
            Rows::My(r) => r.skip().await.map_err(|e| my::error_info(&e)),
            Rows::Ms(r) => r.skip().await.map_err(|e| ms::error_info(&e)),
            Rows::Lite(r) => r.skip().await.map_err(|e| lite::error_info(&e)),
            Rows::Duck(r) => r.skip().await.map_err(|e| duck::error_info(&e)),
            Rows::Ch(r) => r.skip().await.map_err(|e| ch::error_info(&e)),
        }
    }

    fn affected(&self) -> Option<u64> {
        match self {
            Rows::Pg(r) => r.affected(),
            Rows::My(r) => r.affected(),
            Rows::Ms(_) => None,
            Rows::Lite(r) => r.affected(),
            Rows::Duck(r) => r.affected(),
            Rows::Ch(r) => r.affected(),
        }
    }
}

pub(crate) const FLUSH_EVERY: Duration = Duration::from_millis(20);

pub(crate) fn wants_more(demand: &tokio::sync::watch::Receiver<Demand>, have: usize) -> bool {
    let d = *demand.borrow();
    !d.abort && (d.all || have < d.rows)
}

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

/// What a paused result reads on while its server connection is not its own (a pooler per
/// transaction hands it to the next client only once the result has ended).
pub(crate) const SHARED_READ_AHEAD_BYTES: usize = 64 * 1024 * 1024;

/// Rows read past what the grid asked for, not shown yet.
#[derive(Default)]
struct Ahead {
    chunks: std::collections::VecDeque<(Chunk, usize)>,
    bytes: usize,
    ended: bool,
}

/// While the grid has what it asked for: reads on into `ahead`, up to `cap` bytes or the end of
/// the result, until the grid asks for more.
async fn read_ahead<S: RowSource>(src: &mut S, cols: &[ColumnInfo], demand: &mut tokio::sync::watch::Receiver<Demand>, ahead: &mut Ahead, cap: usize) -> Result<(), S::Error> {
    let mut b = ChunkBuilder::new(cols);
    let keep = |b: &mut ChunkBuilder, ahead: &mut Ahead| {
        let n = b.bytes();
        ahead.bytes += n;
        ahead.chunks.push_back((b.finish(), n));
    };
    while !ahead.ended && ahead.bytes + b.bytes() < cap {
        let more = tokio::select! {
            biased;
            _ = demand.changed() => break,
            r = src.next_into(&mut b) => r?,
        };
        if !more {
            ahead.ended = true;
        } else if b.len() >= CHUNK {
            keep(&mut b, ahead);
        }
    }
    if !b.is_empty() {
        keep(&mut b, ahead);
    }
    Ok(())
}

/// `read_ahead`: bytes read past the grid's demand while it is paused (0: none, the server
/// connection waits with it).
pub(crate) async fn pump<S: RowSource>(
    src: &mut S,
    rs: &Arc<ResultSet>,
    cols: &[ColumnInfo],
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
    read_ahead_bytes: usize,
) -> Result<Option<u64>, S::Error> {
    let mut b = ChunkBuilder::new(cols);
    let mut demand = rs.demand();
    let mut last_flush = Instant::now();
    let mut first = true;
    let mut draining = false;
    let mut ahead = Ahead::default();
    loop {
        if !draining && !wants_more(&demand, rs.rows() + b.len()) {
            rs.push_chunk(b.finish());
            last_flush = Instant::now();
            if read_ahead_bytes > 0
                && let Err(e) = read_ahead(src, cols, &mut demand, &mut ahead, read_ahead_bytes).await
            {
                for (c, _) in ahead.chunks.drain(..) {
                    rs.push_chunk(c);
                }
                return Err(e);
            }
            if !wait_for_demand(rs, &mut demand, rs.rows(), on_pause).await {
                draining = true;
            }
        }
        if draining {
            if ahead.ended {
                return Ok(src.affected());
            }
            match src.skip().await {
                Ok(true) => continue,
                Err(e) => return Err(e),
                Ok(false) => return Ok(src.affected()),
            }
        }
        if let Some((c, n)) = ahead.chunks.pop_front() {
            ahead.bytes -= n;
            rs.push_chunk(c);
            continue;
        }
        if ahead.ended {
            return Ok(if cols.is_empty() { src.affected() } else { None });
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

#[allow(clippy::enum_variant_names)] // "never ran" is the outcome, not a repeat of the type.
pub(crate) enum Ran {
    NeverRan { settle: bool },
    Done(Option<u64>),
    Cancelled,
    Failed(ErrorInfo),
}

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
        Db::Lite(c) => lite::run(c, rs, started, on_pause, aborted, next).await,
        Db::Duck(c) => duck::run(c, rs, started, on_pause, aborted, next).await,
        Db::Ch(c) => ch::run(c, rs, started, on_pause, aborted).await,
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
        let lite = Caps::of(Engine::Sqlite);
        assert!(!lite.schemas && lite.explain && lite.triggers && lite.transactional_ddl && !lite.drop_database && !lite.truncate && !lite.import);
        let duck = Caps::of(Engine::DuckDb);
        assert!(duck.schemas && duck.explain && !duck.triggers && !duck.drop_database && duck.truncate && !duck.activity && !duck.functions);
        let ch = Caps::of(Engine::ClickHouse);
        assert!(!ch.schemas && !ch.transactional_ddl && !ch.row_edits && !ch.explain && !ch.agent_login && ch.read_only_on_connect && ch.truncate);
    }

    #[test]
    fn redis_replies_are_classified_by_their_code() {
        let reply = |text: &str| anyhow::Error::from(redis::ReplyError(text.into())).context("connecting");
        assert!(needs_password(&reply("NOAUTH Authentication required.")));
        assert!(needs_password(&reply("WRONGPASS invalid username-password pair or user is disabled.")));
        assert!(!needs_password(&reply("WRONGTYPE Operation against a key holding the wrong kind of value")));
        assert_eq!(server_error_class(&reply("NOPERM this user has no permissions")), Some(crate::result::ErrorClass::Permission));
    }
}
