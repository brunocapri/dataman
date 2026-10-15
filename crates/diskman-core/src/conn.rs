//! Connections, editor sessions and statement execution. The drivers are in [`crate::backend`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use anyhow::Result;
use parking_lot::Mutex;
use tokio::runtime::Handle;

use crate::backend::{self, Caps, Canceller, Db, Dedicated, Ran};
use crate::config::{ConnectionProfile, Engine};
use crate::history::{History, HistoryEntry, Outcome};
use crate::result::{ErrorInfo, Phase, ResultSet};
use crate::schema::SchemaCache;
use crate::server::{ServerExtension, ServerInfo};
use crate::sql::Dialect;
use crate::tx::{TxState, TxStatus, TxWatch};
use crate::{Task, spawn};

pub(crate) use crate::backend::{first_ok, text_rows};

// ---------------------------------------------------------------------------------------------
// Connection

/// An open connection profile: server info, metadata sessions (one per database) and the
/// schema cache. Editor tabs open [`Session`]s on it.
pub struct Connection {
    pub profile: ConnectionProfile,
    pub info: ServerInfo,
    pub schema: Arc<SchemaCache>,
    password: Option<String>,
    rt: Handle,
    history: Arc<History>,
    meta: tokio::sync::Mutex<HashMap<String, Db>>,
    /// The metadata connection negotiated TLS.
    tls_active: bool,
    /// The SSH tunnel every connection of this profile goes through.
    tunnel: Option<Arc<crate::ssh::Tunnel>>,
    /// Per-connection caches of the newer APIs (relationship graphs with inferred LAR links…),
    /// keyed by (what, database, schema).
    pub(crate) caches: Mutex<HashMap<(&'static str, String, String), Arc<dyn std::any::Any + Send + Sync>>>,
    /// The databases' own read-only settings, changed while connected
    /// ([`Connection::set_read_only_for`]); the profile's `read_only` is the default.
    read_only_dbs: parking_lot::RwLock<std::collections::BTreeMap<String, bool>>,
}

impl Connection {
    pub(crate) async fn open(
        rt: Handle,
        profile: ConnectionProfile,
        password: Option<String>,
        history: Arc<History>,
        exts: Vec<Arc<dyn ServerExtension>>,
        tunnel: Option<Arc<crate::ssh::Tunnel>>,
    ) -> Result<Arc<Connection>> {
        let d = backend::dedicated(&profile, password.as_deref(), None, tunnel.as_deref()).await?;
        let (client, tls_active) = (d.db(), d.tls_active());
        drop(d);
        let info = backend::server_info(&client, &exts).await?;
        let db = if info.database.is_empty() { profile.database.clone() } else { info.database.clone() };
        let mut meta = HashMap::new();
        meta.insert(db.clone(), client);
        let schema = Arc::new(SchemaCache::with_dialect(db, info.search_path.clone(), profile.engine.dialect()));
        Ok(Arc::new(Connection {
            read_only_dbs: parking_lot::RwLock::new(profile.read_only_databases.clone()),
            profile,
            info,
            schema,
            password,
            rt,
            history,
            meta: tokio::sync::Mutex::new(meta),
            tls_active,
            tunnel,
            caches: Default::default(),
        }))
    }

    /// The engine this connection speaks to (LAR is a PostgreSQL engine).
    pub fn engine(&self) -> Engine {
        self.profile.engine
    }

    /// The SQL dialect of its statements.
    pub fn dialect(&self) -> Dialect {
        self.profile.engine.dialect()
    }

    /// What the engine can do beyond statements and tables.
    pub fn caps(&self) -> Caps {
        Caps::of(self.profile.engine)
    }

    /// Whether `database` is read-only now: its own setting, else the connection's. Every write
    /// path asks this (the editor, grid edits, imports, the MCP tools).
    pub fn read_only_for(&self, database: &str) -> bool {
        crate::config::read_only_in(self.profile.read_only, &self.read_only_dbs.read(), self.profile.engine, database)
    }

    /// Changes `database`'s read-only state while connected (the explorer's toggle, saved to the
    /// profile by the caller): checks see it at once, and each editor session on it sets its
    /// server session to match before its next statement outside a transaction.
    pub fn set_read_only_for(&self, database: &str, on: bool) {
        crate::config::set_read_only_in(self.profile.read_only, &mut self.read_only_dbs.write(), self.profile.engine, database, on);
    }

