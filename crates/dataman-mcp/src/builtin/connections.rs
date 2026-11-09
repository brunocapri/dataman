//! Passwords are write-only: they go to the Keychain, never into connections.toml, a response
//! or the audit log. Through MCP a connection's safety settings only stay or tighten, a saved
//! password never follows a connection to another server, and moving one there waits for the
//! user's Allow. A cloud sign-in (a token from the user's own az, aws or gcloud) goes only to the
//! cloud's own database hosts and never through a tunnel: an agent cannot point a bearer token
//! good for every database of the user's at a host of its choosing.

use dataman_core::cloudauth::{self, host_allowed};
use dataman_core::server::{self, FLAVORS, Flavor};
use dataman_core::{Auth, AuthKind, ConfigStore, ConnectionProfile, Engine, Environment, McpAccess, SshAuth, SshSettings, SslMode};
use serde_json::{Value, json};

use super::Builtin;
use crate::tool::{Args, BoxFut, Call, Schema, ToolError};

/// The `engine` field: the engines, and the servers that speak one's protocol by their own names
/// (what each one is, from `server::FLAVORS`).
pub(super) fn engine_prop(engines: Vec<&'static str>, lead: &str) -> Value {
    let flavors: Vec<String> = FLAVORS.iter().filter(|f| engines.contains(&f.slug)).map(|f| format!("{} ({})", f.slug, f.engine.slug())).collect();
    json!({"type": "string", "enum": engines, "description": format!("{lead} A server that speaks an engine's protocol by its own name is that engine's connection, shown as that server, with its defaults: {}.", flavors.join(", "))})
}

/// The flavors' own ports, after the engines' (`yugabytedb 5433`).
pub(super) fn port_text() -> String {
    let own: Vec<String> = FLAVORS.iter().filter_map(|f| Some(format!("{} {}", f.slug, f.default_port?))).collect();
    format!("Default: the engine's own ({}; {}).", Engine::port_list(), own.join(", "))
}

fn fields(s: Schema, mcp: Value) -> Schema {
    s.prop("connection_string", false, json!({"type": "string", "description": "A full connection string (postgres://, mysql://, sqlserver://, sqlite:///path, duckdb:///path URL, libpq key=value, ADO.NET); the fields given here override whatever it sets."}))
        .prop("engine", false, engine_prop(server::names(), "Default postgres; sqlite and duckdb are files (see path)."))
        .prop("path", false, json!({"type": "string", "description": "The database file of a sqlite or duckdb connection: an existing file (~ is the home folder). A file outside the home folder waits for the user's Allow."}))
        .prop("port", false, json!({"type": "integer", "minimum": 1, "maximum": 65535, "description": port_text()}))
        .prop("password", false, json!({"type": "string", "maxLength": 1000, "description": "Saved to the system keyring (the macOS Keychain, the Secret Service on Linux, the Credential Manager on Windows); never echoed back."}))
        .prop("database", false, json!({"type": "string", "description": "Default: postgres (postgres), empty (mysql, sql-server: the login's default); redis takes 0–15."}))
        .prop("sslmode", false, json!({"type": "string", "enum": ["disable", "prefer", "require", "verify-full"], "description": "Default: prefer on this machine (localhost, a socket) or through an SSH tunnel, require anywhere else (verify-full on ClickHouse)."}))
        .prop("color", false, json!({"type": "string", "description": "A swatch name shown in the explorer and on tabs; default: picked from the id."}))
        .prop("env", false, json!({"type": "string", "enum": ["none", "dev", "staging", "production"], "description": "production asks before every write and DDL, and gets the red accent."}))
        .prop("read_only", false, json!({"type": "boolean", "description": "Sessions start with default_transaction_read_only on; Dataman also refuses writes client-side."}))
        .prop("mcp", false, mcp)
        .prop("ssh_host", false, json!({"type": "string", "description": "Reach the server through this SSH host (an ~/.ssh/config alias works); \"\" removes the tunnel."}))
        .prop("ssh_port", false, json!({"type": "integer", "minimum": 1, "maximum": 65535, "description": "SSH port; default: the alias' Port, or 22."}))
        .prop("ssh_user", false, json!({"type": "string", "description": "SSH user; default: the alias' User, or the local user."}))
        .prop("ssh_auth", false, json!({"type": "string", "enum": ["agent", "key", "password"], "description": "How the tunnel signs in (a password or passphrase is typed in Dataman)."}))
        .prop("ssh_key_file", false, json!({"type": "string", "description": "Private key for ssh_auth key; default: the alias' IdentityFile, or ~/.ssh/id_*."}))
        .prop("auth", false, json!({"type": "string", "enum": ["password", "entra-id", "aws-iam", "google-iam"], "description": "How it signs in: a password (default), or a short-lived token the user's own cloud CLI makes for each connection (az, aws, gcloud; nothing to save). postgres and mysql take all three, sql-server entra-id. A cloud method only for that cloud's own hosts (*.database.windows.net, *.database.azure.com, *.rds.amazonaws.com, *.sql.goog), never through an SSH tunnel (aws-iam excepted), never with a password; TLS is always required."}))
        .prop("auth_tenant", false, json!({"type": "string", "description": "entra-id: the Entra tenant (id or domain); default: az's own."}))
        .prop("aws_profile", false, json!({"type": "string", "description": "aws-iam: the AWS CLI profile (IAM Identity Center or keys); default: the default profile."}))
        .prop("aws_region", false, json!({"type": "string", "description": "aws-iam: the region; default: the one in the RDS endpoint's name."}))
        .prop("gcp_account", false, json!({"type": "string", "description": "google-iam: the gcloud account; default: gcloud's active one."}))
}

