pub(crate) mod sniff;

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskCx, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures_util::StreamExt;
use parking_lot::Mutex;
use postgres_protocol::types as pt;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{AsyncMessage, CancelToken, Client, NoTls, Notification, SimpleQueryMessage, Statement};

use self::sniff::{Sniff, SniffTls};
use super::{Ran, RowSource, Rows, wait_for_demand, wants_more};
use crate::config::{ConnectionProfile, SslMode};
use crate::result::{ChunkBuilder, ColumnInfo, ColumnOrigin, ErrorInfo, ResultSet};
use crate::rows::TextRows;
use crate::server::{self, ServerExtension, ServerInfo};
use crate::tx::TxWatch;
use crate::value::{self, Kind, RawCell, Storage};

#[derive(Clone)]
pub(crate) enum Tls {
    None,
    Rustls(tokio_postgres_rustls::MakeRustlsConnect),
}

mod no_verify {
    use rustls::DigitallySignedStruct;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use std::sync::Arc;

    #[derive(Debug)]
    pub struct NoVerify(pub Arc<CryptoProvider>);

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            d: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            d: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}

fn tls_for(mode: SslMode) -> Result<Tls> {
    Ok(match rustls_config(mode)? {
        None => Tls::None,
        Some(cfg) => Tls::Rustls(tokio_postgres_rustls::MakeRustlsConnect::new(cfg)),
    })
}

pub(crate) fn rustls_config(mode: SslMode) -> Result<Option<rustls::ClientConfig>> {
    if mode == SslMode::Disable {
        return Ok(None);
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
    let cfg = if mode == SslMode::VerifyFull {
        let mut roots = rustls::RootCertStore::empty();
        let found = rustls_native_certs::load_native_certs();
        for c in found.certs {
            let _ = roots.add(c);
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(no_verify::NoVerify(provider)))
            .with_no_client_auth()
    };
    Ok(Some(cfg))
}

/// With `sslmode=prefer` tokio-postgres only calls the connector when the server accepts TLS,
/// and does not say so afterwards.
mod tracked {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio_postgres::Socket;
    use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
    use tokio_postgres_rustls::MakeRustlsConnect;

    type Inner = MakeRustlsConnect;
    type InnerConnect = <Inner as MakeTlsConnect<Socket>>::TlsConnect;
    type Stream = <Inner as MakeTlsConnect<Socket>>::Stream;

    pub struct Tracked {
        pub inner: Inner,
        pub used: Arc<AtomicBool>,
    }

    pub struct TrackedConnect {
        inner: InnerConnect,
        used: Arc<AtomicBool>,
    }

    impl MakeTlsConnect<Socket> for Tracked {
        type Stream = Stream;
        type TlsConnect = TrackedConnect;
        type Error = <Inner as MakeTlsConnect<Socket>>::Error;
        fn make_tls_connect(&mut self, domain: &str) -> Result<TrackedConnect, Self::Error> {
            Ok(TrackedConnect { inner: <Inner as MakeTlsConnect<Socket>>::make_tls_connect(&mut self.inner, domain)?, used: self.used.clone() })
        }
    }

    impl TlsConnect<Socket> for TrackedConnect {
        type Stream = Stream;
        type Error = <InnerConnect as TlsConnect<Socket>>::Error;
        type Future = Pin<Box<dyn Future<Output = Result<Stream, Self::Error>> + Send>>;
        fn connect(self, s: Socket) -> Self::Future {
            let used = self.used;
            let fut = <InnerConnect as TlsConnect<Socket>>::connect(self.inner, s);
            Box::pin(async move {
                let r = fut.await;
                if r.is_ok() {
                    used.store(true, Ordering::Release);
                }
                r
            })
        }
    }
}

pub(crate) struct Dedicated {
    pub client: Arc<Client>,
    pub tls: Tls,
    pub tls_active: bool,
    pub notifications: tokio::sync::mpsc::UnboundedReceiver<Notification>,
}

impl Dedicated {
    pub(crate) async fn cancel(&self) -> Result<()> {
        cancel(&self.client.cancel_token(), &self.tls).await
    }
}

/// Polls the connection's messages (instead of just awaiting it) so notifications reach `tx`.
fn drive<S, T>(
    conn: tokio_postgres::Connection<S, T>,
    tx: Option<tokio::sync::mpsc::UnboundedSender<Notification>>,
    watch: Option<Arc<TxWatch>>,
    mark: Arc<Mutex<Option<crate::own::Mark>>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut conn = conn;
        let mut msgs = futures_util::stream::poll_fn(move |cx| conn.poll_message(cx));
        while let Some(m) = msgs.next().await {
            match m {
                Ok(AsyncMessage::Notification(n)) => {
                    if let Some(tx) = &tx {
                        let _ = tx.send(n);
                    }
                }
                Ok(AsyncMessage::Notice(n)) => {
                    log::debug!("notice: {}", n.message());
                    if let Some(w) = &watch {
                        w.push_notice(format!("{}: {}", n.severity(), n.message()));
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log::debug!("connection closed: {e}");
                    break;
                }
            }
        }
        if let Some(w) = watch {
            w.closed();
        }
        drop(mark.lock().take());
    });
}

