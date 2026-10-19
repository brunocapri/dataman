//! Redis over RESP2: a small client of our own on tokio, TLS through the process' rustls (ring),
//! SSH tunnels to the forwarded port. One connection per Dataman connection, its commands one
//! at a time; each selects the database it is for (a tab's), so tabs on different databases
//! share it safely. A broken connection is dialed again on the next command.
//!
//! A key's whole value (`HGETALL`, `LRANGE k 0 -1`, `SMEMBERS`, `ZRANGE k 0 -1 WITHSCORES`,
//! `GET`) comes back as documents, one per entry (a hash's field, a list's item, a set's member,
//! a sorted set's member and score), paged and saved back entry by entry.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};

use crate::config::{ConnectionProfile, SslMode};
use crate::nosql::redis::{self as cmd};
use crate::nosql::{Body, Doc, Docs, Item, ItemKind, Output, Page, Source};
use crate::ssh::Tunnel;

/// A reply.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Reply>>),
}

impl Reply {
    pub fn json(&self) -> Value {
        match self {
            Reply::Simple(s) | Reply::Error(s) => Value::String(s.clone()),
            Reply::Int(n) => json!(n),
            // Bytes that are not text: as redis-cli shows them.
            Reply::Bulk(Some(b)) => Value::String(cmd::escape_bytes(b)),
            Reply::Bulk(None) | Reply::Array(None) => Value::Null,
            Reply::Array(Some(a)) => Value::Array(a.iter().map(Reply::json).collect()),
        }
    }

    /// Bytes that are not UTF-8 (shown escaped: an entry holding them is not edited as text).
    fn binary(&self) -> bool {
        match self {
            Reply::Bulk(Some(b)) => std::str::from_utf8(b).is_err(),
            Reply::Array(Some(a)) => a.iter().any(Reply::binary),
            _ => false,
        }
    }