pub fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "create_connection",
            description: "Saves a new database connection, for every engine Dataman speaks and the servers that speak one's protocol (see engine; sqlite and duckdb take a path to an existing file instead of host and user). Fields directly, or connection_string (a postgres://, mysql://, sqlserver://, mongodb:// (+srv) or redis:// URL, a libpq key=value string, or an ADO.NET string); given fields win over whatever the string sets. A password goes to the system keyring only (the macOS Keychain, the Secret Service on Linux), never into connections.toml, this tool's response, or a log. Refuses to replace a connection of the same name unless overwrite is true.",
            schema: fields(
                Schema::new()
                    .prop("name", true, json!({"type": "string", "maxLength": 200, "description": "A short label; must be unique (case-insensitive) unless overwrite is true."}))
                    .prop("host", false, json!({"type": "string", "description": "Host, or a Unix-socket directory (starts with /). Required unless connection_string gives one."}))
                    .prop("user", false, json!({"type": "string", "description": "Required unless connection_string gives one."})),
                json!({"type": "string", "enum": ["off", "read", "write"], "description": "What agents may do here; default: read, off on production."}),
            )
            .prop("overwrite", false, json!({"type": "boolean", "description": "Replace the existing connection of the same name (keeping its id and any saved password) instead of refusing."})),
            need: None,
            run: create_connection,
        },
        Builtin {
            name: "update_connection",
            description: "Edits a saved connection (from list_connections): only the fields given change. A password is write-only: saved to the system keyring, never returned. env, sslmode, read_only and mcp may only stay the same or tighten; moving the connection to another server (engine, host, port, user or SSH) waits for the user's Allow in Dataman and drops its saved password unless a new one is given. Refused while the connection has a transaction open in Dataman. Returns the fields that changed.",
            schema: fields(
                Schema::new()
                    .prop("connection", true, json!({"type": "string", "description": "Connection name (or id) from list_connections."}))
                    .prop("name", false, json!({"type": "string", "maxLength": 200, "description": "A new label; must be unique (case-insensitive)."}))
                    .prop("host", false, json!({"type": "string", "description": "Host, or a Unix-socket directory (starts with /)."}))
                    .prop("user", false, json!({"type": "string", "description": "The database user."})),
                json!({"type": "string", "enum": ["off", "read", "write", "full"], "description": "What agents may do here; may only stay the same or narrow."}),
            ),
            need: None,
            run: update_connection,
        },
    ]
}

pub(super) fn engine_of(s: &str) -> Result<(Engine, Flavor), ToolError> {
    server::named(s).ok_or_else(|| {
        let shown: String = s.chars().take(40).collect();
        let known: Vec<&str> = Engine::slugs().into_iter().chain(FLAVORS.iter().map(|f| f.slug)).collect();
        ToolError::Invalid(format!("unknown engine {shown:?}; known: {}", known.join(", ")))
    })
}

fn sslmode_of(s: &str) -> Result<SslMode, ToolError> {
    SslMode::ALL.into_iter().find(|m| m.label() == s).ok_or_else(|| ToolError::Invalid(format!("unknown sslmode {s:?}")))
}

fn env_of(s: &str) -> Result<Environment, ToolError> {
    Environment::ALL.into_iter().find(|e| e.label() == s).ok_or_else(|| ToolError::Invalid(format!("unknown env {s:?}")))
}

fn mcp_of(s: &str) -> Result<McpAccess, ToolError> {
    McpAccess::ALL.into_iter().find(|m| m.label() == s).ok_or_else(|| ToolError::Invalid(format!("unknown mcp {s:?}")))
}

fn ssh_auth_of(s: &str) -> Result<SshAuth, ToolError> {
    SshAuth::ALL.into_iter().find(|m| m.label() == s).ok_or_else(|| ToolError::Invalid(format!("unknown ssh_auth {s:?}")))
}

fn auth_value(key: &str, v: &str) -> Result<Option<String>, ToolError> {
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if !cloudauth::plain_value(v) {
        return Err(ToolError::Invalid(format!("{key:?} takes letters, digits and . _ @ : / + - only")));
    }
    Ok(Some(v.to_string()))
}

/// `auth` and its fields over the connection's: a field of another method is refused.
fn apply_auth(profile: &mut ConnectionProfile, a: &Args) -> Result<(), ToolError> {
    if let Some(k) = a.opt_str("auth") {
        let kind = AuthKind::parse(k).ok_or_else(|| ToolError::Invalid(format!("unknown auth {k:?}")))?;
        if kind != profile.auth.kind() {
            profile.auth = kind.fresh();
        }
    }
    for (key, kind) in [("auth_tenant", AuthKind::EntraId), ("aws_profile", AuthKind::AwsIam), ("aws_region", AuthKind::AwsIam), ("gcp_account", AuthKind::GoogleIam)] {
        let Some(v) = a.opt_str(key) else { continue };
        if profile.auth.kind() != kind {
            return Err(ToolError::Invalid(format!("{key:?} is for auth {:?}", kind.slug())));
        }
        let v = auth_value(key, v)?;
        match (&mut profile.auth, key) {
            (Auth::EntraId { tenant }, _) => *tenant = v,
            (Auth::AwsIam { profile, .. }, "aws_profile") => *profile = v,
            (Auth::AwsIam { region, .. }, _) => *region = v,
            (Auth::GoogleIam { account }, _) => *account = v,
            (Auth::Password, _) => {}
        }
    }
    Ok(())
}

/// A cloud token only for the cloud's own hosts, never through a tunnel the agent picked (an
/// AWS token is signed for its host: harmless anywhere else, but an RDS endpoint is all that
/// takes one).
fn cloud_target(p: &ConnectionProfile) -> Result<(), ToolError> {
    let kind = p.auth.kind();
    if kind == AuthKind::Password {
        return Ok(());
    }
    if !p.engine.supports(kind) {
        return Err(ToolError::Invalid(format!("{} does not sign in with auth {:?}", p.engine.label(), kind.slug())));
    }
    if !host_allowed(kind, &p.host) {
        return Err(ToolError::Refused(format!(
            "{} is not a known {} database host: through MCP a cloud sign-in goes to the cloud's own hosts only (the user can set up any other in Dataman)",
            p.host,
            kind.label()
        )));
    }
    if p.ssh.is_some() && kind != AuthKind::AwsIam {
        return Err(ToolError::Refused(format!("through MCP a {} token never goes through an SSH tunnel (the user can set one up in Dataman)", kind.label())));
    }
    Ok(())
}