fn own_mark(p: &ConnectionProfile, slot: &Mutex<Option<crate::own::Mark>>, pid: Option<i32>) {
    if let Some(pid) = pid {
        *slot.lock() = Some(crate::own::mark(crate::own::server_key(p), pid));
    }
}

async fn backend_pid(client: &Client) -> Option<i32> {
    let rows = text_rows(client, "SELECT pg_backend_pid()").await.ok()?;
    rows.first()?.first()?.as_deref()?.trim().parse().ok()
}

/// A server that refuses it (or, like LAR, accepts it without enforcing it) still gets the
/// client-side refusal (`guard::read_only_refusal`).
async fn set_read_only(client: &Client) {
    if let Err(e) = client.simple_query("SET default_transaction_read_only = on").await {
        log::debug!("default_transaction_read_only: {e}");
    }
}

pub(crate) async fn connect_through(
    p: &ConnectionProfile,
    password: Option<&str>,
    database: Option<&str>,
    tunnel: Option<&crate::ssh::Tunnel>,
) -> Result<Dedicated> {
    let via = match tunnel {
        Some(t) => {
            t.ensure().await?;
            Some(t.local_addr())
        }
        None => None,
    };
    match connect_dedicated(p, password, database, via).await {
        Ok(d) => Ok(d),
        Err(e) => match tunnel.and_then(|t| t.take_error()) {
            Some(s) => Err(s.into()),
            None => Err(e),
        },
    }
}

/// Through a tunnel the profile's host still names the server for TLS (SNI, verify-full), and
/// the cancel token points at the tunnel too, so a CancelRequest takes the same way.
async fn connect_dedicated(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, via: Option<SocketAddr>) -> Result<Dedicated> {
    let (cfg, sslmode) = pg_config(p, password, database, via);
    let tls = tls_for(sslmode)?;
    let (tx, notifications) = tokio::sync::mpsc::unbounded_channel();
    let used = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mark = Arc::new(Mutex::new(None));
    let client = match &tls {
        Tls::None => {
            let (client, conn) = cfg.connect(NoTls).await?;
            drive(conn, Some(tx), None, mark.clone());
            client
        }
        Tls::Rustls(m) => {
            let t = tracked::Tracked { inner: m.clone(), used: used.clone() };
            let (client, conn) = cfg.connect(t).await?;
            drive(conn, Some(tx), None, mark.clone());
            client
        }
    };
    own_mark(p, &mark, backend_pid(&client).await);
    if p.read_only {
        set_read_only(&client).await;
    }
    let tls_active = used.load(std::sync::atomic::Ordering::Acquire);
    Ok(Dedicated { client: Arc::new(client), tls, tls_active, notifications })
}

fn pg_config(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, via: Option<SocketAddr>) -> (tokio_postgres::Config, SslMode) {
    let mut cfg = tokio_postgres::Config::new();
    let sslmode = if p.is_socket() { SslMode::Disable } else { p.sslmode };
    match via {
        Some(addr) => {
            cfg.host(if p.is_socket() { "localhost" } else { &p.host }).hostaddr(addr.ip()).port(addr.port());
        }
        None => {
            cfg.host(&p.host).port(p.port);
        }
    }
    cfg.user(&p.user)
        .dbname(database.unwrap_or(&p.database))
        .application_name("Dataman")
        .connect_timeout(Duration::from_secs(10))
        .ssl_mode(match sslmode {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        });
    if let Some(pw) = password {
        cfg.password(pw);
    }
    (cfg, sslmode)
}

