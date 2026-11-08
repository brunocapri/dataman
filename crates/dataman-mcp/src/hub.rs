use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dataman_core::bounded;
use dataman_core::stamp::FileStamp;
use dataman_core::{ConfigStore, Connection, ConnectionProfile, Core, Dirs, Environment, SecretStore, Settings, SettingsStore};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::Semaphore;

use dataman_core::audit::Audit;
use dataman_core::agent_login;
use dataman_core::privacy::Types;

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

pub(crate) fn invisible(c: char) -> bool {
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
    /// The rules as they apply (generated columns protected as their sources): what SQLite's
    /// authorizer on the app's agent connection keeps. Only where an authorizer reads them.
    pub rules: Option<dataman_core::privacy::AgentRules>,
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

pub const SHOW_NEEDS_APP: &str = "Dataman has to be open for this: ask the user to open it, then try again";

/// Where `show_in_dataman` takes the user; a saved connection by the id the call resolved. Only
/// the user's click saves anything from there.
#[derive(Clone, Debug)]
pub enum Place {
    /// The new connection sheet, filled with what the agent knows (never a password).
    NewConnection(Box<ConnectionProfile>),
    Connection(String),
    Protection(String),
    AgentLogin(String),
    AgentsGeneral,
    SettingsMcp,
    OutsideReview,
}

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
    fn show(&self, _place: Place) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Err(SHOW_NEEDS_APP.to_string()) })
    }
}

pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Questions waiting for the user at once: per client, and from all clients (a client names
/// itself, so the total is what bounds a client posing as several).
pub const WAITING_PER_CLIENT: usize = 3;
pub const WAITING: usize = 8;
/// After a Deny, the same client's next question waits this long: no sheet right after "no".
pub const AFTER_DENY: Duration = Duration::from_secs(5);

/// `show_in_dataman` brings the window forward: this often at most, from all clients together
/// (a client names itself), so that an agent in a loop cannot keep taking the screen.
pub const SHOWS: usize = 4;
pub const SHOWS_WITHIN: Duration = Duration::from_secs(60);

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
    /// None: never read.
    stamp: Option<Option<FileStamp>>,
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
    /// The user's own login, where an agent login is in use: for what the user allowed only.
    mine: Mutex<HashMap<String, (ConnectionProfile, Arc<Connection>)>>,
    limits: Mutex<HashMap<String, Arc<Semaphore>>>,
    global: Arc<Semaphore>,
    front: Mutex<Option<Arc<dyn Front>>>,
    approval_timeout: Mutex<Duration>,
    pub(crate) snapshots: Mutex<HashMap<(String, String, bool), Arc<String>>>,
    refreshed: Mutex<HashMap<(String, String), Instant>>,
    asks: Arc<Mutex<Asks>>,
    after_deny: Mutex<Duration>,
    types: Mutex<HashMap<String, Arc<Types>>>,
    shows: Mutex<VecDeque<Instant>>,
    show_limit: Mutex<(usize, Duration)>,
}

pub const SCHEMA_TTL: Duration = Duration::from_secs(30);

impl Hub {
    pub fn new(dirs: Dirs, secrets: Arc<dyn SecretStore>) -> anyhow::Result<Arc<Hub>> {
        Ok(Hub::of(Core::new(dirs.clone(), secrets)?, dirs))
    }

    /// With the approved rules the app keeps: what agents save through it never waits for review.
    pub fn with_approved(dirs: Dirs, secrets: Arc<dyn SecretStore>, approved: Arc<dataman_core::approved::Approved>) -> anyhow::Result<Arc<Hub>> {
        Ok(Hub::of(Core::with_approved(dirs.clone(), secrets, approved)?, dirs))
    }