    fn text(&self) -> Option<String> {
        match self {
            Reply::Simple(s) => Some(s.clone()),
            Reply::Bulk(Some(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Reply::Int(n) => Some(n.to_string()),
            _ => None,
        }
    }

    fn items(self) -> Vec<Reply> {
        match self {
            Reply::Array(Some(a)) => a,
            _ => Vec::new(),
        }
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// How to reach the server again.
struct Dial {
    addr: SocketAddr,
    tls: Option<(Arc<rustls::ClientConfig>, rustls::pki_types::ServerName<'static>)>,
    auth: Vec<String>,
}

struct Conn {
    io: BufStream<Box<dyn Stream>>,
    /// The database selected on it.
    db: u32,
}

pub(crate) struct Client {
    dial: Dial,
    conn: tokio::sync::Mutex<Option<Conn>>,
    pub tls_active: bool,
    /// `Redis 7.4.1` (Valkey says so).
    pub version: String,
    pub default_db: u32,
}

/// The longest one command may take (blocking commands are refused).
const TIMEOUT: Duration = Duration::from_secs(60);

async fn write_command(io: &mut BufStream<Box<dyn Stream>>, args: &[&[u8]]) -> std::io::Result<()> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    io.write_all(&buf).await?;
    io.flush().await
}

fn read_reply<'a>(io: &'a mut BufStream<Box<dyn Stream>>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Reply>> + Send + 'a>> {
    Box::pin(async move {
        let mut line = Vec::new();
        if io.read_until(b'\n', &mut line).await? == 0 {
            bail!("the server closed the connection");
        }
        let line = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
        let (t, rest) = line.split_at(1.min(line.len()));
        let num = || rest.parse::<i64>().map_err(|_| anyhow!("unexpected reply {line:?}"));
        Ok(match t {
            "+" => Reply::Simple(rest.to_string()),
            "-" => Reply::Error(rest.to_string()),
            ":" => Reply::Int(num()?),
            "$" => {
                let n = num()?;
                if n < 0 {
                    Reply::Bulk(None)
                } else {
                    let mut b = vec![0; n as usize + 2];
                    io.read_exact(&mut b).await?;
                    b.truncate(n as usize);
                    Reply::Bulk(Some(b))
                }
            }
            "*" => {
                let n = num()?;
                if n < 0 {
                    Reply::Array(None)
                } else {
                    let mut v = Vec::with_capacity(n.min(10_000) as usize);
                    for _ in 0..n {
                        v.push(read_reply(io).await?);
                    }
                    Reply::Array(Some(v))
                }
            }
            _ => bail!("unexpected reply {line:?} (not RESP2)"),
        })
    })
}

async fn open(d: &Dial) -> Result<Conn> {
    let tcp = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(d.addr)).await.map_err(|_| anyhow!("timed out connecting to {}", d.addr))??;
    let _ = tcp.set_nodelay(true);
    let stream: Box<dyn Stream> = match &d.tls {
        Some((cfg, name)) => Box::new(tokio_rustls::TlsConnector::from(cfg.clone()).connect(name.clone(), tcp).await.context("TLS")?),
        None => Box::new(tcp),
    };
    let mut c = Conn { io: BufStream::new(stream), db: 0 };
    if !d.auth.is_empty() {
        let mut args: Vec<&[u8]> = vec![b"AUTH"];
        args.extend(d.auth.iter().map(|a| a.as_bytes()));
        write_command(&mut c.io, &args).await?;
        if let Reply::Error(e) = read_reply(&mut c.io).await? {
            bail!("authentication failed: {}", e.trim_start_matches("WRONGPASS ").trim_start_matches("ERR "));
        }
    }
    Ok(c)
}

pub(crate) async fn connect(p: &ConnectionProfile, password: Option<&str>, tunnel: Option<&Tunnel>) -> Result<Client> {
    let addr = match tunnel {
        Some(t) => t.local_addr(),
        None => tokio::net::lookup_host((p.host.as_str(), p.port)).await.with_context(|| format!("resolving {}", p.host))?.next().ok_or_else(|| anyhow!("{} has no address", p.host))?,
    };
    let tls = match crate::backend::pg::rustls_config(p.sslmode)? {
        Some(cfg) if p.sslmode != SslMode::Prefer => {
            let name = rustls::pki_types::ServerName::try_from(p.host.clone()).map_err(|_| anyhow!("{} is not a TLS server name", p.host))?;
            Some((Arc::new(cfg), name))
        }
        _ => None,
    };
    let auth = match (p.user.trim(), password.filter(|s| !s.is_empty())) {
        ("", Some(pw)) => vec![pw.to_string()],
        (u, Some(pw)) => vec![u.to_string(), pw.to_string()],
        _ => Vec::new(),
    };
    let default_db = if p.database.trim().is_empty() { 0 } else { p.database.trim().parse().map_err(|_| anyhow!("the database is a number (0–15)"))? };
    let tls_active = tls.is_some();
    let dial = Dial { addr, tls, auth };
    let conn = match open(&dial).await {
        Ok(c) => c,
        Err(e) => {
            if let Some(t) = tunnel
                && let Some(te) = t.take_error()
            {
                return Err(te.into());
            }
            return Err(e);
        }
    };
    let mut c = Client { dial, conn: tokio::sync::Mutex::new(Some(conn)), tls_active, version: String::new(), default_db };
    let info = c.call(default_db, &["INFO", "server"]).await?.text().unwrap_or_default();
    let field = |k: &str| info.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
    c.version = match field("valkey_version:") {
        Some(v) => format!("Valkey {v}"),
        None => format!("Redis {}", field("redis_version:").unwrap_or_default()),
    };
    Ok(c)
}

impl Client {
    /// One command on database `db`: its reply (a server error is an error).
    pub(crate) async fn call<S: AsRef<[u8]>>(&self, db: u32, args: &[S]) -> Result<Reply> {
        let mut r = self.raw(db, &[args.iter().map(|a| a.as_ref()).collect()]).await?;
        match r.pop() {
            Some(Reply::Error(e)) => Err(anyhow!(e)),
            Some(r) => Ok(r),
            None => Err(anyhow!("no reply")),
        }
    }

    /// Commands sent together (a pipeline), their replies in order (errors kept as replies).
    async fn pipeline(&self, db: u32, cmds: Vec<Vec<Vec<u8>>>) -> Result<Vec<Reply>> {
        let refs: Vec<Vec<&[u8]>> = cmds.iter().map(|c| c.iter().map(|a| a.as_slice()).collect()).collect();
        self.raw(db, &refs).await
    }

    async fn raw(&self, db: u32, cmds: &[Vec<&[u8]>]) -> Result<Vec<Reply>> {
        let mut g = self.conn.lock().await;
        if g.is_none() {
            *g = Some(open(&self.dial).await?);
        }
        let c = g.as_mut().expect("connected");
        let r = tokio::time::timeout(TIMEOUT, async {
            if c.db != db {
                write_command(&mut c.io, &[b"SELECT", db.to_string().as_bytes()]).await?;
                if let Reply::Error(e) = read_reply(&mut c.io).await? {
                    bail!(e);
                }
                c.db = db;
            }
            for cmd in cmds {
                write_command(&mut c.io, cmd).await?;
            }
            let mut out = Vec::with_capacity(cmds.len());
            for _ in cmds {
                out.push(read_reply(&mut c.io).await?);
            }
            Ok(out)
        })
        .await;
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                // The stream may be anywhere in a reply: dial again next time.
                *g = None;
                Err(e)
            }
            Err(_) => {
                *g = None;
                Err(anyhow!("no answer in {} s (the command was stopped)", TIMEOUT.as_secs()))
            }
        }
    }
}

