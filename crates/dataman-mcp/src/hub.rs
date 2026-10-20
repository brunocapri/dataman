use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use dataman_core::bounded;
use dataman_core::{ConfigStore, Connection, ConnectionProfile, Core, Dirs, Environment, SecretStore, Settings, SettingsStore};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::audit::Audit;

pub const PER_CONNECTION: usize = 2;
pub const GLOBAL: usize = 8;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default)]
pub struct Approval {
    pub client: String,
    pub tool: String,
    pub connection: String,
    pub connection_id: String,
    pub database: String,
    pub env: Environment,
    pub sql: String,
    pub reasons: Vec<String>,
    pub destructive: bool,
    pub affected: Option<u64>,
    pub preview: Option<(Vec<String>, Vec<Vec<String>>)>,
    pub tab: Option<u64>,
    pub allowable: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TabRead {
    pub client: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub params: Vec<Option<String>>,
    pub title: Option<String>,
    pub limits: bounded::Limits,
    pub cancel: tokio_util::sync::CancellationToken,
}

#[derive(Clone, Debug)]
pub struct TabWrite {
    pub client: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TabDraft {
    pub client: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drafted {
    pub tab: String,
    pub database: String,
}

pub const DRAFTS_NEED_APP: &str = "drafts need Dataman open; open the app and try again";

#[derive(Clone, Debug)]
pub enum WriteShown {
    Ran { outcome: bounded::Outcome, note: Option<String> },
    Failed(String),
}

pub trait Front: Send + Sync {
    fn approve(&self, a: Approval) -> BoxFuture<'static, bool>;
    fn tabs(&self) -> bool {
        false
    }
    fn run_read(&self, _r: TabRead) -> BoxFuture<'static, Result<bounded::Outcome, bounded::Stop>> {
        Box::pin(async { Err(bounded::Stop::Refused("no agent tabs here".into())) })
    }
    fn show_write(&self, _w: TabWrite) -> BoxFuture<'static, Option<u64>> {
        Box::pin(async { None })
    }
    fn write_done(&self, _tab: u64, _sql: String, _shown: WriteShown) {}
    fn show_doc(&self, _d: TabWrite) -> BoxFuture<'static, Option<u64>> {
        Box::pin(async { None })
    }
    fn doc_done(&self, _tab: u64, _ran: Result<dataman_core::nosql::Ran, String>) {}
    fn draft(&self, _d: TabDraft) -> BoxFuture<'static, Result<Drafted, String>> {
        Box::pin(async { Err(DRAFTS_NEED_APP.to_string()) })
    }
    fn open_transactions(&self, _connection_id: &str) -> BoxFuture<'static, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    fn connections_changed(&self) {}
}

pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

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
    pub(crate) snapshots: Mutex<HashMap<(String, String, bool), Arc<String>>>,
    refreshed: Mutex<HashMap<(String, String), Instant>>,
}

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

    pub fn set_front(&self, f: Option<Arc<dyn Front>>) {
        *self.front.lock() = f;
    }

    pub fn front(&self) -> Option<Arc<dyn Front>> {
        self.front.lock().clone()
    }

    pub fn approval_timeout(&self) -> Duration {
        *self.approval_timeout.lock()
    }

    pub fn set_approval_timeout(&self, d: Duration) {
        *self.approval_timeout.lock() = d;
    }

    pub fn row_limit(&self, p: &ConnectionProfile) -> u64 {
        p.mcp_row_limit.unwrap_or_else(|| self.settings().mcp_write_row_limit)
    }

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

    pub async fn connect(&self, p: &ConnectionProfile) -> Result<Arc<Connection>, String> {
        let stale = {
            let mut g = self.open.lock();
            if let Some((saved, c)) = g.get_mut(&p.id)
                && saved != p
                && (ConnectionProfile { read_only_databases: p.read_only_databases.clone(), ..saved.clone() }).same_but_agents(p)
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

    pub fn connected(&self, id: &str) -> Option<Arc<Connection>> {
        self.open.lock().get(id).map(|(_, c)| c.clone())
    }

    pub async fn permit(&self, id: &str) -> (tokio::sync::OwnedSemaphorePermit, tokio::sync::OwnedSemaphorePermit) {
        let sem = self.limits.lock().entry(id.to_string()).or_insert_with(|| Arc::new(Semaphore::new(PER_CONNECTION))).clone();
        let a = sem.acquire_owned().await.expect("semaphore closed");
        let b = self.global.clone().acquire_owned().await.expect("semaphore closed");
        (a, b)
    }

    pub fn close_all(&self) {
        for (_, (_, c)) in self.open.lock().drain() {
            c.close();
        }
    }
}