    /// The profile a new server session on `database` connects with: read-only as that database
    /// is (PostgreSQL's `default_transaction_read_only`, MySQL's session READ ONLY); SQL
    /// Server's `ApplicationIntent=ReadOnly` only when no database of the connection is writable
    /// (`read_only_everywhere`).
    fn profile_for(&self, database: &str) -> ConnectionProfile {
        let mut p = self.profile.clone();
        p.read_only_databases = self.read_only_dbs.read().clone();
        if p.engine != Engine::SqlServer {
            p.read_only = p.read_only_for(database);
        }
        p
    }

    /// Whether our connection to the server is encrypted (TLS was negotiated).
    pub fn tls_in_use(&self) -> bool {
        self.tls_active
    }

    /// The connection goes through an SSH tunnel.
    pub fn tunneled(&self) -> bool {
        self.tunnel.is_some()
    }

    /// Ends the SSH tunnel, if any: every session through it closes. Called on disconnect.
    pub fn close(&self) {
        if let Some(t) = &self.tunnel {
            t.close();
        }
    }

    /// A new client on its own backend (exports, edits, live queries): never shared with an
    /// editor tab, so its transactions and notifications are its own.
    pub(crate) async fn dedicated(&self, database: Option<&str>) -> Result<Dedicated> {
        let db = database.unwrap_or(self.database());
        backend::dedicated(&self.profile_for(db), self.password.as_deref(), Some(db), self.tunnel.as_deref()).await
    }

    pub fn handle(&self) -> &Handle {
        &self.rt
    }

    /// The database this connection was opened on.
    pub fn database(&self) -> &str {
        self.schema.default_database()
    }

    /// The metadata client for `db`, connecting on first use (PostgreSQL does not query
    /// across databases).
    pub(crate) async fn meta(&self, db: &str) -> Result<Db> {
        let mut g = self.meta.lock().await;
        if let Some(c) = g.get(db)
            && !c.is_closed()
        {
            return Ok(c.clone());
        }
        let client = self.dedicated(Some(db)).await?.db();
        if !self.schema.has_search_path(db) {
            let sp = backend::search_path(&client, &self.info.user).await;
            self.schema.set_search_path(db, sp);
        }
        g.insert(db.to_string(), client.clone());
        Ok(client)
    }

    pub(crate) fn spawn<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Task<T> {
        spawn(&self.rt, fut)
    }

    /// Writes a statement that ran outside any session of this connection (an agent's write,
    /// run on the agent's own backend and shown in its tab) to the connection's history.
    pub fn record_history(&self, entry: &HistoryEntry) {
        if let Err(e) = self.history.append(&self.profile.id, entry) {
            log::warn!("history: {e}");
        }
    }

    /// A new editor session on `database` (the connection's own when `None`). It connects on
    /// its first statement.
    pub fn session(self: &Arc<Self>, database: Option<&str>) -> Arc<Session> {
        Arc::new(Session {
            conn: self.clone(),
            database: database.unwrap_or(self.database()).to_string(),
            client: tokio::sync::Mutex::new(None),
            cancel: Mutex::new(None),
            current: Mutex::new(None),
            queue: Arc::new(tokio::sync::Mutex::new(())),
            tx: Arc::new(TxWatch::default()),
            ro_applied: Mutex::new(None),
        })
    }
}

/// How [`Session::end_transaction`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxEnd {
    Committed,
    /// Rolled back, asked for or because the transaction had failed (a COMMIT of a failed
    /// transaction is a rollback).
    RolledBack,
    /// There was no transaction open.
    NoTransaction,
}

// ---------------------------------------------------------------------------------------------
// Session

/// One server session (one backend), used by one editor tab. Statements run one at a time.
///
/// Its transaction status is the server's (as the driver reads it, [`crate::tx`]). Statements
/// autocommit; a `BEGIN` typed in the editor opens a transaction that stays open until
/// [`Session::end_transaction`] (or a COMMIT / ROLLBACK typed in the editor).
pub struct Session {
    conn: Arc<Connection>,
    pub database: String,
    client: tokio::sync::Mutex<Option<Db>>,
    cancel: Mutex<Option<Canceller>>,
    current: Mutex<Option<Arc<ResultSet>>>,
    queue: Arc<tokio::sync::Mutex<()>>,
    tx: Arc<TxWatch>,
    /// The read-only state the server session was put in (when it connected or since).
    ro_applied: Mutex<Option<bool>>,
}

