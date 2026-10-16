//! The activity monitor's data: sessions (`pg_stat_activity`), the blocking tree (`pg_locks` and
//! `pg_blocking_pids()`), table health (`pg_stat_user_tables` with a bloat estimate) and the top
//! statements (`pg_stat_statements`).
//!
//! Everything is read through the connection's metadata client in text form, so a server that
//! lacks a view (LAR has no `pg_stat_activity`) just fails that query: the UI hides what is not
//! reported. Ages are whole seconds computed by the server (`now() - …`), so two reads of an idle
//! server compare equal and the monitor does not redraw.
//!
//! Signals (`pg_cancel_backend`, `pg_terminate_backend`) and `pg_stat_statements_reset()` are
//! plain functions here; asking first is the UI's job. VACUUM and ANALYZE are only templates.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::Task;
use crate::conn::{Connection, first_ok, text_rows};
use crate::sqlgen::qualified;

// ---------------------------------------------------------------------------------------------
// Sessions

/// One backend of `pg_stat_activity`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRow {
    pub pid: i32,
    pub user: Option<String>,
    pub db: Option<String>,
    pub app: Option<String>,
    /// `10.0.0.5:51234`, `local` (Unix socket), or nothing (background processes).
    pub client: Option<String>,
    pub state: Option<String>,
    pub wait_type: Option<String>,
    pub wait: Option<String>,
    /// Seconds since the transaction, the statement, the state and the backend started.
    pub xact_age: Option<i64>,
    pub query_age: Option<i64>,
    pub state_age: Option<i64>,
    pub backend_age: Option<i64>,
    pub backend_type: Option<String>,
    /// As the server reports it: `<insufficient privilege>` for another role's session when the
    /// user may not see it.
    pub query: Option<String>,
    pub blocked_by: Vec<i32>,
    /// The backend running this very query (the monitor's own).
    pub is_self: bool,
}

/// What a session's `state` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    Idle,
    IdleInTx,
    /// Idle in a transaction that failed.
    Aborted,
    FastPath,
    Disabled,
    /// No state: a background process.
    None,
}

impl State {
    pub fn of(s: Option<&str>) -> State {
        match s {
            Some("active") => State::Active,
            Some("idle") => State::Idle,
            Some("idle in transaction") => State::IdleInTx,
            Some("idle in transaction (aborted)") => State::Aborted,
            Some("fastpath function call") => State::FastPath,
            Some("disabled") => State::Disabled,
            _ => State::None,
        }
    }
}

/// When a session is worth a look: a statement running long, or a transaction left idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// An active statement this old (seconds) is a warning, then an error.
    pub query_warn: i64,
    pub query_err: i64,
    /// Idle in a transaction this long (seconds).
    pub idle_tx_warn: i64,
    pub idle_tx_err: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { query_warn: 60, query_err: 300, idle_tx_warn: 30, idle_tx_err: 300 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warn,
    Err,
}

impl SessionRow {
    pub fn state_kind(&self) -> State {
        State::of(self.state.as_deref())
    }

    /// Not a client backend (autovacuum, the checkpointer, WAL senders, parallel workers…).
    pub fn is_background(&self) -> bool {
        match self.backend_type.as_deref() {
            Some(t) => t != "client backend",
            // Before PostgreSQL 10: background processes have no state.
            None => self.state.is_none(),
        }
    }

    pub fn is_idle(&self) -> bool {
        self.state_kind() == State::Idle
    }

    /// Shown with the default filter: not idle, not a background process.
    pub fn shown_by_default(&self) -> bool {
        !self.is_idle() && !self.is_background()
    }

    /// A long-running statement or a transaction left idle, past the thresholds.
    pub fn severity(&self, th: &Thresholds) -> Option<Severity> {
        let level = |age: Option<i64>, warn: i64, err: i64| match age {
            Some(a) if a >= err => Some(Severity::Err),
            Some(a) if a >= warn => Some(Severity::Warn),
            _ => None,
        };
        match self.state_kind() {
            State::Active | State::FastPath if !self.is_background() => level(self.query_age, th.query_warn, th.query_err),
            State::IdleInTx | State::Aborted => level(self.state_age.or(self.xact_age), th.idle_tx_warn, th.idle_tx_err),
            _ => None,
        }
    }

    /// The query text, unless the server withheld it.
    pub fn visible_query(&self) -> Option<&str> {
        self.query.as_deref().filter(|q| !q.trim().is_empty() && !is_withheld(q))
    }

    /// The text the filter looks in.
    pub fn haystack(&self) -> String {
        let parts = [
            Some(self.pid.to_string()),
            self.user.clone(),
            self.db.clone(),
            self.app.clone(),
            self.client.clone(),
            self.state.clone(),
            self.wait.clone(),
            self.backend_type.clone(),
            self.query.clone(),
        ];
        parts.into_iter().flatten().collect::<Vec<_>>().join("\u{1}").to_lowercase()
    }
}

/// `<insufficient privilege>`: the server hides another role's query from this user.
pub fn is_withheld(q: &str) -> bool {
    q.trim() == "<insufficient privilege>"
}

/// The sessions query for a server of major version `major` (0: unknown, taken as recent).
/// Before PostgreSQL 10 there is no backend type, and the waits are left out (9.6 had them,
/// but `major` cannot tell 9.6 from 9.5).
pub fn sessions_sql(major: u32) -> String {
    let recent = major == 0 || major >= 10;
    let backend_type = if recent { "a.backend_type" } else { "NULL::text" };
    let (wait_type, wait) = if recent { ("a.wait_event_type", "a.wait_event") } else { ("NULL::text", "NULL::text") };
    let blocking = if recent { "pg_catalog.pg_blocking_pids(a.pid)::text" } else { "'{}'::text" };
    format!(
        "SELECT a.pid, a.usename, a.datname, a.application_name, \
         CASE WHEN a.client_addr IS NOT NULL THEN pg_catalog.host(a.client_addr) || ':' || a.client_port \
              WHEN a.client_port = -1 THEN 'local' END, \
         a.state, {wait_type}, {wait}, \
         round(extract(epoch FROM now() - a.xact_start))::int8, \
         round(extract(epoch FROM now() - a.query_start))::int8, \
         round(extract(epoch FROM now() - a.state_change))::int8, \
         round(extract(epoch FROM now() - a.backend_start))::int8, \
         {backend_type}, a.query, {blocking}, a.pid = pg_catalog.pg_backend_pid() \
         FROM pg_catalog.pg_stat_activity a ORDER BY a.pid"
    )
}