fn db_of(db: &str, c: &Client) -> Result<u32> {
    if db.trim().is_empty() {
        return Ok(c.default_db);
    }
    db.trim().parse().map_err(|_| anyhow!("Redis' databases are numbers: {db:?}"))
}

// ---------------------------------------------------------------------------------------------
// The explorer

/// The databases that hold keys (and the connection's own), with their key counts.
pub(crate) async fn databases(c: &Client) -> Result<Vec<Item>> {
    let info = c.call(c.default_db, &["INFO", "keyspace"]).await?.text().unwrap_or_default();
    let mut items: Vec<Item> = info
        .lines()
        .filter_map(|l| {
            let (db, rest) = l.strip_prefix("db")?.split_once(':')?;
            let keys = rest.split(',').find_map(|kv| kv.strip_prefix("keys="))?.parse().ok();
            Some(Item { name: db.to_string(), kind: ItemKind::Database, count: keys, size: None, note: None })
        })
        .collect();
    if !items.iter().any(|i| i.name == c.default_db.to_string()) {
        items.push(Item { name: c.default_db.to_string(), kind: ItemKind::Database, count: Some(0), size: None, note: None });
    }
    items.sort_by_key(|i| i.name.parse::<u32>().unwrap_or(u32::MAX));
    Ok(items)
}

/// The most keys the explorer lists under a database.
const KEYS_LISTED: usize = 2000;