fn failed(e: anyhow::Error) -> ToolError {
    ToolError::Failed(format!("{e:#}"))
}

/// How much `env` locks the connection down: higher stays, or tightens, on overwrite; it never
/// loosens through this tool (`production` demoted to `dev` would drop the write confirmation
/// and the default MCP permission's `off`).
fn env_rank(e: Environment) -> u8 {
    match e {
        Environment::None => 0,
        Environment::Dev => 1,
        Environment::Staging => 2,
        Environment::Production => 3,
    }
}

pub(super) fn apply(profile: &mut ConnectionProfile, a: &Args) -> Result<Option<String>, ToolError> {
    let mut password = None;
    if let Some(cs) = a.opt_str("connection_string") {
        let (p, pw) = dataman_core::connstr::parse_profile(cs).ok_or_else(|| ToolError::Invalid("could not parse connection_string".into()))?;
        // A string that says nothing of TLS keeps the connection's on the same server, and takes
        // the default of another (create_connection picks the default for a new one).
        if dataman_core::connstr::tls_given(cs) {
            profile.sslmode = p.sslmode;
        } else if !p.host.eq_ignore_ascii_case(&profile.host) {
            profile.sslmode = dataman_core::default_sslmode(p.engine, &p.host, profile.ssh.is_some());
        }
        profile.path = p.path;
        profile.read_only |= p.read_only && p.engine.is_file();
        // A string of a flavor's own scheme names it; one of the engine's leaves the connection's.
        if !p.flavor.is_standard() || p.engine != profile.engine {
            profile.flavor = p.flavor;
        }
        profile.engine = p.engine;
        profile.host = p.host;
        profile.port = p.port;
        profile.user = p.user;
        profile.database = p.database;
        password = pw;
    }
    if let Some(e) = a.opt_str("engine") {
        let (before, before_flavor) = (profile.engine, profile.flavor);
        (profile.engine, profile.flavor) = engine_of(e)?;
        // What the other engine's (or flavor's) defaults filled in (a new profile's 5432,
        // `postgres`) follows this one's; given fields below still win.
        if profile.port == before_flavor.default_port(before) {
            profile.port = profile.flavor.default_port(profile.engine);
        }
        if profile.engine != before && profile.database == before.spec().default_database {
            profile.database = profile.engine.spec().default_database.to_string();
        }
    }
    if let Some(h) = a.opt_str("host") {
        profile.host = h.to_string();
    }
    if let Some(p) = a.get("port").and_then(Value::as_u64) {
        profile.port = p as u16;
    }
    if let Some(u) = a.opt_str("user") {
        profile.user = u.to_string();
    }
    if let Some(d) = a.opt_str("database") {
        profile.database = d.to_string();
    }
    if let Some(s) = a.opt_str("sslmode") {
        profile.sslmode = sslmode_of(s)?;
    }
    if let Some(c) = a.opt_str("color") {
        (profile.color, profile.color_picked) = (Some(c.to_string()), true);
    }
    if let Some(e) = a.opt_str("env") {
        profile.env = env_of(e)?;
    }
    if a.get("read_only").is_some() {
        profile.read_only = a.bool("read_only");
    }
    if let Some(m) = a.opt_str("mcp") {
        let m = mcp_of(m)?;
        if m != profile.mcp_access() {
            profile.mcp = Some(m);
        }
    }
    if let Some(h) = a.opt_str("ssh_host") {
        match h.trim() {
            "" => profile.ssh = None,
            h => profile.ssh.get_or_insert_with(SshSettings::default).host = h.to_string(),
        }
    }
    let ssh_given = ["ssh_port", "ssh_user", "ssh_auth", "ssh_key_file"].into_iter().find(|k| a.get(k).is_some());
    if let Some(k) = ssh_given {
        let Some(s) = profile.ssh.as_mut() else {
            return Err(ToolError::Invalid(format!("{k:?} needs an SSH tunnel: pass ssh_host too")));
        };
        if let Some(p) = a.get("ssh_port").and_then(Value::as_u64) {
            s.port = Some(p as u16);
        }
        if let Some(u) = a.opt_str("ssh_user") {
            s.user = u.trim().to_string();
        }
        if let Some(m) = a.opt_str("ssh_auth") {
            s.auth = ssh_auth_of(m)?;
        }
        if let Some(f) = a.opt_str("ssh_key_file") {
            s.key_file = f.trim().to_string();
        }
    }
    if let Some(pw) = a.opt_str("password") {
        password = Some(pw.to_string());
    }
    apply_auth(profile, a)?;
    if profile.auth.is_token() && password.is_some() {
        return Err(ToolError::Invalid(format!("auth {:?} takes no password: the user's cloud CLI makes a token for each connection", profile.auth.kind().slug())));
    }
    if profile.auth.is_token() && !profile.engine.supports(profile.auth.kind()) {
        return Err(ToolError::Invalid(format!("{} does not sign in with auth {:?}", profile.engine.label(), profile.auth.kind().slug())));
    }
    if let Some(p) = a.opt_str("path") {
        profile.path = Some(p.trim().to_string()).filter(|p| !p.is_empty());
    }
    if profile.engine.is_file() {
        // A file has no server: what a string or an older profile set for one goes.
        if profile.ssh.is_some() || password.is_some() {
            return Err(ToolError::Invalid(format!("a {} connection is a file: no password and no SSH tunnel", profile.engine.label())));
        }
        if a.opt_str("sslmode").is_some_and(|m| m != "disable") {
            return Err(ToolError::Invalid(format!("a {} connection is a file: no TLS", profile.engine.label())));
        }
        profile.host.clear();
        profile.port = 0;
        profile.user.clear();
        profile.database.clear();
        profile.sslmode = SslMode::Disable;
    } else {
        profile.path = None;
    }
    Ok(password)
}