pub fn parse_sessions(rows: &[Vec<Option<String>>]) -> Vec<SessionRow> {
    rows.iter()
        .filter_map(|r| {
            Some(SessionRow {
                pid: int(r, 0)? as i32,
                user: text(r, 1),
                db: text(r, 2),
                app: text(r, 3),
                client: text(r, 4),
                state: text(r, 5),
                wait_type: text(r, 6),
                wait: text(r, 7),
                xact_age: int(r, 8),
                query_age: int(r, 9),
                state_age: int(r, 10),
                backend_age: int(r, 11),
                backend_type: text(r, 12),
                query: cell(r, 13),
                blocked_by: int_array(cell(r, 14).as_deref().unwrap_or("")),
                is_self: cell(r, 15).as_deref().is_some_and(|b| b == "t" || b == "true"),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Locks

/// A session waiting on another: what it waits for, and what the blocker holds there.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LockEdge {
    pub waiter: i32,
    pub blocker: i32,
    /// `relation`, `transactionid` (a row lock), `tuple`, `advisory`…
    pub locktype: Option<String>,
    /// The mode the waiter asks for.
    pub mode: Option<String>,
    pub relation: Option<String>,
    /// Seconds the waiter has waited.
    pub wait: Option<i64>,
    /// The modes the blocker holds on that relation (or transaction).
    pub held: Option<String>,
}

/// The edges query: one row per (waiter, blocker) pair from `pg_blocking_pids()`, with the lock
/// the waiter asks for (a row lock is a wait on the holder's transaction id; the relation then
/// comes from the waiter's tuple lock) and the modes the blocker holds there.
pub fn locks_sql(major: u32) -> String {
    let waitstart = major == 0 || major >= 14;
    let (ws_col, since) = if waitstart { (", l.waitstart", "coalesce(wl.waitstart, w.state_change)") } else { ("", "w.state_change") };
    format!(
        "SELECT w.pid, b.pid, wl.locktype, wl.mode, coalesce(wl.relation, tl.relation)::regclass::text, \
         round(extract(epoch FROM now() - {since}))::int8, \
         (SELECT string_agg(DISTINCT hl.mode, ', ' ORDER BY hl.mode) FROM pg_catalog.pg_locks hl \
           WHERE hl.pid = b.pid AND hl.granted \
             AND (hl.relation = coalesce(wl.relation, tl.relation) \
                  OR (hl.locktype = 'transactionid' AND hl.transactionid = wl.transactionid))) \
         FROM pg_catalog.pg_stat_activity w \
         CROSS JOIN LATERAL unnest(pg_catalog.pg_blocking_pids(w.pid)) AS b(pid) \
         LEFT JOIN LATERAL (SELECT l.locktype, l.mode, l.relation, l.transactionid{ws_col} FROM pg_catalog.pg_locks l \
           WHERE l.pid = w.pid AND NOT l.granted LIMIT 1) wl ON true \
         LEFT JOIN LATERAL (SELECT l.relation FROM pg_catalog.pg_locks l \
           WHERE l.pid = w.pid AND l.locktype = 'tuple' LIMIT 1) tl ON true \
         ORDER BY 1, 2"
    )
}

pub fn parse_locks(rows: &[Vec<Option<String>>]) -> Vec<LockEdge> {
    rows.iter()
        .filter_map(|r| {
            Some(LockEdge {
                waiter: int(r, 0)? as i32,
                blocker: int(r, 1)? as i32,
                locktype: text(r, 2),
                mode: text(r, 3),
                relation: text(r, 4),
                wait: int(r, 5),
                held: text(r, 6),
            })
        })
        .filter(|e| e.waiter != e.blocker)
        .collect()
}

/// One line of the blocking tree.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub pid: i32,
    pub depth: usize,
    /// How this session waits on the row above it (`None` at the top).
    pub edge: Option<LockEdge>,
    /// Part of a wait cycle (a deadlock the server has not broken yet).
    pub cycle: bool,
    /// Already shown under another blocker (a session waiting on several): not expanded again.
    pub again: bool,
    /// Sessions waiting behind it, directly or not.
    pub behind: usize,
}

/// The blocking tree from the edges: the blockers that wait on nobody at the top (the most
/// sessions behind them first), the sessions waiting on each as its children, recursively. A
/// session waiting on several blockers appears under each (`again` after the first); sessions
/// that only wait on each other (a cycle) start from the lowest pid, marked `cycle`.
pub fn blocking_tree(edges: &[LockEdge]) -> Vec<TreeRow> {
    let mut children: BTreeMap<i32, Vec<&LockEdge>> = BTreeMap::new();
    let mut waiting: BTreeSet<i32> = BTreeSet::new();
    let mut nodes: BTreeSet<i32> = BTreeSet::new();
    let mut seen_pairs = HashSet::new();
    for e in edges {
        if e.waiter == e.blocker || !seen_pairs.insert((e.waiter, e.blocker)) {
            continue;
        }
        children.entry(e.blocker).or_default().push(e);
        waiting.insert(e.waiter);
        nodes.insert(e.waiter);
        nodes.insert(e.blocker);
    }
    // Everyone behind a node (the cycles included, counted once).
    let behind = |root: i32| -> usize {
        let mut seen = HashSet::from([root]);
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            for e in children.get(&n).into_iter().flatten() {
                if seen.insert(e.waiter) {
                    stack.push(e.waiter);
                }
            }
        }
        seen.len() - 1
    };
    let counts: HashMap<i32, usize> = nodes.iter().map(|&n| (n, behind(n))).collect();
    let mut roots: Vec<i32> = nodes.iter().copied().filter(|n| !waiting.contains(n)).collect();
    roots.sort_by(|a, b| counts[b].cmp(&counts[a]).then(a.cmp(b)));

    struct Walk<'a> {
        children: &'a BTreeMap<i32, Vec<&'a LockEdge>>,
        counts: &'a HashMap<i32, usize>,
        shown: HashSet<i32>,
        out: Vec<TreeRow>,
    }
    impl Walk<'_> {
        /// `path`: the sessions above this one, with their rows.
        fn visit(&mut self, pid: i32, depth: usize, edge: Option<LockEdge>, path: &mut Vec<(i32, usize)>) {
            if let Some(k) = path.iter().position(|(p, _)| *p == pid) {
                // Back to a session on this path: the cycle closes here, and every session
                // from there down is on it.
                for &(_, row) in &path[k..] {
                    self.out[row].cycle = true;
                }
                self.out.push(TreeRow { pid, depth, edge, cycle: true, again: true, behind: self.counts[&pid] });
                return;
            }
            let again = !self.shown.insert(pid);
            self.out.push(TreeRow { pid, depth, edge, cycle: false, again, behind: self.counts[&pid] });
            if again {
                return;
            }
            path.push((pid, self.out.len() - 1));
            let kids: Vec<LockEdge> = self.children.get(&pid).into_iter().flatten().map(|e| (*e).clone()).collect();
            for e in kids {
                self.visit(e.waiter, depth + 1, Some(e), path);
            }
            path.pop();
        }
    }
    let mut w = Walk { children: &children, counts: &counts, shown: HashSet::new(), out: Vec::new() };
    for r in roots {
        w.visit(r, 0, None, &mut Vec::new());
    }
    // What is left only waits on itself, around a cycle.
    for &n in &nodes {
        if !w.shown.contains(&n) {
            w.visit(n, 0, None, &mut Vec::new());
        }
    }
    w.out
}

