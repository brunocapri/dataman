use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext, Context, Entity, Focusable, SharedString, Task, WeakEntity, Window, div, prelude::*};
use crate::theme::px;
use dataman_core::Connection;
use dataman_core::lar::live::{ChangeKind, LiveListen, NotificationEntry, SubState, SubscribeOptions, Subscription};

use super::grid::{GRow, LarGrid, RowPatch, RowStyle, columns_for, fade_alpha};
use super::widgets;
use super::{LarBar, LiveStmt};
use crate::app::Workspace;
use crate::result_bar::ResultState;
use crate::theme::ActiveTheme;

const COALESCE: Duration = Duration::from_millis(33);
const SAMPLES: usize = 40;

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: u64,
    pub key: Vec<Option<String>>,
    pub values: Vec<Option<String>>,
    pub style: RowStyle,
    pub changed: Vec<bool>,
    pub at: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct LiveIn {
    pub key: Vec<Option<String>>,
    pub values: Option<Vec<Option<String>>>,
    pub style: RowStyle,
    pub changed: Vec<bool>,
    pub at: Option<Instant>,
}

pub fn reconcile(entries: &mut Vec<Entry>, ins: Vec<LiveIn>, now: Instant, append_new: bool, next_id: &mut u64) -> (usize, usize) {
    let before = entries.len();
    entries.retain(|e| !(e.style == RowStyle::LiveGone && e.at.is_some_and(|a| now >= a + super::grid::GONE)));
    let mut dirty = 0usize;
    let pruned = entries.len() != before;
    let mut index: HashMap<Vec<Option<String>>, usize> = HashMap::with_capacity(entries.len());
    for (i, e) in entries.iter().enumerate() {
        if e.style != RowStyle::LiveGone {
            index.insert(e.key.clone(), i);
        }
    }
    let mut seen = vec![false; entries.len()];
    let mut fresh = Vec::new();
    for li in ins {
        match index.get(&li.key) {
            Some(&i) => {
                seen[i] = true;
                if let Some(v) = li.values {
                    dirty += 1;
                    let e = &mut entries[i];
                    e.values = v;
                    e.style = li.style;
                    e.changed = li.changed;
                    e.at = li.at;
                }
            }
            None => fresh.push(li),
        }
    }
    for (i, e) in entries.iter_mut().enumerate() {
        if !seen[i] && e.style != RowStyle::LiveGone {
            dirty += 1;
            e.style = RowStyle::LiveGone;
            e.at = Some(now);
        }
    }
    let mk = |li: LiveIn, next_id: &mut u64| {
        *next_id += 1;
        Entry { id: *next_id, key: li.key, values: li.values.unwrap_or_default(), style: li.style, changed: li.changed, at: li.at }
    };
    dirty += fresh.len();
    let changed = dirty + pruned as usize;
    if append_new {
        for li in fresh {
            entries.push(mk(li, next_id));
        }
        (0, changed)
    } else {
        let n = fresh.len();
        let mut top: Vec<Entry> = fresh.into_iter().map(|li| mk(li, next_id)).collect();
        top.append(entries);
        *entries = top;
        (n, changed)
    }
}

enum Run {
    Starting,
    Sub(Arc<Subscription>),
    Listen(Arc<LiveListen>),
    Failed(String),
    Stopped,
}

/// LAR's idle cut comes after ~1 s: a stream that lived this long was healthy.
const HEALTHY: Duration = Duration::from_millis(500);

pub fn restart_delay(fails: u32) -> Duration {
    if fails == 0 {
        return Duration::ZERO;
    }
    let ms = 250u64.saturating_mul(1 << (fails - 1).min(10));
    Duration::from_millis(ms.min(8_000))
}

fn cells(v: &[Option<String>]) -> Vec<Option<SharedString>> {
    v.iter().map(|v| v.clone().map(SharedString::from)).collect()
}

fn key_of(key: &Option<Vec<usize>>, values: &[Option<String>]) -> Vec<Option<String>> {
    match key {
        Some(k) => k.iter().map(|&i| values.get(i).cloned().flatten()).collect(),
        None => values.to_vec(),
    }
}

/// LAR may apply one commit in more than one batch, so the time tells them apart.
type Watermark = (u64, Option<Instant>);

fn newer(w: Watermark, ts: u64, at: Instant) -> bool {
    ts > w.0 || (ts == w.0 && w.1.is_none_or(|a| at > a))
}

fn bump(w: &mut Watermark, ts: u64, at: Instant) {
    if ts > w.0 {
        *w = (ts, Some(at));
    } else if ts == w.0 && w.1.is_none_or(|a| at > a) {
        w.1 = Some(at);
    }
}

/// Must match the core's `REMOVED_CAP` (`LiveTable::removed`).
const REMOVED_KEPT: usize = 256;

struct Delta {
    changed: Vec<(Vec<Option<String>>, Vec<Option<String>>, ChangeKind, Instant)>,
    removed: Vec<Vec<Option<String>>>,
    watermark: Watermark,
    total: usize,
    lost: bool,
}