impl Drop for Session {
    /// A dropped session closes its connection: the server rolls back whatever transaction
    /// it had open.
    fn drop(&mut self) {
        if let Some(rs) = self.current.lock().take()
            && !rs.phase().is_finished()
        {
            rs.abort();
            if let Some(c) = self.cancel.lock().clone() {
                self.conn.rt.spawn(async move {
                    let _ = c.cancel().await;
                });
            }
        }
    }
}

impl Session {
    pub fn connection(&self) -> &Arc<Connection> {
        &self.conn
    }

    pub fn current(&self) -> Option<Arc<ResultSet>> {
        self.current.lock().clone()
    }

    /// The transaction state, as the server last reported it (idle once the connection is
    /// gone: the server rolled back).
    pub fn tx(&self) -> TxState {
        if let Ok(g) = self.client.try_lock()
            && g.as_ref().is_some_and(|c| c.is_closed())
        {
            return TxState::default();
        }
        self.tx.get()
    }

    /// The backend's process id, once the session has connected.
    pub fn backend_pid(&self) -> Option<i32> {
        self.tx.pid()
    }

    /// Runs one statement. Returns at once; the result fills in the background. A statement
    /// still running (or paused with rows left) in this session is cancelled first; inside a
    /// transaction a paused one is read to its end instead (a cancel would abort it).
    pub fn execute(self: &Arc<Self>, sql: impl Into<String>) -> Arc<ResultSet> {
        self.execute_with(sql, Vec::new())
    }

    /// `execute` with values for `$1 … $n`, bound through the extended protocol in text format
    /// (the server parses each as its parameter's type; `None` is NULL).
    pub fn execute_with(self: &Arc<Self>, sql: impl Into<String>, params: Vec<Option<String>>) -> Arc<ResultSet> {
        self.execute_opts(sql, params, false)
    }

    /// `execute_with`, and with `rollback` inside a transaction that is rolled back once it ends
    /// (a savepoint rolled back when a transaction is open): what it writes is undone, whatever
    /// happens (Explain Analyze).
    pub fn execute_opts(self: &Arc<Self>, sql: impl Into<String>, params: Vec<Option<String>>, rollback: bool) -> Arc<ResultSet> {
        let rs = ResultSet::new(sql);
        if !params.is_empty() {
            rs.set_params(params);
        }
        rs.rollback.store(rollback, AtomicOrdering::Release);
        self.start(rs)
    }

    /// An agent's read (`client` names it in the history): checked by the caller with
    /// `guard::agent_read_refusal`, run here in a read-only transaction of its own that is rolled
    /// back once it ends (`BEGIN READ ONLY`; SQL Server, which has none, a plain one: the check is
    /// what keeps it a read there, and on LAR, which does not enforce READ ONLY). Refused when a
    /// transaction is open in the session.
    pub fn execute_agent(self: &Arc<Self>, sql: impl Into<String>, params: Vec<Option<String>>, client: &str) -> Arc<ResultSet> {
        let rs = ResultSet::new(sql);
        if !params.is_empty() {
            rs.set_params(params);
        }
        rs.rollback.store(true, AtomicOrdering::Release);
        rs.read_only.store(true, AtomicOrdering::Release);
        rs.set_agent(client);
        self.start(rs)
    }

    /// Runs `rs` after the statement before it (stopped, or a paused one inside a transaction
    /// read to its end).
    fn start(self: &Arc<Self>, rs: Arc<ResultSet>) -> Arc<ResultSet> {
        let prev = self.current.lock().replace(rs.clone());
        let this = self.clone();
        let out = rs.clone();
        self.conn.rt.spawn(async move {
            if let Some(prev) = prev
                && !prev.phase().is_finished()
            {
                if prev.phase() == Phase::Paused && this.tx.get().status.is_open() {
                    prev.stop_fetching();
                } else {
                    this.stop(&prev).await;
                }
            }
            let _turn = this.queue.clone().lock_owned().await;
            this.run(&rs).await;
        });
        out
    }

