//! Live queries on LAR, each on its own dedicated connection (they only work outside
//! transaction blocks).
//!
//! - [`Lar::listen`]: `LISTEN ch AS <query>`. Every notification carries the *whole* result as
//!   JSON, `{"rows":[{…}],"ts":N}` (the first one right after LISTEN). The connection task now
//!   polls its messages, so notifications reach a channel; [`LiveListen`] keeps the latest result
//!   and a log of notifications.
//! - [`Lar::subscribe`]: `SUBSCRIBE <query>`, a COPY that never ends: text lines
//!   `<ts>\t<diff>\t<col1>\t…`. The first batch is the full result (diff = multiplicity), later
//!   ones are multiset deltas, removals before additions; an UPDATE is `-1 old` and `+1 new` with
//!   the same ts, paired into "changed" when a key is known. [`Subscription`] maintains the live
//!   rows (with each row's last change, for highlight fading), counters, pause (keeps reading
//!   and buffering) / resume, and stop (CancelRequest; LAR answers 57014, which is the normal end).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
use tokio::sync::{oneshot, watch};

use super::Lar;
use crate::Task;
use crate::result::ErrorInfo;
use crate::sqlgen::quote_ident;

// ---------------------------------------------------------------------------------------------
// LISTEN … AS

/// One notification's result: column names in the order of the first row's keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LivePayload {
    pub ts: u64,
    pub columns: Vec<String>,
    /// Values as JSON: numbers, strings (timestamps RFC 3339, bytea `\x…`, vectors `[…]`),
    /// booleans, JSON values for json columns, null.
    pub rows: Vec<Vec<serde_json::Value>>,
}

impl LivePayload {
    /// Puts the columns in `order` (LAR serializes each row with its keys sorted, so the
    /// payload alone loses the query's column order). Unknown columns go last.
    pub fn reorder(&mut self, order: &[String]) {
        let mut idx: Vec<usize> = order.iter().filter_map(|n| self.columns.iter().position(|c| c == n)).collect();
        for i in 0..self.columns.len() {
            if !idx.contains(&i) {
                idx.push(i);
            }
        }
        self.columns = idx.iter().map(|&i| self.columns[i].clone()).collect();
        for r in self.rows.iter_mut() {
            *r = idx.iter().map(|&i| r.get(i).cloned().unwrap_or(serde_json::Value::Null)).collect();
        }
    }

    pub fn text(&self, row: usize, col: usize) -> Option<String> {
        self.rows.get(row).and_then(|r| r.get(col)).and_then(json_text)
    }
}

/// How a JSON value is shown in a cell (`None` = NULL).
pub fn json_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// A JSON object with its keys in document order.
struct Ordered(Vec<(String, serde_json::Value)>);

impl<'de> serde::Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Ordered, A::Error> {
                let mut v = Vec::new();
                while let Some((k, x)) = m.next_entry::<String, serde_json::Value>()? {
                    v.push((k, x));
                }
                Ok(Ordered(v))
            }
        }
        d.deserialize_map(V)
    }
}

struct Rows(Vec<Ordered>);

impl<'de> serde::Deserialize<'de> for Rows {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Rows;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of objects")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Rows, A::Error> {
                let mut v = Vec::new();
                while let Some(o) = s.next_element::<Ordered>()? {
                    v.push(o);
                }
                Ok(Rows(v))
            }
        }
        d.deserialize_seq(V)
    }
}

/// Parses a `LISTEN … AS` payload, `{"rows":[{…}],"ts":N}`.
pub fn parse_payload(s: &str) -> Result<LivePayload> {
    #[derive(serde::Deserialize)]
    struct P {
        rows: Rows,
        #[serde(default)]
        ts: u64,
    }
    let p: P = serde_json::from_str(s).map_err(|e| anyhow!("not a live result: {e}"))?;
    let mut columns: Vec<String> = Vec::new();
    for r in &p.rows.0 {
        for (k, _) in &r.0 {
            if !columns.contains(k) {
                columns.push(k.clone());
            }
        }
    }
    let rows = p
        .rows
        .0
        .into_iter()
        .map(|r| {
            let mut out = vec![serde_json::Value::Null; columns.len()];
            for (k, v) in r.0 {
                if let Some(i) = columns.iter().position(|c| *c == k) {
                    out[i] = v;
                }
            }
            out
        })
        .collect();
    Ok(LivePayload { ts: p.ts, columns, rows })
}

