//! The local channel from `diskman-mcp` (stdio) to the running app: while the app runs,
//! the stdio binary forwards every `tools/call` to it, so the app's own hub runs the tool, with
//! its agent tabs and a human to ask (the binary alone can ask nobody, so it refuses what needs
//! approval). One Unix domain socket in the config directory, `app.sock`, mode 0600; a key
//! made at each launch in `app.key` (0600, 256 bits) that every request carries, compared in
//! constant time; the peer must be the socket's owner (the kernel's credentials). One request per
//! connection, newline-delimited JSON:
//!
//! ```text
//! → {"v":1,"key":"…","op":"call","tool":"execute","args":{…},"client":"claude-code"}
//! ← {"ok":true,"result":{…CallToolResult…}}            (or {"ok":false,"error":"unauthorized"})
//! ```
//!
//! The client closing its end cancels the call in the app. Without the app (no socket, a stale
//! one, no key) the binary runs the call itself, headless; once a request was sent it never runs
//! it again locally (a write must not run twice).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use diskman_core::Dirs;
use rmcp::model::CallToolResult;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::hub::Hub;
use crate::server::Server;
use crate::tool::Registry;

pub const SOCKET: &str = "app.sock";
pub const KEY: &str = "app.key";
pub const VERSION: u32 = 1;
/// The longest request line (a tool call's arguments).
const MAX_LINE: u64 = 8 * 1024 * 1024;
/// How long a connection may take to send its request.
const READ_REQUEST: Duration = Duration::from_secs(5);

pub fn socket_path(dirs: &Dirs) -> PathBuf {
    dirs.config.join(SOCKET)
}

pub fn key_path(dirs: &Dirs) -> PathBuf {
    dirs.config.join(KEY)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub key: String,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Ping,
    /// A tool call, as the stdio client received it.
    Call {
        tool: String,
        #[serde(default)]
        args: Map<String, Value>,
        client: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The `CallToolResult`, as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

impl Reply {
    fn refused(e: impl Into<String>) -> Reply {
        Reply { ok: false, error: Some(e.into()), result: None }
    }
}

/// A launch's key: 256 random bits as 64 hex digits (two v4 UUIDs: the OS's secure generator).
pub fn new_key() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

/// Equal in time independent of where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A request line checked against the launch's key: its operation, or what to answer.
pub fn authorize(line: &str, key: &str) -> Result<Op, String> {
    let r: Request = serde_json::from_str(line.trim()).map_err(|_| "bad request".to_string())?;
    if r.v != VERSION {
        return Err(format!("unsupported version {}", r.v));
    }
    if key.len() < 32 || !same(r.key.as_bytes(), key.as_bytes()) {
        return Err("unauthorized".into());
    }
    Ok(r.op)
}

/// Writes `text` to a file only its owner can read or write.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    std::io::Write::write_all(&mut f, text.as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

fn owner(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.uid())
}

// ---------------------------------------------------------------------------------------------
// The app's side

/// The hub and tools a call runs with: made on the first connection, not at launch (an idle
/// app does nothing but wait on the socket).
pub type Provide = Arc<dyn Fn() -> anyhow::Result<(Arc<Hub>, Arc<Registry>)> + Send + Sync>;

/// The listening socket; dropping it (or [`IpcServer::stop`]) closes it and removes the socket
/// and the key.
pub struct IpcServer {
    path: PathBuf,
    key_file: PathBuf,
    stop: Arc<AtomicBool>,
}

impl IpcServer {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stop(&self) {
        if self.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = std::fs::remove_file(&self.key_file);
        // Wakes the accept so its thread sees the flag, then the socket goes.
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Listens on `app.sock` in `dirs` with a new key. Refuses to take over a socket another app
/// is listening on; a stale one (a crash) is replaced. Blocking (a bind): off the UI thread.
pub fn serve(dirs: &Dirs, provide: Provide) -> std::io::Result<IpcServer> {
    std::fs::create_dir_all(&dirs.config)?;
    let path = socket_path(dirs);
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "another Diskman is listening"));
    }
    let _ = std::fs::remove_file(&path);
    let key = new_key();
    let key_file = key_path(dirs);
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    write_private(&key_file, &key)?;
    let uid = owner(&path)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new().name("mcp-ipc".into()).spawn(move || {
        for s in listener.incoming() {
            if flag.load(Ordering::Acquire) {
                break;
            }
            let Ok(s) = s else { continue };
            let (hub, reg) = match provide() {
                Ok(x) => x,
                Err(e) => {
                    let mut s = s;
                    let line = serde_json::to_string(&Reply::refused(format!("{e:#}"))).unwrap_or_default();
                    let _ = std::io::Write::write_all(&mut s, format!("{line}\n").as_bytes());
                    continue;
                }
            };
            if s.set_nonblocking(true).is_err() {
                continue;
            }
            let key = key.clone();
            let rt = hub.handle();
            rt.spawn(async move {
                let Ok(s) = tokio::net::UnixStream::from_std(s) else { return };
                connection(s, hub, reg, &key, uid).await;
            });
        }
    })?;
    Ok(IpcServer { path, key_file, stop })
}

