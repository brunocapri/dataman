//! SQL Server (and Azure SQL) through tiberius: TLS on the same rustls as the other engines, SSH
//! tunnels, named instances, cancels with a TDS attention, the transaction state read after
//! each batch, and rows decoded straight into the grid's chunks.
//!
//! A TDS connection serves one request at a time and its response must be read to the end
//! before the next. Each [`Client`] holds its connection behind an async mutex; a request's
//! response is read by a task that owns the lock until its end and hands the result sets over
//! through a small channel ([`MsRows`]): the grid's pump may stop and drop its future at any point
//! without breaking the connection's state, and when it stops asking the channel fills and the
//! server waits on the socket.
//!
//! **Batches.** The editor's unit is a batch (the text between `GO` lines): it may return any
//! number of result sets, each becoming a result of its own ([`run`]'s `next`). `GO n` runs its
//! batch n times. Informational messages (`PRINT`; tiberius is patched for them, see
//! `vendor/README.md`) go to the result set they follow as they arrive. tiberius reports a
//! batch's first error after its last result set, and no row counts: after each batch the
//! reading task asks for `@@ROWCOUNT`, `@@TRANCOUNT` and `XACT_STATE()` in one round trip
//! ([`Probe`]), which gives the count of a batch without rows and the session's transaction
//! state.
//!
//! **Cancel.** [`Canceller::cancel`] tells the reading task, which stops reading, sends an
//! attention and drains the response up to its acknowledgement: the statement stops at once and
//! the session (its temporary tables, an open transaction) stays. If the acknowledgement does not
//! come in time, or the connection cannot be drained, the connection is dropped instead: the
//! server rolls back and ends the session, and the editor's next statement opens a new one.

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

/// Rows read ahead of the reader of a result set (then the server waits on the socket).
const IN_FLIGHT: usize = 1024;
/// Connecting (TCP, TLS, login) gives up after this.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a cancelled request may take to acknowledge the attention before the connection
/// is dropped instead.
const CANCEL_WAIT: Duration = Duration::from_secs(5);

fn closed_error() -> MsError {
    MsError::Protocol("the connection is closed".into())
}

/// The connection itself failed (not a statement): it cannot be used again.
fn breaks_connection(e: &MsError) -> bool {
    !matches!(e, MsError::Server(_))
}

// ---------------------------------------------------------------------------------------------
// Connecting

/// `host\instance` → (`host`, `instance`).
pub(crate) fn split_instance(host: &str) -> (&str, Option<&str>) {
    match host.split_once('\\') {
        Some((h, i)) if !i.is_empty() => (h, Some(i)),
        _ => (host, None),
    }
}

/// The encryption levels to try for a profile's mode, in order. `disable` still encrypts the
/// login packet (SQL Server's `Encrypt=false`); `prefer` falls back to that when the server
/// does not encrypt; `require` encrypts without checking the certificate (`TrustServerCertificate`);
/// `verify-full` checks it against the system roots and the host name.
fn levels(mode: SslMode) -> &'static [EncryptionLevel] {
    match mode {
        SslMode::Disable => &[EncryptionLevel::Off],
        SslMode::Prefer => &[EncryptionLevel::On, EncryptionLevel::Off],
        SslMode::Require | SslMode::VerifyFull => &[EncryptionLevel::Required],
    }
}

/// The driver's options for a profile. Through a tunnel the TCP connection goes to the tunnel,
/// while TLS still names (and checks) the profile's host.
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
    // Editor statements run as long as they run; the cancel is the user's.
    c.command_timeout(None);
    c.handshake_timeout(Some(CONNECT_TIMEOUT));
    c
}

async fn dial(c: Config, via: Option<SocketAddr>, named: bool) -> Result<TClient> {
    let tcp = match via {
        Some(a) => TcpStream::connect(a).await?,
        // The instance's port, from the SQL Browser (UDP 1434).
        None if named => <TcpStream as SqlBrowser>::connect_named(&c).await?,
        None => TcpStream::connect(c.get_addr()).await?,
    };
    tcp.set_nodelay(true)?;
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(60));
    let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&ka);
    Ok(tiberius::Client::connect(c, tcp.compat_write()).await?)
}