// ---------------------------------------------------------------------------------------------
// Tables

/// One table's health: dead tuples, (auto)vacuum and (auto)analyze ages, scans, sizes and the
/// bloat estimate.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableRow {
    pub schema: String,
    pub name: String,
    pub live: Option<i64>,
    pub dead: Option<i64>,
    pub mod_since_analyze: Option<i64>,
    /// Seconds since the last manual and automatic vacuum and analyze (`None`: never).
    pub vacuum_age: Option<i64>,
    pub autovacuum_age: Option<i64>,
    pub analyze_age: Option<i64>,
    pub autoanalyze_age: Option<i64>,
    pub seq_scan: Option<i64>,
    pub idx_scan: Option<i64>,
    pub table_bytes: Option<i64>,
    pub index_bytes: Option<i64>,
    /// The estimate: wasted bytes and their share of the table (`None`: no estimate).
    pub bloat_bytes: Option<i64>,
    pub bloat_pct: Option<f64>,
}

impl TableRow {
    /// Dead tuples among all, 0–1.
    pub fn dead_ratio(&self) -> Option<f64> {
        let (l, d) = (self.live?, self.dead?);
        (l + d > 0).then(|| d as f64 / (l + d) as f64)
    }

    /// Sequential scans among all scans, 0–1.
    pub fn seq_ratio(&self) -> Option<f64> {
        let s = self.seq_scan.unwrap_or(0);
        let i = self.idx_scan.unwrap_or(0);
        (s + i > 0).then(|| s as f64 / (s + i) as f64)
    }

    /// The more recent of the two (`None`: never).
    pub fn last_vacuum(&self) -> Option<i64> {
        min_some(self.vacuum_age, self.autovacuum_age)
    }

    pub fn last_analyze(&self) -> Option<i64> {
        min_some(self.analyze_age, self.autoanalyze_age)
    }
}

