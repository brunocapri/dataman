pub(crate) mod decode;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use mysql_async::consts::StatusFlags;
use mysql_async::prelude::{Protocol, Queryable};
use mysql_async::{Column, Conn, DriverError, Opts, OptsBuilder, Params, QueryResult, Row, SslOpts, Statement, Value};
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};

use self::decode::Dec;
use super::{Ran, RowSource};
use crate::config::{ConnectionProfile, SslMode};
use crate::result::{ChunkBuilder, ColumnInfo, ErrorInfo, ResultSet};
use crate::server::{ServerExtension, ServerInfo};
use crate::tx::{Seen, TxWatch};

type MyError = mysql_async::Error;

const IN_FLIGHT: usize = 1024;

const QUERY_INTERRUPTED: u16 = 1317;

fn closed_error() -> MyError {
    MyError::Driver(DriverError::ConnectionClosed)
}

fn ssl_opts(p: &ConnectionProfile, mode: SslMode, via: Option<SocketAddr>) -> Option<SslOpts> {
    let mut o = match mode {
        SslMode::Disable => return None,
        SslMode::Prefer | SslMode::Require => SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true),
        SslMode::VerifyFull => {
            let roots = rustls_native_certs::load_native_certs().certs.into_iter().map(|c| c.as_ref().to_vec().into()).collect();
            SslOpts::default().with_root_certs(roots)
        }
    };
    if via.is_some() {
        o = o.with_danger_tls_hostname_override(Some(p.host.clone()));
    }
    Some(o)
}

fn opts(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, via: Option<SocketAddr>, ssl: Option<SslOpts>) -> Opts {
    let db = database.filter(|d| !d.is_empty()).or(Some(p.database.as_str()).filter(|d| !d.is_empty()));
    let mut b = OptsBuilder::default()
        .user(Some(p.user.clone()))
        .pass(password.map(str::to_string))
        .db_name(db.map(str::to_string))
        // Never switch to the server's Unix socket on our own (it would leave the tunnel).
        .prefer_socket(false)
        .stmt_cache_size(0)
        // UPDATE reports the rows it matched, as PostgreSQL does (edits count them).
        .client_found_rows(true)
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .ssl_opts(ssl);
    if p.read_only {
        b = b.init(vec!["SET SESSION TRANSACTION READ ONLY"]);
    }
    b = match via {
        Some(a) => b.ip_or_hostname(a.ip().to_string()).tcp_port(a.port()),
        None if p.is_socket() => b.socket(Some(p.host.clone())),
        None => b.ip_or_hostname(p.host.clone()).tcp_port(p.port),
    };
    Opts::from(b)
}

async fn dial(opts: Opts) -> Result<Conn> {
    match tokio::time::timeout(Duration::from_secs(10), Conn::new(opts)).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(anyhow!("connection timed out")),
    }
}

fn no_tls(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<MyError>(), Some(MyError::Driver(DriverError::NoClientSslFlagFromServer)))
}

pub(crate) async fn connect(
    p: &ConnectionProfile,
    password: Option<&str>,
    database: Option<&str>,
    tunnel: Option<&crate::ssh::Tunnel>,
    watch: Option<&Arc<TxWatch>>,
) -> Result<Arc<Client>> {
    let via = match tunnel {
        Some(t) => {
            t.ensure().await?;
            Some(t.local_addr())
        }
        None => None,
    };
    match open(p, password, database, via, watch).await {
        Ok(c) => Ok(c),
        Err(e) => match tunnel.and_then(|t| t.take_error()) {
            Some(s) => Err(s.into()),
            None => Err(e),
        },
    }
}

async fn open(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, via: Option<SocketAddr>, watch: Option<&Arc<TxWatch>>) -> Result<Arc<Client>> {
    super::install_tls_provider();
    let mode = if p.is_socket() && via.is_none() { SslMode::Disable } else { p.sslmode };
    let plain = || opts(p, password, database, via, None);
    let (conn, side, tls_active) = match ssl_opts(p, mode, via) {
        None => {
            let o = plain();
            (dial(o.clone()).await?, o, false)
        }
        Some(ssl) => {
            let o = opts(p, password, database, via, Some(ssl));
            match dial(o.clone()).await {
                Ok(c) => (c, o, true),
                Err(e) if mode == SslMode::Prefer && no_tls(&e) => {
                    let o = plain();
                    (dial(o.clone()).await?, o, false)
                }
                Err(e) => return Err(e),
            }
        }
    };
    let id = conn.id();
    if let Some(w) = watch {
        w.seen(Seen::Pid(id as i32));
    }
    let mark = crate::own::mark(crate::own::server_key(p), id as i32);
    Ok(Arc::new(Client {
        conn: Arc::new(tokio::sync::Mutex::new(Some(conn))),
        closed: AtomicBool::new(false),
        id,
        tls_active,
        watch: watch.cloned(),
        side,
        _mark: mark,
    }))
}

