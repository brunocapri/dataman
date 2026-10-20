//! Saved connections (`~/.config/dataman/connections.toml`). Passwords never go here: they
//! live in the [`SecretStore`](crate::secret::SecretStore).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    #[default]
    Postgres,
    #[serde(rename = "mysql")]
    MySql,
    SqlServer,
    #[serde(rename = "mongodb")]
    MongoDb,
    Elasticsearch,
    #[serde(rename = "opensearch")]
    OpenSearch,
    Redis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    Sql,
    Document,
    KeyValue,
}

impl Engine {
    pub const ALL: [Engine; 7] =
        [Engine::Postgres, Engine::MySql, Engine::SqlServer, Engine::MongoDb, Engine::Elasticsearch, Engine::OpenSearch, Engine::Redis];

    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::MySql => "MySQL",
            Engine::SqlServer => "SQL Server",
            Engine::MongoDb => "MongoDB",
            Engine::Elasticsearch => "Elasticsearch",
            Engine::OpenSearch => "OpenSearch",
            Engine::Redis => "Redis",
        }
    }

    /// Stored in the file and used by the MCP tools and the logos: must not change.
    pub fn slug(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::MySql => "mysql",
            Engine::SqlServer => "sql-server",
            Engine::MongoDb => "mongodb",
            Engine::Elasticsearch => "elasticsearch",
            Engine::OpenSearch => "opensearch",
            Engine::Redis => "redis",
        }
    }

    pub fn parse(s: &str) -> Option<Engine> {
        let s = s.trim().to_ascii_lowercase();
        let alias = match s.as_str() {
            "pg" | "postgresql" | "lar" => Some(Engine::Postgres),
            "mariadb" => Some(Engine::MySql),
            "mssql" | "sqlserver" | "sql_server" => Some(Engine::SqlServer),
            "mongo" => Some(Engine::MongoDb),
            "es" | "elastic" => Some(Engine::Elasticsearch),
            "os" => Some(Engine::OpenSearch),
            _ => None,
        };
        alias.or_else(|| Engine::ALL.into_iter().find(|e| e.slug() == s || e.label().eq_ignore_ascii_case(&s)))
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::MySql => 3306,
            Engine::SqlServer => 1433,
            Engine::MongoDb => 27017,
            Engine::Elasticsearch | Engine::OpenSearch => 9200,
            Engine::Redis => 6379,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::MySql => "root",
            Engine::SqlServer => "sa",
            Engine::Elasticsearch => "elastic",
            Engine::OpenSearch => "admin",
            Engine::MongoDb | Engine::Redis => "",
        }
    }

    pub fn family(self) -> Family {
        match self {
            Engine::Postgres | Engine::MySql | Engine::SqlServer => Family::Sql,
            Engine::MongoDb | Engine::Elasticsearch | Engine::OpenSearch => Family::Document,
            Engine::Redis => Family::KeyValue,
        }
    }

    pub fn is_sql(self) -> bool {
        self.family() == Family::Sql
    }

    pub fn dialect(self) -> crate::sql::Dialect {
        crate::sql::Dialect::of(self)
    }

    pub fn is_postgres(&self) -> bool {
        *self == Engine::Postgres
    }
}

pub const MCP_ADVANCED: bool = false;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyFull,
}

/// This machine: a socket, a loopback address or a `localhost` name.
pub fn is_local_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    h.is_empty()
        || h.starts_with('/')
        || h == "localhost"
        || h.ends_with(".localhost")
        || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// What a new connection starts with until the user picks: opportunistic (`prefer`, which is
/// plain text for the document engines) on this machine or through an SSH tunnel (already
/// encrypted up to the bastion), TLS required anywhere else.
pub fn default_sslmode(engine: Engine, host: &str, tunnel: bool) -> SslMode {
    match (tunnel || is_local_host(host), engine.is_sql()) {
        (false, _) => SslMode::Require,
        (true, true) => SslMode::Prefer,
        (true, false) => SslMode::Disable,
    }
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
    #[serde(default)]
    pub save_password: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshSettings>,
    #[serde(default, skip_serializing_if = "Environment::is_none")]
    pub env: Environment,
    /// Dataman refuses writes before they reach the server: LAR accepts READ ONLY without
    /// enforcing it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub read_only_databases: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpAccess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_row_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_allowed: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mcp_yolo: bool,
    /// Privacy and Permissions for agents (the Agents area); an older Dataman ignores them.
    #[serde(default, skip_serializing_if = "crate::privacy::AgentRules::is_default")]
    pub mcp_rules: crate::privacy::AgentRules,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    /// Made by an agent (create_connection): its host is the agent's choice, so a `~/.pgpass`
    /// line that matches any host is not sent there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_made: bool,
}

