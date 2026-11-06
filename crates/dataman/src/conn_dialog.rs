//! New connection, in steps (Database, Connection, Sign-in, MCP), and Edit connection, one form
//! with the same panels. A connection string's password goes where a typed one goes (the Keychain),
//! never into connections.toml.

use std::sync::Arc;
use std::time::Instant;

use crate::theme::{Pt, px};
use dataman_core::cloudauth::{self, Cli, Fix, SignInError};
use dataman_core::config::Engine;
use dataman_core::{Auth, AuthKind, ConnectionProfile, Environment, HostKeyPrompt, McpAccess, ServerInfo, SshAuth, SshError, SshSettings, SslMode, connstr};
use dataman_mcp::agents::{self, AGENTS, Added, Listing};
use gpui::{App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, SharedString, Task, Window, actions, div, prelude::*};

const ENGINES: [Engine; 9] = Engine::ALL;
const SAVED_IN: &str = if cfg!(target_os = "macos") {
    "saved in Keychain"
} else if cfg!(windows) {
    "saved in Credential Manager"
} else {
    "saved in keyring"
};

fn user_hint(engine: Engine, kind: AuthKind) -> (&'static str, &'static str) {
    match (kind, engine) {
        (AuthKind::EntraId, _) => ("ana@contoso.com, or a group's name", "USER · ENTRA UPN OR GROUP"),
        (AuthKind::AwsIam, Engine::MySql) => ("a user WITH AWSAuthenticationPlugin", "USER · DATABASE USER FOR IAM"),
        (AuthKind::AwsIam, _) => ("a user granted rds_iam", "USER · DATABASE USER FOR IAM"),
        (AuthKind::GoogleIam, Engine::MySql) => ("ana (the e-mail before @)", "USER · IAM USER"),
        (AuthKind::GoogleIam, _) => ("ana@example.com, or sa@project.iam", "USER · IAM USER OR SERVICE ACCOUNT"),
        (AuthKind::Password, e) if e.spec().form.user_required => ("user", "USER"),
        (AuthKind::Password, _) => ("user", "USER (OPTIONAL)"),
    }
}

fn preselected(engine: Engine, host: &str) -> AuthKind {
    engine.auth_kinds().iter().copied().find(|k| k.is_token() && cloudauth::host_allowed(*k, host)).unwrap_or(AuthKind::Password)
}

fn foreign_host_note(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::EntraId => "Not an Azure host: your token goes to it.",
        AuthKind::AwsIam => "Not an RDS host: your token goes to it.",
        AuthKind::GoogleIam => "Your Google token goes to this address.",
        AuthKind::Password => "",
    }
}

fn method_label(kind: AuthKind, user: &str, password_there: bool) -> &'static str {
    match kind {
        AuthKind::Password if !user.trim().is_empty() && password_there => "Keep user & password",
        AuthKind::Password => "User & password",
        k => k.label(),
    }
}

/// What an unnamed connection is called: its host's first label (an address stays whole).
fn host_stem(host: &str) -> &str {
    let host = host.trim();
    if host.parse::<std::net::IpAddr>().is_ok() { host } else { host.split('.').next().unwrap_or(host) }
}

pub fn engines_matching(q: &str) -> Vec<Engine> {
    let q = q.trim().to_lowercase();
    ENGINES
        .into_iter()
        .filter(|e| {
            if q.is_empty() {
                return true;
            }
            let words = e.spec().search;
            let kind = if e.is_sql() { "sql relational" } else { "nosql" };
            format!("{words} {kind} {}", e.family_label().to_lowercase()).split_whitespace().any(|w| w.starts_with(&q))
                || (q.len() >= 3 && e.label().to_lowercase().contains(&q))
        })
        .collect()
}

/// The Database step's tiles for what is in its search: none for a connection string, which either
/// reads (and goes on to Connection) or does not.
fn tiles_for(search: &str) -> Vec<Engine> {
    if search.contains("://") { Vec::new() } else { engines_matching(search) }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Database,
    Connection,
    SignIn,
    Mcp,
}

/// Always all four, whichever database: a file's Sign-in shows dimmed and is skipped.
const STEPS: [(Step, &str); 4] = [(Step::Database, "Database"), (Step::Connection, "Connection"), (Step::SignIn, "Sign-in"), (Step::Mcp, "MCP")];

impl Step {
    fn after(self, file: bool) -> Option<Step> {
        match self {
            Step::Database => Some(Step::Connection),
            Step::Connection if file => Some(Step::Mcp),
            Step::Connection => Some(Step::SignIn),
            Step::SignIn => Some(Step::Mcp),
            Step::Mcp => None,
        }
    }
}

fn crumb_state(step: Step, at: Step, seen: [bool; 4], picked: bool, file: bool) -> ui::CrumbState {
    if step == at {
        ui::CrumbState::Current
    } else if picked && file && step == Step::SignIn {
        ui::CrumbState::Skipped
    } else if picked && seen[step as usize] {
        ui::CrumbState::Visited
    } else {
        ui::CrumbState::Next
    }
}

/// The TLS modes an engine offers: each one's key for the check stages, and its name.
const ENVIRONMENTS: [(Environment, &str); 4] =
    [(Environment::None, "None"), (Environment::Dev, "Dev"), (Environment::Staging, "Staging"), (Environment::Production, "Production")];

const SQL_TLS: [(SslMode, &str, &str); 4] = [
    (SslMode::Disable, "disable", "Disable"),
    (SslMode::Prefer, "prefer", "Prefer"),
    (SslMode::Require, "require", "Require"),
    (SslMode::VerifyFull, "verify-full", "Verify full"),
];
const DOC_TLS: [(SslMode, &str, &str); 3] = [(SslMode::Disable, "off", "Off"), (SslMode::Require, "on", "On"), (SslMode::VerifyFull, "verify", "Verify")];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Can {
    No,
    Asks,
    Yes,
}

/// READS, WRITES, DESTRUCTIVE of each tier.
const TIERS: [(McpAccess, &str, [Can; 3]); 4] = [
    (McpAccess::Off, "Off", [Can::No, Can::No, Can::No]),
    (McpAccess::Read, "Read", [Can::Yes, Can::No, Can::No]),
    (McpAccess::Write, "Write", [Can::Yes, Can::Yes, Can::No]),
    (McpAccess::Full, "Full", [Can::Yes, Can::Yes, Can::Asks]),
];

/// A tier's answer on this connection: read-only takes every write away, production asks first.
fn tier_cell(column: usize, can: Can, production: bool, read_only: bool) -> Can {
    match (column, can) {
        (1 | 2, _) if read_only => Can::No,
        (1, Can::Yes) if production => Can::Asks,
        _ => can,
    }
}

use crate::assets::Icon;
use crate::conns::ConnStore;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, SWATCHES, Theme};
use crate::ui::Tipped;
use crate::ui::{self, Btn};

