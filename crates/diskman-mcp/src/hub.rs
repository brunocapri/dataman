//! What every tool call shares: the core (its runtime, the Keychain, pgpass, SSH tunnels), the
//! connections and settings read fresh from the app's files when they change, the open
//! connections, the concurrency limits, the audit log and the optional approver (the app asking
//! the user before a write).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use diskman_core::bounded;
use diskman_core::{ConfigStore, Connection, ConnectionProfile, Core, Dirs, Environment, SecretStore, Settings, SettingsStore};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::audit::Audit;

/// Statements one connection runs at once for agents.
pub const PER_CONNECTION: usize = 2;
/// Statements all connections run at once for agents.
pub const GLOBAL: usize = 8;
/// The longest wait for a connection to open (an SSH tunnel, a Keychain prompt).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// A write waiting for a human's Allow or Deny in the app.
#[derive(Clone, Debug, Default)]
pub struct Approval {
    pub client: String,
    pub tool: String,
    /// The connection's name and id (its colour and environment come with the profile).
    pub connection: String,
    pub connection_id: String,
    pub database: String,
    pub env: Environment,
    pub sql: String,
    /// Why it asks, tersely: `DELETE without WHERE`, `1204 rows (limit 100)`, `production`.
    pub reasons: Vec<String>,
    /// Destructive statements (not only a row count or production).
    pub destructive: bool,
    /// Rows it changed in the transaction held for the answer (`None`: asked before it runs).
    pub affected: Option<u64>,
    /// The first RETURNING / OUTPUT rows of that transaction: columns and rows as text.
    pub preview: Option<(Vec<String>, Vec<Vec<String>>)>,
    /// The agent tab showing it, if any ([`Front::show_write`]).
    pub tab: Option<u64>,
}

/// A read for the agent tab: the app runs it in that tab's own session (a read-only
/// transaction, rolled back) and answers with the agent's bounded view of the result; the whole
/// result stays in the tab.
#[derive(Clone, Debug)]
pub struct TabRead {
    pub client: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub params: Vec<Option<String>>,
    pub title: Option<String>,
    pub limits: bounded::Limits,
    /// Cancelled by the agent: the tab's statement is cancelled too.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// A write for the agent tab: its SQL now, then what came of it ([`Front::write_done`]).
#[derive(Clone, Debug)]
pub struct TabWrite {
    pub client: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub title: Option<String>,
}

/// A draft (`open_query` with `run: false`): the SQL goes into the client's agent tab for the
/// connection, bound to it and the database, and nothing runs. The user runs it, as the user.
#[derive(Clone, Debug)]
pub struct TabDraft {
    pub client: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub title: Option<String>,
}

/// Where a draft went: the tab's title and the database it is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drafted {
    pub tab: String,
    pub database: String,
}

/// Why a draft could not be placed when no app (no agent tabs) is there.
pub const DRAFTS_NEED_APP: &str = "drafts need Diskman open; open the app and try again";

/// What came of an agent's write, for its tab and its history.
#[derive(Clone, Debug)]
pub enum WriteShown {
    /// Ran: committed or rolled back (a denial, a timeout, a limit nobody could raise).
    Ran { outcome: bounded::Outcome, note: Option<String> },
    /// Refused or failed before or while it ran.
    Failed(String),
}

/// The app, for the tools: a human to ask, and agent tabs to work in. Present only in the
/// process of a running app (the stdio calls forwarded to it).
pub trait Front: Send + Sync {
    /// Asks the human; resolves `true` for Allow. Dropping the future withdraws the question.
    fn approve(&self, a: Approval) -> BoxFuture<'static, bool>;
    /// Whether this front has agent tabs (a test's may not).
    fn tabs(&self) -> bool {
        false
    }
    /// Runs a read in the client's agent tab for the connection ([`TabRead`]).
    fn run_read(&self, _r: TabRead) -> BoxFuture<'static, Result<bounded::Outcome, bounded::Stop>> {
        Box::pin(async { Err(bounded::Stop::Refused("no agent tabs here".into())) })
    }
    /// Shows a write in the client's agent tab before it runs; the tab, if one shows it.
    fn show_write(&self, _w: TabWrite) -> BoxFuture<'static, Option<u64>> {
        Box::pin(async { None })
    }
    /// What came of a write [`Front::show_write`] showed in `tab`.
    fn write_done(&self, _tab: u64, _sql: String, _shown: WriteShown) {}
    /// Places a draft in the client's agent tab ([`TabDraft`]); runs nothing.
    fn draft(&self, _d: TabDraft) -> BoxFuture<'static, Result<Drafted, String>> {
        Box::pin(async { Err(DRAFTS_NEED_APP.to_string()) })
    }
    /// The app's tabs with a transaction open on the connection (their titles): an edit of the
    /// connection would end them, so it waits until they are committed or rolled back.
    fn open_transactions(&self, _connection_id: &str) -> BoxFuture<'static, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    /// The saved connections changed (`create_connection`, `update_connection`): the app shows
    /// them at once (it also follows the file on its own).
    fn connections_changed(&self) {}
}