pub struct LiveRun {
    pub grid: Entity<LarGrid>,
    pub stmt: LiveStmt,
    pub source_sql: String,
    pub since: chrono::DateTime<chrono::Local>,
    run: Run,
    entries: Vec<Entry>,
    next_id: u64,
    columns: Vec<String>,
    key: Option<Vec<usize>>,
    first: bool,
    measured: bool,
    last_apply: Option<Instant>,
    pub last_change: Option<chrono::DateTime<chrono::Local>>,
    pub rate: VecDeque<f32>,
    last_sample: Option<Instant>,
    listen_seen: u64,
    snapshot: bool,
    ended_at: Option<chrono::DateTime<chrono::Local>>,
    conn: Arc<Connection>,
    user_stopped: bool,
    pub restarts: u32,
    resumed: bool,
    user_paused: bool,
    restarting: bool,
    sub_started: Option<Instant>,
    fast_fails: u32,
    by_key: HashMap<Vec<Option<String>>, u64>,
    seen: Watermark,
    needs_full: bool,
    rate_moved: bool,
    window: usize,
    looks: usize,
    snapshot_changed: bool,
    _task: Option<Task<()>>,
}

impl LiveRun {
    pub fn start(conn: Arc<Connection>, stmt: LiveStmt, source_sql: String, cx: &mut Context<Self>) -> Self {
        let grid = cx.new(|cx| {
            let mut g = LarGrid::new(cx);
            g.empty = "[ WAITING ]".into();
            g
        });
        let mut me = LiveRun {
            grid,
            stmt: stmt.clone(),
            source_sql,
            since: chrono::Local::now(),
            run: Run::Starting,
            entries: vec![],
            next_id: 0,
            columns: vec![],
            key: None,
            first: true,
            measured: false,
            last_apply: None,
            last_change: None,
            rate: VecDeque::from(vec![0.; SAMPLES]),
            last_sample: None,
            listen_seen: 0,
            snapshot: false,
            ended_at: None,
            conn: conn.clone(),
            user_stopped: false,
            restarts: 0,
            resumed: false,
            user_paused: false,
            restarting: false,
            sub_started: None,
            fast_fails: 0,
            by_key: HashMap::new(),
            seen: (0, None),
            needs_full: false,
            rate_moved: false,
            window: 0,
            looks: 0,
            snapshot_changed: false,
            _task: None,
        };
        let Some(lar) = conn.lar() else {
            me.run = Run::Failed("not a LAR server".into());
            return me;
        };
        match stmt {
            LiveStmt::Subscribe { .. } => me.spawn_subscribe(cx),
            LiveStmt::Listen { channel, query } => me._task = Some({
                let task = lar.listen(&channel, &query);
                cx.spawn(async move |this, cx| {
                    let l = match task.await {
                        Ok(l) => Arc::new(l),
                        Err(e) => {
                            let _ = this.update(cx, |r, cx| {
                                r.run = Run::Failed(format!("{e:#}"));
                                cx.notify();
                            });
                            return;
                        }
                    };
                    let mut rx = l.changes();
                    let _ = this.update(cx, |r, cx| {
                        r.run = Run::Listen(l.clone());
                        r.apply(cx);
                    });
                    drop(l);
                    watch_loop(this, &mut rx, cx).await;
                })
            }),
        }
        me
    }