    /// Cancels the running statement (CancelRequest), keeping the rows already loaded. A paused
    /// result inside a transaction just stops fetching: a cancel would abort the transaction.
    pub fn cancel(self: &Arc<Self>) {
        let Some(rs) = self.current() else { return };
        if rs.phase().is_finished() {
            return;
        }
        if rs.phase() == Phase::Paused && self.tx.get().status.is_open() {
            rs.stop_fetching();
            return;
        }
        let this = self.clone();
        self.conn.rt.spawn(async move { this.stop(&rs).await });
    }

    /// Ends the open transaction: COMMIT (`commit`) or ROLLBACK, after the statement holding
    /// the session (a paused result is read to its end, never cancelled). Recorded in the
    /// history like a statement.
    pub fn end_transaction(self: &Arc<Self>, commit: bool) -> Task<TxEnd> {
        let this = self.clone();
        spawn(&self.conn.rt, async move {
            if let Some(rs) = this.current()
                && !rs.phase().is_finished()
            {
                rs.stop_fetching();
            }
            let _turn = this.queue.clone().lock_owned().await;
            let before = this.tx().status;
            if !before.is_open() {
                return Ok(TxEnd::NoTransaction);
            }
            let client = this.client().await?;
            let d = this.conn.dialect();
            let sql = if commit { d.commit() } else { d.rollback() };
            let started = Instant::now();
            let r = client.execute(sql).await;
            if r.is_err() {
                this.tx.settled(SETTLE).await;
            }
            this.record_text(sql, started.elapsed(), r.as_ref().err().map(|e| e.to_string()));
            r.map_err(|e| anyhow::anyhow!(e.to_string()))?;
            Ok(if commit && before == TxStatus::Open { TxEnd::Committed } else { TxEnd::RolledBack })
        })
    }

    /// Stops a statement: one that has not reached the server yet never runs; one that is
    /// running (or paused with rows in flight) gets a CancelRequest.
    async fn stop(&self, rs: &ResultSet) {
        use std::sync::atomic::Ordering;
        rs.abort();
        if !rs.executing.load(Ordering::Acquire) {
            return;
        }
        // A paused result whose rows the server already sent drains in moments: then there is
        // nothing to cancel, and a late CancelRequest could hit the next statement instead.
        if tokio::time::timeout(Duration::from_millis(30), rs.wait_finished()).await.is_ok() {
            return;
        }
        let c = self.cancel.lock().clone();
        if let Some(c) = c
            && let Err(e) = c.cancel().await
        {
            log::warn!("cancel request failed: {e}");
        }
    }

    async fn client(&self) -> Result<Db> {
        let mut g = self.client.lock().await;
        if let Some(c) = g.as_ref()
            && !c.is_closed()
        {
            return Ok(c.clone());
        }
        // A new backend: the old one's transaction (if any) is gone.
        self.tx.closed();
        let p = self.conn.profile_for(&self.database);
        let (client, cancel) = backend::session(&p, self.conn.password.as_deref(), Some(&self.database), self.conn.tunnel.as_deref(), &self.tx).await?;
        *self.cancel.lock() = Some(cancel);
        *self.ro_applied.lock() = Some(self.conn.read_only_for(&self.database));
        *g = Some(client.clone());
        Ok(client)
    }

    /// The database's read-only state changed since the server session was set
    /// ([`Connection::set_read_only_for`]): it follows before the next statement, outside a
    /// transaction (PostgreSQL's `default_transaction_read_only`, MySQL's session READ ONLY;
    /// SQL Server has no such setting and relies on the client-side refusal).
    async fn sync_read_only(&self, client: &Db) {
        let want = self.conn.read_only_for(&self.database);
        if *self.ro_applied.lock() == Some(want) || self.tx.get().status.is_open() {
            return;
        }
        let Some(sql) = self.conn.dialect().session_read_only(want) else { return };
        match client.execute(sql).await {
            Ok(_) => *self.ro_applied.lock() = Some(want),
            Err(e) => log::debug!("{sql}: {e}"),
        }
    }

