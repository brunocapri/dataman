//! Saved connections (`~/.config/dataman/connections.toml`). Passwords never go here: they
//! live in the [`SecretStore`](crate::secret::SecretStore).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// The database engine a connection speaks to. LAR is a PostgreSQL engine (wire protocol and
/// SQL), recognized after connecting ([`crate::server`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    #[default]
    Postgres,
    #[serde(rename = "mysql")]
    MySql,
    SqlServer,
}

impl Engine {
    pub const ALL: [Engine; 3] = [Engine::Postgres, Engine::MySql, Engine::SqlServer];

    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::MySql => "MySQL",
            Engine::SqlServer => "SQL Server",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::MySql => 3306,
            Engine::SqlServer => 1433,
        }
    }

    /// The SQL dialect the engine's statements are written in.
    pub fn dialect(self) -> crate::sql::Dialect {
        crate::sql::Dialect::of(self)
    }

    pub fn is_postgres(&self) -> bool {
        *self == Engine::Postgres
    }
}

/// The connection's advanced MCP options (allowed queries, YOLO) are off for now: kept in the
/// file as set, but not shown anywhere and not applied ([`ConnectionProfile::mcp_yolo_on`],
/// [`ConnectionProfile::mcp_allowed_on`]).
pub const MCP_ADVANCED: bool = false;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    /// TLS if the server offers it, without verifying the certificate (libpq's default).
    #[default]
    Prefer,
    /// TLS required, certificate not verified (as in libpq).
    Require,
    /// TLS required, certificate verified against the system roots and the host name.
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 4] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyFull];

    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyFull => "verify-full",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    /// Left out of the file for PostgreSQL, so files of older versions load unchanged.
    #[serde(default, skip_serializing_if = "Engine::is_postgres")]
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    #[serde(default)]
    pub sslmode: SslMode,
    /// A password is stored in the keychain for this connection.
    #[serde(default)]
    pub save_password: bool,
    /// Color tag shown in the explorer and on tabs (a swatch name such as `rose`); `None`
    /// picks one from the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Reach the server through an SSH tunnel. With `save_password`, the SSH password or key
    /// passphrase is in the keychain too (account `<id>:ssh`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshSettings>,
    /// Which environment the server is: production gets the red accent and asks before every
    /// write and DDL (`guard`).
    #[serde(default, skip_serializing_if = "Environment::is_none")]
    pub env: Environment,
    /// Sessions start with `default_transaction_read_only = on` and Dataman refuses writes
    /// before they reach the server (LAR accepts READ ONLY without enforcing it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Databases whose read-only state differs from `read_only` (the default for the others),
    /// set from the explorer (a database's menu ▸ Read Only). Only differences are kept.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub read_only_databases: BTreeMap<String, bool>,
    /// What agents may do through the MCP server (`dataman-mcp`); `None` is the default for the
    /// environment ([`ConnectionProfile::mcp_access`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpAccess>,
    /// The most rows an agent's write may change before it waits for a human (`None`: the
    /// setting `mcp_write_row_limit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_row_limit: Option<u64>,
    /// Statements agents may run here without a human, whatever the tier would say (one
    /// statement each; `?` stands for one value: [`crate::guard::allowlist_match`]). The switch,
    /// the tier off and the database's read-only still refuse them. Set in the connection's
    /// MCP › Advanced, or by "Always allow" on an approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_allowed: Vec<String>,
    /// YOLO: agents run anything here without a human (destructive statements, writes past the
    /// row limit, production), in any tier but off. The switch and the database's read-only
    /// still refuse writes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mcp_yolo: bool,
}

/// The override of `database` in `overrides`: by its exact name (SQL Server's names, compared
/// without case there, also by its case-folded one).
fn override_key<'a>(overrides: &'a BTreeMap<String, bool>, engine: Engine, database: &str) -> Option<&'a String> {
    overrides
        .get_key_value(database)
        .map(|(k, _)| k)
        .or_else(|| (engine == Engine::SqlServer).then(|| overrides.keys().find(|k| k.eq_ignore_ascii_case(database))).flatten())
}