/// The server answered that it does not encrypt (`prefer` then goes without).
fn declined_tls(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<MsError>(), Some(MsError::Protocol(m)) if m.contains("encryption level"))
}

/// A connection to the profile's server (through its tunnel, if any) on `database` or the
/// profile's own. `watch` (editor sessions) follows its transaction state.
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

// ---------------------------------------------------------------------------------------------
// The client

/// One SQL Server connection, shared by whoever holds the [`Db`](super::Db): requests take turns.
pub(crate) struct Client {
    conn: Arc<tokio::sync::Mutex<Option<TClient>>>,
    closed: AtomicBool,
    pub(crate) tls_active: bool,
    /// An editor session's transaction state.
    watch: Option<Arc<TxWatch>>,
    /// Cancel requests: a request started when this was `g` stops once it passes `g`.
    cancel: Arc<watch::Sender<u64>>,
    /// "This app" for the activity monitor, while the connection lives.
    _mark: crate::own::Mark,
}

/// What a batch left behind, asked right after it: the count of its last statement and the
/// session's transaction state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Probe {
    pub rowcount: u64,
    pub trancount: i32,
    /// `XACT_STATE()`: 1 a transaction that can commit, -1 one that can only roll back.
    pub xact_state: i16,
}

impl Probe {
    /// The transaction status byte of the PostgreSQL protocol (`I`, `T`, `E`).
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

    /// What cancels this connection's requests.
    pub(crate) fn canceller(&self) -> Canceller {
        Canceller(self.cancel.clone())
    }

    /// Stops the request running on this connection.
    pub(crate) async fn cancel(&self) -> Result<()> {
        self.canceller().cancel().await
    }

    /// The connection broke (or a cancel could not drain it): it is gone, and its transaction
    /// with it.
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

    /// After a request on the connection itself: a broken connection is dropped; an editor
    /// session's transaction state (or a count asked for) is read. Returns the probe.
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

    /// Runs `sql` (a batch) and returns what it reported: the row count of its first result set,
    /// else the rows its last statement affected.
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

    /// Every row of every result set of `sql`, as text.
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

    /// Starts a request: its result sets to read (a task reads them off the connection, which
    /// stays taken until the response ends). `count`: ask for the count of a batch without
    /// rows.
    async fn begin(self: &Arc<Self>, exec: Exec, count: bool) -> MsRows {
        let slot = self.conn.clone().lock_owned().await;
        let gen0 = *self.cancel.borrow();
        let (tx, rx) = mpsc::channel(IN_FLIGHT);
        tokio::spawn(self.clone().read(slot, exec, gen0, count, tx));
        MsRows::new(rx)
    }

