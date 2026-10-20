use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dataman_core::bounded;
use dataman_core::stamp::FileStamp;
use dataman_core::{ConfigStore, Connection, ConnectionProfile, Core, Dirs, Environment, SecretStore, Settings, SettingsStore};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::audit::Audit;
use dataman_core::privacy::{HashKey, Types};

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
    /// `affected` and `preview` come from a run rolled back before asking: a count from a
    /// moment ago, not a promise (the write is run again after Allow, never with more rows).
    pub counted: bool,
}

/// Characters that draw as nothing, or reorder what is drawn around them.
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{80}'..='\u{9F}' | '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
        | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFFB}' | '\u{E0000}'..='\u{E0FFF}')
        || c.is_control() && c != '\n' && c != '\t'
}

const BLANK_RUN: usize = 2;
const SPACE_RUN: usize = 16;

/// What the Allow sheet shows: every character the statement holds is visible, and padding
/// (runs of blank lines or spaces) cannot push what runs out of sight.
pub fn shown_text(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace(['\r', '\u{85}', '\u{2028}', '\u{2029}', '\u{B}', '\u{C}'], "\n").replace('\t', "    ");
    let mut out = String::with_capacity(text.len());
    let mut blank = 0usize;
    let flush_blank = |out: &mut String, blank: &mut usize| {
        if *blank > BLANK_RUN {
            out.push_str(&format!("⟨{} blank lines⟩\n", *blank));
        } else {
            out.push_str(&"\n".repeat(*blank));
        }
        *blank = 0;
    };
    for line in text.trim().split('\n') {
        if line.trim().is_empty() {
            blank += 1;
            continue;
        }
        flush_blank(&mut out, &mut blank);
        let mut spaces = 0usize;
        for c in line.trim_end().chars() {
            if c == ' ' {
                spaces += 1;
                continue;
            }
            if spaces > SPACE_RUN {
                out.push_str(&format!(" ⟨{spaces} spaces⟩ "));
            } else {
                out.push_str(&" ".repeat(spaces));
            }
            spaces = 0;
            if invisible(c) {
                out.push_str(&format!("⟨U+{:04X}⟩", c as u32));
            } else {
                out.push(c);
            }
        }
        out.push('\n');
    }
    out.truncate(out.trim_end().len());
    out
}