actions!(conn_dialog, [DialogSave, DialogTest, DialogCancel, NextField, PrevField, PickNext, PickPrev]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("ConnDialog");
    cx.bind_keys([
        // The footer's last button, in whichever step: a field's own Enter does the same.
        crate::keymap::kb("enter", DialogSave, c),
        crate::keymap::kb("cmd-enter", DialogSave, c),
        crate::keymap::kb("cmd-t", DialogTest, c),
        crate::keymap::kb("escape", DialogCancel, c),
        crate::keymap::kb("tab", NextField, c),
        crate::keymap::kb("shift-tab", PrevField, c),
        crate::keymap::kb("down", PickNext, Some("EnginePicker")),
        crate::keymap::kb("up", PickPrev, Some("EnginePicker")),
        crate::keymap::kb("right", PickNext, Some("EnginePicker && !TextInput")),
        crate::keymap::kb("left", PickPrev, Some("EnginePicker && !TextInput")),
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
    Testing { target: String },
    Ok { info: ServerInfo, ms: f64 },
    Failed { title: String, body: String },
    Invalid(String),
    HostKey(HostKeyPrompt),
}

/// A cloud method's state, one per method: nothing asks its CLI until `Sign in` is clicked.
enum SignIn {
    Unchecked,
    Checking,
    SigningIn,
    Ok { who: Option<String> },
    Failed { title: String, body: String, fix: Option<Fix> },
}

impl SignIn {
    fn busy(&self) -> bool {
        matches!(self, SignIn::Checking | SignIn::SigningIn)
    }

    fn failed(e: &anyhow::Error) -> SignIn {
        let (title, body, _) = server_message(e);
        SignIn::Failed { title, body, fix: cloudauth::sign_in_error(e).and_then(|s| s.action) }
    }

    fn missing(&self) -> bool {
        matches!(self, SignIn::Failed { fix: Some(Fix::Install(_)), .. })
    }

    /// The status at the right of the method's row: grey, but for the dot of who is signed in.
    fn line(&self, cli: &str) -> Option<(&'static str, String)> {
        Some(match self {
            SignIn::Unchecked => return None,
            SignIn::Checking => ("◌", format!("Checking {cli}")),
            SignIn::SigningIn => ("◌", "Waiting for the browser".to_string()),
            SignIn::Ok { who: Some(w) } => ("●", cloudauth::short_identity(w).to_string()),
            SignIn::Ok { who: None } => ("●", "Signed in".to_string()),
            SignIn::Failed { fix: Some(Fix::Install(_)), .. } => ("✕", format!("{cli} not found")),
            SignIn::Failed { fix: Some(Fix::Again(_)), .. } => ("✕", format!("{cli} did not answer")),
            SignIn::Failed { title, .. } => ("✕", title.clone()),
        })
    }
}

/// Which fields of the connection a field's edit touches: the string that carries it, and what a
/// signed-in status said about the old value.
#[derive(Clone, Copy)]
struct Feeds {
    string: bool,
    sign_in: bool,
}

const PLAIN: Feeds = Feeds { string: false, sign_in: false };
const IN_STRING: Feeds = Feeds { string: true, sign_in: false };
const IN_SIGN_IN: Feeds = Feeds { string: false, sign_in: true };
const IN_BOTH: Feeds = Feeds { string: true, sign_in: true };

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

#[derive(Clone, PartialEq, Eq)]
enum AgentAdd {
    Idle,
    Adding,
    Done,
    Failed(String),
}

pub struct ConnDialog {
    store: Entity<ConnStore>,
    focus: FocusHandle,
    profile: ConnectionProfile,
    is_new: bool,
    engine: Engine,
    /// The Database step was answered (or skipped by a filled sheet): the header shows its logo.
    engine_picked: bool,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    password: Entity<TextInput>,
    database: Entity<TextInput>,
    sslmode: SslMode,
    /// Picked by the user (or a pasted string, or a saved connection): the host no longer decides.
    ssl_chosen: bool,
    color: usize,
    /// A swatch was clicked (now or before); production's red still wins while it is production.
    color_picked: bool,
    env: Environment,
    read_only: bool,
    mcp: Option<McpAccess>,
    advanced: Advanced,
    ssh: SshFields,
    conn_str: Entity<TextInput>,
    str_bad: bool,
    /// A password is really saved for this connection.
    saved_pw: bool,
    /// What the string gave for each: compared with the fields for the note under them.
    str_user: Option<String>,
    str_pw: Option<String>,
    auto_name: Option<String>,
    paste_len: [usize; 2],
    test: Test,
    /// Bumped per test, so a stopped test's late answer is ignored.
    test_gen: u64,
    password_needed: bool,
    confirm_delete: bool,
    step: Step,
    seen: [bool; 4],
    /// The step the Database step was opened from: picking a database goes back to it.
    return_to: Option<Step>,
    search: Entity<TextInput>,
    search_len: usize,
    pick_hi: usize,
    options: Entity<TextInput>,
    file: Entity<TextInput>,
    /// Picked with New…: the file is created when the connection is tested or saved.
    create_file: bool,
    saving: bool,
    /// DuckDB: SQL may read and write files (`read_csv`, `COPY … TO`, ATTACH).
    file_access: bool,
    /// How it signs in; each cloud method's fields stay while another is picked.
    auth: AuthKind,
    /// The user (or a password) chose it: the host no longer does.
    auth_picked: bool,
    tenant: Entity<TextInput>,
    aws_profile: Entity<TextInput>,
    aws_region: Entity<TextInput>,
    gcp_account: Entity<TextInput>,
    /// By `AuthKind`: each method keeps its own, and its task (dropped, the CLI is stopped).
    sign_in: [SignIn; 4],
    sign_in_tasks: [Option<Task<()>>; 4],
    unrolls: [crate::motion::Unroll; 4],
    body_scroll: gpui::ScrollHandle,
    /// A field that took the focus, to scroll to once it has been drawn.
    reveal: Option<Entity<TextInput>>,
    agents: Option<Arc<Listing>>,
    agent_adds: [AgentAdd; AGENTS.len()],
    copied: ui::CopiedFlash,
}

impl EventEmitter<DialogEvent> for ConnDialog {}

impl Focusable for ConnDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn text_field(placeholder: &str, value: &str, feeds: Feeds, window: &mut Window, cx: &mut Context<ConnDialog>) -> Entity<TextInput> {
    let (placeholder, value) = (placeholder.to_string(), value.to_string());
    let e = cx.new(|cx| {
        let mut t = TextInput::new(cx, placeholder);
        t.set_text(value, cx);
        t
    });
    cx.subscribe_in(&e, window, move |d: &mut ConnDialog, _, ev: &InputEvent, window, cx| match ev {
        InputEvent::Submit => d.primary(window, cx),
        InputEvent::Changed => d.field_changed(feeds, cx),
    })
    .detach();
    e
}

fn secret_field(placeholder: &str, feeds: Feeds, window: &mut Window, cx: &mut Context<ConnDialog>) -> Entity<TextInput> {
    let placeholder = placeholder.to_string();
    let e = cx.new(|cx| TextInput::new(cx, placeholder).masked());
    cx.subscribe_in(&e, window, move |d: &mut ConnDialog, input, ev: &InputEvent, window, cx| match ev {
        InputEvent::Submit => d.primary(window, cx),
        InputEvent::Changed => {
            input.update(cx, |p, _| p.error = false);
            d.field_changed(feeds, cx);
        }
    })
    .detach();
    e
}

impl ConnDialog {
    pub fn new(store: Entity<ConnStore>, profile: Option<ConnectionProfile>, is_new: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let is_new = is_new || profile.is_none();
        let profile = profile.unwrap_or_else(|| ConnectionProfile::new(""));
        // A blank sheet asks for everything, its defaults only as placeholders.
        let blank = is_new && profile.name.is_empty() && profile.host == ConnectionProfile::new("").host;
        let shown = |v: &'_ str| if blank { String::new() } else { v.to_string() };
        let name = text_field("", &profile.name, PLAIN, window, cx);
        let host = text_field("localhost", &shown(&profile.host), IN_BOTH, window, cx);
        let port = text_field(&profile.engine.default_port().to_string(), &profile.port.to_string(), IN_BOTH, window, cx);
        let (user_placeholder, _) = user_hint(profile.engine, profile.auth.kind());
        let user = text_field(user_placeholder, &shown(&profile.user), IN_BOTH, window, cx);
        let database = text_field(profile.engine.spec().form.database_placeholder, &shown(&profile.database), IN_STRING, window, cx);
        for (i, e) in [&name, &host].into_iter().enumerate() {
            cx.subscribe(e, move |d: &mut ConnDialog, input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Changed) {
                    d.on_name_or_host(i, &input, cx);
                    if i == 1 {
                        d.host_moved();
                        d.follow_host(cx);
                        d.name_follows_host(cx);
                    }
                }
            })
            .detach();
        }
        let password = secret_field("", IN_STRING, window, cx);
        let s = profile.ssh.clone().unwrap_or_default();
        let local_user = std::env::var("USER").unwrap_or_default();
        let ssh = SshFields {
            on: profile.ssh.is_some(),
            host: text_field("bastion, or an ~/.ssh/config alias", &s.host, PLAIN, window, cx),
            port: text_field("22", &s.port.map(|p| p.to_string()).unwrap_or_default(), PLAIN, window, cx),
            user: text_field(&local_user, &s.user, PLAIN, window, cx),
            auth: if profile.ssh.is_some() { s.auth } else { SshAuth::default() },
            key: text_field("~/.ssh/id_ed25519", &s.key_file, PLAIN, window, cx),
            secret: secret_field("", PLAIN, window, cx),
        };
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
        let conn_str = cx.new(|cx| {
            let mut t = TextInput::new(cx, string_placeholder(profile.engine));
            if !blank && !profile.engine.is_file() {
                t.set_text(profile.url(), cx);
            }
            t
        });
        cx.subscribe_in(&conn_str, window, |d: &mut ConnDialog, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Submit => d.primary(window, cx),
            InputEvent::Changed => d.apply_string(cx),
        })
        .detach();
        let paste_len = [profile.name.len(), profile.host.len()];
        let search = cx.new(|cx| TextInput::new(cx, "Search databases or paste a connection string").bare());
        cx.subscribe_in(&search, window, |d: &mut ConnDialog, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Submit => d.submit_search(window, cx),
            InputEvent::Changed => d.search_changed(window, cx),
        })
        .detach();
        let opts: Vec<String> = profile.options.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let options = text_field("authSource=admin&replicaSet=rs0", &opts.join("&"), PLAIN, window, cx);
        let file = text_field("~/data/app.sqlite", profile.path.as_deref().unwrap_or(""), PLAIN, window, cx);
        let file_access = profile.options.get(dataman_core::DUCKDB_EXTERNAL_ACCESS).is_none_or(|v| v != "false");
        let (tenant, aws_profile, aws_region, gcp_account) = match &profile.auth {
            Auth::EntraId { tenant } => (tenant.clone(), None, None, None),
            Auth::AwsIam { profile, region } => (None, profile.clone(), region.clone(), None),
            Auth::GoogleIam { account } => (None, None, None, account.clone()),
            Auth::Password => (None, None, None, None),
        };
        let tenant = text_field("az's default", tenant.as_deref().unwrap_or(""), IN_SIGN_IN, window, cx);
        let aws_profile = text_field("default", aws_profile.as_deref().unwrap_or(""), IN_SIGN_IN, window, cx);
        let aws_region = text_field("us-east-1", aws_region.as_deref().unwrap_or(""), IN_SIGN_IN, window, cx);
        let gcp_account = text_field("gcloud's active account", gcp_account.as_deref().unwrap_or(""), IN_SIGN_IN, window, cx);
        let step = if blank { Step::Database } else { Step::Connection };
        if step == Step::Database {
            window.focus(&search.focus_handle(cx), cx);
        } else {
            window.focus(&name.focus_handle(cx), cx);
        }
        let color = profile
            .color
            .as_deref()
            .and_then(|c| SWATCHES.iter().position(|s| *s == c))
            .unwrap_or_else(|| profile.id.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)) % 5);
        let profile_auth = profile.auth.kind();
        if !is_new && profile.save_password {
            let secrets = store.read(cx).core.secrets.clone();
            let (id, ssh_id) = (profile.id.clone(), profile.ssh_secret_id());
            cx.spawn(async move |this, cx| {
                let (pw, ssh) = cx.background_executor().spawn(async move { (secrets.get(&id), secrets.get(&ssh_id)) }).await;
                let there = |what: &str, r: anyhow::Result<Option<String>>| match r {
                    Ok(s) => s.is_some(),
                    Err(e) => {
                        log::warn!("looking for the saved {what}: {e:#}");
                        false
                    }
                };
                let (pw, ssh) = (there("password", pw), there("SSH secret", ssh));
                let _ = this.update(cx, |d, cx| {
                    d.saved_pw = pw;
                    if pw && !d.password_needed {
                        d.password.update(cx, |t, cx| t.set_placeholder(SAVED_IN, cx));
                    }
                    if ssh {
                        d.ssh.secret.update(cx, |t, cx| t.set_placeholder(SAVED_IN, cx));
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        let mut d = ConnDialog {
            store,
            focus: cx.focus_handle(),
            sslmode: if is_new && profile.sslmode == SslMode::Prefer {
                dataman_core::default_sslmode(profile.engine, if blank { "" } else { &profile.host }, profile.ssh.is_some())
            } else {
                profile.sslmode
            },
            ssl_chosen: !is_new || profile.sslmode != SslMode::Prefer,
            color,
            color_picked: profile.color_picked,
            env: profile.env,
            read_only: profile.read_only,
            mcp: profile.mcp,
            advanced,
            engine: profile.engine,
            engine_picked: !blank,
            profile,
            is_new,
            name,
            host,
            port,
            user,
            password,
            database,
            ssh,
            conn_str,
            str_bad: false,
            saved_pw: false,
            str_user: None,
            str_pw: None,
            auto_name: None,
            paste_len,
            test: Test::NotTested,
            test_gen: 0,
            password_needed: false,
            confirm_delete: false,
            step,
            seen: [true, !blank, !is_new, !is_new],
            return_to: None,
            search,
            search_len: 0,
            pick_hi: 0,
            options,
            file_access,
            file,
            create_file: false,
            saving: false,
            auth: profile_auth,
            auth_picked: !is_new || profile_auth.is_token(),
            tenant,
            aws_profile,
            aws_region,
            gcp_account,
            sign_in: std::array::from_fn(|_| SignIn::Unchecked),
            sign_in_tasks: std::array::from_fn(|_| None),
            unrolls: std::array::from_fn(|k| crate::motion::Unroll::rest(k == profile_auth as usize)),
            body_scroll: Default::default(),
            reveal: None,
            agents: None,
            agent_adds: std::array::from_fn(|_| AgentAdd::Idle),
            copied: Default::default(),
        };
        d.name_follows_host(cx);
        // Filled in: the string says what the fields say, with the TLS the host decided above.
        if is_new && !blank {
            d.rewrite_string(cx);
        }
        d
    }

    fn is_file(&self) -> bool {
        self.engine.is_file()
    }

    /// The file picked (or typed) as the dialog shows it: `~` for the home folder.
    pub fn set_file(&mut self, path: &std::path::Path, create: bool, cx: &mut Context<Self>) {
        let shown = dataman_core::config::tilde(&path.to_string_lossy());
        self.file.update(cx, |t, cx| t.set_text(shown, cx));
        self.create_file = create;
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let name = self.name.read(cx).text().trim().to_string();
        if name.is_empty() || self.auto_name.as_deref() == Some(name.as_str()) {
            self.name.update(cx, |t, cx| t.set_text(stem.clone(), cx));
            self.auto_name = Some(stem);
        }
        self.untest();
        cx.notify();
    }

    fn browse_file(&mut self, cx: &mut Context<Self>) {
        let rx = crate::sql_picker::pick_files(self.engine.file_extensions(), false, cx);
        cx.spawn(async move |this, cx| {
            let Ok(paths) = rx.await else { return };
            let Some(p) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |d, cx| d.set_file(&p, false, cx));
        })
        .detach();
    }

    fn new_file(&mut self, cx: &mut Context<Self>) {
        let exts = self.engine.file_extensions();
        let name = match self.name.read(cx).text().trim() {
            "" => format!("database.{}", exts[0]),
            n => format!("{n}.{}", exts[0]),
        };
        let rx = crate::sql_picker::pick_new_file(exts, &name, cx);
        cx.spawn(async move |this, cx| {
            let Ok(Some(p)) = rx.await else { return };
            let _ = this.update(cx, |d, cx| d.set_file(&p, true, cx));
        })
        .detach();
    }

    #[cfg(feature = "checks")]
    pub fn demo_asks_password(&self) -> bool {
        self.password_needed
    }

    #[cfg(feature = "checks")]
    pub fn step(&self) -> Step {
        self.step
    }

    /// A test running keeps going; any other verdict is about what was just changed.
    fn untest(&mut self) {
        if !matches!(self.test, Test::Testing { .. }) {
            self.test = Test::NotTested;
        }
    }

    fn field_changed(&mut self, feeds: Feeds, cx: &mut Context<Self>) {
        self.untest();
        if feeds.sign_in {
            self.forget_sign_ins();
        }
        if feeds.string {
            self.rewrite_string(cx);
        }
        cx.notify();
    }

    fn rewrite_string(&mut self, cx: &mut Context<Self>) {
        if self.is_file() {
            return;
        }
        self.str_bad = false;
        let text = if self.host.read(cx).text().trim().is_empty() { String::new() } else { self.url(cx) };
        self.conn_str.update(cx, |t, cx| t.set_text(text, cx));
    }

    /// Sign-in said where its password or token goes: with another host, it has to be seen again.
    fn host_moved(&mut self) {
        self.seen[Step::SignIn as usize] = false;
        self.seen[Step::Mcp as usize] = false;
    }

    /// An unnamed connection takes this name when saved: its placeholder says so.
    fn name_follows_host(&mut self, cx: &mut Context<Self>) {
        let host = self.host.read(cx).text().trim().to_string();
        let shown = if self.is_file() {
            "app"
        } else if host.is_empty() {
            "orders"
        } else {
            host_stem(&host)
        }
        .to_string();
        self.name.update(cx, |t, cx| t.set_placeholder(shown, cx));
    }

    pub fn pick_engine(&mut self, e: Engine, window: &mut Window, cx: &mut Context<Self>) {
        let same_kind = !self.engine_picked || self.engine.is_file() == e.is_file();
        let back = self.return_to.take();
        self.set_engine(e, cx);
        self.engine_picked = true;
        self.seen[Step::Connection as usize] = true;
        if !same_kind || e.is_file() {
            self.seen[Step::SignIn as usize] = false;
        }
        if !same_kind {
            self.seen[Step::Mcp as usize] = false;
        }
        self.search.update(cx, |t, cx| t.set_text("", cx));
        self.search_len = 0;
        self.pick_hi = 0;
        // Changing the database goes back to the step it was changed from.
        let holds = self.collect_as(true, cx).is_ok();
        let to = back.filter(|s| same_kind && holds && (*s != Step::SignIn || !e.is_file())).unwrap_or(Step::Connection);
        self.enter(to, window, cx);
    }

    /// Shows a step. Nothing here runs a program or waits on anything.
    fn enter(&mut self, to: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.return_to = (to == Step::Database && self.step != Step::Database).then_some(self.step);
        self.test_gen += 1;
        self.test = Test::NotTested;
        self.step = to;
        self.seen[to as usize] = true;
        self.body_scroll.set_offset(gpui::Point::default());
        match to {
            Step::Database => self.pick_hi = tiles_for(self.search.read(cx).text()).iter().position(|e| *e == self.engine).unwrap_or(0),
            Step::SignIn => {
                if !self.auth_picked {
                    let host = self.host.read(cx).text().trim().to_string();
                    self.set_auth(preselected(self.engine, &host), cx);
                }
                self.settle_methods();
            }
            Step::Mcp => self.find_agents(cx),
            Step::Connection => {}
        }
        // Once the click (or Return) that got here is done with the focus.
        cx.defer_in(window, |d, window, cx| {
            let target = match d.step {
                Step::Database => Some(d.search.focus_handle(cx)),
                Step::Connection => Some(d.name.focus_handle(cx)),
                Step::SignIn => d.method_inputs(d.auth, cx).first().map(|(_, _, i)| i.focus_handle(cx)),
                Step::Mcp => None,
            };
            window.focus(&target.unwrap_or_else(|| d.focus.clone()), cx);
        });
        cx.notify();
    }

    /// The methods as they are, with no motion: the open one is not animated in.
    fn settle_methods(&mut self) {
        for k in AuthKind::ALL {
            let open = k == self.auth && !self.sign_in[k as usize].missing();
            self.unrolls[k as usize].settle(open);
        }
    }

    fn submit_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search.read(cx).text().to_string();
        if self.string_from_search(&text, window, cx) {
            return;
        }
        let found = tiles_for(&text);
        if let Some(e) = found.get(self.pick_hi.min(found.len().saturating_sub(1))) {
            self.pick_engine(*e, window, cx);
        }
    }

    fn search_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search.read(cx).text().to_string();
        let pasted = text.len() > self.search_len + 3;
        self.search_len = text.len();
        self.pick_hi = 0;
        if !(pasted && self.string_from_search(&text, window, cx)) {
            cx.notify();
        }
    }

    /// A connection string in the search goes on to Connection with everything filled in. One that
    /// does not read stays in the search, with no message.
    fn string_from_search(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if connstr::resolved(text).is_none() {
            return false;
        }
        self.set_conn_str(text.trim(), cx);
        self.search.update(cx, |t, cx| t.set_text("", cx));
        self.search_len = 0;
        self.seen = [true, true, false, false];
        self.return_to = None;
        self.enter(Step::Connection, window, cx);
        true
    }

    fn move_pick(&mut self, step: isize, cx: &mut Context<Self>) {
        let n = tiles_for(self.search.read(cx).text()).len() as isize;
        if n > 0 {
            self.pick_hi = (self.pick_hi as isize + step).rem_euclid(n) as usize;
            cx.notify();
        }
    }

    #[cfg(feature = "checks")]
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
        self.reveal = Some(self.password.clone());
        self.password.update(cx, |p, cx| p.set_placeholder("required by server", cx));
        cx.notify();
    }

    /// A password handed over (a duplicated connection's) means it signs in with one.
    pub fn set_password(&mut self, pw: &str, cx: &mut Context<Self>) {
        self.password.update(cx, |p, cx| p.set_text(pw, cx));
        self.auth_picked = true;
    }

    #[cfg(feature = "checks")]
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
        if secret_needed && self.ssh.auth != SshAuth::Agent {
            self.ssh.secret.update(cx, |p, _| p.error = true);
            window.focus(&self.ssh.secret.focus_handle(cx), cx);
            self.reveal = Some(self.ssh.secret.clone());
        }
        cx.notify();
    }

    fn on_name_or_host(&mut self, which: usize, input: &Entity<TextInput>, cx: &mut Context<Self>) {
        let text = input.read(cx).text().to_string();
        let pasted = text.len() > self.paste_len[which] + 1;
        self.paste_len[which] = text.len();
        if !pasted || connstr::resolved(&text).is_none() {
            return;
        }
        if which == 0 {
            self.name.update(cx, |t, cx| t.set_text("", cx));
            self.paste_len[0] = 0;
        }
        self.set_conn_str(text.trim(), cx);
    }

    /// Across the network, said plainly when the password and data may travel in clear (a
    /// warning when they surely do).
    fn tls_note(&self, cx: &App) -> Option<(bool, &'static str)> {
        // Through a tunnel the network hop is SSH's: no warning.
        let remote = !self.ssh.on && !dataman_core::is_local_host(self.host.read(cx).text());
        match (remote, self.sslmode, self.engine.is_sql()) {
            (true, SslMode::Disable, _) => Some((true, "Not encrypted: data, and maybe the password, cross the network in clear.")),
            (true, SslMode::Prefer, true) => Some((false, "Encrypted only if the server offers it; the server is not verified.")),
            _ => None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_host(&mut self, host: &str, cx: &mut Context<Self>) {
        self.host.update(cx, |t, cx| t.set_text(host, cx));
        self.follow_host(cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_tls(&mut self, m: SslMode, cx: &mut Context<Self>) {
        self.sslmode = m;
        self.ssl_chosen = true;
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_tls_note(&self, cx: &App) -> Option<(bool, &'static str)> {
        self.tls_note(cx)
    }

    #[cfg(feature = "checks")]
    pub fn demo_ssh(&mut self, on: bool, cx: &mut Context<Self>) {
        self.ssh.on = on;
        self.follow_host(cx);
    }

    /// Until the user picks, TLS follows the host: opportunistic only on this machine or
    /// through a tunnel.
    fn follow_host(&mut self, cx: &mut Context<Self>) {
        if self.ssl_chosen {
            return;
        }
        let host = self.host.read(cx).text().to_string();
        let want = dataman_core::default_sslmode(self.engine, &host, self.ssh.on);
        if want != self.sslmode {
            self.sslmode = want;
            self.clamp_tls();
            cx.notify();
        }
    }

    /// A cloud token never goes without TLS: `disable` and `prefer` are not offered under one.
    fn clamp_tls(&mut self) {
        if self.token_auth() && matches!(self.sslmode, SslMode::Disable | SslMode::Prefer) {
            self.sslmode = SslMode::Require;
        }
    }

    /// A connection string, set as if typed: it fills every field it says, user and password too,
    /// and can change the database. What it does not say keeps its value, except a password that
    /// came from the string before.
    pub fn prefill_conn_str(&mut self, s: &str, cx: &mut Context<Self>) {
        for field in [&self.user, &self.database] {
            field.update(cx, |t, cx| t.set_text("", cx));
        }
        self.set_conn_str(s, cx);
    }

    pub fn set_conn_str(&mut self, s: &str, cx: &mut Context<Self>) {
        self.conn_str.update(cx, |i, cx| i.set_text(s, cx));
        self.apply_string(cx);
    }

    fn apply_string(&mut self, cx: &mut Context<Self>) {
        let text = self.conn_str.read(cx).text().trim().to_string();
        let Some(spec) = connstr::resolved(&text) else {
            self.str_bad = !text.is_empty();
            cx.notify();
            return;
        };
        self.str_bad = false;
        let said_user = spec.user.clone().filter(|u| !u.is_empty());
        let said_db = spec.database.clone().filter(|d| !d.is_empty());
        let (p, pw) = spec.into_profile();
        // An http(s) URL reads as Elasticsearch; on an OpenSearch sheet it is OpenSearch's.
        let engine = if p.engine == Engine::Elasticsearch && self.engine == Engine::OpenSearch { Engine::OpenSearch } else { p.engine };
        self.engine = engine;
        self.engine_picked = true;
        self.port.update(cx, |t, cx| t.set_placeholder(engine.default_port().to_string(), cx));
        self.database.update(cx, |t, cx| t.set_placeholder(engine.spec().form.database_placeholder, cx));
        self.conn_str.update(cx, |t, cx| t.set_placeholder(string_placeholder(engine), cx));
        if engine.is_file() {
            self.file.update(cx, |t, cx| t.set_text(p.path.clone().unwrap_or_default(), cx));
            self.read_only = p.read_only;
        } else {
            if self.host.read(cx).text().trim() != p.host {
                self.host_moved();
            }
            self.host.update(cx, |t, cx| t.set_text(p.host.clone(), cx));
            self.port.update(cx, |t, cx| t.set_text(p.port.to_string(), cx));
            if let Some(u) = &said_user {
                self.user.update(cx, |t, cx| t.set_text(u.clone(), cx));
            }
            if let Some(d) = &said_db {
                self.database.update(cx, |t, cx| t.set_text(d.clone(), cx));
            }
            if engine.spec().form.options {
                let opts: Vec<String> = p.options.iter().map(|(k, v)| format!("{k}={v}")).collect();
                self.options.update(cx, |t, cx| t.set_text(opts.join("&"), cx));
            }
        }
        if said_user.is_some() {
            self.str_user = said_user;
        }
        // A password in the string means password sign-in, whatever the host suggests.
        let kept = if engine.supports(self.auth) { self.auth } else { AuthKind::Password };
        match &pw {
            Some(pw) => {
                self.password.update(cx, |t, cx| t.set_text(pw.clone(), cx));
                self.str_pw = Some(pw.clone());
                self.set_auth(AuthKind::Password, cx);
                self.auth_picked = true;
            }
            None => {
                if self.password_from_string(cx) {
                    self.password.update(cx, |t, cx| t.set_text("", cx));
                    self.str_pw = None;
                    self.auth_picked = false;
                }
                self.set_auth(kept, cx);
            }
        }
        self.ssl_chosen = connstr::tls_given(&text);
        self.sslmode = if self.ssl_chosen { p.sslmode } else { dataman_core::default_sslmode(engine, &p.host, self.ssh.on) };
        self.clamp_tls();
        self.paste_len[1] = self.host.read(cx).text().len();
        self.name_follows_host(cx);
        self.forget_sign_ins();
        self.untest();
        cx.notify();
    }

    /// What the CLIs answered is about the fields as they were.
    fn forget_sign_ins(&mut self) {
        for s in &mut self.sign_in {
            if !s.busy() {
                *s = SignIn::Unchecked;
            }
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_conn_str(&self, cx: &App) -> String {
        self.conn_str.read(cx).text().to_string()
    }

    #[cfg(feature = "checks")]
    pub fn conn_str_invalid(&self, _cx: &App) -> bool {
        self.str_bad
    }

    #[cfg(feature = "checks")]
    pub fn focus_name(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.name.focus_handle(cx), cx);
    }

    #[cfg(feature = "checks")]
    pub fn current(&self, cx: &App) -> (ConnectionProfile, String) {
        (self.fields(cx), self.password.read(cx).text().to_string())
    }

    fn invalid(&mut self, msg: String, cx: &mut Context<Self>) {
        self.test = Test::Invalid(msg);
        cx.notify();
    }

    fn connection_inputs(&self) -> Vec<&Entity<TextInput>> {
        let mut v = vec![&self.name];
        if self.is_file() {
            v.push(&self.file);
            return v;
        }
        v.extend([&self.conn_str, &self.host, &self.port]);
        if self.has_database() {
            v.push(&self.database);
        }
        if self.engine.spec().form.options {
            v.push(&self.options);
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
        v
    }

    fn inputs<'a>(&'a self, cx: &'a App) -> Vec<&'a Entity<TextInput>> {
        let advanced = dataman_core::MCP_ADVANCED && self.advanced.open;
        let sign_in = || self.method_inputs(self.auth, cx).into_iter().map(|(_, _, i)| i).collect::<Vec<_>>();
        let mut v = if !self.is_new {
            let mut v = self.connection_inputs();
            if !self.is_file() {
                v.extend(sign_in());
            }
            v
        } else {
            match self.step {
                Step::Database => vec![&self.search],
                Step::Connection => self.connection_inputs(),
                Step::SignIn => sign_in(),
                Step::Mcp => Vec::new(),
            }
        };
        if advanced && (!self.is_new || self.step == Step::Mcp) {
            v.push(&self.advanced.input);
        }
        v
    }

    fn focus_field(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let v = self.inputs(cx);
        let n = v.len() as isize;
        if n == 0 {
            return;
        }
        let next = match v.iter().position(|i| i.focus_handle(cx).is_focused(window)) {
            Some(at) => (at as isize + step).rem_euclid(n),
            None if step > 0 => 0,
            None => n - 1,
        };
        let f = v[next as usize].clone();
        window.focus(&f.focus_handle(cx), cx);
        f.update(cx, |f, cx| f.select_all(cx));
        self.reveal = Some(f);
        cx.notify();
    }

    /// The body scrolled so a field that took the focus shows, with its label above it.
    fn reveal_field(&mut self, window: &mut Window, cx: &App) {
        let Some(input) = &self.reveal else { return };
        let Some(b) = input.read(cx).drawn_at() else {
            // Not laid out yet: the next frame is.
            return window.request_animation_frame();
        };
        let view = self.body_scroll.bounds();
        let mut at = self.body_scroll.offset();
        let label = px(24.);
        if b.top() - label < view.top() {
            at.y += view.top() - (b.top() - label);
        } else if b.bottom() + px(8.) > view.bottom() {
            at.y -= b.bottom() + px(8.) - view.bottom();
        }
        self.body_scroll.set_offset(at);
        self.reveal = None;
    }

    fn collect(&self, cx: &App) -> Result<(ConnectionProfile, Option<String>, Option<String>), String> {
        self.collect_as(false, cx)
    }

    /// `target_only`: what Connection answers for, before Sign-in has been asked.
    fn collect_as(&self, target_only: bool, cx: &App) -> Result<(ConnectionProfile, Option<String>, Option<String>), String> {
        // Production has its own red: a color picked before comes back when it is no longer.
        let picked = self.color_picked && !self.env.is_production();
        if self.is_file() {
            let mut p = self.fields(cx);
            (p.color, p.color_picked) = (Some(SWATCHES[self.color].to_string()), picked);
            let path = p.file_path();
            if p.path.as_deref().is_none_or(|s| s.trim().is_empty()) {
                return Err("file is required".into());
            }
            if !path.is_file() {
                if !self.create_file {
                    return Err(format!("{} does not exist (New… creates a database)", dataman_core::config::tilde(&path.to_string_lossy())));
                }
                if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
                    && !dir.is_dir()
                {
                    return Err(format!("the folder {} does not exist", dir.display()));
                }
            }
            if p.name.is_empty() {
                p.name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "database".into());
            }
            return Ok((p, None, None));
        }
        if self.str_bad {
            return Err("not a connection string".into());
        }
        let mut p = self.fields(cx);
        let port = self.port.read(cx).text().trim().to_string();
        p.port = if port.is_empty() { p.engine.default_port() } else { port.parse().map_err(|_| "port must be a number".to_string())? };
        (p.color, p.color_picked) = (Some(SWATCHES[self.color].to_string()), picked);
        if p.name.is_empty() {
            p.name = host_stem(&p.host).to_string();
        }
        if target_only {
            p.validate_target()?
        } else {
            p.validate()?
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
        let password = if p.auth.is_token() { None } else { text(&self.password) };
        Ok((p, password, secret))
    }

    /// Picked with New…: the file made, once what was typed is valid.
    fn create_picked_file(&self, p: &ConnectionProfile) -> Result<(), String> {
        let path = p.file_path();
        if !self.is_file() || !self.create_file || path.is_file() {
            return Ok(());
        }
        dataman_core::import::create_database(self.engine, &path).map_err(|e| format!("{e:#}"))
    }

    fn fields(&self, cx: &App) -> ConnectionProfile {
        let mut p = self.profile.clone();
        let text = |e: &Entity<TextInput>| e.read(cx).text().trim().to_string();
        if self.is_file() {
            let mut f = ConnectionProfile::new_file(self.engine, text(&self.file));
            f.id = p.id;
            f.name = text(&self.name);
            f.color = p.color;
            f.read_only_databases = p.read_only_databases;
            f.mcp_rules = p.mcp_rules;
            f.mcp_row_limit = p.mcp_row_limit;
            if self.engine == Engine::DuckDb && !self.file_access {
                f.options.insert(dataman_core::DUCKDB_EXTERNAL_ACCESS.into(), "false".into());
            }
            f.env = self.env;
            f.read_only = self.read_only;
            f.mcp = self.mcp;
            f.mcp_allowed = self.advanced.allowed.clone();
            f.mcp_yolo = self.advanced.yolo;
            f.save_password = false;
            return f;
        }
        p.name = text(&self.name);
        p.host = text(&self.host);
        p.user = text(&self.user);
        p.database = text(&self.database);
        p.engine = self.engine;
        p.port = match text(&self.port) {
            t if t.is_empty() => p.engine.default_port(),
            t => t.parse().unwrap_or(p.port),
        };
        // The core's rules, as the MCP tools take them: PostgreSQL's default database, no TLS
        // `prefer` for the document engines, no database where the engine has none.
        p.sslmode = self.sslmode;
        p.normalize();
        p.options = if p.engine.spec().form.options { ConnectionProfile::parse_options(&text(&self.options)) } else { Default::default() };
        // Every password goes to the Keychain; one left empty is asked for when connecting.
        p.save_password = true;
        p.auth = if self.engine.supports(self.auth) { self.auth_value(self.auth, cx) } else { Auth::Password };
        if p.auth.is_token() {
            // Nothing to save, and a token never travels without TLS.
            p.save_password = false;
            p.sslmode = p.tls_mode();
        }
        p.env = self.env;
        p.read_only = self.read_only;
        p.mcp = self.mcp;
        p.mcp_allowed = self.advanced.allowed.clone();
        p.mcp_yolo = self.advanced.yolo;
        // Saved here, the user has seen where it goes.
        p.agent_made = false;
        p.from_project = false;
        p.path = None;
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
        if !e.supports(self.auth) {
            self.set_auth(AuthKind::Password, cx);
            self.auth_picked = false;
        }
        let text = |i: &Entity<TextInput>, cx: &Context<Self>| i.read(cx).text().trim().to_string();
        // A file and a server have nothing in common: the other one's fields are cleared.
        if old.is_file() != e.is_file() {
            self.conn_str.update(cx, |t, cx| t.set_text("", cx));
            self.str_bad = false;
            if e.is_file() {
                for i in [&self.host, &self.port, &self.user, &self.database, &self.password] {
                    i.update(cx, |t, cx| t.set_text("", cx));
                }
                self.ssh.on = false;
            } else {
                self.file.update(cx, |t, cx| t.set_text("", cx));
                self.create_file = false;
            }
        } else if e.is_file() && !text(&self.file, cx).is_empty() {
            // SQLite and DuckDB files are not each other's.
            self.file.update(cx, |t, cx| t.set_text("", cx));
            self.create_file = false;
        }
        self.conn_str.update(cx, |t, cx| t.set_placeholder(string_placeholder(e), cx));
        self.name_follows_host(cx);
        self.forget_sign_ins();
        self.untest();
        if e.is_file() {
            let ph = format!("~/data/app.{}", e.file_extensions()[0]);
            self.file.update(cx, |t, cx| t.set_placeholder(ph, cx));
            cx.notify();
            return;
        }
        let port = text(&self.port, cx);
        if port.is_empty() || port == old.default_port().to_string() {
            self.port.update(cx, |t, cx| t.set_text(e.default_port().to_string(), cx));
        }
        self.port.update(cx, |t, cx| t.set_placeholder(e.default_port().to_string(), cx));
        let db = text(&self.database, cx);
        if !e.is_postgres() && db == "postgres" {
            self.database.update(cx, |t, cx| t.set_text("", cx));
        }
        if !self.ssl_chosen {
            self.sslmode = dataman_core::default_sslmode(e, &text(&self.host, cx), self.ssh.on);
        } else if !e.is_sql() && self.sslmode == SslMode::Prefer {
            self.sslmode = SslMode::Disable;
        } else if e.is_sql() && !old.is_sql() && self.sslmode == SslMode::Disable {
            self.sslmode = SslMode::Prefer;
        }
        self.clamp_tls();
        let ph = e.spec().form.database_placeholder;
        self.database.update(cx, |t, cx| t.set_placeholder(ph, cx));
        if !text(&self.host, cx).is_empty() {
            self.rewrite_string(cx);
        }
        cx.notify();
    }

    fn target(&self, cx: &App) -> String {
        let of = if self.is_file() { &self.file } else { &self.host };
        of.read(cx).text().trim().to_string()
    }

    /// Test ⌘T: on Sign-in, on a file's Connection, and in Edit.
    fn shows_test(&self) -> bool {
        !self.is_new || self.step == Step::SignIn || (self.step == Step::Connection && self.is_file())
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        if matches!(self.test, Test::Testing { .. }) {
            return;
        }
        let (p, pw, ssh_secret) = match self.collect(cx) {
            Ok(v) => v,
            Err(e) => return self.invalid(e, cx),
        };
        if let Err(e) = self.create_picked_file(&p) {
            return self.invalid(e, cx);
        }
        let core = self.store.read(cx).core.clone();
        let target = match &p.ssh {
            _ if p.engine.is_file() => dataman_core::config::tilde(p.path.as_deref().unwrap_or("")),
            Some(s) => format!("{}:{} via {}", p.host, p.port, s.host),
            None => format!("{}:{}", p.host, p.port),
        };
        self.test = Test::Testing { target };
        self.test_gen += 1;
        let gen_ = self.test_gen;
        // Saved secrets (keychain, pgpass) are looked up off the UI thread.
        let task = core.test_connection(p, pw, ssh_secret);
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
                            // What fixes a cloud sign-in shows where the sign-in is.
                            if cloudauth::sign_in_error(&e).is_some_and(|s| s.action.is_some()) && !d.sign_in[d.auth as usize].busy() {
                                d.sign_in[d.auth as usize] = SignIn::failed(&e);
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

    /// Continue can't go on while Connection has nowhere to go or a string that does not read.
    fn blocked(&self, cx: &App) -> bool {
        self.saving || (self.is_new && self.step == Step::Connection && (self.target(cx).is_empty() || self.str_bad))
    }

    fn primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_new || self.step == Step::Mcp {
            self.save(window, cx);
            return;
        }
        if self.step == Step::Database {
            return self.submit_search(window, cx);
        }
        if self.blocked(cx) {
            return;
        }
        if let Err(e) = self.collect_as(self.step == Step::Connection, cx) {
            return self.invalid(e, cx);
        }
        if let Some(next) = self.step.after(self.is_file()) {
            self.enter(next, window, cx);
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.saving {
            return false;
        }
        if let Err(e) = self.collect(cx) {
            if self.is_new {
                let to = if self.is_file() || self.collect_as(true, cx).is_err() { Step::Connection } else { Step::SignIn };
                if to != self.step {
                    self.enter(to, window, cx);
                }
            }
            self.invalid(e, cx);
            return false;
        }
        // YOLO that became production's in this sheet (tagged production after YOLO was on):
        // confirmed first, as turning it on there is.
        if self.yolo_needs_confirm() {
            self.advanced.open = true;
            self.advanced.confirm = true;
            cx.notify();
            return false;
        }
        cx.emit(DialogEvent::Save);
        true
    }

    /// Saves with no tier chosen, so the environment's default applies. One that can't save keeps
    /// the tier picked.
    fn skip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Skip goes back to what the sheet came with: an agent's may start with MCP off.
        let picked = std::mem::replace(&mut self.mcp, self.profile.mcp.filter(|_| self.profile.agent_made));
        if !self.save(window, cx) {
            self.mcp = picked;
        }
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

    #[cfg(feature = "checks")]
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

    #[cfg(feature = "checks")]
    pub fn demo_advanced_state(&self) -> (Vec<String>, bool, bool) {
        (self.advanced.allowed.clone(), self.advanced.yolo, self.advanced.confirm)
    }

    #[cfg(feature = "checks")]
    pub fn demo_engine(&mut self, e: Engine, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_engine(e, window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_step(&mut self, to: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.enter(to, window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_test(&mut self, cx: &mut Context<Self>) {
        self.test(cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_still(&self) -> bool {
        self.unrolls.iter().all(|u| u.is_settled())
    }

    #[cfg(feature = "checks")]
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

    #[cfg(feature = "checks")]
    pub fn demo_method(&self, cx: &App) -> (AuthKind, &'static str, bool, String) {
        let line = Cli::of(self.auth).and_then(|c| self.sign_in[self.auth as usize].line(c.program())).map_or(String::new(), |(_, l)| l);
        (self.auth, method_label(AuthKind::Password, self.user.read(cx).text(), self.password_there(cx)), self.from_string_note(cx), line)
    }

    #[cfg(feature = "checks")]
    pub fn demo_signed_in(&mut self, who: &str, cx: &mut Context<Self>) {
        self.sign_in[self.auth as usize] = SignIn::Ok { who: Some(who.into()) };
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_pick_auth(&mut self, k: AuthKind, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_auth(k, window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_env(&mut self, e: Environment, cx: &mut Context<Self>) {
        self.env = e;
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_installed(&self, id: &str) -> Option<bool> {
        self.agents.as_ref()?.rows.iter().find(|r| r.agent.id == id).map(|r| r.installed)
    }

    #[cfg(feature = "checks")]
    pub fn demo_copy(&self, id: &str) -> Option<String> {
        self.agents.as_ref()?.rows.iter().find(|r| r.agent.id == id).map(|r| r.copy.clone())
    }

    #[cfg(feature = "checks")]
    pub fn demo_tier(&self) -> McpAccess {
        self.tier()
    }

    #[cfg(feature = "checks")]
    pub fn demo_color(&self) -> (usize, bool) {
        (self.color, self.color_picked)
    }

    #[cfg(feature = "checks")]
    pub fn demo_agent_add(&self, id: &str) -> Option<String> {
        let i = AGENTS.iter().position(|a| a.id == id)?;
        Some(match &self.agent_adds[i] {
            AgentAdd::Idle => "idle".to_string(),
            AgentAdd::Adding => "adding".to_string(),
            AgentAdd::Done => "done".to_string(),
            AgentAdd::Failed(e) => format!("failed: {e}"),
        })
    }

    #[cfg(feature = "checks")]
    pub fn demo_tab_on_region(&self, window: &Window, cx: &App) -> bool {
        self.aws_region.focus_handle(cx).is_focused(window)
    }

    #[cfg(feature = "checks")]
    pub fn demo_focused(&self, window: &Window, cx: &App) -> Option<usize> {
        [&self.name, &self.host, &self.port, &self.user, &self.password, &self.database].iter().position(|f| f.focus_handle(cx).is_focused(window))
    }

    #[cfg(feature = "checks")]
    pub fn demo_field(&self, ix: usize, cx: &App) -> String {
        [&self.name, &self.host, &self.port, &self.user, &self.password, &self.database][ix].read(cx).text().to_string()
    }

    #[cfg(feature = "checks")]
    pub fn demo_input(&self, ix: usize) -> Entity<TextInput> {
        [&self.name, &self.host, &self.port, &self.user, &self.password, &self.database][ix].clone()
    }

    #[cfg(feature = "checks")]
    pub fn demo_set_field(&self, ix: usize, s: &str, cx: &mut Context<Self>) {
        [&self.name, &self.host, &self.port, &self.user, &self.password, &self.database][ix].clone().update(cx, |t, cx| t.set_text(s, cx));
    }

    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let (mut p, pw, ssh_secret) = match self.collect(cx) {
            Ok(v) => v,
            Err(e) => return self.invalid(e, cx),
        };
        if let Err(e) = self.create_picked_file(&p) {
            return self.invalid(e, cx);
        }
        let core = self.store.read(cx).core.clone();
        // Its agent login is Permissions' own: as saved now (made, rotated or removed
        // while this dialog was open), never this dialog's copy, nor a copied connection's.
        (p.agent_login, p.agents_refused) = core.config.get(&p.id).map(|c| (c.agent_login, c.agents_refused)).unwrap_or((None, false));
        // The keychain may be slow, or ask to be unlocked: saved off the UI thread.
        self.saving = true;
        cx.notify();
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let (saved, typed) = (p.clone(), pw.clone());
            let r = cx.background_executor().spawn(async move { core.save_connection(saved, typed.as_deref(), ssh_secret.as_deref()) }).await;
            // Saved: the list follows even if the dialog is gone by now.
            if r.is_ok() {
                store.update(cx, |s, cx| {
                    s.disconnect(&p.id, cx);
                    s.reload(cx);
                });
            }
            let _ = this.update(cx, |d, cx| {
                d.saving = false;
                if let Err(e) = r {
                    d.test = Test::Invalid(format!("{e:#}"));
                    cx.notify();
                    return;
                }
                cx.emit(DialogEvent::Saved { id: p.id, password: pw });
            });
        })
        .detach();
    }

    /// Save pressed and the write not done: the dialog stays until it is.
    pub fn is_saving(&self) -> bool {
        self.saving
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
        self.engine.spec().form.database
    }

    fn token_auth(&self) -> bool {
        self.auth.is_token() && self.engine.supports(self.auth)
    }

    fn tier(&self) -> McpAccess {
        self.mcp.unwrap_or_else(|| McpAccess::default_for(self.env))
    }

    fn password_there(&self, cx: &App) -> bool {
        !self.password.read(cx).text().is_empty() || self.saved_pw
    }

    /// Still what the last string said: another string with none must not send it to its host.
    fn password_from_string(&self, cx: &App) -> bool {
        self.str_pw.as_deref().is_some_and(|s| s == self.password.read(cx).text())
    }

    /// Both came from the string, and neither was typed over.
    #[allow(clippy::wrong_self_convention)] // "From string": asks about the fields, converts nothing.
    fn from_string_note(&self, cx: &App) -> bool {
        let same = |said: &Option<String>, i: &Entity<TextInput>| said.as_deref().is_some_and(|s| s == i.read(cx).text().trim());
        self.auth == AuthKind::Password && same(&self.str_user, &self.user) && same(&self.str_pw, &self.password)
    }

    fn method_inputs(&self, kind: AuthKind, cx: &App) -> Vec<(&'static str, &'static str, &Entity<TextInput>)> {
        let (_, user_label) = user_hint(self.engine, kind);
        let mut v = Vec::with_capacity(3);
        // SQL Server's Entra token names its user itself.
        if !(kind == AuthKind::EntraId && self.engine == Engine::SqlServer) {
            v.push((if kind == AuthKind::Password { user_label } else { "USER" }, user_label, &self.user));
        }
        match kind {
            AuthKind::Password => v.push(("PASSWORD", "PASSWORD", &self.password)),
            AuthKind::EntraId => v.push(("TENANT", "TENANT (OPTIONAL)", &self.tenant)),
            AuthKind::AwsIam => v.push(("AWS PROFILE", "AWS PROFILE (OPTIONAL)", &self.aws_profile)),
            AuthKind::GoogleIam => v.push(("ACCOUNT", "ACCOUNT (OPTIONAL)", &self.gcp_account)),
        }
        if kind == AuthKind::AwsIam && self.asks_region(cx) {
            v.push(("REGION", "REGION", &self.aws_region));
        }
        v
    }

    fn auth_value(&self, kind: AuthKind, cx: &App) -> Auth {
        let text = |e: &Entity<TextInput>| Some(e.read(cx).text().trim().to_string()).filter(|s| !s.is_empty());
        match kind {
            AuthKind::Password => Auth::Password,
            AuthKind::EntraId => Auth::EntraId { tenant: text(&self.tenant) },
            AuthKind::AwsIam => Auth::AwsIam { profile: text(&self.aws_profile), region: if self.asks_region(cx) { text(&self.aws_region) } else { None } },
            AuthKind::GoogleIam => Auth::GoogleIam { account: text(&self.gcp_account) },
        }
    }

    /// An RDS endpoint says its region: a value from before, no longer shown, must not override it.
    fn asks_region(&self, cx: &App) -> bool {
        let host = self.host.read(cx).text().trim();
        !host.is_empty() && cloudauth::aws_region_of(host).is_none()
    }

    /// The connection as `kind` signs in, whichever method is picked by now.
    fn signing_in_as(&self, kind: AuthKind, cx: &App) -> ConnectionProfile {
        let mut p = self.fields(cx);
        p.auth = self.auth_value(kind, cx);
        p.save_password = false;
        p.sslmode = p.tls_mode();
        p
    }

    pub fn set_auth(&mut self, kind: AuthKind, cx: &mut Context<Self>) {
        if kind != self.auth {
            self.auth = kind;
            self.untest();
        }
        if kind.is_token() && self.engine.supports(kind) && matches!(self.sslmode, SslMode::Disable | SslMode::Prefer) {
            // Azure's certificates are publicly trusted: the server is checked before the token
            // goes. RDS and Cloud SQL sign theirs with their own CAs.
            self.sslmode = if kind == AuthKind::EntraId { SslMode::VerifyFull } else { SslMode::Require };
        }
        let (ph, _) = user_hint(self.engine, kind);
        self.user.update(cx, |t, cx| t.set_placeholder(ph, cx));
        cx.notify();
    }

    /// A row of the list clicked. On a method whose CLI was not found, it looks again next time.
    fn pick_auth(&mut self, kind: AuthKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.sign_in[kind as usize].missing() {
            self.sign_in[kind as usize] = SignIn::Unchecked;
        }
        if kind != self.auth {
            self.test_gen += 1;
            self.test = Test::NotTested;
        }
        self.auth_picked = true;
        self.set_auth(kind, cx);
        if let Some((_, _, first)) = self.method_inputs(kind, cx).first() {
            window.focus(&first.focus_handle(cx), cx);
        }
    }

    /// The method's one button. It asks for a token; with none, the CLI's own login.
    fn sign_in_clicked(&mut self, kind: AuthKind, cx: &mut Context<Self>) {
        let k = kind as usize;
        match &self.sign_in[k] {
            SignIn::SigningIn => {
                // Dropped, its CLI is stopped.
                self.sign_in_tasks[k] = None;
                self.sign_in[k] =
                    SignIn::Failed { title: Cli::of(kind).map_or(String::new(), Cli::signed_out), body: String::new(), fix: Cli::of(kind).map(Fix::SignIn) };
                cx.notify();
            }
            SignIn::Checking => {}
            SignIn::Failed { fix: Some(Fix::SignIn(_)), .. } => self.run_login(kind, cx),
            _ => self.run_check(kind, true, cx),
        }
    }

    /// Asks the cloud CLI for a token now, and who it signs in as. `then_login`: a CLI with no
    /// session goes on to its own login.
    fn run_check(&mut self, kind: AuthKind, then_login: bool, cx: &mut Context<Self>) {
        let k = kind as usize;
        let task = self.store.read(cx).core.cloud_sign_in(self.signing_in_as(kind, cx)).abort_on_drop();
        self.sign_in[k] = SignIn::Checking;
        self.sign_in_tasks[k] = Some(cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |d, cx| {
                match r {
                    Ok(s) => d.sign_in[k] = SignIn::Ok { who: s.who },
                    Err(e) => {
                        let lapsed = matches!(cloudauth::sign_in_error(&e).and_then(|s| s.action), Some(Fix::SignIn(_)));
                        d.sign_in[k] = SignIn::failed(&e);
                        if lapsed && then_login && d.auth == kind {
                            d.run_login(kind, cx);
                        }
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The cloud CLI's own sign-in (`az login`, `aws login`, `gcloud auth login`), which opens the
    /// browser; checked again once it is done.
    fn run_login(&mut self, kind: AuthKind, cx: &mut Context<Self>) {
        let k = kind as usize;
        let task = self.store.read(cx).core.cloud_login(self.signing_in_as(kind, cx)).abort_on_drop();
        self.sign_in[k] = SignIn::SigningIn;
        self.sign_in_tasks[k] = Some(cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |d, cx| {
                match r {
                    Ok(()) => d.run_check(kind, false, cx),
                    Err(e) => d.sign_in[k] = SignIn::failed(&e),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// A saved connection that could not sign in: why, and what fixes it, where the sign-in is.
    pub fn set_sign_in_failed(&mut self, e: &SignInError, cx: &mut Context<Self>) {
        self.sign_in[self.auth as usize] = SignIn::Failed { title: e.title.clone(), body: e.body.clone(), fix: e.action };
        cx.notify();
    }

    /// Looks for the agents the first time MCP shows. Only looks: nothing runs until Add.
    fn find_agents(&mut self, cx: &mut Context<Self>) {
        if self.agents.is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async { Listing::here() }).await;
            let _ = this.update(cx, |d, cx| {
                d.agents = Some(found);
                cx.notify();
            });
        })
        .detach();
    }

    fn add_agent(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(listing) = self.agents.clone() else { return };
        if self.agent_adds[i] == AgentAdd::Adding {
            return;
        }
        self.agent_adds[i] = AgentAdd::Adding;
        let job = self.store.read(cx).core.handle().spawn(async move { agents::add(&AGENTS[i], &listing).await });
        cx.spawn(async move |this, cx| {
            let r = job.await;
            let _ = this.update(cx, |d, cx| {
                d.agent_adds[i] = match r.map_err(|e| e.to_string()).and_then(|r| r) {
                    Ok(Added::Done) => AgentAdd::Done,
                    Ok(Added::Open(link)) => {
                        cx.open_url(&link);
                        AgentAdd::Done
                    }
                    Err(e) => {
                        log::warn!("adding Dataman to {}: {e}", AGENTS[i].label);
                        AgentAdd::Failed(e)
                    }
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

fn string_placeholder(e: Engine) -> String {
    format!("{}://user@host:{}/db", e.spec().form.scheme, e.default_port())
}

fn panel() -> gpui::Div {
    div().flex_none().px(px(20.)).py(px(18.)).flex().flex_col().gap(px(14.))
}

fn caption(t: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div().flex_none().text_size(px(crate::theme::text::TINY)).text_color(t.tx3).whitespace_nowrap().child(text.into())
}

fn row() -> gpui::Div {
    div().flex().flex_row().gap(px(12.))
}

fn line(color: Hsla, text: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(crate::theme::text::UI)).line_height(px(17.)).text_color(color).child(text.into())
}

impl ConnDialog {
    fn field(&self, t: &Theme, label: &'static str, input: &Entity<TextInput>, label_color: Option<Hsla>) -> gpui::Div {
        self.field_as(t, label, label, input, label_color)
    }

    fn field_as(&self, t: &Theme, probe: &str, label: impl Into<gpui::SharedString>, input: &Entity<TextInput>, label_color: Option<Hsla>) -> gpui::Div {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(div().text_size(px(crate::theme::text::TINY)).text_color(label_color.unwrap_or(t.tx3)).child(label.into()))
            .child(ui::probe(format!("field-{probe}"), input.clone()))
    }

    fn toggle(&self, t: &Theme, id: &'static str, on: bool, label: &'static str) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .h(px(28.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .text_size(px(crate::theme::text::UI))
            .text_color(t.tx2)
            .whitespace_nowrap()
            .cursor_pointer()
            .child(ui::switch_track(t, format!("{id}-switch"), on))
            .child(label)
    }

    fn render_database(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let text = self.search.read(cx).text();
        let found = tiles_for(text);
        let hi = found.get(self.pick_hi.min(found.len().saturating_sub(1))).copied();
        let nothing = found.is_empty() && !text.contains("://");
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
                .child(div().text_size(px(crate::theme::text::BODY)).text_color(t.tx).child(e.label()))
                .child(div().text_size(px(crate::theme::text::HINT)).text_color(t.tx3).child(e.family_label()))
                .on_click(cx.listener(move |d, _, window, cx| d.pick_engine(e, window, cx)))
        };
        let tile = |e: Engine| ui::probe(format!("engine-{}", e.label()), tile(e));
        let section = |label: &'static str, engines: Vec<Engine>| {
            (!engines.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(caption(t, label))
                    .child(div().flex().flex_row().flex_wrap().gap(px(10.)).children(engines.into_iter().map(tile)))
            })
        };
        let (sql, other): (Vec<Engine>, Vec<Engine>) = found.iter().partition(|e| e.is_sql());
        panel()
            .key_context("EnginePicker")
            .on_action(cx.listener(|d, _: &PickNext, _, cx| d.move_pick(1, cx)))
            .on_action(cx.listener(|d, _: &PickPrev, _, cx| d.move_pick(-1, cx)))
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
            .when(nothing, |d| {
                d.child(
                    div()
                        .h(px(96.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(crate::theme::text::BODY))
                        .text_color(t.tx3)
                        .child("No database by that name"),
                )
            })
    }

    fn swatches(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        // Production has its own red: no color to pick. Leaving it brings the color back.
        let off = self.env.is_production();
        let (edge, color) = (t.tx, self.color);
        div().h(px(30.)).flex().flex_row().items_center().gap(px(2.)).children((0..SWATCHES.len()).map(|i| {
            let on = i == color && !off;
            let swatch = div()
                .id(("swatch", i))
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .when(on, |d| d.border_1().border_color(edge))
                .child(div().size(px(18.)).bg(t.swatches[i]).when(off, |d| d.opacity(0.25)))
                .when(!off, |d| {
                    d.cursor_pointer().on_click(cx.listener(move |d, _, _, cx| {
                        (d.color, d.color_picked) = (i, true);
                        cx.notify();
                    }))
                });
            ui::probe(format!("swatch-{i}"), swatch)
        }))
    }

    fn connection_panel(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let file_mode = self.is_file();
        let top = row()
            .items_end()
            .child(self.field(t, "NAME", &self.name, None))
            .child(div().flex_none().flex().flex_col().gap(px(6.)).child(caption(t, "COLOR")).child(self.swatches(t, cx)));
        let env = ui::segmented(t).children(ENVIRONMENTS.map(|(e, label)| {
            ui::probe(
                format!("env-{}", e.label()),
                ui::segment(t, ("env", e as usize), label, e == self.env).when(e.is_production() && e == self.env, |d| d.text_color(t.err)).on_click(
                    cx.listener(move |d, _, _, cx| {
                        d.env = e;
                        cx.notify();
                    }),
                ),
            )
        }));
        let read_only = self.toggle(t, "read-only", self.read_only, "Read-only").on_click(cx.listener(|d, _, _, cx| {
            d.read_only = !d.read_only;
            cx.notify();
        }));
        let ssh = self.toggle(t, "ssh-toggle", self.ssh.on, "SSH tunnel").on_click(cx.listener(|d, _, window, cx| {
            d.ssh.on = !d.ssh.on;
            d.follow_host(cx);
            if d.ssh.on {
                window.focus(&d.ssh.host.focus_handle(cx), cx);
                d.body_scroll.scroll_to_bottom();
            }
            d.untest();
            cx.notify();
        }));
        let context = div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(16.))
            .child(div().flex().flex_col().gap(px(6.)).child(caption(t, "ENVIRONMENT")).child(env))
            .child(ui::probe("read-only", read_only))
            .when(!file_mode, |d| d.child(ui::probe("ssh-toggle", ssh)));
        let panel = panel().child(top);
        if file_mode {
            let file_button = |id: &'static str, label: &'static str| ui::button(t, id, Btn::Secondary).h(px(30.)).px(px(10.)).child(label);
            let access = self
                .toggle(t, "file-access", self.file_access, "Allow SQL to read and write files")
                .tip(ui::text_tooltip(
                    "read_csv, read_parquet, COPY … TO, ATTACH, EXPORT. Off, the database is all SQL can reach (agents never get files either way).",
                ))
                .on_click(cx.listener(|d, _, _, cx| {
                    d.file_access = !d.file_access;
                    d.untest();
                    cx.notify();
                }));
            return panel
                .child(
                    row()
                        .items_end()
                        .child(self.field(t, "FILE", &self.file, None))
                        .child(ui::probe("file-browse", file_button("file-browse", "Browse…").on_click(cx.listener(|d, _, _, cx| d.browse_file(cx)))))
                        .child(ui::probe("file-new", file_button("file-new", "New…").on_click(cx.listener(|d, _, _, cx| d.new_file(cx))))),
                )
                .when(self.engine == Engine::DuckDb, |d| d.child(ui::probe("file-access", access)))
                .child(context);
        }
        let form = self.engine.spec().form;
        let db_label: SharedString = if self.engine.spec().numbered_dbs.is_some() {
            form.database_label.to_uppercase().into()
        } else {
            format!("{} (OPTIONAL)", form.database_label.to_uppercase()).into()
        };
        let modes: &[(SslMode, &str, &str)] = if self.engine.is_sql() { &SQL_TLS } else { &DOC_TLS };
        // A cloud token never goes without TLS.
        let token = self.token_auth();
        let ssl = ui::segmented(t).h(px(30.)).flex_none().children(
            modes.iter().filter(|(m, ..)| !token || matches!(m, SslMode::Require | SslMode::VerifyFull)).map(|(m, key, label)| {
                let m = *m;
                ui::probe(
                    format!("tls-{key}"),
                    ui::segment(t, m.label(), *label, m == self.sslmode).whitespace_nowrap().on_click(cx.listener(move |d, _, _, cx| {
                        d.sslmode = m;
                        d.ssl_chosen = true;
                        d.untest();
                        cx.notify();
                    })),
                )
            }),
        );
        let tls_note = self.tls_note(cx);
        panel
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(self.field_as(t, "CONNECTION STRING", "CONNECTION STRING", &self.conn_str, None).flex_none())
                    .when(self.str_bad, |d| d.child(ui::probe("string-bad", line(t.tx, "Not a connection string")))),
            )
            .child(row().child(self.field(t, "HOST", &self.host, None)).child(div().w(px(96.)).flex_none().child(self.field(t, "PORT", &self.port, None))))
            .child(
                row()
                    .items_end()
                    .when(form.database, |r| r.child(self.field_as(t, form.database_label.to_uppercase().as_str(), db_label, &self.database, None)))
                    .child(div().flex().flex_col().gap(px(6.)).child(caption(t, if self.engine.is_sql() { "SSL" } else { "TLS" })).child(ssl)),
            )
            .children(tls_note.map(|(warn, note)| ui::probe("tls-note", line(if warn { t.tx } else { t.tx3 }, note).mt(px(-6.)))))
            .when(form.options, |d| d.child(self.field(t, "OPTIONS", &self.options, None)))
            .child(context)
            .when(self.ssh.on, |d| d.child(self.ssh_fields(t, cx)))
    }

    fn ssh_fields(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let s = &self.ssh;
        let auth = ui::segmented(t).h(px(30.)).flex_none().children(SshAuth::ALL.iter().map(|a| {
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
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(row().child(self.field(t, "SSH HOST", &s.host, None)).child(div().w(px(96.)).flex_none().child(self.field(t, "SSH PORT", &s.port, None))))
            .child(
                row()
                    .items_end()
                    .child(self.field(t, "SSH USER", &s.user, None))
                    .child(div().flex().flex_col().gap(px(6.)).child(caption(t, "AUTH")).child(auth)),
            )
            .when(s.auth == SshAuth::Key, |d| d.child(row().child(self.field(t, "KEY FILE", &s.key, None)).child(self.field(t, "PASSPHRASE", &s.secret, None))))
            .when(s.auth == SshAuth::Password, |d| d.child(self.field(t, "SSH PASSWORD", &s.secret, None)))
    }

    fn method_row(&self, kind: AuthKind, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let k = kind as usize;
        let on = kind == self.auth;
        let hov = t.hov;
        let cli = Cli::of(kind).map_or("", Cli::program);
        let label = method_label(kind, self.user.read(cx).text(), self.password_there(cx));
        let status = self.sign_in[k].line(cli).map(|(glyph, text)| {
            let signed_in = matches!(self.sign_in[k], SignIn::Ok { .. });
            let failed = if let SignIn::Failed { title, body, .. } = &self.sign_in[k] {
                Some(if body.is_empty() { title.clone() } else { format!("{title}\n{body}") })
            } else {
                None
            };
            div()
                .id(("auth-status", k))
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .justify_end()
                .items_center()
                .gap(px(6.))
                .text_size(px(crate::theme::text::UI))
                .child(div().flex_none().text_size(px(crate::theme::text::TINY)).text_color(if signed_in { t.ok } else { t.tx3 }).child(glyph))
                .child(
                    div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if signed_in { t.tx } else { t.tx3 }).child(text.clone()),
                )
                .map(|d| match failed {
                    Some(full) => d.tip(ui::error_tip(full)),
                    None => d.tip(ui::text_tooltip(text)),
                })
        });
        let row = ui::selectable(div().id(("auth-row", k)).h(px(38.)).px(px(8.)).flex().flex_row().items_center().gap(px(12.)).cursor_pointer(), t, on, true)
            .when(!on, |d| d.opacity(0.5).hover(move |s| s.opacity(1.).bg(hov)))
            .child(div().flex_none().text_size(px(crate::theme::text::BODY)).whitespace_nowrap().child(label))
            .children(status.map(|s| ui::probe(format!("auth-status-{}", kind.slug()), s)))
            .on_click(cx.listener(move |d, _, window, cx| d.pick_auth(kind, window, cx)));
        ui::probe(format!("auth-{}", kind.slug()), row)
    }

    fn method_body(&self, kind: AuthKind, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let fields = self.method_inputs(kind, cx);
        let rows: Vec<gpui::Div> = fields
            .chunks(2)
            .map(|pair| {
                pair.iter().fold(row(), |r, (probe, label, input)| {
                    let color = (*probe == "PASSWORD" && self.password_needed).then_some(t.acc_t);
                    r.child(self.field_as(t, probe, *label, input, color))
                })
            })
            .collect();
        let host = self.host.read(cx).text().trim();
        let foreign = kind.is_token() && !host.is_empty() && !cloudauth::host_allowed(kind, host);
        let action = match (&self.sign_in[kind as usize], kind.is_token()) {
            (_, false) | (SignIn::Ok { .. } | SignIn::Checking, _) => None,
            (SignIn::SigningIn, _) => Some("Cancel"),
            _ => Some("Sign in"),
        };
        let off = action == Some("Sign in") && kind == AuthKind::AwsIam && self.user.read(cx).text().trim().is_empty();
        let button = action.map(|a| {
            let b = if off {
                ui::button_off(t, "auth-action", Btn::Secondary)
            } else {
                ui::button(t, "auth-action", Btn::Secondary).on_click(cx.listener(move |d, _, _, cx| d.sign_in_clicked(kind, cx)))
            };
            div().flex().flex_row().child(ui::probe(format!("auth-action-{}", kind.slug()), b.child(a)))
        });
        div()
            .px(px(8.))
            .py(px(12.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(rows)
            .when(kind == AuthKind::Password && self.from_string_note(cx), |d| d.child(ui::probe("from-string", line(t.tx3, "* From your connection string"))))
            .when(foreign, |d| d.child(ui::probe("foreign-host", line(t.tx, foreign_host_note(kind)))))
            .children(button)
    }

    fn sign_in_panel(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let kinds = self.engine.auth_kinds();
        let list = kinds.len() > 1;
        let reduced = crate::motion::still(cx);
        let now = Instant::now();
        let mut col = div().mx(px(-8.)).flex().flex_col().gap(px(2.));
        for kind in kinds.iter().copied() {
            let k = kind as usize;
            let open = kind == self.auth && !self.sign_in[k].missing();
            let rolled = self.unrolls[k].frame(open, reduced, now);
            if matches!(rolled, crate::motion::Rolled::At(_)) {
                window.request_animation_frame();
            }
            let natural = self.unrolls[k].natural.clone();
            let head = list.then(|| self.method_row(kind, t, cx));
            let body =
                (rolled != crate::motion::Rolled::Shut).then(|| self.method_body(kind, t, cx)).and_then(|b| ui::unrolled(("auth-body", k), rolled, natural, b));
            col = col.child(div().flex().flex_col().children(head).children(body));
        }
        panel().gap(px(0.)).child(col)
    }

    fn tiers(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        const COLUMNS: [(&str, f32); 3] = [("READS", 84.), ("WRITES", 84.), ("DESTRUCTIVE", 100.)];
        let production = self.env.is_production();
        let tier = self.tier();
        let hov = t.hov;
        let head = div()
            .h(px(20.))
            .px(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .child(div().flex_1().child(caption(t, "TIER")))
            .children(COLUMNS.map(|(label, w)| div().flex_none().w(px(w)).child(caption(t, label))));
        let rows = TIERS.map(|(a, label, cans)| {
            let on = a == tier;
            let cells = std::array::from_fn::<_, 3, _>(|i| {
                let (text, color) = match tier_cell(i, cans[i], production, self.read_only) {
                    Can::Yes => ("●", t.tx),
                    Can::Asks => ("Asks", t.tx2),
                    Can::No => ("–", t.tx4),
                };
                div().flex_none().w(px(COLUMNS[i].1)).text_size(px(crate::theme::text::UI)).text_color(color).child(text)
            });
            let row = ui::selectable(div().id(("mcp", a as usize)).h(px(38.)).px(px(8.)).flex().flex_row().items_center().cursor_pointer(), t, on, true)
                .when(!on, |d| d.hover(move |s| s.bg(hov)))
                .child(
                    div().flex_1().text_size(px(crate::theme::text::BODY)).when(on && production && cans[1] != Can::No, |d| d.text_color(t.err)).child(label),
                )
                .children(cells)
                .on_click(cx.listener(move |d, _, _, cx| {
                    d.mcp = Some(a);
                    cx.notify();
                }));
            ui::probe(format!("mcp-{}", a.label()), row)
        });
        div().mx(px(-8.)).flex().flex_col().gap(px(2.)).child(head).children(rows)
    }

    fn agent_rows(&self, t: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let listing = self.agents.as_ref()?;
        let rows = listing.rows.iter().map(|r| {
            let (i, a) = (r.index, r.agent);
            let state = &self.agent_adds[i];
            let logo = if a.mono {
                crate::assets::agent_logo_mono(a.logo).text_color(t.tx).into_any_element()
            } else {
                gpui::img(a.logo).flex_none().size(px(16.)).into_any_element()
            };
            let status = match state {
                AgentAdd::Done => Some((if a.waits { "◌" } else { "●" }, if a.waits { t.tx3 } else { t.ok }, t.tx, a.done, a.done.to_string())),
                AgentAdd::Failed(why) => Some(("✕", t.tx3, t.tx3, "Could not add", why.clone())),
                _ => None,
            };
            let status = status.map(|(glyph, gc, tc, text, tip)| {
                div()
                    .id(("agent-status", i))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(crate::theme::text::UI))
                    .child(div().text_size(px(crate::theme::text::TINY)).text_color(gc).child(glyph))
                    .child(div().text_color(tc).child(text))
                    .map(|d| if matches!(state, AgentAdd::Failed(_)) { d.tip(ui::error_tip(tip)) } else { d.tip(ui::text_tooltip(tip)) })
            });
            let add = (r.installed && *state != AgentAdd::Done).then(|| {
                let id = format!("agent-add-{}", a.id);
                let b = if *state == AgentAdd::Adding {
                    ui::button_off(t, SharedString::from(id.clone()), Btn::Secondary).child("Adding…")
                } else {
                    ui::button(t, SharedString::from(id.clone()), Btn::Secondary)
                        .child("Add")
                        .tip(ui::text_tooltip(a.add_hint))
                        .on_click(cx.listener(move |d, _, _, cx| d.add_agent(i, cx)))
                };
                ui::probe(id, b.h(px(24.)))
            });
            let copy_id = format!("agent-copy-{}", a.id);
            let text = r.copy.clone();
            let copy = ui::copy_button(t, SharedString::from(copy_id.clone()), Btn::Ghost, "Copy", self.copied.on(&copy_id, cx))
                .h(px(24.))
                .tip(ui::text_tooltip(format!("{}: {}", a.copy_hint, r.copy)))
                .on_click({
                    let copy_id = copy_id.clone();
                    cx.listener(move |d, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        d.copied.start(copy_id.clone(), cx);
                    })
                });
            let name = div().flex_1().min_w_0().text_size(px(crate::theme::text::BODY)).text_color(if r.installed { t.tx } else { t.tx3 }).child(a.label);
            div()
                .h(px(36.))
                .flex_none()
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(div().flex_none().mr(px(2.)).when(!r.installed, |d| d.opacity(0.45)).child(logo))
                .child(name)
                .children(status)
                .children(add)
                .child(ui::probe(copy_id, copy))
        });
        Some(div().flex().flex_col().gap(px(8.)).child(caption(t, "ADD TO YOUR AGENTS")).child(div().mx(px(-8.)).flex().flex_col().children(rows)))
    }

    fn mcp_panel(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        panel()
            .child(line(t.tx3, "What AI agents can do on this connection. More in Agents › Protection."))
            .child(self.tiers(t, cx))
            .when(dataman_core::MCP_ADVANCED, |d| d.child(self.mcp_advanced(t, cx)))
            .children(self.agent_rows(t, cx))
    }

    fn status(&self, t: &Theme) -> Option<gpui::AnyElement> {
        let (glyph, color, title, body): (&str, Hsla, String, String) = match &self.test {
            Test::NotTested => return None,
            Test::Testing { target } => ("◌", t.tx3, format!("Testing {target} …"), String::new()),
            Test::Ok { info, ms } => ("●", t.ok, format!("Connected in {} · {}", crate::fmt::rtt(*ms), info.short_version()), String::new()),
            Test::Failed { title, body } => ("✕", t.tx3, title.clone(), body.clone()),
            Test::Invalid(msg) => ("✕", t.tx3, msg.clone(), String::new()),
            Test::HostKey(p) => ("?", t.tx3, format!("Unknown host key · {}", p.host), format!("{}  {}", p.algorithm, p.fingerprint)),
        };
        let shown = |s: String, c: Hsla, lines: usize| div().overflow_hidden().line_clamp(lines).text_ellipsis().text_color(c).child(s);
        let tip = if body.is_empty() { title.clone() } else { format!("{title}\n{body}") };
        let first = body.lines().next().unwrap_or("").to_string();
        let failed = matches!(self.test, Test::Failed { .. } | Test::Invalid(_));
        Some(ui::probe(
            "dlg-status",
            div()
                .id("dlg-status")
                .min_w_0()
                .flex()
                .flex_row()
                .gap(px(6.))
                .text_size(px(crate::theme::text::UI))
                .line_height(px(17.))
                .child(div().flex_none().text_size(px(crate::theme::text::TINY)).text_color(color).child(glyph))
                .child(div().flex_1().min_w_0().flex().flex_col().child(shown(title, t.tx, 2)).when(!first.is_empty(), |d| d.child(shown(first, t.tx3, 3))))
                .map(|d| if failed { d.tip(ui::error_tip(tip)) } else { d.tip(ui::text_tooltip(tip)) }),
        ))
    }

    fn footer(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let testing = matches!(self.test, Test::Testing { .. });
        let edit = !self.is_new;
        let last = edit || self.step == Step::Mcp;
        let label = if self.saving {
            "Saving…"
        } else if edit && !self.password_needed {
            "Save"
        } else if last {
            "Save & connect"
        } else {
            "Continue"
        };
        let primary = if self.blocked(cx) {
            ui::button_off(t, "dlg-save", Btn::Primary)
        } else {
            ui::button(t, "dlg-save", Btn::Primary).on_click(cx.listener(|d, _, window, cx| d.primary(window, cx)))
        };
        let test = if testing || self.target(cx).is_empty() {
            ui::button_off(t, "dlg-test", Btn::Secondary)
        } else {
            ui::button(t, "dlg-test", Btn::Secondary).on_click(cx.listener(|d, _, _, cx| d.test(cx)))
        };
        ui::modal_footer(t)
            .when(edit && !self.password_needed, |d| {
                d.child(ui::probe(
                    "dlg-delete",
                    ui::button(t, "dlg-delete", Btn::Ghost)
                        .text_color(t.err)
                        .child(if self.confirm_delete { "Delete — sure?" } else { "Delete" })
                        .on_click(cx.listener(|d, _, _, cx| d.delete(cx))),
                ))
            })
            .when(self.shows_test(), |d| d.child(ui::probe("dlg-test", test.child(if testing { "Testing…" } else { "Test" }).child(ui::key_hint("⌘T")))))
            .child(div().flex_1().min_w_0().children(self.status(t)))
            .when(matches!(self.test, Test::HostKey(_)), |d| {
                d.child(ui::probe("dlg-trust", ui::button(t, "dlg-trust", Btn::Secondary).child("Trust").on_click(cx.listener(|d, _, _, cx| d.trust(cx)))))
            })
            .when(!edit && self.step == Step::Mcp, |d| {
                d.child(ui::probe(
                    "dlg-skip",
                    if self.saving {
                        ui::button_off(t, "dlg-skip", Btn::Ghost).child("Skip")
                    } else {
                        ui::button(t, "dlg-skip", Btn::Ghost).child("Skip").on_click(cx.listener(|d, _, window, cx| d.skip(window, cx)))
                    },
                ))
            })
            .child(ui::probe("dlg-save", primary.font_weight(FontWeight::SEMIBOLD).child(label).child(ui::key_hint(if last { "⌘↩" } else { "↩" }))))
    }

    fn header(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let logo = div().flex_none().size(px(16.)).mr(px(8.)).when(self.engine_picked, |d| d.child(crate::assets::logo(self.engine).size(px(16.))));
        let middle = if self.is_new {
            let file = self.engine_picked && self.is_file();
            let mut crumbs = div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(6.));
            for (i, (step, label)) in STEPS.into_iter().enumerate() {
                let state = crumb_state(step, self.step, self.seen, self.engine_picked, file);
                let crumb = ui::crumb(t, ("crumb", i), label, state);
                let crumb =
                    if state == ui::CrumbState::Visited { crumb.on_click(cx.listener(move |d, _, window, cx| d.enter(step, window, cx))) } else { crumb };
                crumbs = crumbs.child(ui::probe(format!("crumb-{label}"), crumb));
                if i + 1 < STEPS.len() {
                    crumbs = crumbs.child(ui::crumb_sep(t));
                }
            }
            crumbs
        } else {
            let title: SharedString = if self.password_needed { format!("Password needed · {}", self.profile.name).into() } else { "Edit connection".into() };
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(crate::theme::text::BODY))
                .font_weight(FontWeight::MEDIUM)
                .child(title)
        };
        ui::modal_header_bar(t)
            .child(logo)
            .child(middle)
            .when(self.profile.agent_made, |d| {
                d.child(ui::probe(
                    "dlg-agent-made",
                    div().flex_none().mx(px(12.)).text_size(px(crate::theme::text::SMALL)).text_color(t.tx3).child("Filled in by an agent"),
                ))
            })
            .child(ui::modal_close(t, "dlg-close").on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close))))
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
            .child(div().text_size(px(crate::theme::text::TINY)).text_color(if a.open { t.tx2 } else { t.tx3 }).child("MCP · ADVANCED"))
            .when(!a.open, |d| d.children(summary.map(|s| div().text_size(px(crate::theme::text::HINT)).text_color(t.tx3).child(s))))
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
        let note = |text: &'static str| div().text_size(px(crate::theme::text::SMALL)).line_height(px(16.)).text_color(t.tx3).child(text);
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
                .child(div().flex_1().min_w_0().py(px(4.)).text_size(px(crate::theme::text::UI)).text_color(t.tx2).child(s.clone()))
                .child(ui::icon_button(t, ("allowed-rm", i), Icon::Close, 22., 11.).tip(ui::text_tooltip("Remove")).on_click(cx.listener(
                    move |d, _, _, cx| {
                        if i < d.advanced.allowed.len() {
                            d.advanced.allowed.remove(i);
                        }
                        cx.notify();
                    },
                )))
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
            .child(div().text_size(px(crate::theme::text::TINY)).text_color(t.tx3).child("ALLOWED QUERIES"))
            .child(note("Agents run these without asking, whatever the tier above (never while it is off or the database is read-only). Each is one statement, matched statement by statement: the same words in the same order, case, spacing and comments aside; a ? stands for exactly one value (a string, a number or $1), never for more SQL."))
            .children(rows)
            .child(add)
            .children(a.error.clone().map(|e| div().text_size(px(crate::theme::text::SMALL)).text_color(t.err).child(e)));
        let switch = div().flex_none().w(px(26.)).h(px(14.)).relative().bg(if a.yolo { t.err } else { t.line2 }).child(
            div().absolute().top(px(2.)).when(a.yolo, |d| d.right(px(2.))).when(!a.yolo, |d| d.left(px(2.))).size(px(10.)).rounded(px(5.)).bg(if a.yolo {
                gpui::white()
            } else {
                t.tx3
            }),
        );
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
                    .child(div().text_size(px(crate::theme::text::BODY)).font_weight(FontWeight::SEMIBOLD).text_color(t.err).child("YOLO · agents run anything"))
                    .child(div().text_size(px(crate::theme::text::SMALL)).line_height(px(16.)).text_color(t.tx2).child(
                        "Every statement an agent sends runs here at once, with nobody asked: DROP, TRUNCATE, DELETE without WHERE, any number of rows, on production too. Read-only still refuses writes; MCP off still hides the connection.",
                    ))
                    .when(a.yolo && off, |d| d.child(div().text_size(px(crate::theme::text::SMALL)).text_color(t.tx3).child("No effect while MCP is off."))),
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
                .child(div().flex_none().text_size(px(crate::theme::text::BODY)).text_color(t.err).child("!"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(crate::theme::text::UI))
                        .line_height(px(17.))
                        .text_color(t.tx)
                        .child("This connection is production: with YOLO, an agent can change or destroy its data and nobody is asked first."),
                )
                .child(ui::button(t, "yolo-cancel", Btn::Ghost).h(px(26.)).child("Cancel").on_click(cx.listener(|d, _, _, cx| d.confirm_yolo(false, cx))))
                .child(
                    ui::button(t, "yolo-confirm", Btn::Danger)
                        .h(px(26.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Turn On YOLO")
                        .on_click(cx.listener(|d, _, _, cx| d.confirm_yolo(true, cx))),
                )
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
        .text_size(px(crate::theme::text::HINT))
        .font_weight(FontWeight::SEMIBOLD)
        .child("YOLO")
}

pub fn ssh_error(e: &anyhow::Error) -> Option<SshError> {
    e.chain().find_map(|c| c.downcast_ref::<SshError>()).cloned()
}

pub fn server_message(e: &anyhow::Error) -> (String, String, bool) {
    if let Some(s) = ssh_error(e) {
        return (s.to_string(), String::new(), false);
    }
    if let Some(s) = e.chain().find_map(|c| c.downcast_ref::<SignInError>()) {
        return (s.title.clone(), s.body.clone(), false);
    }
    let hint = cloudauth::hint_of(e);
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
            lines.extend(hint.map(str::to_string));
            // By the error's class on its engine (28P01, 1045, 18456…), in any language; a
            // refused cloud token has no password to fix.
            (title, lines.join("\n"), hint.is_none() && dataman_core::needs_password(e))
        }
        None => {
            let msg = format!("{e:#}");
            let (title, rest) = msg.split_once('\n').unwrap_or((&msg, ""));
            let body = [rest, hint.unwrap_or_default()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
            (title.to_string(), body, hint.is_none() && dataman_core::needs_password(e))
        }
    }
}

impl Render for ConnDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        self.reveal_field(window, cx);
        let database = self.is_new && self.step == Step::Database;
        let mut body = div().flex().flex_col();
        if !self.is_new {
            body = body.child(self.connection_panel(&t, cx).pb(px(14.)));
            if !self.is_file() {
                body = body.child(self.sign_in_panel(&t, window, cx).pt(px(0.)).pb(px(14.)));
            }
            body = body.child(panel().pt(px(0.)).child(self.tiers(&t, cx)).when(dataman_core::MCP_ADVANCED, |d| d.child(self.mcp_advanced(&t, cx))));
        } else {
            body = match self.step {
                Step::Database => body.child(self.render_database(&t, cx)),
                Step::Connection => body.child(self.connection_panel(&t, cx)),
                Step::SignIn => body.child(self.sign_in_panel(&t, window, cx)),
                Step::Mcp => body.child(self.mcp_panel(&t, cx)),
            };
        }
        let max_h = window.viewport_size().height.pt() - crate::theme::metrics::TITLE_H - 32.;
        // One size, whichever the step: the body scrolls.
        let card = ui::modal(&t, "conn-dialog")
            .w(px(600.))
            .h(px(560.))
            .max_h(px(max_h))
            .child(self.header(&t, cx))
            .child(div().id("dlg-body").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.body_scroll).child(body))
            .when(!database, |d| d.child(self.footer(&t, cx)));
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
            .on_action(cx.listener(|d, _: &DialogSave, window, cx| d.primary(window, cx)))
            .on_action(cx.listener(|d, _: &DialogTest, _, cx| {
                if d.shows_test() && !d.target(cx).is_empty() {
                    d.test(cx);
                }
            }))
            .on_action(cx.listener(|d, _: &DialogCancel, _, cx| d.cancel(cx)))
            .on_action(cx.listener(|d, _: &NextField, window, cx| d.focus_field(1, window, cx)))
            .on_action(cx.listener(|d, _: &PrevField, window, cx| d.focus_field(-1, window, cx)))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(ui::esc_closes(Some("ConnDialog"), cx.listener(|d, _: &DialogCancel, _, cx| d.cancel(cx))))
            .child(ui::probe("dlg-card", card))
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

    #[test]
    fn the_search_tells_a_name_from_a_connection_string() {
        for name in ["post", "oracle", "mongo", "pg", "sql server", "redis", "my"] {
            assert!(connstr::resolved(name).is_none(), "{name} would leave the picker");
        }
        assert!(connstr::resolved("postgres://ana@db.internal/orders").is_some());
        assert!(connstr::resolved("oracle://db.internal/orders").is_none(), "no message, no tiles");
        assert!(tiles_for("oracle://db.internal/orders").is_empty());
        assert_eq!(tiles_for("redi"), vec![Engine::Redis]);
    }

    #[test]
    fn the_host_picks_the_method() {
        assert_eq!(preselected(Engine::Postgres, "orders.postgres.database.azure.com"), AuthKind::EntraId);
        assert_eq!(preselected(Engine::SqlServer, "orders.database.windows.net"), AuthKind::EntraId);
        assert_eq!(preselected(Engine::MySql, "orders.abc123.us-east-1.rds.amazonaws.com"), AuthKind::AwsIam);
        assert_eq!(preselected(Engine::Postgres, "orders.europe-west1.sql.goog"), AuthKind::GoogleIam);
        assert_eq!(preselected(Engine::Postgres, "db.internal"), AuthKind::Password);
        assert_eq!(preselected(Engine::SqlServer, "orders.abc123.us-east-1.rds.amazonaws.com"), AuthKind::Password, "SQL Server has no IAM sign-in");
        assert_eq!(preselected(Engine::MongoDb, "orders.database.windows.net"), AuthKind::Password);
    }

    #[test]
    fn the_password_method_is_named_for_what_is_there() {
        assert_eq!(method_label(AuthKind::Password, "ana", true), "Keep user & password");
        assert_eq!(method_label(AuthKind::Password, "ana", false), "User & password");
        assert_eq!(method_label(AuthKind::Password, "  ", true), "User & password");
        assert_eq!(method_label(AuthKind::EntraId, "ana", true), "Microsoft Entra ID");
    }

    #[test]
    fn a_tier_answers_for_this_connection() {
        let cell = |tier: McpAccess, col: usize, prod, ro| {
            let cans = TIERS.iter().find(|(a, ..)| *a == tier).unwrap().2;
            tier_cell(col, cans[col], prod, ro)
        };
        assert_eq!([0, 1, 2].map(|c| cell(McpAccess::Off, c, false, false)), [Can::No; 3]);
        assert_eq!([0, 1, 2].map(|c| cell(McpAccess::Read, c, false, false)), [Can::Yes, Can::No, Can::No]);
        assert_eq!([0, 1, 2].map(|c| cell(McpAccess::Write, c, false, false)), [Can::Yes, Can::Yes, Can::No]);
        assert_eq!([0, 1, 2].map(|c| cell(McpAccess::Write, c, true, false)), [Can::Yes, Can::Asks, Can::No], "production asks");
        assert_eq!([0, 1, 2].map(|c| cell(McpAccess::Full, c, false, false)), [Can::Yes, Can::Yes, Can::Asks]);
        assert_eq!([0, 1, 2].map(|c| cell(McpAccess::Full, c, false, true)), [Can::Yes, Can::No, Can::No], "read-only reads");
    }

    #[test]
    fn the_breadcrumb_says_where_each_step_stands() {
        use ui::CrumbState::*;
        let seen = [true, true, false, false];
        assert_eq!(crumb_state(Step::Connection, Step::Connection, seen, true, false), Current);
        assert_eq!(crumb_state(Step::Database, Step::Connection, seen, true, false), Visited);
        assert_eq!(crumb_state(Step::SignIn, Step::Connection, seen, true, false), Next);
        assert_eq!(crumb_state(Step::SignIn, Step::Connection, seen, true, true), Skipped, "a file's");
        assert_eq!(crumb_state(Step::SignIn, Step::Database, [true, false, false, false], false, false), Next, "no database yet");
        assert_eq!(Step::Connection.after(true), Some(Step::Mcp));
        assert_eq!(Step::Connection.after(false), Some(Step::SignIn));
        assert_eq!(Step::Mcp.after(false), None);
    }

    #[test]
    fn every_string_placeholder_reads_as_its_engine() {
        for e in Engine::ALL.into_iter().filter(|e| !e.is_file()) {
            let got = connstr::resolved(&string_placeholder(e)).map(|s| s.engine);
            // https:// reads as Elasticsearch; an OpenSearch sheet keeps its own.
            let want = if e == Engine::OpenSearch { Engine::Elasticsearch } else { e };
            assert_eq!(got, Some(want), "{e:?}");
        }
    }

    #[test]
    fn an_unnamed_connection_is_called_after_its_host() {
        assert_eq!(host_stem("orders.postgres.database.azure.com"), "orders");
        assert_eq!(host_stem("10.20.30.40"), "10.20.30.40");
        assert_eq!(host_stem("localhost"), "localhost");
    }

    #[test]
    fn a_sign_in_status_is_short() {
        let failed = |fix| SignIn::Failed { title: "Azure CLI (az) not found".into(), body: "long".into(), fix };
        assert_eq!(SignIn::Unchecked.line("az"), None);
        assert_eq!(SignIn::Checking.line("az").unwrap().1, "Checking az");
        assert_eq!(SignIn::SigningIn.line("az").unwrap().1, "Waiting for the browser");
        assert_eq!(failed(Some(Fix::Install(Cli::Az))).line("az").unwrap().1, "az not found");
        assert_eq!(failed(Some(Fix::Again(Cli::Az))).line("az").unwrap().1, "az did not answer");
        assert_eq!(SignIn::Ok { who: Some("arn:aws:sts::1:assumed-role/Admin/ana".into()) }.line("aws").unwrap(), ("●", "Admin/ana".to_string()));
    }
}