/// A database's keys (the first [`KEYS_LISTED`], by name), each with its type.
pub(crate) async fn keys(c: &Client, db: &str) -> Result<Vec<Item>> {
    let db = db_of(db, c)?;
    let mut cursor = "0".to_string();
    let mut keys: Vec<String> = Vec::new();
    loop {
        let r = c.call(db, &["SCAN", cursor.as_str(), "COUNT", "1000"]).await?.items();
        let mut it = r.into_iter();
        cursor = it.next().and_then(|r| r.text()).unwrap_or_else(|| "0".into());
        keys.extend(it.next().map(Reply::items).unwrap_or_default().iter().filter_map(Reply::text));
        if cursor == "0" || keys.len() >= KEYS_LISTED {
            break;
        }
    }
    keys.sort();
    keys.dedup();
    keys.truncate(KEYS_LISTED);
    let types = c.pipeline(db, keys.iter().map(|k| vec![b"TYPE".to_vec(), k.as_bytes().to_vec()]).collect()).await?;
    Ok(keys
        .into_iter()
        .zip(types)
        .map(|(name, t)| Item { name, kind: ItemKind::Collection, count: None, size: None, note: t.text() })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Commands

/// A command that shows a key's whole value, read as one: its type and key.
fn view_of(args: &[String]) -> Option<(&'static str, String)> {
    let up: Vec<String> = args.iter().map(|a| a.to_ascii_uppercase()).collect();
    let key = args.get(1)?.clone();
    Some(match (up[0].as_str(), args.len()) {
        ("HGETALL", 2) => ("hash", key),
        ("SMEMBERS", 2) => ("set", key),
        ("GET", 2) => ("string", key),
        ("LRANGE", 4) if args[2] == "0" && args[3] == "-1" => ("list", key),
        ("ZRANGE", 5) if args[2] == "0" && args[3] == "-1" && up[4] == "WITHSCORES" => ("zset", key),
        _ => return None,
    })
}

/// Runs one command on database `db`: a key's value as entries (paged), a write's reply, or
/// the reply as JSON.
pub(crate) async fn run(c: &Client, db: &str, args: &[String], raw: &[Vec<u8>], page: Page) -> Result<Output> {
    if let Some(why) = cmd::refusal(args) {
        bail!(why);
    }
    if cmd::name(args) == "SELECT" {
        let n = args.get(1).ok_or_else(|| anyhow!("SELECT takes a database number"))?;
        n.parse::<u32>().map_err(|_| anyhow!("SELECT takes a database number"))?;
        return Ok(Output::Use(n.clone()));
    }
    let dbn = db_of(db, c)?;
    if let Some((kind, key)) = view_of(args) {
        return view(c, dbn, db, kind, &key, page).await;
    }
    let r = c.call(dbn, raw).await?;
    if cmd::class(args).is_write() {
        let affected = match &r {
            Reply::Int(n) => (*n).max(0) as u64,
            Reply::Bulk(None) | Reply::Array(None) => 0,
            _ => 1,
        };
        let summary = match &r {
            Reply::Int(n) => format!("(integer) {n}"),
            Reply::Bulk(None) => "(nil)".into(),
            other => other.text().unwrap_or_else(|| "OK".into()),
        };
        return Ok(Output::Written { affected, summary, detail: None });
    }
    Ok(Output::Value(Body::Json(r.json())))
}

fn entry(id: Value, fields: Value) -> Doc {
    Doc { id: Some(Body::Json(id)), index: None, body: Body::Json(fields) }
}

/// The entries of a key's value, page `page`.
async fn view(c: &Client, db: u32, dbname: &str, kind: &str, key: &str, page: Page) -> Result<Output> {
    let (skip, limit) = (page.skip as i64, page.limit as i64);
    let t = c.call(db, &["TYPE", key]).await?.text().unwrap_or_default();
    // No such key: what the command itself says (nil, or an empty list).
    if t == "none" {
        return Ok(Output::Value(Body::Json(if kind == "string" { Value::Null } else { json!([]) })));
    }
    if t != kind {
        bail!("{key:?} is a {t}, not a {kind}");
    }
    let mut source = Some(Source::Redis { db: dbname.to_string(), key: key.to_string(), kind: kind.to_string() });
    let (docs, total): (Vec<Doc>, u64) = match kind {
        "string" => {
            let r = c.call(db, &["GET", key]).await?;
            if r.binary() {
                source = None;
            }
            (vec![entry(json!(key), json!({ "value": r.json() }))], 1)
        }
        "list" => {
            let total = c.call(db, &["LLEN", key]).await?.json().as_u64().unwrap_or(0);
            let r = c.call(db, &["LRANGE", key, &skip.to_string(), &(skip + limit - 1).to_string()]).await?;
            if r.binary() {
                source = None;
            }
            let items = r.items();
            let docs = items.into_iter().enumerate().map(|(i, v)| entry(json!(skip + i as i64), json!({ "index": skip + i as i64, "value": v.json() }))).collect();
            (docs, total)
        }
        "zset" => {
            let total = c.call(db, &["ZCARD", key]).await?.json().as_u64().unwrap_or(0);
            let r = c.call(db, &["ZRANGE", key, &skip.to_string(), &(skip + limit - 1).to_string(), "WITHSCORES"]).await?;
            if r.binary() {
                source = None;
            }
            let items = r.items();
            let docs = items
                .chunks(2)
                .map(|p| {
                    let m = p[0].json();
                    let score = p.get(1).and_then(Reply::text).and_then(|s| s.parse::<f64>().ok()).map_or(Value::Null, |f| json!(f));
                    entry(m.clone(), json!({ "member": m, "score": score }))
                })
                .collect();
            (docs, total)
        }
        "hash" | "set" => {
            let (count_cmd, scan_cmd) = if kind == "hash" { ("HLEN", "HSCAN") } else { ("SCARD", "SSCAN") };
            let total = c.call(db, &[count_cmd, key]).await?.json().as_u64().unwrap_or(0);
            // Scanned in the server's order, which stays put while the value does.
            let want = (skip + limit) as usize;
            let mut cursor = "0".to_string();
            let mut got: Vec<Reply> = Vec::new();
            loop {
                let r = c.call(db, &[scan_cmd, key, cursor.as_str(), "COUNT", "500"]).await?.items();
                let mut it = r.into_iter();
                cursor = it.next().and_then(|r| r.text()).unwrap_or_else(|| "0".into());
                got.extend(it.next().map(Reply::items).unwrap_or_default());
                let n = if kind == "hash" { got.len() / 2 } else { got.len() };
                if cursor == "0" || n >= want {
                    break;
                }
            }
            if got.iter().any(Reply::binary) {
                source = None;
            }
            let docs: Vec<Doc> = if kind == "hash" {
                got.chunks(2).skip(skip as usize).take(limit as usize).map(|p| entry(p[0].json(), json!({ "field": p[0].json(), "value": p.get(1).map(Reply::json) }))).collect()
            } else {
                got.into_iter().skip(skip as usize).take(limit as usize).map(|m| entry(m.json(), json!({ "member": m.json() }))).collect()
            };
            (docs, total)
        }
        _ => bail!("{kind} values are not shown as entries"),
    };
    let more = page.skip + (docs.len() as u64) < total;
    Ok(Output::Docs(Docs { docs, source, total: Some(total), skip: page.skip, more }))
}

/// How many keys or entries a write would change (its keys or members named).
pub(crate) fn affects(args: &[String]) -> Option<u64> {
    let n = cmd::name(args);
    Some(match n.as_str() {
        "DEL" | "UNLINK" => args.len().saturating_sub(1) as u64,
        "HDEL" | "SREM" | "ZREM" => args.len().saturating_sub(2) as u64,
        "MSET" | "MSETNX" => (args.len().saturating_sub(1) / 2) as u64,
        _ if cmd::class(args).is_write() => 1,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Entries

fn text_of(v: Option<&Value>, what: &str) -> Result<String> {
    match v {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Null) | None => Err(anyhow!("the entry needs its {what}")),
        Some(other) => Ok(other.to_string()),
    }
}

/// Saves an entry as edited (`id`: the entry it was, `None`: a new one); its id now.
pub(crate) async fn save(c: &Client, db: &str, key: &str, kind: &str, id: Option<Value>, entry: Value) -> Result<Value> {
    let db = db_of(db, c)?;
    let e = entry.as_object().ok_or_else(|| anyhow!("an entry is an object, as shown"))?;
    let old = id.as_ref().map(|v| text_of(Some(v), "id")).transpose()?;
    let check = |r: Reply| match r {
        Reply::Error(e) => Err(anyhow!(e)),
        _ => Ok(()),
    };
    match kind {
        "string" => {
            check(c.call(db, &["SET", key, &text_of(e.get("value"), "value")?, "KEEPTTL"]).await?)?;
            Ok(json!(key))
        }
        "hash" => {
            let field = text_of(e.get("field"), "field")?;
            let value = text_of(e.get("value"), "value")?;
            if let Some(o) = old.filter(|o| *o != field) {
                c.call(db, &["HDEL", key, &o]).await?;
            }
            c.call(db, &["HSET", key, &field, &value]).await?;
            Ok(json!(field))
        }
        "set" => {
            let m = text_of(e.get("member"), "member")?;
            if let Some(o) = old.filter(|o| *o != m) {
                c.call(db, &["SREM", key, &o]).await?;
            }
            c.call(db, &["SADD", key, &m]).await?;
            Ok(json!(m))
        }
        "zset" => {
            let m = text_of(e.get("member"), "member")?;
            let score = e.get("score").and_then(Value::as_f64).ok_or_else(|| anyhow!("the entry needs its score (a number)"))?;
            if let Some(o) = old.filter(|o| *o != m) {
                c.call(db, &["ZREM", key, &o]).await?;
            }
            c.call(db, &["ZADD", key, &score.to_string(), &m]).await?;
            Ok(json!(m))
        }
        "list" => {
            let value = text_of(e.get("value"), "value")?;
            match id.as_ref().and_then(Value::as_i64) {
                Some(i) => {
                    c.call(db, &["LSET", key, &i.to_string(), &value]).await?;
                    Ok(json!(i))
                }
                None => {
                    let n = c.call(db, &["RPUSH", key, &value]).await?.json().as_i64().unwrap_or(1);
                    Ok(json!(n - 1))
                }
            }
        }
        k => bail!("{k} values cannot be edited here"),
    }
}

/// Deletes an entry (a string: its key).
pub(crate) async fn delete(c: &Client, db: &str, key: &str, kind: &str, id: Value) -> Result<u64> {
    let db = db_of(db, c)?;
    let n = |r: Reply| r.json().as_u64().unwrap_or(0);
    Ok(match kind {
        "string" => n(c.call(db, &["DEL", key]).await?),
        "hash" => n(c.call(db, &["HDEL", key, &text_of(Some(&id), "field")?]).await?),
        "set" => n(c.call(db, &["SREM", key, &text_of(Some(&id), "member")?]).await?),
        "zset" => n(c.call(db, &["ZREM", key, &text_of(Some(&id), "member")?]).await?),
        "list" => {
            // By its place: marked, then the mark removed (LREM goes by value).
            let i = id.as_i64().ok_or_else(|| anyhow!("a list entry is its index"))?;
            let mark = format!("__dataman_deleted_{}__", uuid::Uuid::new_v4().simple());
            c.call(db, &["LSET", key, &i.to_string(), &mark]).await?;
            n(c.call(db, &["LREM", key, "1", &mark]).await?)
        }
        k => bail!("{k} values cannot be edited here"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_values_are_views() {
        let v = |s: &str| view_of(&cmd::parse(s).unwrap());
        assert_eq!(v("HGETALL user:1"), Some(("hash", "user:1".into())));
        assert_eq!(v("lrange q 0 -1"), Some(("list", "q".into())));
        assert_eq!(v("LRANGE q 0 10"), None);
        assert_eq!(v("ZRANGE z 0 -1 withscores"), Some(("zset", "z".into())));
        assert_eq!(v("GET \"a b\""), Some(("string", "a b".into())));
        assert_eq!(v("GET a b"), None);
        assert_eq!(affects(&cmd::parse("DEL a b c").unwrap()), Some(3));
        assert_eq!(affects(&cmd::parse("GET a").unwrap()), None);
    }
}
