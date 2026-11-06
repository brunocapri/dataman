//! ClickHouse through its HTTP interface, in `TabSeparatedWithNamesAndTypes`: one statement per
//! request, nothing to hold open between them. What the protocol does was probed on 25.8:
//!
//! - A failure after the first rows has status 200 already: the body ends with `__exception__`,
//!   a raw `\r` and the error. A cell's `\r` is always escaped, so no row can read as that.
//!   Formats that can say it themselves (`FORMAT JSON`) end cleanly instead, unless told not to.
//! - The server closes a result nobody reads for `http_send_timeout` (30 s, the server's own: a
//!   URL parameter does not change it), so a reader drains the response into [`IN_FLIGHT_BYTES`]
//!   while the grid is paused: what fits there finishes however long the user looks at it.
//! - A session takes one statement at a time (`SESSION_IS_LOCKED`): `KILL QUERY … SYNC` frees it.

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use parking_lot::Mutex;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::{Ran, RowSource};
use crate::config::{ConnectionProfile, Engine, SslMode};
use crate::result::{ChunkBuilder, ColumnInfo, ErrorInfo, ResultSet};
use crate::server::{Flavor, ServerInfo};
use crate::ssh::Tunnel;
use crate::value::{Kind, date_cell, timestamp_cell};

const FORMAT: &str = "TabSeparatedWithNamesAndTypes";
const IN_FLIGHT_BYTES: usize = 64 * 1024 * 1024;
/// A statement that chose its own format is shown by lines; a binary one may have none.
const RAW_LINE_CAP: usize = 1024 * 1024;
const EXCEPTION_MARK: &[u8] = b"__exception__";
/// The server's own limit for a session (`max_session_timeout`, 3600 by default).
const SESSION_SECONDS: &str = "3600";
const CODE_SESSION_LOCKED: &str = "373";
const CODES_CANCELLED: [&str; 2] = ["394", "735"];

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum ChError {
    #[error("{message}")]
    Server { code: String, message: String, position: Option<u32> },
    /// The request never got an answer: no statement ran, as far as this side knows.
    #[error("{0}")]
    Transport(String),
    /// The result stopped before its end, without the server saying why.
    #[error("ClickHouse ended the result early ({0}): it drops a result nobody reads for 30 s. Run it again, or narrow it with LIMIT")]
    Cut(String),
    #[error("{0}")]
    Params(String),
}

/// `Code: 62. DB::Exception: Syntax error: failed at position 1 (SELEC): …. (SYNTAX_ERROR) (version …)`
fn server_error(header_code: Option<&str>, body: &[u8]) -> ChError {
    let text = String::from_utf8_lossy(body);
    let at = text.find("Code: ").unwrap_or(0);
    let text = text[at..].trim();
    let parsed = text.strip_prefix("Code: ").and_then(|t| t.split_once('.'));
    let code = header_code.map(str::to_string).or_else(|| parsed.map(|(c, _)| c.to_string())).unwrap_or_default();
    let body = parsed.map_or(text, |(_, m)| m.trim_start());
    let body = body.strip_prefix("DB::Exception:").map_or(body, str::trim_start);
    let message = body.rfind(" (version ").map_or(body, |i| &body[..i]).trim_end();
    let position = message
        .split_once("failed at position ")
        .and_then(|(_, r)| r.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok());
    ChError::Server { code, message: message.to_string(), position }
}

fn transport(base: &str, e: &reqwest::Error) -> ChError {
    let mut message = format!("cannot reach {base}: {e}");
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        let m = s.to_string();
        if !message.contains(&m) {
            message.push_str(": ");
            message.push_str(&m);
        }
        src = s.source();
    }
    ChError::Transport(message)
}

pub(crate) fn error_info(e: &ChError) -> ErrorInfo {
    match e {
        ChError::Server { code, message, .. } => ErrorInfo { code: Some(code.clone()), message: message.clone(), severity: Some("ERROR".into()), ..Default::default() },
        e => ErrorInfo { message: e.to_string(), ..Default::default() },
    }
}

/// The server counts a syntax error's position in bytes from 1, the editor in characters.
pub(crate) fn error_info_at(e: &ChError, sql: &str) -> ErrorInfo {
    let mut info = error_info(e);
    if let ChError::Server { position: Some(at), .. } = e {
        let bytes = (*at as usize).saturating_sub(1).min(sql.len());
        info.position = Some(sql.bytes().take(bytes).filter(|b| b & 0xc0 != 0x80).count() as u32 + 1);
    }
    info
}

fn interrupted(e: &ChError) -> bool {
    matches!(e, ChError::Server { code, .. } if CODES_CANCELLED.contains(&code.as_str()))
}