#[derive(Clone, Debug, PartialEq)]
pub struct NotificationEntry {
    pub at: DateTime<Utc>,
    pub ts: Option<u64>,
    pub rows: usize,
    pub bytes: usize,
    /// The payload did not parse.
    pub error: Option<String>,
}

const LOG_CAP: usize = 1000;

#[derive(Default)]
struct ListenState {
    latest: Option<Arc<LivePayload>>,
    log: VecDeque<NotificationEntry>,
    received: u64,
    ended: Option<String>,
}

/// A running `LISTEN ch AS query`.
pub struct LiveListen {
    channel: String,
    state: Arc<Mutex<ListenState>>,
    version: watch::Receiver<u64>,
    stop: Mutex<Option<oneshot::Sender<oneshot::Sender<()>>>>,
    rt: tokio::runtime::Handle,
}

impl LiveListen {
    pub fn channel(&self) -> &str {
        &self.channel
    }
    /// The latest result received.
    pub fn latest(&self) -> Option<Arc<LivePayload>> {
        self.state.lock().latest.clone()
    }
    /// Notifications received, newest last (the last 1000).
    pub fn log(&self) -> Vec<NotificationEntry> {
        self.state.lock().log.iter().cloned().collect()
    }
    pub fn received(&self) -> u64 {
        self.state.lock().received
    }
    /// Why the listener stopped on its own (connection lost), if it did.
    pub fn ended(&self) -> Option<String> {
        self.state.lock().ended.clone()
    }
    /// Changes on every notification.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.version.clone()
    }
    /// `UNLISTEN` and closes the connection.
    pub fn unlisten(&self) -> Task<()> {
        let stop = self.stop.lock().take();
        crate::spawn(&self.rt, async move {
            if let Some(stop) = stop {
                let (tx, rx) = oneshot::channel();
                if stop.send(tx).is_ok() {
                    let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
                }
            }
            Ok(())
        })
    }
}

impl Drop for LiveListen {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.lock().take() {
            let (tx, _rx) = oneshot::channel();
            let _ = stop.send(tx);
        }
    }
}

impl Lar {
    /// `LISTEN channel AS query` on a dedicated connection.
    pub fn listen(&self, channel: &str, query: &str) -> Task<LiveListen> {
        let (conn, channel, query) = (self.conn.clone(), channel.to_string(), query.trim().trim_end_matches(';').to_string());
        let rt = self.conn.handle().clone();
        self.conn.spawn(async move {
            let mut d = conn.dedicated(None).await?.into_pg()?;
            // The column order, which the payload does not keep.
            let order: Vec<String> = match d.client.prepare(&query).await {
                Ok(stmt) => stmt.columns().iter().map(|c| c.name().to_string()).collect(),
                Err(_) => vec![],
            };
            let ch = quote_ident(&channel);
            d.client
                .simple_query(&format!("LISTEN {ch} AS {query}"))
                .await
                .map_err(|e| anyhow!(crate::backend::pg::error_info(&e).to_string()))?;
            let state = Arc::new(Mutex::new(ListenState::default()));
            let (vtx, vrx) = watch::channel(0u64);
            let (stop_tx, mut stop_rx) = oneshot::channel::<oneshot::Sender<()>>();
            let st = state.clone();
            let name = channel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        n = d.notifications.recv() => {
                            let Some(n) = n else {
                                st.lock().ended = Some("the connection closed".into());
                                vtx.send_modify(|v| *v += 1);
                                break;
                            };
                            if n.channel() != name {
                                continue;
                            }
                            let payload = n.payload();
                            let parsed = parse_payload(payload).map(|mut p| {
                                p.reorder(&order);
                                p
                            });
                            let mut g = st.lock();
                            g.received += 1;
                            let entry = match parsed {
                                Ok(p) => {
                                    let e = NotificationEntry { at: Utc::now(), ts: Some(p.ts), rows: p.rows.len(), bytes: payload.len(), error: None };
                                    g.latest = Some(Arc::new(p));
                                    e
                                }
                                Err(err) => NotificationEntry { at: Utc::now(), ts: None, rows: 0, bytes: payload.len(), error: Some(err.to_string()) },
                            };
                            if g.log.len() == LOG_CAP {
                                g.log.pop_front();
                            }
                            g.log.push_back(entry);
                            drop(g);
                            vtx.send_modify(|v| *v += 1);
                        }
                        done = &mut stop_rx => {
                            let _ = d.client.simple_query(&format!("UNLISTEN {}", quote_ident(&name))).await;
                            if let Ok(done) = done {
                                let _ = done.send(());
                            }
                            break;
                        }
                    }
                }
            });
            Ok(LiveListen { channel, state, version: vrx, stop: Mutex::new(Some(stop_tx)), rt })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// SUBSCRIBE: COPY text lines