/// How long a write waits for a human before it is denied (rolled back).
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// A file's contents, read again when its modification time changes.
struct Fresh<T> {
    stamp: Option<SystemTime>,
    value: Arc<T>,
}

fn mtime(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

pub struct Hub {
    pub core: Core,
    pub dirs: Dirs,
    pub audit: Audit,
    profiles: Mutex<Fresh<Vec<ConnectionProfile>>>,
    settings: Mutex<Fresh<Settings>>,
    open: Mutex<HashMap<String, (ConnectionProfile, Arc<Connection>)>>,
    limits: Mutex<HashMap<String, Arc<Semaphore>>>,
    global: Arc<Semaphore>,
    front: Mutex<Option<Arc<dyn Front>>>,
    approval_timeout: Mutex<Duration>,
    /// Schema snapshots (resources): (connection id, database, json) → text.
    pub(crate) snapshots: Mutex<HashMap<(String, String, bool), Arc<String>>>,
    /// When each (connection id, database) schema cache was last emptied.
    refreshed: Mutex<HashMap<(String, String), Instant>>,
}

/// How long the catalog read for one tool stays good for the next ones.
pub const SCHEMA_TTL: Duration = Duration::from_secs(30);

impl Hub {
    pub fn new(dirs: Dirs, secrets: Arc<dyn SecretStore>) -> anyhow::Result<Arc<Hub>> {
        let core = Core::new(dirs.clone(), secrets)?;
        Ok(Arc::new(Hub {
            audit: Audit::new(dirs.mcp_audit_file()),
            core,
            dirs,
            profiles: Mutex::new(Fresh { stamp: None, value: Arc::new(Vec::new()) }),
            settings: Mutex::new(Fresh { stamp: None, value: Arc::new(Settings::default()) }),
            open: Mutex::new(HashMap::new()),
            limits: Mutex::new(HashMap::new()),
            global: Arc::new(Semaphore::new(GLOBAL)),
            front: Mutex::new(None),
            approval_timeout: Mutex::new(APPROVAL_TIMEOUT),
            snapshots: Mutex::new(HashMap::new()),
            refreshed: Mutex::new(HashMap::new()),
        }))
    }

    /// Empties the cached schema of `db` when it is older than [`SCHEMA_TTL`] (or `now`), so
    /// tools see tables created since; the snapshots of `db` go too.
    pub fn fresh_schema(&self, id: &str, db: &str, conn: &Connection, now: bool) {
        let key = (id.to_string(), db.to_string());
        let mut g = self.refreshed.lock();
        let stale = now || g.get(&key).is_none_or(|t| t.elapsed() >= SCHEMA_TTL);
        if stale {
            conn.schema.invalidate(Some(db));
            g.insert(key, Instant::now());
            self.snapshots.lock().retain(|(i, d, _), _| !(i == id && d == db));
        }
    }

    pub fn handle(&self) -> tokio::runtime::Handle {
        self.core.handle()
    }

    pub fn tools_file(&self) -> PathBuf {
        self.dirs.mcp_tools_file()
    }

    /// The app's side of the tools (set by the app on its own hub; never in a standalone
    /// `diskman-mcp`).
    pub fn set_front(&self, f: Option<Arc<dyn Front>>) {
        *self.front.lock() = f;
    }

    pub fn front(&self) -> Option<Arc<dyn Front>> {
        self.front.lock().clone()
    }

    pub fn approval_timeout(&self) -> Duration {
        *self.approval_timeout.lock()
    }

    /// How long writes wait for an answer (tests shorten it).
    pub fn set_approval_timeout(&self, d: Duration) {
        *self.approval_timeout.lock() = d;
    }

    /// The rows an agent's write on `p` may change before it asks: the connection's own limit,
    /// else the setting.
    pub fn row_limit(&self, p: &ConnectionProfile) -> u64 {
        p.mcp_row_limit.unwrap_or_else(|| self.settings().mcp_write_row_limit)
    }

    /// The saved connections, as the app last wrote them.
    pub fn profiles(&self) -> Arc<Vec<ConnectionProfile>> {
        let file = self.dirs.connections_file();
        let stamp = mtime(&file);
        let mut g = self.profiles.lock();
        if g.stamp != stamp || stamp.is_none() {
            match ConfigStore::open(&self.dirs) {
                Ok(s) => g.value = Arc::new(s.connections()),
                Err(e) => log::warn!("{e:#}"),
            }
            g.stamp = stamp;
        }
        g.value.clone()
    }

    /// The app's settings, as last saved.
    pub fn settings(&self) -> Arc<Settings> {
        let file = self.dirs.settings_file();
        let stamp = mtime(&file);
        let mut g = self.settings.lock();
        if g.stamp != stamp || stamp.is_none() {
            g.value = Arc::new(SettingsStore::open(&self.dirs).get());
            g.stamp = stamp;
        }
        g.value.clone()
    }

    /// A connection by name (case-insensitive) or id.
    pub fn profile(&self, name: &str) -> Result<ConnectionProfile, String> {
        let all = self.profiles();
        if let Some(p) = all.iter().find(|p| p.id == name) {
            return Ok(p.clone());
        }
        let hits: Vec<&ConnectionProfile> = all.iter().filter(|p| p.name.eq_ignore_ascii_case(name.trim())).collect();
        match hits.as_slice() {
            [p] => Ok((*p).clone()),
            [] => {
                let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
                Err(format!("no connection named {name:?}; known: {}", names.join(", ")))
            }
            _ => Err(format!("several connections are named {name:?}; use the id from list_connections")),
        }
    }

    /// The open connection for a profile, connecting on first use (and again when the profile
    /// changed in the app).
    pub async fn connect(&self, p: &ConnectionProfile) -> Result<Arc<Connection>, String> {
        let stale = {
            let mut g = self.open.lock();
            // Only a database's Read Only changed (the explorer's toggle): the live connection
            // follows, without reconnecting.
            if let Some((saved, c)) = g.get_mut(&p.id)
                && saved != p
                && (ConnectionProfile { read_only_databases: p.read_only_databases.clone(), ..saved.clone() }) == *p
            {
                for db in saved.read_only_databases.keys().chain(p.read_only_databases.keys()) {
                    c.set_read_only_for(db, p.read_only_for(db));
                }
                *saved = p.clone();
            }
            match g.get(&p.id) {
                Some((saved, c)) if saved == p => return Ok(c.clone()),
                Some(_) => g.remove(&p.id).map(|(_, c)| c),
                None => None,
            }
        };
        if let Some(c) = stale {
            c.close();
        }
        let task = self.core.connect(p.clone(), None);
        let c = match tokio::time::timeout(CONNECT_TIMEOUT, task).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return Err(format!("cannot connect to {}: {e:#}", p.name)),
            Err(_) => return Err(format!("cannot connect to {}: timed out", p.name)),
        };
        self.open.lock().insert(p.id.clone(), (p.clone(), c.clone()));
        Ok(c)
    }

    /// Already connected (no I/O).
    pub fn connected(&self, id: &str) -> Option<Arc<Connection>> {
        self.open.lock().get(id).map(|(_, c)| c.clone())
    }

    /// One of the connection's agent slots, and one of all connections'.
    pub async fn permit(&self, id: &str) -> (tokio::sync::OwnedSemaphorePermit, tokio::sync::OwnedSemaphorePermit) {
        let sem = self.limits.lock().entry(id.to_string()).or_insert_with(|| Arc::new(Semaphore::new(PER_CONNECTION))).clone();
        let a = sem.acquire_owned().await.expect("semaphore closed");
        let b = self.global.clone().acquire_owned().await.expect("semaphore closed");
        (a, b)
    }

    /// Closes every open connection (their SSH tunnels too).
    pub fn close_all(&self) {
        for (_, (_, c)) in self.open.lock().drain() {
            c.close();
        }
    }
}