pub(crate) struct Client {
    http: reqwest::Client,
    base: String,
    auth: HeaderValue,
    /// Named on every request, `USE` follows it: a session idle past its timeout is gone with what
    /// it had `USE`d, and the next statement must not run on another database for that.
    database: Mutex<Option<String>>,
    session: Option<String>,
    read_only: bool,
    /// The login is read-only already: it may set nothing, not even for one request.
    locked: bool,
    current: Mutex<Option<String>>,
    pub(crate) tls_active: bool,
}

pub(crate) type Canceller = Arc<Client>;

fn builder(mode: SslMode) -> reqwest::ClientBuilder {
    let b = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).tcp_keepalive(Duration::from_secs(60)).tcp_nodelay(true).user_agent("Dataman");
    if matches!(mode, SslMode::Prefer | SslMode::Require) { b.danger_accept_invalid_certs(true) } else { b }
}

/// Through a tunnel the name is still the server's, for its certificate; the port in the URL is
/// the tunnel's, because a resolved name takes the URL's port.
fn base_url(p: &ConnectionProfile, tls: bool, via: Option<std::net::SocketAddr>, mut b: reqwest::ClientBuilder) -> (String, reqwest::ClientBuilder) {
    let scheme = if tls { "https" } else { "http" };
    let host = if p.host.contains(':') && !p.host.starts_with('[') { format!("[{}]", p.host) } else { p.host.clone() };
    let base = match via {
        Some(a) if p.host.parse::<std::net::IpAddr>().is_err() => {
            b = b.resolve(&p.host, a);
            format!("{scheme}://{host}:{}", a.port())
        }
        Some(a) => format!("{scheme}://127.0.0.1:{}", a.port()),
        None => format!("{scheme}://{host}:{}", p.port),
    };
    (base, b)
}

/// `session`: a tab's connection, whose `SET` and `USE` last from one statement to the next.
pub(crate) async fn connect(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, tunnel: Option<&Tunnel>, session: bool) -> Result<Arc<Client>> {
    super::install_tls_provider();
    let via = match tunnel {
        Some(t) => {
            t.ensure().await?;
            Some(t.local_addr())
        }
        None => None,
    };
    let mode = p.tls_mode();
    let opened = match mode {
        SslMode::Disable => open(p, password, database, via, session, false).await,
        SslMode::Require | SslMode::VerifyFull => open(p, password, database, via, session, true).await,
        // No answer in TLS is what a plain port gives: an answer of any kind ends the search.
        SslMode::Prefer => match open(p, password, database, via, session, true).await {
            Err(ChError::Transport(_)) => open(p, password, database, via, session, false).await,
            r => r,
        },
    };
    match opened {
        Ok(c) => Ok(c),
        Err(e) => Err(match tunnel.and_then(|t| t.take_error()) {
            Some(s) => s.into(),
            None => e.into(),
        }),
    }
}

async fn open(p: &ConnectionProfile, password: Option<&str>, database: Option<&str>, via: Option<std::net::SocketAddr>, session: bool, tls: bool) -> Result<Arc<Client>, ChError> {
    let mode = if tls { p.tls_mode() } else { SslMode::Disable };
    let (base, b) = base_url(p, tls, via, builder(mode));
    let http = b.build().map_err(|e| transport(&base, &e))?;
    let raw = format!("{}:{}", p.user, password.unwrap_or(""));
    let mut auth = HeaderValue::from_str(&format!("Basic {}", data_encoding::BASE64.encode(raw.as_bytes()))).map_err(|e| ChError::Transport(e.to_string()))?;
    auth.set_sensitive(true);
    let database = database.filter(|d| !d.is_empty()).or(Some(p.database.as_str()).filter(|d| !d.is_empty())).map(str::to_string);
    let mut c = Client { http, base, auth, database: Mutex::new(database), session: None, read_only: false, locked: true, current: Mutex::new(None), tls_active: tls };
    // A login that is read-only already refuses to have the setting named: it is asked first.
    let locked = c.text_rows("SELECT getSetting('readonly')").await?.first().and_then(|r| r.first().cloned().flatten()).is_some_and(|v| v != "0");
    c.locked = locked;
    c.read_only = p.read_only && !locked;
    if session {
        c.session = Some(uuid::Uuid::new_v4().simple().to_string());
    }
    Ok(Arc::new(c))
}

impl Client {
    pub(crate) fn is_closed(&self) -> bool {
        false
    }