/// Splits one COPY text line into fields: `\N` is NULL, backslash escapes decoded.
pub fn parse_copy_line(line: &[u8]) -> Vec<Option<String>> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut out = Vec::new();
    for field in line.split(|&b| b == b'\t') {
        if field == b"\\N" {
            out.push(None);
            continue;
        }
        let mut v: Vec<u8> = Vec::with_capacity(field.len());
        let mut i = 0;
        while i < field.len() {
            let c = field[i];
            if c != b'\\' || i + 1 >= field.len() {
                v.push(c);
                i += 1;
                continue;
            }
            let n = field[i + 1];
            i += 2;
            match n {
                b'b' => v.push(8),
                b'f' => v.push(12),
                b'n' => v.push(b'\n'),
                b'r' => v.push(b'\r'),
                b't' => v.push(b'\t'),
                b'v' => v.push(11),
                b'0'..=b'7' => {
                    let mut x = (n - b'0') as u32;
                    let mut k = 0;
                    while k < 2 && i < field.len() && (b'0'..=b'7').contains(&field[i]) {
                        x = x * 8 + (field[i] - b'0') as u32;
                        i += 1;
                        k += 1;
                    }
                    v.push(x as u8);
                }
                b'x' if i < field.len() && field[i].is_ascii_hexdigit() => {
                    let mut x = 0u32;
                    let mut k = 0;
                    while k < 2 && i < field.len() && field[i].is_ascii_hexdigit() {
                        x = x * 16 + (field[i] as char).to_digit(16).unwrap_or(0);
                        i += 1;
                        k += 1;
                    }
                    v.push(x as u8);
                }
                other => v.push(other),
            }
        }
        out.push(Some(String::from_utf8_lossy(&v).into_owned()));
    }
    out
}

/// One line of a subscription: a row added (`diff` > 0) or removed (`diff` < 0) at commit `ts`.
#[derive(Clone, Debug, PartialEq)]
pub struct Delta {
    pub ts: u64,
    pub diff: i64,
    pub values: Vec<Option<String>>,
}

