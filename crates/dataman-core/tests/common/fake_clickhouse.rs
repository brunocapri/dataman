//! A server that answers as ClickHouse's HTTP port does, for what a client does with an answer.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

const MIB: u64 = 1024 * 1024;
/// How long an endless answer really is: past every cap, short of filling this machine.
pub const ENDLESS: u64 = 1600 * MIB;

pub enum Reply {
    Rows(&'static str),
    /// Its start, then `x` for as long as the client reads.
    Endless { status: u16, start: &'static str },
    /// A length it does not keep to: the connection closes in the middle.
    Short(&'static str),
    /// The connection closes with no answer.
    Hangup,
}

#[derive(Default)]
pub struct Wire {
    /// What arrived, as it arrived: decrypted on a TLS server, whatever it was on a plain one.
    pub bytes: Mutex<Vec<u8>>,
    pub requests: Mutex<Vec<String>>,
    pub sent: AtomicU64,
}

impl Wire {
    pub fn holds(&self, what: &[u8]) -> bool {
        self.bytes.lock().windows(what.len()).any(|w| w == what)
    }

    pub fn got_any(&self) -> bool {
        !self.bytes.lock().is_empty()
    }

    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn asked(&self, what: &str) -> bool {
        self.requests.lock().iter().any(|r| r.contains(what))
    }
}

pub type Answer = Arc<dyn Fn(&str) -> Reply + Send + Sync>;

/// The first row is what a connection asks for first: `getSetting('readonly')` reads 0.
pub fn plain_answers(_: &str) -> Reply {
    Reply::Rows("a\nString\n0\n")
}

/// Reads one request. What is no HTTP is answered as a plain port answers TLS.
fn read_request<S: Read + Write>(s: &mut S, wire: &Wire) -> Option<String> {
    let mut got = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = s.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        wire.bytes.lock().extend_from_slice(&buf[..n]);
        got.extend_from_slice(&buf[..n]);
        if !got[0].is_ascii_uppercase() {
            let _ = s.write_all(b"HTTP/1.0 400 Bad Request\r\nConnection: close\r\n\r\n");
            return None;
        }
        let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
        let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        if got.len() >= end + 4 + length {
            return Some(String::from_utf8_lossy(&got).into_owned());
        }
    }
}

pub fn serve<S: Read + Write>(mut s: S, wire: &Wire, answer: &Answer) {
    let Some(req) = read_request(&mut s, wire) else { return };
    wire.requests.lock().push(req.clone());
    let head = |status: u16| format!("HTTP/1.1 {status} X\r\nConnection: close\r\n{}", if status == 200 { "" } else { "X-ClickHouse-Exception-Code: 1\r\n" });
    match answer(&req) {
        Reply::Hangup => {}
        Reply::Short(body) => {
            let _ = s.write_all(format!("{}Content-Length: {}\r\n\r\n{body}", head(200), body.len() + 1000).as_bytes());
        }
        Reply::Rows(body) => {
            // A length: a TLS server here closes with no closing word, which ends no body.
            let _ = s.write_all(format!("{}Content-Length: {}\r\n\r\n{body}", head(200), body.len()).as_bytes());
        }
        Reply::Endless { status, start } => {
            if s.write_all(format!("{}\r\n{start}", head(status)).as_bytes()).is_err() {
                return;
            }
            let piece = vec![b'x'; 256 * 1024];
            while wire.sent() < ENDLESS {
                if s.write_all(&piece).is_err() {
                    return;
                }
                wire.sent.fetch_add(piece.len() as u64, Ordering::Relaxed);
            }
        }
    }
}

pub fn listen(on_each: impl Fn(TcpStream) + Send + Sync + 'static) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let on_each = Arc::new(on_each);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let f = on_each.clone();
            std::thread::spawn(move || f(s));
        }
    });
    port
}

pub fn plain_server(answer: impl Fn(&str) -> Reply + Send + Sync + 'static) -> (u16, Arc<Wire>) {
    let wire = Arc::new(Wire::default());
    let w = wire.clone();
    let answer: Answer = Arc::new(answer);
    (listen(move |s| serve(s, &w, &answer)), wire)
}

