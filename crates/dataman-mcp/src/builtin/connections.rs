//! create_connection and update_connection: saved connections, for every engine, by fields or a
//! connection string. Passwords are write-only: they go to the Keychain, never into
//! connections.toml, a response or the audit log. Through MCP a connection's safety settings
//! only stay or tighten (env, read_only, the MCP tier), and a saved password never follows a
//! connection to another server.

use dataman_core::{ConfigStore, ConnectionProfile, Engine, Environment, McpAccess, SshAuth, SshSettings, SslMode};
use serde_json::{Value, json};

use super::Builtin;
use crate::tool::{Args, BoxFut, Call, Schema, ToolError};

/// The fields both tools take, after their own `name` / `connection` and `host` / `user`.
fn fields(s: Schema, mcp: Value) -> Schema {
    s.prop("connection_string", false, json!({"type": "string", "description": "A full connection string (postgres://, mysql://, sqlserver:// URL, libpq key=value, ADO.NET); the fields given here override whatever it sets."}))
        .prop("engine", false, json!({"type": "string", "enum": ["postgres", "lar", "mysql", "sql-server", "mongodb", "elasticsearch", "opensearch", "redis"], "description": "lar is the postgres wire protocol (default postgres)."}))
        .prop("port", false, json!({"type": "integer", "minimum": 1, "maximum": 65535, "description": "Default: the engine's own (5432 / 3306 / 1433 / 27017 / 9200 / 6379)."}))
        .prop("password", false, json!({"type": "string", "maxLength": 1000, "description": "Saved to the Keychain; never echoed back."}))
        .prop("database", false, json!({"type": "string", "description": "Default: the user's name (postgres), empty (mysql, sql-server)."}))
        .prop("sslmode", false, json!({"type": "string", "enum": ["disable", "prefer", "require", "verify-full"], "description": "Default prefer."}))
        .prop("color", false, json!({"type": "string", "description": "A swatch name shown in the explorer and on tabs; default: picked from the id."}))
        .prop("env", false, json!({"type": "string", "enum": ["none", "dev", "staging", "production"], "description": "production asks before every write and DDL, and gets the red accent."}))
        .prop("read_only", false, json!({"type": "boolean", "description": "Sessions start with default_transaction_read_only on; Dataman also refuses writes client-side."}))
        .prop("mcp", false, mcp)
        .prop("ssh_host", false, json!({"type": "string", "description": "Reach the server through this SSH host (an ~/.ssh/config alias works); \"\" removes the tunnel."}))
        .prop("ssh_port", false, json!({"type": "integer", "minimum": 1, "maximum": 65535, "description": "SSH port; default: the alias' Port, or 22."}))
        .prop("ssh_user", false, json!({"type": "string", "description": "SSH user; default: the alias' User, or the local user."}))
        .prop("ssh_auth", false, json!({"type": "string", "enum": ["agent", "key", "password"], "description": "How the tunnel signs in (a password or passphrase is typed in Dataman)."}))
        .prop("ssh_key_file", false, json!({"type": "string", "description": "Private key for ssh_auth key; default: the alias' IdentityFile, or ~/.ssh/id_*."}))
}

pub fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "create_connection",
            description: "Saves a new database connection, for every engine Dataman speaks (postgres, mysql, sql-server, mongodb, elasticsearch, opensearch, redis; lar is a postgres connection, recognized automatically once connected). Fields directly, or connection_string (a postgres://, mysql://, sqlserver://, mongodb:// (+srv) or redis:// URL, a libpq key=value string, or an ADO.NET string); given fields win over whatever the string sets. A password goes to the macOS Keychain only, never into connections.toml, this tool's response, or a log. Refuses to replace a connection of the same name unless overwrite is true.",
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
            description: "Edits a saved connection (from list_connections): only the fields given change. A password is write-only: saved to the Keychain, never returned. env, read_only and mcp may only stay the same or tighten; moving the connection to another server (engine, host, port, user or SSH) drops its saved password unless a new one is given. Refused while the connection has a transaction open in Dataman. Returns the fields that changed.",
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