    /// Writes a statement Diskman ran for the user (COMMIT, ROLLBACK) to the history.
    fn record_text(&self, sql: &str, elapsed: Duration, error: Option<String>) {
        let entry = HistoryEntry {
            ts: chrono::Utc::now(),
            database: self.database.clone(),
            sql: sql.to_string(),
            duration_ms: elapsed.as_secs_f64() * 1e3,
            rows: None,
            outcome: if error.is_some() { Outcome::Error } else { Outcome::Ok },
            error,
            source: None,
        };
        if let Err(e) = self.conn.history.append(&self.conn.profile.id, &entry) {
            log::warn!("history: {e}");
        }
    }

    /// Runs a statement Diskman puts around the user's (BEGIN, SAVEPOINT); on failure the
    /// result fails with the server's error.
    async fn wrapper(&self, client: &Db, rs: &ResultSet, sql: &str) -> bool {
        match client.execute(sql).await {
            Ok(_) => true,
            Err(info) => {
                self.tx.settled(SETTLE).await;
                self.record_as(rs, Duration::ZERO, Some(Phase::Failed), None, Some(info.clone()));
                rs.update_status(|s| {
                    s.phase = Some(Phase::Failed);
                    s.elapsed = Some(Duration::ZERO);
                    s.error = Some(info);
                });
                false
            }
        }
    }