fn min_some(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The table statistics with their sizes. The size functions take a share lock on each table:
/// `lock_timeout` keeps a table locked exclusively (VACUUM FULL, ALTER) from stalling the
/// monitor (the SET is rolled back with the implicit transaction if the SELECT fails); the second
/// query does without sizes.
pub fn tables_sql() -> [String; 2] {
    let cols = "s.schemaname, s.relname, s.n_live_tup, s.n_dead_tup, s.n_mod_since_analyze, \
         round(extract(epoch FROM now() - s.last_vacuum))::int8, \
         round(extract(epoch FROM now() - s.last_autovacuum))::int8, \
         round(extract(epoch FROM now() - s.last_analyze))::int8, \
         round(extract(epoch FROM now() - s.last_autoanalyze))::int8, \
         s.seq_scan, s.idx_scan";
    [
        format!(
            "SET lock_timeout = '500ms'; \
             SELECT {cols}, pg_catalog.pg_table_size(s.relid), pg_catalog.pg_indexes_size(s.relid) \
             FROM pg_catalog.pg_stat_user_tables s ORDER BY s.n_dead_tup DESC NULLS LAST, 1, 2; \
             RESET lock_timeout"
        ),
        format!("SELECT {cols}, NULL, NULL FROM pg_catalog.pg_stat_user_tables s ORDER BY s.n_dead_tup DESC NULLS LAST, 1, 2"),
    ]
}

pub fn parse_tables(rows: &[Vec<Option<String>>]) -> Vec<TableRow> {
    rows.iter()
        .filter_map(|r| {
            Some(TableRow {
                schema: text(r, 0)?,
                name: text(r, 1)?,
                live: int(r, 2),
                dead: int(r, 3),
                mod_since_analyze: int(r, 4),
                vacuum_age: int(r, 5),
                autovacuum_age: int(r, 6),
                analyze_age: int(r, 7),
                autoanalyze_age: int(r, 8),
                seq_scan: int(r, 9),
                idx_scan: int(r, 10),
                table_bytes: int(r, 11),
                index_bytes: int(r, 12),
                ..Default::default()
            })
        })
        .collect()
}

/// The usual lightweight table bloat estimate (after ioguix's `table_bloat` query): the
/// expected pages from the planner's statistics (average widths, null fractions, the tuple
/// header and alignment, the fill factor) against the pages the table has. It reads only the
/// catalog and `pg_stats`, takes no lock, and is an estimate: a table never analyzed, or with a
/// `name` column, gives none.
pub fn bloat_sql() -> &'static str {
    "SELECT schemaname, tblname, \
       CASE WHEN tblpages > 0 AND tblpages - est_pages_ff > 0 AND NOT is_na THEN ((tblpages - est_pages_ff) * bs)::int8 ELSE 0 END, \
       CASE WHEN tblpages > 0 AND tblpages - est_pages_ff > 0 AND NOT is_na THEN round((100 * (tblpages - est_pages_ff) / tblpages)::numeric, 1) ELSE 0 END, \
       is_na \
     FROM ( \
       SELECT ceil(reltuples / ((bs - page_hdr) * fillfactor / (tpl_size * 100))) + ceil(toasttuples / 4) AS est_pages_ff, \
              tblpages, bs, schemaname, tblname, is_na \
       FROM ( \
         SELECT (4 + tpl_hdr_size + tpl_data_size + (2 * ma) \
                 - CASE WHEN tpl_hdr_size % ma = 0 THEN ma ELSE tpl_hdr_size % ma END \
                 - CASE WHEN ceil(tpl_data_size)::int % ma = 0 THEN ma ELSE ceil(tpl_data_size)::int % ma END) AS tpl_size, \
                heappages + toastpages AS tblpages, reltuples, toasttuples, bs, page_hdr, schemaname, tblname, fillfactor, is_na \
         FROM ( \
           SELECT ns.nspname AS schemaname, tbl.relname AS tblname, tbl.reltuples, \
                  tbl.relpages AS heappages, coalesce(toast.relpages, 0) AS toastpages, \
                  coalesce(toast.reltuples, 0) AS toasttuples, \
                  coalesce(substring(array_to_string(tbl.reloptions, ' ') FROM 'fillfactor=([0-9]+)')::smallint, 100) AS fillfactor, \
                  current_setting('block_size')::numeric AS bs, 8 AS ma, 24 AS page_hdr, \
                  23 + CASE WHEN max(coalesce(s.null_frac, 0)) > 0 THEN (7 + count(s.attname)) / 8 ELSE 0::int END AS tpl_hdr_size, \
                  sum((1 - coalesce(s.null_frac, 0)) * coalesce(s.avg_width, 0)) AS tpl_data_size, \
                  bool_or(att.atttypid = 'pg_catalog.name'::regtype) \
                    OR sum(CASE WHEN att.attnum > 0 THEN 1 ELSE 0 END) <> count(s.attname) AS is_na \
           FROM pg_catalog.pg_attribute att \
           JOIN pg_catalog.pg_class tbl ON att.attrelid = tbl.oid \
           JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace \
           LEFT JOIN pg_catalog.pg_stats s ON s.schemaname = ns.nspname AND s.tablename = tbl.relname \
                AND s.inherited = false AND s.attname = att.attname \
           LEFT JOIN pg_catalog.pg_class toast ON tbl.reltoastrelid = toast.oid \
           WHERE NOT att.attisdropped AND att.attnum > 0 AND tbl.relkind = 'r' \
             AND ns.nspname NOT IN ('pg_catalog', 'information_schema') AND ns.nspname !~ '^pg_toast' \
           GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10 \
         ) AS s \
       ) AS s2 \
     ) AS s3"
}

/// Adds the bloat estimate (schema, table, bytes, percent, not applicable) to the tables.
pub fn merge_bloat(tables: &mut [TableRow], rows: &[Vec<Option<String>>]) {
    let est: HashMap<(String, String), (i64, f64)> = rows
        .iter()
        .filter(|r| !cell(r, 4).as_deref().is_some_and(|b| b == "t" || b == "true"))
        .filter_map(|r| Some(((text(r, 0)?, text(r, 1)?), (int(r, 2)?, float(r, 3)?))))
        .collect();
    for t in tables {
        if let Some(&(b, p)) = est.get(&(t.schema.clone(), t.name.clone())) {
            t.bloat_bytes = Some(b);
            t.bloat_pct = Some(p);
        }
    }
}

/// What the Tables section's actions open in a new tab (never run from the monitor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Maintenance {
    Vacuum,
    Analyze,
    VacuumAnalyze,
}

pub fn maintenance_template(schema: &str, table: &str, m: Maintenance) -> String {
    let t = qualified(Some(schema), table);
    match m {
        Maintenance::Vacuum => format!("VACUUM (VERBOSE) {t};"),
        Maintenance::Analyze => format!("ANALYZE (VERBOSE) {t};"),
        Maintenance::VacuumAnalyze => format!("VACUUM (VERBOSE, ANALYZE) {t};"),
    }
}

// ---------------------------------------------------------------------------------------------
// Statements

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatementRow {
    pub queryid: Option<String>,
    /// Normalized: constants are `$1`, `$2`…
    pub query: String,
    pub user: Option<String>,
    pub db: Option<String>,
    pub calls: i64,
    pub total_ms: f64,
    pub mean_ms: f64,
    pub min_ms: Option<f64>,
    pub max_ms: Option<f64>,
    pub stddev_ms: Option<f64>,
    pub rows: i64,
    pub shared_hit: i64,
    pub shared_read: i64,
    pub temp_blocks: i64,
    /// Planning, PostgreSQL 13+ (counted only with `pg_stat_statements.track_planning`).
    pub plans: Option<i64>,
    pub plan_ms: Option<f64>,
}