    fn spawn_subscribe(&mut self, cx: &mut Context<Self>) {
        let (Some(lar), LiveStmt::Subscribe { query, copy_form }) = (self.conn.lar(), self.stmt.clone()) else { return };
        let task = lar.subscribe(&query, SubscribeOptions { key: None, copy_form });
        self._task = Some(cx.spawn(async move |this, cx| {
            let sub = match task.await {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    let _ = this.update(cx, |r, cx| {
                        r.run = Run::Failed(format!("{e:#}"));
                        cx.notify();
                    });
                    return;
                }
            };
            let mut rx = sub.changes();
            let _ = this.update(cx, |r, cx| {
                if r.user_stopped {
                    let _ = sub.stop();
                    r.run = Run::Stopped;
                    r.restarting = false;
                    cx.notify();
                    return;
                }
                r.columns = sub.columns();
                r.key = sub.with_table(|t| t.key().map(|k| k.to_vec()));
                r.run = Run::Sub(sub.clone());
                r.restarting = false;
                r.sub_started = Some(Instant::now());
                if r.user_paused {
                    sub.pause();
                }
                r.apply(cx);
            });
            drop(sub);
            watch_loop(this, &mut rx, cx).await;
        }));
    }

    pub fn kind(&self) -> &'static str {
        match self.stmt {
            LiveStmt::Subscribe { .. } => "SUBSCRIBE",
            LiveStmt::Listen { .. } => "LISTEN",
        }
    }

    pub fn channel(&self) -> Option<&str> {
        match &self.stmt {
            LiveStmt::Listen { channel, .. } => Some(channel),
            _ => None,
        }
    }

    pub fn state(&self) -> SubState {
        let kept = if self.user_paused { SubState::Paused } else { SubState::Live };
        match &self.run {
            Run::Starting if self.restarting && !self.user_stopped => kept,
            Run::Starting => SubState::Starting,
            Run::Stopped => SubState::Ended { error: None },
            Run::Sub(s) => match s.state() {
                SubState::Ended { error: None } if !self.user_stopped => kept,
                st => st,
            },
            Run::Listen(l) => match l.ended() {
                Some(e) => SubState::Ended { error: Some(dataman_core::result::ErrorInfo { message: e, ..Default::default() }) },
                None => SubState::Live,
            },
            Run::Failed(e) => SubState::Ended { error: Some(dataman_core::result::ErrorInfo { message: e.clone(), ..Default::default() }) },
        }
    }

    pub fn failed(&self) -> Option<&str> {
        match &self.run {
            Run::Failed(e) => Some(e),
            _ => None,
        }
    }

    pub fn buffered(&self) -> usize {
        match &self.run {
            Run::Sub(s) => s.stats().buffered,
            _ => 0,
        }
    }

    pub fn log(&self) -> Vec<NotificationEntry> {
        match &self.run {
            Run::Listen(l) => l.log(),
            _ => vec![],
        }
    }

    pub fn latest_body(&self) -> Option<String> {
        let Run::Listen(l) = &self.run else { return None };
        let p = l.latest()?;
        let rows: Vec<String> = p
            .rows
            .iter()
            .take(3)
            .map(|r| {
                let kv: Vec<String> = p.columns.iter().zip(r).map(|(c, v)| format!("\"{c}\": {v}")).collect();
                format!("{{{}}}", kv.join(", "))
            })
            .collect();
        let more = p.rows.len().saturating_sub(3);
        Some(if more > 0 { format!("{}, … +{more}", rows.join(", ")) } else { rows.join(", ") })
    }

    pub fn rows_shown(&self) -> usize {
        self.entries.iter().filter(|e| e.style != RowStyle::LiveGone).count()
    }

    pub fn current_rate(&self) -> f32 {
        self.rate.back().copied().unwrap_or(0.)
    }

    fn sampling(&self) -> bool {
        self.rate.iter().any(|v| *v > 0.) && !matches!(self.state(), SubState::Ended { .. })
    }

    fn sample(&mut self, now: Instant) {
        if let Run::Listen(l) = &self.run {
            let n = l.received();
            self.window += n.saturating_sub(self.listen_seen) as usize;
            self.listen_seen = n;
        }
        let Some(t) = self.last_sample else {
            self.last_sample = Some(now);
            return;
        };
        let secs = now.duration_since(t).as_secs();
        if secs == 0 {
            return;
        }
        self.rate_moved = self.window > 0 || self.rate.iter().any(|v| *v > 0.);
        if std::env::var_os("DATAMAN_TRACE_LIVE").is_some() {
            eprintln!("live: {} looks, {} changes in the last {secs} s", self.looks, self.window);
            self.looks = 0;
        }
        self.rate.push_back(self.window as f32);
        for _ in 1..secs.min(SAMPLES as u64) {
            self.rate.push_back(0.);
        }
        while self.rate.len() > SAMPLES {
            self.rate.pop_front();
        }
        self.window = 0;
        self.last_sample = Some(t + Duration::from_secs(secs));
    }

    fn schedule_restart(&mut self, immediate: bool, cx: &mut Context<Self>) {
        let healthy = immediate || (self.sub_started.is_some_and(|t| t.elapsed() >= HEALTHY) && matches!(&self.run, Run::Sub(s) if s.stats().batches > 0));
        self.fast_fails = if healthy { 0 } else { self.fast_fails + 1 };
        let delay = restart_delay(self.fast_fails);
        self.restarts += 1;
        self.resumed = true;
        self.restarting = true;
        self.run = Run::Starting;
        if delay.is_zero() {
            self.spawn_subscribe(cx);
        } else {
            self._task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |r, cx| {
                    if matches!(r.run, Run::Starting) && !r.user_stopped {
                        r.spawn_subscribe(cx);
                    }
                });
            }));
        }
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        // LAR ends an extended-protocol SUBSCRIBE after ~1 s without changes (it takes the
        // client's pipelined Sync for "the client left"): open it again, keeping the rows —
        // unless the user paused (it opens again on resume) or stopped.
        if let Run::Sub(s) = &self.run
            && matches!(s.state(), SubState::Ended { error: None })
            && !self.user_stopped
            && !self.snapshot
            && !self.user_paused
            && !s.paused_by_user()
        {
            self.schedule_restart(false, cx);
            return;
        }
        let now = Instant::now();
        self.last_apply = Some(now);
        self.looks += 1;
        self.sample(now);
        if let Run::Sub(s) = &self.run
            && !self.first
            && !self.resumed
            && self.measured
            && !self.needs_full
            && matches!(self.state(), SubState::Live | SubState::Paused)
            && let Some(d) = self.delta(s)
        {
            self.apply_delta(d, now, cx);
            return;
        }
        let (ins, append) = match &self.run {
            Run::Sub(s) => {
                if s.stats().batches == 0 && !self.first {
                    return;
                }
                let index: HashMap<Vec<Option<String>>, (Option<Instant>, RowStyle)> =
                    self.entries.iter().filter(|e| e.style != RowStyle::LiveGone).map(|e| (e.key.clone(), (e.at, e.style))).collect();
                let key = self.key.clone();
                let first = self.first;
                let resumed = self.resumed;
                let shown: HashMap<Vec<Option<String>>, &Vec<Option<String>>> =
                    if resumed { self.entries.iter().filter(|e| e.style != RowStyle::LiveGone).map(|e| (e.key.clone(), &e.values)).collect() } else { HashMap::new() };
                let (ins, seen) = s.with_table(|t| {
                    // What this look covers, read under the same lock as the rows.
                    let mut w = (0, None);
                    for r in t.rows() {
                        bump(&mut w, r.ts, r.at);
                    }
                    for r in &t.removed {
                        bump(&mut w, r.ts, r.at);
                    }
                    let ins: Vec<LiveIn> = t
                        .rows()
                        .iter()
                        .map(|r| {
                            let k: Vec<Option<String>> = match &key {
                                Some(k) => k.iter().map(|&i| r.values.get(i).cloned().flatten()).collect(),
                                None => r.values.clone(),
                            };
                            let (style, changed, at) = match &r.kind {
                                ChangeKind::Initial if resumed => match shown.get(&k) {
                                    None => (RowStyle::LiveNew, vec![], Some(now)),
                                    Some(v) if **v != r.values => (RowStyle::LiveChanged, v.iter().zip(&r.values).map(|(a, b)| a != b).collect(), Some(now)),
                                    Some(_) => match index.get(&k) {
                                        Some((a, st)) => (*st, vec![], *a),
                                        None => (RowStyle::Plain, vec![], None),
                                    },
                                },
                                ChangeKind::Initial => (RowStyle::Plain, vec![], None),
                                ChangeKind::Inserted => (RowStyle::LiveNew, vec![], Some(r.at)),
                                ChangeKind::Updated { changed } => (RowStyle::LiveChanged, changed.clone(), Some(r.at)),
                            };
                            let unchanged = index.get(&k).is_some_and(|(a, st)| *a == at && *st == style);
                            LiveIn { key: k, values: (!unchanged).then(|| r.values.clone()), style, changed, at }
                        })
                        .collect();
                    (ins, w)
                });
                self.seen = seen;
                self.needs_full = false;
                self.resumed = false;
                (ins, first || self.entries.is_empty())
            }
            Run::Listen(l) => {
                let Some(p) = l.latest() else { return };
                if self.columns.is_empty() {
                    self.columns = p.columns.clone();
                }
                let id_col = p.columns.iter().position(|c| c == "id");
                let old: HashMap<Vec<Option<String>>, &Entry> = self.entries.iter().filter(|e| e.style != RowStyle::LiveGone).map(|e| (e.key.clone(), e)).collect();
                let first = self.first;
                let ins = (0..p.rows.len())
                    .map(|r| {
                        let values: Vec<Option<String>> = (0..p.columns.len()).map(|c| p.text(r, c)).collect();
                        let key = match id_col {
                            Some(i) => vec![values[i].clone()],
                            None => values.clone(),
                        };
                        match old.get(&key) {
                            Some(e) if e.values == values => LiveIn { key, values: None, style: e.style, changed: e.changed.clone(), at: e.at },
                            Some(e) => {
                                let changed = e.values.iter().zip(&values).map(|(a, b)| a != b).collect();
                                LiveIn { key, values: Some(values), style: RowStyle::LiveChanged, changed, at: Some(now) }
                            }
                            None if first => LiveIn { key, values: Some(values), style: RowStyle::Plain, changed: vec![], at: None },
                            None => LiveIn { key, values: Some(values), style: RowStyle::LiveNew, changed: vec![], at: Some(now) },
                        }
                    })
                    .collect();
                (ins, first)
            }
            Run::Stopped => (
                self.entries
                    .iter()
                    .filter(|e| e.style != RowStyle::LiveGone)
                    .map(|e| LiveIn { key: e.key.clone(), values: None, style: e.style, changed: e.changed.clone(), at: e.at })
                    .collect(),
                false,
            ),
            _ => return,
        };
        let (prepended, changes) = reconcile(&mut self.entries, ins, now, append, &mut self.next_id);
        self.by_key = self.entries.iter().filter(|e| e.style != RowStyle::LiveGone).map(|e| (e.key.clone(), e.id)).collect();
        let dirty = changes > 0;
        if !append {
            self.window += changes;
        }
        if dirty && !self.first {
            self.last_change = Some(chrono::Local::now());
        }
        let ended = matches!(self.state(), SubState::Ended { .. });
        let was_ended = self.ended_at.is_some();
        if ended && self.ended_at.is_none() {
            self.ended_at = Some(chrono::Local::now());
        }
        let first_cols = self.first || !self.measured;
        self.first = false;
        if first_cols {
            self.measured = !self.entries.is_empty();
        }
        if !dirty && !first_cols && ended == was_ended && !self.snapshot_changed {
            if self.rate_moved {
                cx.notify();
            }
            return;
        }
        self.snapshot_changed = false;
        let rows: Vec<GRow> = self
            .entries
            .iter()
            .map(|e| GRow {
                id: e.id,
                cells: e.values.iter().map(|v| v.clone().map(SharedString::from)).collect(),
                changed: e.changed.clone(),
                style: if ended && !self.snapshot && e.style != RowStyle::LiveGone { RowStyle::Stale } else { e.style },
                at: e.at,
            })
            .collect();
        let grid = self.grid.clone();
        if first_cols {
            let sample: Vec<Vec<Option<String>>> = self.entries.iter().take(200).map(|e| e.values.clone()).collect();
            let cols = columns_for(&self.columns, &sample);
            grid.update(cx, |g, cx| {
                g.empty = "[ 0 ROWS ]".into();
                g.set_data(cols, rows, cx)
            });
        } else {
            grid.update(cx, |g, cx| g.replace_rows(rows, prepended, cx));
        }
        cx.notify();
    }

    fn delta(&self, s: &Subscription) -> Option<Delta> {
        let (seen, key) = (self.seen, self.key.clone());
        let d = s.with_table(|t| {
            let mut w = seen;
            let mut changed = Vec::new();
            for r in t.rows() {
                if newer(seen, r.ts, r.at) {
                    changed.push((key_of(&key, &r.values), r.values.clone(), r.kind.clone(), r.at));
                    bump(&mut w, r.ts, r.at);
                }
            }
            let mut removed = Vec::new();
            for r in &t.removed {
                if newer(seen, r.ts, r.at) {
                    removed.push(key_of(&key, &r.values));
                    bump(&mut w, r.ts, r.at);
                }
            }
            let lost = t.removed.len() >= REMOVED_KEPT && t.removed.front().is_some_and(|r| newer(seen, r.ts, r.at));
            Delta { changed, removed, watermark: w, total: t.len(), lost }
        });
        (!d.lost).then_some(d)
    }

    fn apply_delta(&mut self, d: Delta, now: Instant, cx: &mut Context<Self>) {
        let mut patches: HashMap<u64, RowPatch> = HashMap::new();
        let mut fresh: Vec<Entry> = Vec::new();
        let mut updates: HashMap<u64, (Vec<Option<String>>, RowStyle, Vec<bool>, Option<Instant>)> = HashMap::new();
        let present: std::collections::HashSet<&Vec<Option<String>>> = d.changed.iter().map(|c| &c.0).collect();
        let mut gone_keys = Vec::new();
        for k in &d.removed {
            if !present.contains(k)
                && let Some(id) = self.by_key.remove(k)
            {
                gone_keys.push(id);
            }
        }
        for (k, values, kind, at) in d.changed {
            let (style, changed) = match kind {
                ChangeKind::Initial => (RowStyle::Plain, vec![]),
                ChangeKind::Inserted => (RowStyle::LiveNew, vec![]),
                ChangeKind::Updated { changed } => (RowStyle::LiveChanged, changed),
            };
            let at = (style != RowStyle::Plain).then_some(at);
            match self.by_key.get(&k) {
                Some(&id) => {
                    patches.insert(id, RowPatch { cells: Some(cells(&values)), style, changed: changed.clone(), at });
                    updates.insert(id, (values, style, changed, at));
                }
                None => {
                    self.next_id += 1;
                    self.by_key.insert(k.clone(), self.next_id);
                    fresh.push(Entry { id: self.next_id, key: k, values, style, changed, at });
                }
            }
        }
        for id in gone_keys {
            patches.insert(id, RowPatch { cells: None, style: RowStyle::LiveGone, changed: vec![], at: Some(now) });
        }
        self.seen = d.watermark;
        let changes = patches.len() + fresh.len();
        let pending = updates;
        let gone: std::collections::HashSet<u64> = patches.iter().filter(|(_, p)| p.style == RowStyle::LiveGone).map(|(id, _)| *id).collect();
        if !pending.is_empty() || !gone.is_empty() || self.entries.iter().any(|e| e.style == RowStyle::LiveGone) {
            self.entries.retain_mut(|e| {
                if let Some((v, st, ch, at)) = pending.get(&e.id) {
                    e.values = v.clone();
                    e.style = *st;
                    e.changed = ch.clone();
                    e.at = *at;
                } else if gone.contains(&e.id) {
                    e.style = RowStyle::LiveGone;
                    e.at = Some(now);
                }
                !(e.style == RowStyle::LiveGone && e.at.is_some_and(|a| now >= a + super::grid::GONE))
            });
        }
        let top: Vec<GRow> = fresh.iter().map(|e| GRow { id: e.id, cells: cells(&e.values), changed: e.changed.clone(), style: e.style, at: e.at }).collect();
        if !fresh.is_empty() {
            let mut v = fresh;
            v.append(&mut self.entries);
            self.entries = v;
        }
        if self.by_key.len() != d.total {
            self.needs_full = true;
        }
        if changes == 0 {
            if self.rate_moved {
                cx.notify();
            }
            return;
        }
        self.window += changes;
        self.last_change = Some(chrono::Local::now());
        let grid = self.grid.clone();
        grid.update(cx, |g, cx| g.patch(patches, top, cx));
        cx.notify();
    }

    pub fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        match self.state() {
            SubState::Live => {
                self.user_paused = true;
                if let Run::Sub(s) = &self.run
                    && s.state() == SubState::Live
                {
                    s.pause();
                }
            }
            SubState::Paused => {
                self.user_paused = false;
                match &self.run {
                    Run::Sub(s) if s.state() == SubState::Paused => s.resume(),
                    Run::Sub(s) if matches!(s.state(), SubState::Ended { error: None }) => self.schedule_restart(true, cx),
                    _ => {}
                }
            }
            _ => {}
        }
        cx.notify();
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.user_stopped = true;
        match &self.run {
            Run::Sub(s) => {
                let _ = s.stop();
            }
            Run::Listen(l) => {
                let _ = l.unlisten();
            }
            Run::Starting => {
                self.run = Run::Stopped;
                self.restarting = false;
                if self.ended_at.is_none() {
                    self.ended_at = Some(chrono::Local::now());
                }
                self._task = None;
                self.snapshot_changed = true;
                self.apply(cx);
            }
            _ => {}
        }
        cx.notify();
    }

    pub fn keep_snapshot(&mut self, cx: &mut Context<Self>) {
        self.snapshot = true;
        self.snapshot_changed = true;
        self.apply(cx);
    }

    pub fn bar(&self) -> LarBar {
        let rows = Some(crate::grid::ui_count(self.rows_shown()));
        let last = self.last_change.map(|t| format!("last change {}", t.format("%H:%M:%S"))).unwrap_or_else(|| "no change yet".into());
        let pause: (&'static str, &'static str, &'static str, Box<dyn gpui::Action>) = ("rb-pause", "Pause", "⌥P", Box::new(super::PauseLive));
        let stop: (&'static str, &'static str, &'static str, Box<dyn gpui::Action>) = ("rb-stop", "Stop", "", Box::new(super::StopLive));
        let mk = |state, label: &str, note: String, err: bool, actions| LarBar {
            state,
            label: SharedString::from(label.to_string()),
            rows: rows.clone(),
            rows_label: Some("live rows"),
            metrics: vec![],
            note,
            note_err: err,
            actions,
        };
        if let Some(e) = self.failed() {
            return LarBar { rows: None, ..mk(ResultState::Error, "NOT LIVE", e.to_string(), true, vec![]) };
        }
        match self.state() {
            SubState::Starting => mk(ResultState::Running, "STARTING", String::new(), false, vec![]),
            SubState::Live => {
                let rate = match self.stmt {
                    LiveStmt::Subscribe { .. } => format!("{} changes/s", self.current_rate() as u64),
                    LiveStmt::Listen { .. } => format!("{} notifications", self.log().len()),
                };
                mk(ResultState::Live, "LIVE", format!("{rate} · {last}"), false, vec![pause, stop])
            }
            SubState::Paused => mk(
                ResultState::Paused,
                "PAUSED",
                format!("{} buffered", crate::grid::ui_count(self.buffered())),
                false,
                vec![("rb-pause", "Resume", "⌥P", Box::new(super::PauseLive)), stop],
            ),
            SubState::Ended { error } => {
                let since = self.ended_at.map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default();
                let actions = vec![
                    ("rb-resub", "Resubscribe", "", Box::new(super::Resubscribe) as Box<dyn gpui::Action>),
                    ("rb-keep", "Keep as snapshot", "", Box::new(super::KeepSnapshot)),
                ];
                if self.snapshot {
                    let resub: Vec<(&'static str, &'static str, &'static str, Box<dyn gpui::Action>)> =
                        vec![("rb-resub", "Resubscribe", "", Box::new(super::Resubscribe))];
                    return mk(ResultState::Done, "SNAPSHOT", format!("ended {since}"), false, resub);
                }
                match error {
                    Some(e) => mk(ResultState::Error, "STREAM ENDED", format!("{since} · {}", e.message), true, actions),
                    None => mk(ResultState::Cancelled, "STOPPED", format!("at {since}"), false, actions),
                }
            }
        }
    }
}

async fn watch_loop(this: WeakEntity<LiveRun>, rx: &mut tokio::sync::watch::Receiver<u64>, cx: &mut gpui::AsyncApp) {
    use std::pin::pin;
    use std::task::Poll;
    loop {
        let sampling = this.update(cx, |r, _| r.sampling()).unwrap_or(false);
        let changed = {
            let mut changed = pin!(rx.changed());
            let mut timer = pin!(cx.background_executor().timer(Duration::from_secs(1)));
            std::future::poll_fn(|c| {
                if let Poll::Ready(r) = changed.as_mut().poll(c) {
                    return Poll::Ready(r.is_ok());
                }
                if sampling && timer.as_mut().poll(c).is_ready() {
                    return Poll::Ready(true);
                }
                Poll::Pending
            })
            .await
        };
        let wait = this.update(cx, |r, _| r.last_apply.map(|t| COALESCE.saturating_sub(t.elapsed()))).ok().flatten();
        if let Some(w) = wait
            && !w.is_zero()
        {
            cx.background_executor().timer(w).await;
        }
        if this.update(cx, |r, cx| r.apply(cx)).is_err() || !changed {
            let _ = this.update(cx, |r, cx| r.apply(cx));
            break;
        }
    }
}

pub struct LivePanel {
    conn: Arc<Connection>,
    ws: WeakEntity<Workspace>,
}

impl LivePanel {
    pub fn new(conn: Arc<Connection>, ws: WeakEntity<Workspace>, _: &mut Context<Self>) -> Self {
        LivePanel { conn, ws }
    }

    fn runs(&self, cx: &App) -> Vec<(usize, SharedString, Entity<LiveRun>, Entity<crate::tab::QueryTab>)> {
        let Some(ws) = self.ws.upgrade() else { return vec![] };
        ws.read(cx)
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let tab = t.read(cx);
                let same = super::same_conn(tab.connection(), &self.conn);
                let run = tab.lar.live().cloned()?;
                same.then(|| (i, tab.title(cx), run, t.clone()))
            })
            .collect()
    }

    fn insert_template(&self, window: &mut Window, cx: &mut App) {
        let Some(tab) = super::target_tab(&self.ws, &self.conn, cx) else { return };
        let table = tab.read(cx).lar.last_query.as_deref().and_then(super::first_table).unwrap_or_else(|| "my_table".into());
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| e.insert_at_cursor(&format!("SUBSCRIBE SELECT * FROM {table};"), cx));
        window.focus(&editor.focus_handle(cx), cx);
    }
}