/// [`ConnectionProfile::read_only_for`] over a default and its overrides (a live connection
/// keeps its own copy of them, changed without reconnecting).
pub fn read_only_in(default: bool, overrides: &BTreeMap<String, bool>, engine: Engine, database: &str) -> bool {
    override_key(overrides, engine, database).and_then(|k| overrides.get(k)).copied().unwrap_or(default)
}

/// [`ConnectionProfile::set_read_only_for`] over a default and its overrides.
pub fn set_read_only_in(default: bool, overrides: &mut BTreeMap<String, bool>, engine: Engine, database: &str, on: bool) {
    while let Some(k) = override_key(overrides, engine, database).cloned() {
        overrides.remove(&k);
    }
    if on != default {
        overrides.insert(database.to_string(), on);
    }
}

/// What agents (MCP clients) may do on a connection: the tiers, from least to most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpAccess {
    /// Not visible to agents' tools.
    Off,
    /// Reads only: read-only transactions and the client-side guard.
    Read,
    /// Non-destructive writes (`guard::WriteClass::SafeWrite`) within the row limit; anything
    /// destructive is refused, more rows than the limit wait for a human in the app.
    Write,
    /// Anything, but destructive statements and writes over the row limit wait for a human's
    /// Allow in the app.
    Full,
}

impl McpAccess {
    pub const ALL: [McpAccess; 4] = [McpAccess::Off, McpAccess::Read, McpAccess::Write, McpAccess::Full];

    pub fn label(self) -> &'static str {
        match self {
            McpAccess::Off => "off",
            McpAccess::Read => "read",
            McpAccess::Write => "write",
            McpAccess::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<McpAccess> {
        McpAccess::ALL.into_iter().find(|a| a.label().eq_ignore_ascii_case(s.trim()))
    }

    /// The default for a connection without an explicit setting: read, off on production.
    pub fn default_for(env: Environment) -> McpAccess {
        if env.is_production() { McpAccess::Off } else { McpAccess::Read }
    }
}

/// A connection's environment tag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Environment {
    #[default]
    None,
    Dev,
    Staging,
    Production,
}

impl Environment {
    pub const ALL: [Environment; 4] = [Environment::None, Environment::Dev, Environment::Staging, Environment::Production];

    pub fn label(self) -> &'static str {
        match self {
            Environment::None => "none",
            Environment::Dev => "dev",
            Environment::Staging => "staging",
            Environment::Production => "production",
        }
    }

    pub fn is_none(&self) -> bool {
        *self == Environment::None
    }

    pub fn is_production(self) -> bool {
        self == Environment::Production
    }
}

/// How the SSH tunnel authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuth {
    /// The keys of the running ssh-agent (`SSH_AUTH_SOCK`), then the unencrypted identity files.
    Agent,
    /// A private key file, with an optional passphrase.
    Key,
    Password,
}

impl SshAuth {
    pub const ALL: [SshAuth; 3] = [SshAuth::Agent, SshAuth::Key, SshAuth::Password];

    pub fn label(self) -> &'static str {
        match self {
            SshAuth::Agent => "agent",
            SshAuth::Key => "key",
            SshAuth::Password => "password",
        }
    }
}

impl Default for SshAuth {
    /// The agent when one is running, the key file otherwise.
    fn default() -> Self {
        if std::env::var_os("SSH_AUTH_SOCK").is_some_and(|s| !s.is_empty()) { SshAuth::Agent } else { SshAuth::Key }
    }
}

/// The SSH hop in front of the database. `host` may be an alias of `~/.ssh/config`: its
/// HostName, User, Port, IdentityFile and ProxyJump apply where these fields are empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSettings {
    pub host: String,
    /// `None`: the alias' Port, or 22.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Empty: the alias' User, or the local user.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(default)]
    pub auth: SshAuth,
    /// The private key (`SshAuth::Key`); empty: the alias' IdentityFile, or `~/.ssh/id_*`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_file: String,
}