impl StatementRow {
    /// Shared buffer hits among hits and reads, 0–1.
    pub fn hit_ratio(&self) -> Option<f64> {
        let all = self.shared_hit + self.shared_read;
        (all > 0).then(|| self.shared_hit as f64 / all as f64)
    }
}

/// What `pg_stat_statements` gives in this database.
#[derive(Clone, Debug, PartialEq)]
pub enum Statements {
    /// The extension is not created in this database.
    NotInstalled,
    /// Created, but the library is not in `shared_preload_libraries`.
    NotLoaded,
    Rows(Vec<StatementRow>),
    Failed(String),
}

pub const CREATE_PGSS: &str = "CREATE EXTENSION IF NOT EXISTS pg_stat_statements;";
pub const RESET_PGSS: &str = "SELECT pg_stat_statements_reset()";
pub const STATEMENTS_LIMIT: usize = 500;

/// The top statements by total time; the column names changed in PostgreSQL 13
/// (`total_exec_time`, planning counters).
pub fn statements_sql(major: u32) -> String {
    let exec = major == 0 || major >= 13;
    let (total, mean, min, max, dev, plans, plan_ms) = if exec {
        ("s.total_exec_time", "s.mean_exec_time", "s.min_exec_time", "s.max_exec_time", "s.stddev_exec_time", "s.plans", "s.total_plan_time")
    } else {
        ("s.total_time", "s.mean_time", "s.min_time", "s.max_time", "s.stddev_time", "NULL::int8", "NULL::float8")
    };
    format!(
        "SELECT s.queryid::text, s.query, r.rolname, d.datname, s.calls, {total}, {mean}, {min}, {max}, {dev}, s.rows, \
         s.shared_blks_hit, s.shared_blks_read, s.temp_blks_read + s.temp_blks_written, {plans}, {plan_ms} \
         FROM pg_stat_statements s \
         LEFT JOIN pg_catalog.pg_roles r ON r.oid = s.userid \
         LEFT JOIN pg_catalog.pg_database d ON d.oid = s.dbid \
         ORDER BY {total} DESC LIMIT {STATEMENTS_LIMIT}"
    )
}

pub fn parse_statements(rows: &[Vec<Option<String>>]) -> Vec<StatementRow> {
    rows.iter()
        .filter_map(|r| {
            Some(StatementRow {
                queryid: text(r, 0),
                query: cell(r, 1).unwrap_or_default(),
                user: text(r, 2),
                db: text(r, 3),
                calls: int(r, 4)?,
                total_ms: float(r, 5)?,
                mean_ms: float(r, 6).unwrap_or(0.),
                min_ms: float(r, 7),
                max_ms: float(r, 8),
                stddev_ms: float(r, 9),
                rows: int(r, 10).unwrap_or(0),
                shared_hit: int(r, 11).unwrap_or(0),
                shared_read: int(r, 12).unwrap_or(0),
                temp_blocks: int(r, 13).unwrap_or(0),
                plans: int(r, 14),
                plan_ms: float(r, 15),
            })
        })
        .collect()
}