/// Agents point a file connection at an existing database file only (never create one), and
/// the user allows one outside the home folder.
async fn check_file(cx: &Call, old: Option<&ConnectionProfile>, p: &ConnectionProfile) -> Result<(), ToolError> {
    if !p.engine.is_file() {
        return Ok(());
    }
    let shown = p.path.clone().unwrap_or_default();
    if shown.trim().is_empty() {
        return Err(ToolError::Invalid(format!("\"path\" is required for a {} connection", p.engine.label())));
    }
    let file = p.file_path();
    if !file.is_absolute() {
        return Err(ToolError::Invalid("\"path\" must be absolute (or start with ~)".into()));
    }
    if !file.is_file() {
        return Err(ToolError::Refused(format!("{shown} does not exist: agents connect to existing database files only (a new one is created in the app)")));
    }
    if dataman_core::import::detect_database(&file) != Some(p.engine) {
        return Err(ToolError::Refused(format!("{shown} is not a {} database", p.engine.label())));
    }
    let home = dataman_core::os::home();
    let canonical = dataman_core::os::canonical(&file).unwrap_or(file);
    let inside = home.and_then(|h| dataman_core::os::canonical(h).ok()).is_some_and(|h| canonical.starts_with(&h));
    let same = old.is_some_and(|o| o.engine.is_file() && dataman_core::os::canonical(o.file_path()).is_ok_and(|c| c == canonical));
    // A changed file of a saved connection was asked about already (a move).
    let asked = old.is_some_and(|o| target_changed(o, p));
    if !inside && !same && !asked {
        let anchor = old.unwrap_or(p);
        cx.ask_about(anchor, &format!("file: {}", canonical.display()), vec!["a database file outside the home folder".into()]).await?;
    }
    Ok(())
}

/// As `engine` reads it: the engines with no opportunistic TLS take prefer for disable (which
/// `ConnectionProfile::normalize` writes in its place).
fn ssl_rank(m: SslMode, engine: Engine) -> u8 {
    match m {
        SslMode::Disable => 0,
        SslMode::Prefer if !engine.is_sql() => 0,
        SslMode::Prefer => 1,
        SslMode::Require => 2,
        SslMode::VerifyFull => 3,
    }
}

fn no_loosening(old: &ConnectionProfile, new: &ConnectionProfile) -> Result<(), ToolError> {
    if ssl_rank(new.sslmode, new.engine) < ssl_rank(old.sslmode, new.engine) {
        return Err(ToolError::Refused(format!("sslmode would loosen from {:?} to {:?}; through MCP it may only stay the same or tighten", old.sslmode.label(), new.sslmode.label())));
    }
    if env_rank(new.env) < env_rank(old.env) {
        return Err(ToolError::Refused(format!("env would loosen from {:?} to {:?}; through MCP it may only stay the same or tighten", old.env.label(), new.env.label())));
    }
    if old.read_only && !new.read_only {
        return Err(ToolError::Refused("read_only would turn off; through MCP it may only stay the same or turn on".into()));
    }
    if new.mcp_access() > old.mcp_access() {
        return Err(ToolError::Refused(format!(
            "mcp would widen from {:?} to {:?}; through MCP it may only stay the same or narrow (wider access is granted in the app)",
            old.mcp_access().label(),
            new.mcp_access().label()
        )));
    }
    Ok(())
}

fn target_changed(old: &ConnectionProfile, new: &ConnectionProfile) -> bool {
    new.engine != old.engine || new.host != old.host || new.port != old.port || new.user != old.user || new.ssh != old.ssh || new.path != old.path || new.auth.kind() != old.auth.kind()
}

fn ssh_text(p: &ConnectionProfile) -> String {
    p.ssh.as_ref().map(|s| format!("{}@{}:{}", s.user, s.host, s.port.unwrap_or(22))).unwrap_or_else(|| "none".into())
}

/// A new connection to what a saved one already reaches (the same file, or the same server and
/// user) would leave that one's protections behind: MCP off, Privacy, read-only, production. A
/// file needs no password and a server may let the user in without one (~/.pgpass, peer or trust
/// authentication), so the twin is refused: the agent uses the saved connection.
fn no_twin(cx: &Call, p: &ConnectionProfile) -> Result<(), ToolError> {
    if let Some(q) = guarded_twin(&cx.hub.profiles(), p) {
        return Err(ToolError::Refused(format!(
            "{} already reaches this {} with protections a new connection would not have (MCP access, Privacy, read-only, production or its agent login): use that connection, or ask the user to add another in Dataman",
            q.name,
            if p.engine.is_file() { "file" } else { "server as this user" }
        )));
    }
    Ok(())
}

pub(super) fn guarded_twin<'a>(profiles: &'a [ConnectionProfile], p: &ConnectionProfile) -> Option<&'a ConnectionProfile> {
    let canonical = |q: &ConnectionProfile| dataman_core::os::canonical(q.file_path()).ok();
    let same = |q: &ConnectionProfile| {
        if p.engine.is_file() || q.engine.is_file() {
            return p.engine.is_file() && q.engine.is_file() && canonical(p).is_some_and(|c| Some(c) == canonical(q));
        }
        // This machine by any of its names (localhost, 127.0.0.1, ::1) is the same server.
        let host = |h: &str| if dataman_core::config::is_local_host(h) { "localhost".to_string() } else { h.trim().trim_end_matches('.').to_ascii_lowercase() };
        q.engine == p.engine && host(&q.host) == host(&p.host) && q.port == p.port && q.user == p.user && q.ssh == p.ssh
    };
    // An agent login (or agents refused there) is a protection too: a twin would reach the
    // server as the user, not as the login.
    let guarded = |q: &ConnectionProfile| {
        q.mcp_access() == McpAccess::Off
            || !q.mcp_rules.is_default()
            || q.read_only
            || !q.read_only_databases.is_empty()
            || q.env == Environment::Production
            || q.agent_login.is_some()
            || q.agents_refused
    };
    profiles.iter().find(|q| q.id != p.id && same(q) && guarded(q))
}