impl Render for LivePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let runs = self.runs(cx);
        let active = self.ws.upgrade().map(|w| w.read(cx).active).unwrap_or(0);
        let n = runs.len();
        let any_live = runs.iter().any(|(_, _, r, _)| r.read(cx).state() == SubState::Live);
        let header = widgets::header(&t, "● LIVE", t.lar)
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(t.tx3).child(format!(
                "{} · {n} stream{}",
                self.conn.profile.name,
                if n == 1 { "" } else { "s" }
            )))
            .child(
                div()
                    .id("live-pause-all")
                    .text_size(px(11.))
                    .text_color(if n > 0 { t.tx2 } else { t.tx4 })
                    .cursor_pointer()
                    .child(crate::ui::keys_text(if any_live { "pause all ⌥P" } else { "resume all ⌥P" }))
                    .on_click(cx.listener(move |p, _, _, cx| {
                        for (_, _, r, _) in p.runs(cx) {
                            r.update(cx, |r, cx| {
                                let st = r.state();
                                if (any_live && st == SubState::Live) || (!any_live && st == SubState::Paused) {
                                    r.toggle_pause(cx);
                                }
                            });
                        }
                    })),
            );
        if runs.is_empty() {
            return div().size_full().flex().flex_col().text_size(px(12.)).child(header).child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(14.))
                    .child(crate::ui::empty_state(&t, "[ NO STREAMS ]", 20., None))
                    .child(
                        widgets::lar_btn(&t, "live-insert", "Insert SUBSCRIBE")
                            .px(px(10.))
                            .on_click(cx.listener(|p, _, window, cx| p.insert_template(window, cx))),
                    ),
            );
        }
        let now = Instant::now();
        let cards = runs.iter().map(|(i, title, run, owner)| {
            let r = run.read(cx);
            let st = r.state();
            let (glyph, gc) = match &st {
                SubState::Live | SubState::Starting => ("●", t.lar),
                SubState::Paused => ("‖", t.info),
                SubState::Ended { error: Some(_) } => ("✕", t.warn),
                SubState::Ended { error: None } => ("■", t.tx3),
            };
            let on = st == SubState::Live && *i == active;
            let (btn, act): (&str, u8) = match st {
                SubState::Live => ("‖ Pause", 0),
                SubState::Paused => ("▶ Resume", 0),
                SubState::Ended { .. } => ("Resubscribe", 1),
                SubState::Starting => ("…", 2),
            };
            let rate_text = match (&st, &r.stmt) {
                (SubState::Paused, _) => format!("paused · {} buffered", crate::grid::ui_count(r.buffered())),
                (SubState::Ended { error: Some(_) }, _) => "stream ended · stale".to_string(),
                (SubState::Ended { error: None }, _) => "stopped".to_string(),
                (_, LiveStmt::Subscribe { .. }) => format!("{} changes/s", r.current_rate() as u64),
                (_, LiveStmt::Listen { .. }) => format!("{} notifications", r.log().len()),
            };
            let where_ = if *i == active { "this tab".to_string() } else { format!("tab {title}") };
            let since = match &st {
                SubState::Paused => "paused".to_string(),
                _ => format!("since {}", r.since.format("%H:%M")),
            };
            let what = match &r.stmt {
                LiveStmt::Subscribe { query, .. } => query.split_whitespace().collect::<Vec<_>>().join(" "),
                LiveStmt::Listen { channel, query } => format!("{channel} AS {}", query.split_whitespace().collect::<Vec<_>>().join(" ")),
            };
            let spark: Vec<f32> = r.rate.iter().copied().collect();
            let spark_color = if st == SubState::Live { if on { t.lar } else { t.lar.opacity(0.6) } } else { t.line2 };
            let run2 = run.clone();
            let ix = *i;
            let (owner, owner2) = (owner.clone(), owner.clone());
            div()
                .id(("live-card", ix))
                .p(px(10.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .cursor_pointer()
                .when(on, |d| d.bg(t.lar_s).border_1().border_color(t.lar))
                .when(!on, |d| d.bg(t.hov).border_1().border_color(t.line))
                .on_click(cx.listener(move |p, _, window, cx| {
                    if let Some(ws) = p.ws.upgrade() {
                        ws.update(cx, |w, cx| w.activate_tab(&owner, window, cx));
                    }
                }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(div().text_color(gc).child(glyph))
                        .child(div().text_color(t.lkw).child(r.kind()))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(what))
                        .child(widgets::small_btn(&t, ("live-btn", ix), btn).h(px(22.)).px(px(7.)).on_click(cx.listener(move |p, _, _, cx| {
                            cx.stop_propagation();
                            match act {
                                0 => run2.update(cx, |r, cx| r.toggle_pause(cx)),
                                1 => {
                                    let sql = run2.read(cx).source_sql.clone();
                                    let _ = &p;
                                    owner2.update(cx, |t, cx| t.run(vec![sql], cx));
                                }
                                _ => {}
                            }
                        }))),
                )
                .child(widgets::sparkline(&spark, spark_color, 22., 1.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(12.))
                        .text_size(px(10.5))
                        .text_color(t.tx3)
                        .child(div().text_color(if st == SubState::Live { t.lar } else if matches!(st, SubState::Paused) { t.info } else { t.tx2 }).child(rate_text))
                        .child(where_)
                        .child(div().flex_1())
                        .child(since),
                )
        });
        let listen = runs
            .iter()
            .find(|(i, _, r, _)| *i == active && r.read(cx).channel().is_some())
            .or_else(|| runs.iter().find(|(_, _, r, _)| r.read(cx).channel().is_some()))
            .map(|(_, _, r, _)| r.clone());
        let log = listen.map(|r| {
            let r = r.read(cx);
            let channel = r.channel().unwrap_or("").to_string();
            let entries = r.log();
            let body = r.latest_body();
            let since = r.since;
            let items = entries.iter().rev().take(60).enumerate().map(|(k, e)| {
                let local = e.at.with_timezone(&chrono::Local);
                let fresh = fade_alpha_wall(e.at, now);
                let text = match (&e.error, k) {
                    (Some(err), _) => format!("payload not understood: {err}"),
                    (None, 0) => body.clone().unwrap_or_else(|| format!("{} rows", e.rows)),
                    (None, _) => format!("{} rows · {} bytes", e.rows, crate::grid::ui_count(e.bytes)),
                };
                div()
                    .px(px(10.))
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .when(fresh > 0., |d| d.bg(t.lar_s.opacity(fresh)).border_l_2().border_color(t.lar.opacity(fresh)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .text_color(t.tx3)
                            .child(local.format("%H:%M:%S%.3f").to_string())
                            .child(div().text_color(t.tx2).child(e.ts.map(|ts| format!("ts {}", super::fmt_commit(ts).trim_start_matches('#'))).unwrap_or_else(|| "—".into()))),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(div().flex().flex_row().child(div().text_color(t.tx3).child("{\"rows\": [")).child(div().text_color(t.tx).child(text)).child(div().text_color(t.tx3).child("]}"))),
                    )
            });
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .px(px(14.))
                        .pt(px(10.))
                        .pb(px(6.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(widgets::caps(&t, format!("NOTIFICATIONS · {channel}"))),
                )
                .child(
                    div()
                        .id("live-log")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .mx(px(10.))
                        .mb(px(10.))
                        .py(px(6.))
                        .bg(t.card)
                        .border_1()
                        .border_color(t.line)
                        .text_size(px(11.))
                        .line_height(px(17.))
                        .children(items)
                        .child(
                            div()
                                .px(px(10.))
                                .py(px(4.))
                                .text_color(t.tx3)
                                .child(format!("{} · LISTEN {channel} registered", since.format("%H:%M:%S%.3f"))),
                        ),
                )
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(12.))
            .child(header)
            .child(div().id("live-cards").px(px(10.)).pt(px(10.)).pb(px(4.)).flex().flex_col().gap(px(6.)).children(cards))
            .children(log)
    }
}

fn fade_alpha_wall(at: chrono::DateTime<chrono::Utc>, now: Instant) -> f32 {
    let age = (chrono::Utc::now() - at).to_std().unwrap_or(Duration::ZERO);
    let start = now.checked_sub(age).unwrap_or(now);
    fade_alpha(start, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn li(k: &str, v: Option<&str>, style: RowStyle, at: Option<Instant>) -> LiveIn {
        LiveIn { key: vec![Some(k.into())], values: v.map(|v| vec![Some(k.into()), Some(v.into())]), style, changed: vec![], at }
    }

    #[test]
    fn first_result_appends_then_new_rows_go_on_top() {
        let t0 = Instant::now();
        let mut e = vec![];
        let mut id = 0;
        let (top, dirty) = reconcile(&mut e, vec![li("1", Some("a"), RowStyle::Plain, None), li("2", Some("b"), RowStyle::Plain, None)], t0, true, &mut id);
        assert_eq!(top, 0);
        assert!(dirty > 0);
        assert_eq!(e.iter().map(|x| x.key[0].clone().unwrap()).collect::<Vec<_>>(), vec!["1", "2"]);
        let (top, _) = reconcile(
            &mut e,
            vec![li("1", None, RowStyle::Plain, None), li("2", None, RowStyle::Plain, None), li("3", Some("c"), RowStyle::LiveNew, Some(t0))],
            t0,
            false,
            &mut id,
        );
        assert_eq!(top, 1);
        let (_, dirty) = reconcile(&mut e, vec![li("1", None, RowStyle::Plain, None), li("2", None, RowStyle::Plain, None), li("3", None, RowStyle::LiveNew, Some(t0))], t0, false, &mut id);
        assert_eq!(dirty, 0);
        assert_eq!(e[0].key[0].as_deref(), Some("3"));
        assert_eq!((e[1].id, e[2].id), (1, 2));
    }

    #[test]
    fn removed_rows_linger_600ms_then_go() {
        let t0 = Instant::now();
        let mut e = vec![];
        let mut id = 0;
        reconcile(&mut e, vec![li("1", Some("a"), RowStyle::Plain, None), li("2", Some("b"), RowStyle::Plain, None)], t0, true, &mut id);
        reconcile(&mut e, vec![li("1", None, RowStyle::Plain, None)], t0, false, &mut id);
        assert_eq!(e.len(), 2);
        assert_eq!(e[1].style, RowStyle::LiveGone);
        reconcile(&mut e, vec![li("1", None, RowStyle::Plain, None)], t0 + Duration::from_millis(300), false, &mut id);
        assert_eq!(e.len(), 2, "still fading out");
        reconcile(&mut e, vec![li("1", None, RowStyle::Plain, None)], t0 + Duration::from_millis(650), false, &mut id);
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn restarts_back_off_after_short_streams() {
        assert_eq!(restart_delay(0), Duration::ZERO, "LAR's idle cut after a healthy stream: at once");
        assert_eq!(restart_delay(1), Duration::from_millis(250));
        assert_eq!(restart_delay(2), Duration::from_millis(500));
        assert_eq!(restart_delay(4), Duration::from_secs(2));
        assert_eq!(restart_delay(6), Duration::from_secs(8));
        assert_eq!(restart_delay(60), Duration::from_secs(8), "capped");
    }

    #[test]
    fn watermark_sees_split_commits() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(7);
        let mut w: Watermark = (0, None);
        assert!(newer(w, 10, t0));
        bump(&mut w, 10, t0);
        assert!(!newer(w, 10, t0));
        assert!(newer(w, 10, t1));
        assert!(!newer(w, 9, t1), "older commits were seen");
        bump(&mut w, 10, t1);
        assert_eq!(w, (10, Some(t1)));
        bump(&mut w, 11, t0);
        assert_eq!(w, (11, Some(t0)));
    }

    #[test]
    fn changes_update_in_place() {
        let t0 = Instant::now();
        let mut e = vec![];
        let mut id = 0;
        reconcile(&mut e, vec![li("1", Some("a"), RowStyle::Plain, None)], t0, true, &mut id);
        let mut c = li("1", Some("z"), RowStyle::LiveChanged, Some(t0));
        c.changed = vec![false, true];
        let (top, dirty) = reconcile(&mut e, vec![c], t0, false, &mut id);
        assert_eq!(top, 0);
        assert!(dirty > 0);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].values[1].as_deref(), Some("z"));
        assert_eq!(e[0].style, RowStyle::LiveChanged);
        assert_eq!(e[0].id, 1);
    }
}