pub(crate) struct Client {
    conn: Arc<tokio::sync::Mutex<Option<Conn>>>,
    closed: AtomicBool,
    pub(crate) id: u32,
    pub(crate) tls_active: bool,
    watch: Option<Arc<TxWatch>>,
    side: Opts,
    _mark: crate::own::Mark,
}

impl Client {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn canceller(&self) -> Canceller {
        Canceller { id: self.id, opts: self.side.clone() }
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        self.canceller().cancel().await
    }

    fn lost(&self, slot: &mut Option<Conn>) {
        *slot = None;
        self.closed.store(true, Ordering::Release);
        if let Some(w) = &self.watch {
            w.closed();
        }
    }

    /// An error carries no status flags, so a `DO 0` asks for the transaction status.
    async fn settle(&self, slot: &mut Option<Conn>, failed: Option<&MyError>) {
        let Some(conn) = slot.as_mut() else { return };
        if failed.is_some_and(|e| !matches!(e, MyError::Server(_))) || conn.is_disconnected() {
            return self.lost(slot);
        }
        let Some(w) = &self.watch else { return };
        if failed.is_some()
            && let Err(e) = conn.query_drop("DO 0").await
        {
            if !matches!(e, MyError::Server(_)) {
                self.lost(slot);
            }
            return;
        }
        if let Some(ok) = conn.last_ok_packet() {
            let open = ok.status_flags().contains(StatusFlags::SERVER_STATUS_IN_TRANS);
            w.seen(Seen::Ready(if open { b'T' } else { b'I' }));
        }
    }

    pub(crate) async fn execute(&self, sql: &str) -> Result<Option<u64>, MyError> {
        let mut g = self.conn.lock().await;
        let Some(conn) = g.as_mut() else { return Err(closed_error()) };
        let r = execute_on(conn, sql).await;
        self.settle(&mut g, r.as_ref().err()).await;
        r
    }

