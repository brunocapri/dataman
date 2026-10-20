//! tiberius is patched for informational messages (`PRINT`): see the workspace Cargo.toml.

pub(crate) mod decode;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures_util::TryStreamExt;
use tiberius::{AuthMethod, Config, EncryptionLevel, QueryItem, SqlBrowser, ToSql};
use tokio::net::TcpStream;
use tokio::sync::{OwnedMutexGuard, mpsc, watch};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use self::decode::Dec;
use super::{Ran, RowSource};
use crate::config::{ConnectionProfile, SslMode};
use crate::result::{ChunkBuilder, ColumnInfo, ErrorInfo, Phase, ResultSet};
use crate::server::{ServerExtension, ServerInfo};
use crate::sql::split::{GoBatch, go_batches};
use crate::tx::{Seen, TxWatch};

type TClient = tiberius::Client<Compat<TcpStream>>;
pub(crate) type MsError = tiberius::error::Error;

const IN_FLIGHT: usize = 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CANCEL_WAIT: Duration = Duration::from_secs(5);

fn closed_error() -> MsError {
    MsError::Protocol("the connection is closed".into())
}

fn breaks_connection(e: &MsError) -> bool {
    !matches!(e, MsError::Server(_))
}

pub(crate) fn split_instance(host: &str) -> (&str, Option<&str>) {
    match host.split_once('\\') {
        Some((h, i)) if !i.is_empty() => (h, Some(i)),
        _ => (host, None),
    }
}

fn levels(mode: SslMode) -> &'static [EncryptionLevel] {
    match mode {
        SslMode::Disable => &[EncryptionLevel::Off],
        SslMode::Prefer => &[EncryptionLevel::On, EncryptionLevel::Off],
        SslMode::Require | SslMode::VerifyFull => &[EncryptionLevel::Required],
    }
}

/// Through a tunnel the TCP connection goes to the tunnel, while TLS still names (and checks)
/// the profile's host.
fn config(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, level: EncryptionLevel) -> Config {
    let (host, instance) = split_instance(&p.host);
    let mut c = Config::new();
    c.host(host);
    c.port(p.port);
    if let Some(i) = instance {
        c.instance_name(i);
    }
    if let Some(db) = database.filter(|d| !d.is_empty()).or(Some(p.database.as_str()).filter(|d| !d.is_empty())) {
        c.database(db);
    }
    c.authentication(AuthMethod::sql_server(&p.user, password.unwrap_or("")));
    c.application_name("Dataman");
    c.encryption(level);
    if p.sslmode != SslMode::VerifyFull {
        c.trust_cert();
    }
    // `ApplicationIntent=ReadOnly`: routed to a readable secondary where there is one; only for
    // a connection with no writable database (per database, the client-side refusal is all
    // there is: SQL Server has no read-only session setting).
    c.readonly(p.read_only_everywhere());
    c.command_timeout(None);
    c.handshake_timeout(Some(CONNECT_TIMEOUT));
    c
}

async fn dial(c: Config, via: Option<SocketAddr>, named: bool) -> Result<TClient> {
    let tcp = match via {
        Some(a) => TcpStream::connect(a).await?,
        None if named => <TcpStream as SqlBrowser>::connect_named(&c).await?,
        None => TcpStream::connect(c.get_addr()).await?,
    };
    tcp.set_nodelay(true)?;
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(60));
    let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&ka);
    Ok(tiberius::Client::connect(c, tcp.compat_write()).await?)
}

fn declined_tls(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<MsError>(), Some(MsError::Protocol(m)) if m.contains("encryption level"))
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
    match tokio::time::timeout(CONNECT_TIMEOUT, open(p, password, database, via, watch)).await {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => match tunnel.and_then(|t| t.take_error()) {
            Some(s) => Err(s.into()),
            None => Err(e),
        },
        Err(_) => Err(anyhow!("connection timed out")),
    }
}