async fn write_reply(w: &mut (impl AsyncWriteExt + Unpin), r: &Reply) {
    let mut line = serde_json::to_string(r).unwrap_or_default();
    line.push('\n');
    let _ = w.write_all(line.as_bytes()).await;
    let _ = w.flush().await;
}

/// One client: its request, checked, run; its end closing cancels the call.
async fn connection(s: tokio::net::UnixStream, hub: Arc<Hub>, reg: Arc<Registry>, key: &str, uid: u32) {
    let peer = s.peer_cred().map(|c| c.uid()).ok();
    let (r, mut w) = s.into_split();
    if peer != Some(uid) {
        return write_reply(&mut w, &Reply::refused("unauthorized")).await;
    }
    let mut r = BufReader::new(r);
    let mut line = String::new();
    let read = tokio::time::timeout(READ_REQUEST, (&mut r).take(MAX_LINE).read_line(&mut line)).await;
    if !matches!(read, Ok(Ok(n)) if n > 0) {
        return write_reply(&mut w, &Reply::refused("bad request")).await;
    }
    let (tool, args, client) = match authorize(&line, key) {
        Err(e) => return write_reply(&mut w, &Reply::refused(e)).await,
        Ok(Op::Ping) => return write_reply(&mut w, &Reply { ok: true, ..Default::default() }).await,
        Ok(Op::Call { tool, args, client }) => (tool, args, client),
    };
    reg.refresh();
    let Some(t) = reg.find(&tool) else {
        return write_reply(&mut w, &Reply::refused(format!("unknown tool {tool:?}"))).await;
    };
    let server = Server::new(hub, reg, "stdio");
    let ct = CancellationToken::new();
    let run = server.run_tool(t, args, client, ct.clone());
    tokio::pin!(run);
    let eof = async {
        let mut buf = [0u8; 256];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    };
    let result = tokio::select! {
        res = &mut run => res,
        _ = eof => {
            ct.cancel();
            run.await
        }
    };
    let reply = Reply { ok: true, error: None, result: serde_json::to_value(&result).ok() };
    write_reply(&mut w, &reply).await;
}

// ---------------------------------------------------------------------------------------------
// The stdio binary's side

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// No app to reach (no socket, a stale one, no key): the call runs locally.
    Absent(String),
    /// The app answered with a refusal (a bad key, an unknown tool).
    Refused(String),
    /// The request went out and the answer did not come back: never run again locally.
    Lost(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Absent(e) => write!(f, "Diskman is not running ({e})"),
            CallError::Refused(e) => write!(f, "Diskman refused the call: {e}"),
            CallError::Lost(e) => write!(f, "Diskman did not answer: {e}"),
        }
    }
}

/// Connects to the app with its key: the stream and the key, or why not (always
/// [`CallError::Absent`]).
async fn connect(dirs: &Dirs) -> Result<(tokio::net::UnixStream, String), CallError> {
    let path = socket_path(dirs);
    let absent = |e: String| CallError::Absent(e);
    let key = std::fs::read_to_string(key_path(dirs)).map_err(|e| absent(format!("no key: {e}")))?.trim().to_string();
    // The socket must be this user's (the config directory's owner).
    if owner(&path).map_err(|e| absent(e.to_string()))? != owner(&dirs.config).map_err(|e| absent(e.to_string()))? {
        return Err(absent("the socket is someone else's".into()));
    }
    let s = match tokio::time::timeout(Duration::from_secs(2), tokio::net::UnixStream::connect(&path)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(absent(e.to_string())),
        Err(_) => return Err(absent("timed out".into())),
    };
    Ok((s, key))
}

async fn request(dirs: &Dirs, op: Op, cancel: CancellationToken) -> Result<Reply, CallError> {
    let (s, key) = connect(dirs).await?;
    let (r, mut w) = s.into_split();
    let mut line = serde_json::to_string(&Request { v: VERSION, key, op }).map_err(|e| CallError::Absent(e.to_string()))?;
    line.push('\n');
    if let Err(e) = w.write_all(line.as_bytes()).await {
        return Err(CallError::Absent(e.to_string()));
    }
    let _ = w.flush().await;
    let mut r = BufReader::new(r);
    let mut back = String::new();
    let read = tokio::select! {
        n = r.read_line(&mut back) => n,
        // Dropping the stream on return tells the app to cancel.
        _ = cancel.cancelled() => return Err(CallError::Lost("cancelled".into())),
    };
    match read {
        Ok(n) if n > 0 => {}
        Ok(_) => return Err(CallError::Lost("the app closed the connection".into())),
        Err(e) => return Err(CallError::Lost(e.to_string())),
    }
    let reply: Reply = serde_json::from_str(back.trim()).map_err(|e| CallError::Lost(format!("bad reply: {e}")))?;
    if !reply.ok {
        return Err(CallError::Refused(reply.error.unwrap_or_default()));
    }
    Ok(reply)
}

/// Whether the app answers (with this launch's key).
pub async fn ping(dirs: &Dirs) -> Result<(), CallError> {
    request(dirs, Op::Ping, CancellationToken::new()).await.map(|_| ())
}