    async fn run(&self, rs: &Arc<ResultSet>) {
        let client = match self.client().await {
            Ok(c) => c,
            Err(e) => {
                rs.update_status(|s| {
                    s.phase = Some(Phase::Failed);
                    s.error = Some(ErrorInfo { message: format!("{e:#}"), ..Default::default() });
                });
                self.record(rs, Duration::ZERO);
                return;
            }
        };
        let aborted = || rs.demand().borrow().abort;
        let never_ran = |rs: &ResultSet| {
            rs.update_status(|s| {
                s.phase = Some(Phase::Cancelled);
                s.elapsed = Some(Duration::ZERO);
            });
            self.record(rs, Duration::ZERO);
        };
        // Cancelled while connecting or queued: it must not run at all.
        if aborted() {
            return never_ran(rs);
        }
        self.sync_read_only(&client).await;
        let dialect = self.conn.dialect();
        let facts = crate::guard::classify_in(&rs.sql, dialect);
        let rollback = rs.rolled_back();
        // Rolled back: a transaction of its own, or a savepoint inside the open one (whose work
        // must survive).
        let wrap = if rollback {
            let savepoint = self.tx.get().status.is_open();
            // An agent's read never runs inside a transaction someone else opened: a read-only
            // one of its own, or not at all.
            let read_only = rs.read_only.load(AtomicOrdering::Acquire);
            if read_only && savepoint {
                let info = ErrorInfo { message: "a transaction is open in this tab: an agent's read runs in a read-only transaction of its own".into(), ..Default::default() };
                self.record_as(rs, Duration::ZERO, Some(Phase::Failed), None, Some(info.clone()));
                rs.update_status(|s| {
                    s.phase = Some(Phase::Failed);
                    s.elapsed = Some(Duration::ZERO);
                    s.error = Some(info);
                });
                return;
            }
            let begin = if read_only { dialect.begin_read_only().unwrap_or(dialect.begin()) } else { dialect.begin() };
            let sql = if savepoint { dialect.savepoint(ROLLBACK_SAVEPOINT) } else { begin.to_string() };
            if !self.wrapper(&client, rs, &sql).await {
                return;
            }
            Some(savepoint)
        } else {
            None
        };
        let started = Instant::now();
        // A T-SQL batch returns any number of result sets: each after the first is a result of
        // its own, linked from the one before (`ResultSet::next_result`); the last is finished
        // here, and the statement goes to the history once.
        let last = Mutex::new(rs.clone());
        let on_pause = || {
            let cur = last.lock().clone();
            self.record(&cur, started.elapsed());
        };
        let next = |prev: &Arc<ResultSet>| -> Arc<ResultSet> {
            let new = ResultSet::new(rs.sql.clone());
            new.executing.store(true, AtomicOrdering::Release);
            new.recorded.store(prev.recorded.load(AtomicOrdering::Acquire), AtomicOrdering::Release);
            prev.set_next(new.clone());
            {
                let mut cur = self.current.lock();
                if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, prev)) {
                    *cur = Some(new.clone());
                }
            }
            *last.lock() = new.clone();
            let elapsed = started.elapsed().saturating_sub(prev.status().paused_time());
            prev.update_status(|s| {
                s.elapsed = Some(elapsed);
                s.phase = Some(Phase::Done);
            });
            new
        };
        let r = match backend::run(&client, rs, started, &on_pause, &aborted, &next).await {
            Ran::NeverRan { settle } => {
                end_rollback(&client, dialect, wrap).await;
                if settle {
                    self.tx.settled(SETTLE).await;
                }
                return never_ran(rs);
            }
            r => r,
        };
        let rs = &last.lock().clone();
        // Running time: the pauses (the reader had what it asked for) are left out.
        let elapsed = started.elapsed().saturating_sub(rs.status().paused_time());
        // Undone before anyone sees the end: the transaction state is back to what it was.
        end_rollback(&client, dialect, wrap).await;
        let aborted = *rs.demand().borrow();
        let (phase, affected, error) = match r {
            // `stop_fetching` drains the rest on purpose: the statement completed.
            Ran::Done(affected) => (if aborted.abort && !rs.status().partial { Phase::Cancelled } else { Phase::Done }, affected, None),
            Ran::Cancelled | Ran::NeverRan { .. } => (Phase::Cancelled, None, None),
            Ran::Failed(info) => (Phase::Failed, None, Some(info)),
        };
        // A failure reaches us before the ReadyForQuery that follows it: the status it leaves
        // (a failed transaction) is read first.
        self.tx.settled(SETTLE).await;
        // Counted before the final phase is published: whoever sees it sees the count too.
        self.tx.statement(facts.tx.is_none());
        // History first: whoever sees the final phase also finds the statement recorded.
        self.record_as(rs, elapsed, Some(phase), affected.or(rs.status().affected), error.clone());
        rs.update_status(|s| {
            s.elapsed = Some(elapsed);
            s.phase = Some(phase);
            if affected.is_some() {
                s.affected = affected;
            }
            if error.is_some() {
                s.error = error;
            }
        });
    }

    /// Writes the statement to the history, once: when it first pauses with rows left (a big
    /// SELECT whose rest is never fetched) or when it ends.
    fn record(&self, rs: &ResultSet, elapsed: Duration) {
        let st = rs.status();
        self.record_as(rs, elapsed, st.phase, st.affected, st.error);
    }

    /// `record` with the final status given explicitly, before it is published.
    fn record_as(&self, rs: &ResultSet, elapsed: Duration, phase: Option<Phase>, affected: Option<u64>, error: Option<ErrorInfo>) {
        if rs.recorded.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let entry = HistoryEntry {
            ts: chrono::Utc::now(),
            database: self.database.clone(),
            sql: rs.sql.clone(),
            duration_ms: elapsed.as_secs_f64() * 1e3,
            rows: if rs.columns().is_some_and(|c| !c.is_empty()) { Some(rs.rows() as u64) } else { affected },
            outcome: match phase {
                Some(Phase::Cancelled) => Outcome::Cancelled,
                Some(Phase::Failed) => Outcome::Error,
                _ => Outcome::Ok,
            },
            error: error.map(|e| e.to_string()),
            source: rs.agent().map(|_| crate::history::AGENT.to_string()),
        };
        if let Err(e) = self.conn.history.append(&self.conn.profile.id, &entry) {
            log::warn!("history: {e}");
        }
    }
}

/// The savepoint a rolled-back statement runs under inside an open transaction.
const ROLLBACK_SAVEPOINT: &str = "diskman_rollback";

/// Ends the transaction (`Some(false)`) or savepoint (`Some(true)`) a rolled-back statement ran
/// in. After an error the server refuses all but this, which is what it is for.
async fn end_rollback(client: &Db, dialect: Dialect, wrap: Option<bool>) {
    let sql = match wrap {
        None => return,
        Some(true) => dialect.rollback_to(ROLLBACK_SAVEPOINT),
        Some(false) => dialect.rollback().to_string(),
    };
    if let Err(e) = client.execute(&sql).await {
        log::warn!("rollback after a rolled-back statement failed: {e}");
    }
}

/// The longest wait for the ReadyForQuery after an error (the connection is dying otherwise).
const SETTLE: Duration = Duration::from_secs(5);