impl ConnectionProfile {
    pub fn new(name: impl Into<String>) -> Self {
        ConnectionProfile {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name: name.into(),
            engine: Engine::Postgres,
            host: "127.0.0.1".into(),
            port: 5432,
            user: "postgres".into(),
            database: "postgres".into(),
            sslmode: SslMode::Prefer,
            save_password: false,
            color: None,
            ssh: None,
            env: Environment::None,
            read_only: false,
            read_only_databases: BTreeMap::new(),
            mcp: None,
            mcp_row_limit: None,
            mcp_allowed: Vec::new(),
            mcp_yolo: false,
        }
    }

    /// Whether `database` is read-only: its own setting, else the connection's.
    pub fn read_only_for(&self, database: &str) -> bool {
        read_only_in(self.read_only, &self.read_only_databases, self.engine, database)
    }

    /// Makes `database` read-only or writable (kept as an override only where it differs
    /// from the connection's default).
    pub fn set_read_only_for(&mut self, database: &str, on: bool) {
        set_read_only_in(self.read_only, &mut self.read_only_databases, self.engine, database, on);
    }

    /// No database is writable: the default and every override are read-only (SQL Server's
    /// `ApplicationIntent=ReadOnly` is for such a connection only).
    pub fn read_only_everywhere(&self) -> bool {
        self.read_only && self.read_only_databases.values().all(|v| *v)
    }

    /// What agents may do here: the explicit setting, or the environment's default.
    pub fn mcp_access(&self) -> McpAccess {
        self.mcp.unwrap_or_else(|| McpAccess::default_for(self.env))
    }

    /// The same connection but for what agents may do on it (the MCP tier, row limit, allowed
    /// queries, YOLO): no session reads those, so a live connection stays as it is.
    pub fn same_but_agents(&self, other: &ConnectionProfile) -> bool {
        let strip = |p: &ConnectionProfile| ConnectionProfile { mcp: None, mcp_row_limit: None, mcp_allowed: Vec::new(), mcp_yolo: false, ..p.clone() };
        strip(self) == strip(other)
    }

    /// YOLO in effect: the advanced options on ([`MCP_ADVANCED`]), set, and the tier is not off
    /// (an off connection stays invisible).
    pub fn mcp_yolo_on(&self) -> bool {
        MCP_ADVANCED && self.mcp_yolo && self.mcp_access() != McpAccess::Off
    }

    /// The allowed queries in effect: none while the advanced options are off ([`MCP_ADVANCED`]).
    pub fn mcp_allowed_on(&self) -> &[String] {
        if MCP_ADVANCED { &self.mcp_allowed } else { &[] }
    }

    /// The tier as shown wherever it is: `full`, or `full · YOLO`.
    pub fn mcp_label(&self) -> String {
        let a = self.mcp_access().label();
        if self.mcp_yolo_on() { format!("{a} · YOLO") } else { a.to_string() }
    }

    /// Adds statements to the allowed queries (each checked as an entry, those already allowed
    /// by an entry left out); the ones added.
    pub fn mcp_allow<S: AsRef<str>>(&mut self, stmts: &[S]) -> Vec<String> {
        let d = self.engine.dialect();
        let mut added = Vec::new();
        for s in stmts {
            let Ok(e) = crate::guard::allowlist_entry(s.as_ref(), d) else { continue };
            if !self.mcp_allowed.iter().any(|a| crate::guard::allowlist_match(a, &e, d)) {
                self.mcp_allowed.push(e.clone());
                added.push(e);
            }
        }
        added
    }

    pub fn summary(&self) -> String {
        format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }

    /// The host is a Unix-socket directory (`/tmp`, `/var/run/postgresql`), as in libpq.
    pub fn is_socket(&self) -> bool {
        self.host.starts_with('/')
    }

    /// The keychain account of the SSH password or key passphrase.
    pub fn ssh_secret_id(&self) -> String {
        format!("{}:ssh", self.id)
    }