    /// The reading task of [`Client::begin`].
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

/// Resolves once a cancel newer than `gen0` was asked for.
async fn cancel_asked(rx: &mut watch::Receiver<u64>, gen0: u64) {
    if rx.wait_for(|g| *g > gen0).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Sends an attention for the request the connection is reading (its response already let go)
/// and reads up to the acknowledgement; false when the connection could not be brought back in
/// step (it must be dropped).
///
/// SQL Server ends the cancelled request's response first and may acknowledge in a message of
/// its own after it (always so when the request had not sent anything yet, `WAITFOR`), while
/// tiberius' `cancel_query` stops reading at the end of the first message. Then the
/// acknowledgement is still on its way: a request that fails before anything is written (a
/// procedure name past TDS' 65535 characters) leaves the connection waiting for a response, so
/// the next request (the probe after every request) first reads that message and drops it.
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

/// How long a cancelled stream may go on to the end of the token it is reading (the connection
/// stays in step at a token's end) before it is let go where it is.
const TOKEN_END: Duration = Duration::from_millis(50);

/// Sends the request and hands its result sets over until the response ends (or a cancel).
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

/// What a reading task runs.
enum Exec {
    /// A batch, as the editor and Dataman's own statements send them.
    Batch(String),
    /// One statement with values for `@P1 … @Pn`.
    Params(String, Vec<Option<String>>),
}

/// What a reading task hands over.
enum Msg {
    /// A result set begins.
    Columns(Arc<[tiberius::Column]>),
    Row(tiberius::Row),
    /// An informational message (`PRINT`).
    Info(String),
    /// A statement failed (the batch may go on): where the server sent the error. The response
    /// still ends with [`Msg::Fail`] for the first error of the batch.
    StmtError(MsError),
    /// The response ended: the count of a batch without result sets (when asked), or a cancel.
    End { affected: Option<u64>, cancelled: bool },
    Fail(MsError),
}

/// What comes after a result set.
pub(crate) enum Next {
    /// A statement failed here, and the batch goes on (only when [`MsRows::positional_errors`]).
    Error(MsError),
    Columns(Arc<[tiberius::Column]>),
    End { affected: Option<u64>, cancelled: bool },
    Fail(MsError),
}

/// A request's result sets, decoded into the grid's chunks: [`MsRows::next_result`] moves to the
/// next one, and as a [`RowSource`] its rows run until the next result set or the end.
pub(crate) struct MsRows {
    rx: mpsc::Receiver<Msg>,
    dec: Vec<Dec>,
    /// What ended the current result set, kept for [`MsRows::next_result`].
    pending: Option<Next>,
    /// Where the server's messages go as they arrive (the editor's current result); dropped
    /// when `None`.
    messages: Option<Arc<ResultSet>>,
    /// Statement errors come as [`Next::Error`] where they happened; else only the batch's first
    /// comes, at the end ([`Next::Fail`]).
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

    /// The next result set's columns (its rows follow as a [`RowSource`]), or how the response
    /// ended. Rows of the current result set nobody read are dropped.
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

    /// Decodes the coming rows as `cols`: their column infos.
    pub(crate) fn columns(&mut self, cols: &[tiberius::Column]) -> Vec<ColumnInfo> {
        let (infos, dec) = decode::column_infos(cols);
        self.dec = dec;
        infos
    }

    /// The next row of the current result set; `None` once it ended.
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
    /// Never: a batch's error comes after its result sets ([`MsRows::next_result`]).
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

/// The first result set's columns, its rows to follow; a request without result sets has no
/// columns, and its end (with its count) stays for [`MsRows::next_result`].
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

/// Runs a batch for Dataman's own reads (exports, rows): its first result set's columns, and
/// its rows.
pub(crate) async fn start(client: &Arc<Client>, sql: &str) -> Result<(Vec<ColumnInfo>, MsRows), MsError> {
    first_result(client.begin(Exec::Batch(sql.to_string()), false).await).await
}

/// One statement with its parameters (the agents'): its first result set's columns and rows,
/// and the count of a statement without rows.
pub(crate) async fn query(client: &Arc<Client>, sql: &str, params: &[Option<String>]) -> Result<(Vec<ColumnInfo>, MsRows), MsError> {
    let exec = if params.is_empty() { Exec::Batch(sql.to_string()) } else { Exec::Params(sql.to_string(), params.to_vec()) };
    first_result(client.begin(exec, true).await).await
}

/// Tells a connection's reading task to stop its request (an attention).
#[derive(Clone)]
pub(crate) struct Canceller(Arc<watch::Sender<u64>>);

impl Canceller {
    pub(crate) async fn cancel(&self) -> Result<()> {
        self.0.send_modify(|g| *g += 1);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Server

/// `16.0.4305.5` → `2022` (the release of a major version).
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

/// `SQL Server 2022 16.0.4305.5`, `Azure SQL Database 12.0.2000.8` (`EngineEdition` 5),
/// `Azure SQL Managed Instance …` (8), `Azure SQL Edge …` (9).
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

/// The schema searched for unqualified names: the user's default schema.
pub(crate) async fn search_path(client: &Client) -> Vec<String> {
    match client.text_rows("SELECT SCHEMA_NAME()").await {
        Ok(rows) => rows.first().and_then(|r| r.first().cloned().flatten()).into_iter().collect(),
        Err(_) => vec!["dbo".into()],
    }
}

// ---------------------------------------------------------------------------------------------
// Errors

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

/// [`error_info`] of an error in the batch that starts at byte `batch` of `sql`: the server's
/// line (counted from the batch's start; not inside a procedure it called) becomes a position
/// in `sql`.
pub(crate) fn error_info_at(e: &MsError, sql: &str, batch: usize) -> ErrorInfo {
    let mut info = error_info(e);
    if let MsError::Server(t) = e
        && t.procedure().is_empty()
    {
        info.position = line_position(sql, batch, t.line(), near_token(t.message()));
    }
    info
}

/// The token a syntax error names: `Incorrect syntax near the keyword 'WHERE'.`, `… near ','.`
fn near_token(message: &str) -> Option<&str> {
    let start = message.find(" near the keyword '").map(|i| i + " near the keyword '".len()).or_else(|| message.find(" near '").map(|i| i + " near '".len()))?;
    let rest = &message[start..];
    Some(&rest[..rest.find('\'')?]).filter(|t| !t.is_empty())
}

/// The 1-based character position of line `line` (1-based) of the batch starting at byte
/// `batch` of `sql`: the `near` token on that line, else its first non-blank character.
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

// ---------------------------------------------------------------------------------------------
// Editor statements

/// Runs the editor's text: each `GO` batch (`n` times for `GO n`), every result set into a
/// result of its own. `rs` takes the first; `next(prev)` finishes `prev` and returns the result
/// that follows it. Values for `@P1 …` go with a text that is one statement. The error of a
/// batch stops the run (later batches do not run), with its line mapped to `rs.sql`.
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
    // `cur` holds a result set already (the next one gets a result of its own).
    let mut shown = false;
    let mut affected: Option<u64> = None;
    let mut sent = false;
    // A statement's error was shown where it happened (the batch's own error, at its end, is it).
    let mut reported = false;
    // The next result set after `prev`, which stays failed if it was (`next` finishes it as done).
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
            // A message belongs to the result set before it (the first one's to the first).
            rows.messages = Some(cur.clone());
            // The user stopped a result set: the rest of the batch is cancelled, not shown.
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
                        // Aborted without `stop_fetching` (which only drops the rest of its rows).
                        if cur.demand().borrow().abort && !cur.status().partial {
                            stopping = true;
                            let _ = client.cancel().await;
                        }
                    }
                    Next::Error(_) if stopping => {}
                    // Where the statement failed: its own result set (or a new one), failed, with the
                    // results of what follows after it.
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
                        // Failed once the run moves past it (or ends there): a reader that sees the
                        // phase takes the statement as over.
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
                    // The batch's first error, already shown in its place: the run ends failed
                    // when it was the last thing that happened, done when results followed.
                    Next::Fail(e) if reported => {
                        let info = error_info_at(&e, sql, b.range.start);
                        return if cur.status().error.is_some() { Ran::Failed(info) } else { Ran::Done(None) };
                    }
                    Next::Fail(e) => {
                        // A result of its own when the result set before it has rows to keep.
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

    /// The server's line, counted from its batch, lands on that line of the editor's text.
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
        // The token a syntax error names, on its line.
        let p = line_position(sql, second, 2, near_token("Incorrect syntax near the keyword 'nope'.")).unwrap() as usize;
        assert_eq!(&sql[p - 1..p + 3], "nope");
        assert_eq!(near_token("Incorrect syntax near ','."), Some(","));
        assert_eq!(near_token("Invalid object name 'x'."), None);
        // Characters, not bytes.
        let sql = "SELECT 'été'\nGO\nSELECT x";
        let b = go_batches(sql);
        assert_eq!(line_position(sql, b[1].range.start, 1, None), Some(sql.chars().count() as u32 - 7));
    }
}