fn override_key<'a>(overrides: &'a BTreeMap<String, bool>, engine: Engine, database: &str) -> Option<&'a String> {
    overrides
        .get_key_value(database)
        .map(|(k, _)| k)
        .or_else(|| (engine == Engine::SqlServer).then(|| overrides.keys().find(|k| k.eq_ignore_ascii_case(database))).flatten())
}

pub fn read_only_in(default: bool, overrides: &BTreeMap<String, bool>, engine: Engine, database: &str) -> bool {
    override_key(overrides, engine, database).and_then(|k| overrides.get(k)).copied().unwrap_or(default)
}

pub fn set_read_only_in(default: bool, overrides: &mut BTreeMap<String, bool>, engine: Engine, database: &str, on: bool) {
    while let Some(k) = override_key(overrides, engine, database).cloned() {
        overrides.remove(&k);
    }
    if on != default {
        overrides.insert(database.to_string(), on);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpAccess {
    Off,
    Read,
    Write,
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

    pub fn default_for(env: Environment) -> McpAccess {
        if env.is_production() { McpAccess::Off } else { McpAccess::Read }
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuth {
    Agent,
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
    fn default() -> Self {
        if std::env::var_os("SSH_AUTH_SOCK").is_some_and(|s| !s.is_empty()) { SshAuth::Agent } else { SshAuth::Key }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSettings {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(default)]
    pub auth: SshAuth,
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
            mcp_rules: Default::default(),
            options: BTreeMap::new(),
            agent_made: false,
        }
    }

    pub fn read_only_for(&self, database: &str) -> bool {
        read_only_in(self.read_only, &self.read_only_databases, self.engine, database)
    }

    pub fn set_read_only_for(&mut self, database: &str, on: bool) {
        set_read_only_in(self.read_only, &mut self.read_only_databases, self.engine, database, on);
    }

    pub fn read_only_everywhere(&self) -> bool {
        self.read_only && self.read_only_databases.values().all(|v| *v)
    }

    pub fn mcp_access(&self) -> McpAccess {
        self.mcp.unwrap_or_else(|| McpAccess::default_for(self.env))
    }

    pub fn same_but_agents(&self, other: &ConnectionProfile) -> bool {
        let strip = |p: &ConnectionProfile| ConnectionProfile { mcp: None, mcp_row_limit: None, mcp_allowed: Vec::new(), mcp_yolo: false, mcp_rules: Default::default(), ..p.clone() };
        strip(self) == strip(other)
    }

    pub fn mcp_yolo_on(&self) -> bool {
        MCP_ADVANCED && self.mcp_yolo && self.mcp_access() != McpAccess::Off
    }

    pub fn mcp_allowed_on(&self) -> &[String] {
        if MCP_ADVANCED { &self.mcp_allowed } else { &[] }
    }

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

    pub fn is_socket(&self) -> bool {
        self.host.starts_with('/')
    }

    pub fn ssh_secret_id(&self) -> String {
        format!("{}:ssh", self.id)
    }

    /// Never with the password (it lives in the keychain).
    pub fn url(&self) -> String {
        let host = if self.is_socket() {
            percent_encode(&self.host)
        } else if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.engine == Engine::MySql {
            let ssl = match self.sslmode {
                SslMode::Disable => "DISABLED",
                SslMode::Prefer => "PREFERRED",
                SslMode::Require => "REQUIRED",
                SslMode::VerifyFull => "VERIFY_IDENTITY",
            };
            return format!("mysql://{}@{host}:{}/{}?ssl-mode={ssl}", percent_encode(&self.user), self.port, percent_encode(&self.database));
        }
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", percent_encode(&self.user)) };
        match self.engine {
            Engine::MongoDb => {
                let srv = self.options.get("srv").is_some_and(|v| v == "true");
                let mut q: Vec<String> = self.options.iter().filter(|(k, _)| *k != "srv").map(|(k, v)| format!("{k}={}", percent_encode(v))).collect();
                match self.sslmode {
                    SslMode::Require => q.push("tls=true&tlsAllowInvalidCertificates=true".into()),
                    SslMode::VerifyFull if !srv => q.push("tls=true".into()),
                    SslMode::Disable | SslMode::Prefer if srv => q.push("tls=false".into()),
                    _ => {}
                }
                let q = if q.is_empty() { String::new() } else { format!("?{}", q.join("&")) };
                let hostport = if srv || self.host.contains(',') { self.host.clone() } else { format!("{host}:{}", self.port) };
                let scheme = if srv { "mongodb+srv" } else { "mongodb" };
                return format!("{scheme}://{user}{hostport}/{}{q}", percent_encode(&self.database));
            }
            Engine::Redis => {
                let scheme = if matches!(self.sslmode, SslMode::Require | SslMode::VerifyFull) { "rediss" } else { "redis" };
                let db = if self.database.is_empty() { String::new() } else { format!("/{}", self.database) };
                return format!("{scheme}://{user}{host}:{}{db}", self.port);
            }
            Engine::Elasticsearch | Engine::OpenSearch => {
                let scheme = if matches!(self.sslmode, SslMode::Require | SslMode::VerifyFull) { "https" } else { "http" };
                return format!("{scheme}://{user}{host}:{}", self.port);
            }
            _ => {}
        }
        if self.engine == Engine::SqlServer {
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

#[derive(Clone, Debug)]
pub struct Dirs {
    pub config: PathBuf,
}

impl Dirs {
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

    /// The directory holds the connections, the history and unsaved editors: this user's only,
    /// whatever the files' own modes are (an older Dataman left it 0755).
    pub fn ensure_private(&self) -> std::io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.config)?;
        let mode = std::fs::metadata(&self.config)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(&self.config, std::fs::Permissions::from_mode(mode & 0o700))?;
        }
        Ok(())
    }

    pub fn connections_file(&self) -> PathBuf {
        self.config.join("connections.toml")
    }

    pub fn history_dir(&self) -> PathBuf {
        self.config.join("history")
    }

    pub fn favorites_file(&self) -> PathBuf {
        self.config.join("favorites.toml")
    }

    pub fn pins_file(&self) -> PathBuf {
        self.config.join("pins.toml")
    }

    pub fn session_dir(&self) -> PathBuf {
        self.config.join("session")
    }

    pub fn known_hosts_file(&self) -> PathBuf {
        self.config.join("known_hosts")
    }

    pub fn mcp_tools_file(&self) -> PathBuf {
        self.config.join("mcp-tools.toml")
    }

    pub fn mcp_audit_file(&self) -> PathBuf {
        self.config.join("mcp-audit.jsonl")
    }
}

/// Other processes write the file too (`dataman-mcp` without the app, a hand edit): every
/// change starts from the file as it is on disk, so nobody's write is lost.
pub struct ConfigStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    connections: Vec<ConnectionProfile>,
    stamp: Option<crate::stamp::FileStamp>,
}

fn mtime(p: &Path) -> Option<crate::stamp::FileStamp> {
    crate::stamp::FileStamp::of(p)
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

    /// A file that does not parse (a hand edit half done) keeps the connections as they were.
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

    fn change(&self, f: impl FnOnce(&mut Vec<ConnectionProfile>)) -> Result<()> {
        let mut g = self.inner.lock();
        Self::sync(&self.path, &mut g);
        f(&mut g.connections);
        save(&self.path, &g.connections)?;
        g.stamp = mtime(&self.path);
        Ok(())
    }

    pub fn upsert(&self, profile: ConnectionProfile) -> Result<()> {
        self.change(|g| match g.iter_mut().find(|c| c.id == profile.id) {
            Some(c) => *c = profile,
            None => g.push(profile),
        })
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.change(|g| g.retain(|c| c.id != id))
    }

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
    fn tls_defaults_follow_the_host() {
        for h in ["", "localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "::1", "[::1]", "/tmp", "db.localhost"] {
            assert!(is_local_host(h), "{h}");
            assert_eq!(default_sslmode(Engine::Postgres, h, false), SslMode::Prefer, "{h}");
            assert_eq!(default_sslmode(Engine::Redis, h, false), SslMode::Disable, "{h}");
        }
        for h in ["db.example.com", "10.0.0.5", "192.168.1.2", "localhost.evil.com", "::2", "host.docker.internal"] {
            assert!(!is_local_host(h), "{h}");
            for e in Engine::ALL {
                assert_eq!(default_sslmode(e, h, false), SslMode::Require, "{h} {e:?}");
                // Through a tunnel the hop to the bastion is encrypted: as on this machine.
                assert_eq!(default_sslmode(e, h, true), if e.is_sql() { SslMode::Prefer } else { SslMode::Disable }, "{h} {e:?} tunnel");
            }
        }
    }

    #[test]
    fn config_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("new").join("dataman"));
        dirs.ensure_private().unwrap();
        assert_eq!(std::fs::metadata(&dirs.config).unwrap().permissions().mode() & 0o777, 0o700);
        let old = Dirs::at(dir.path().join("old"));
        std::fs::create_dir(&old.config).unwrap();
        std::fs::set_permissions(&old.config, std::fs::Permissions::from_mode(0o755)).unwrap();
        let core = crate::Core::new(old.clone(), std::sync::Arc::new(crate::MemorySecrets::default())).unwrap();
        assert_eq!(std::fs::metadata(&old.config).unwrap().permissions().mode() & 0o777, 0o700, "an older, open directory is closed");
        drop(core);
    }

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

    #[test]
    fn refresh_follows_other_writers() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let app = ConfigStore::open(&dirs).unwrap();
        assert!(!app.refresh(), "no file, nothing changed");
        let a = ConnectionProfile::new("a");
        app.upsert(a.clone()).unwrap();
        assert!(!app.refresh(), "its own write is not a change");
        let other = ConfigStore::open(&dirs).unwrap();
        let b = ConnectionProfile::new("b");
        other.upsert(b.clone()).unwrap();
        assert!(app.refresh());
        assert_eq!(app.connections(), vec![a.clone(), b.clone()]);
        assert!(!app.refresh());
        let mut b2 = b.clone();
        b2.name = "b renamed".into();
        other.upsert(b2.clone()).unwrap();
        let c = ConnectionProfile::new("c");
        app.upsert(c.clone()).unwrap();
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![a.clone(), b2.clone(), c.clone()]);
        assert_eq!(app.connections(), vec![a.clone(), b2.clone(), c.clone()]);
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

    #[test]
    fn read_only_per_database() {
        let mut p = ConnectionProfile::new("pg");
        assert!(!p.read_only_for("shop"));
        p.set_read_only_for("shop", true);
        assert!(p.read_only_for("shop") && !p.read_only_for("other"));
        assert_eq!(p.read_only_databases, BTreeMap::from([("shop".to_string(), true)]));
        p.set_read_only_for("shop", false);
        assert!(p.read_only_databases.is_empty());
        p.read_only = true;
        p.set_read_only_for("scratch", false);
        assert!(p.read_only_for("shop") && !p.read_only_for("scratch"));
        assert!(!p.read_only_everywhere());
        p.set_read_only_for("scratch", true);
        assert!(p.read_only_everywhere() && p.read_only_databases.is_empty());
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

    #[test]
    fn mcp_allowed_and_yolo_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let mut p = ConnectionProfile::new("staging");
        p.env = Environment::Staging;
        assert_eq!(p.mcp_allow(&["TRUNCATE audit_tmp;", "DELETE FROM sessions WHERE expires_at < ?", "BEGIN", "a; b"]), ["TRUNCATE audit_tmp", "DELETE FROM sessions WHERE expires_at < ?"]);
        assert!(p.mcp_allow(&["delete from sessions where expires_at < 5", "truncate AUDIT_TMP"]).is_empty());
        p.mcp_yolo = true;
        assert_eq!(p.mcp_yolo_on(), MCP_ADVANCED, "in effect only with the advanced options on");
        let q = ConnectionProfile::new("plain");
        store.upsert(p.clone()).unwrap();
        store.upsert(q.clone()).unwrap();
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("mcp_yolo = true") && text.contains("TRUNCATE audit_tmp"), "{text}");
        assert_eq!((text.matches("mcp_yolo").count(), text.matches("mcp_allowed").count()), (1, 1), "unset stays out of the file: {text}");
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![p.clone(), q]);
        std::fs::write(dirs.connections_file(), "[[connection]]\nid = \"a\"\nname = \"n\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n").unwrap();
        let old = &ConfigStore::open(&dirs).unwrap().connections()[0];
        assert!(old.mcp_allowed.is_empty() && !old.mcp_yolo);
        p.mcp = Some(McpAccess::Off);
        assert!(p.mcp_yolo && !p.mcp_yolo_on());
    }

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
        assert_eq!(Engine::ALL.map(Engine::default_port), [5432, 3306, 1433, 27017, 9200, 9200, 6379]);
    }

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
