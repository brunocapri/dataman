//! Databases that run inside this process (SQLite, DuckDB). Their drivers block: each connection
//! lives on a thread of its own, which runs the jobs the async side sends it, one at a time, and
//! streams rows back through a channel as the MySQL reader does.
// A build with neither file engine keeps the module, unused.
#![cfg_attr(not(any(feature = "sqlite", feature = "duckdb")), allow(dead_code))]

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot};

use super::RowSource;
use crate::result::{ChunkBuilder, ColumnInfo, NamedOrigin, ResultSet};
use crate::value::{Kind, Storage};

pub(crate) const IN_FLIGHT: usize = 1024;

/// A cell as the engine stores it, before it meets its column's kind.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Null,
    #[cfg_attr(not(feature = "duckdb"), allow(dead_code))]
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Val {
    pub(crate) fn text(&self) -> Option<String> {
        let mut s = String::new();
        match self {
            Val::Null => return None,
            Val::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
            Val::Int(v) => s = v.to_string(),
            Val::Float(f) => crate::value::format_float(*f, false, &mut s),
            Val::Text(t) => s.push_str(t),
            Val::Blob(b) => hex(b, &mut s),
        }
        Some(s)
    }
}

/// Bytes as PostgreSQL shows `bytea`: `\x` and hex.
pub(crate) fn hex(b: &[u8], out: &mut String) {
    use std::fmt::Write;
    out.push_str("\\x");
    for x in b {
        let _ = write!(out, "{x:02x}");
    }
}

/// Stores `v` in its column. False when the cell does not fit the column's kind (a text in an
/// INTEGER column of SQLite): the caller turns the column to text and stores it again.
pub(crate) fn push(b: &mut ChunkBuilder, i: usize, v: &Val) -> bool {
    let kind = b.kind(i);
    match (kind.storage(), v) {
        (_, Val::Null) => b.push_null(i),
        (Storage::Bool, Val::Bool(x)) => b.push_bool(i, *x),
        (Storage::Bool, Val::Int(x @ (0 | 1))) => b.push_bool(i, *x == 1),
        (Storage::I64, Val::Int(x)) => b.push_i64(i, *x),
        (Storage::F64, Val::Float(x)) => b.push_f64(i, *x),
        // Exact in a double: shown as the same number.
        (Storage::F64, Val::Int(x)) if x.unsigned_abs() < 1 << 53 => b.push_f64(i, *x as f64),
        (Storage::Text, Val::Blob(x)) => b.push_text_with(i, |t| hex(x, t)),
        // A text where bytes are expected would read as hex that is not there.
        (Storage::Text, Val::Text(_)) if kind == Kind::Bytes => return false,
        (Storage::Text, v) => {
            let s = v.text().unwrap_or_default();
            b.push_text(i, &s);
        }
        _ => return false,
    }
    true
}

type Job<C> = Box<dyn FnOnce(&mut C) + Send>;

/// A connection on its own thread. Dropped, the thread closes the connection and ends.
pub(crate) struct Worker<C> {
    tx: std::sync::mpsc::Sender<Job<C>>,
    closed: Arc<AtomicBool>,
}

impl<C: 'static> Worker<C> {
    /// Opens the connection on the new thread (a file may take a while to open, or to wait for a
    /// lock): the runtime is not held meanwhile.
    pub(crate) async fn start(name: &str, open: impl FnOnce() -> Result<C> + Send + 'static) -> Result<Worker<C>> {
        let (tx, rx) = std::sync::mpsc::channel::<Job<C>>();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
        let closed = Arc::new(AtomicBool::new(false));
        let done = closed.clone();
        std::thread::Builder::new().name(name.into()).spawn(move || {
            let mut conn = match open() {
                Ok(c) => c,
                Err(e) => {
                    done.store(true, Ordering::Release);
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            while let Ok(job) = rx.recv() {
                if std::panic::catch_unwind(AssertUnwindSafe(|| job(&mut conn))).is_err() {
                    log::error!("a database thread's job panicked: its connection is closed");
                    break;
                }
            }
            done.store(true, Ordering::Release);
        })?;
        ready_rx.await.map_err(|_| anyhow!("the database thread ended before opening"))??;
        Ok(Worker { tx, closed })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// None: the thread has ended (the connection is gone).
    pub(crate) async fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut C) -> R + Send + 'static) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Box::new(move |c| {
                let _ = tx.send(f(c));
            }))
            .ok()?;
        rx.await.ok()
    }

    /// Sends a job without waiting for it: a stream answers through its own channels.
    pub(crate) fn post(&self, f: impl FnOnce(&mut C) + Send + 'static) -> bool {
        self.tx.send(Box::new(f)).is_ok()
    }
}

pub(crate) type Columns = (Vec<ColumnInfo>, Vec<Option<NamedOrigin>>);

pub(crate) enum Msg<E> {
    Row(Vec<Val>),
    /// The next statement's result set.
    Set(Columns),
    Done(Option<u64>),
    Fail(E),
}

/// What a streaming job reports through: the first result's columns once, then rows.
pub(crate) struct Sink<E> {
    first: Option<oneshot::Sender<Result<Columns, E>>>,
    tx: mpsc::Sender<Msg<E>>,
    open: bool,
}

