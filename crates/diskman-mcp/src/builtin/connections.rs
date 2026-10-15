//! create_connection: a saved connection, for every engine, by fields or a connection string.

use diskman_core::{ConfigStore, ConnectionProfile, Engine, Environment, McpAccess, SslMode};
use serde_json::{Value, json};

use super::Builtin;
use crate::tool::{Args, BoxFut, Call, Schema, ToolError};

pub fn tools() -> Vec<Builtin> {
    vec![Builtin {
        name: "create_connection",
        description: "Saves a new database connection, for every engine Diskman speaks (postgres, mysql, sql-server; lar is a postgres connection, recognized automatically once connected). Fields directly, or connection_string (a postgres://, mysql:// or sqlserver:// URL, a libpq key=value string, or an ADO.NET string); given fields win over whatever the string sets. A password goes to the macOS Keychain only, never into connections.toml, this tool's response, or a log. Refuses to replace a connection of the same name unless overwrite is true.",
        schema: Schema::new()
            .prop("name", true, json!({"type": "string", "maxLength": 200, "description": "A short label; must be unique (case-insensitive) unless overwrite is true."}))
            .prop("connection_string", false, json!({"type": "string", "description": "A full connection string; the fields below override whatever it sets."}))
            .prop("engine", false, json!({"type": "string", "enum": ["postgres", "lar", "mysql", "sql-server"], "description": "lar is the postgres wire protocol (default postgres)."}))
            .prop("host", false, json!({"type": "string", "description": "Host, or a Unix-socket directory (starts with /). Required unless connection_string gives one."}))
            .prop("port", false, json!({"type": "integer", "minimum": 1, "maximum": 65535, "description": "Default: the engine's own (5432 / 3306 / 1433)."}))
            .prop("user", false, json!({"type": "string", "description": "Required unless connection_string gives one."}))
            .prop("password", false, json!({"type": "string", "maxLength": 1000, "description": "Saved to the Keychain; never echoed back."}))
            .prop("database", false, json!({"type": "string", "description": "Default: the user's name (postgres), empty (mysql, sql-server)."}))
            .prop("sslmode", false, json!({"type": "string", "enum": ["disable", "prefer", "require", "verify-full"], "description": "Default prefer."}))
            .prop("color", false, json!({"type": "string", "description": "A swatch name shown in the explorer and on tabs; default: picked from the id."}))
            .prop("env", false, json!({"type": "string", "enum": ["none", "dev", "staging", "production"], "description": "production asks before every write and DDL, and gets the red accent."}))
            .prop("read_only", false, json!({"type": "boolean", "description": "Sessions start with default_transaction_read_only on; Diskman also refuses writes client-side."}))
            .prop("mcp", false, json!({"type": "string", "enum": ["off", "read", "write"], "description": "What agents may do here; default: read, off on production."}))
            .prop("overwrite", false, json!({"type": "boolean", "description": "Replace the existing connection of the same name (keeping its id and any saved password) instead of refusing."})),
        need: None,
        run: create_connection,
    }]
}

fn engine_of(s: &str) -> Result<Engine, ToolError> {
    match s {
        "postgres" | "lar" => Ok(Engine::Postgres),
        "mysql" => Ok(Engine::MySql),
        "sql-server" => Ok(Engine::SqlServer),
        other => Err(ToolError::Invalid(format!("unknown engine {other:?}"))),
    }
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

        // The baseline is the existing connection (so a field left out stays as it was, not
        // reset to a blank profile's defaults), or a fresh one when there is none.
        let mut profile = match existing {
            Some(p) => p.clone(),
            None => ConnectionProfile::new(name.clone()),
        };
        profile.name = name;
        let (had_secret, old_access, old_env, old_read_only) =
            match existing { Some(p) => (p.save_password, p.mcp_access(), p.env, p.read_only), None => (false, McpAccess::Off, Environment::None, false) };
        let (old_engine, old_host, old_port, old_user, old_ssh) =
            match existing { Some(p) => (p.engine, p.host.clone(), p.port, p.user.clone(), p.ssh.clone()), None => Default::default() };

        let mut password = None;
        if let Some(cs) = a.opt_str("connection_string") {
            let (p, pw) = diskman_core::connstr::parse_profile(cs).ok_or_else(|| ToolError::Invalid("could not parse connection_string".into()))?;
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
            profile.mcp = Some(mcp_of(m)?);
        }
        if let Some(pw) = a.opt_str("password") {
            password = Some(pw.to_string());
        }

        if existing.is_some() {
            if env_rank(profile.env) < env_rank(old_env) {
                return Err(ToolError::Refused(format!("env would loosen from {:?} to {:?}; through MCP it may only stay the same or tighten", old_env.label(), profile.env.label())));
            }
            if old_read_only && !profile.read_only {
                return Err(ToolError::Refused("read_only would turn off; through MCP it may only stay the same or turn on".into()));
            }
            if profile.mcp_access() > old_access {
                return Err(ToolError::Refused(format!(
                    "mcp would widen from {:?} to {:?}; through MCP it may only stay the same or narrow (write is granted in the app)",
                    old_access.label(),
                    profile.mcp_access().label()
                )));
            }
        }

        // A password kept from before is only safe to keep pointed at the same server: if the
        // target moved, an old secret would otherwise follow it there unseen.
        let target_changed = existing.is_some_and(|_| profile.engine != old_engine || profile.host != old_host || profile.port != old_port || profile.user != old_user || profile.ssh != old_ssh);
        if let Some(pw) = &password {
            profile.save_password = true;
            cx.hub.core.secrets.set(&profile.id, pw).map_err(failed)?;
        } else if had_secret && !target_changed {
            profile.save_password = true;
        } else {
            profile.save_password = false;
            if had_secret {
                let _ = cx.hub.core.secrets.delete(&profile.id);
            }
        }
        if target_changed {
            let _ = cx.hub.core.secrets.delete(&profile.ssh_secret_id());
        }

        let store = ConfigStore::open(&cx.hub.dirs).map_err(failed)?;
        store.upsert(profile.clone()).map_err(failed)?;
        Ok(json!({"id": profile.id, "name": profile.name, "overwritten": existing.is_some()}))
    })
}