/// The user types this connection's password into Dataman: sending it to another server is
/// the user's call (an agent could otherwise point a trusted connection at its own host).
async fn allow_move(cx: &Call, old: &ConnectionProfile, new: &ConnectionProfile) -> Result<(), ToolError> {
    if !target_changed(old, new) {
        return Ok(());
    }
    let mut lines = Vec::new();
    let mut line = |what: &str, a: String, b: String| {
        if a != b {
            lines.push(format!("{what}: {a} → {b}"));
        }
    };
    line("engine", old.engine.label().to_string(), new.engine.label().to_string());
    line("host", old.host.clone(), new.host.clone());
    line("port", old.port.to_string(), new.port.to_string());
    line("user", old.user.clone(), new.user.clone());
    line("ssh", ssh_text(old), ssh_text(new));
    line("file", old.path.clone().unwrap_or_default(), new.path.clone().unwrap_or_default());
    line("sign-in", old.auth.describe(), new.auth.describe());
    cx.ask_about(old, &lines.join("\n"), vec!["moves the connection to another server".into()]).await
}

/// An old secret must not follow the connection to another server unseen: kept only while
/// the target stays. Never returns the password.
fn settle_secrets(cx: &Call, old: Option<&ConnectionProfile>, profile: &mut ConnectionProfile, password: Option<String>) -> Result<&'static str, ToolError> {
    let had = old.is_some_and(|o| o.save_password);
    let moved = old.is_some_and(|o| target_changed(o, profile));
    let state = if let Some(pw) = &password {
        profile.save_password = true;
        cx.hub.core.secrets.set(&profile.id, pw).map_err(failed)?;
        "set"
    } else if had && !moved {
        profile.save_password = true;
        "kept"
    } else {
        profile.save_password = false;
        if had {
            let _ = cx.hub.core.secrets.delete(&profile.id);
            "dropped"
        } else {
            "none"
        }
    };
    if moved {
        let _ = cx.hub.core.secrets.delete(&profile.ssh_secret_id());
    }
    Ok(state)
}

/// A connection of the open project is the team's: an agent's change would be written to the
/// project's file as if the user made it, so none goes through, even one that tightens.
pub(super) fn of_project(cx: &Call, p: &ConnectionProfile) -> Result<(), ToolError> {
    let settings = cx.hub.settings();
    if dataman_core::project::Shared::of(&cx.hub.dirs, &settings).has_connection(&p.id) {
        let root = settings.project.clone().unwrap_or_default();
        return Err(ToolError::Refused(format!("{:?} belongs to the open project ({root}): change it in Dataman", p.name)));
    }
    Ok(())
}

/// Written over the connection as it was read (`base`): what the user changed in Dataman while
/// this call waited for an Allow (a tighter tier, read-only, rules) is never undone; the call
/// is refused instead, in the same write.
fn save(cx: &Call, base: Option<&ConnectionProfile>, profile: &ConnectionProfile) -> Result<(), ToolError> {
    let store = ConfigStore::open_with(&cx.hub.dirs, cx.hub.core.approved.clone()).map_err(failed)?;
    let mut profile = profile.clone();
    // The agent login is the user's, in Dataman only: as saved now (it may have been made or
    // removed while this call waited for the user), never what the agent's copy says.
    let bare = |p: &ConnectionProfile| ConnectionProfile { agent_login: None, agents_refused: false, ..p.clone() };
    let mut moved_on = false;
    store
        .edit_all(|g| {
            let now = g.iter_mut().find(|c| c.id == profile.id);
            if let (Some(b), Some(n)) = (base, now.as_deref())
                && bare(n) != bare(b)
            {
                moved_on = true;
                return;
            }
            match now {
                Some(c) => {
                    (profile.agent_login, profile.agents_refused) = (c.agent_login.clone(), c.agents_refused);
                    *c = profile.clone();
                }
                None => {
                    (profile.agent_login, profile.agents_refused) = (None, false);
                    g.push(profile.clone());
                }
            }
        })
        .map_err(failed)?;
    if moved_on {
        return Err(ToolError::Refused(format!("{:?} was changed in Dataman while this call waited: nothing was saved; read it again (list_connections) and retry", profile.name)));
    }
    if let Some(f) = cx.hub.front() {
        f.connections_changed();
    }
    Ok(())
}

fn changed_fields(old: &ConnectionProfile, new: &ConnectionProfile) -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut f = |name: &'static str, differs: bool| {
        if differs {
            v.push(name);
        }
    };
    f("name", old.name != new.name);
    f("engine", old.engine != new.engine);
    f("flavor", old.flavor != new.flavor);
    f("host", old.host != new.host);
    f("port", old.port != new.port);
    f("user", old.user != new.user);
    f("database", old.database != new.database);
    f("sslmode", old.sslmode != new.sslmode);
    f("color", old.color != new.color);
    f("env", old.env != new.env);
    f("read_only", old.read_only != new.read_only);
    f("mcp", old.mcp_access() != new.mcp_access());
    f("ssh", old.ssh != new.ssh);
    f("path", old.path != new.path);
    f("auth", old.auth != new.auth);
    v
}