    fn request(&self, sql: &str, id: Option<&str>, bind: &[(String, String)]) -> reqwest::RequestBuilder {
        let database = self.database.lock().clone();
        let mut q: Vec<(&str, &str)> = vec![("default_format", FORMAT)];
        if let Some(d) = &database {
            q.push(("database", d));
        }
        if let Some(s) = &self.session {
            q.push(("session_id", s));
            q.push(("session_timeout", SESSION_SECONDS));
        }
        if !self.locked {
            q.push(("http_write_exception_in_output_format", "0"));
        }
        if self.read_only {
            // 1, not 2: at 2 the server still runs `INSERT INTO FUNCTION file(…)`, and writes the file.
            // Its price is no `SET` and no `SETTINGS` of a statement's own on this connection.
            q.push(("readonly", "1"));
        }
        if let Some(id) = id {
            q.push(("query_id", id));
        }
        q.extend(bind.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        self.http.post(&self.base).header(AUTHORIZATION, self.auth.clone()).query(&q).body(sql.to_string())
    }

    /// A status that is no success carries the error in full.
    async fn send(&self, sql: &str, id: &str, bind: &[(String, String)]) -> Result<reqwest::Response, ChError> {
        // The statement before this one may hold the session for a moment after its connection
        // closed, when its KILL was refused (a read-only login's).
        let mut tries = 0;
        loop {
            let resp = self.request(sql, Some(id), bind).send().await.map_err(|e| transport(&self.base, &e))?;
            if resp.status().is_success() {
                return Ok(resp);
            }
            let code = header(resp.headers(), "x-clickhouse-exception-code").map(str::to_string);
            let body = resp.bytes().await.map_err(|e| transport(&self.base, &e))?;
            let err = server_error(code.as_deref(), &body);
            match &err {
                ChError::Server { code, .. } if code == CODE_SESSION_LOCKED && tries < 20 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ => return Err(err),
            }
        }
    }

    async fn start(self: &Arc<Self>, sql: &str, params: &[Option<String>]) -> Result<ChRows, ChError> {
        let bind = bind_params(sql, params)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        *self.current.lock() = Some(id.clone());
        let resp = match self.send(sql, &id, &bind).await {
            Ok(r) => r,
            Err(e) => {
                *self.current.lock() = None;
                return Err(e);
            }
        };
        if let Some(db) = used_database(sql) {
            *self.database.lock() = Some(db);
        }
        let raw = header(resp.headers(), "x-clickhouse-format").is_some_and(|f| f != FORMAT);
        let affected = written_rows(resp.headers());
        let mut rows = ChRows::new(self.clone(), resp, id, raw, affected);
        rows.read_header().await?;
        Ok(rows)
    }

    pub(crate) async fn query(self: &Arc<Self>, sql: &str, params: &[Option<String>]) -> Result<(Vec<ColumnInfo>, ChRows), ChError> {
        let rows = self.start(sql, params).await?;
        Ok((rows.columns.clone(), rows))
    }

    pub(crate) async fn execute(self: &Arc<Self>, sql: &str) -> Result<Option<u64>, ChError> {
        let mut rows = self.start(sql, &[]).await?;
        while rows.next_line().await?.is_some() {}
        Ok(rows.affected)
    }

    pub(crate) async fn text_rows(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>, ChError> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let resp = self.send(sql, &id, &[]).await?;
        let body = resp.bytes().await.map_err(|e| ChError::Cut(e.to_string()))?;
        if let Some(at) = exception_at(&body) {
            return Err(server_error(None, &body[at..]));
        }
        let Some(lines) = body.strip_suffix(b"\n") else {
            return if body.is_empty() { Ok(Vec::new()) } else { Err(ChError::Cut("the last row has no end".into())) };
        };
        let mut out = Vec::new();
        let mut scratch = String::new();
        // The names and the types come first. A line is a row even when empty: one empty cell.
        for line in lines.split(|&c| c == b'\n').skip(2) {
            out.push(
                line.strip_suffix(b"\r")
                    .unwrap_or(line)
                    .split(|&c| c == b'\t')
                    .map(|cell| {
                        (cell != b"\\N").then(|| {
                            unescape_into(cell, &mut scratch);
                            scratch.clone()
                        })
                    })
                    .collect(),
            );
        }
        Ok(out)
    }

    /// Waits until the statement is gone: the session is free for the next one after this.
    pub(crate) async fn kill(&self, id: &str) -> Result<()> {
        let sql = format!("KILL QUERY WHERE query_id = '{id}' SYNC");
        // Not in the session it would have to wait for, and whatever the connection may do.
        let q = [("default_format", FORMAT)];
        let resp = self.http.post(&self.base).header(AUTHORIZATION, self.auth.clone()).query(&q).body(sql).timeout(Duration::from_secs(5)).send().await?;
        if !resp.status().is_success() {
            let code = header(resp.headers(), "x-clickhouse-exception-code").map(str::to_string);
            let body = resp.bytes().await?;
            return Err(server_error(code.as_deref(), &body).into());
        }
        Ok(())
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        let id = self.current.lock().clone();
        match id {
            Some(id) => self.kill(&id).await,
            None => Ok(()),
        }
    }

    fn ended(&self, id: &str) {
        let mut g = self.current.lock();
        if g.as_deref() == Some(id) {
            *g = None;
        }
    }
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// The database `USE x` names.
fn used_database(sql: &str) -> Option<String> {
    use crate::sql::{Dialect, ident_value_in, tokenize_in};
    let d = Dialect::ClickHouse;
    let toks: Vec<_> = tokenize_in(sql, d).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    if !toks.first()?.text(sql).eq_ignore_ascii_case("USE") {
        return None;
    }
    // `USE database` is a database of that name.
    let name = match toks.get(2).filter(|t| t.text(sql) != ";") {
        Some(n) if toks[1].text(sql).eq_ignore_ascii_case("DATABASE") => n,
        _ => toks.get(1)?,
    };
    Some(ident_value_in(sql, name, d))
}

/// `X-ClickHouse-Summary`: every row written, those of the materialized views the statement fed
/// too (the server does not tell them apart). A statement that writes none has no count.
fn written_rows(h: &HeaderMap) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(header(h, "x-clickhouse-summary")?).ok()?;
    v["written_rows"].as_str()?.parse().ok().filter(|n| *n > 0)
}

/// The values by the names `{name:Type}` give them, in the order they first appear.
fn bind_params(sql: &str, params: &[Option<String>]) -> Result<Vec<(String, String)>, ChError> {
    let names = crate::params::brace_names(sql);
    if names.len() != params.len() {
        if params.is_empty() {
            // The server names what is missing.
            return Ok(Vec::new());
        }
        return Err(ChError::Params(format!("the statement has {} parameters and {} values were given", names.len(), params.len())));
    }
    Ok(names.into_iter().zip(params).map(|(n, v)| (format!("param_{n}"), param_text(v.as_deref()))).collect())
}

/// A parameter is read as a TSV cell.
fn param_text(v: Option<&str>) -> String {
    let Some(v) = v else { return "\\N".into() };
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

type Piece = Result<(Bytes, OwnedSemaphorePermit), ChError>;

/// Reads ahead of the grid, up to [`IN_FLIGHT_BYTES`]; ends when the receiver goes away, which
/// closes the connection.
fn spawn_reader(mut resp: reqwest::Response) -> mpsc::UnboundedReceiver<Piece> {
    let (tx, rx) = mpsc::unbounded_channel();
    let room = Arc::new(Semaphore::new(IN_FLIGHT_BYTES));
    tokio::spawn(async move {
        loop {
            let piece = tokio::select! {
                p = resp.chunk() => p,
                () = tx.closed() => return,
            };
            match piece {
                Ok(Some(b)) => {
                    let n = b.len().clamp(1, IN_FLIGHT_BYTES) as u32;
                    let permit = tokio::select! {
                        p = room.clone().acquire_many_owned(n) => p,
                        () = tx.closed() => return,
                    };
                    let Ok(permit) = permit else { return };
                    if tx.send(Ok((b, permit))).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    let mut message = e.to_string();
                    if let Some(s) = std::error::Error::source(&e) {
                        message = format!("{message}: {s}");
                    }
                    let _ = tx.send(Err(ChError::Cut(message)));
                    return;
                }
            }
        }
    });
    rx
}

fn exception_at(buf: &[u8]) -> Option<usize> {
    let mark = buf.windows(EXCEPTION_MARK.len()).position(|w| w == EXCEPTION_MARK)?;
    let after = buf.get(mark + EXCEPTION_MARK.len()..)?;
    let line_start = mark == 0 || buf[mark - 1] == b'\n';
    (line_start && after.first() == Some(&b'\r')).then_some(mark)
}

pub(crate) struct ChRows {
    client: Arc<Client>,
    rx: mpsc::UnboundedReceiver<Piece>,
    id: String,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    raw: bool,
    columns: Vec<ColumnInfo>,
    kinds: Vec<Kind>,
    rs: Option<Arc<ResultSet>>,
    /// One cell's bytes with its escapes undone, reused from cell to cell.
    scratch: Vec<u8>,
    affected: Option<u64>,
    done: bool,
    /// `output_format_tsv_crlf_end_of_line`: every line ends in `\r\n`, so that mark is no mark.
    crlf: bool,
}

impl ChRows {
    fn new(client: Arc<Client>, resp: reqwest::Response, id: String, raw: bool, affected: Option<u64>) -> ChRows {
        ChRows {
            client,
            rx: spawn_reader(resp),
            id,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            raw,
            columns: Vec::new(),
            kinds: Vec::new(),
            rs: None,
            scratch: Vec::new(),
            affected,
            done: false,
            crlf: false,
        }
    }

    pub(crate) fn show_in(&mut self, rs: &Arc<ResultSet>) {
        self.rs = Some(rs.clone());
    }

    /// One more piece into the buffer. Cancel-safe: `recv` loses nothing, and a piece is stored
    /// before anything else awaits.
    async fn fill(&mut self) -> Result<bool, ChError> {
        if self.eof {
            return Ok(false);
        }
        match self.rx.recv().await {
            // Its room goes back here, not when it is parsed: a row longer than the room has to
            // be read to its end. The buffer holds one unfinished line and one piece.
            Some(Ok((b, _room))) => {
                if self.pos > 0 {
                    self.buf.drain(..self.pos);
                    self.pos = 0;
                }
                self.buf.extend_from_slice(&b);
                Ok(true)
            }
            Some(Err(e)) => {
                self.finish();
                // What the server said before it closed, when it did.
                if let Some(at) = exception_at(&self.buf[self.pos..]) {
                    return Err(server_error(None, &self.buf[self.pos + at..]));
                }
                Err(e)
            }
            None => {
                self.eof = true;
                self.finish();
                Ok(false)
            }
        }
    }

    fn finish(&mut self) {
        if !self.done {
            self.done = true;
            self.client.ended(&self.id);
        }
    }

    /// The next line's range in the buffer. Cancel-safe.
    async fn next_line(&mut self) -> Result<Option<Range<usize>>, ChError> {
        let mut searched = 0;
        loop {
            let rest = &self.buf[self.pos..];
            if let Some(n) = rest[searched..].iter().position(|&c| c == b'\n') {
                let mut line = self.pos..self.pos + searched + n;
                let cr = self.buf[line.clone()].last() == Some(&b'\r');
                // In a format of the statement's own it may follow what was written on that line.
                let text = &self.buf[line.clone()];
                if cr && !self.crlf && (text.starts_with(EXCEPTION_MARK) || (self.raw && text[..text.len() - 1].ends_with(EXCEPTION_MARK))) {
                    return self.server_failure().await;
                }
                self.pos = line.end + 1;
                if cr && self.crlf {
                    line.end -= 1;
                }
                return Ok(Some(line));
            }
            searched = rest.len();
            if self.raw && rest.len() >= RAW_LINE_CAP {
                let line = self.pos..self.pos + whole_chars(rest);
                self.pos = line.end;
                return Ok(Some(line));
            }
            if !self.fill().await? {
                let rest = self.buf.len() - self.pos;
                if rest == 0 {
                    return Ok(None);
                }
                if let Some(at) = exception_at(&self.buf[self.pos..]) {
                    return Err(server_error(None, &self.buf[self.pos + at..]));
                }
                // The server ends every row: this one was cut, whatever closed the stream.
                if !self.raw {
                    return Err(ChError::Cut("the last row has no end".into()));
                }
                let line = self.pos..self.buf.len();
                self.pos = line.end;
                return Ok(Some(line));
            }
            // A piece may have moved what was read to the buffer's start.
            searched = searched.min(self.buf.len() - self.pos);
        }
    }

    /// The marker's line was read: the rest of the stream is the error.
    async fn server_failure(&mut self) -> Result<Option<Range<usize>>, ChError> {
        while let Ok(true) = self.fill().await {}
        self.finish();
        Err(server_error(None, &self.buf[self.pos..]))
    }

    async fn read_header(&mut self) -> Result<(), ChError> {
        if self.raw {
            self.columns = vec![ColumnInfo { name: "result".into(), type_name: "String".into(), type_oid: 0, kind: Kind::Text }];
            self.kinds = vec![Kind::Text];
            return Ok(());
        }
        let Some(mut names) = self.next_line().await? else { return Ok(()) };
        if self.buf[names.clone()].last() == Some(&b'\r') {
            self.crlf = true;
            names.end -= 1;
        }
        // A range survives only until the next piece: the names are taken first.
        let names = self.cells_text(names);
        let Some(types) = self.next_line().await? else {
            return Err(ChError::Cut("no column types".into()));
        };
        let types = self.cells_text(types);
        self.columns = names
            .into_iter()
            .zip(types)
            .map(|(name, type_name)| {
                let kind = kind_of(&type_name);
                ColumnInfo { name, type_name, type_oid: 0, kind }
            })
            .collect();
        self.kinds = self.columns.iter().map(|c| c.kind).collect();
        Ok(())
    }

    fn cells_text(&mut self, line: Range<usize>) -> Vec<String> {
        let mut text = String::new();
        self.buf[line]
            .split(|&c| c == b'\t')
            .map(|cell| {
                unescape_into(cell, &mut text);
                text.clone()
            })
            .collect()
    }

    fn demote(&mut self, b: &mut ChunkBuilder, col: usize) {
        b.demote_to_text(col);
        self.kinds[col] = Kind::Text;
        if let Some(rs) = &self.rs {
            rs.demote_to_text(col);
        }
    }
}

impl RowSource for ChRows {
    type Error = ChError;

    async fn next_into(&mut self, b: &mut ChunkBuilder) -> Result<bool, ChError> {
        let Some(line) = self.next_line().await? else { return Ok(false) };
        if self.raw {
            push_text_cell(b, 0, &self.buf[line], &mut self.scratch, false);
            b.end_row();
            return Ok(true);
        }
        let n = self.buf[line.clone()].iter().filter(|&&c| c == b'\t').count() + 1;
        if n != self.kinds.len() {
            // An older server's failure, without the marker.
            if self.buf[line.clone()].starts_with(b"Code: ") {
                return Err(server_error(None, &self.buf[line]));
            }
            return Err(ChError::Cut(format!("a row of {n} cells where {} were expected", self.kinds.len())));
        }
        let mut at = line.start;
        for i in 0..n {
            let end = self.buf[at..line.end].iter().position(|&c| c == b'\t').map_or(line.end, |p| at + p);
            let cell = at..end;
            if !push_cell(b, i, self.kinds[i], &self.buf[cell.clone()], &mut self.scratch) {
                self.demote(b, i);
                push_text_cell(b, i, &self.buf[cell], &mut self.scratch, true);
            }
            at = end + 1;
        }
        b.end_row();
        Ok(true)
    }

    /// Nobody wants the rest: the statement is stopped instead of read to its end.
    async fn skip(&mut self) -> Result<bool, ChError> {
        if !self.done {
            self.rx.close();
            self.finish();
            if let Err(e) = self.client.kill(&self.id).await {
                log::debug!("kill after a stopped result: {e:#}");
            }
        }
        Ok(false)
    }

    fn affected(&self) -> Option<u64> {
        self.affected
    }
}

/// How much of `b` ends on a character's end: a line cut at a cap must not split one.
fn whole_chars(b: &[u8]) -> usize {
    for i in 1..=b.len().min(4) {
        let c = b[b.len() - i];
        if c & 0xc0 == 0xc0 {
            return b.len() - i;
        }
        if c & 0x80 == 0 {
            break;
        }
    }
    b.len()
}

fn cell_str(cell: &[u8]) -> Option<&str> {
    std::str::from_utf8(cell).ok()
}

/// False when the cell does not fit its column (a user's format, a value the type's text did not
/// promise): the caller turns the column to text and stores it again.
fn push_cell(b: &mut ChunkBuilder, i: usize, kind: Kind, cell: &[u8], scratch: &mut Vec<u8>) -> bool {
    if cell == b"\\N" {
        b.push_null(i);
        return true;
    }
    match kind {
        Kind::Bool => match cell {
            b"true" | b"1" => b.push_bool(i, true),
            b"false" | b"0" => b.push_bool(i, false),
            _ => return false,
        },
        Kind::Int => match cell_str(cell).and_then(|s| s.parse().ok()) {
            Some(v) => b.push_i64(i, v),
            None => return false,
        },
        Kind::Float | Kind::Float4 => match cell_str(cell).and_then(|s| s.parse().ok()) {
            Some(v) => b.push_f64(i, v),
            None => return false,
        },
        Kind::Date => match parse_date(cell) {
            Some((y, m, d)) => b.push_i64(i, date_cell(y, m, d)),
            None => return false,
        },
        Kind::Timestamp => match parse_timestamp(cell) {
            Some(v) => b.push_i64(i, v),
            None => return false,
        },
        // An array's, tuple's or map's text is a literal with its own quoting, as the server wrote
        // it: undoing escapes would make `['a\',\'b']` read as two elements. A `Variant`'s or a
        // `Dynamic`'s scalar is written as any cell.
        Kind::Other => push_text_cell(b, i, cell, scratch, !matches!(cell.first(), Some(b'[' | b'(' | b'{'))),
        _ => push_text_cell(b, i, cell, scratch, true),
    }
    true
}

/// A `String` holds any bytes: those that are no text are shown as hex, never replaced.
fn push_text_cell(b: &mut ChunkBuilder, i: usize, cell: &[u8], scratch: &mut Vec<u8>, escaped: bool) {
    let bytes = if escaped && cell.contains(&b'\\') {
        scratch.clear();
        unescape_bytes(cell, scratch);
        &scratch[..]
    } else {
        cell
    };
    match std::str::from_utf8(bytes) {
        Ok(s) => b.push_text(i, s),
        Err(_) => b.push_text_with(i, |t| super::embed::hex(bytes, t)),
    }
}

/// TSV's escapes, as the server writes them.
fn unescape_bytes(cell: &[u8], out: &mut Vec<u8>) {
    let mut it = cell.iter().copied();
    while let Some(c) = it.next() {
        if c != b'\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'r') => out.push(b'\r'),
            Some(b'b') => out.push(8),
            Some(b'f') => out.push(12),
            Some(b'a') => out.push(7),
            Some(b'v') => out.push(11),
            Some(b'0') => out.push(0),
            Some(o) => out.push(o),
            None => out.push(b'\\'),
        }
    }
}

fn unescape_into(cell: &[u8], out: &mut String) {
    out.clear();
    let mut raw = Vec::with_capacity(cell.len());
    unescape_bytes(cell, &mut raw);
    match std::str::from_utf8(&raw) {
        Ok(s) => out.push_str(s),
        Err(_) => super::embed::hex(&raw, out),
    }
}

fn digits<T: std::str::FromStr>(b: &[u8]) -> Option<T> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

fn parse_date(b: &[u8]) -> Option<(u16, u8, u8)> {
    (b.len() >= 10 && b[4] == b'-' && b[7] == b'-').then_some(())?;
    Some((digits(&b[..4])?, digits(&b[5..7])?, digits(&b[8..10])?))
}

/// `YYYY-MM-DD hh:mm:ss[.fraction]`, the wall clock in the column's time zone.
fn parse_timestamp(b: &[u8]) -> Option<i64> {
    let (y, m, d) = parse_date(b)?;
    if b.len() < 19 || b[10] != b' ' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let us = match b.get(19) {
        None => 0,
        Some(b'.') => {
            let f = &b[20..];
            // Only a type of up to 6 digits is a timestamp here (`kind_of`).
            if f.is_empty() || f.len() > 6 {
                return None;
            }
            digits::<u32>(f)? * 10u32.pow(6 - f.len() as u32)
        }
        Some(_) => return None,
    };
    Some(timestamp_cell(y, m, d, digits(&b[11..13])?, digits(&b[14..16])?, digits(&b[17..19])?, us))
}

/// What a column's type is, under the wrappers that do not change how its values read.
pub(crate) fn kind_of(type_name: &str) -> Kind {
    let mut t = type_name.trim();
    loop {
        let inner = t.strip_prefix("Nullable(").or_else(|| t.strip_prefix("LowCardinality(")).and_then(|r| r.strip_suffix(')'));
        match inner {
            Some(i) => t = i,
            None => break,
        }
    }
    let (head, args) = t.split_once('(').unwrap_or((t, ""));
    match head {
        "Bool" => Kind::Bool,
        "Int8" | "Int16" | "Int32" | "Int64" | "UInt8" | "UInt16" | "UInt32" => Kind::Int,
        // Past what an i64 holds: kept exact, as text.
        "UInt64" | "Int128" | "Int256" | "UInt128" | "UInt256" => Kind::Numeric,
        "Float32" => Kind::Float4,
        "Float64" => Kind::Float,
        h if h.starts_with("Decimal") => Kind::Numeric,
        "String" | "FixedString" | "Enum" | "Enum8" | "Enum16" | "Nothing" => Kind::Text,
        // Text to read, but the server's `LIKE` refuses them: a filter casts what is no text.
        "UUID" | "IPv4" | "IPv6" | "Time" | "Time64" => Kind::Other,
        "Date" | "Date32" => Kind::Date,
        "DateTime" => Kind::Timestamp,
        // Past microseconds the digits would be lost: text.
        "DateTime64" => {
            let p: u32 = args.split([',', ')']).next().and_then(|p| p.trim().parse().ok()).unwrap_or(9);
            if p <= 6 { Kind::Timestamp } else { Kind::Text }
        }
        "JSON" | "Object" => Kind::Json,
        _ => Kind::Other,
    }
}

pub(crate) async fn server_info(c: &Client) -> Result<ServerInfo> {
    let rows = c.text_rows("SELECT version(), currentDatabase(), currentUser()").await?;
    let row = rows.first().ok_or_else(|| anyhow::anyhow!("version() returned no row"))?;
    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let raw = get(0);
    let mut info = ServerInfo { database: get(1), user: get(2), ..ServerInfo::of(Engine::ClickHouse, Flavor::Standard, format!("ClickHouse {raw}"), &raw) };
    info.search_path = vec![info.database.clone()];
    Ok(info)
}

pub(crate) async fn search_path(c: &Client) -> Vec<String> {
    match c.text_rows("SELECT currentDatabase()").await {
        Ok(rows) => rows.first().and_then(|r| r.first().cloned().flatten()).into_iter().collect(),
        Err(e) => {
            log::debug!("currentDatabase(): {e}");
            Vec::new()
        }
    }
}

pub(crate) async fn run(client: &Arc<Client>, rs: &Arc<ResultSet>, started: Instant, on_pause: &(dyn Fn() + Sync), aborted: &(dyn Fn() -> bool + Sync)) -> Ran {
    let fail = |e: &ChError| if interrupted(e) { Ran::Cancelled } else { Ran::Failed(error_info_at(e, &rs.sql)) };
    if aborted() {
        return Ran::NeverRan { settle: false };
    }
    rs.executing.store(true, std::sync::atomic::Ordering::Release);
    let (columns, mut rows) = match client.query(&rs.sql, rs.params()).await {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    rows.show_in(rs);
    rs.set_columns(columns);
    let cols = rs.columns().unwrap_or_default();
    match super::pump(&mut rows, rs, &cols, started, on_pause).await {
        Ok(n) => Ran::Done(n),
        Err(e) => fail(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_under_wrappers() {
        assert_eq!(kind_of("LowCardinality(Nullable(String))"), Kind::Text);
        assert_eq!(kind_of("Nullable(Int32)"), Kind::Int);
        assert_eq!(kind_of("UInt64"), Kind::Numeric, "past an i64");
        assert_eq!(kind_of("Decimal(18, 2)"), Kind::Numeric);
        assert_eq!(kind_of("DateTime('UTC')"), Kind::Timestamp);
        assert_eq!(kind_of("DateTime64(6, 'UTC')"), Kind::Timestamp);
        assert_eq!(kind_of("DateTime64(9)"), Kind::Text, "nanoseconds do not fit");
        assert_eq!(kind_of("Array(Nullable(Int32))"), Kind::Other);
        assert_eq!(kind_of("Enum8('a' = 1)"), Kind::Text);
        // Text to read, none to the server's `LIKE`: a filter casts them.
        for t in ["UUID", "IPv4", "IPv6", "Nullable(UUID)", "LowCardinality(IPv4)", "Time", "Time64(3)"] {
            assert_eq!(kind_of(t), Kind::Other, "{t}");
        }
    }

    #[test]
    fn errors_as_the_server_writes_them() {
        let e = server_error(Some("62"), b"Code: 62. DB::Exception: Syntax error: failed at position 8 (FORM): x. (SYNTAX_ERROR) (version 25.8.33.6 (official build))\n");
        let ChError::Server { code, message, position } = &e else { panic!("{e:?}") };
        assert_eq!((code.as_str(), *position), ("62", Some(8)));
        assert_eq!(message, "Syntax error: failed at position 8 (FORM): x. (SYNTAX_ERROR)");
        assert_eq!(error_info_at(&e, "SELECT é FORM t").position, Some(8));
        // Inside a stream: no header, the marker before it.
        let e = server_error(None, b"__exception__\r\nCode: 395. DB::Exception: boom. (X) (version 1)\n\r\n");
        let ChError::Server { code, message, .. } = &e else { panic!("{e:?}") };
        assert_eq!((code.as_str(), message.as_str()), ("395", "boom. (X)"));
    }

    #[test]
    fn the_failure_mark_is_only_a_whole_line_of_its_own() {
        assert_eq!(exception_at(b"1\t2\n__exception__\r\nCode: 1."), Some(4));
        assert_eq!(exception_at(b"a__exception__\r\n"), None);
        assert_eq!(exception_at(b"__exception__\n"), None, "a cell's text: its \\r would be escaped");
    }

    #[test]
    fn cells_unescape_and_stay_exact() {
        let mut s = String::new();
        unescape_into(br"a\tb\nc\\d\'e\0", &mut s);
        assert_eq!(s, "a\tb\nc\\d'e\0");
        unescape_into(b"\xff", &mut s);
        assert_eq!(s, "\\xff", "bytes that are no text are shown as hex");
        assert_eq!(parse_date(b"2024-01-02"), Some((2024, 1, 2)));
        assert_eq!(parse_timestamp(b"2000-01-01 00:00:01.5"), Some(1_500_000));
        assert_eq!(parse_timestamp(b"2000-01-01 00:00:01"), Some(1_000_000));
        assert_eq!(parse_timestamp(b"2000-01-01 00:00:01.1234567"), None);
        assert_eq!(param_text(Some("a\\b\tc\nd")), "a\\\\b\\tc\\nd");
        assert_eq!(param_text(None), "\\N");
    }

    #[test]
    fn use_is_followed() {
        assert_eq!(used_database("USE system").as_deref(), Some("system"));
        assert_eq!(used_database("/* x */ use `a b` ;").as_deref(), Some("a b"));
        assert_eq!(used_database("USE DATABASE d").as_deref(), Some("d"));
        assert_eq!(used_database("USE database").as_deref(), Some("database"), "a database named so");
        assert_eq!(used_database("USE database;").as_deref(), Some("database"));
        assert_eq!(used_database(r"USE `a\x62`").as_deref(), Some("ab"), "as the server reads the name");
        assert_eq!(used_database("SELECT 1"), None);
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        assert_eq!(whole_chars(b"abc"), 3);
        assert_eq!(whole_chars("aé".as_bytes()), 1, "a whole one at the end waits too: harmless");
        assert_eq!(whole_chars(&"é".as_bytes()[..1]), 0);
        assert_eq!(whole_chars(&"a€".as_bytes()[..3]), 1);
    }
}