pub(crate) enum RawStream {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl tokio::io::AsyncRead for RawStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut TaskCx<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            RawStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            RawStream::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for RawStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut TaskCx<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            RawStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            RawStream::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            RawStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            RawStream::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            RawStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            RawStream::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Reach {
    host: String,
    port: u16,
    socket: bool,
    via: Option<SocketAddr>,
}

impl Reach {
    fn of(p: &ConnectionProfile, via: Option<SocketAddr>) -> Reach {
        Reach { host: p.host.clone(), port: p.port, socket: p.is_socket(), via }
    }

    fn domain(&self) -> &str {
        if self.socket { "localhost" } else { &self.host }
    }

    async fn open(&self) -> std::io::Result<RawStream> {
        let connect = async {
            if let Some(a) = self.via {
                return Ok(RawStream::Tcp(tokio::net::TcpStream::connect(a).await?));
            }
            #[cfg(unix)]
            if self.socket {
                let path = format!("{}/.s.PGSQL.{}", self.host.trim_end_matches('/'), self.port);
                return Ok(RawStream::Unix(tokio::net::UnixStream::connect(path).await?));
            }
            Ok::<_, std::io::Error>(RawStream::Tcp(tokio::net::TcpStream::connect((self.host.as_str(), self.port)).await?))
        };
        let s = tokio::time::timeout(Duration::from_secs(10), connect)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out"))??;
        if let RawStream::Tcp(t) = &s {
            super::refuse_self_connect(t)?;
            let _ = t.set_nodelay(true);
            let _ = socket2::SockRef::from(t).set_keepalive(true);
        }
        Ok(s)
    }
}

#[derive(Clone)]
pub(crate) struct Canceller {
    token: CancelToken,
    tls: Tls,
    reach: Reach,
}

impl Canceller {
    pub(crate) async fn cancel(&self) -> Result<()> {
        let s = self.reach.open().await?;
        match &self.tls {
            Tls::None => self.token.cancel_query_raw(s, NoTls).await?,
            Tls::Rustls(m) => {
                let mut m = m.clone();
                let c = <tokio_postgres_rustls::MakeRustlsConnect as MakeTlsConnect<RawStream>>::make_tls_connect(&mut m, self.reach.domain())?;
                self.token.cancel_query_raw(s, c).await?
            }
        }
        Ok(())
    }
}

async fn connect_session(
    p: &ConnectionProfile,
    password: Option<&str>,
    database: Option<&str>,
    via: Option<SocketAddr>,
    watch: &Arc<TxWatch>,
) -> Result<(Client, Canceller)> {
    let (cfg, sslmode) = pg_config(p, password, database, via);
    let reach = Reach::of(p, via);
    let socket = reach.open().await.context("error connecting to server")?;
    let stream = Sniff::new(socket, watch.clone());
    let tls = tls_for(sslmode)?;
    let mark = Arc::new(Mutex::new(None));
    let client = match &tls {
        Tls::None => {
            let (client, conn) = cfg.connect_raw(stream, NoTls).await?;
            drive(conn, None, Some(watch.clone()), mark.clone());
            client
        }
        Tls::Rustls(m) => {
            let mut m = m.clone();
            let inner = <tokio_postgres_rustls::MakeRustlsConnect as MakeTlsConnect<Sniff<RawStream>>>::make_tls_connect(&mut m, reach.domain())?;
            let t = SniffTls { inner, watch: watch.clone(), used: Arc::new(std::sync::atomic::AtomicBool::new(false)) };
            let (client, conn) = cfg.connect_raw(stream, t).await?;
            drive(conn, None, Some(watch.clone()), mark.clone());
            client
        }
    };
    own_mark(p, &mark, watch.pid());
    if p.read_only {
        set_read_only(&client).await;
    }
    let cancel = Canceller { token: client.cancel_token(), tls, reach };
    Ok((client, cancel))
}

pub(crate) async fn connect_session_through(
    p: &ConnectionProfile,
    password: Option<&str>,
    database: Option<&str>,
    tunnel: Option<&crate::ssh::Tunnel>,
    watch: &Arc<TxWatch>,
) -> Result<(Client, Canceller)> {
    let via = match tunnel {
        Some(t) => {
            t.ensure().await?;
            Some(t.local_addr())
        }
        None => None,
    };
    match connect_session(p, password, database, via, watch).await {
        Ok(c) => Ok(c),
        Err(e) => match tunnel.and_then(|t| t.take_error()) {
            Some(s) => Err(s.into()),
            None => Err(e),
        },
    }
}

pub(crate) async fn cancel(token: &CancelToken, tls: &Tls) -> Result<()> {
    match tls {
        Tls::None => token.cancel_query(NoTls).await?,
        Tls::Rustls(m) => token.cancel_query(m.clone()).await?,
    }
    Ok(())
}

pub(crate) async fn text_rows(client: &Client, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let msgs = client.simple_query(sql).await?;
    Ok(msgs
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect()),
            _ => None,
        })
        .collect())
}