fn create_connection(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let name = a.str("name")?.trim().to_string();
        if name.is_empty() {
            return Err(ToolError::Invalid("\"name\" must not be empty".into()));
        }
        let file = a.opt_str("engine").and_then(server::named).is_some_and(|(e, _)| e.is_file())
            || a.opt_str("connection_string").and_then(dataman_core::connstr::parse).is_some_and(|s| s.engine.is_file());
        if file && a.opt_str("path").is_none() && a.opt_str("connection_string").is_none() {
            return Err(ToolError::Invalid("\"path\" is required for a sqlite or duckdb connection".into()));
        }
        if !file && a.opt_str("connection_string").is_none() && (a.opt_str("host").is_none() || a.opt_str("user").is_none()) {
            return Err(ToolError::Invalid("\"host\" and \"user\" are required, directly or via connection_string".into()));
        }
        // The most an agent may grant through this tool is read: write stays an app-only choice,
        // whatever the connection is (new, or replaced with overwrite).
        if a.opt_str("mcp") == Some("write") {
            return Err(ToolError::Refused("MCP cannot grant \"write\" access; the most an agent may set is \"read\" (write is granted in the app)".into()));
        }
        let overwrite = a.bool("overwrite");
        let profiles = cx.hub.profiles();
        let existing = profiles.iter().find(|p| p.name.eq_ignore_ascii_case(&name));
        if existing.is_some() && !overwrite {
            return Err(ToolError::Refused(format!("a connection named {name:?} already exists; pass overwrite: true to replace it")));
        }
        cx.note_connection(&name);

        if existing.is_some_and(|p| p.mcp_access() == McpAccess::Off) {
            return Err(ToolError::Refused("MCP access to this connection is off (connection settings › MCP); it is edited in Dataman only".into()));
        }
        if let Some(p) = existing {
            of_project(cx, p)?;
        }
        // What an agent saves is its own, a replaced connection too (the same id): never the
        // project's, and its host is the agent's choice (no `*` line of ~/.pgpass goes there).
        let mut profile = match existing {
            Some(p) => ConnectionProfile { agent_made: true, ..p.clone() },
            None => ConnectionProfile { agent_made: true, ..ConnectionProfile::new(name.clone()) },
        };
        profile.name = name;
        let password = apply(&mut profile, &a)?;
        // TLS not asked for: opportunistic on this machine only, required anywhere else.
        let tls_given = a.opt_str("sslmode").is_some() || a.opt_str("connection_string").is_some_and(dataman_core::connstr::tls_given);
        if existing.is_none() && !tls_given {
            profile.sslmode = dataman_core::default_sslmode(profile.engine, &profile.host, profile.ssh.is_some());
        }
        // The connection form's rules (default port and database, Redis' 0–15…).
        profile.normalize();
        profile.validate().map_err(ToolError::Invalid)?;
        cloud_target(&profile)?;
        if let Some(old) = existing {
            no_loosening(old, &profile)?;
            allow_move(cx, old, &profile).await?;
        }
        // A saved connection moved onto another's target was asked about above (allow_move).
        if existing.is_none() {
            no_twin(cx, &profile)?;
        }
        check_file(cx, existing, &profile).await?;
        settle_secrets(cx, existing, &mut profile, password)?;
        save(cx, existing, &profile)?;
        Ok(json!({"id": profile.id, "name": profile.name, "engine": profile.engine.slug(), "flavor": Some(profile.flavor.slug()).filter(|s| !s.is_empty()), "overwritten": existing.is_some()}))
    })
}