    /// `postgresql://user@host:port/db?sslmode=…`: the connection as a libpq URL. Never with the
    /// password (it lives in the keychain); user and database are percent-encoded, an IPv6 host
    /// goes in brackets, a socket directory is percent-encoded (`%2Ftmp`), as libpq reads it.
    pub fn url(&self) -> String {
        let host = if self.is_socket() {
            percent_encode(&self.host)
        } else if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.engine == Engine::MySql {
            // MySQL Shell's form, with its own TLS mode names.
            let ssl = match self.sslmode {
                SslMode::Disable => "DISABLED",
                SslMode::Prefer => "PREFERRED",
                SslMode::Require => "REQUIRED",
                SslMode::VerifyFull => "VERIFY_IDENTITY",
            };
            return format!("mysql://{}@{host}:{}/{}?ssl-mode={ssl}", percent_encode(&self.user), self.port, percent_encode(&self.database));
        }
        if self.engine == Engine::SqlServer {
            // JDBC's form (Prisma's too): the encryption in SQL Server's words.
            let enc = match self.sslmode {
                SslMode::Disable => "encrypt=false",
                SslMode::Prefer => "encrypt=optional;trustServerCertificate=true",
                SslMode::Require => "encrypt=true;trustServerCertificate=true",
                SslMode::VerifyFull => "encrypt=true",
            };
            let db = if self.database.is_empty() { String::new() } else { format!(";database={}", self.database) };
            return format!("sqlserver://{}:{}{db};user={};{enc}", self.host, self.port, self.user);
        }
        format!(
            "postgresql://{}@{host}:{}/{}?sslmode={}",
            percent_encode(&self.user),
            self.port,
            percent_encode(&self.database),
            self.sslmode.label()
        )
    }
}

/// RFC 3986 percent-encoding of everything but the unreserved characters.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "connection")]
    connections: Vec<ConnectionProfile>,
}

/// Where Dataman keeps its files.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub config: PathBuf,
}

impl Dirs {
    /// `$XDG_CONFIG_HOME/dataman`, or `~/.config/dataman`.
    pub fn default_dirs() -> Result<Dirs> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(x) => PathBuf::from(x),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
        };
        Ok(Dirs { config: base.join("dataman") })
    }

    pub fn at(path: impl Into<PathBuf>) -> Dirs {
        Dirs { config: path.into() }
    }

    pub fn connections_file(&self) -> PathBuf {
        self.config.join("connections.toml")
    }

    pub fn history_dir(&self) -> PathBuf {
        self.config.join("history")
    }

    /// Favorite queries, shared with `dataman-mcp`.
    pub fn favorites_file(&self) -> PathBuf {
        self.config.join("favorites.toml")
    }

    /// The explorer's pinned tables (`pins`); only there while something is pinned.
    pub fn pins_file(&self) -> PathBuf {
        self.config.join("pins.toml")
    }

    /// The workspace kept between launches (`session`).
    pub fn session_dir(&self) -> PathBuf {
        self.config.join("session")
    }

    /// SSH host keys the user trusted in Dataman (read after `~/.ssh/known_hosts`).
    pub fn known_hosts_file(&self) -> PathBuf {
        self.config.join("known_hosts")
    }

    /// The MCP server's user-defined tools.
    pub fn mcp_tools_file(&self) -> PathBuf {
        self.config.join("mcp-tools.toml")
    }

    /// The MCP server's audit log (one JSON object per tool call).
    pub fn mcp_audit_file(&self) -> PathBuf {
        self.config.join("mcp-audit.jsonl")
    }
}

/// The saved connections. Other processes write the file too (`dataman-mcp` without the app, a
/// hand edit): [`ConfigStore::refresh`] reads it again when its modification time moved, and
/// every change here starts from the file as it is on disk, so nobody's write is lost.
pub struct ConfigStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    connections: Vec<ConnectionProfile>,
    /// The file's modification time as last read or written (`None`: no file).
    stamp: Option<std::time::SystemTime>,
}

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn read(path: &Path) -> Result<Vec<ConnectionProfile>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(toml::from_str::<File>(&s).with_context(|| format!("reading {}", path.display()))?.connections),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

impl ConfigStore {
    pub fn open(dirs: &Dirs) -> Result<ConfigStore> {
        let path = dirs.connections_file();
        let stamp = mtime(&path);
        let connections = read(&path)?;
        Ok(ConfigStore { path, inner: Mutex::new(Inner { connections, stamp }) })
    }