pub(crate) async fn text_query(client: &Client, sql: &str, max: Option<usize>) -> Result<(TextRows, bool)> {
    let stream = client.simple_query_raw(sql).await?;
    futures_util::pin_mut!(stream);
    let mut out = TextRows::default();
    let mut first = true;
    let mut truncated = false;
    while let Some(m) = stream.as_mut().next().await {
        match m? {
            SimpleQueryMessage::RowDescription(cols) if first => {
                out.columns = cols.iter().map(|c| c.name().to_string()).collect();
            }
            SimpleQueryMessage::Row(r) if first => {
                if max.is_some_and(|m| out.rows.len() >= m) {
                    truncated = true;
                    continue;
                }
                if out.columns.is_empty() {
                    out.columns = r.columns().iter().map(|c| c.name().to_string()).collect();
                }
                out.rows.push((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect());
            }
            SimpleQueryMessage::CommandComplete(_) => first = false,
            _ => {}
        }
    }
    Ok((out, truncated))
}

pub(crate) async fn execute(client: &Client, sql: &str) -> Result<Option<u64>, tokio_postgres::Error> {
    let msgs = client.simple_query(sql).await?;
    Ok(msgs.iter().find_map(|m| match m {
        SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    }))
}

pub(crate) async fn start(client: &Client, stmt: &Statement, columns: &[ColumnInfo], sql: &str) -> Result<Rows, tokio_postgres::Error> {
    if needs_text(stmt) {
        let stream = client.simple_query_raw(sql).await?;
        return Ok(Rows::Pg(PgRows::text(stream, columns)));
    }
    let stream = client.query_raw(stmt, std::iter::empty::<&(dyn ToSql + Sync)>()).await?;
    Ok(Rows::Pg(PgRows::new(stream, stmt, columns)))
}

const BINARY_DECODED: &[&str] = &["vector", "halfvec", "sparsevec", "hstore", "ltree", "lquery", "ltxtquery", "citext"];

const REG_TYPES: &[&str] = &[
    "regclass", "regproc", "regprocedure", "regoper", "regoperator", "regtype", "regnamespace", "regrole", "regconfig", "regdictionary",
    "regcollation",
];

/// `aclitem` has no binary output function (the server fails the whole query), and an
/// extension type the grid has no decoder for would show as hex.
fn sendless(ty: &Type) -> bool {
    use tokio_postgres::types::Kind as K;
    match ty.kind() {
        K::Array(e) | K::Range(e) | K::Multirange(e) | K::Domain(e) => sendless(e),
        K::Composite(fields) => fields.iter().any(|f| sendless(f.type_())),
        K::Enum(_) | K::Pseudo => false,
        // OIDs from 16384 up are created by users and extensions.
        K::Simple => ty.name() == "aclitem" || REG_TYPES.contains(&ty.name()) || (ty.oid() >= 16384 && !BINARY_DECODED.contains(&ty.name())),
        _ => false,
    }
}

/// The server read more than one statement where the splitter saw one: the text protocol would
/// run them all, unseen by the agents' checks.
pub(crate) fn several_commands(e: &tokio_postgres::Error) -> bool {
    e.as_db_error().is_some_and(|d| d.code() == &tokio_postgres::error::SqlState::SYNTAX_ERROR && d.message().contains("multiple commands"))
}

pub(crate) fn needs_text(stmt: &Statement) -> bool {
    stmt.columns().iter().any(|c| sendless(c.type_()))
}

pub(crate) fn column_infos(stmt: &Statement) -> Vec<ColumnInfo> {
    let text = needs_text(stmt);
    stmt.columns()
        .iter()
        .map(|c| {
            let mut kind = Kind::of(c.type_());
            if text && matches!(kind, Kind::Date | Kind::Time | Kind::Timestamp | Kind::TimestampTz) {
                kind = Kind::Other;
            }
            ColumnInfo { name: c.name().to_string(), type_name: c.type_().name().to_string(), type_oid: c.type_().oid(), kind }
        })
        .collect()
}

async fn first_ok(client: &Client, queries: &[&str]) -> Result<Vec<Vec<Option<String>>>> {
    let mut last = None;
    for q in queries {
        match text_rows(client, q).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                log::debug!("catalog query failed ({e}): {q}");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no query")))
}

pub(crate) async fn server_info(client: &Client, exts: &[Arc<dyn ServerExtension>]) -> Result<ServerInfo> {
    let row = first_ok(client, &["SELECT version(), current_database(), current_user", "SELECT version()"])
        .await?
        .into_iter()
        .next()
        .context("version() returned no row")?;
    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let mut info = ServerInfo { version: get(0), database: get(1), user: get(2), ..Default::default() };
    info.is_lar = server::detect_lar(&info.version);
    info.search_path = search_path(client, &info.user).await;
    info.extensions = exts.iter().filter(|e| e.detect(&info)).map(|e| e.id()).collect();
    Ok(info)
}

pub(crate) async fn search_path(client: &Client, user: &str) -> Vec<String> {
    match first_ok(client, &["SELECT pg_catalog.current_schemas(false)", "SHOW search_path"]).await {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .map(|s| server::parse_search_path(&s, user))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec!["public".into()]),
        Err(_) => vec!["public".into()],
    }
}

pub(crate) fn error_info(e: &tokio_postgres::Error) -> ErrorInfo {
    if let Some(db) = e.as_db_error() {
        ErrorInfo {
            code: Some(db.code().code().to_string()),
            message: db.message().to_string(),
            detail: db.detail().map(str::to_string),
            hint: db.hint().map(str::to_string),
            position: match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
                _ => None,
            },
            severity: Some(db.severity().to_string()),
        }
    } else {
        let mut message = e.to_string();
        let mut src = std::error::Error::source(e);
        while let Some(s) = src {
            message.push_str(": ");
            message.push_str(&s.to_string());
            src = s.source();
        }
        ErrorInfo { message, ..Default::default() }
    }
}