    fn of(core: Core, dirs: Dirs) -> Arc<Hub> {
        Arc::new(Hub {
            types: Mutex::new(HashMap::new()),
            audit: Audit::new(dirs.mcp_audit_file()),
            core,
            dirs,
            profiles: Mutex::new(Fresh { stamp: None, value: Arc::new(Vec::new()) }),
            settings: Mutex::new(Fresh { stamp: None, value: Arc::new(Settings::default()) }),
            open: Mutex::new(HashMap::new()),
            mine: Mutex::new(HashMap::new()),
            limits: Mutex::new(HashMap::new()),
            global: Arc::new(Semaphore::new(GLOBAL)),
            front: Mutex::new(None),
            approval_timeout: Mutex::new(APPROVAL_TIMEOUT),
            snapshots: Mutex::new(HashMap::new()),
            refreshed: Mutex::new(HashMap::new()),
            asks: Arc::new(Mutex::new(Asks::default())),
            after_deny: Mutex::new(AFTER_DENY),
            shows: Mutex::new(VecDeque::new()),
            show_limit: Mutex::new((SHOWS, SHOWS_WITHIN)),
        })
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

    /// A place for the window to come forward now, or why not: no sooner than a question could be
    /// asked (right after a Deny, or with questions already waiting), and within the limit.
    pub fn show_slot(&self, client: &str) -> Result<(), String> {
        drop(self.ask_slot(client)?);
        let (most, within) = *self.show_limit.lock();
        let mut g = self.shows.lock();
        while g.front().is_some_and(|t| t.elapsed() >= within) {
            g.pop_front();
        }
        if let Some(first) = g.front().filter(|_| g.len() >= most) {
            let wait = within.saturating_sub(first.elapsed()).as_secs() + 1;
            return Err(format!("Dataman came forward {most} times in the last {} s; tell the user where to go, or try again in {wait} s", within.as_secs()));
        }
        g.push_back(Instant::now());
        Ok(())
    }

    pub fn show_not_used(&self) {
        self.shows.lock().pop_back();
    }

    pub fn set_show_limit(&self, most: usize, within: Duration) {
        *self.show_limit.lock() = (most, within);
    }

    pub fn approval_timeout(&self) -> Duration {
        *self.approval_timeout.lock()
    }

    pub fn set_approval_timeout(&self, d: Duration) {
        *self.approval_timeout.lock() = d;
    }

    /// The sensitive data types switched on (one list for every connection), compiled once per
    /// settings change.
    pub fn types(&self) -> Arc<Types> {
        let settings = self.settings();
        let key = serde_json::to_string(&settings.agent_types).unwrap_or_default();
        let mut g = self.types.lock();
        if let Some(t) = g.get(&key) {
            return t.clone();
        }
        if g.len() > 64 {
            g.clear();
        }
        let t = Arc::new(Types::new(&settings.agent_types));
        g.insert(key, t.clone());
        t
    }

    /// The types that mask what a call on `p` returns: all of them where its agent sees Masked
    /// data, none on Data or Structure only (nor for a call on no connection).
    pub fn types_for(&self, p: Option<&ConnectionProfile>) -> Arc<Types> {
        if p.is_some_and(|p| p.mcp_rules.view == dataman_core::privacy::AgentView::Masked) { self.types() } else { Arc::new(Types::none()) }
    }

    pub fn keep_reply(&self, id: &str, text: &str) {
        self.audit.write_reply(id, text);
    }

    pub fn row_limit(&self, p: &ConnectionProfile) -> u64 {
        dataman_core::privacy::row_cap(p.mcp_row_limit.unwrap_or(dataman_core::settings::MCP_WRITE_ROW_LIMIT))
    }

    pub fn profiles(&self) -> Arc<Vec<ConnectionProfile>> {
        let file = self.dirs.connections_file();
        let stamp = mtime(&file);
        let mut g = self.profiles.lock();
        if g.stamp != Some(stamp) {
            // The file and the approved rules in the Keychain, read again only when it changed.
            match dataman_core::off_runtime(|| ConfigStore::open_with(&self.dirs, self.core.approved.clone())) {
                Ok(s) => g.value = Arc::new(s.connections()),
                Err(e) => log::warn!("{e:#}"),
            }
            g.stamp = Some(stamp);
        }
        g.value.clone()
    }

    pub fn settings(&self) -> Arc<Settings> {
        let file = self.dirs.settings_file();
        let stamp = mtime(&file);
        let mut g = self.settings.lock();
        if g.stamp != Some(stamp) {
            g.value = Arc::new(dataman_core::off_runtime(|| SettingsStore::open_with(&self.dirs, self.core.approved.clone()).get()));
            g.stamp = Some(stamp);
        }
        g.value.clone()
    }

    /// The connection with this id (not a name: what a call resolved already).
    pub fn profile_by_id(&self, id: &str) -> Option<ConnectionProfile> {
        self.profiles().iter().find(|p| p.id == id).cloned()
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

    /// The connection agents use: as its agent login when it has one (its password the
    /// Keychain's), never falling back to the user's own login when that fails.
    pub async fn connect(&self, p: &ConnectionProfile) -> Result<Arc<Connection>, String> {
        if let Err(e) = refusal(p) {
            // What was open for it (as a login it no longer has) goes too.
            self.forget(&p.id);
            return Err(e);
        }
        let stale = {
            let mut g = self.open.lock();
            if let Some((saved, c)) = g.get_mut(&p.id)
                && saved != p
                && (&saved.agent_login, saved.agents_refused) == (&p.agent_login, p.agents_refused)
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
            // What it read of the schema came through that login (another one, or the user's).
            self.forget_schema(&p.id);
        }
        // Agents' reads on MongoDB go where Permissions › Read from says.
        let mut agent = p.clone();
        if let Some(r) = p.mcp_rules.read_from.filter(|_| p.engine == dataman_core::Engine::MongoDb) {
            agent.options.insert("readPreference".into(), r.mongo().into());
        }
        let mut agent_pw = None;
        let task = match &p.agent_login {
            Some(l) => {
                // The Keychain may wait for a prompt: not on the runtime's workers.
                let (secrets, id, user) = (self.core.secrets.clone(), p.id.clone(), l.user.clone());
                let pw = tokio::task::spawn_blocking(move || agent_login::live::password_of(&*secrets, &id, &user)).await.map_err(|e| e.to_string())?.map_err(|e| format!("{e:#}"))?;
                agent_pw = Some(pw.clone());
                self.core.connect_agent(p, agent_login::live::agent_profile(&agent, &l.user), pw)
            }
            None => self.core.connect(agent.clone(), None),
        };
        // Past its time, or this call dropped: the tunnel and the Keychain read stop too.
        let mut task = task.abort_on_drop();
        let as_login = p.agent_login.as_ref().map(|l| format!(" as its agent login {}", l.user)).unwrap_or_default();
        let never = if p.agent_login.is_some() { ". Agents are refused here until it connects: Dataman never uses your own login for them (Agents › Permissions › Verify)" } else { "" };
        let first = match tokio::time::timeout(CONNECT_TIMEOUT, &mut task).await {
            Ok(Ok(c)) => Ok(c),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => return Err(format!("cannot connect to {}{as_login}: timed out{never}", p.name)),
        };
        // SQL Server: its database made again (a restore, a new copy) took the login's user with
        // it: refused at login, or let in and unable to open it. Put back where Auto sync is on
        // (the user's login, as a sync), then connected again.
        let lost = match (&first, &p.agent_login) {
            (Err(e), Some(_)) if p.engine == dataman_core::Engine::SqlServer => agent_login::live::lost_database(e),
            (Ok(c), Some(l)) if p.engine == dataman_core::Engine::SqlServer => !agent_login::live::reaches(c, &agent_login::live::database_for(p, Some(l))).await,
            _ => false,
        };
        let c = match (lost, &p.agent_login, &agent_pw) {
            (true, Some(l), Some(pw)) => {
                if let Ok(c) = &first {
                    c.close();
                }
                self.put_back(p, l, pw).await?;
                let again = self.core.connect_agent(p, agent_login::live::agent_profile(&agent, &l.user), pw.clone()).abort_on_drop();
                match tokio::time::timeout(CONNECT_TIMEOUT, again).await {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => return Err(format!("cannot connect to {}{as_login}: {e:#}{never}", p.name)),
                    Err(_) => return Err(format!("cannot connect to {}{as_login}: timed out{never}", p.name)),
                }
            }
            _ => first.map_err(|e| format!("cannot connect to {}{as_login}: {e}{never}", p.name))?,
        };
        if let Some(l) = &p.agent_login {
            c.set_agent_views(&l.views);
        }
        self.note_flavor(p, &c.info);
        self.open.lock().insert(p.id.clone(), (p.clone(), c.clone()));
        Ok(c)
    }

    /// What the server told itself as, kept by the saved connection (shown while it is not
    /// connected). Its own write: a failure only leaves the old one shown.
    fn note_flavor(&self, p: &ConnectionProfile, info: &dataman_core::ServerInfo) {
        if p.flavor == info.flavor || p.engine != info.engine {
            return;
        }
        let noted = dataman_core::off_runtime(|| ConfigStore::open_with(&self.dirs, self.core.approved.clone())?.note_flavor(&p.id, info));
        match noted {
            Ok(true) => {
                if let Some(f) = self.front() {
                    f.connections_changed();
                }
            }
            Ok(false) => {}
            Err(e) => log::warn!("{}: what the server is ({}) not kept: {e:#}", p.name, info.flavor.label(p.engine)),
        }
    }

    /// The agent login put back in its database made again, with the user's login, where the
    /// user lets Dataman keep it in step on its own (Auto sync); the record kept as a sync keeps
    /// it. Otherwise, why agents are refused and what puts it back.
    fn put_back<'a>(&'a self, p: &'a ConnectionProfile, l: &'a dataman_core::agent_login::AgentLogin, pw: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(self.put_back_now(p, l, pw))
    }

    async fn put_back_now(&self, p: &ConnectionProfile, l: &dataman_core::agent_login::AgentLogin, pw: &str) -> Result<(), String> {
        let db = agent_login::live::database_for(p, Some(l));
        let lost = || format!("cannot connect to {} as its agent login {}: {}", p.name, l.user, agent_login::live::lost_user_text(&l.user, &db));
        if !l.auto_sync {
            return Err(lost());
        }
        let admin = self.connect_user(p).await.map_err(|e| format!("{}; and Dataman could not connect as you to put it back: {e}", lost()))?;
        let plan = agent_login::live::reattach(&self.core, &admin, p, l, pw).await.map_err(|e| format!("cannot connect to {} as its agent login: {e:#}", p.name))?;
        let Some(plan) = plan else {
            return Err(format!("cannot connect to {} as its agent login {}: it doesn't reach {db}", p.name, l.user));
        };
        log::info!("{}: {} had lost its user in {db} (the database made again): put back", p.name, l.user);
        let store = dataman_core::config::ConfigStore::open_with(&self.dirs, self.core.approved.clone()).map_err(|e| format!("{e:#}"))?;
        if let Some(mut saved) = store.get(&p.id)
            && let Some(sl) = saved.agent_login.as_mut()
        {
            let hash = agent_login::rules_hash(p, &sl.database);
            agent_login::live::note_synced(sl, hash, &plan);
            store.upsert(saved).map_err(|e| format!("{e:#}"))?;
            if let Some(f) = self.front() {
                f.connections_changed();
            }
        }
        Ok(())
    }

    /// The user's own login, for what the user allowed in the app that the agent's login may
    /// not run (DDL, TRUNCATE, a procedure…). Only after that Allow.
    pub async fn connect_user(&self, p: &ConnectionProfile) -> Result<Arc<Connection>, String> {
        if p.agent_login.is_none() {
            return self.connect(p).await;
        }
        {
            let g = self.mine.lock();
            if let Some((saved, c)) = g.get(&p.id)
                && saved.same_but_agents(p)
            {
                return Ok(c.clone());
            }
        }
        let mut task = self.core.connect(ConnectionProfile { agent_login: None, ..p.clone() }, None).abort_on_drop();
        let c = match tokio::time::timeout(CONNECT_TIMEOUT, &mut task).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return Err(format!("cannot connect to {} as you, for what you allowed: {e:#}", p.name)),
            Err(_) => return Err(format!("cannot connect to {} as you: timed out", p.name)),
        };
        if let Some((_, old)) = self.mine.lock().insert(p.id.clone(), (p.clone(), c.clone())) {
            old.close();
        }
        Ok(c)
    }