    pub fn connections(&self) -> Vec<ConnectionProfile> {
        self.inner.lock().connections.clone()
    }

    pub fn get(&self, id: &str) -> Option<ConnectionProfile> {
        self.inner.lock().connections.iter().find(|c| c.id == id).cloned()
    }

    /// Reads the file again when it changed on disk since it was last read or written here;
    /// true when the connections differ from before. A file that does not parse (a hand edit
    /// half done) keeps the connections as they were.
    pub fn refresh(&self) -> bool {
        let mut g = self.inner.lock();
        Self::sync(&self.path, &mut g)
    }

    fn sync(path: &Path, g: &mut Inner) -> bool {
        let stamp = mtime(path);
        if stamp == g.stamp {
            return false;
        }
        g.stamp = stamp;
        match read(path) {
            Ok(c) if c != g.connections => {
                g.connections = c;
                true
            }
            Ok(_) => false,
            Err(e) => {
                log::warn!("{e:#}");
                false
            }
        }
    }

    /// A change made on the file as it is now, saved.
    fn change(&self, f: impl FnOnce(&mut Vec<ConnectionProfile>)) -> Result<()> {
        let mut g = self.inner.lock();
        Self::sync(&self.path, &mut g);
        f(&mut g.connections);
        save(&self.path, &g.connections)?;
        g.stamp = mtime(&self.path);
        Ok(())
    }

    /// Adds or replaces (by id) a connection and saves the file.
    pub fn upsert(&self, profile: ConnectionProfile) -> Result<()> {
        self.change(|g| match g.iter_mut().find(|c| c.id == profile.id) {
            Some(c) => *c = profile,
            None => g.push(profile),
        })
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.change(|g| g.retain(|c| c.id != id))
    }

    /// Puts the connections in the order of `ids` (the explorer's, the file's); any it does not
    /// name keep their order after them.
    pub fn reorder(&self, ids: &[String]) -> Result<()> {
        self.change(|g| g.sort_by_key(|c| ids.iter().position(|i| *i == c.id).unwrap_or(usize::MAX)))
    }
}