pub(crate) async fn run(
    client: &Client,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
    aborted: &(dyn Fn() -> bool + Sync),
) -> Ran {
    let executing = || rs.executing.store(true, std::sync::atomic::Ordering::Release);
    // COPY over the client's own channel: the extended protocol cannot carry it (the server's
    // reply kills the connection), so it never gets there.
    match copy_channel(&rs.sql) {
        Some(Copy::Out) if !aborted() => {
            executing();
            return match fetch_copy_out(client, rs, started, on_pause).await {
                Ok(n) => Ran::Done(n),
                Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED) => Ran::Cancelled,
                Err(e) => Ran::Failed(error_info(&e)),
            };
        }
        Some(Copy::Out) => return Ran::NeverRan { settle: false },
        Some(Copy::In) => {
            return Ran::Failed(ErrorInfo {
                message: "COPY … FROM STDIN needs data the editor cannot send".into(),
                hint: Some("Use the table's Import, or INSERT; COPY … FROM 'file' reads a file on the server".into()),
                ..Default::default()
            });
        }
        Some(Copy::Binary) => {
            return Ran::Failed(ErrorInfo {
                message: "COPY … STDOUT in binary format cannot be shown".into(),
                hint: Some("Run a SELECT and export its result".into()),
                ..Default::default()
            });
        }
        None => {}
    }
    let r = match client.prepare(&rs.sql).await {
        Ok(_) if aborted() => return Ran::NeverRan { settle: false },
        Ok(stmt) => {
            executing();
            fetch_extended(client, &stmt, rs, started, on_pause).await
        }
        Err(e) if aborted() || e.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED) => return Ran::NeverRan { settle: true },
        // With parameters there is no such fallback: values are never spliced into SQL.
        Err(e) if e.as_db_error().is_some() && rs.params().is_empty() && !(rs.agent().is_some() && several_commands(&e)) => {
            rs.update_status(|s| s.text_mode = true);
            executing();
            match fetch_simple(client, rs, started, on_pause).await {
                Err(e2) if e2.code() == Some(&tokio_postgres::error::SqlState::IN_FAILED_SQL_TRANSACTION) => {
                    rs.update_status(|s| s.text_mode = false);
                    Err(e)
                }
                r => r,
            }
        }
        Err(e) => Err(e),
    };
    match r {
        Ok(affected) => Ran::Done(affected),
        Err(e) => {
            let info = error_info(&e);
            if info.code.as_deref() == Some("57014") { Ran::Cancelled } else { Ran::Failed(info) }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Copy {
    Out,
    In,
    Binary,
}

fn copy_channel(sql: &str) -> Option<Copy> {
    let toks: Vec<_> = crate::sql::tokenize(sql).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let word = |i: usize, w: &str| toks.get(i).is_some_and(|t| matches!(t.kind, crate::sql::TokenKind::Keyword | crate::sql::TokenKind::Ident) && t.text(sql).eq_ignore_ascii_case(w));
    if !word(0, "COPY") {
        return None;
    }
    let end = toks.iter().position(|t| t.text(sql) == ";").unwrap_or(toks.len());
    let stdout = (1..end).any(|i| word(i, "STDOUT"));
    let stdin = (1..end).any(|i| word(i, "STDIN"));
    if stdout && (1..end).any(|i| word(i, "BINARY")) {
        Some(Copy::Binary)
    } else if stdout {
        Some(Copy::Out)
    } else if stdin {
        Some(Copy::In)
    } else {
        None
    }
}

async fn fetch_copy_out(
    client: &Client,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
) -> Result<Option<u64>, tokio_postgres::Error> {
    let cols = vec![ColumnInfo { name: "copy".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text }];
    rs.set_columns(cols.clone());
    let stream = client.copy_out(rs.sql.as_str()).await?;
    let mut rows = CopyRows { stream: Box::pin(stream), partial: Vec::new(), lines: std::collections::VecDeque::new(), ended: false };
    super::pump(&mut rows, rs, &cols, started, on_pause).await
}

struct CopyRows {
    stream: Pin<Box<tokio_postgres::CopyOutStream>>,
    partial: Vec<u8>,
    lines: std::collections::VecDeque<String>,
    ended: bool,
}

impl CopyRows {
    async fn line(&mut self) -> Result<Option<String>, tokio_postgres::Error> {
        loop {
            if let Some(l) = self.lines.pop_front() {
                return Ok(Some(l));
            }
            if self.ended {
                return Ok(None);
            }
            match self.stream.as_mut().next().await {
                Some(chunk) => {
                    for &b in chunk?.iter() {
                        if b == b'\n' {
                            let mut line = std::mem::take(&mut self.partial);
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                            self.lines.push_back(String::from_utf8_lossy(&line).into_owned());
                        } else {
                            self.partial.push(b);
                        }
                    }
                }
                None => {
                    self.ended = true;
                    if !self.partial.is_empty() {
                        let line = std::mem::take(&mut self.partial);
                        self.lines.push_back(String::from_utf8_lossy(&line).into_owned());
                    }
                }
            }
        }
    }
}

impl RowSource for CopyRows {
    type Error = tokio_postgres::Error;

    async fn next_into(&mut self, b: &mut ChunkBuilder) -> Result<bool, tokio_postgres::Error> {
        match self.line().await? {
            Some(l) => {
                b.push_text(0, &l);
                b.end_row();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn skip(&mut self) -> Result<bool, tokio_postgres::Error> {
        Ok(self.line().await?.is_some())
    }

    fn affected(&self) -> Option<u64> {
        None
    }
}

async fn fetch_extended(
    client: &Client,
    stmt: &Statement,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
) -> Result<Option<u64>, tokio_postgres::Error> {
    rs.set_origins(
        stmt.columns()
            .iter()
            .map(|c| ColumnOrigin { table_oid: c.table_oid().filter(|o| *o != 0), column_id: c.column_id().filter(|n| *n > 0) })
            .collect(),
    );
    rs.set_columns(column_infos(stmt));
    let cols = rs.columns().unwrap_or_default();
    let params: Vec<TextParam> = rs.params().iter().map(|p| TextParam(p.as_deref())).collect();
    // Values of types without a binary form come as text (never with parameters: they cannot be
    // bound through the simple protocol).
    let mut rows = if params.is_empty() && needs_text(stmt) {
        rs.update_status(|s| s.text_mode = true);
        PgRows::text(client.simple_query_raw(&rs.sql).await?, &cols)
    } else {
        let stream = client.query_raw(stmt, params.iter().map(|p| p as &(dyn ToSql + Sync))).await?;
        PgRows::new(stream, stmt, &cols)
    };
    super::pump(&mut rows, rs, &cols, started, on_pause).await
}

pub(crate) struct PgRows {
    src: PgSource,
    types: Vec<Type>,
    storage: Vec<Storage>,
}

enum PgSource {
    Binary(Pin<Box<tokio_postgres::RowStream>>),
    Text { stream: Pin<Box<tokio_postgres::SimpleQueryStream>>, affected: Option<u64> },
}

impl PgRows {
    pub(crate) fn new(stream: tokio_postgres::RowStream, stmt: &Statement, cols: &[ColumnInfo]) -> PgRows {
        PgRows {
            src: PgSource::Binary(Box::pin(stream)),
            types: stmt.columns().iter().map(|c| c.type_().clone()).collect(),
            storage: cols.iter().map(|c| c.kind.storage()).collect(),
        }
    }

    pub(crate) fn text(stream: tokio_postgres::SimpleQueryStream, cols: &[ColumnInfo]) -> PgRows {
        PgRows { src: PgSource::Text { stream: Box::pin(stream), affected: None }, types: Vec::new(), storage: cols.iter().map(|c| c.kind.storage()).collect() }
    }
}

fn push_text_row(b: &mut ChunkBuilder, row: &tokio_postgres::SimpleQueryRow, storage: &[Storage]) {
    if row.len() != storage.len() {
        return;
    }
    for (i, st) in storage.iter().enumerate() {
        match row.get(i) {
            None => b.push_null(i),
            Some(v) => match st {
                Storage::Text => b.push_text(i, v),
                Storage::I64 => match v.parse::<i64>() {
                    Ok(n) => b.push_i64(i, n),
                    Err(_) => b.push_null(i),
                },
                Storage::F64 => match v.parse::<f64>() {
                    Ok(n) => b.push_f64(i, n),
                    Err(_) => b.push_null(i),
                },
                Storage::Bool => b.push_bool(i, matches!(v, "t" | "true")),
            },
        }
    }
    b.end_row();
}

impl RowSource for PgRows {
    type Error = tokio_postgres::Error;

    async fn next_into(&mut self, b: &mut ChunkBuilder) -> Result<bool, tokio_postgres::Error> {
        match &mut self.src {
            PgSource::Binary(stream) => match stream.as_mut().next().await {
                Some(Ok(row)) => {
                    push_row(b, &row, &self.types, &self.storage);
                    Ok(true)
                }
                Some(Err(e)) => Err(e),
                None => Ok(false),
            },
            PgSource::Text { stream, affected } => loop {
                match stream.as_mut().next().await {
                    Some(Ok(SimpleQueryMessage::Row(row))) => {
                        push_text_row(b, &row, &self.storage);
                        return Ok(true);
                    }
                    Some(Ok(SimpleQueryMessage::CommandComplete(n))) => *affected = Some(n),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e),
                    None => return Ok(false),
                }
            },
        }
    }

    async fn skip(&mut self) -> Result<bool, tokio_postgres::Error> {
        match &mut self.src {
            PgSource::Binary(stream) => match stream.as_mut().next().await {
                Some(Ok(_)) => Ok(true),
                Some(Err(e)) => Err(e),
                None => Ok(false),
            },
            PgSource::Text { stream, affected } => loop {
                match stream.as_mut().next().await {
                    Some(Ok(SimpleQueryMessage::Row(_))) => return Ok(true),
                    Some(Ok(SimpleQueryMessage::CommandComplete(n))) => *affected = Some(n),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e),
                    None => return Ok(false),
                }
            },
        }
    }

    fn affected(&self) -> Option<u64> {
        match &self.src {
            PgSource::Binary(stream) => stream.rows_affected(),
            PgSource::Text { affected, .. } => *affected,
        }
    }
}

#[derive(Debug)]
pub(crate) struct TextParam<'a>(pub(crate) Option<&'a str>);

impl ToSql for TextParam<'_> {
    fn to_sql(&self, _: &Type, out: &mut bytes::BytesMut) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self.0 {
            None => Ok(tokio_postgres::types::IsNull::Yes),
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(tokio_postgres::types::IsNull::No)
            }
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, _: &Type) -> tokio_postgres::types::Format {
        tokio_postgres::types::Format::Text
    }

    tokio_postgres::types::to_sql_checked!();
}