    pub(crate) async fn text_rows(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>, MyError> {
        let mut g = self.conn.lock().await;
        let Some(conn) = g.as_mut() else { return Err(closed_error()) };
        let r = text_rows_on(conn, sql).await;
        self.settle(&mut g, r.as_ref().err()).await;
        r
    }

    pub(crate) async fn prepare(&self, sql: &str) -> Result<Statement, MyError> {
        let mut g = self.conn.lock().await;
        let Some(conn) = g.as_mut() else { return Err(closed_error()) };
        let r = conn.prep(sql).await;
        if let Err(e) = &r
            && (!matches!(e, MyError::Server(_)) || conn.is_disconnected())
        {
            self.lost(&mut g);
        }
        r
    }

    pub(crate) async fn close(&self, stmt: Statement) {
        if let Some(conn) = self.conn.lock().await.as_mut() {
            let _ = conn.close(stmt).await;
        }
    }

    async fn begin(self: &Arc<Self>, exec: Exec) -> Result<(Arc<[Column]>, mpsc::Receiver<Msg>), MyError> {
        let slot = self.conn.clone().lock_owned().await;
        let (first_tx, first_rx) = oneshot::channel();
        let (tx, rx) = mpsc::channel(IN_FLIGHT);
        tokio::spawn(self.clone().read(slot, exec, first_tx, tx));
        match first_rx.await {
            Ok(Ok(cols)) => Ok((cols, rx)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(closed_error()),
        }
    }

    async fn read(self: Arc<Self>, mut slot: OwnedMutexGuard<Option<Conn>>, exec: Exec, first: oneshot::Sender<FirstMsg>, tx: mpsc::Sender<Msg>) {
        let mut first = Some(first);
        let r = match slot.as_mut() {
            None => Err(closed_error()),
            Some(conn) => match &exec {
                Exec::Text(sql) => match conn.query_iter(sql.as_str()).await {
                    Ok(qr) => stream(qr, &mut first, &tx).await,
                    Err(e) => Err(e),
                },
                Exec::Stmt(stmt, params) => match conn.exec_iter(stmt, params.clone()).await {
                    Ok(qr) => stream(qr, &mut first, &tx).await,
                    Err(e) => Err(e),
                },
            },
        };
        if let (Exec::Stmt(stmt, _), Some(conn)) = (&exec, slot.as_mut())
            && !conn.is_disconnected()
        {
            let _ = conn.close(stmt.clone()).await;
        }
        self.settle(&mut slot, r.as_ref().err()).await;
        drop(slot);
        match r {
            Ok(n) => {
                let _ = tx.send(Msg::Done(n)).await;
            }
            Err(e) => match first.take() {
                Some(f) => {
                    let _ = f.send(Err(e));
                }
                None => {
                    let _ = tx.send(Msg::Fail(e)).await;
                }
            },
        }
    }
}

async fn execute_on(conn: &mut Conn, sql: &str) -> Result<Option<u64>, MyError> {
    let mut qr = conn.query_iter(sql).await?;
    let n = match qr.columns() {
        Some(c) if !c.is_empty() => {
            let mut n = 0;
            while qr.next().await?.is_some() {
                n += 1;
            }
            n
        }
        _ => qr.affected_rows(),
    };
    qr.drop_result().await?;
    Ok(Some(n))
}

async fn text_rows_on(conn: &mut Conn, sql: &str) -> Result<Vec<Vec<Option<String>>>, MyError> {
    let mut qr = conn.query_iter(sql).await?;
    let mut out = Vec::new();
    loop {
        while let Some(row) = qr.next().await? {
            out.push((0..row.len()).map(|i| row.as_ref(i).and_then(decode::value_text)).collect());
        }
        if qr.is_empty() {
            break;
        }
    }
    Ok(out)
}

enum Exec {
    Text(String),
    Stmt(Statement, Params),
}

type FirstMsg = Result<Arc<[Column]>, MyError>;

enum Msg {
    Row(Row),
    Set(Arc<[Column]>),
    Done(u64),
    Fail(MyError),
}

async fn stream<P: Protocol>(mut qr: QueryResult<'_, 'static, P>, first: &mut Option<oneshot::Sender<FirstMsg>>, tx: &mpsc::Sender<Msg>) -> Result<u64, MyError> {
    let mut affected = qr.affected_rows();
    let mut n = 0;
    let mut shown = false;
    let mut open = true;
    while let Some(cols) = qr.columns() {
        if cols.is_empty() {
            if !shown {
                affected = qr.affected_rows();
            }
            qr.next().await?;
            continue;
        }
        match first.take() {
            Some(f) => {
                let _ = f.send(Ok(cols.clone()));
            }
            None => {
                if open && tx.send(Msg::Set(cols.clone())).await.is_err() {
                    open = false;
                }
            }
        }
        shown = true;
        n = 0;
        while let Some(row) = qr.next().await? {
            n += 1;
            // A reader that went away (an export stopped) still gets its rows read: the
            // connection is only free after the last one.
            if open && tx.send(Msg::Row(row)).await.is_err() {
                open = false;
            }
        }
    }
    if let Some(f) = first.take() {
        let _ = f.send(Ok(Arc::from(Vec::new())));
    }
    qr.drop_result().await?;
    Ok(if shown { n } else { affected })
}

pub(crate) struct MyRows {
    rx: mpsc::Receiver<Msg>,
    dec: Vec<Dec>,
    affected: Option<u64>,
    ended: bool,
    next_set: Option<Arc<[Column]>>,
}

impl MyRows {
    fn new(rx: mpsc::Receiver<Msg>, dec: Vec<Dec>) -> MyRows {
        MyRows { rx, dec, affected: None, ended: false, next_set: None }
    }

    async fn next_msg(&mut self) -> Result<Option<Row>, MyError> {
        if self.ended || self.next_set.is_some() {
            return Ok(None);
        }
        match self.rx.recv().await {
            Some(Msg::Row(r)) => Ok(Some(r)),
            Some(Msg::Set(cols)) => {
                self.next_set = Some(cols);
                Ok(None)
            }
            Some(Msg::Done(n)) => {
                self.ended = true;
                self.affected = Some(n);
                Ok(None)
            }
            Some(Msg::Fail(e)) => {
                self.ended = true;
                Err(e)
            }
            None => {
                self.ended = true;
                Err(closed_error())
            }
        }
    }
}

impl MyRows {
    fn next_set(&mut self) -> Option<Arc<[Column]>> {
        let cols = self.next_set.take()?;
        let (_, dec) = decode::column_infos(&cols);
        self.dec = dec;
        Some(cols)
    }
}

impl RowSource for MyRows {
    type Error = MyError;

    async fn next_into(&mut self, b: &mut ChunkBuilder) -> Result<bool, MyError> {
        let Some(row) = self.next_msg().await? else { return Ok(false) };
        for (i, d) in self.dec.iter().enumerate() {
            match row.as_ref(i) {
                Some(v) => decode::push_value(b, i, *d, v),
                None => b.push_null(i),
            }
        }
        b.end_row();
        Ok(true)
    }

    async fn skip(&mut self) -> Result<bool, MyError> {
        Ok(self.next_msg().await?.is_some())
    }

    fn affected(&self) -> Option<u64> {
        self.affected
    }
}

pub(crate) async fn start(client: &Arc<Client>, stmt: &Statement, columns: &[ColumnInfo]) -> Result<MyRows, MyError> {
    let (_, rx) = client.begin(Exec::Stmt(stmt.clone(), Params::Empty)).await?;
    let dec = columns.iter().map(|c| Dec { kind: c.kind, bits: c.type_name == "bit" }).collect();
    Ok(MyRows::new(rx, dec))
}

pub(crate) fn column_infos(stmt: &Statement) -> Vec<ColumnInfo> {
    decode::column_infos(&stmt.columns()).0
}

#[derive(Clone)]
pub(crate) struct Canceller {
    id: u32,
    opts: Opts,
}

impl Canceller {
    pub(crate) async fn cancel(&self) -> Result<()> {
        let mut c = dial(self.opts.clone()).await?;
        let r = c.query_drop(format!("KILL QUERY {}", self.id)).await;
        let _ = c.disconnect().await;
        r.context("KILL QUERY")
    }
}

pub(crate) fn version_label(v: &str) -> String {
    if v.contains("MariaDB") {
        // Old MariaDB releases put a replication prefix in front (`5.5.5-10.6.12-MariaDB`).
        let v = v.strip_prefix("5.5.5-").unwrap_or(v);
        format!("MariaDB {}", v.split('-').next().unwrap_or(v))
    } else {
        format!("MySQL {}", v.split('-').next().unwrap_or(v))
    }
}

pub(crate) async fn server_info(client: &Client, exts: &[Arc<dyn ServerExtension>]) -> Result<ServerInfo> {
    let rows = client.text_rows("SELECT VERSION(), DATABASE(), SUBSTRING_INDEX(CURRENT_USER(), '@', 1)").await?;
    let row = rows.into_iter().next().context("VERSION() returned no row")?;
    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let mut info = ServerInfo { version: version_label(&get(0)), database: get(1), user: get(2), ..Default::default() };
    info.search_path = if info.database.is_empty() { vec![] } else { vec![info.database.clone()] };
    info.extensions = exts.iter().filter(|e| e.detect(&info)).map(|e| e.id()).collect();
    Ok(info)
}

pub(crate) async fn search_path(client: &Client) -> Vec<String> {
    match client.text_rows("SELECT DATABASE()").await {
        Ok(rows) => rows.first().and_then(|r| r.first().cloned().flatten()).into_iter().collect(),
        Err(_) => vec![],
    }
}

pub(crate) fn error_info(e: &MyError) -> ErrorInfo {
    match e {
        MyError::Server(se) => ErrorInfo {
            code: Some(se.code.to_string()),
            message: se.message.clone(),
            severity: Some("ERROR".into()),
            ..Default::default()
        },
        e => {
            let mut message = e.to_string();
            let mut src = std::error::Error::source(e);
            while let Some(s) = src {
                let m = s.to_string();
                if !message.contains(&m) {
                    message.push_str(": ");
                    message.push_str(&m);
                }
                src = s.source();
            }
            ErrorInfo { message, ..Default::default() }
        }
    }
}

pub(crate) fn error_info_at(e: &MyError, sql: &str) -> ErrorInfo {
    let mut info = error_info(e);
    if matches!(e, MyError::Server(_)) {
        info.position = near_position(sql, &info.message);
    }
    info
}

pub(crate) fn near_position(sql: &str, message: &str) -> Option<u32> {
    let start = message.find(" near '")? + " near '".len();
    let end = message.rfind("' at line ")?;
    if end < start {
        return None;
    }
    let near = &message[start..end];
    let line: usize = message[end + "' at line ".len()..].trim().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    let line_start = if line <= 1 { 0 } else { sql.match_indices('\n').nth(line - 2).map(|(i, _)| i + 1)? };
    let at = if near.is_empty() {
        sql.trim_end().len().saturating_sub(1).max(line_start)
    } else {
        // MySQL cuts the text at 80 characters, sometimes inside a character: look for a prefix.
        let probe: String = near.chars().take(40).collect();
        line_start + sql[line_start..].find(&probe)?
    };
    Some(sql[..at].chars().count() as u32 + 1)
}

async fn plan(client: &Client, sql: &str, params: &[Option<String>], aborted: &(dyn Fn() -> bool + Sync)) -> Result<Option<Exec>, MyError> {
    match client.prepare(sql).await {
        Ok(stmt) if aborted() => {
            client.close(stmt).await;
            Ok(None)
        }
        Ok(stmt) => {
            let p = if params.is_empty() {
                Params::Empty
            } else {
                Params::Positional(params.iter().map(|v| v.as_ref().map(|s| Value::Bytes(s.clone().into_bytes())).unwrap_or(Value::NULL)).collect())
            };
            Ok(Some(Exec::Stmt(stmt, p)))
        }
        Err(_) if aborted() => Ok(None),
        // With parameters there is no such fallback: values are never spliced into SQL.
        Err(MyError::Server(_)) if params.is_empty() && !client.is_closed() => Ok(Some(Exec::Text(sql.to_string()))),
        Err(e) => Err(e),
    }
}

pub(crate) async fn query(client: &Arc<Client>, sql: &str, params: &[Option<String>]) -> Result<(Vec<ColumnInfo>, MyRows), MyError> {
    let exec = plan(client, sql, params, &|| false).await?.ok_or_else(closed_error)?;
    let (cols, rx) = client.begin(exec).await?;
    let (infos, dec) = decode::column_infos(&cols);
    Ok((infos, MyRows::new(rx, dec)))
}

pub(crate) fn interrupted(e: &MyError) -> bool {
    matches!(e, MyError::Server(se) if se.code == QUERY_INTERRUPTED)
}

pub(crate) async fn run(
    client: &Arc<Client>,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
    aborted: &(dyn Fn() -> bool + Sync),
    next: &(dyn Fn(&Arc<ResultSet>) -> Arc<ResultSet> + Sync),
) -> Ran {
    let fail = |e: &MyError| if interrupted(e) { Ran::Cancelled } else { Ran::Failed(error_info_at(e, &rs.sql)) };
    let exec = match plan(client, &rs.sql, rs.params(), aborted).await {
        Ok(Some(x)) => x,
        Ok(None) => return Ran::NeverRan { settle: false },
        Err(e) => return fail(&e),
    };
    rs.executing.store(true, std::sync::atomic::Ordering::Release);
    let (cols, rx) = match client.begin(exec).await {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let (infos, dec) = decode::column_infos(&cols);
    rs.set_named_origins(cols.iter().map(decode::origin).collect());
    rs.set_columns(infos);
    let cols = rs.columns().unwrap_or_default();
    let mut rows = MyRows::new(rx, dec);
    let mut cur = rs.clone();
    let mut cols = cols;
    loop {
        let n = match super::pump(&mut rows, &cur, &cols, started, on_pause).await {
            Ok(n) => n,
            Err(e) => return fail(&e),
        };
        let Some(set) = rows.next_set() else { return Ran::Done(n) };
        if cur.demand().borrow().abort && !cur.status().partial {
            let _ = client.cancel().await;
            return Ran::Cancelled;
        }
        cur = next(&cur);
        let (infos, _) = decode::column_infos(&set);
        cur.set_named_origins(set.iter().map(decode::origin).collect());
        cur.set_columns(infos);
        cols = cur.columns().unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(version_label("8.4.6"), "MySQL 8.4.6");
        assert_eq!(version_label("8.0.39-0ubuntu0.22.04.1"), "MySQL 8.0.39");
        assert_eq!(version_label("11.4.2-MariaDB-ubu2404"), "MariaDB 11.4.2");
        assert_eq!(version_label("5.5.5-10.6.12-MariaDB-log"), "MariaDB 10.6.12");
    }

    #[test]
    fn syntax_error_positions() {
        let msg = |near: &str, line: usize| {
            format!(
                "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near '{near}' at line {line}"
            )
        };
        let sql = "SELECT * FORM customer";
        assert_eq!(near_position(sql, &msg("FORM customer", 1)), Some(10));
        let sql = "SELECT id,\n       name\n  FROM customer\n WHERE id = = 3";
        let p = near_position(sql, &msg("= 3", 4)).unwrap() as usize;
        assert_eq!(&sql[sql.char_indices().nth(p - 1).unwrap().0..], "= 3", "the second `=` on line 4");
        let sql = "SELECT 'x'\nFROM t WHERE 'x'";
        let p = near_position(sql, &msg("'x'", 2)).unwrap() as usize;
        assert_eq!(p, sql.rfind("'x'").unwrap() + 1);
        assert_eq!(near_position("SELECT * FROM", &msg("", 1)), Some(13));
        let sql = "SELECT 'été' 'it''s' x y z";
        assert_eq!(near_position(sql, &msg("y z", 1)), Some(sql.chars().count() as u32 - 2));
        assert_eq!(near_position("x", "Unknown column 'a' in 'field list'"), None);
    }
}
