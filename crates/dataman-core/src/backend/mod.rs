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

    pub const NONE: Caps = Caps { schemas: false, ..Caps::BASIC };

    pub fn of(engine: Engine) -> Caps {
        match engine {
            Engine::Postgres => Caps::POSTGRES,
            Engine::MySql => Caps { schemas: false, functions: true, triggers: true, ..Caps::BASIC },
            Engine::SqlServer => Caps {
                transactional_ddl: true,
                functions: true,
                sequences: true,
                types: true,
                triggers: true,
                toggle_triggers: true,
                ..Caps::BASIC
            },
            Engine::MongoDb | Engine::Elasticsearch | Engine::OpenSearch | Engine::Redis => Caps::NONE,
        }
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
        match c.downcast_ref::<mysql_async::Error>() {
            Some(e @ mysql_async::Error::Server(_)) => Some(my::error_info(e)),
            _ => None,
        }
    })
}

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

    pub(crate) async fn execute(&self, sql: &str) -> Result<Option<u64>, ErrorInfo> {
        match self {
            Db::Pg(c) => pg::execute(c, sql).await.map_err(|e| pg::error_info(&e)),
            Db::My(c) => c.execute(sql).await.map_err(|e| my::error_info_at(&e, sql)),
            Db::Ms(c) => c.execute(sql).await.map_err(|e| ms::error_info_at(&e, sql, 0)),
        }
    }

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
            Db::Ms(c) => {
                let (columns, rows) = ms::start(c, sql).await.map_err(|e| ms::error_info_at(&e, sql, 0))?;
                Ok(Prepared { columns, stmt: PreparedStmt::Ms(parking_lot::Mutex::new(Some(rows))) })
            }
        }
    }
}

pub(crate) async fn text_rows(db: &Db, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    match db {
        Db::Pg(c) => pg::text_rows(c, sql).await,
        Db::My(c) => Ok(c.text_rows(sql).await?),
        Db::Ms(c) => Ok(c.text_rows(sql).await?),
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
}

impl Dedicated {
    pub(crate) fn db(&self) -> Db {
        match self {
            Dedicated::Pg(d) => Db::Pg(d.client.clone()),
            Dedicated::My(c) => Db::My(c.clone()),
            Dedicated::Ms(c) => Db::Ms(c.clone()),
        }
    }

    pub(crate) fn tls_active(&self) -> bool {
        match self {
            Dedicated::Pg(d) => d.tls_active,
            Dedicated::My(c) => c.tls_active,
            Dedicated::Ms(c) => c.tls_active,
        }
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        match self {
            Dedicated::Pg(d) => d.cancel().await,
            Dedicated::My(c) => c.cancel().await,
            Dedicated::Ms(c) => c.cancel().await,
        }
    }

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

fn no_sql(e: Engine) -> anyhow::Error {
    anyhow!("{} has no SQL sessions", e.label())
}

pub(crate) async fn dedicated(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, tunnel: Option<&Tunnel>) -> Result<Dedicated> {
    match p.engine {
        Engine::Postgres => Ok(Dedicated::Pg(pg::connect_through(p, password, database, tunnel).await?)),
        Engine::MySql => Ok(Dedicated::My(my::connect(p, password, database, tunnel, None).await?)),
        Engine::SqlServer => Ok(Dedicated::Ms(ms::connect(p, password, database, tunnel, None).await?)),
        e => Err(no_sql(e)),
    }
}

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

pub(crate) async fn server_info(db: &Db, exts: &[Arc<dyn ServerExtension>]) -> Result<ServerInfo> {
    match db {
        Db::Pg(c) => pg::server_info(c, exts).await,
        Db::My(c) => my::server_info(c, exts).await,
        Db::Ms(c) => ms::server_info(c, exts).await,
    }
}

pub(crate) async fn search_path(db: &Db, user: &str) -> Vec<String> {
    match db {
        Db::Pg(c) => pg::search_path(c, user).await,
        Db::My(c) => my::search_path(c).await,
        Db::Ms(c) => ms::search_path(c).await,
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
