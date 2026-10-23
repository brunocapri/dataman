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

pub struct Connection {
    pub profile: ConnectionProfile,
    pub info: ServerInfo,
    pub schema: Arc<SchemaCache>,
    password: Option<String>,
    rt: Handle,
    history: Arc<History>,
    meta: tokio::sync::Mutex<HashMap<String, Db>>,
    tls_active: bool,
    tunnel: Option<Arc<crate::ssh::Tunnel>>,
    pub(crate) caches: Mutex<HashMap<(&'static str, String, String), Arc<dyn std::any::Any + Send + Sync>>>,
    read_only_dbs: parking_lot::RwLock<std::collections::BTreeMap<String, bool>>,
    pub(crate) nosql: Option<crate::nosql::Client>,
    /// The rules for agents as the last agent call read them: SQLite's agent connections check
    /// what triggers and views read against them.
    pub(crate) agent_rules: Arc<parking_lot::RwLock<crate::privacy::AgentRules>>,
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
        if !profile.engine.is_sql() {
            let (client, info) = crate::nosql::connect(&profile, password.as_deref(), tunnel.as_deref()).await?;
            let schema = Arc::new(SchemaCache::with_dialect(info.database.clone(), Vec::new(), profile.engine.dialect()));
            return Ok(Arc::new(Connection {
                read_only_dbs: parking_lot::RwLock::new(profile.read_only_databases.clone()),
                agent_rules: Arc::new(parking_lot::RwLock::new(profile.mcp_rules.clone())),
                tls_active: client.tls_active(),
                nosql: Some(client),
                profile,
                info,
                schema,
                password,
                rt,
                history,
                meta: tokio::sync::Mutex::new(HashMap::new()),
                tunnel,
                caches: Default::default(),
            }));
        }
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
            agent_rules: Arc::new(parking_lot::RwLock::new(profile.mcp_rules.clone())),
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
            nosql: None,
        }))
    }

    pub fn engine(&self) -> Engine {
        self.profile.engine
    }

    /// The rules agents run under now (an agent call reads them fresh from the saved profile).
    pub fn set_agent_rules(&self, r: &crate::privacy::AgentRules) {
        if *self.agent_rules.read() != *r {
            *self.agent_rules.write() = r.clone();
        }
    }

    pub fn dialect(&self) -> Dialect {
        self.profile.engine.dialect()
    }

    pub fn caps(&self) -> Caps {
        Caps::of(self.profile.engine)
    }

    pub fn read_only_for(&self, database: &str) -> bool {
        crate::config::read_only_in(self.profile.read_only, &self.read_only_dbs.read(), self.profile.engine, database)
    }

    pub fn set_read_only_for(&self, database: &str, on: bool) {
        crate::config::set_read_only_in(self.profile.read_only, &mut self.read_only_dbs.write(), self.profile.engine, database, on);
    }

    pub(crate) fn profile_for(&self, database: &str) -> ConnectionProfile {
        let mut p = self.profile.clone();
        p.read_only_databases = self.read_only_dbs.read().clone();
        // DuckDB opens a file in one mode for every connection: the profile's own (a database
        // marked read-only is held to it by the client-side refusal).
        if !matches!(p.engine, Engine::SqlServer | Engine::DuckDb) {
            p.read_only = p.read_only_for(database);
        }
        p
    }

    pub fn tls_in_use(&self) -> bool {
        self.tls_active
    }

    pub fn tunneled(&self) -> bool {
        self.tunnel.is_some()
    }

    pub fn close(&self) {
        if let Some(t) = &self.tunnel {
            t.close();
        }
    }

    pub(crate) async fn dedicated(&self, database: Option<&str>) -> Result<Dedicated> {
        let db = database.unwrap_or(self.database());
        backend::dedicated(&self.profile_for(db), self.password.as_deref(), Some(db), self.tunnel.as_deref()).await
    }

    pub fn handle(&self) -> &Handle {
        &self.rt
    }

    pub fn database(&self) -> &str {
        self.schema.default_database()
    }

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

    pub fn record_history(&self, entry: &HistoryEntry) {
        let mut e = std::borrow::Cow::Borrowed(entry);
        if let std::borrow::Cow::Owned(sql) = crate::mask::statement(self.profile.engine, &entry.sql) {
            e.to_mut().sql = sql;
        }
        // An agent's rows on disk keep the cleaned error only; the raw one stays in its tab.
        if entry.source.as_deref() == Some(crate::history::AGENT)
            && let Some(err) = &entry.error
            && let std::borrow::Cow::Owned(clean) = crate::privacy::clean_error(err)
        {
            e.to_mut().error = Some(clean);
        }
        let entry = e.as_ref();
        if let Err(e) = self.history.append(&self.profile.id, entry) {
            log::warn!("history: {e}");
        }
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxEnd {
    Committed,
    RolledBack,
    NoTransaction,
}

pub struct Session {
    conn: Arc<Connection>,
    pub database: String,
    client: tokio::sync::Mutex<Option<Db>>,
    cancel: Mutex<Option<Canceller>>,
    current: Mutex<Option<Arc<ResultSet>>>,
    queue: Arc<tokio::sync::Mutex<()>>,
    tx: Arc<TxWatch>,
    ro_applied: Mutex<Option<bool>>,
}

impl Drop for Session {
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

    pub fn tx(&self) -> TxState {
        if let Ok(g) = self.client.try_lock()
            && g.as_ref().is_some_and(|c| c.is_closed())
        {
            return TxState::default();
        }
        self.tx.get()
    }

    pub fn backend_pid(&self) -> Option<i32> {
        self.tx.pid()
    }

    pub fn execute(self: &Arc<Self>, sql: impl Into<String>) -> Arc<ResultSet> {
        self.execute_with(sql, Vec::new())
    }

    pub fn execute_with(self: &Arc<Self>, sql: impl Into<String>, params: Vec<Option<String>>) -> Arc<ResultSet> {
        self.execute_opts(sql, params, false)
    }

    pub fn execute_opts(self: &Arc<Self>, sql: impl Into<String>, params: Vec<Option<String>>, rollback: bool) -> Arc<ResultSet> {
        let rs = ResultSet::new(sql);
        if !params.is_empty() {
            rs.set_params(params);
        }
        rs.rollback.store(rollback, AtomicOrdering::Release);
        self.start(rs)
    }

    /// SQL Server has no `BEGIN READ ONLY` and LAR does not enforce it: there the caller's check
    /// is what keeps it a read.
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
            // A file database is opened read-only or not: a change of mind reopens it (between
            // transactions only).
            let stale = self.conn.engine().is_file()
                && *self.ro_applied.lock() != Some(self.conn.read_only_for(&self.database))
                && !self.tx.get().status.is_open();
            if !stale {
                return Ok(c.clone());
            }
        }
        self.tx.closed();
        let p = self.conn.profile_for(&self.database);
        let (client, cancel) = backend::session(&p, self.conn.password.as_deref(), Some(&self.database), self.conn.tunnel.as_deref(), &self.tx).await?;
        *self.cancel.lock() = Some(cancel);
        *self.ro_applied.lock() = Some(self.conn.read_only_for(&self.database));
        *g = Some(client.clone());
        Ok(client)
    }

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
        self.conn.record_history(&entry);
    }

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
        self.tx.take_notices();
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
        for m in self.tx.take_notices() {
            rs.push_message(m);
        }
        let elapsed = started.elapsed().saturating_sub(rs.status().paused_time());
        // Undone before anyone sees the end: the transaction state is back to what it was.
        end_rollback(&client, dialect, wrap).await;
        let aborted = *rs.demand().borrow();
        let (phase, affected, error) = match r {
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

    fn record(&self, rs: &ResultSet, elapsed: Duration) {
        let st = rs.status();
        self.record_as(rs, elapsed, st.phase, st.affected, st.error);
    }

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
        self.conn.record_history(&entry);
    }
}

const ROLLBACK_SAVEPOINT: &str = "dataman_rollback";

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

const SETTLE: Duration = Duration::from_secs(5);
