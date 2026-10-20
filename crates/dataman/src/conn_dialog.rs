//! A connection string's password goes where a typed one goes (the Keychain), never into
//! connections.toml.

use std::time::{Duration, Instant};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, KeyBinding, SharedString, Task,
    Window, actions, div, prelude::*,
};
use crate::theme::{Pt, px};
use dataman_core::config::Engine;
use dataman_core::{ConnectionProfile, Environment, HostKeyPrompt, McpAccess, ServerInfo, SshAuth, SshError, SshSettings, SslMode};

const ENGINES: [Engine; 7] = Engine::ALL;

fn family_label(e: Engine) -> &'static str {
    match e {
        Engine::Postgres | Engine::MySql | Engine::SqlServer => "Relational",
        Engine::MongoDb => "Document",
        Engine::Elasticsearch | Engine::OpenSearch => "Search",
        Engine::Redis => "Key-value",
    }
}

pub fn engines_matching(q: &str) -> Vec<Engine> {
    let q = q.trim().to_lowercase();
    ENGINES
        .into_iter()
        .filter(|e| {
            if q.is_empty() {
                return true;
            }
            let words = match e {
                Engine::Postgres => "postgresql postgres pg lar",
                Engine::MySql => "mysql mariadb",
                Engine::SqlServer => "sql server mssql azure microsoft",
                Engine::MongoDb => "mongodb mongo atlas",
                Engine::Elasticsearch => "elasticsearch elastic es",
                Engine::OpenSearch => "opensearch aws os",
                Engine::Redis => "redis valkey cache",
            };
            let kind = if e.is_sql() { "sql relational" } else { "nosql" };
            format!("{words} {kind} {}", family_label(*e).to_lowercase()).split_whitespace().any(|w| w.starts_with(&q)) || (q.len() >= 3 && e.label().to_lowercase().contains(&q))
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Engine,
    Form,
}

const DOC_TLS: [(SslMode, &str); 3] = [(SslMode::Disable, "off"), (SslMode::Require, "on"), (SslMode::VerifyFull, "verify")];

use crate::conns::ConnStore;
use crate::assets::Icon;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, SWATCHES, Theme};
use crate::ui::{self, Btn};

actions!(conn_dialog, [DialogSave, DialogTest, DialogCancel, NextField, PrevField, PickNext, PickPrev]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("ConnDialog");
    cx.bind_keys([
        KeyBinding::new("cmd-enter", DialogSave, c),
        KeyBinding::new("cmd-t", DialogTest, c),
        KeyBinding::new("escape", DialogCancel, c),
        KeyBinding::new("tab", NextField, c),
        KeyBinding::new("shift-tab", PrevField, c),
        KeyBinding::new("down", PickNext, Some("EnginePicker")),
        KeyBinding::new("up", PickPrev, Some("EnginePicker")),
        KeyBinding::new("right", PickNext, Some("EnginePicker && !TextInput")),
        KeyBinding::new("left", PickPrev, Some("EnginePicker && !TextInput")),
    ]);
}

pub enum DialogEvent {
    Close,
    Save,
    Delete(String),
    Saved { id: String, password: Option<String> },
}

enum Test {
    NotTested,
    Testing { since: Instant, target: String },
    Ok { info: ServerInfo, ms: f64 },
    Failed { title: String, body: String },
    Invalid(String),
    HostKey(HostKeyPrompt),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    Fields,
    Url,
}

struct SshFields {
    on: bool,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    auth: SshAuth,
    key: Entity<TextInput>,
    secret: Entity<TextInput>,
}

struct Advanced {
    open: bool,
    allowed: Vec<String>,
    input: Entity<TextInput>,
    error: Option<String>,
    yolo: bool,
    confirm: bool,
    confirmed: bool,
}

pub struct ConnDialog {
    store: Entity<ConnStore>,
    focus: FocusHandle,
    profile: ConnectionProfile,
    is_new: bool,
    engine: Engine,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    password: Entity<TextInput>,
    database: Entity<TextInput>,
    sslmode: SslMode,
    save_password: bool,
    color: usize,
    env: Environment,
    read_only: bool,
    mcp: Option<McpAccess>,
    advanced: Advanced,
    ssh: SshFields,
    entry: Entry,
    conn_str: Entity<TextInput>,
    auto_name: Option<String>,
    pw_from_str: bool,
    paste_len: [usize; 2],
    test: Test,
    /// Bumped per test, so a stopped test's late answer is ignored.
    test_gen: u64,
    password_needed: bool,
    confirm_delete: bool,
    _ticker: Option<Task<()>>,
    pending_focus: bool,
    step: Step,
    search: Entity<TextInput>,
    pick_hi: usize,
    options: Entity<TextInput>,
}

impl EventEmitter<DialogEvent> for ConnDialog {}

impl Focusable for ConnDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ConnDialog {
    pub fn new(
        store: Entity<ConnStore>,
        profile: Option<ConnectionProfile>,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let is_new = is_new || profile.is_none();
        let profile = profile.unwrap_or_else(|| ConnectionProfile::new(""));
        let field = |cx: &mut Context<Self>, ph: &str, v: &str| {
            let v = v.to_string();
            let ph = ph.to_string();
            let e = cx.new(|cx| {
                let mut t = TextInput::new(cx, ph);
                t.set_text(v, cx);
                t
            });
            cx.subscribe(&e, |d: &mut ConnDialog, _, ev: &InputEvent, cx| match ev {
                InputEvent::Submit => d.save(cx),
                InputEvent::Changed => {
                    if !matches!(d.test, Test::Testing { .. }) {
                        d.test = Test::NotTested;
                    }
                    cx.notify();
                }
            })
            .detach();
            e
        };
        let name = field(cx, "my-database", &profile.name);
        let host = field(cx, "localhost", &profile.host);
        let port = field(cx, &profile.engine.default_port().to_string(), &profile.port.to_string());
        let user = field(cx, "user", &profile.user);
        let database = field(cx, "database", &profile.database);
        for (i, e) in [&name, &host].into_iter().enumerate() {
            cx.subscribe(e, move |d: &mut ConnDialog, input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Changed) {
                    d.on_name_or_host(i, &input, cx);
                }
            })
            .detach();
        }
        let saved = profile.save_password && !is_new;
        let placeholder = if saved { "saved in Keychain" } else { "required by server" };
        let password = cx.new(|cx| TextInput::new(cx, placeholder).masked());
        cx.subscribe(&password, |d: &mut ConnDialog, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => d.save(cx),
            InputEvent::Changed => {
                d.password.update(cx, |p, _| p.error = false);
                if !matches!(d.test, Test::Testing { .. }) {
                    d.test = Test::NotTested;
                }
                cx.notify();
            }
        })
        .detach();
        let s = profile.ssh.clone().unwrap_or_default();
        let local_user = std::env::var("USER").unwrap_or_default();
        let ssh = SshFields {
            on: profile.ssh.is_some(),
            host: field(cx, "bastion", &s.host),
            port: field(cx, "22", &s.port.map(|p| p.to_string()).unwrap_or_default()),
            user: field(cx, &local_user, &s.user),
            auth: if profile.ssh.is_some() { s.auth } else { SshAuth::default() },
            key: field(cx, "~/.ssh/id_ed25519", &s.key_file),
            secret: cx.new(|cx| TextInput::new(cx, if saved && profile.ssh.is_some() { "saved in Keychain" } else { "" }).masked()),
        };
        cx.subscribe(&ssh.secret, |d: &mut ConnDialog, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => d.save(cx),
            InputEvent::Changed => {
                d.ssh.secret.update(cx, |p, _| p.error = false);
                if !matches!(d.test, Test::Testing { .. }) {
                    d.test = Test::NotTested;
                }
                cx.notify();
            }
        })
        .detach();
        let allow_input = cx.new(|cx| TextInput::new(cx, "DELETE FROM sessions WHERE expires_at < ?"));
        cx.subscribe(&allow_input, |d: &mut ConnDialog, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => d.add_allowed(cx),
            InputEvent::Changed => {
                if d.advanced.error.take().is_some() {
                    d.advanced.input.update(cx, |i, _| i.error = false);
                    cx.notify();
                }
            }
        })
        .detach();
        let advanced = Advanced {
            open: false,
            allowed: profile.mcp_allowed.clone(),
            input: allow_input,
            error: None,
            yolo: profile.mcp_yolo,
            confirm: false,
            confirmed: false,
        };
        let conn_str = cx.new(|cx| TextInput::new(cx, "postgresql://user@host:5432/database"));
        cx.subscribe(&conn_str, |d: &mut ConnDialog, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => d.save(cx),
            InputEvent::Changed => d.apply_conn_str(cx),
        })
        .detach();
        let paste_len = [profile.name.len(), profile.host.len()];
        let search = cx.new(|cx| TextInput::new(cx, "Search databases"));
        cx.subscribe(&search, |d: &mut ConnDialog, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => {
                let found = engines_matching(d.search.read(cx).text());
                if let Some(e) = found.get(d.pick_hi.min(found.len().saturating_sub(1))) {
                    d.pick_engine(*e, cx);
                }
            }
            InputEvent::Changed => {
                d.pick_hi = 0;
                cx.notify();
            }
        })
        .detach();
        let opts: Vec<String> = profile.options.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let options = field(cx, "authSource=admin&replicaSet=rs0", &opts.join("&"));
        let step = if profile.host == ConnectionProfile::new("").host && is_new && profile.name.is_empty() { Step::Engine } else { Step::Form };
        if step == Step::Engine {
            window.focus(&search.focus_handle(cx), cx);
        } else {
            window.focus(&name.focus_handle(cx), cx);
        }
        let color = profile
            .color
            .as_deref()
            .and_then(|c| SWATCHES.iter().position(|s| *s == c))
            .unwrap_or_else(|| profile.id.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)) % 5);
        ConnDialog {
            store,
            focus: cx.focus_handle(),
            sslmode: profile.sslmode,
            save_password: profile.save_password || is_new,
            color,
            env: profile.env,
            read_only: profile.read_only,
            mcp: profile.mcp,
            advanced,
            engine: profile.engine,
            profile,
            is_new,
            name,
            host,
            port,
            user,
            password,
            database,
            ssh,
            entry: Entry::Fields,
            conn_str,
            auto_name: None,
            pw_from_str: false,
            paste_len,
            test: Test::NotTested,
            test_gen: 0,
            password_needed: false,
            confirm_delete: false,
            _ticker: None,
            pending_focus: false,
            step,
            search,
            pick_hi: 0,
            options,
        }
    }

    pub fn step(&self) -> Step {
        self.step
    }

    pub fn pick_engine(&mut self, e: Engine, cx: &mut Context<Self>) {
        self.set_engine(e, cx);
        self.step = Step::Form;
        self.pending_focus = true;
        cx.notify();
    }

    fn change_engine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::Engine;
        self.pick_hi = engines_matching(self.search.read(cx).text()).iter().position(|e| *e == self.engine).unwrap_or(0);
        window.focus(&self.search.focus_handle(cx), cx);
        cx.notify();
    }

    fn move_pick(&mut self, step: isize, cx: &mut Context<Self>) {
        let n = engines_matching(self.search.read(cx).text()).len() as isize;
        if n > 0 {
            self.pick_hi = (self.pick_hi as isize + step).rem_euclid(n) as usize;
            cx.notify();
        }
    }

    pub fn demo_search(&mut self, q: &str, cx: &mut Context<Self>) {
        self.search.update(cx, |i, cx| i.set_text(q, cx));
        self.pick_hi = 0;
        cx.notify();
    }

    pub fn password_focus(&self, cx: &App) -> FocusHandle {
        self.password.focus_handle(cx)
    }

    pub fn set_password_needed(&mut self, cx: &mut Context<Self>) {
        self.password_needed = true;
        self.password.update(cx, |p, cx| p.set_placeholder("required by server", cx));
        cx.notify();
    }

    pub fn set_password(&mut self, pw: &str, cx: &mut Context<Self>) {
        self.password.update(cx, |p, cx| p.set_text(pw, cx));
    }

    pub fn run_test(&mut self, cx: &mut Context<Self>) {
        self.test(cx);
    }

    pub fn set_host_key(&mut self, prompt: HostKeyPrompt, cx: &mut Context<Self>) {
        self.ssh.on = true;
        self.test = Test::HostKey(prompt);
        cx.notify();
    }

    pub fn set_ssh_failed(&mut self, message: String, secret_needed: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.ssh.on = true;
        self.test = Test::Failed { title: message, body: String::new() };
        if secret_needed {
            self.ssh.secret.update(cx, |p, _| p.error = true);
            window.focus(&self.ssh.secret.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn on_name_or_host(&mut self, which: usize, input: &Entity<TextInput>, cx: &mut Context<Self>) {
        let text = input.read(cx).text().to_string();
        let pasted = text.len() > self.paste_len[which] + 1;
        self.paste_len[which] = text.len();
        if !pasted {
            return;
        }
        let Some((p, pw)) = dataman_core::connstr::parse_profile(&text) else { return };
        self.fill(&p, pw, which == 0, cx);
    }

    pub fn fill(&mut self, p: &ConnectionProfile, pw: Option<String>, name_too: bool, cx: &mut Context<Self>) {
        let keep_name = !name_too && !self.name.read(cx).text().trim().is_empty();
        if !keep_name {
            self.name.update(cx, |t, cx| t.set_text(p.name.clone(), cx));
        }
        self.engine = p.engine;
        self.step = Step::Form;
        let opts: Vec<String> = p.options.iter().map(|(k, v)| format!("{k}={v}")).collect();
        self.options.update(cx, |t, cx| t.set_text(opts.join("&"), cx));
        self.port.update(cx, |t, cx| t.set_placeholder(p.engine.default_port().to_string(), cx));
        self.host.update(cx, |t, cx| t.set_text(p.host.clone(), cx));
        self.port.update(cx, |t, cx| t.set_text(p.port.to_string(), cx));
        self.user.update(cx, |t, cx| t.set_text(p.user.clone(), cx));
        self.database.update(cx, |t, cx| t.set_text(p.database.clone(), cx));
        if let Some(pw) = pw {
            self.password.update(cx, |t, cx| t.set_text(pw, cx));
        }
        self.sslmode = p.sslmode;
        self.paste_len = [self.name.read(cx).text().len(), self.host.read(cx).text().len()];
        if !matches!(self.test, Test::Testing { .. }) {
            self.test = Test::NotTested;
        }
        cx.notify();
    }

    pub fn set_entry(&mut self, e: Entry, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::Form;
        if self.entry == e {
            return;
        }
        self.entry = e;
        match e {
            Entry::Url => {
                if !self.host.read(cx).text().trim().is_empty() {
                    let url = self.url(cx);
                    self.conn_str.update(cx, |i, cx| {
                        i.set_text(url, cx);
                        i.error = false;
                    });
                }
                window.focus(&self.conn_str.focus_handle(cx), cx);
            }
            Entry::Fields => window.focus(&self.name.focus_handle(cx), cx),
        }
        cx.notify();
    }

    pub fn set_conn_str(&mut self, s: &str, cx: &mut Context<Self>) {
        self.conn_str.update(cx, |i, cx| i.set_text(s, cx));
        self.apply_conn_str(cx);
    }

    pub fn demo_conn_str(&self, cx: &App) -> String {
        self.conn_str.read(cx).text().to_string()
    }

    pub fn conn_str_invalid(&self, cx: &App) -> bool {
        self.conn_str.read(cx).error
    }

    pub fn focus_name(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.name.focus_handle(cx), cx);
    }

    pub fn current(&self, cx: &App) -> (ConnectionProfile, String) {
        (self.fields(cx), self.password.read(cx).text().to_string())
    }

    fn apply_conn_str(&mut self, cx: &mut Context<Self>) {
        let text = self.conn_str.read(cx).text().trim().to_string();
        let parsed = dataman_core::connstr::parse_profile(&text);
        let bad = parsed.is_none() && !text.is_empty();
        self.conn_str.update(cx, |i, _| i.error = bad);
        let Some((mut p, pw)) = parsed else {
            cx.notify();
            return;
        };
        // An http(s) URL reads as Elasticsearch; on an OpenSearch sheet it is OpenSearch's.
        if p.engine == Engine::Elasticsearch && self.engine == Engine::OpenSearch {
            p.engine = Engine::OpenSearch;
            if !text.contains('@') {
                p.user = Engine::OpenSearch.default_user().into();
            }
        }
        let name = self.name.read(cx).text().trim().to_string();
        let rename = name.is_empty() || self.auto_name.as_deref() == Some(name.as_str());
        if rename {
            self.auto_name = Some(p.name.clone());
        }
        let had_pw = pw.is_some();
        self.fill(&p, pw, rename, cx);
        if !had_pw && self.pw_from_str {
            self.password.update(cx, |t, cx| t.set_text("", cx));
        }
        self.pw_from_str = had_pw;
    }

    fn invalid(&mut self, msg: String, cx: &mut Context<Self>) {
        if self.entry == Entry::Url && dataman_core::connstr::parse_profile(self.conn_str.read(cx).text()).is_none() {
            self.conn_str.update(cx, |i, _| i.error = true);
        }
        self.test = Test::Invalid(msg);
        cx.notify();
    }

    fn inputs(&self) -> Vec<&Entity<TextInput>> {
        let mut v = vec![&self.name];
        if self.entry == Entry::Url {
            v.push(&self.conn_str);
        } else {
            v.extend([&self.host, &self.port, &self.user]);
            if self.has_database() {
                v.push(&self.database);
            }
            v.push(&self.password);
            if self.engine == Engine::MongoDb {
                v.push(&self.options);
            }
        }
        if self.ssh.on {
            v.extend([&self.ssh.host, &self.ssh.port, &self.ssh.user]);
            if self.ssh.auth == SshAuth::Key {
                v.push(&self.ssh.key);
            }
            if self.ssh.auth != SshAuth::Agent {
                v.push(&self.ssh.secret);
            }
        }
        if dataman_core::MCP_ADVANCED && self.advanced.open {
            v.push(&self.advanced.input);
        }
        v
    }

    fn focus_field(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let v = self.inputs();
        let n = v.len() as isize;
        let next = match v.iter().position(|i| i.focus_handle(cx).is_focused(window)) {
            Some(at) => (at as isize + step).rem_euclid(n),
            None if step > 0 => 0,
            None => n - 1,
        };
        let f = v[next as usize].clone();
        window.focus(&f.focus_handle(cx), cx);
        f.update(cx, |f, cx| f.select_all(cx));
        cx.notify();
    }

    fn collect(&self, cx: &App) -> Result<(ConnectionProfile, Option<String>, Option<String>), String> {
        if self.entry == Entry::Url && dataman_core::connstr::parse_profile(self.conn_str.read(cx).text()).is_none() {
            return Err("not a connection string".into());
        }
        let mut p = self.fields(cx);
        let port = self.port.read(cx).text().trim().to_string();
        p.port = if port.is_empty() { p.engine.default_port() } else { port.parse().map_err(|_| "port must be a number".to_string())? };
        p.color = Some(SWATCHES[self.color].to_string());
        if p.name.is_empty() {
            p.name = format!("{}@{}", if p.database.is_empty() { &p.user } else { &p.database }, p.host);
        }
        if p.host.is_empty() {
            return Err("host is required".into());
        }
        if p.user.is_empty() && p.engine.is_sql() {
            return Err("host and user are required".into());
        }
        if p.engine == Engine::Redis && !p.database.is_empty() && p.database.parse::<u32>().is_err() {
            return Err("Redis' database is a number (0–15)".into());
        }
        if let Some(s) = &mut p.ssh {
            if s.host.is_empty() {
                return Err("SSH host is required".into());
            }
            let port = self.ssh.port.read(cx).text().trim().to_string();
            s.port = if port.is_empty() { None } else { Some(port.parse().map_err(|_| "SSH port must be a number".to_string())?) };
        }
        let text = |e: &Entity<TextInput>| Some(e.read(cx).text().to_string()).filter(|s| !s.is_empty());
        let secret = if self.ssh.on && self.ssh.auth != SshAuth::Agent { text(&self.ssh.secret) } else { None };
        Ok((p, text(&self.password), secret))
    }

    fn fields(&self, cx: &App) -> ConnectionProfile {
        let mut p = self.profile.clone();
        let text = |e: &Entity<TextInput>| e.read(cx).text().trim().to_string();
        p.name = text(&self.name);
        p.host = text(&self.host);
        p.user = text(&self.user);
        p.database = text(&self.database);
        p.engine = self.engine;
        p.port = match text(&self.port) {
            t if t.is_empty() => p.engine.default_port(),
            t => t.parse().unwrap_or(p.port),
        };
        // MySQL and SQL Server connect without a database (the login's default; every one is
        // browsable); PostgreSQL needs one.
        if p.database.is_empty() && p.engine.is_postgres() {
            p.database = "postgres".into();
        }
        p.sslmode = self.sslmode;
        if !p.engine.is_sql() && p.sslmode == SslMode::Prefer {
            p.sslmode = SslMode::Disable;
        }
        if !self.has_database() {
            p.database.clear();
        }
        p.options = if p.engine == Engine::MongoDb {
            text(&self.options)
                .split('&')
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    Some((k.trim().to_string(), v.trim().to_string())).filter(|(k, _)| !k.is_empty())
                })
                .collect()
        } else {
            Default::default()
        };
        p.save_password = self.save_password;
        p.env = self.env;
        p.read_only = self.read_only;
        p.mcp = self.mcp;
        p.mcp_allowed = self.advanced.allowed.clone();
        p.mcp_yolo = self.advanced.yolo;
        // Saved here, the user has seen where it goes.
        p.agent_made = false;
        p.ssh = self.ssh.on.then(|| SshSettings {
            host: text(&self.ssh.host),
            port: text(&self.ssh.port).parse().ok(),
            user: text(&self.ssh.user),
            auth: self.ssh.auth,
            key_file: if self.ssh.auth == SshAuth::Key { text(&self.ssh.key) } else { String::new() },
        });
        p
    }

    /// Never includes the password.
    fn url(&self, cx: &App) -> String {
        self.fields(cx).url()
    }

    fn set_engine(&mut self, e: Engine, cx: &mut Context<Self>) {
        let old = self.engine;
        if old == e {
            return;
        }
        self.engine = e;
        let text = |i: &Entity<TextInput>, cx: &Context<Self>| i.read(cx).text().trim().to_string();
        let port = text(&self.port, cx);
        if port.is_empty() || port == old.default_port().to_string() {
            self.port.update(cx, |t, cx| t.set_text(e.default_port().to_string(), cx));
        }
        self.port.update(cx, |t, cx| t.set_placeholder(e.default_port().to_string(), cx));
        let default_user = |e: Engine| e.default_user();
        let user = text(&self.user, cx);
        if user.is_empty() || user == default_user(old) {
            self.user.update(cx, |t, cx| t.set_text(default_user(e), cx));
        }
        let db = text(&self.database, cx);
        if !e.is_postgres() && db == "postgres" {
            self.database.update(cx, |t, cx| t.set_text("", cx));
        }
        if !e.is_sql() && self.sslmode == SslMode::Prefer {
            self.sslmode = SslMode::Disable;
        }
        if e.is_sql() && !old.is_sql() && self.sslmode == SslMode::Disable {
            self.sslmode = SslMode::Prefer;
        }
        let ph = match e {
            Engine::Redis => "0",
            Engine::MongoDb => "test",
            _ => "database",
        };
        self.database.update(cx, |t, cx| t.set_placeholder(ph, cx));
        if !matches!(self.test, Test::Testing { .. }) {
            self.test = Test::NotTested;
        }
        cx.notify();
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        if matches!(self.test, Test::Testing { .. }) {
            return;
        }
        let (p, pw, ssh_secret) = match self.collect(cx) {
            Ok(v) => v,
            Err(e) => return self.invalid(e, cx),
        };
        let core = self.store.read(cx).core.clone();
        let since = Instant::now();
        let target = match &p.ssh {
            Some(s) => format!("{}:{} via {}", p.host, p.port, s.host),
            None => format!("{}:{}", p.host, p.port),
        };
        self.test = Test::Testing { since, target };
        self.test_gen += 1;
        let gen_ = self.test_gen;
        // Saved secrets (keychain, pgpass) are looked up off the UI thread.
        let task = core.test_connection(p, pw, ssh_secret);
        self._ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let go_on = this
                    .update(cx, |d, cx| {
                        cx.notify();
                        matches!(d.test, Test::Testing { .. }) && d.test_gen == gen_
                    })
                    .unwrap_or(false);
                if !go_on {
                    break;
                }
            }
        }));
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |d, cx| {
                if d.test_gen != gen_ || !matches!(d.test, Test::Testing { .. }) {
                    return;
                }
                d.test = match r {
                    Ok((info, rtt)) => Test::Ok { info, ms: rtt.as_secs_f64() * 1e3 },
                    Err(e) => match ssh_error(&e) {
                        Some(SshError::UnknownHostKey(prompt)) => Test::HostKey(prompt),
                        Some(s) => {
                            if matches!(s, SshError::SecretNeeded { .. } | SshError::Auth { .. }) && d.ssh.auth != SshAuth::Agent {
                                d.ssh.secret.update(cx, |p, _| p.error = true);
                            }
                            Test::Failed { title: s.to_string(), body: String::new() }
                        }
                        None => {
                            let (title, body, password) = server_message(&e);
                            if password {
                                d.password.update(cx, |p, _| p.error = true);
                            }
                            Test::Failed { title, body }
                        }
                    },
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.test, Test::Testing { .. }) {
            self.test = Test::NotTested;
            self.test_gen += 1;
            cx.notify();
        } else {
            cx.emit(DialogEvent::Close);
        }
    }

    fn trust(&mut self, cx: &mut Context<Self>) {
        let Test::HostKey(prompt) = &self.test else { return };
        let task = self.store.read(cx).core.trust_host_key(prompt.clone());
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |d, cx| match r {
                Ok(()) => {
                    d.test = Test::NotTested;
                    d.test(cx);
                }
                Err(e) => {
                    d.test = Test::Invalid(format!("known_hosts: {e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn profile_id(&self) -> &str {
        &self.profile.id
    }

    pub fn is_new(&self) -> bool {
        self.is_new
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = self.collect(cx) {
            return self.invalid(e, cx);
        }
        // YOLO that became production's in this sheet (tagged production after YOLO was on):
        // confirmed first, as turning it on there is.
        if self.yolo_needs_confirm() {
            self.advanced.open = true;
            self.advanced.confirm = true;
            cx.notify();
            return;
        }
        cx.emit(DialogEvent::Save);
    }

    fn yolo_needs_confirm(&self) -> bool {
        let saved = self.profile.mcp_yolo && self.profile.env.is_production() && !self.is_new;
        self.advanced.yolo && self.env.is_production() && !self.advanced.confirmed && !saved
    }

    fn toggle_yolo(&mut self, cx: &mut Context<Self>) {
        let a = &mut self.advanced;
        if a.yolo || a.confirm {
            (a.yolo, a.confirm) = (false, false);
        } else {
            a.yolo = true;
            if self.yolo_needs_confirm() {
                self.advanced.yolo = false;
                self.advanced.confirm = true;
            }
        }
        cx.notify();
    }

    fn confirm_yolo(&mut self, on: bool, cx: &mut Context<Self>) {
        let a = &mut self.advanced;
        a.confirm = false;
        a.yolo = on;
        a.confirmed |= on;
        cx.notify();
    }

    fn add_allowed(&mut self, cx: &mut Context<Self>) {
        let d = self.engine.dialect();
        let text = self.advanced.input.read(cx).text().to_string();
        let r = dataman_core::guard::allowlist_entry(&text, d).and_then(|e| {
            if self.advanced.allowed.iter().any(|a| dataman_core::guard::allowlist_match(a, &e, d)) { Err("already allowed".to_string()) } else { Ok(e) }
        });
        match r {
            Ok(e) => {
                self.advanced.allowed.push(e);
                self.advanced.error = None;
                self.advanced.input.update(cx, |i, cx| {
                    i.set_text("", cx);
                    i.error = false;
                });
            }
            Err(e) => {
                self.advanced.error = Some(e);
                self.advanced.input.update(cx, |i, _| i.error = true);
            }
        }
        cx.notify();
    }

    pub fn demo_advanced(&mut self, stmt: Option<&str>, yolo: bool, cx: &mut Context<Self>) {
        self.advanced.open = true;
        if let Some(s) = stmt {
            self.advanced.input.update(cx, |i, cx| i.set_text(s, cx));
            self.add_allowed(cx);
        }
        if yolo != (self.advanced.yolo || self.advanced.confirm) {
            self.toggle_yolo(cx);
        }
        cx.notify();
    }

    pub fn demo_advanced_state(&self) -> (Vec<String>, bool, bool) {
        (self.advanced.allowed.clone(), self.advanced.yolo, self.advanced.confirm)
    }

    pub fn demo_engine(&mut self, e: Engine, cx: &mut Context<Self>) {
        self.pick_engine(e, cx);
    }

    pub fn demo_test(&mut self, cx: &mut Context<Self>) {
        self.test(cx);
    }

    pub fn demo_save(&mut self, cx: &mut Context<Self>) {
        self.save(cx);
    }

    pub fn demo_status(&self, cx: &App) -> (String, bool) {
        let s = match &self.test {
            Test::NotTested => "untested".into(),
            Test::Testing { .. } => "testing".into(),
            Test::Ok { .. } => "ok".into(),
            Test::Failed { title, .. } => format!("failed: {title}"),
            Test::Invalid(m) => format!("invalid: {m}"),
            Test::HostKey(_) => "hostkey".into(),
        };
        (s, self.password.read(cx).error)
    }

    pub fn demo_focused(&self, window: &Window, cx: &App) -> Option<usize> {
        [&self.name, &self.host, &self.port, &self.user, &self.password, &self.database].iter().position(|f| f.focus_handle(cx).is_focused(window))
    }

    pub fn demo_field(&self, ix: usize, cx: &App) -> String {
        [&self.name, &self.host, &self.port, &self.user, &self.password, &self.database][ix].read(cx).text().to_string()
    }

    pub fn demo_input(&self, ix: usize) -> Entity<TextInput> {
        [&self.name, &self.host, &self.port, &self.user, &self.password, &self.database][ix].clone()
    }

    pub fn demo_set_field(&self, ix: usize, s: &str, cx: &mut Context<Self>) {
        [&self.name, &self.host, &self.port, &self.user, &self.password, &self.database][ix].clone().update(cx, |t, cx| t.set_text(s, cx));
    }

    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        let (p, pw, ssh_secret) = match self.collect(cx) {
            Ok(v) => v,
            Err(e) => return self.invalid(e, cx),
        };
        let core = self.store.read(cx).core.clone();
        if let Err(e) = core.save_connection(p.clone(), pw.as_deref(), ssh_secret.as_deref()) {
            self.test = Test::Invalid(format!("{e:#}"));
            cx.notify();
            return;
        }
        self.store.update(cx, |s, cx| {
            s.disconnect(&p.id, cx);
            s.reload(cx);
        });
        cx.emit(DialogEvent::Saved { id: p.id, password: pw });
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if !self.confirm_delete {
            self.confirm_delete = true;
            cx.notify();
            return;
        }
        cx.emit(DialogEvent::Delete(self.profile.id.clone()));
    }

    fn has_database(&self) -> bool {
        !matches!(self.engine, Engine::Elasticsearch | Engine::OpenSearch)
    }

    fn render_picker(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let found = engines_matching(self.search.read(cx).text());
        let hi = found.get(self.pick_hi.min(found.len().saturating_sub(1))).copied();
        let tile = |e: Engine| {
            let on = Some(e) == hi;
            let (hov, line) = (t.hov, t.line2);
            div()
                .id(("engine-tile", e as usize))
                .w(px(127.))
                .h(px(96.))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .border_1()
                .border_color(if on { t.acc } else { t.line })
                .when(on, |d| d.bg(t.acc_s))
                .when(!on, |d| d.hover(move |s| s.bg(hov).border_color(line)))
                .cursor_pointer()
                .child(crate::assets::logo(e).size(px(30.)))
                .child(div().text_size(px(12.)).text_color(t.tx).child(e.label()))
                .child(div().text_size(px(10.5)).text_color(t.tx3).child(family_label(e)))
                .on_click(cx.listener(move |d, _, _, cx| d.pick_engine(e, cx)))
        };
        let tile = |e: Engine| ui::probe(format!("engine-{}", e.label()), tile(e));
        let section = |label: &'static str, engines: Vec<Engine>| {
            (!engines.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().text_size(px(10.)).text_color(t.tx3).child(label))
                    .child(div().flex().flex_row().flex_wrap().gap(px(10.)).children(engines.into_iter().map(tile)))
            })
        };
        let (sql, other): (Vec<Engine>, Vec<Engine>) = found.iter().partition(|e| e.is_sql());
        ui::modal(t, "conn-dialog")
            .w(px(600.))
            .child(ui::modal_header(t, None, "New connection", ui::modal_close(t, "dlg-close").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close)))))
            .child(
                div()
                    .key_context("EnginePicker")
                    .on_action(cx.listener(|d, _: &PickNext, _, cx| d.move_pick(1, cx)))
                    .on_action(cx.listener(|d, _: &PickPrev, _, cx| d.move_pick(-1, cx)))
                    .px(px(20.))
                    .pt(px(16.))
                    .pb(px(20.))
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(crate::assets::icon(Icon::Search).size(px(13.)).text_color(t.tx3))
                            .child(div().flex_1().child(self.search.clone())),
                    )
                    .children(section("RELATIONAL", sql))
                    .children(section("NOSQL", other))
                    .when(found.is_empty(), |d| d.child(div().h(px(96.)).flex().items_center().justify_center().text_size(px(12.)).text_color(t.tx3).child("No database by that name"))),
            )
    }

    fn field(&self, t: &Theme, label: &'static str, input: &Entity<TextInput>, label_color: Option<Hsla>) -> gpui::Div {
        self.field_as(t, label, label, input, label_color)
    }

    fn field_as(&self, t: &Theme, probe: &str, label: &'static str, input: &Entity<TextInput>, label_color: Option<Hsla>) -> gpui::Div {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(div().text_size(px(10.)).text_color(label_color.unwrap_or(t.tx3)).child(label))
            .child(ui::probe(format!("field-{probe}"), input.clone()))
    }

    fn result_box(&self, t: &Theme) -> Option<gpui::Div> {
        let (glyph, gcolor, title, body, bg, ring, lar): (&str, Hsla, String, String, Hsla, Hsla, bool) = match &self.test {
            Test::NotTested => return None,
            Test::Testing { since, target } => (
                "◌",
                t.warn,
                format!("Testing {target} … {:.1} s", since.elapsed().as_secs_f64()),
                String::new(),
                t.hov,
                t.line2,
                false,
            ),
            Test::Ok { info, ms } if info.is_lar => (
                "●",
                t.ok,
                format!("Connected in {} · {}", fmt_ms(*ms), info.version_line()),
                String::new(),
                t.lar_s,
                t.lar,
                true,
            ),
            Test::Ok { info, ms } => {
                ("●", t.ok, format!("Connected in {} · {}", fmt_ms(*ms), info.short_version()), String::new(), t.ok_s, t.line2, false)
            }
            Test::Failed { title, body, .. } => ("✕", t.err, title.clone(), body.clone(), t.err_s, t.err, false),
            Test::Invalid(msg) => ("✕", t.err, msg.clone(), String::new(), t.err_s, t.err, false),
            Test::HostKey(_) => return None,
        };
        Some(
            div()
            .px(px(12.))
            .py(px(10.))
            .flex()
            .flex_row()
            .gap(px(10.))
            .bg(bg)
            .border_1()
            .border_color(ring)
            .text_size(px(11.5))
            .line_height(px(17.))
            .child(div().flex_none().text_size(px(11.)).text_color(gcolor).child(glyph))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_color(t.tx).child(title))
                    .children(body.lines().map(|l| div().text_color(t.tx2).child(l.to_string())).collect::<Vec<_>>()),
            )
            .when(lar, |d| d.child(ui::lar_badge(t, "◆ LAR").h(px(18.)).text_size(px(12.)))),
        )
    }

    fn host_key_box(&self, t: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let Test::HostKey(p) = &self.test else { return None };
        Some(
            div()
                .px(px(12.))
                .py(px(10.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .bg(t.warn_s)
                .border_1()
                .border_color(t.line2)
                .text_size(px(11.5))
                .line_height(px(17.))
                .child(div().flex_none().text_size(px(11.)).text_color(t.warn).child("?"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_color(t.tx).child(format!("Unknown host key · {}", p.host)))
                        .child(div().text_color(t.tx2).overflow_hidden().whitespace_nowrap().text_ellipsis().child(format!("{}  {}", p.algorithm, p.fingerprint))),
                )
                .child(
                    ui::probe(
                        "dlg-trust",
                        ui::button(t, "dlg-trust", Btn::Secondary)
                            .h(px(26.))
                            .px(px(10.))
                            .child("Trust")
                            .on_click(cx.listener(|d, _, _, cx| d.trust(cx))),
                    ),
                ),
        )
    }

    fn ssh_section(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let s = &self.ssh;
        let toggle = div()
            .id("ssh-toggle")
            .h(px(18.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .child(ui::checkbox(t, s.on))
            .child(div().text_size(px(10.)).text_color(if s.on { t.tx2 } else { t.tx3 }).child("SSH"))
            .on_click(cx.listener(|d, _, window, cx| {
                d.ssh.on = !d.ssh.on;
                if d.ssh.on {
                    window.focus(&d.ssh.host.focus_handle(cx), cx);
                }
                if !matches!(d.test, Test::Testing { .. }) {
                    d.test = Test::NotTested;
                }
                cx.notify();
            }));
        let mut col = div().flex().flex_col().gap(px(10.)).child(ui::probe("ssh-toggle", toggle));
        if !s.on {
            return col;
        }
        let auth = ui::segmented(t).children(SshAuth::ALL.iter().map(|a| {
            let a = *a;
            ui::probe(
                format!("ssh-auth-{}", a.label()),
                ui::segment(t, ("ssh-auth", a as usize), a.label(), a == s.auth).on_click(cx.listener(move |d, _, _, cx| {
                    d.ssh.auth = a;
                    d.ssh.secret.update(cx, |p, _| p.error = false);
                    cx.notify();
                })),
            )
        }));
        let row = || div().flex().flex_row().gap(px(12.));
        let inner = div()
            .pl(px(22.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(row().child(self.field_as(t, "SSH HOST", "HOST", &s.host, None)).child(div().w(px(96.)).flex_none().child(self.field_as(t, "SSH PORT", "PORT", &s.port, None))))
            .child(
                row()
                    .items_end()
                    .child(self.field_as(t, "SSH USER", "USER", &s.user, None))
                    .child(div().flex_1().flex().flex_col().gap(px(6.)).child(div().text_size(px(10.)).text_color(t.tx3).child("AUTH")).child(auth)),
            )
            .when(s.auth == SshAuth::Key, |d| {
                d.child(row().child(self.field(t, "KEY FILE", &s.key, None)).child(self.field(t, "PASSPHRASE", &s.secret, None)))
            })
            .when(s.auth == SshAuth::Password, |d| d.child(row().child(self.field(t, "SSH PASSWORD", &s.secret, None)).child(div().flex_1())));
        col = col.child(inner);
        col
    }
}

impl ConnDialog {
    fn mcp_advanced(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let a = &self.advanced;
        let off = self.mcp.unwrap_or_else(|| McpAccess::default_for(self.env)) == McpAccess::Off;
        let n = a.allowed.len();
        let summary = match n {
            0 => None,
            1 => Some("1 allowed query".to_string()),
            n => Some(format!("{n} allowed queries")),
        };
        let head = div()
            .id("mcp-advanced")
            .h(px(18.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .child(ui::disclosure("mcp-advanced-chev", a.open, 11., t.tx3))
            .child(div().text_size(px(10.)).text_color(if a.open { t.tx2 } else { t.tx3 }).child("MCP · ADVANCED"))
            .when(!a.open, |d| d.children(summary.map(|s| div().text_size(px(10.5)).text_color(t.tx3).child(s))))
            .when(!a.open && a.yolo && !off, |d| d.child(yolo_badge(t)))
            .on_click(cx.listener(|d, _, window, cx| {
                d.advanced.open = !d.advanced.open;
                if d.advanced.open {
                    window.focus(&d.advanced.input.focus_handle(cx), cx);
                }
                cx.notify();
            }));
        let mut col = div().flex().flex_col().gap(px(10.)).child(head);
        if !a.open {
            return col;
        }
        let note = |text: &'static str| div().text_size(px(11.)).line_height(px(16.)).text_color(t.tx3).child(text);
        let rows = a.allowed.iter().enumerate().map(|(i, s)| {
            div()
                .min_h(px(26.))
                .pl(px(10.))
                .pr(px(2.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .bg(t.card)
                .border_1()
                .border_color(t.line)
                .child(div().flex_1().min_w_0().py(px(4.)).text_size(px(11.5)).text_color(t.tx2).child(s.clone()))
                .child(ui::icon_button(t, ("allowed-rm", i), Icon::Close, 22., 11.).tooltip(ui::text_tooltip("Remove")).on_click(cx.listener(move |d, _, _, cx| {
                    if i < d.advanced.allowed.len() {
                        d.advanced.allowed.remove(i);
                    }
                    cx.notify();
                })))
        });
        let add = div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(8.))
            .child(div().flex_1().child(a.input.clone()))
            .child(ui::button(t, "allowed-add", Btn::Secondary).h(px(28.)).px(px(10.)).child("Add").on_click(cx.listener(|d, _, _, cx| d.add_allowed(cx))));
        let allowed = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(div().text_size(px(10.)).text_color(t.tx3).child("ALLOWED QUERIES"))
            .child(note("Agents run these without asking, whatever the tier above (never while it is off or the database is read-only). Each is one statement, matched statement by statement: the same words in the same order, case, spacing and comments aside; a ? stands for exactly one value (a string, a number or $1), never for more SQL."))
            .children(rows)
            .child(add)
            .children(a.error.clone().map(|e| div().text_size(px(11.)).text_color(t.err).child(e)));
        let switch = div()
            .flex_none()
            .w(px(26.))
            .h(px(14.))
            .relative()
            .bg(if a.yolo { t.err } else { t.line2 })
            .child(div().absolute().top(px(2.)).when(a.yolo, |d| d.right(px(2.))).when(!a.yolo, |d| d.left(px(2.))).size(px(10.)).rounded(px(5.)).bg(if a.yolo { gpui::white() } else { t.tx3 }));
        let yolo = div()
            .id("yolo")
            .px(px(12.))
            .py(px(10.))
            .flex()
            .flex_row()
            .gap(px(10.))
            .border_1()
            .border_color(if a.yolo || a.confirm { t.err } else { t.line2 })
            .when(a.yolo, |d| d.bg(t.err_s))
            .cursor_pointer()
            .on_click(cx.listener(|d, _, _, cx| d.toggle_yolo(cx)))
            .child(div().pt(px(2.)).child(switch))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(t.err).child("YOLO · agents run anything"))
                    .child(div().text_size(px(11.)).line_height(px(16.)).text_color(t.tx2).child(
                        "Every statement an agent sends runs here at once, with nobody asked: DROP, TRUNCATE, DELETE without WHERE, any number of rows, on production too. Read-only still refuses writes; MCP off still hides the connection.",
                    ))
                    .when(a.yolo && off, |d| d.child(div().text_size(px(11.)).text_color(t.tx3).child("No effect while MCP is off."))),
            );
        let confirm = a.confirm.then(|| {
            div()
                .px(px(12.))
                .py(px(10.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .bg(t.err_s)
                .border_1()
                .border_color(t.err)
                .child(div().flex_none().text_size(px(12.)).text_color(t.err).child("!"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.5))
                        .line_height(px(17.))
                        .text_color(t.tx)
                        .child("This connection is production: with YOLO, an agent can change or destroy its data and nobody is asked first."),
                )
                .child(ui::button(t, "yolo-cancel", Btn::Ghost).h(px(26.)).child("Cancel").on_click(cx.listener(|d, _, _, cx| d.confirm_yolo(false, cx))))
                .child(ui::button(t, "yolo-confirm", Btn::Danger).h(px(26.)).font_weight(FontWeight::SEMIBOLD).child("Turn On YOLO").on_click(cx.listener(|d, _, _, cx| d.confirm_yolo(true, cx))))
        });
        col = col.child(div().pl(px(17.)).flex().flex_col().gap(px(12.)).child(allowed).child(yolo).children(confirm));
        col
    }
}

pub(crate) fn yolo_badge(t: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .h(px(17.))
        .px(px(6.))
        .flex()
        .items_center()
        .bg(t.err)
        .text_color(t.on_err)
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .child("YOLO")
}

pub fn ssh_error(e: &anyhow::Error) -> Option<SshError> {
    e.chain().find_map(|c| c.downcast_ref::<SshError>()).cloned()
}

impl ServerInfoExt for ServerInfo {
    fn version_line(&self) -> String {
        let mut v = self.short_version();
        if self.is_lar {
            v.push_str(" (LAR)");
        }
        v
    }
}

trait ServerInfoExt {
    fn version_line(&self) -> String;
}

fn fmt_ms(ms: f64) -> String {
    if ms < 10. { format!("{ms:.1} ms") } else { format!("{ms:.0} ms") }
}

pub fn server_message(e: &anyhow::Error) -> (String, String, bool) {
    if let Some(s) = ssh_error(e) {
        return (s.to_string(), String::new(), false);
    }
    match dataman_core::server_error(e) {
        Some(db) => {
            let code = db.code.unwrap_or_default();
            let title = format!("{} {} · {}", db.severity.unwrap_or_default(), code, db.message);
            let mut lines = Vec::new();
            if let Some(d) = db.detail {
                lines.push(format!("DETAIL  {d}"));
            }
            if let Some(h) = db.hint {
                lines.push(format!("HINT  {h}"));
            }
            // PostgreSQL's 28P01/28000, MySQL's 1045 (access denied), SQL Server's 18456 (login
            // failed).
            (title, lines.join("\n"), code == "28P01" || code == "28000" || code == "1045" || code == "18456")
        }
        None => {
            let msg = format!("{e:#}");
            let password = msg.contains("password");
            let title = msg.lines().next().unwrap_or(&msg).to_string();
            (title, String::new(), password)
        }
    }
}

impl Render for ConnDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let testing = matches!(self.test, Test::Testing { .. });
        let color = if self.env.is_production() { t.err } else { t.swatches[self.color] };
        let title: SharedString = if self.password_needed {
            format!("Password needed · {}", self.profile.name).into()
        } else if self.is_new {
            "New connection".into()
        } else {
            "Edit connection".into()
        };
        let pw_focused = self.password.focus_handle(cx).is_focused(window);
        let swatches = div().h(px(30.)).flex().flex_row().items_center().gap(px(6.)).children((0..SWATCHES.len()).map(|i| {
            let c = t.swatches[i];
            let on = i == self.color;
            let swatch = div()
                .id(("swatch", i))
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .when(on, |d| d.border_2().border_color(c))
                .on_click(cx.listener(move |d, _, _, cx| {
                    d.color = i;
                    cx.notify();
                }))
                .child(div().size(px(if on { 14. } else { 18. })).bg(c));
            ui::probe(format!("swatch-{i}"), swatch)
        }));
        if self.step == Step::Engine {
            return self.wrap(self.render_picker(&t, cx), &t, cx);
        }
        if std::mem::take(&mut self.pending_focus) {
            window.focus(&self.name.focus_handle(cx), cx);
        }
        let (hov, prs) = (t.hov, t.prs);
        let engine = div()
            .id("engine-chip")
            .h(px(30.))
            .px(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            .border_1()
            .border_color(t.line2)
            .text_size(px(11.5))
            .text_color(t.tx)
            .cursor_pointer()
            .hover(move |s| s.bg(hov))
            .active(move |s| s.bg(prs))
            .child(crate::assets::logo(self.engine).size(px(16.)))
            .child(self.engine.label())
            .child(ui::dropdown_arrow("engine-chip-arrow", false, 9., t.tx3))
            .tooltip(ui::text_tooltip("Change database"))
            .on_click(cx.listener(|d, _, window, cx| d.change_engine(window, cx)));
        let modes: Vec<(SslMode, &'static str)> = if self.engine.is_sql() { SslMode::ALL.iter().map(|m| (*m, m.label())).collect() } else { DOC_TLS.to_vec() };
        let ssl = ui::segmented(&t).children(modes.into_iter().map(|(m, label)| {
            ui::probe(
                format!("tls-{label}"),
                ui::segment(&t, m.label(), label, m == self.sslmode).on_click(cx.listener(move |d, _, _, cx| {
                    d.sslmode = m;
                    cx.notify();
                })),
            )
        }));
        let env = ui::segmented(&t).children(Environment::ALL.iter().map(|e| {
            let e = *e;
            ui::probe(
                format!("env-{}", e.label()),
                ui::segment(&t, ("env", e as usize), e.label(), e == self.env)
                    .when(e.is_production() && e == self.env, |d| d.text_color(t.err))
                    .on_click(cx.listener(move |d, _, _, cx| {
                        d.env = e;
                        cx.notify();
                    })),
            )
        }));
        let mcp_cur = self.mcp.unwrap_or_else(|| McpAccess::default_for(self.env));
        let yolo_on = dataman_core::MCP_ADVANCED && self.advanced.yolo && mcp_cur != McpAccess::Off;
        let mcp = ui::segmented(&t).children(McpAccess::ALL.iter().map(|a| {
            let a = *a;
            ui::probe(
                format!("mcp-{}", a.label()),
                ui::segment(&t, ("mcp", a as usize), a.label(), a == mcp_cur)
                    .when(a >= McpAccess::Write && a == mcp_cur && self.env.is_production(), |d| d.text_color(t.err))
                    .on_click(cx.listener(move |d, _, _, cx| {
                        d.mcp = Some(a);
                        cx.notify();
                    })),
            )
        }));
        let read_only = div()
            .id("read-only")
            .h(px(28.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .text_size(px(11.5))
            .text_color(t.tx2)
            .cursor_pointer()
            .child(ui::checkbox(&t, self.read_only))
            .child("Read-only")
            .on_click(cx.listener(|d, _, _, cx| {
                d.read_only = !d.read_only;
                cx.notify();
            }));
        let keychain = div()
            .id("save-pw")
            .h(px(30.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .text_size(px(11.5))
            .text_color(t.tx2)
            .cursor_pointer()
            .child(ui::checkbox(&t, self.save_password))
            .child("Save in Keychain")
            .on_click(cx.listener(|d, _, _, cx| {
                d.save_password = !d.save_password;
                cx.notify();
            }));
        let url = self.url(cx);
        let url2 = url.clone();
        let by_url = self.entry == Entry::Url;
        let entry = ui::segmented(&t).h(px(24.)).children([(Entry::Fields, "Fields"), (Entry::Url, "URL")].map(|(e, label)| {
            ui::probe(format!("entry-{label}"), ui::segment(&t, ("entry", e as usize), label, e == self.entry).on_click(cx.listener(move |d, _, window, cx| d.set_entry(e, window, cx))))
        }));
        let caption = |text: &'static str| div().text_size(px(10.)).text_color(t.tx3).child(text);
        let row = || div().flex().flex_row().gap(px(12.));
        let server: Vec<gpui::Div> = if by_url {
            vec![row().items_end().child(self.field(&t, "URL", &self.conn_str, None)).child(ui::probe("save-pw", keychain))]
        } else {
            let db_label = if self.engine == Engine::Redis { "DB" } else { "DATABASE" };
            let user_label = if self.engine.is_sql() { "USER" } else { "USER (OPTIONAL)" };
            let mut v = vec![
                row().child(self.field(&t, "HOST", &self.host, None)).child(div().w(px(96.)).flex_none().child(self.field(&t, "PORT", &self.port, None))),
                row().child(self.field(&t, user_label, &self.user, None)).when(self.has_database(), |r| r.child(self.field(&t, db_label, &self.database, None))),
                row()
                    .items_end()
                    .child(self.field(&t, "PASSWORD", &self.password, (pw_focused || self.password_needed).then_some(t.acc_t)))
                    .child(ui::probe("save-pw", keychain)),
            ];
            if self.engine == Engine::MongoDb {
                v.push(row().child(self.field(&t, "OPTIONS", &self.options, None)));
            }
            v
        };
        let max_h = window.viewport_size().height.pt() - crate::theme::metrics::TITLE_H - 32.;
        let card = ui::modal(&t, "conn-dialog")
            .w(px(600.))
            .max_h(px(max_h))
            .child(ui::modal_header_with(
                &t,
                Some(ui::chip(color, 10.).into_any_element()),
                title,
                (!self.password_needed).then(|| entry.into_any_element()),
                ui::modal_close(&t, "dlg-close").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close))),
            ))
            .child(
                div()
                    .id("dlg-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(20.))
                    .pt(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(12.))
                            .items_end()
                            .child(self.field(&t, "NAME", &self.name, None))
                            .when(!by_url, |d| d.child(div().flex().flex_col().gap(px(6.)).child(caption("ENGINE")).child(ui::probe("engine-chip", engine))))
                            .child(div().flex().flex_col().gap(px(6.)).child(caption("COLOR")).child(swatches)),
                    )
                    .children(server)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_end()
                            .gap(px(16.))
                            .when(!by_url, |d| d.child(div().flex().flex_col().gap(px(6.)).child(caption(if self.engine.is_sql() { "SSL" } else { "TLS" })).child(ssl)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.))
                                    .child(div().h(px(12.)).flex().flex_row().items_center().gap(px(6.)).child(caption("MCP")).when(yolo_on, |d| d.child(yolo_badge(&t).h(px(12.)).px(px(4.)).text_size(px(9.)))))
                                    .child(mcp),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_end()
                            .gap(px(16.))
                            .child(div().flex().flex_col().gap(px(6.)).child(div().text_size(px(10.)).text_color(t.tx3).child("ENVIRONMENT")).child(env))
                            .child(ui::probe("read-only", read_only)),
                    )
                    .when(dataman_core::MCP_ADVANCED, |d| d.child(self.mcp_advanced(&t, cx)))
                    .child(self.ssh_section(&t, cx))
                    .when(!by_url, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.))
                                .text_size(px(11.))
                                .text_color(t.tx3)
                                .child(div().text_color(t.tx4).child("url"))
                                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(url))
                                .child(
                                    div()
                                        .id("copy-url")
                                        .text_color(t.acc_t)
                                        .cursor_pointer()
                                        .child("copy")
                                        .on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(url2.clone())))),
                                ),
                        )
                    })
                    .children(self.result_box(&t))
                    .children(self.host_key_box(&t, cx)),
            )
            .child(
                ui::modal_footer(&t)
                    .mt(px(14.))
                    .child(
                        ui::probe("dlg-test", ui::button(&t, "dlg-test", if testing { Btn::Ghost } else { Btn::Secondary })
                            .h(px(28.))
                            .px(px(10.))
                            .when(testing, |d| d.bg(t.warn_s).text_color(t.warn))
                            .child(if testing { "◌ Testing…" } else { "Test" })
                            .child(ui::key_hint("⌘T"))
                            .on_click(cx.listener(|d, _, _, cx| d.test(cx))))
                    )
                    .when(!self.is_new && !self.password_needed, |d| {
                        d.child(
                            ui::probe("dlg-delete", ui::button(&t, "dlg-delete", Btn::Ghost)
                                .h(px(28.))
                                .px(px(10.))
                                .text_color(t.err)
                                .child(if self.confirm_delete { "Delete — sure?" } else { "Delete…" })
                                .on_click(cx.listener(|d, _, _, cx| d.delete(cx))))
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        ui::probe("dlg-cancel", ui::button(&t, "dlg-cancel", Btn::Ghost)
                            .h(px(28.))
                            .px(px(12.))
                            .child("Cancel")
                            .child(div().text_size(px(10.5)).opacity(0.6).child("esc"))
                            .on_click(cx.listener(|d, _, _, cx| d.cancel(cx))))
                    )
                    .child(
                        ui::probe("dlg-save", ui::button(&t, "dlg-save", if testing { Btn::Ghost } else { Btn::Primary })
                            .h(px(28.))
                            .px(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(testing, |d| d.bg(t.prs).text_color(t.tx3))
                            .child(if self.is_new || self.password_needed { "Save & connect" } else { "Save" })
                            .child(div().text_size(px(10.5)).font_weight(FontWeight::NORMAL).opacity(0.75).child("⌘↩"))
                            .on_click(cx.listener(|d, _, _, cx| d.save(cx))))
                    ),
            );
        self.wrap(card, &t, cx)
    }
}

impl ConnDialog {
    fn wrap(&self, card: gpui::Stateful<gpui::Div>, t: &Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id("conn-dialog-backdrop")
            .absolute()
            .top(px(crate::theme::metrics::TITLE_H))
            .left_0()
            .right_0()
            .bottom_0()
            .bg(t.scrim)
            .flex()
            .items_center()
            .justify_center()
            .key_context("ConnDialog Modal")
            .track_focus(&self.focus)
            .on_action(cx.listener(|d, _: &DialogSave, _, cx| d.save(cx)))
            .on_action(cx.listener(|d, _: &DialogTest, _, cx| d.test(cx)))
            .on_action(cx.listener(|d, _: &DialogCancel, _, cx| d.cancel(cx)))
            .on_action(cx.listener(|d, _: &NextField, window, cx| d.focus_field(1, window, cx)))
            .on_action(cx.listener(|d, _: &PrevField, window, cx| d.focus_field(-1, window, cx)))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_failures_are_told_apart() {
        let e = anyhow::Error::from(SshError::SecretNeeded { host: "bastion".into(), what: "password" }).context("connecting");
        assert!(matches!(ssh_error(&e), Some(SshError::SecretNeeded { .. })));
        assert_eq!(server_message(&e), ("SSH bastion · password needed".into(), String::new(), false));
        let e = anyhow::anyhow!("password authentication failed for user \"app\"");
        assert!(ssh_error(&e).is_none());
        assert!(server_message(&e).2);
        let k = HostKeyPrompt::from_openssh("bastion", 2222, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ").unwrap();
        assert_eq!((k.host.as_str(), k.algorithm.as_str()), ("bastion:2222", "ssh-ed25519"));
        assert!(k.fingerprint.starts_with("SHA256:"));
    }

    #[test]
    fn engines_are_found_by_what_people_call_them() {
        assert_eq!(engines_matching(""), ENGINES.to_vec());
        assert_eq!(engines_matching("mongo"), vec![Engine::MongoDb]);
        assert_eq!(engines_matching("ES"), vec![Engine::Elasticsearch]);
        assert_eq!(engines_matching("pg"), vec![Engine::Postgres]);
        assert_eq!(engines_matching("maria"), vec![Engine::MySql]);
        assert_eq!(engines_matching("nosql"), vec![Engine::MongoDb, Engine::Elasticsearch, Engine::OpenSearch, Engine::Redis]);
        assert_eq!(engines_matching("search"), vec![Engine::Elasticsearch, Engine::OpenSearch]);
        assert!(engines_matching("oracle").is_empty());
    }
}