/// The extension's state from the failed query: `55000` (object not in prerequisite state) is
/// the "must be loaded via shared_preload_libraries" error.
pub fn statements_error(code: Option<&str>, message: &str) -> Statements {
    if code == Some("55000") || message.contains("shared_preload_libraries") {
        Statements::NotLoaded
    } else if code == Some("42P01") {
        Statements::NotInstalled
    } else {
        Statements::Failed(message.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Signals

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// `pg_cancel_backend`: stops the running statement.
    Cancel,
    /// `pg_terminate_backend`: ends the session (its transaction rolls back).
    Terminate,
}

impl Signal {
    pub fn sql(self, pid: i32) -> String {
        match self {
            Signal::Cancel => format!("SELECT pg_catalog.pg_cancel_backend({pid})"),
            Signal::Terminate => format!("SELECT pg_catalog.pg_terminate_backend({pid})"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Signal::Cancel => "Cancel",
            Signal::Terminate => "Terminate",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Formatting

/// `<1s` → `0s`, `42s`, `3m 05s`, `2h 07m`, `4d 3h`: an age in whole seconds.
pub fn fmt_age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h {:02}m", s / 3600, (s / 60) % 60),
        _ => format!("{}d {}h", s / 86_400, (s / 3600) % 24),
    }
}

/// A coarser age for things that happen daily (vacuum, analyze): `42s`, `12m`, `5h`, `3d`.
pub fn fmt_age_coarse(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// `0.08 ms`, `12.4 ms`, `318 ms`, `2.41 s`, `3m 12s`.
pub fn fmt_ms(ms: f64) -> String {
    let ms = ms.max(0.);
    if ms < 1. {
        format!("{ms:.2} ms")
    } else if ms < 100. {
        format!("{ms:.1} ms")
    } else if ms < 1000. {
        format!("{ms:.0} ms")
    } else if ms < 60_000. {
        format!("{:.2} s", ms / 1000.)
    } else {
        fmt_age((ms / 1000.).round() as i64)
    }
}

/// `37%`, `0.4%`, `100%`: a 0–1 share.
pub fn fmt_pct(x: f64) -> String {
    let p = x * 100.;
    if p > 0. && p < 1. { format!("{p:.1}%") } else { format!("{:.0}%", p.round()) }
}

pub fn fmt_count(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

// ---------------------------------------------------------------------------------------------
// Cells

fn cell(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

fn text(r: &[Option<String>], i: usize) -> Option<String> {
    cell(r, i).filter(|s| !s.is_empty())
}

fn int(r: &[Option<String>], i: usize) -> Option<i64> {
    let s = cell(r, i)?;
    let s = s.trim();
    s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| f.round() as i64))
}

fn float(r: &[Option<String>], i: usize) -> Option<f64> {
    cell(r, i)?.trim().parse::<f64>().ok().filter(|f| f.is_finite())
}

/// `{12,345}` → `[12, 345]`.
fn int_array(s: &str) -> Vec<i32> {
    s.trim_matches(|c| c == '{' || c == '}').split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

/// The SQLSTATE of a failed query, if the server sent one.
fn sqlstate(e: &anyhow::Error) -> Option<String> {
    crate::backend::server_error(e).and_then(|i| i.code)
}

fn message(e: &anyhow::Error) -> String {
    match crate::backend::server_error(e) {
        Some(i) => i.message,
        None => format!("{e:#}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Reading

impl Connection {
    /// The monitor's client for `db`, on an engine that has the monitor ([`Caps::activity`]).
    ///
    /// [`Caps::activity`]: crate::Caps::activity
    async fn monitor(&self, db: &str) -> anyhow::Result<crate::backend::Db> {
        anyhow::ensure!(self.caps().activity, "{} has no activity monitor", self.engine().label());
        self.meta(db).await
    }

    /// Whether backend `pid` of this server is one of this process's (an editor tab, the
    /// explorer, the monitor, an export…).
    pub fn owns_backend(&self, pid: i32) -> bool {
        crate::own::is_own(&crate::own::server_key(&self.profile), pid)
    }

    /// Every backend of `pg_stat_activity`.
    pub fn activity_sessions(self: &Arc<Self>, db: &str) -> Task<Vec<SessionRow>> {
        let (this, db) = (self.clone(), db.to_string());
        let sql = sessions_sql(self.info.major());
        self.spawn(async move {
            let client = this.monitor(&db).await?;
            Ok(parse_sessions(&text_rows(&client, &sql).await?))
        })
    }

    /// The waits between sessions.
    pub fn activity_locks(self: &Arc<Self>, db: &str) -> Task<Vec<LockEdge>> {
        let (this, db) = (self.clone(), db.to_string());
        let sql = locks_sql(self.info.major());
        self.spawn(async move {
            let client = this.monitor(&db).await?;
            Ok(parse_locks(&text_rows(&client, &sql).await?))
        })
    }

    /// Table health, with the bloat estimate (`bloat`) when it can be computed.
    pub fn activity_tables(self: &Arc<Self>, db: &str, bloat: bool) -> Task<Vec<TableRow>> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            let client = this.monitor(&db).await?;
            let qs = tables_sql();
            let mut tables = parse_tables(&first_ok(&client, &[&qs[0], &qs[1]]).await?);
            if bloat
                && !tables.is_empty()
                && let Ok(rows) = text_rows(&client, bloat_sql()).await
            {
                merge_bloat(&mut tables, &rows);
            }
            Ok(tables)
        })
    }

    /// The top statements, or why there are none.
    pub fn activity_statements(self: &Arc<Self>, db: &str) -> Task<Statements> {
        let (this, db) = (self.clone(), db.to_string());
        let sql = statements_sql(self.info.major());
        self.spawn(async move {
            let client = this.monitor(&db).await?;
            let ext = text_rows(&client, "SELECT extversion FROM pg_catalog.pg_extension WHERE extname = 'pg_stat_statements'").await?;
            if ext.is_empty() {
                return Ok(Statements::NotInstalled);
            }
            Ok(match text_rows(&client, &sql).await {
                Ok(rows) => Statements::Rows(parse_statements(&rows)),
                Err(e) => statements_error(sqlstate(&e).as_deref(), &message(&e)),
            })
        })
    }

    /// `pg_cancel_backend` / `pg_terminate_backend`: whether the server signalled the process.
    /// A refusal (another role's session without the right) is the error.
    pub fn signal_backend(self: &Arc<Self>, db: &str, pid: i32, signal: Signal) -> Task<bool> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            let client = this.monitor(&db).await?;
            let rows = text_rows(&client, &signal.sql(pid)).await.map_err(|e| anyhow::anyhow!(message(&e)))?;
            Ok(rows.first().and_then(|r| r.first().cloned().flatten()).is_some_and(|b| b == "t" || b == "true"))
        })
    }

    /// `pg_stat_statements_reset()`.
    pub fn reset_statements(self: &Arc<Self>, db: &str) -> Task<()> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            let client = this.monitor(&db).await?;
            text_rows(&client, RESET_PGSS).await.map_err(|e| anyhow::anyhow!(message(&e)))?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|c| c.map(str::to_string)).collect()
    }

    fn edge(w: i32, b: i32) -> LockEdge {
        LockEdge { waiter: w, blocker: b, ..Default::default() }
    }

    /// (pid, depth, cycle, again) of each tree row.
    fn shape(rows: &[TreeRow]) -> Vec<(i32, usize, bool, bool)> {
        rows.iter().map(|r| (r.pid, r.depth, r.cycle, r.again)).collect()
    }

    #[test]
    fn builders_follow_the_version() {
        let s18 = sessions_sql(18);
        assert!(s18.contains("a.backend_type") && s18.contains("pg_blocking_pids") && s18.contains("a.wait_event_type"));
        assert!(s18.contains("pg_backend_pid()"));
        let s96 = sessions_sql(9);
        assert!(s96.contains("NULL::text") && !s96.contains("a.backend_type"));
        assert!(!s96.contains("pg_blocking_pids"), "9.5 and older have neither waits nor blocking pids");
        assert_eq!(sessions_sql(0), s18, "unknown is taken as recent");
        assert!(locks_sql(14).contains("l.waitstart") && locks_sql(14).contains("coalesce(wl.waitstart, w.state_change)"));
        assert!(!locks_sql(13).contains("waitstart") && locks_sql(13).contains("now() - w.state_change"));
        let st = statements_sql(13);
        assert!(st.contains("total_exec_time") && st.contains("s.plans") && st.contains("LIMIT 500"));
        let st12 = statements_sql(12);
        assert!(st12.contains("s.total_time") && st12.contains("NULL::int8") && !st12.contains("exec_time"));
        let [with, without] = tables_sql();
        assert!(with.starts_with("SET lock_timeout") && with.trim_end().ends_with("RESET lock_timeout"));
        assert!(with.contains("pg_table_size") && !without.contains("pg_table_size"));
        assert!(bloat_sql().contains("pg_stats") && bloat_sql().contains("fillfactor"));
        assert_eq!(Signal::Cancel.sql(42), "SELECT pg_catalog.pg_cancel_backend(42)");
        assert_eq!(Signal::Terminate.sql(7), "SELECT pg_catalog.pg_terminate_backend(7)");
        assert_eq!(maintenance_template("public", "orders", Maintenance::Vacuum), "VACUUM (VERBOSE) public.orders;");
        assert_eq!(maintenance_template("My Schema", "t", Maintenance::Analyze), "ANALYZE (VERBOSE) \"My Schema\".t;");
        assert_eq!(maintenance_template("public", "order", Maintenance::VacuumAnalyze), "VACUUM (VERBOSE, ANALYZE) public.\"order\";");
    }

    #[test]
    fn parses_sessions() {
        let rows = vec![
            row(&[
                Some("4242"),
                Some("app"),
                Some("shop"),
                Some("psql"),
                Some("10.0.0.5:51234"),
                Some("idle in transaction"),
                Some("Client"),
                Some("ClientRead"),
                Some("95"),
                Some("90"),
                Some("88"),
                Some("4000"),
                Some("client backend"),
                Some("UPDATE orders SET paid = true WHERE id = 1"),
                Some("{}"),
                Some("f"),
            ]),
            row(&[Some("31"), None, None, Some(""), None, None, Some("Activity"), Some("BgwriterMain"), None, None, None, Some("9"), Some("background writer"), Some(""), Some("{}"), Some("f")]),
            row(&[Some("77"), Some("app"), Some("shop"), Some("Dataman"), Some("local"), Some("active"), Some("Lock"), Some("transactionid"), Some("5"), Some("400"), Some("400"), Some("60"), Some("client backend"), Some("UPDATE orders SET paid = false WHERE id = 1"), Some("{4242,12}"), Some("t")]),
            row(&[Some("not a pid")]),
        ];
        let s = parse_sessions(&rows);
        assert_eq!(s.len(), 3, "a row without a pid is skipped");
        assert_eq!(s[0].state_kind(), State::IdleInTx);
        assert_eq!((s[0].xact_age, s[0].query_age, s[0].state_age), (Some(95), Some(90), Some(88)));
        assert!(!s[0].is_background() && s[0].shown_by_default());
        assert_eq!(s[0].severity(&Thresholds::default()), Some(Severity::Warn), "idle in transaction for 88 s");
        assert!(s[1].is_background() && !s[1].shown_by_default());
        assert_eq!(s[1].app, None, "empty is nothing");
        assert_eq!(s[1].visible_query(), None);
        assert_eq!(s[2].blocked_by, vec![4242, 12]);
        assert!(s[2].is_self && !s[0].is_self);
        assert_eq!(s[2].severity(&Thresholds::default()), Some(Severity::Err), "active for 400 s");
        assert!(s[2].haystack().contains("dataman") && s[2].haystack().contains("77"));
        // Older servers: no backend type, background processes have no state.
        let old = SessionRow { pid: 1, state: None, ..Default::default() };
        assert!(old.is_background());
        let idle = SessionRow { pid: 2, state: Some("idle".into()), backend_type: Some("client backend".into()), query_age: Some(9999), ..Default::default() };
        assert!(idle.is_idle() && !idle.shown_by_default());
        assert_eq!(idle.severity(&Thresholds::default()), None, "an idle session is never long-running");
        // Another role's query, withheld.
        let hidden = SessionRow { query: Some("<insufficient privilege>".into()), ..Default::default() };
        assert_eq!(hidden.visible_query(), None);
        assert!(is_withheld(" <insufficient privilege> "));
        assert_eq!(State::of(Some("idle in transaction (aborted)")), State::Aborted);
        let aborted = SessionRow { state: Some("idle in transaction (aborted)".into()), backend_type: Some("client backend".into()), state_age: Some(301), ..Default::default() };
        assert_eq!(aborted.severity(&Thresholds::default()), Some(Severity::Err));
    }

    #[test]
    fn parses_locks_and_tables_and_statements() {
        let e = parse_locks(&[
            row(&[Some("20"), Some("10"), Some("transactionid"), Some("ShareLock"), Some("orders"), Some("12"), Some("ExclusiveLock, RowExclusiveLock")]),
            row(&[Some("30"), Some("30"), None, None, None, None, None]),
        ]);
        assert_eq!(e.len(), 1, "a session never blocks itself");
        assert_eq!((e[0].waiter, e[0].blocker, e[0].wait), (20, 10, Some(12)));
        assert_eq!(e[0].held.as_deref(), Some("ExclusiveLock, RowExclusiveLock"));

        let mut t = parse_tables(&[
            row(&[Some("public"), Some("orders"), Some("900"), Some("100"), Some("40"), None, Some("3600"), Some("7200"), None, Some("30"), Some("70"), Some("819200"), Some("16384")]),
            row(&[Some("public"), Some("empty"), Some("0"), Some("0"), Some("0"), None, None, None, None, Some("0"), None, None, None]),
        ]);
        assert_eq!(t[0].dead_ratio(), Some(0.1));
        assert_eq!(t[0].seq_ratio(), Some(0.3));
        assert_eq!(t[0].last_vacuum(), Some(3600));
        assert_eq!(t[0].last_analyze(), Some(7200));
        assert_eq!((t[1].dead_ratio(), t[1].seq_ratio(), t[1].last_vacuum()), (None, None, None));
        merge_bloat(&mut t, &[
            row(&[Some("public"), Some("orders"), Some("163840"), Some("20.0"), Some("f")]),
            row(&[Some("public"), Some("empty"), Some("0"), Some("0"), Some("t")]),
        ]);
        assert_eq!((t[0].bloat_bytes, t[0].bloat_pct), (Some(163_840), Some(20.0)));
        assert_eq!(t[1].bloat_bytes, None, "not applicable: no estimate");

        let s = parse_statements(&[row(&[
            Some("-123"),
            Some("SELECT * FROM orders WHERE id = $1"),
            Some("app"),
            Some("shop"),
            Some("1200"),
            Some("3456.5"),
            Some("2.88"),
            Some("0.1"),
            Some("40.2"),
            Some("1.5"),
            Some("1200"),
            Some("9000"),
            Some("1000"),
            Some("0"),
            Some("1200"),
            Some("10.5"),
        ])]);
        assert_eq!((s[0].calls, s[0].total_ms, s[0].rows), (1200, 3456.5, 1200));
        assert_eq!(s[0].hit_ratio(), Some(0.9));
        assert_eq!(s[0].plans, Some(1200));
        assert_eq!(statements_error(Some("55000"), "pg_stat_statements must be loaded via \"shared_preload_libraries\""), Statements::NotLoaded);
        assert_eq!(statements_error(None, "x must be loaded via shared_preload_libraries"), Statements::NotLoaded);
        assert_eq!(statements_error(Some("42P01"), "relation does not exist"), Statements::NotInstalled);
        assert_eq!(statements_error(Some("42501"), "permission denied"), Statements::Failed("permission denied".into()));
    }

    #[test]
    fn tree_chain_and_fan_out() {
        // 10 blocks 20 and 30; 30 blocks 40. 50 blocks 60.
        let t = blocking_tree(&[edge(20, 10), edge(30, 10), edge(40, 30), edge(60, 50)]);
        assert_eq!(shape(&t), vec![(10, 0, false, false), (20, 1, false, false), (30, 1, false, false), (40, 2, false, false), (50, 0, false, false), (60, 1, false, false)]);
        assert_eq!(t[0].behind, 3, "three sessions behind 10");
        assert_eq!(t[2].behind, 1);
        assert_eq!(t[1].edge.as_ref().map(|e| (e.waiter, e.blocker)), Some((20, 10)));
        assert!(t[0].edge.is_none());
        // The blocker with the most behind it comes first.
        let t = blocking_tree(&[edge(2, 1), edge(4, 3), edge(5, 3)]);
        assert_eq!(t[0].pid, 3);
        // Duplicate edges count once.
        let t = blocking_tree(&[edge(2, 1), edge(2, 1)]);
        assert_eq!(shape(&t), vec![(1, 0, false, false), (2, 1, false, false)]);
        assert!(blocking_tree(&[]).is_empty());
    }

    #[test]
    fn tree_multiple_blockers() {
        // 30 waits on both 10 and 20 (e.g. a lock queue): under each, expanded once.
        let t = blocking_tree(&[edge(30, 10), edge(30, 20), edge(40, 30)]);
        assert_eq!(shape(&t), vec![(10, 0, false, false), (30, 1, false, false), (40, 2, false, false), (20, 0, false, false), (30, 1, false, true)]);
        assert_eq!(t[0].behind, 2);
        assert_eq!(t[3].behind, 2);
    }

    #[test]
    fn tree_cycles() {
        // 1 ↔ 2 (a deadlock in progress): nobody is free, the cycle starts from the lowest pid.
        let t = blocking_tree(&[edge(1, 2), edge(2, 1)]);
        assert_eq!(shape(&t), vec![(1, 0, true, false), (2, 1, true, false), (1, 2, true, true)]);
        // A cycle 3 → 4 → 5 → 3 with 6 waiting on 4, and a free blocker 7 elsewhere.
        let t = blocking_tree(&[edge(4, 3), edge(5, 4), edge(3, 5), edge(6, 4), edge(8, 7)]);
        assert_eq!(t[0].pid, 7, "free blockers first");
        let cyc: Vec<_> = t.iter().skip(2).map(|r| (r.pid, r.depth, r.cycle, r.again)).collect();
        assert_eq!(cyc, vec![(3, 0, true, false), (4, 1, true, false), (5, 2, true, false), (3, 3, true, true), (6, 2, false, false)]);
        // Every session appears, and the walk ends.
        let pids: BTreeSet<i32> = t.iter().map(|r| r.pid).collect();
        assert_eq!(pids, BTreeSet::from([3, 4, 5, 6, 7, 8]));
        // A free blocker feeding into a cycle: walked from the blocker, only the loop marked.
        let t = blocking_tree(&[edge(2, 1), edge(3, 2), edge(2, 3)]);
        assert_eq!(shape(&t), vec![(1, 0, false, false), (2, 1, true, false), (3, 2, true, false), (2, 3, true, true)]);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_age(0), "0s");
        assert_eq!(fmt_age(59), "59s");
        assert_eq!(fmt_age(185), "3m 05s");
        assert_eq!(fmt_age(7620), "2h 07m");
        assert_eq!(fmt_age(4 * 86_400 + 3 * 3600 + 5), "4d 3h");
        assert_eq!(fmt_age(-3), "0s");
        assert_eq!(fmt_age_coarse(720), "12m");
        assert_eq!(fmt_age_coarse(3 * 86_400 + 5), "3d");
        assert_eq!(fmt_ms(0.083), "0.08 ms");
        assert_eq!(fmt_ms(12.44), "12.4 ms");
        assert_eq!(fmt_ms(318.2), "318 ms");
        assert_eq!(fmt_ms(2410.), "2.41 s");
        assert_eq!(fmt_ms(192_000.), "3m 12s");
        assert_eq!(fmt_pct(0.372), "37%");
        assert_eq!(fmt_pct(0.004), "0.4%");
        assert_eq!(fmt_pct(1.), "100%");
        assert_eq!(fmt_pct(0.), "0%");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_count(-1200), "-1,200");
        assert_eq!(int_array("{1,22, 333}"), vec![1, 22, 333]);
        assert!(int_array("{}").is_empty());
        let r = row(&[Some("12.6"), Some("x")]);
        assert_eq!(int(&r, 0), Some(13));
        assert_eq!(int(&r, 1), None);
    }
}