/// Writes atomically: a temporary file in the same directory, then `rename`.
fn save(path: &Path, connections: &[ConnectionProfile]) -> Result<()> {
    let dir = path.parent().context("config path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(&File { connections: connections.to_vec() })?;
    let tmp = dir.join(format!(".connections.toml.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_without_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        assert!(store.connections().is_empty());
        let mut p = ConnectionProfile::new("local pg");
        p.port = 55432;
        p.save_password = true;
        store.upsert(p.clone()).unwrap();
        let mut q = ConnectionProfile::new("lar");
        q.sslmode = SslMode::Disable;
        store.upsert(q.clone()).unwrap();
        p.name = "renamed".into();
        store.upsert(p.clone()).unwrap();

        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(!text.lines().any(|l| l.trim_start().starts_with("password")), "{text}");
        let again = ConfigStore::open(&dirs).unwrap();
        assert_eq!(again.connections(), vec![p.clone(), q.clone()]);
        again.remove(&p.id).unwrap();
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![q]);
    }

    /// Another process writes the file (`dataman-mcp` without the app): `refresh` picks it up,
    /// once; a change made here afterwards keeps it; a broken file keeps what was there.
    #[test]
    fn refresh_follows_other_writers() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let app = ConfigStore::open(&dirs).unwrap();
        assert!(!app.refresh(), "no file, nothing changed");
        let a = ConnectionProfile::new("a");
        app.upsert(a.clone()).unwrap();
        assert!(!app.refresh(), "its own write is not a change");
        // The other writer, with a store of its own.
        let other = ConfigStore::open(&dirs).unwrap();
        let b = ConnectionProfile::new("b");
        other.upsert(b.clone()).unwrap();
        assert!(app.refresh());
        assert_eq!(app.connections(), vec![a.clone(), b.clone()]);
        assert!(!app.refresh());
        // A write the app has not read yet survives the app's next change.
        let mut b2 = b.clone();
        b2.name = "b renamed".into();
        other.upsert(b2.clone()).unwrap();
        let c = ConnectionProfile::new("c");
        app.upsert(c.clone()).unwrap();
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![a.clone(), b2.clone(), c.clone()]);
        assert_eq!(app.connections(), vec![a.clone(), b2.clone(), c.clone()]);
        // Half a hand edit: the connections stay.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(dirs.connections_file(), "[[connection]]\nid = ").unwrap();
        assert!(!app.refresh());
        assert_eq!(app.connections(), vec![a, b2, c]);
    }

    #[test]
    fn reorder_is_the_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let (a, b, c) = (ConnectionProfile::new("a"), ConnectionProfile::new("b"), ConnectionProfile::new("c"));
        for p in [&a, &b, &c] {
            store.upsert(p.clone()).unwrap();
        }
        // `a` is not named: it keeps its place after the ones that are.
        store.reorder(&[c.id.clone(), b.id.clone()]).unwrap();
        assert_eq!(store.connections(), vec![c.clone(), b.clone(), a.clone()]);
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![c, b, a]);
    }

    #[test]
    fn url_never_has_the_password() {
        let mut p = ConnectionProfile::new("prod");
        p.host = "db.prod.internal".into();
        p.port = 6543;
        p.user = "bruno".into();
        p.database = "app_db".into();
        p.sslmode = SslMode::Require;
        p.save_password = true;
        assert_eq!(p.url(), "postgresql://bruno@db.prod.internal:6543/app_db?sslmode=require");
        p.user = "a@b:c".into();
        p.database = "my db/é".into();
        p.host = "::1".into();
        assert_eq!(p.url(), "postgresql://a%40b%3Ac@[::1]:6543/my%20db%2F%C3%A9?sslmode=require");
    }

    /// SSH settings round-trip; a file without them (older versions) loads with `ssh = None`.
    #[test]
    fn ssh_settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let mut p = ConnectionProfile::new("via bastion");
        p.ssh = Some(SshSettings { host: "bastion".into(), port: Some(2222), user: "jump".into(), auth: SshAuth::Key, key_file: "~/.ssh/k".into() });
        let mut q = ConnectionProfile::new("agent");
        q.ssh = Some(SshSettings { host: "gw".into(), auth: SshAuth::Agent, ..Default::default() });
        store.upsert(p.clone()).unwrap();
        store.upsert(q.clone()).unwrap();
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("auth = \"key\"") && text.contains("port = 2222"), "{text}");
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![p, q]);
        std::fs::write(dirs.connections_file(), "[[connection]]\nid = \"a\"\nname = \"n\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n").unwrap();
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections()[0].ssh, None);
    }

    /// The environment tag and the read-only flag round-trip; left out of the file when unset,
    /// so older files load as `none` / writable.
    #[test]
    fn environment_and_read_only_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let mut p = ConnectionProfile::new("prod");
        p.env = Environment::Production;
        p.read_only = true;
        let q = ConnectionProfile::new("plain");
        store.upsert(p.clone()).unwrap();
        store.upsert(q.clone()).unwrap();
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("env = \"production\"") && text.contains("read_only = true"), "{text}");
        assert_eq!(text.matches("env =").count(), 1, "unset tags stay out of the file");
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![p, q]);
    }

    /// Per-database read-only: the connection's flag is the default, a database's own setting
    /// wins; only differences are kept; older files (none) load unchanged and files without
    /// overrides do not gain the key.
    #[test]
    fn read_only_per_database() {
        let mut p = ConnectionProfile::new("pg");
        assert!(!p.read_only_for("shop"));
        p.set_read_only_for("shop", true);
        assert!(p.read_only_for("shop") && !p.read_only_for("other"));
        assert_eq!(p.read_only_databases, BTreeMap::from([("shop".to_string(), true)]));
        // Back to the default: the override goes.
        p.set_read_only_for("shop", false);
        assert!(p.read_only_databases.is_empty());
        // A read-only connection with one writable database.
        p.read_only = true;
        p.set_read_only_for("scratch", false);
        assert!(p.read_only_for("shop") && !p.read_only_for("scratch"));
        assert!(!p.read_only_everywhere());
        p.set_read_only_for("scratch", true);
        assert!(p.read_only_everywhere() && p.read_only_databases.is_empty());
        // PostgreSQL names keep their case; SQL Server's do not.
        p.read_only = false;
        p.set_read_only_for("Shop", true);
        assert!(!p.read_only_for("shop"));
        p.engine = Engine::SqlServer;
        assert!(p.read_only_for("SHOP"));
        p.set_read_only_for("shop", false);
        assert!(p.read_only_databases.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let mut a = ConnectionProfile::new("a");
        a.read_only = true;
        a.set_read_only_for("scratch", false);
        a.set_read_only_for("my db", false);
        let b = ConnectionProfile::new("b");
        store.upsert(a.clone()).unwrap();
        store.upsert(b.clone()).unwrap();
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert_eq!(text.matches("read_only_databases").count(), 1, "{text}");
        assert!(text.contains("scratch = false") && text.contains("\"my db\" = false"), "{text}");
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![a, b]);
        std::fs::write(dirs.connections_file(), "[[connection]]\nid = \"a\"\nname = \"n\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\nread_only = true\n").unwrap();
        let old = &ConfigStore::open(&dirs).unwrap().connections()[0];
        assert!(old.read_only_databases.is_empty() && old.read_only_for("d") && old.read_only_for("other"));
    }

    /// The MCP permission: absent means read (off on production); an explicit one round-trips.
    #[test]
    fn mcp_access_defaults_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let mut p = ConnectionProfile::new("dev");
        assert_eq!(p.mcp_access(), McpAccess::Read);
        p.env = Environment::Production;
        assert_eq!(p.mcp_access(), McpAccess::Off);
        p.mcp = Some(McpAccess::Write);
        assert_eq!(p.mcp_access(), McpAccess::Write);
        let q = ConnectionProfile::new("plain");
        store.upsert(p.clone()).unwrap();
        store.upsert(q.clone()).unwrap();
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("mcp = \"write\""), "{text}");
        assert_eq!(text.matches("mcp =").count(), 1, "unset stays out of the file");
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![p, q]);
    }

    /// The tiers: a file from before them keeps its meaning (write is now the non-destructive
    /// tier; nothing becomes full), full and the row limit round-trip, an unknown tier fails the
    /// file instead of granting anything.
    #[test]
    fn mcp_tiers_migration() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        let conn = |id: &str, extra: &str| format!("[[connection]]\nid = \"{id}\"\nname = \"{id}\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n{extra}\n");
        let old = [conn("w", "mcp = \"write\""), conn("r", "mcp = \"read\""), conn("o", "mcp = \"off\""), conn("p", "env = \"production\""), conn("n", "")].concat();
        std::fs::write(dirs.connections_file(), old).unwrap();
        let got: Vec<(String, McpAccess, Option<u64>)> =
            ConfigStore::open(&dirs).unwrap().connections().into_iter().map(|p| (p.id.clone(), p.mcp_access(), p.mcp_row_limit)).collect();
        assert_eq!(
            got,
            vec![
                ("w".into(), McpAccess::Write, None),
                ("r".into(), McpAccess::Read, None),
                ("o".into(), McpAccess::Off, None),
                ("p".into(), McpAccess::Off, None),
                ("n".into(), McpAccess::Read, None),
            ]
        );
        let store = ConfigStore::open(&dirs).unwrap();
        let mut f = ConnectionProfile::new("f");
        (f.mcp, f.mcp_row_limit) = (Some(McpAccess::Full), Some(25));
        store.upsert(f.clone()).unwrap();
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("mcp = \"full\"") && text.contains("mcp_row_limit = 25"), "{text}");
        assert!(ConfigStore::open(&dirs).unwrap().connections().contains(&f));
        assert_eq!((McpAccess::parse("Full"), McpAccess::parse("admin")), (Some(McpAccess::Full), None));
        assert!(McpAccess::Off < McpAccess::Read && McpAccess::Read < McpAccess::Write && McpAccess::Write < McpAccess::Full);
        std::fs::write(dirs.connections_file(), conn("x", "mcp = \"admin\"")).unwrap();
        assert!(ConfigStore::open(&dirs).is_err() || ConfigStore::open(&dirs).unwrap().connections().iter().all(|p| p.mcp_access() <= McpAccess::Read));
    }

    /// The advanced MCP options: allowed queries and YOLO round-trip, stay out of the file when
    /// unset (older files load without them), and YOLO is not in effect on an off connection.
    #[test]
    fn mcp_allowed_and_yolo_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let mut p = ConnectionProfile::new("staging");
        p.env = Environment::Staging;
        assert_eq!(p.mcp_allow(&["TRUNCATE audit_tmp;", "DELETE FROM sessions WHERE expires_at < ?", "BEGIN", "a; b"]), ["TRUNCATE audit_tmp", "DELETE FROM sessions WHERE expires_at < ?"]);
        // Already allowed (by the pattern, or the same statement written otherwise): not added again.
        assert!(p.mcp_allow(&["delete from sessions where expires_at < 5", "truncate AUDIT_TMP"]).is_empty());
        p.mcp_yolo = true;
        assert_eq!(p.mcp_yolo_on(), MCP_ADVANCED, "in effect only with the advanced options on");
        assert_eq!(p.mcp_label(), if MCP_ADVANCED { "read · YOLO" } else { "read" });
        let q = ConnectionProfile::new("plain");
        store.upsert(p.clone()).unwrap();
        store.upsert(q.clone()).unwrap();
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("mcp_yolo = true") && text.contains("TRUNCATE audit_tmp"), "{text}");
        assert_eq!((text.matches("mcp_yolo").count(), text.matches("mcp_allowed").count()), (1, 1), "unset stays out of the file: {text}");
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![p.clone(), q]);
        std::fs::write(dirs.connections_file(), "[[connection]]\nid = \"a\"\nname = \"n\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n").unwrap();
        let old = &ConfigStore::open(&dirs).unwrap().connections()[0];
        assert!(old.mcp_allowed.is_empty() && !old.mcp_yolo && old.mcp_label() == "read");
        // Off wins: YOLO stays set but is not in effect.
        p.mcp = Some(McpAccess::Off);
        assert!(p.mcp_yolo && !p.mcp_yolo_on());
        assert_eq!(p.mcp_label(), "off");
    }

    /// The engine: PostgreSQL stays out of the file (older files load as PostgreSQL), the others
    /// round-trip by name.
    #[test]
    fn engine_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let pg = ConnectionProfile::new("pg");
        let mut my = ConnectionProfile::new("my");
        my.engine = Engine::MySql;
        let mut ms = ConnectionProfile::new("ms");
        ms.engine = Engine::SqlServer;
        for p in [&pg, &my, &ms] {
            store.upsert(p.clone()).unwrap();
        }
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("engine = \"mysql\"") && text.contains("engine = \"sql-server\""), "{text}");
        assert_eq!(text.matches("engine =").count(), 2, "PostgreSQL stays out of the file");
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![pg, my, ms]);
        std::fs::write(dirs.connections_file(), "[[connection]]\nid = \"a\"\nname = \"n\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n").unwrap();
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections()[0].engine, Engine::Postgres);
        assert_eq!(Engine::ALL.map(Engine::default_port), [5432, 3306, 1433]);
    }

    /// Files written by older versions load: keys Dataman no longer knows (`metrics_url`, from
    /// the engine health panel) are ignored and dropped on the next save.
    #[test]
    fn old_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        let old = "[[connection]]\nid = \"a1\"\nname = \"lar\"\nhost = \"127.0.0.1\"\nport = 5490\nuser = \"lar\"\ndatabase = \"lar\"\nmetrics_url = \"http://127.0.0.1:9187/metrics\"\n";
        std::fs::write(dirs.connections_file(), old).unwrap();
        let store = ConfigStore::open(&dirs).unwrap();
        let c = store.connections();
        assert_eq!((c.len(), c[0].name.as_str(), c[0].port), (1, "lar", 5490));
        store.upsert(c[0].clone()).unwrap();
        assert!(!std::fs::read_to_string(dirs.connections_file()).unwrap().contains("metrics_url"));
    }
}