pub(crate) fn push_row(b: &mut ChunkBuilder, row: &tokio_postgres::Row, types: &[Type], storage: &[Storage]) {
    for (i, ty) in types.iter().enumerate() {
        let raw = match row.try_get::<_, RawCell>(i) {
            Ok(RawCell(Some(raw))) => raw,
            _ => {
                b.push_null(i);
                continue;
            }
        };
        match storage[i] {
            Storage::I64 => match value::decode_i64(ty, raw) {
                Ok(v) => b.push_i64(i, v),
                Err(_) => b.push_null(i),
            },
            Storage::F64 => match value::decode_f64(ty, raw) {
                Ok(v) => b.push_f64(i, v),
                Err(_) => b.push_null(i),
            },
            Storage::Bool => match pt::bool_from_sql(raw) {
                Ok(v) => b.push_bool(i, v),
                Err(_) => b.push_null(i),
            },
            Storage::Text => b.push_text_with(i, |s| value::format_binary(ty, raw, s)),
        }
    }
    b.end_row();
}

async fn fetch_simple(
    client: &Client,
    rs: &Arc<ResultSet>,
    started: Instant,
    on_pause: &(dyn Fn() + Sync),
) -> Result<Option<u64>, tokio_postgres::Error> {
    let stream = client.simple_query_raw(&rs.sql).await?;
    futures_util::pin_mut!(stream);
    let mut b: Option<ChunkBuilder> = None;
    let mut ncols = 0;
    let mut affected = None;
    let mut first_done = false;
    let mut demand = rs.demand();
    let mut draining = false;
    let mut first = true;
    let mut last_flush = Instant::now();
    while let Some(msg) = stream.as_mut().next().await {
        match msg? {
            SimpleQueryMessage::RowDescription(cols) => {
                if b.is_none() && !first_done {
                    let info: Vec<ColumnInfo> = cols
                        .iter()
                        .map(|c| ColumnInfo { name: c.name().to_string(), type_name: "text".into(), type_oid: 25, kind: Kind::Text })
                        .collect();
                    ncols = info.len();
                    b = Some(ChunkBuilder::new(&info));
                    rs.set_columns(info);
                }
            }
            SimpleQueryMessage::Row(row) => {
                let Some(b) = b.as_mut() else { continue };
                if draining || first_done || row.len() != ncols {
                    continue;
                }
                if !wants_more(&demand, rs.rows() + b.len()) {
                    rs.push_chunk(b.finish());
                    last_flush = Instant::now();
                    if !wait_for_demand(rs, &mut demand, rs.rows(), on_pause).await {
                        draining = true;
                        continue;
                    }
                }
                if first {
                    first = false;
                    let t = started.elapsed();
                    rs.update_status(|s| s.first_row = Some(t));
                }
                for i in 0..ncols {
                    match row.get(i) {
                        Some(v) => b.push_text(i, v),
                        None => b.push_null(i),
                    }
                }
                b.end_row();
                if b.len() >= crate::result::CHUNK || last_flush.elapsed() >= super::FLUSH_EVERY {
                    rs.push_chunk(b.finish());
                    last_flush = Instant::now();
                }
            }
            SimpleQueryMessage::CommandComplete(n) => {
                if first_done {
                    continue;
                }
                first_done = true;
                match b.as_mut() {
                    Some(b) => rs.push_chunk(b.finish()),
                    None => affected = Some(n),
                }
            }
            _ => {}
        }
    }
    if let Some(b) = b.as_mut() {
        rs.push_chunk(b.finish());
    }
    if rs.columns().is_none() {
        rs.set_columns(vec![]);
    }
    Ok(affected)
}