async fn open(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, via: Option<SocketAddr>, watch: Option<&Arc<TxWatch>>) -> Result<Arc<Client>> {
    super::install_tls_provider();
    let named = split_instance(&p.host).1.is_some();
    let mut last = None;
    let mut found = None;
    for &level in levels(p.sslmode) {
        match dial(config(p, password, database, level), via, named).await {
            Ok(c) => {
                found = Some((c, level != EncryptionLevel::Off));
                break;
            }
            Err(e) if level == EncryptionLevel::On && declined_tls(&e) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    let Some((mut conn, tls_active)) = found else { return Err(last.unwrap_or_else(|| anyhow!("no encryption level to try"))) };
    let spid = conn.simple_query("SELECT @@SPID").await?.into_row().await?.and_then(|r| r.get::<i16, _>(0)).unwrap_or(0) as i32;
    if let Some(w) = watch {
        w.seen(Seen::Pid(spid));
    }
    let mark = crate::own::mark(crate::own::server_key(p), spid);
    Ok(Arc::new(Client {
        conn: Arc::new(tokio::sync::Mutex::new(Some(conn))),
        closed: AtomicBool::new(false),
        tls_active,
        watch: watch.cloned(),
        cancel: Arc::new(watch::Sender::new(0)),
        _mark: mark,
    }))
}

pub(crate) struct Client {
    conn: Arc<tokio::sync::Mutex<Option<TClient>>>,
    closed: AtomicBool,
    pub(crate) tls_active: bool,
    watch: Option<Arc<TxWatch>>,
    cancel: Arc<watch::Sender<u64>>,
    _mark: crate::own::Mark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Probe {
    pub rowcount: u64,
    pub trancount: i32,
    pub xact_state: i16,
}

impl Probe {
    pub(crate) fn status(&self) -> u8 {
        match (self.trancount, self.xact_state) {
            (_, -1) => b'E',
            (0, _) => b'I',
            _ => b'T',
        }
    }
}

const PROBE: &str = "SELECT CAST(@@ROWCOUNT AS bigint), @@TRANCOUNT, CAST(XACT_STATE() AS smallint)";

async fn probe(c: &mut TClient) -> Result<Probe, MsError> {
    let row = c.simple_query(PROBE).await?.into_row().await?;
    let row = row.ok_or_else(|| MsError::Protocol("the state query returned no row".into()))?;
    Ok(Probe {
        rowcount: row.get::<i64, _>(0).unwrap_or(0).max(0) as u64,
        trancount: row.get::<i32, _>(1).unwrap_or(0),
        xact_state: row.get::<i16, _>(2).unwrap_or(0),
    })
}

impl Client {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn canceller(&self) -> Canceller {
        Canceller(self.cancel.clone())
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        self.canceller().cancel().await
    }

    fn lost(&self, slot: &mut Option<TClient>) {
        *slot = None;
        self.closed.store(true, Ordering::Release);
        if let Some(w) = &self.watch {
            w.closed();
        }
    }

    fn report(&self, p: &Probe) {
        if let Some(w) = &self.watch {
            w.seen(Seen::Ready(p.status()));
        }
    }

    async fn settle(&self, slot: &mut Option<TClient>, failed: Option<&MsError>, want: bool) -> Option<Probe> {
        if failed.is_some_and(breaks_connection) {
            self.lost(slot);
            return None;
        }
        if !want && self.watch.is_none() {
            return None;
        }
        let c = slot.as_mut()?;
        match probe(c).await {
            Ok(p) => {
                self.report(&p);
                Some(p)
            }
            Err(e) => {
                if breaks_connection(&e) {
                    self.lost(slot);
                }
                None
            }
        }
    }

    pub(crate) async fn execute(&self, sql: &str) -> Result<Option<u64>, MsError> {
        let mut g = self.conn.lock().await;
        let Some(c) = g.as_mut() else { return Err(closed_error()) };
        let r = match c.simple_query(sql).await {
            Ok(s) => s.into_results().await.map(|r| r.first().map(|rows| rows.len() as u64)),
            Err(e) => Err(e),
        };
        let p = self.settle(&mut g, r.as_ref().err(), matches!(r, Ok(None))).await;
        match r {
            Ok(None) => Ok(p.map(|p| p.rowcount)),
            r => r,
        }
    }

    pub(crate) async fn text_rows(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>, MsError> {
        let mut g = self.conn.lock().await;
        let Some(c) = g.as_mut() else { return Err(closed_error()) };
        let r = async {
            let mut s = c.simple_query(sql).await?;
            let mut out = Vec::new();
            while let Some(item) = s.try_next().await? {
                if let QueryItem::Row(row) = item {
                    out.push(row.cells().map(|(col, v)| decode::value_text(v, col.column_type())).collect());
                }
            }
            Ok(out)
        }
        .await;
        self.settle(&mut g, r.as_ref().err(), false).await;
        r
    }

    async fn begin(self: &Arc<Self>, exec: Exec, count: bool) -> MsRows {
        let slot = self.conn.clone().lock_owned().await;
        let gen0 = *self.cancel.borrow();
        let (tx, rx) = mpsc::channel(IN_FLIGHT);
        tokio::spawn(self.clone().read(slot, exec, gen0, count, tx));
        MsRows::new(rx)
    }

    async fn read(self: Arc<Self>, mut slot: OwnedMutexGuard<Option<TClient>>, exec: Exec, gen0: u64, count: bool, tx: mpsc::Sender<Msg>) {
        let mut cancel = self.cancel.subscribe();
        let Some(c) = slot.as_mut() else {
            let _ = tx.send(Msg::Fail(closed_error())).await;
            return;
        };
        let mut cancelled = false;
        let mut results = false;
        let r = stream(c, &exec, gen0, &mut cancel, &tx, &mut cancelled, &mut results).await;
        let mut lost = r.as_ref().err().is_some_and(breaks_connection);
        if cancelled && !lost {
            lost = !attention(c).await;
        }
        let p = if lost {
            self.lost(&mut slot);
            None
        } else {
            self.settle(&mut slot, None, count).await
        };
        drop(slot);
        let end = match r {
            Err(e) if !cancelled => Msg::Fail(e),
            _ => Msg::End { affected: (!results).then_some(p.map(|p| p.rowcount)).flatten(), cancelled },
        };
        let _ = tx.send(end).await;
    }
}

async fn cancel_asked(rx: &mut watch::Receiver<u64>, gen0: u64) {
    if rx.wait_for(|g| *g > gen0).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// SQL Server ends the cancelled request's response first and may acknowledge in a message of
/// its own after it (always so when the request had not sent anything yet, `WAITFOR`), while
/// tiberius' `cancel_query` stops reading at the end of the first message. A request that fails
/// before anything is written (a procedure name past TDS' 65535 characters) leaves the
/// connection waiting for a response, so the next request first reads that message and drops it.
async fn attention(c: &mut TClient) -> bool {
    match tokio::time::timeout(CANCEL_WAIT, c.cancel_query()).await {
        Ok(Ok(())) => true,
        Ok(Err(MsError::Protocol(m))) if m.contains("acknowledging the Attention") => {
            if tiberius::Command::new("x".repeat(65_536)).exec(c).await.is_ok() {
                return false;
            }
            matches!(tokio::time::timeout(CANCEL_WAIT, probe(c)).await, Ok(Ok(_)))
        }
        Ok(Err(e)) => {
            log::warn!("SQL Server cancel: {e}; the connection is dropped");
            false
        }
        Err(_) => {
            log::warn!("SQL Server cancel not acknowledged; the connection is dropped");
            false
        }
    }
}

const TOKEN_END: Duration = Duration::from_millis(50);

async fn stream(
    c: &mut TClient,
    exec: &Exec,
    gen0: u64,
    cancel: &mut watch::Receiver<u64>,
    tx: &mpsc::Sender<Msg>,
    cancelled: &mut bool,
    results: &mut bool,
) -> Result<(), MsError> {
    let (sql, params) = match exec {
        Exec::Batch(sql) => (sql.as_str(), None),
        Exec::Params(sql, p) => (sql.as_str(), Some(p)),
    };
    let refs: Vec<&dyn ToSql> = params.map(|p| p.iter().map(|v| v as &dyn ToSql).collect()).unwrap_or_default();
    let start = async {
        match params {
            // Values never go into the SQL: `sp_executesql` with `@P1 …` bound as text the server
            // converts.
            Some(_) => c.query(sql, &refs).await,
            // A plain batch (not `sp_executesql`): temporary tables and `USE` outlive it.
            None => c.simple_query(sql).await,
        }
    };
    let mut s = tokio::select! {
        biased;
        _ = cancel_asked(cancel, gen0) => {
            *cancelled = true;
            return Ok(());
        }
        s = start => s?,
    };
    let mut open = true;
    loop {
        let item = tokio::select! {
            biased;
            _ = cancel_asked(cancel, gen0) => {
                // The stream keeps the token it was reading: let it finish while the server
                // sends, so the connection is let go at a token's end.
                let _ = tokio::time::timeout(TOKEN_END, s.try_next()).await;
                *cancelled = true;
                return Ok(());
            }
            it = s.try_next() => it?,
        };
        let Some(item) = item else { return Ok(()) };
        let msg = match item {
            QueryItem::Metadata(m) => {
                *results = true;
                Msg::Columns(m.columns().to_vec().into())
            }
            QueryItem::Row(r) => Msg::Row(r),
            QueryItem::Info(i) => Msg::Info(i.message().to_string()),
            QueryItem::Error(e) => Msg::StmtError(MsError::Server(e)),
        };
        // A reader that went away (an export stopped) still gets its response read: the
        // connection is only free after the last row.
        if open {
            tokio::select! {
                biased;
                _ = cancel_asked(cancel, gen0) => {
                    *cancelled = true;
                    return Ok(());
                }
                r = tx.send(msg) => open = r.is_ok(),
            }
        }
    }
}

enum Exec {
    Batch(String),
    Params(String, Vec<Option<String>>),
}

enum Msg {
    Columns(Arc<[tiberius::Column]>),
    Row(tiberius::Row),
    Info(String),
    StmtError(MsError),
    End { affected: Option<u64>, cancelled: bool },
    Fail(MsError),
}

pub(crate) enum Next {
    Error(MsError),
    Columns(Arc<[tiberius::Column]>),
    End { affected: Option<u64>, cancelled: bool },
    Fail(MsError),
}

pub(crate) struct MsRows {
    rx: mpsc::Receiver<Msg>,
    dec: Vec<Dec>,
    pending: Option<Next>,
    messages: Option<Arc<ResultSet>>,
    pub(crate) positional_errors: bool,
}

impl MsRows {
    fn new(rx: mpsc::Receiver<Msg>) -> MsRows {
        MsRows { rx, dec: Vec::new(), pending: None, messages: None, positional_errors: false }
    }

    fn message(&self, m: String) {
        if let Some(rs) = &self.messages {
            rs.push_message(m);
        }
    }

    pub(crate) async fn next_result(&mut self) -> Next {
        if let Some(n) = self.pending.take() {
            return n;
        }
        loop {
            match self.rx.recv().await {
                Some(Msg::Row(_)) => continue,
                Some(Msg::Info(m)) => self.message(m),
                Some(Msg::StmtError(e)) if self.positional_errors => return Next::Error(e),
                Some(Msg::StmtError(_)) => continue,
                Some(Msg::Columns(c)) => return Next::Columns(c),
                Some(Msg::End { affected, cancelled }) => return Next::End { affected, cancelled },
                Some(Msg::Fail(e)) => return Next::Fail(e),
                None => return Next::Fail(closed_error()),
            }
        }
    }

    pub(crate) fn columns(&mut self, cols: &[tiberius::Column]) -> Vec<ColumnInfo> {
        let (infos, dec) = decode::column_infos(cols);
        self.dec = dec;
        infos
    }

    async fn next_row(&mut self) -> Option<tiberius::Row> {
        if self.pending.is_some() {
            return None;
        }
        let end = loop {
            match self.rx.recv().await {
                Some(Msg::Row(r)) => return Some(r),
                Some(Msg::Info(m)) => self.message(m),
                Some(Msg::StmtError(e)) if self.positional_errors => break Next::Error(e),
                Some(Msg::StmtError(_)) => continue,
                Some(Msg::Columns(c)) => break Next::Columns(c),
                Some(Msg::End { affected, cancelled }) => break Next::End { affected, cancelled },
                Some(Msg::Fail(e)) => break Next::Fail(e),
                None => break Next::Fail(closed_error()),
            }
        };
        self.pending = Some(end);
        None
    }
}

impl RowSource for MsRows {
    type Error = MsError;

    async fn next_into(&mut self, b: &mut ChunkBuilder) -> Result<bool, MsError> {
        let Some(row) = self.next_row().await else { return Ok(false) };
        for (i, ((_, v), d)) in row.cells().zip(&self.dec).enumerate() {
            decode::push_value(b, i, *d, v);
        }
        b.end_row();
        Ok(true)
    }

    async fn skip(&mut self) -> Result<bool, MsError> {
        Ok(self.next_row().await.is_some())
    }

    fn affected(&self) -> Option<u64> {
        None
    }
}

async fn first_result(mut rows: MsRows) -> Result<(Vec<ColumnInfo>, MsRows), MsError> {
    match rows.next_result().await {
        Next::Columns(c) => {
            let infos = rows.columns(&c);
            Ok((infos, rows))
        }
        end @ Next::End { .. } => {
            rows.pending = Some(end);
            Ok((Vec::new(), rows))
        }
        Next::Fail(e) | Next::Error(e) => Err(e),
    }
}

pub(crate) async fn start(client: &Arc<Client>, sql: &str) -> Result<(Vec<ColumnInfo>, MsRows), MsError> {
    first_result(client.begin(Exec::Batch(sql.to_string()), false).await).await
}

pub(crate) async fn query(client: &Arc<Client>, sql: &str, params: &[Option<String>]) -> Result<(Vec<ColumnInfo>, MsRows), MsError> {
    let exec = if params.is_empty() { Exec::Batch(sql.to_string()) } else { Exec::Params(sql.to_string(), params.to_vec()) };
    first_result(client.begin(exec, true).await).await
}

#[derive(Clone)]
pub(crate) struct Canceller(Arc<watch::Sender<u64>>);

impl Canceller {
    pub(crate) async fn cancel(&self) -> Result<()> {
        self.0.send_modify(|g| *g += 1);
        Ok(())
    }
}

fn release(product_version: &str) -> Option<&'static str> {
    Some(match product_version.split('.').next()?.parse::<u32>().ok()? {
        17 => "2025",
        16 => "2022",
        15 => "2019",
        14 => "2017",
        13 => "2016",
        12 => "2014",
        11 => "2012",
        _ => return None,
    })
}

pub(crate) fn version_label(product_version: &str, engine_edition: i32) -> String {
    match engine_edition {
        5 => format!("Azure SQL Database {product_version}"),
        8 => format!("Azure SQL Managed Instance {product_version}"),
        9 => format!("Azure SQL Edge {product_version}"),
        _ => match release(product_version) {
            Some(r) => format!("SQL Server {r} {product_version}"),
            None => format!("SQL Server {product_version}"),
        },
    }
}

pub(crate) async fn server_info(client: &Client, exts: &[Arc<dyn ServerExtension>]) -> Result<ServerInfo> {
    let rows = client
        .text_rows(
            "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), CAST(SERVERPROPERTY('EngineEdition') AS int), \
                    DB_NAME(), SUSER_SNAME(), SCHEMA_NAME(), CAST(SERVERPROPERTY('Edition') AS nvarchar(128))",
        )
        .await?;
    let row = rows.into_iter().next().ok_or_else(|| anyhow!("SERVERPROPERTY returned no row"))?;
    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let edition: i32 = get(1).parse().unwrap_or(0);
    let mut info = ServerInfo { version: version_label(&get(0), edition), database: get(2), user: get(3), ..Default::default() };
    let schema = get(4);
    info.search_path = if schema.is_empty() { vec!["dbo".into()] } else { vec![schema] };
    info.extensions = exts.iter().filter(|e| e.detect(&info)).map(|e| e.id()).collect();
    Ok(info)
}

pub(crate) async fn search_path(client: &Client) -> Vec<String> {
    match client.text_rows("SELECT SCHEMA_NAME()").await {
        Ok(rows) => rows.first().and_then(|r| r.first().cloned().flatten()).into_iter().collect(),
        Err(_) => vec!["dbo".into()],
    }
}

pub(crate) fn error_info(e: &MsError) -> ErrorInfo {
    match e {
        MsError::Server(t) => {
            let mut detail = format!("Level {}, state {}", t.class(), t.state());
            if !t.procedure().is_empty() {
                detail.push_str(&format!(", procedure {}", t.procedure()));
            }
            if t.line() > 0 {
                detail.push_str(&format!(", line {}", t.line()));
            }
            ErrorInfo {
                code: Some(t.code().to_string()),
                message: t.message().to_string(),
                severity: Some("ERROR".into()),
                detail: Some(detail),
                ..Default::default()
            }
        }
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

pub(crate) fn error_info_at(e: &MsError, sql: &str, batch: usize) -> ErrorInfo {
    let mut info = error_info(e);
    if let MsError::Server(t) = e
        && t.procedure().is_empty()
    {
        info.position = line_position(sql, batch, t.line(), near_token(t.message()));
    }
    info
}

fn near_token(message: &str) -> Option<&str> {
    let start = message.find(" near the keyword '").map(|i| i + " near the keyword '".len()).or_else(|| message.find(" near '").map(|i| i + " near '".len()))?;
    let rest = &message[start..];
    Some(&rest[..rest.find('\'')?]).filter(|t| !t.is_empty())
}

pub(crate) fn line_position(sql: &str, batch: usize, line: u32, near: Option<&str>) -> Option<u32> {
    if line == 0 {
        return None;
    }
    let body = sql.get(batch..)?;
    let start = if line == 1 { 0 } else { body.match_indices('\n').nth(line as usize - 2)?.0 + 1 };
    let text = &body[start..];
    let text = &text[..text.find('\n').unwrap_or(text.len())];
    let found = near.and_then(|n| text.to_lowercase().find(&n.to_lowercase())).filter(|i| text.is_char_boundary(*i));
    let at = batch + start + found.unwrap_or(text.len() - text.trim_start().len());
    Some(sql[..at].chars().count() as u32 + 1)
}

pub(crate) async fn run(
    client: &Arc<Client>,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
    aborted: &(dyn Fn() -> bool + Sync),
    next: &(dyn Fn(&Arc<ResultSet>) -> Arc<ResultSet> + Sync),
) -> Ran {
    let sql = rs.sql.as_str();
    let params = rs.params().to_vec();
    let batches = if params.is_empty() { go_batches(sql) } else { vec![GoBatch { range: 0..sql.len(), repeat: 1, end: sql.len() }] };
    let mut cur = rs.clone();
    let mut shown = false;
    let mut affected: Option<u64> = None;
    let mut sent = false;
    let mut reported = false;
    let advance = |prev: &Arc<ResultSet>| {
        let failed = prev.status().error;
        let new = next(prev);
        if let Some(e) = failed {
            prev.update_status(|s| {
                s.phase = Some(Phase::Failed);
                s.error = Some(e);
            });
        }
        new
    };
    for b in batches.iter() {
        for _ in 0..b.repeat {
            let stop = if Arc::ptr_eq(&cur, rs) { aborted() } else { cur.demand().borrow().abort };
            if stop {
                return if sent { Ran::Cancelled } else { Ran::NeverRan { settle: false } };
            }
            let text = sql[b.range.clone()].to_string();
            let exec = if params.is_empty() { Exec::Batch(text) } else { Exec::Params(text, params.clone()) };
            cur.executing.store(true, Ordering::Release);
            sent = true;
            let mut rows = client.begin(exec, true).await;
            rows.positional_errors = true;
            rows.messages = Some(cur.clone());
            let mut stopping = false;
            loop {
                match rows.next_result().await {
                    Next::Columns(_) if stopping => {}
                    Next::Columns(cols) => {
                        if shown {
                            cur = advance(&cur);
                            rows.messages = Some(cur.clone());
                        }
                        shown = true;
                        let infos = rows.columns(&cols);
                        cur.set_columns(infos);
                        let ci = cur.columns().unwrap_or_default();
                        let _ = super::pump(&mut rows, &cur, &ci, started, on_pause).await;
                        if cur.demand().borrow().abort && !cur.status().partial {
                            stopping = true;
                            let _ = client.cancel().await;
                        }
                    }
                    Next::Error(_) if stopping => {}
                    Next::Error(e) => {
                        let info = error_info_at(&e, sql, b.range.start);
                        // A set that has columns and no rows is the failing statement's own
                        // (the server sends the metadata first).
                        let own = cur.rows() == 0 && cur.columns().is_some() && cur.status().error.is_none();
                        if shown && !own {
                            cur = advance(&cur);
                            rows.messages = Some(cur.clone());
                        }
                        shown = true;
                        reported = true;
                        let elapsed = started.elapsed();
                        cur.update_status(|s| {
                            s.error = Some(info);
                            s.elapsed = Some(elapsed);
                        });
                    }
                    Next::End { .. } if stopping => return Ran::Cancelled,
                    Next::End { cancelled: true, .. } => return Ran::Cancelled,
                    Next::End { affected: a, .. } => {
                        if !shown && let Some(a) = a {
                            affected = Some(affected.unwrap_or(0) + a);
                        }
                        break;
                    }
                    Next::Fail(_) if stopping => return Ran::Cancelled,
                    Next::Fail(e) if reported => {
                        let info = error_info_at(&e, sql, b.range.start);
                        return if cur.status().error.is_some() { Ran::Failed(info) } else { Ran::Done(None) };
                    }
                    Next::Fail(e) => {
                        if shown && cur.rows() > 0 {
                            next(&cur);
                        }
                        return Ran::Failed(error_info_at(&e, sql, b.range.start));
                    }
                }
            }
        }
    }
    Ran::Done(if shown { None } else { affected })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(version_label("16.0.4305.5", 3), "SQL Server 2022 16.0.4305.5");
        assert_eq!(version_label("15.0.2000.5", 2), "SQL Server 2019 15.0.2000.5");
        assert_eq!(version_label("12.0.2000.8", 5), "Azure SQL Database 12.0.2000.8");
        assert_eq!(version_label("15.0.0.0", 9), "Azure SQL Edge 15.0.0.0");
        assert_eq!(version_label("99.0", 3), "SQL Server 99.0");
        let info = ServerInfo { version: version_label("16.0.4305.5", 3), ..Default::default() };
        assert_eq!(info.short_version(), "SQL Server 2022");
    }

    #[test]
    fn instances() {
        assert_eq!(split_instance("db1\\SQLEXPRESS"), ("db1", Some("SQLEXPRESS")));
        assert_eq!(split_instance("db1"), ("db1", None));
        assert_eq!(split_instance("db1\\"), ("db1\\", None));
    }

    #[test]
    fn probe_status() {
        let p = |trancount, xact_state| Probe { rowcount: 0, trancount, xact_state }.status();
        assert_eq!(p(0, 0), b'I');
        assert_eq!(p(1, 1), b'T');
        assert_eq!(p(2, 1), b'T');
        assert_eq!(p(1, -1), b'E', "a doomed transaction only rolls back");
    }

    #[test]
    fn error_lines_across_batches() {
        let sql = "SELECT 1\nGO\n-- second\nSELECT 2\n  FROM nope\nGO";
        let b = go_batches(sql);
        assert_eq!(b.len(), 2);
        let second = b[1].range.start;
        assert_eq!(&sql[second..second + 8], "SELECT 2");
        let p = line_position(sql, second, 2, None).unwrap() as usize;
        assert_eq!(&sql[p - 1..p + 3], "FROM");
        assert_eq!(line_position(sql, second, 1, None).unwrap() as usize, second + 1);
        assert_eq!(line_position(sql, 0, 0, None), None);
        assert_eq!(line_position(sql, second, 40, None), None);
        let p = line_position(sql, second, 2, near_token("Incorrect syntax near the keyword 'nope'.")).unwrap() as usize;
        assert_eq!(&sql[p - 1..p + 3], "nope");
        assert_eq!(near_token("Incorrect syntax near ','."), Some(","));
        assert_eq!(near_token("Invalid object name 'x'."), None);
        let sql = "SELECT 'été'\nGO\nSELECT x";
        let b = go_batches(sql);
        assert_eq!(line_position(sql, b[1].range.start, 1, None), Some(sql.chars().count() as u32 - 7));
    }
}