/// A client's name as it introduced itself: one line, visible characters, bounded.
pub fn shown_client(name: &str) -> String {
    let one: String = shown_text(name).split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(48) {
        Some((at, _)) => format!("{}…", &one[..at]),
        None => one,
    }
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

/// Questions waiting for the user at once: per client, and from all clients (a client names
/// itself, so the total is what bounds a client posing as several).
pub const WAITING_PER_CLIENT: usize = 3;
pub const WAITING: usize = 8;
/// After a Deny, the same client's next question waits this long: no sheet right after "no".
pub const AFTER_DENY: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Asks {
    waiting: HashMap<String, usize>,
    denied: HashMap<String, Instant>,
}

/// A question on the user's screen; its place is given back when dropped.
pub struct AskSlot {
    asks: Arc<Mutex<Asks>>,
    client: String,
}

impl Drop for AskSlot {
    fn drop(&mut self) {
        let mut g = self.asks.lock();
        if let Some(n) = g.waiting.get_mut(&self.client) {
            *n -= 1;
            if *n == 0 {
                g.waiting.remove(&self.client);
            }
        }
    }
}

struct Fresh<T> {
    stamp: Option<FileStamp>,
    value: Arc<T>,
}

fn mtime(p: &std::path::Path) -> Option<FileStamp> {
    FileStamp::of(p)
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
    asks: Arc<Mutex<Asks>>,
    after_deny: Mutex<Duration>,
    types: Mutex<HashMap<String, Arc<Types>>>,
    mask_key: Mutex<HashKey>,
}

pub const SCHEMA_TTL: Duration = Duration::from_secs(30);

impl Hub {
    pub fn new(dirs: Dirs, secrets: Arc<dyn SecretStore>) -> anyhow::Result<Arc<Hub>> {
        let core = Core::new(dirs.clone(), secrets.clone())?;
        Ok(Arc::new(Hub {
            types: Mutex::new(HashMap::new()),
            mask_key: Mutex::new(HashKey::stored(secrets)),
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
            asks: Arc::new(Mutex::new(Asks::default())),
            after_deny: Mutex::new(AFTER_DENY),
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

    pub fn ask_slot(&self, client: &str) -> Result<AskSlot, String> {
        let mut g = self.asks.lock();
        let pause = *self.after_deny.lock();
        if let Some(t) = g.denied.get(client)
            && t.elapsed() < pause
        {
            return Err(format!("the user just denied this client's request in Dataman; wait {} s before asking again", pause.as_secs().max(1)));
        }
        let mine = g.waiting.get(client).copied().unwrap_or(0);
        let all: usize = g.waiting.values().sum();
        if mine >= WAITING_PER_CLIENT || all >= WAITING {
            return Err(format!("{} questions already wait for the user in Dataman; wait for an answer before asking more", if mine >= WAITING_PER_CLIENT { mine } else { all }));
        }
        *g.waiting.entry(client.to_string()).or_default() += 1;
        Ok(AskSlot { asks: self.asks.clone(), client: client.to_string() })
    }

    pub fn set_after_deny(&self, d: Duration) {
        *self.after_deny.lock() = d;
    }

    pub fn denied(&self, client: &str) {
        self.asks.lock().denied.insert(client.to_string(), Instant::now());
    }

    pub fn approval_timeout(&self) -> Duration {
        *self.approval_timeout.lock()
    }

    pub fn set_approval_timeout(&self, d: Duration) {
        *self.approval_timeout.lock() = d;
    }

    /// The sensitive data types switched on for a connection (`None`: All connections),
    /// compiled once per settings change.
    pub fn types(&self, p: Option<&ConnectionProfile>) -> Arc<Types> {
        let settings = self.settings();
        let empty = Default::default();
        let overrides = p.map_or(&empty, |p| &p.mcp_rules.types);
        let key = serde_json::to_string(&(&settings.agent_types, overrides)).unwrap_or_default();
        let mut g = self.types.lock();
        if let Some(t) = g.get(&key) {
            return t.clone();
        }
        if g.len() > 64 {
            g.clear();
        }
        let t = Arc::new(Types::new(&settings.agent_types, overrides));
        g.insert(key, t.clone());
        t
    }

    pub fn mask_key(&self) -> HashKey {
        self.mask_key.lock().clone()
    }

    pub fn set_mask_key(&self, k: HashKey) {
        *self.mask_key.lock() = k;
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
        // Agents' reads on MongoDB go where Permissions › Read from says.
        let mut agent = p.clone();
        if let Some(r) = p.mcp_rules.read_from.filter(|_| p.engine == dataman_core::Engine::MongoDb) {
            agent.options.insert("readPreference".into(), r.mongo().into());
        }
        let task = self.core.connect(agent, None);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(Dirs::at(dir.path().join("d")), Arc::new(dataman_core::MemorySecrets::default())).unwrap();
        let mine: Vec<AskSlot> = (0..WAITING_PER_CLIENT).map(|_| hub.ask_slot("a").unwrap()).collect();
        assert!(hub.ask_slot("a").err().unwrap().contains("already wait"));
        let others: Vec<AskSlot> = (0..WAITING - WAITING_PER_CLIENT).map(|i| hub.ask_slot(&format!("b{i}")).unwrap()).collect();
        assert!(hub.ask_slot("fresh-name").err().unwrap().contains("already wait"), "renaming does not help past the total");
        drop(others);
        drop(mine);
        assert!(hub.ask_slot("a").is_ok(), "answered questions give their place back");
        hub.denied("a");
        assert!(hub.ask_slot("a").err().unwrap().contains("just denied"));
        assert!(hub.ask_slot("b").is_ok(), "only the denied client waits");
        std::mem::forget(hub);
    }

    #[test]
    fn approval_text_shows_everything() {
        assert_eq!(shown_text("  DELETE FROM t\r\n WHERE id = 1;  "), "DELETE FROM t\n WHERE id = 1;");
        let padded = format!("INSERT INTO log VALUES (1){}; DROP TABLE users", " ".repeat(400));
        assert_eq!(shown_text(&padded), "INSERT INTO log VALUES (1) ⟨400 spaces⟩ ; DROP TABLE users");
        let tall = format!("db.log.insertOne({{a: 1}})\n{}db.users.deleteMany({{x: 1}})", "\n".repeat(60));
        assert_eq!(shown_text(&tall), "db.log.insertOne({a: 1})\n⟨60 blank lines⟩\ndb.users.deleteMany({x: 1})");
        assert_eq!(shown_text("a\n\nb\n\n\nc"), "a\n\nb\n\n\nc", "a little air stays");
        assert_eq!(shown_text("SELECT 'a\u{202E}gnp.exe'"), "SELECT 'a⟨U+202E⟩gnp.exe'");
        assert_eq!(shown_text("DE\u{200B}LETE x\u{FEFF}"), "DE⟨U+200B⟩LETE x⟨U+FEFF⟩");
        assert_eq!(shown_text("a\rDROP TABLE t"), "a\nDROP TABLE t", "a lone CR starts a line");
        assert_eq!(shown_text("a\u{2028}b\u{0}c"), "a\nb⟨U+0000⟩c");
        assert_eq!(shown_text("\tSELECT 1"), "SELECT 1");
        assert_eq!(shown_text("SELECT 'ünïcødé', '日本'"), "SELECT 'ünïcødé', '日本'");
        assert_eq!(shown_client("claude-code"), "claude-code");
        assert_eq!(shown_client("Dataman\n\nYou approved this\u{202E}"), "Dataman You approved this⟨U+202E⟩");
        assert!(shown_client(&"x".repeat(200)).chars().count() == 49);
    }
}