    /// Drops what is open for a connection (its agent login changed or went).
    pub fn forget(&self, id: &str) {
        if let Some((_, c)) = self.open.lock().remove(id) {
            c.close();
        }
        if let Some((_, c)) = self.mine.lock().remove(id) {
            c.close();
        }
        self.forget_schema(id);
    }

    fn forget_schema(&self, id: &str) {
        self.snapshots.lock().retain(|(i, _, _), _| i != id);
        self.refreshed.lock().retain(|(i, _), _| i != id);
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
        for (_, (_, c)) in self.mine.lock().drain() {
            c.close();
        }
    }
}

/// Why agents can't use a connection's login now: its agent login was removed (and the user
/// has not let agents use their own), or the connection moved since the login was made.
pub fn refusal(p: &ConnectionProfile) -> Result<(), String> {
    match &p.agent_login {
        None if p.agents_refused => Err(format!(
            "agents are refused on {}: its agent database login was removed. In Dataman, create a new one (Agents › Permissions › Create agent login) or let agents use your login there",
            p.name
        )),
        Some(l) if l.server != agent_login::server_of(p) => Err(format!(
            "agents are refused on {}: it now points at another server than its agent login {} was made on. In Dataman, remove the agent login and create it again",
            p.name, l.user
        )),
        _ => Ok(()),
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
    fn showing_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(Dirs::at(dir.path().join("d")), Arc::new(dataman_core::MemorySecrets::default())).unwrap();
        for _ in 0..SHOWS {
            hub.show_slot("a").unwrap();
        }
        assert!(hub.show_slot("a").unwrap_err().contains("came forward"));
        assert!(hub.show_slot("another-name").unwrap_err().contains("came forward"), "renaming does not help");
        hub.show_not_used();
        hub.show_slot("a").expect("what did not show takes no place");
        hub.set_show_limit(1, Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        hub.show_slot("a").expect("the window moves on");
        hub.set_show_limit(SHOWS, SHOWS_WITHIN);
        hub.denied("b");
        assert!(hub.show_slot("b").unwrap_err().contains("just denied"), "no window right after a Deny");
        let waiting: Vec<AskSlot> = (0..WAITING_PER_CLIENT).map(|_| hub.ask_slot("c").unwrap()).collect();
        assert!(hub.show_slot("c").unwrap_err().contains("already wait"), "nor over questions waiting");
        drop(waiting);
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