fn update_connection(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let old = cx.hub.profile(a.str("connection")?).map_err(ToolError::Invalid)?;
        cx.note_profile(&old);
        if old.mcp_access() == McpAccess::Off {
            return Err(ToolError::Refused("MCP access to this connection is off (connection settings › MCP); it is edited in Dataman only".into()));
        }
        of_project(cx, &old)?;
        let mut profile = old.clone();
        if let Some(n) = a.opt_str("name") {
            let n = n.trim();
            if n.is_empty() {
                return Err(ToolError::Invalid("\"name\" must not be empty".into()));
            }
            if cx.hub.profiles().iter().any(|p| p.id != old.id && p.name.eq_ignore_ascii_case(n)) {
                return Err(ToolError::Refused(format!("another connection is named {n:?}; pick another name")));
            }
            profile.name = n.to_string();
        }
        let password = apply(&mut profile, &a)?;
        profile.normalize();
        // A database the call leaves alone stays as saved: PostgreSQL's empty one is the
        // login's own, not `postgres`.
        if profile.engine == old.engine && a.opt_str("database").is_none() && a.opt_str("connection_string").is_none() && profile.engine.spec().form.database {
            profile.database = old.database.clone();
        }
        profile.validate().map_err(ToolError::Invalid)?;
        no_loosening(&old, &profile)?;
        if target_changed(&old, &profile) || profile.auth != old.auth {
            cloud_target(&profile)?;
        }
        let changed = changed_fields(&old, &profile);
        if changed.is_empty() && password.is_none() {
            let state = if old.save_password { "kept" } else { "none" };
            return Ok(json!({"id": old.id, "name": old.name, "changed": [], "password": state}));
        }
        // The app reconnects an edited connection; a transaction open on it would end. The
        // flavor alone is only shown: nothing reconnects for it.
        if let Some(f) = cx.hub.front().filter(|_| changed.iter().any(|f| *f != "flavor")) {
            let open = f.open_transactions(&old.id).await;
            if !open.is_empty() {
                let tabs: Vec<String> = open.iter().map(|t| format!("{t:?}")).collect();
                let s = if open.len() == 1 { "" } else { "s" };
                return Err(ToolError::Refused(format!(
                    "{:?} has a transaction open in Dataman (tab{s} {}); commit or roll back there first, then try again",
                    old.name,
                    tabs.join(", ")
                )));
            }
        }
        allow_move(cx, &old, &profile).await?;
        check_file(cx, Some(&old), &profile).await?;
        cx.note_profile(&profile);
        // Moved by an agent: its host is the agent's choice from now on (see create_connection).
        profile.agent_made |= target_changed(&old, &profile);
        let state = settle_secrets(cx, Some(&old), &mut profile, password)?;
        save(cx, Some(&old), &profile)?;
        Ok(json!({"id": profile.id, "name": profile.name, "changed": changed, "password": state}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn args(v: Value) -> Args {
        Args(v.as_object().cloned().unwrap_or_else(Map::new))
    }

    fn base() -> ConnectionProfile {
        let mut p = ConnectionProfile::new("shop");
        (p.host, p.user, p.database) = ("db1".into(), "app".into(), "shop".into());
        p
    }

    #[test]
    fn a_nosql_prefer_saved_before_is_no_loosening_now() {
        let old = ConnectionProfile { engine: Engine::Redis, host: "localhost".into(), port: 6379, sslmode: SslMode::Prefer, ..ConnectionProfile::new("r") };
        let mut p = old.clone();
        apply(&mut p, &args(json!({"color": "violet"}))).unwrap();
        p.normalize();
        assert_eq!(p.sslmode, SslMode::Disable);
        assert!(no_loosening(&old, &p).is_ok());
        let pg = ConnectionProfile { sslmode: SslMode::Prefer, ..base() };
        assert!(no_loosening(&pg, &ConnectionProfile { sslmode: SslMode::Disable, ..pg.clone() }).is_err());
    }

    #[test]
    fn fields_apply_and_are_listed() {
        let old = base();
        let mut p = old.clone();
        let pw = apply(&mut p, &args(json!({"host": "db2", "port": 6543, "color": "violet", "password": "s3cret"}))).unwrap();
        assert_eq!(pw.as_deref(), Some("s3cret"));
        assert_eq!((p.host.as_str(), p.port, p.color.as_deref(), p.user.as_str(), p.database.as_str()), ("db2", 6543, Some("violet"), "app", "shop"));
        assert_eq!(changed_fields(&old, &p), ["host", "port", "color"]);
        assert!(target_changed(&old, &p));
        let mut q = old.clone();
        let pw = apply(&mut q, &args(json!({"connection_string": "postgresql://bob:pw1@h3:5440/other", "database": "mine"}))).unwrap();
        assert_eq!((q.host.as_str(), q.port, q.user.as_str(), q.database.as_str(), pw.as_deref()), ("h3", 5440, "bob", "mine", Some("pw1")));
        let mut r = old.clone();
        apply(&mut r, &args(json!({"mcp": "read"}))).unwrap();
        assert_eq!(r.mcp, None);
        assert!(changed_fields(&old, &r).is_empty());
    }

    #[test]
    fn flavor_names_are_engines_with_their_defaults() {
        let mut p = ConnectionProfile::new("yb");
        apply(&mut p, &args(json!({"engine": "yugabytedb", "host": "yb.internal", "user": "yugabyte"}))).unwrap();
        p.normalize();
        assert_eq!((p.engine, p.flavor, p.port, p.database.as_str()), (Engine::Postgres, Flavor::Yugabyte, 5433, "postgres"));
        // Its port given, it stays; back to plain PostgreSQL, the default port follows.
        let mut q = ConnectionProfile::new("yb2");
        apply(&mut q, &args(json!({"engine": "YugabyteDB", "port": 6000}))).unwrap();
        assert_eq!((q.flavor, q.port), (Flavor::Yugabyte, 6000));
        let mut r = p.clone();
        apply(&mut r, &args(json!({"engine": "postgres"}))).unwrap();
        assert_eq!((r.flavor, r.port), (Flavor::Standard, 5432));
        assert_eq!(changed_fields(&p, &r), ["flavor", "port"]);
        let mut m = ConnectionProfile::new("maria");
        apply(&mut m, &args(json!({"engine": "mariadb"}))).unwrap();
        assert_eq!((m.engine, m.flavor, m.port, m.database.as_str()), (Engine::MySql, Flavor::MariaDb, 3306, ""));
        let mut k = ConnectionProfile::new("cache");
        apply(&mut k, &args(json!({"engine": "dragonflydb"}))).unwrap();
        assert_eq!((k.engine, k.flavor, k.port), (Engine::Redis, Flavor::Dragonfly, 6379));
        // Only the flavor changes on a connection already there: shown, not a move.
        let mut t = base();
        apply(&mut t, &args(json!({"engine": "timescaledb"}))).unwrap();
        assert_eq!((changed_fields(&base(), &t), target_changed(&base(), &t)), (vec!["flavor"], false));
        let mut s = base();
        apply(&mut s, &args(json!({"connection_string": "jdbc:yugabytedb://yb.internal/shop"}))).unwrap();
        assert_eq!((s.engine, s.flavor, s.port), (Engine::Postgres, Flavor::Yugabyte, 5433));
        // A string of the engine's own scheme keeps the flavor the connection has.
        let mut kept = t.clone();
        apply(&mut kept, &args(json!({"connection_string": "postgres://app@db1/shop"}))).unwrap();
        assert_eq!(kept.flavor, Flavor::Timescale);
        let e = apply(&mut base(), &args(json!({"engine": "cockroachdb"}))).unwrap_err();
        let ToolError::Invalid(text) = e else { panic!("{e:?}") };
        assert!(text.contains("\"cockroachdb\"") && text.contains("yugabytedb") && text.contains("postgres"), "{text}");
        let long = "x".repeat(5000);
        let ToolError::Invalid(text) = apply(&mut base(), &args(json!({"engine": long}))).unwrap_err() else { panic!() };
        assert!(text.len() < 400, "{text}");
    }

    #[test]
    fn the_engine_field_names_every_flavor() {
        let v = engine_prop(server::names(), "Lead.");
        let listed: Vec<&str> = v["enum"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
        assert!(FLAVORS.iter().all(|f| listed.contains(&f.slug)) && Engine::ALL.iter().all(|e| listed.contains(&e.slug())));
        let d = v["description"].as_str().unwrap();
        assert!(d.starts_with("Lead.") && d.contains("yugabytedb (postgres)") && d.contains("ferretdb (mongodb)"), "{d}");
        assert!(port_text().contains("yugabytedb 5433"), "{}", port_text());
    }

    #[test]
    fn file_connections() {
        let mut p = ConnectionProfile::new("lite");
        apply(&mut p, &args(json!({"engine": "sqlite", "path": "~/data/shop.sqlite"}))).unwrap();
        assert_eq!((p.engine, p.path.as_deref(), p.host.as_str(), p.port, p.user.as_str(), p.sslmode), (Engine::Sqlite, Some("~/data/shop.sqlite"), "", 0, "", SslMode::Disable));
        let mut q = ConnectionProfile::new("duck");
        apply(&mut q, &args(json!({"connection_string": "duckdb:///w/x.duckdb?mode=ro"}))).unwrap();
        assert_eq!((q.engine, q.path.as_deref(), q.read_only), (Engine::DuckDb, Some("/w/x.duckdb"), true));
        let mut r = ConnectionProfile::new("lite");
        assert!(apply(&mut r, &args(json!({"engine": "sqlite", "path": "/x.db", "password": "pw"}))).is_err(), "no password for a file");
        let mut r = ConnectionProfile::new("lite");
        assert!(apply(&mut r, &args(json!({"engine": "sqlite", "path": "/x.db", "ssh_host": "b"}))).is_err(), "no tunnel for a file");
        let mut moved = p.clone();
        apply(&mut moved, &args(json!({"path": "/elsewhere.sqlite"}))).unwrap();
        assert!(target_changed(&p, &moved) && changed_fields(&p, &moved) == ["path"]);
        let mut back = p.clone();
        apply(&mut back, &args(json!({"engine": "postgres", "host": "h", "user": "u"}))).unwrap();
        assert_eq!(back.path, None, "a server has no file");
    }

    #[test]
    fn ssh_fields() {
        let old = base();
        let mut p = old.clone();
        apply(&mut p, &args(json!({"ssh_host": "bastion", "ssh_port": 2222, "ssh_user": "jump", "ssh_auth": "key", "ssh_key_file": "~/.ssh/k"}))).unwrap();
        let s = p.ssh.clone().unwrap();
        assert_eq!((s.host.as_str(), s.port, s.user.as_str(), s.auth, s.key_file.as_str()), ("bastion", Some(2222), "jump", SshAuth::Key, "~/.ssh/k"));
        assert!(target_changed(&old, &p) && changed_fields(&old, &p) == ["ssh"]);
        let mut q = p.clone();
        apply(&mut q, &args(json!({"ssh_host": ""}))).unwrap();
        assert_eq!(q.ssh, None);
        let mut r = old.clone();
        assert!(matches!(apply(&mut r, &args(json!({"ssh_user": "x"}))), Err(ToolError::Invalid(m)) if m.contains("ssh_host")));
    }

    #[test]
    fn nothing_loosens() {
        let mut old = base();
        (old.env, old.read_only, old.mcp) = (Environment::Staging, true, Some(McpAccess::Write));
        let try_ = |v: Value| {
            let mut p = old.clone();
            apply(&mut p, &args(v)).unwrap();
            no_loosening(&old, &p)
        };
        assert!(matches!(try_(json!({"env": "dev"})), Err(ToolError::Refused(m)) if m.contains("env would loosen")));
        assert!(matches!(try_(json!({"read_only": false})), Err(ToolError::Refused(m)) if m.contains("read_only")));
        assert!(matches!(try_(json!({"mcp": "full"})), Err(ToolError::Refused(m)) if m.contains("widen")));
        for ok in [json!({"env": "production"}), json!({"read_only": true}), json!({"mcp": "read"}), json!({"mcp": "off"}), json!({"env": "staging", "mcp": "write"})] {
            assert!(try_(ok.clone()).is_ok(), "{ok}");
        }
        let mut p = old.clone();
        apply(&mut p, &args(json!({"env": "production"}))).unwrap();
        assert_eq!(p.mcp_access(), McpAccess::Write);
    }

    #[test]
    fn cloud_sign_in_goes_to_the_clouds_own_hosts_only() {
        let mut p = base();
        apply(&mut p, &args(json!({"host": "srv.postgres.database.azure.com", "user": "ana@contoso.com", "auth": "entra-id", "auth_tenant": "contoso.onmicrosoft.com"}))).unwrap();
        assert_eq!(p.auth, Auth::EntraId { tenant: Some("contoso.onmicrosoft.com".into()) });
        assert!(cloud_target(&p).is_ok());
        assert!(target_changed(&base(), &p) && changed_fields(&base(), &p).contains(&"auth"));
        let mut evil = p.clone();
        apply(&mut evil, &args(json!({"host": "db.attacker.example"}))).unwrap();
        assert!(matches!(cloud_target(&evil), Err(ToolError::Refused(m)) if m.contains("not a known")));
        let mut tunneled = p.clone();
        apply(&mut tunneled, &args(json!({"ssh_host": "bastion.attacker.example"}))).unwrap();
        assert!(matches!(cloud_target(&tunneled), Err(ToolError::Refused(m)) if m.contains("SSH tunnel")));
        let mut google_ip = base();
        apply(&mut google_ip, &args(json!({"host": "34.1.2.3", "auth": "google-iam"}))).unwrap();
        assert!(cloud_target(&google_ip).is_err(), "a Cloud SQL IP cannot be told from anyone's");
        let mut rds = base();
        apply(&mut rds, &args(json!({"host": "db.abc.eu-west-1.rds.amazonaws.com", "auth": "aws-iam", "aws_profile": "dev", "ssh_host": "bastion"}))).unwrap();
        assert!(cloud_target(&rds).is_ok(), "an RDS token is signed for its host");
        let mut q = base();
        assert!(matches!(apply(&mut q, &args(json!({"auth": "entra-id", "password": "pw"}))), Err(ToolError::Invalid(m)) if m.contains("no password")));
        let mut q = base();
        assert!(matches!(apply(&mut q, &args(json!({"aws_profile": "dev"}))), Err(ToolError::Invalid(m)) if m.contains("aws-iam")));
        let mut q = base();
        assert!(apply(&mut q, &args(json!({"auth": "aws-iam", "aws_profile": "--endpoint-url=x"}))).is_err());
        let mut q = base();
        assert!(apply(&mut q, &args(json!({"engine": "redis", "auth": "entra-id"}))).is_err(), "not for this engine");
        let mut back = p.clone();
        apply(&mut back, &args(json!({"auth": "password"}))).unwrap();
        assert_eq!(back.auth, Auth::Password);
    }

    #[test]
    fn update_schema() {
        let t = tools();
        let u = t.iter().find(|t| t.name == "update_connection").unwrap();
        assert_eq!(u.schema.required(), ["connection"]);
        for k in ["name", "host", "port", "user", "database", "sslmode", "password", "color", "env", "read_only", "mcp", "connection_string", "ssh_host", "auth", "aws_profile"] {
            assert!(u.schema.properties().contains_key(k), "{k}");
        }
        assert!(u.schema.validate(&json!({"connection": "x", "port": 0}).as_object().unwrap().clone()).is_err());
    }
}