fn engine_of(s: &str) -> Result<Engine, ToolError> {
    Engine::parse(s).ok_or_else(|| ToolError::Invalid(format!("unknown engine {s:?}")))
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

/// The fields of `a` over `profile`; the password they carry (the connection string's, or
/// `password`, which wins).
fn apply(profile: &mut ConnectionProfile, a: &Args) -> Result<Option<String>, ToolError> {
    let mut password = None;
    if let Some(cs) = a.opt_str("connection_string") {
        let (p, pw) = dataman_core::connstr::parse_profile(cs).ok_or_else(|| ToolError::Invalid("could not parse connection_string".into()))?;
        profile.engine = p.engine;
        profile.host = p.host;
        profile.port = p.port;
        profile.user = p.user;
        profile.database = p.database;
        profile.sslmode = p.sslmode;
        password = pw;
    }
    if let Some(e) = a.opt_str("engine") {
        profile.engine = engine_of(e)?;
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
        profile.color = Some(c.to_string());
    }
    if let Some(e) = a.opt_str("env") {
        profile.env = env_of(e)?;
    }
    if a.get("read_only").is_some() {
        profile.read_only = a.bool("read_only");
    }
    if let Some(m) = a.opt_str("mcp") {
        // The same tier as the default stays the default (no setting written for nothing).
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
    Ok(password)
}

/// Through MCP the safety settings of an existing connection only stay or tighten.
fn no_loosening(old: &ConnectionProfile, new: &ConnectionProfile) -> Result<(), ToolError> {
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

/// The server the saved secrets were for moved: engine, host, port, user or the SSH hop.
fn target_changed(old: &ConnectionProfile, new: &ConnectionProfile) -> bool {
    new.engine != old.engine || new.host != old.host || new.port != old.port || new.user != old.user || new.ssh != old.ssh
}

/// The secrets after the change: a new password saved; else the old one kept while the target
/// stays (an old secret must not follow the connection to another server unseen), dropped
/// otherwise. What became of the password, for the response (never the password).
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

/// Saves the connection and tells the app (when it runs this call) to show it now.
fn save(cx: &Call, profile: &ConnectionProfile) -> Result<(), ToolError> {
    let store = ConfigStore::open(&cx.hub.dirs).map_err(failed)?;
    store.upsert(profile.clone()).map_err(failed)?;
    if let Some(f) = cx.hub.front() {
        f.connections_changed();
    }
    Ok(())
}

/// The names of the fields that differ (what the response lists; never a secret).
fn changed_fields(old: &ConnectionProfile, new: &ConnectionProfile) -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut f = |name: &'static str, differs: bool| {
        if differs {
            v.push(name);
        }
    };
    f("name", old.name != new.name);
    f("engine", old.engine != new.engine);
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
    v
}

fn create_connection(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let name = a.str("name")?.trim().to_string();
        if name.is_empty() {
            return Err(ToolError::Invalid("\"name\" must not be empty".into()));
        }
        if a.opt_str("connection_string").is_none() && (a.opt_str("host").is_none() || a.opt_str("user").is_none()) {
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

        // The baseline is the existing connection (so a field left out stays as it was, not
        // reset to a blank profile's defaults), or a fresh one when there is none.
        let mut profile = match existing {
            Some(p) => p.clone(),
            None => ConnectionProfile::new(name.clone()),
        };
        profile.name = name;
        let password = apply(&mut profile, &a)?;
        if let Some(old) = existing {
            no_loosening(old, &profile)?;
        }
        settle_secrets(cx, existing, &mut profile, password)?;
        save(cx, &profile)?;
        Ok(json!({"id": profile.id, "name": profile.name, "overwritten": existing.is_some()}))
    })
}

fn update_connection(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let old = cx.hub.profile(a.str("connection")?).map_err(ToolError::Invalid)?;
        cx.note_connection(&old.name);
        if old.mcp_access() == McpAccess::Off {
            return Err(ToolError::Refused("MCP access to this connection is off (connection settings › MCP); it is edited in Dataman only".into()));
        }
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
        no_loosening(&old, &profile)?;
        let changed = changed_fields(&old, &profile);
        if changed.is_empty() && password.is_none() {
            let state = if old.save_password { "kept" } else { "none" };
            return Ok(json!({"id": old.id, "name": old.name, "changed": [], "password": state}));
        }
        // The app reconnects an edited connection; a transaction open on it would end.
        if let Some(f) = cx.hub.front() {
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
        cx.note_connection(&profile.name);
        let state = settle_secrets(cx, Some(&old), &mut profile, password)?;
        save(cx, &profile)?;
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

    /// Only the given fields change; the response's list names them.
    #[test]
    fn fields_apply_and_are_listed() {
        let old = base();
        let mut p = old.clone();
        let pw = apply(&mut p, &args(json!({"host": "db2", "port": 6543, "color": "violet", "password": "s3cret"}))).unwrap();
        assert_eq!(pw.as_deref(), Some("s3cret"));
        assert_eq!((p.host.as_str(), p.port, p.color.as_deref(), p.user.as_str(), p.database.as_str()), ("db2", 6543, Some("violet"), "app", "shop"));
        assert_eq!(changed_fields(&old, &p), ["host", "port", "color"]);
        assert!(target_changed(&old, &p));
        // A connection string sets its fields; a given field wins over it.
        let mut q = old.clone();
        let pw = apply(&mut q, &args(json!({"connection_string": "postgresql://bob:pw1@h3:5440/other", "database": "mine"}))).unwrap();
        assert_eq!((q.host.as_str(), q.port, q.user.as_str(), q.database.as_str(), pw.as_deref()), ("h3", 5440, "bob", "mine", Some("pw1")));
        // The same tier as the default writes nothing.
        let mut r = old.clone();
        apply(&mut r, &args(json!({"mcp": "read"}))).unwrap();
        assert_eq!(r.mcp, None);
        assert!(changed_fields(&old, &r).is_empty());
    }

    /// SSH: a host adds the tunnel, "" removes it; its other fields need one.
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

    /// env, read_only and mcp only stay or tighten.
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
        // Tightening env to production with the tier left as it was: the explicit tier stays.
        let mut p = old.clone();
        apply(&mut p, &args(json!({"env": "production"}))).unwrap();
        assert_eq!(p.mcp_access(), McpAccess::Write);
    }

    /// The schema: connection required, password write-only (never in a response by
    /// construction: the handlers build their answers from ids, names and field names).
    #[test]
    fn update_schema() {
        let t = tools();
        let u = t.iter().find(|t| t.name == "update_connection").unwrap();
        assert_eq!(u.schema.required(), ["connection"]);
        for k in ["name", "host", "port", "user", "database", "sslmode", "password", "color", "env", "read_only", "mcp", "connection_string", "ssh_host"] {
            assert!(u.schema.properties().contains_key(k), "{k}");
        }
        assert!(u.schema.validate(&json!({"connection": "x", "port": 0}).as_object().unwrap().clone()).is_err());
    }
}