/// Runs a tool call in the app.
pub async fn call(dirs: &Dirs, tool: &str, args: Map<String, Value>, client: &str, cancel: CancellationToken) -> Result<CallToolResult, CallError> {
    let reply = request(dirs, Op::Call { tool: tool.to_string(), args, client: client.to_string() }, cancel).await?;
    let v = reply.result.ok_or_else(|| CallError::Lost("no result".into()))?;
    serde_json::from_value(v).map_err(|e| CallError::Lost(format!("bad result: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_roundtrip() {
        let r = Request { v: 1, key: "k".into(), op: Op::Call { tool: "execute".into(), args: json!({"sql": "x"}).as_object().unwrap().clone(), client: "c".into() } };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&s).unwrap(), json!({"v": 1, "key": "k", "op": "call", "tool": "execute", "args": {"sql": "x"}, "client": "c"}));
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        let p: Request = serde_json::from_str(r#"{"v":1,"key":"k","op":"ping"}"#).unwrap();
        assert_eq!(p.op, Op::Ping);
        let ok = Reply { ok: true, error: None, result: Some(json!({"content": []})) };
        assert_eq!(serde_json::from_str::<Reply>(&serde_json::to_string(&ok).unwrap()).unwrap(), ok);
        assert_eq!(serde_json::to_string(&Reply::refused("unauthorized")).unwrap(), r#"{"ok":false,"error":"unauthorized"}"#);
    }

    #[test]
    fn authorization() {
        let key = new_key();
        assert!(key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()) && key != new_key());
        let line =|k: &str, v: u32| serde_json::to_string(&Request { v, key: k.into(), op: Op::Ping }).unwrap();
        assert_eq!(authorize(&line(&key, 1), &key), Ok(Op::Ping));
        assert_eq!(authorize(&line("wrong", 1), &key).unwrap_err(), "unauthorized");
        assert_eq!(authorize(&line(&key[..63], 1), &key).unwrap_err(), "unauthorized");
        assert_eq!(authorize(&line("", 1), "").unwrap_err(), "unauthorized", "an empty key never matches");
        assert!(authorize(&line(&key, 2), &key).unwrap_err().contains("version"));
        assert_eq!(authorize("not json", &key).unwrap_err(), "bad request");
        assert_eq!(authorize(r#"{"v":1,"op":"ping"}"#, &key).unwrap_err(), "bad request", "no key at all");
    }

    /// The socket end to end: 0600 files, the key, a call run by the app's hub, a stale socket
    /// replaced, a live one never taken over, everything gone after stop.
    #[test]
    fn socket_end_to_end() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("diskman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.connections_file(), "[[connection]]\nid = \"a\"\nname = \"alpha\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n").unwrap();
        // A stale socket file (a crashed app): replaced.
        drop(std::os::unix::net::UnixListener::bind(socket_path(&dirs)).unwrap());
        let hub = Hub::new(dirs.clone(), Arc::new(diskman_core::MemorySecrets::default())).unwrap();
        let reg = Arc::new(Registry::standard(None));
        let (h, r) = (hub.clone(), reg.clone());
        let srv = serve(&dirs, Arc::new(move || Ok((h.clone(), r.clone())))).unwrap();
        for p in [socket_path(&dirs), key_path(&dirs)] {
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "{}", p.display());
        }
        assert!(serve(&dirs, Arc::new(|| anyhow::bail!("unused"))).is_err(), "a listening app is never taken over");
        let rt = hub.handle();
        rt.block_on(async {
            ping(&dirs).await.unwrap();
            let r = call(&dirs, "list_connections", Map::new(), "ipc-test", CancellationToken::new()).await.unwrap();
            let text = match &r.content[0] {
                rmcp::model::ContentBlock::Text(t) => t.text.clone(),
                other => panic!("{other:?}"),
            };
            assert!(text.contains("alpha") && r.is_error != Some(true), "{text}");
            // The wrong key, by hand.
            let s = tokio::net::UnixStream::connect(socket_path(&dirs)).await.unwrap();
            let (rd, mut w) = s.into_split();
            w.write_all(b"{\"v\":1,\"key\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"op\":\"ping\"}\n").await.unwrap();
            let mut back = String::new();
            BufReader::new(rd).read_line(&mut back).await.unwrap();
            assert_eq!(back.trim(), r#"{"ok":false,"error":"unauthorized"}"#);
            let err = call(&dirs, "nope", Map::new(), "ipc-test", CancellationToken::new()).await.unwrap_err();
            assert!(matches!(&err, CallError::Refused(e) if e.contains("unknown tool")), "{err:?}");
        });
        // The audit line is the app's, with the client's name.
        let log = std::fs::read_to_string(dirs.mcp_audit_file()).unwrap();
        assert!(log.contains("\"client\":\"ipc-test\"") && log.contains("\"tool\":\"list_connections\""), "{log}");
        srv.stop();
        assert!(!socket_path(&dirs).exists() && !key_path(&dirs).exists());
        let err = rt.block_on(ping(&dirs)).unwrap_err();
        assert!(matches!(err, CallError::Absent(_)), "{err:?}");
        std::mem::forget(hub);
    }
}