pub fn parse_delta(line: &[u8]) -> Option<Delta> {
    let mut f = parse_copy_line(line);
    if f.len() < 2 {
        return None;
    }
    let values = f.split_off(2);
    let ts = f[0].as_deref()?.trim().parse().ok()?;
    let diff = f[1].as_deref()?.trim().parse().ok()?;
    Some(Delta { ts, diff, values })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Part of the first result.
    Initial,
    Inserted,
    /// Replaced a row with the same key; `changed` flags the columns that differ.
    Updated { changed: Vec<bool> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveRow {
    pub values: Vec<Option<String>>,
    /// How many identical rows this stands for (multisets).
    pub count: i64,
    /// The last change and when it happened (commit, and local time for fading highlights).
    pub kind: ChangeKind,
    pub ts: u64,
    pub at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemovedRow {
    pub values: Vec<Option<String>>,
    pub ts: u64,
    pub at: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchSummary {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// The current rows of a subscription.
#[derive(Debug, Default)]
pub struct LiveTable {
    pub columns: Vec<String>,
    rows: Vec<LiveRow>,
    index: HashMap<Vec<Option<String>>, usize>,
    key: Option<Vec<usize>>,
    /// Recently removed rows (newest last), for showing them fading out.
    pub removed: VecDeque<RemovedRow>,
    pub last_ts: u64,
    initial_done: bool,
}

const REMOVED_CAP: usize = 256;

impl LiveTable {
    pub fn new(columns: Vec<String>, key: Option<Vec<usize>>) -> Self {
        LiveTable { columns, key, ..Default::default() }
    }

    pub fn rows(&self) -> &[LiveRow] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn key(&self) -> Option<&[usize]> {
        self.key.as_deref()
    }

    fn key_of(&self, v: &[Option<String>]) -> Option<Vec<Option<String>>> {
        self.key.as_ref().map(|k| k.iter().map(|&i| v.get(i).cloned().flatten()).collect())
    }

    fn remove_at(&mut self, i: usize) -> LiveRow {
        let row = self.rows.swap_remove(i);
        self.index.remove(&row.values);
        if i < self.rows.len() {
            let moved = self.rows[i].values.clone();
            self.index.insert(moved, i);
        }
        row
    }

    /// Marks the initial result as complete even if it was empty (no lines ever came).
    pub fn mark_initial_done(&mut self) {
        self.initial_done = true;
    }

    /// Applies the deltas of one commit (`initial`: the first result).
    pub fn apply(&mut self, ts: u64, deltas: Vec<Delta>, now: Instant) -> BatchSummary {
        let mut s = BatchSummary::default();
        let initial = !self.initial_done;
        self.initial_done = true;
        self.last_ts = self.last_ts.max(ts);
        let (removals, additions): (Vec<Delta>, Vec<Delta>) = deltas.into_iter().partition(|d| d.diff < 0);
        // Rows fully removed in this batch, by key, to pair with additions (updates).
        let mut gone: Vec<Vec<Option<String>>> = Vec::new();
        for d in removals {
            let Some(&i) = self.index.get(&d.values) else { continue };
            self.rows[i].count += d.diff;
            if self.rows[i].count <= 0 {
                let row = self.remove_at(i);
                gone.push(row.values);
            }
        }
        for d in additions {
            if let Some(&i) = self.index.get(&d.values) {
                let r = &mut self.rows[i];
                r.count += d.diff;
                r.kind = if initial { ChangeKind::Initial } else { ChangeKind::Inserted };
                r.ts = ts;
                r.at = now;
                if !initial {
                    s.inserted += 1;
                }
                continue;
            }
            let kind = if initial {
                ChangeKind::Initial
            } else if let Some(k) = self.key_of(&d.values)
                && let Some(j) = gone.iter().position(|g| self.key_of(g).as_ref() == Some(&k))
            {
                let old = gone.swap_remove(j);
                s.updated += 1;
                ChangeKind::Updated { changed: old.iter().zip(&d.values).map(|(a, b)| a != b).collect() }
            } else {
                s.inserted += 1;
                ChangeKind::Inserted
            };
            self.index.insert(d.values.clone(), self.rows.len());
            self.rows.push(LiveRow { values: d.values, count: d.diff, kind, ts, at: now });
        }
        s.deleted = gone.len();
        for values in gone {
            if self.removed.len() == REMOVED_CAP {
                self.removed.pop_front();
            }
            self.removed.push_back(RemovedRow { values, ts, at: now });
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SubState {
    Starting,
    Live,
    Paused,
    /// Stopped by the user (57014 from the server), by an error, or by the server closing the
    /// stream (paused or not: see [`Subscription::paused_by_user`]).
    Ended { error: Option<ErrorInfo> },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubStats {
    /// Delta lines received since the start (the first result included).
    pub deltas: u64,
    /// Commits seen (batches).
    pub batches: u64,
    /// Deltas per second over the last second.
    pub deltas_per_s: f64,
    /// Deltas received while paused, not applied yet.
    pub buffered: usize,
    pub last_ts: u64,
    pub last: BatchSummary,
}

#[derive(Clone, Debug)]
pub struct SubscribeOptions {
    /// Key columns for pairing updates. `None`: `id` when the query returns it.
    pub key: Option<Vec<String>>,
    /// Use `COPY (SUBSCRIBE …) TO STDOUT` instead of `SUBSCRIBE …`.
    pub copy_form: bool,
}

impl Default for SubscribeOptions {
    fn default() -> Self {
        SubscribeOptions { key: None, copy_form: false }
    }
}

struct SubShared {
    table: Mutex<LiveTable>,
    pending: Mutex<VecDeque<(u64, Vec<Delta>)>>,
    paused: AtomicBool,
    stats: Mutex<SubStats>,
    rate: Mutex<VecDeque<(Instant, usize)>>,
    state: Mutex<SubState>,
    version: watch::Sender<u64>,
}

impl SubShared {
    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    fn deliver(&self, ts: u64, deltas: Vec<Delta>) {
        let n = deltas.len();
        {
            let mut st = self.stats.lock();
            st.deltas += n as u64;
            st.batches += 1;
            st.last_ts = st.last_ts.max(ts);
        }
        {
            let now = Instant::now();
            let mut r = self.rate.lock();
            r.push_back((now, n));
            while r.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(1)) {
                r.pop_front();
            }
        }
        // The pause flag is read and the batch applied (or buffered) under the table lock, the
        // lock `resume` holds while it drains the buffer and clears the flag: batches are applied
        // in commit order, whatever the timing of pause/resume.
        let mut table = self.table.lock();
        if self.paused.load(Ordering::Acquire) {
            self.pending.lock().push_back((ts, deltas));
            self.stats.lock().buffered += n;
        } else {
            let s = table.apply(ts, deltas, Instant::now());
            self.stats.lock().last = s;
        }
        drop(table);
        self.bump();
    }

    fn pause(&self) {
        let _table = self.table.lock();
        self.paused.store(true, Ordering::Release);
    }

    /// Applies the buffered batches, oldest first, then clears the pause flag, all under the
    /// table lock (see `deliver`).
    fn resume(&self) {
        let mut table = self.table.lock();
        let pending: Vec<(u64, Vec<Delta>)> = self.pending.lock().drain(..).collect();
        for (ts, d) in pending {
            let s = table.apply(ts, d, Instant::now());
            self.stats.lock().last = s;
        }
        self.paused.store(false, Ordering::Release);
        self.stats.lock().buffered = 0;
    }
}

/// A running `SUBSCRIBE`.
pub struct Subscription {
    shared: Arc<SubShared>,
    version: watch::Receiver<u64>,
    cancel: tokio_postgres::CancelToken,
    tls: crate::backend::pg::Tls,
    rt: tokio::runtime::Handle,
    pub sql: String,
}

impl Subscription {
    pub fn columns(&self) -> Vec<String> {
        self.shared.table.lock().columns.clone()
    }

    /// Reads the live rows under the lock (keep `f` short: the reader thread waits for it).
    pub fn with_table<R>(&self, f: impl FnOnce(&LiveTable) -> R) -> R {
        f(&self.shared.table.lock())
    }

    /// A copy of the current rows.
    pub fn rows(&self) -> Vec<LiveRow> {
        self.shared.table.lock().rows().to_vec()
    }

    pub fn stats(&self) -> SubStats {
        let mut s = self.shared.stats.lock().clone();
        let r = self.shared.rate.lock();
        let now = Instant::now();
        let n: usize = r.iter().filter(|(t, _)| now.duration_since(*t) <= Duration::from_secs(1)).map(|(_, n)| n).sum();
        s.deltas_per_s = n as f64;
        s
    }

    pub fn state(&self) -> SubState {
        self.shared.state.lock().clone()
    }

    /// Changes whenever rows, counters or the state change.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.version.clone()
    }

    /// Keeps receiving, but stops applying changes to the rows (they are buffered).
    pub fn pause(&self) {
        self.shared.pause();
        let mut st = self.shared.state.lock();
        if *st == SubState::Live {
            *st = SubState::Paused;
        }
        drop(st);
        self.shared.bump();
    }

    /// The user paused this subscription and has not resumed it. Unlike [`state`](Self::state),
    /// which says [`SubState::Ended`] once the stream ends, this stays true after the end: LAR
    /// currently ends an idle `SUBSCRIBE` about a second after the last change (it takes the
    /// Sync that tokio-postgres sends with COPY for the client leaving), and a UI that
    /// resubscribes automatically should call [`pause`](Self::pause) on the new subscription
    /// (once its first result is in) when this was true, so the user's pause survives. Batches
    /// buffered before the end stay buffered; [`resume`](Self::resume) still applies them.
    pub fn paused_by_user(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    /// Applies what was buffered while paused, then goes on live.
    pub fn resume(&self) {
        self.shared.resume();
        let mut st = self.shared.state.lock();
        if *st == SubState::Paused {
            *st = SubState::Live;
        }
        drop(st);
        self.shared.bump();
    }

    /// Ends the subscription (CancelRequest); resolves when the server confirmed it.
    pub fn stop(&self) -> Task<()> {
        let (token, tls, shared) = (self.cancel.clone(), self.tls.clone(), self.shared.clone());
        crate::spawn(&self.rt, async move {
            if matches!(*shared.state.lock(), SubState::Ended { .. }) {
                return Ok(());
            }
            crate::backend::pg::cancel(&token, &tls).await?;
            let mut rx = shared.version.subscribe();
            let wait = async {
                while !matches!(*shared.state.lock(), SubState::Ended { .. }) {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(10), wait).await;
            Ok(())
        })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if !matches!(*self.shared.state.lock(), SubState::Ended { .. }) {
            let (token, tls) = (self.cancel.clone(), self.tls.clone());
            self.rt.spawn(async move {
                let _ = crate::backend::pg::cancel(&token, &tls).await;
            });
        }
    }
}

/// The server's latest commit number (`lar_replication().last_ts`), read before a SUBSCRIBE
/// starts: a first delta past it means the initial result was empty.
async fn latest_commit(client: &tokio_postgres::Client) -> Result<u64> {
    // The function ignores a column list: ask for everything and pick the column.
    let (rows, _) = crate::backend::pg::text_query(client, "SELECT * FROM lar_replication()", None).await?;
    rows.get(0, "last_ts").and_then(|v| v.parse().ok()).ok_or_else(|| anyhow!("lar_replication() did not report last_ts"))
}

/// How long a partial batch waits for more lines of the same commit before it is applied.
const BATCH_IDLE: Duration = Duration::from_millis(5);

impl Lar {
    /// `SUBSCRIBE <query>` on a dedicated connection.
    pub fn subscribe(&self, query: &str, opts: SubscribeOptions) -> Task<Subscription> {
        let (conn, query) = (self.conn.clone(), query.trim().trim_end_matches(';').to_string());
        let rt = self.conn.handle().clone();
        self.conn.spawn(async move {
            let d = conn.dedicated(None).await?.into_pg()?;
            let columns: Vec<String> = match d.client.prepare(&query).await {
                Ok(stmt) => stmt.columns().iter().map(|c| c.name().to_string()).collect(),
                Err(e) => bail!("{}", crate::backend::pg::error_info(&e)),
            };
            let key = match &opts.key {
                Some(k) if k.is_empty() => None,
                Some(k) => Some(
                    k.iter()
                        .map(|n| columns.iter().position(|c| c == n).ok_or_else(|| anyhow!("key column {n} is not in the query")))
                        .collect::<Result<Vec<_>>>()?,
                ),
                None => columns.iter().position(|c| c == "id").map(|i| vec![i]),
            };
            let before = latest_commit(&d.client).await.unwrap_or(0);
            let sql = if opts.copy_form { format!("COPY (SUBSCRIBE {query}) TO STDOUT") } else { format!("SUBSCRIBE {query}") };
            let stream = d.client.copy_out(sql.as_str()).await.map_err(|e| anyhow!("{}", crate::backend::pg::error_info(&e)))?;
            let (vtx, vrx) = watch::channel(0u64);
            let shared = Arc::new(SubShared {
                table: Mutex::new(LiveTable::new(columns, key)),
                pending: Mutex::new(VecDeque::new()),
                paused: AtomicBool::new(false),
                stats: Mutex::new(SubStats::default()),
                rate: Mutex::new(VecDeque::new()),
                state: Mutex::new(SubState::Live),
                version: vtx,
            });
            let sh = shared.clone();
            let cancel = d.client.cancel_token();
            let tls = d.tls.clone();
            tokio::spawn(async move {
                // The client must live as long as the stream.
                let _client = d;
                futures_util::pin_mut!(stream);
                let mut batch: Vec<Delta> = Vec::new();
                let mut batch_ts = 0u64;
                let mut first = true;
                let end = loop {
                    let next = if batch.is_empty() {
                        stream.as_mut().next().await
                    } else {
                        match tokio::time::timeout(BATCH_IDLE, stream.as_mut().next()).await {
                            Ok(n) => n,
                            Err(_) => {
                                sh.deliver(batch_ts, std::mem::take(&mut batch));
                                continue;
                            }
                        }
                    };
                    match next {
                        Some(Ok(bytes)) => {
                            // A CopyData may hold one line or several.
                            for line in bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
                                let Some(delta) = parse_delta(line) else { continue };
                                if first {
                                    first = false;
                                    // The first commit after the start, not the initial result:
                                    // the initial result was empty.
                                    if delta.ts > before && before > 0 {
                                        sh.table.lock().mark_initial_done();
                                    }
                                }
                                if !batch.is_empty() && delta.ts != batch_ts {
                                    sh.deliver(batch_ts, std::mem::take(&mut batch));
                                }
                                batch_ts = delta.ts;
                                batch.push(delta);
                            }
                        }
                        Some(Err(e)) => {
                            let info = crate::backend::pg::error_info(&e);
                            break if info.code.as_deref() == Some("57014") { None } else { Some(info) };
                        }
                        None => break None,
                    }
                };
                if !batch.is_empty() {
                    sh.deliver(batch_ts, batch);
                }
                // `Ended` replaces `Paused` too; whether the user had paused is kept in the pause
                // flag (`Subscription::paused_by_user`), for a resubscribe to restore it.
                *sh.state.lock() = SubState::Ended { error: end };
                sh.bump();
            });
            Ok(Subscription { shared, version: vrx, cancel, tls, rt, sql })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_keep_column_order() {
        let p = parse_payload(r#"{"rows":[{"z":1,"a":"x","t":"2026-01-01T00:00:00Z","j":{"k":[1]}},{"z":2.5,"a":null}],"ts":42}"#).unwrap();
        assert_eq!(p.ts, 42);
        assert_eq!(p.columns, vec!["z", "a", "t", "j"]);
        assert_eq!(p.text(0, 0).as_deref(), Some("1"));
        assert_eq!(p.text(1, 0).as_deref(), Some("2.5"));
        assert_eq!(p.text(1, 1), None);
        assert_eq!(p.text(1, 3), None);
        assert_eq!(p.text(0, 3).as_deref(), Some("{\"k\":[1]}"));
        let mut q = p.clone();
        q.reorder(&["a".into(), "j".into(), "z".into()]);
        assert_eq!(q.columns, vec!["a", "j", "z", "t"]);
        assert_eq!(q.text(0, 2).as_deref(), Some("1"));
        let e = parse_payload(r#"{"rows":[],"ts":7}"#).unwrap();
        assert!(e.rows.is_empty() && e.columns.is_empty());
        assert!(parse_payload("hello").is_err());
    }

    #[test]
    fn copy_lines() {
        assert_eq!(
            parse_copy_line(b"5\t1\t\\N\ta\\tb\\\\c\\nd\t\\101\\x42\tt\n"),
            vec![Some("5".into()), Some("1".into()), None, Some("a\tb\\c\nd".into()), Some("AB".into()), Some("t".into())]
        );
        let d = parse_delta(b"12\t-1\t3\tx").unwrap();
        assert_eq!(d, Delta { ts: 12, diff: -1, values: vec![Some("3".into()), Some("x".into())] });
        assert_eq!(parse_delta(b"garbage"), None);
    }

    fn d(ts: u64, diff: i64, v: &[&str]) -> Delta {
        Delta { ts, diff, values: v.iter().map(|s| if *s == "-" { None } else { Some(s.to_string()) }).collect() }
    }

    #[test]
    fn pause_resume_races_keep_commit_order() {
        // One thread delivers "insert row i" then "delete row i" for many i while another pauses
        // and resumes as fast as it can. If a buffered insert were applied after the delete that
        // followed it, a ghost row would remain.
        let sh = Arc::new(SubShared {
            table: Mutex::new(LiveTable::new(vec!["id".into()], Some(vec![0]))),
            pending: Mutex::new(VecDeque::new()),
            paused: AtomicBool::new(false),
            stats: Mutex::new(SubStats::default()),
            rate: Mutex::new(VecDeque::new()),
            state: Mutex::new(SubState::Live),
            version: watch::channel(0u64).0,
        });
        sh.table.lock().mark_initial_done();
        let done = Arc::new(AtomicBool::new(false));
        let toggler = {
            let (sh, done) = (sh.clone(), done.clone());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !done.load(Ordering::Acquire) {
                    sh.pause();
                    std::hint::spin_loop();
                    sh.resume();
                    n += 1;
                }
                n
            })
        };
        for i in 0..50_000u64 {
            let id = i.to_string();
            sh.deliver(2 * i, vec![d(2 * i, 1, &[&id])]);
            sh.deliver(2 * i + 1, vec![d(2 * i + 1, -1, &[&id])]);
        }
        done.store(true, Ordering::Release);
        let toggles = toggler.join().unwrap();
        sh.resume();
        assert!(toggles > 0);
        let t = sh.table.lock();
        assert!(t.is_empty(), "{} ghost rows after {toggles} pause/resume cycles", t.len());
        assert_eq!(t.last_ts, 99_999);
        assert_eq!(sh.stats.lock().buffered, 0);
    }

    #[test]
    fn live_table_applies_deltas() {
        let now = Instant::now();
        let mut t = LiveTable::new(vec!["id".into(), "v".into()], Some(vec![0]));
        let s = t.apply(5, vec![d(5, 1, &["1", "a"]), d(5, 1, &["2", "b"]), d(5, 2, &["3", "c"])], now);
        assert_eq!(s, BatchSummary::default());
        assert_eq!(t.len(), 3);
        assert!(t.rows().iter().all(|r| r.kind == ChangeKind::Initial));
        // An update (removal + addition with the same key), a delete and an insert.
        let s = t.apply(6, vec![d(6, -1, &["1", "a"]), d(6, -1, &["2", "b"]), d(6, 1, &["1", "A"]), d(6, 1, &["4", "d"])], now);
        assert_eq!(s, BatchSummary { inserted: 1, updated: 1, deleted: 1 });
        assert_eq!(t.len(), 3);
        let r1 = t.rows().iter().find(|r| r.values[0].as_deref() == Some("1")).unwrap();
        assert_eq!(r1.kind, ChangeKind::Updated { changed: vec![false, true] });
        assert_eq!(r1.ts, 6);
        assert_eq!(t.removed.back().unwrap().values[1].as_deref(), Some("b"));
        // Multiplicity: one of the two "3" rows goes away.
        let s = t.apply(7, vec![d(7, -1, &["3", "c"])], now);
        assert_eq!(s.deleted, 0);
        assert_eq!(t.rows().iter().find(|r| r.values[0].as_deref() == Some("3")).unwrap().count, 1);
        let s = t.apply(8, vec![d(8, -1, &["3", "c"])], now);
        assert_eq!(s.deleted, 1);
        assert_eq!(t.len(), 2);
        assert_eq!(t.last_ts, 8);
        // Without a key, an update is a delete plus an insert.
        let mut t = LiveTable::new(vec!["v".into()], None);
        t.apply(1, vec![d(1, 1, &["a"])], now);
        let s = t.apply(2, vec![d(2, -1, &["a"]), d(2, 1, &["b"])], now);
        assert_eq!(s, BatchSummary { inserted: 1, updated: 0, deleted: 1 });
        // An empty initial result: the first batch is already a change.
        let mut t = LiveTable::new(vec!["v".into()], None);
        t.mark_initial_done();
        let s = t.apply(3, vec![d(3, 1, &["a"])], now);
        assert_eq!(s.inserted, 1);
    }
}