impl<E> Sink<E> {
    pub(crate) fn new() -> (Sink<E>, oneshot::Receiver<Result<Columns, E>>, mpsc::Receiver<Msg<E>>) {
        let (first_tx, first_rx) = oneshot::channel();
        let (tx, rx) = mpsc::channel(IN_FLIGHT);
        (Sink { first: Some(first_tx), tx, open: true }, first_rx, rx)
    }

    pub(crate) fn columns(&mut self, cols: Columns) {
        match self.first.take() {
            Some(f) => {
                let _ = f.send(Ok(cols));
            }
            None => self.send(Msg::Set(cols)),
        }
    }

    /// False once the reader is gone: the job stops reading.
    pub(crate) fn row(&mut self, row: Vec<Val>) -> bool {
        self.send(Msg::Row(row));
        self.open
    }

    pub(crate) fn end(mut self, r: Result<Option<u64>, E>) {
        match r {
            Ok(n) => {
                if let Some(f) = self.first.take() {
                    let _ = f.send(Ok(Default::default()));
                }
                self.send(Msg::Done(n));
            }
            Err(e) => match self.first.take() {
                Some(f) => {
                    let _ = f.send(Err(e));
                }
                None => self.send(Msg::Fail(e)),
            },
        }
    }

    fn send(&mut self, m: Msg<E>) {
        if self.open && self.tx.blocking_send(m).is_err() {
            self.open = false;
        }
    }
}

pub(crate) struct Rows<E> {
    rx: mpsc::Receiver<Msg<E>>,
    affected: Option<u64>,
    ended: bool,
    next_set: Option<Columns>,
    /// The result shown, whose column turns to text with the builder's.
    rs: Option<Arc<ResultSet>>,
    closed: fn() -> E,
}

impl<E> Rows<E> {
    pub(crate) fn new(rx: mpsc::Receiver<Msg<E>>, rs: Option<Arc<ResultSet>>, closed: fn() -> E) -> Rows<E> {
        Rows { rx, affected: None, ended: false, next_set: None, rs, closed }
    }

    pub(crate) fn next_set(&mut self) -> Option<Columns> {
        self.next_set.take()
    }

    pub(crate) fn show_in(&mut self, rs: &Arc<ResultSet>) {
        self.rs = Some(rs.clone());
    }

    async fn next_msg(&mut self) -> Result<Option<Vec<Val>>, E> {
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
                self.affected = n;
                Ok(None)
            }
            Some(Msg::Fail(e)) => {
                self.ended = true;
                Err(e)
            }
            None => {
                self.ended = true;
                Err((self.closed)())
            }
        }
    }
}

impl<E: Send> RowSource for Rows<E> {
    type Error = E;

    async fn next_into(&mut self, b: &mut ChunkBuilder) -> Result<bool, E> {
        let Some(row) = self.next_msg().await? else { return Ok(false) };
        for (i, v) in row.iter().enumerate() {
            if !push(b, i, v) {
                b.demote_to_text(i);
                if let Some(rs) = &self.rs {
                    rs.demote_to_text(i);
                }
                push(b, i, v);
            }
        }
        b.end_row();
        Ok(true)
    }

    async fn skip(&mut self) -> Result<bool, E> {
        Ok(self.next_msg().await?.is_some())
    }

    fn affected(&self) -> Option<u64> {
        self.affected
    }
}

/// A parameter's text, bound as the number it spells exactly (`LIMIT ?` takes no text in
/// SQLite), else as text: `007` stays text.
pub(crate) fn param_val(v: &Option<String>) -> Val {
    match v {
        None => Val::Null,
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n.to_string() == *s => Val::Int(n),
            _ => Val::Text(s.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(kind: Kind) -> ColumnInfo {
        ColumnInfo { name: "c".into(), type_name: String::new(), type_oid: 0, kind }
    }

    #[test]
    fn cells_that_do_not_fit_are_refused() {
        let mut b = ChunkBuilder::new(&[col(Kind::Int), col(Kind::Float), col(Kind::Bool), col(Kind::Bytes), col(Kind::Text)]);
        assert!(push(&mut b, 0, &Val::Int(3)) && !push(&mut b, 0, &Val::Text("abc".into())) && !push(&mut b, 0, &Val::Float(1.5)));
        assert!(push(&mut b, 1, &Val::Int(2)) && !push(&mut b, 1, &Val::Int(1 << 60)));
        assert!(push(&mut b, 2, &Val::Int(1)) && !push(&mut b, 2, &Val::Int(7)));
        assert!(push(&mut b, 3, &Val::Blob(vec![0, 255])) && !push(&mut b, 3, &Val::Text("x".into())));
        assert!(push(&mut b, 4, &Val::Float(0.5)) && push(&mut b, 4, &Val::Blob(vec![1])));
        assert_eq!(param_val(&Some("10".into())), Val::Int(10));
        assert_eq!(param_val(&Some("007".into())), Val::Text("007".into()));
        assert_eq!(param_val(&Some("1.5".into())), Val::Text("1.5".into()));
        assert_eq!(Val::Blob(vec![0, 171]).text().as_deref(), Some("\\x00ab"));
    }
}
