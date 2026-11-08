//! Saved connections (`~/.config/dataman/connections.toml`). Passwords never go here: they
//! live in the [`SecretStore`](crate::secret::SecretStore).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::approved::{self, Approved, Kept};
use crate::project::ConnReview;

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
    Sqlite,
    #[serde(rename = "duckdb")]
    DuckDb,
    #[serde(rename = "clickhouse")]
    ClickHouse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    Sql,
    Document,
    KeyValue,
}

impl Engine {
    pub const ALL: [Engine; 10] = [
        Engine::Postgres,
        Engine::MySql,
        Engine::SqlServer,
        Engine::ClickHouse,
        Engine::Sqlite,
        Engine::DuckDb,
        Engine::MongoDb,
        Engine::Elasticsearch,
        Engine::OpenSearch,
        Engine::Redis,
    ];

    pub fn label(self) -> &'static str {
        self.spec().label
    }

    /// Stored in the file and used by the MCP tools and the logos: must not change.
    pub fn slug(self) -> &'static str {
        self.spec().slug
    }

    pub fn parse(s: &str) -> Option<Engine> {
        let s = s.trim().to_ascii_lowercase();
        crate::engine::SPECS.iter().find(|e| e.slug == s || e.aliases.contains(&s.as_str()) || e.label.eq_ignore_ascii_case(&s)).map(|e| e.engine)
    }

    pub fn default_port(self) -> u16 {
        self.spec().default_port
    }

    pub fn default_user(self) -> &'static str {
        self.spec().default_user
    }

    pub fn family(self) -> Family {
        self.spec().family
    }

    pub fn is_sql(self) -> bool {
        self.family() == Family::Sql
    }

    pub fn dialect(self) -> crate::sql::Dialect {
        crate::sql::Dialect::of(self)
    }

    pub fn is_file(self) -> bool {
        self.spec().caps.file
    }

    pub fn file_extensions(self) -> &'static [&'static str] {
        self.spec().file_exts
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

/// A loopback address or the name `localhost`: what stays on this machine whatever a resolver may
/// answer for another name (`x.localhost` is on [`is_local_host`]'s side, not on this one).
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Hosted services that refuse plain text and present publicly signed certificates for their host
/// (PlanetScale's `aws.connect.psdb.cloud`): a new connection verifies them, through a tunnel too.
const VERIFIED_HOSTS: &[&str] = &[".psdb.cloud"];

pub fn verified_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    VERIFIED_HOSTS.iter().any(|s| h.ends_with(s))
}

/// What a new connection starts with until the user picks: opportunistic (`prefer`, which is
/// plain text for the document engines) on this machine or through an SSH tunnel (already
/// encrypted up to the bastion), TLS required anywhere else, and verified where the engine's
/// spec says so.
pub fn default_sslmode(engine: Engine, host: &str, tunnel: bool) -> SslMode {
    if engine.is_file() {
        return SslMode::Disable;
    }
    if verified_host(host) {
        return SslMode::VerifyFull;
    }
    match (tunnel || is_local_host(host), engine.is_sql()) {
        (false, _) if engine.spec().strict_tls => SslMode::VerifyFull,
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

/// How a connection signs in: a password (typed, in the keychain, `~/.pgpass`), or a short-lived
/// token the user's own cloud CLI mints (`az`, `aws`, `gcloud`) for each new connection. Nothing
/// is registered anywhere by Dataman: the CLI is signed in as the user already.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum Auth {
    #[default]
    Password,
    /// Microsoft Entra ID through `az account get-access-token`; no tenant: az's default.
    EntraId {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },
    /// RDS/Aurora IAM through `aws rds generate-db-auth-token`; no region: the endpoint's.
    AwsIam {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
    },
    /// Cloud SQL IAM through `gcloud sql generate-login-token`; no account: gcloud's active one.
    GoogleIam {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthKind {
    Password,
    EntraId,
    AwsIam,
    GoogleIam,
}

impl AuthKind {
    pub const ALL: [AuthKind; 4] = [AuthKind::Password, AuthKind::EntraId, AuthKind::AwsIam, AuthKind::GoogleIam];

    pub fn label(self) -> &'static str {
        match self {
            AuthKind::Password => "Password",
            AuthKind::EntraId => "Microsoft Entra ID",
            AuthKind::AwsIam => "AWS IAM",
            AuthKind::GoogleIam => "Google Cloud IAM",
        }
    }

    /// Stored nowhere: the MCP tools' and the reviews' word for it.
    pub fn slug(self) -> &'static str {
        match self {
            AuthKind::Password => "password",
            AuthKind::EntraId => "entra-id",
            AuthKind::AwsIam => "aws-iam",
            AuthKind::GoogleIam => "google-iam",
        }
    }

    pub fn parse(s: &str) -> Option<AuthKind> {
        let s = s.trim().to_ascii_lowercase();
        AuthKind::ALL.into_iter().find(|k| k.slug() == s || k.label().eq_ignore_ascii_case(&s))
    }

    pub fn is_token(self) -> bool {
        self != AuthKind::Password
    }

    /// The method as a new connection starts it: every field the CLI's default.
    pub fn fresh(self) -> Auth {
        match self {
            AuthKind::Password => Auth::Password,
            AuthKind::EntraId => Auth::EntraId { tenant: None },
            AuthKind::AwsIam => Auth::AwsIam { profile: None, region: None },
            AuthKind::GoogleIam => Auth::GoogleIam { account: None },
        }
    }
}

impl Auth {
    pub fn kind(&self) -> AuthKind {
        match self {
            Auth::Password => AuthKind::Password,
            Auth::EntraId { .. } => AuthKind::EntraId,
            Auth::AwsIam { .. } => AuthKind::AwsIam,
            Auth::GoogleIam { .. } => AuthKind::GoogleIam,
        }
    }

    pub fn is_password(&self) -> bool {
        *self == Auth::Password
    }

    pub fn is_token(&self) -> bool {
        !self.is_password()
    }

    /// Without what is only this person's (the AWS profile is a name in their ~/.aws/config,
    /// the Google account their e-mail): what a project's file shares.
    pub fn shared(&self) -> Auth {
        match self {
            Auth::AwsIam { region, .. } => Auth::AwsIam { profile: None, region: region.clone() },
            Auth::GoogleIam { .. } => Auth::GoogleIam { account: None },
            a => a.clone(),
        }
    }

    /// `self` (a project's) with `mine`'s personal fields, when both are the same method.
    pub fn with_personal(&self, mine: &Auth) -> Auth {
        match (self, mine) {
            (Auth::AwsIam { region, .. }, Auth::AwsIam { profile, .. }) => Auth::AwsIam { profile: profile.clone(), region: region.clone() },
            (Auth::GoogleIam { .. }, Auth::GoogleIam { account }) => Auth::GoogleIam { account: account.clone() },
            (a, _) => a.clone(),
        }
    }

    pub fn describe(&self) -> String {
        let extra: Vec<String> = match self {
            Auth::Password => Vec::new(),
            Auth::EntraId { tenant } => tenant.iter().map(|t| format!("tenant {t}")).collect(),
            Auth::AwsIam { profile, region } => profile.iter().map(|p| format!("profile {p}")).chain(region.iter().map(|r| format!("region {r}"))).collect(),
            Auth::GoogleIam { account } => account.iter().map(|a| format!("account {a}")).collect(),
        };
        if extra.is_empty() { self.kind().label().to_string() } else { format!("{} ({})", self.kind().label(), extra.join(", ")) }
    }
}

impl Engine {
    pub fn auth_kinds(self) -> &'static [AuthKind] {
        match self {
            Engine::Postgres | Engine::MySql => &AuthKind::ALL,
            Engine::SqlServer => &[AuthKind::Password, AuthKind::EntraId],
            _ => &[AuthKind::Password],
        }
    }

    pub fn supports(self, kind: AuthKind) -> bool {
        self.auth_kinds().contains(&kind)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    /// Left out of the file for PostgreSQL, so files of older versions load unchanged.
    #[serde(default, skip_serializing_if = "Engine::is_postgres")]
    pub engine: Engine,
    /// The server as it was picked or last told itself on connect (TimescaleDB on PostgreSQL),
    /// for its logo and name while not connected. Only shown: what Dataman does goes by the
    /// engine and what the server says once connected (`ServerInfo`). Each person's own.
    #[serde(default, skip_serializing_if = "crate::server::Flavor::is_standard")]
    pub flavor: crate::server::Flavor,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub database: String,
    /// The file of a file database (SQLite, DuckDB), as the user wrote it (`~` included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub sslmode: SslMode,
    #[serde(default)]
    pub save_password: bool,
    /// Left out of the file for a password, so files of older versions load unchanged.
    #[serde(default, skip_serializing_if = "Auth::is_password")]
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// `color` is this person's pick (the dialog writes one either way): it outranks production's
    /// red. Absent in older files, where production stays red.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub color_picked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshSettings>,
    #[serde(default, skip_serializing_if = "Environment::is_none")]
    pub env: Environment,
    /// Dataman refuses writes before they reach the server.
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
    /// Its address came from an open project's file (a teammate's choice): a `~/.pgpass` line
    /// that matches any host is not sent there either. This person's own, never in the file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_project: bool,
    /// The agents' own database login (Agents › Permissions, under Limits): every agent call on this
    /// connection connects as it. Its password is the Keychain's, never here; an older Dataman
    /// ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_login: Option<crate::agent_login::AgentLogin>,
    /// Agents are refused here: an agent login was removed, and this person has not said that
    /// agents may use their own login again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agents_refused: bool,
}

fn override_key<'a>(overrides: &'a BTreeMap<String, bool>, engine: Engine, database: &str) -> Option<&'a String> {
    overrides
        .get_key_value(database)
        .map(|(k, _)| k)
        .or_else(|| engine.spec().db_names_ignore_case.then(|| overrides.keys().find(|k| k.eq_ignore_ascii_case(database))).flatten())
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
    /// What the connection form fills in and drops, by the engine's spec (the form and the MCP
    /// tools alike): the default port and database, no database where the engine has none, and
    /// plain text instead of `prefer` for the engines that have no opportunistic TLS.
    pub fn normalize(&mut self) {
        let spec = self.engine.spec();
        if self.flavor.spec_on(self.engine).is_none() {
            self.flavor = Default::default();
        }
        if spec.form.file {
            return;
        }
        if self.port == 0 {
            self.port = spec.default_port;
        }
        if !spec.form.database {
            self.database.clear();
        } else if self.database.trim().is_empty() {
            self.database = spec.default_database.to_string();
        }
        if !self.engine.is_sql() && self.sslmode == SslMode::Prefer {
            self.sslmode = SslMode::Disable;
        }
    }

    /// What a connection must have to be saved (after [`ConnectionProfile::normalize`]).
    pub fn validate(&self) -> Result<(), String> {
        self.validate_target()?;
        let spec = self.engine.spec();
        if spec.form.file {
            return Ok(());
        }
        // SQL Server's Entra token names its user itself.
        let user_given = self.engine == Engine::SqlServer && self.auth.kind() == AuthKind::EntraId;
        if spec.form.user_required && !user_given && self.user.trim().is_empty() {
            return Err("host and user are required".into());
        }
        if self.auth.is_token() && !self.engine.supports(self.auth.kind()) {
            return Err(format!("{} does not sign in with {}", spec.label, self.auth.kind().label()));
        }
        let odd = match &self.auth {
            Auth::EntraId { tenant } => [("tenant", tenant.as_deref()), ("", None)],
            Auth::AwsIam { profile, region } => [("profile", profile.as_deref()), ("region", region.as_deref())],
            Auth::GoogleIam { account } => [("account", account.as_deref()), ("", None)],
            Auth::Password => [("", None), ("", None)],
        }
        .into_iter()
        .find_map(|(what, v)| v.filter(|v| !crate::cloudauth::plain_value(v.trim())).map(|_| what));
        if let Some(what) = odd {
            return Err(format!("the {what} takes letters, digits and . _ @ : / + - only"));
        }
        Ok(())
    }

    /// Where the connection goes, before how it signs in is asked: its file, or its host and
    /// database.
    pub fn validate_target(&self) -> Result<(), String> {
        let spec = self.engine.spec();
        if spec.form.file {
            return match self.path.as_deref().map(str::trim) {
                Some(p) if !p.is_empty() => Ok(()),
                _ => Err(format!("a {} connection needs its file", spec.label)),
            };
        }
        if self.host.trim().is_empty() {
            return Err("host is required".into());
        }
        if let Some(n) = spec.numbered_dbs
            && !self.database.is_empty()
            && self.database.parse::<u32>().map_or(true, |d| d >= n)
        {
            return Err(format!("{}' database is a number (0–{})", spec.label, n - 1));
        }
        Ok(())
    }

    /// MongoDB's URI options as the form takes them (`k=v&k=v`).
    pub fn parse_options(text: &str) -> BTreeMap<String, String> {
        text.split('&')
            .filter_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                Some((k.trim().to_string(), v.trim().to_string())).filter(|(k, _)| !k.is_empty())
            })
            .collect()
    }

    pub fn new(name: impl Into<String>) -> Self {
        ConnectionProfile {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name: name.into(),
            engine: Engine::Postgres,
            flavor: Default::default(),
            host: "127.0.0.1".into(),
            port: 5432,
            user: "postgres".into(),
            database: "postgres".into(),
            path: None,
            sslmode: SslMode::Prefer,
            save_password: false,
            auth: Auth::Password,
            color: None,
            color_picked: false,
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
            from_project: false,
            agent_login: None,
            agents_refused: false,
        }
    }

    /// Painted production's red: production, and no colour of this person's own.
    pub fn prod_red(&self) -> bool {
        self.env.is_production() && !self.color_picked
    }

    /// Its server's logo: its flavor's, else its engine's.
    pub fn logo(&self) -> &'static str {
        self.flavor.logo(self.engine)
    }

    /// What its server is called: its flavor's name, else its engine's.
    pub fn server_label(&self) -> &'static str {
        self.flavor.label(self.engine)
    }

    pub fn new_file(engine: Engine, path: impl Into<String>) -> Self {
        let path = path.into();
        let name = Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut p = ConnectionProfile::new(name);
        p.engine = engine;
        p.host = String::new();
        p.port = 0;
        p.user = String::new();
        p.database = String::new();
        p.sslmode = SslMode::Disable;
        p.path = Some(path);
        p
    }

    /// The file, `~` expanded (an empty path for a server database).
    pub fn file_path(&self) -> PathBuf {
        expand_home(self.path.as_deref().unwrap_or(""))
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
        let strip = |p: &ConnectionProfile| ConnectionProfile {
            mcp: None,
            mcp_row_limit: None,
            mcp_allowed: Vec::new(),
            mcp_yolo: false,
            mcp_rules: Default::default(),
            agent_login: None,
            agents_refused: false,
            // Only shown: telling it again on connect is no reason to connect again.
            flavor: Default::default(),
            ..p.clone()
        };
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
        if self.engine.is_file() {
            return tilde(self.path.as_deref().unwrap_or(""));
        }
        format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }

    pub fn is_socket(&self) -> bool {
        self.host.starts_with('/')
    }

    /// The TLS a connection dials with: at least `require` for a cloud token (every cloud
    /// demands it, and a token must not cross the network in clear), whatever the file says.
    pub fn tls_mode(&self) -> SslMode {
        match self.sslmode {
            SslMode::Disable | SslMode::Prefer if self.auth.is_token() => SslMode::Require,
            m => m,
        }
    }

    pub fn ssh_secret_id(&self) -> String {
        format!("{}:ssh", self.id)
    }

    /// Never with the password (it lives in the keychain).
    pub fn url(&self) -> String {
        if self.engine.is_file() {
            let path = self.file_path();
            let path = if path.is_absolute() { path } else { std::env::current_dir().map(|d| d.join(&path)).unwrap_or(path) };
            let path = path.to_string_lossy();
            // `C:\a\b.db` → `/C:/a/b.db`: a URL's path, as `connstr` reads it back.
            #[cfg(windows)]
            let path = format!("/{}", path.strip_prefix(r"\\?\").unwrap_or(&path).replace('\\', "/"));
            let enc: String = path.split('/').map(percent_encode).collect::<Vec<_>>().join("/");
            let ro = if self.read_only { "?mode=ro" } else { "" };
            return format!("{}://{enc}{ro}", self.engine.slug());
        }
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
            Engine::ClickHouse => return format!("clickhouse://{user}{host}:{}/{}?sslmode={}", self.port, percent_encode(&self.database), self.sslmode.label()),
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
            "postgresql://{user}{host}:{}/{}?sslmode={}",
            self.port,
            percent_encode(&self.database),
            self.sslmode.label()
        )
    }
}

/// `~` and `~/…` (`~\…` on Windows) are this user's home; anything else is the path as written.
pub fn expand_home(path: &str) -> PathBuf {
    let path = path.trim();
    let home = crate::os::home;
    match path.strip_prefix('~') {
        Some("") => home().unwrap_or_else(|| PathBuf::from(path)),
        Some(rest) if rest.starts_with('/') || (cfg!(windows) && rest.starts_with('\\')) => match home() {
            Some(h) => h.join(&rest[1..]),
            None => PathBuf::from(path),
        },
        _ => PathBuf::from(path),
    }
}

pub fn tilde(path: &str) -> String {
    let expanded = expand_home(path);
    match crate::os::home() {
        Some(h) if expanded.starts_with(&h) && expanded != h => format!("~{}{}", std::path::MAIN_SEPARATOR, expanded.strip_prefix(&h).unwrap_or(&expanded).display()),
        _ => expanded.display().to_string(),
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
    #[serde(default, rename = "folder", skip_serializing_if = "Vec::is_empty")]
    folders: Vec<ConnFolder>,
}

/// A folder of the explorer: how this person arranges their connections on this machine. Optional
/// in the file (one written before folders existed loads as is, one without folders is written
/// as before), never in an open project's file, never a change to review: a connection's folder
/// is not one of its settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnFolder {
    pub id: String,
    pub name: String,
    /// One of the connection swatches, by name (as a connection's color).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The connections in it, by id (their order is the explorer's).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<String>,
}

impl ConnFolder {
    pub fn new(name: &str, taken: &[ConnFolder]) -> ConnFolder {
        let names: Vec<&str> = taken.iter().map(|d| d.name.as_str()).collect();
        let base = if name.trim().is_empty() { "New folder" } else { name };
        ConnFolder { id: uuid::Uuid::new_v4().simple().to_string(), name: crate::favorites::unique_name(base, &names), color: None, connections: Vec::new() }
    }

    pub fn holds(&self, conn: &str) -> bool {
        self.connections.iter().any(|c| c == conn)
    }
}

pub fn folder_of<'a>(folders: &'a [ConnFolder], conn: &str) -> Option<&'a ConnFolder> {
    folders.iter().find(|d| d.holds(conn))
}

/// Each connection in one folder at most (the first that lists it), each listed once.
fn tidy_folders(folders: &mut [ConnFolder]) {
    let mut seen = std::collections::HashSet::new();
    for d in folders {
        d.connections.retain(|c| seen.insert(c.clone()));
    }
}

#[derive(Clone, Debug)]
pub struct Dirs {
    pub config: PathBuf,
}

impl Dirs {
    /// `~/.config/dataman` (XDG_CONFIG_HOME's when set); on Windows `%APPDATA%\dataman`, unless
    /// XDG_CONFIG_HOME is set there too (the tests and check stages set it).
    pub fn default_dirs() -> Result<Dirs> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(x) => PathBuf::from(x),
            None if cfg!(windows) => crate::os::app_data().context("APPDATA is not set")?,
            None => crate::os::home().context("HOME is not set")?.join(".config"),
        };
        Ok(Dirs { config: base.join("dataman") })
    }

    pub fn at(path: impl Into<PathBuf>) -> Dirs {
        Dirs { config: path.into() }
    }

    /// The directory holds the connections, the history and unsaved editors: this user's only,
    /// whatever the files' own modes are (an older Dataman left it 0755).
    /// On Windows the folder is in the user's profile, which only they (and administrators) can
    /// open: it inherits that.
    #[cfg(windows)]
    pub fn ensure_private(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config)
    }

    #[cfg(unix)]
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
/// change starts from the file as it is on disk, so nobody's write is lost. What an edit made
/// outside Dataman loosens or moves waits for review (see [`crate::approved`]).
pub struct ConfigStore {
    path: PathBuf,
    approved: Arc<Approved>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// As they apply: the file's, with the approved protections where it loosens them.
    connections: Vec<ConnectionProfile>,
    held: Vec<ConnReview>,
    file: Vec<ConnectionProfile>,
    kept: Kept<Vec<ConnectionProfile>>,
    /// Entries this version cannot read (an engine from a newer Dataman): kept as they are, and
    /// written back on every save.
    skipped: Vec<toml::Table>,
    folders: Vec<ConnFolder>,
    stamp: Option<crate::stamp::FileStamp>,
}

impl Inner {
    fn resolve(&mut self) {
        let r = if self.kept.on() { approved::resolve(&self.file, self.kept.found().map(Vec::as_slice)) } else { approved::Resolved { connections: self.file.clone(), held: Vec::new() } };
        (self.connections, self.held) = (r.connections, r.held);
    }

    fn held(&self, id: &str) -> bool {
        self.held.iter().any(|h| h.id == id)
    }

    /// Approves the file's connections as they are now, but those held that `keep` does not take:
    /// what was approved for them stays.
    fn approve(&mut self, approved: &Approved, keep: impl Fn(&ConnectionProfile) -> bool) -> Result<()> {
        if !self.kept.writable() {
            return Ok(());
        }
        let old = self.kept.found().cloned().unwrap_or_default();
        let list: Vec<ConnectionProfile> = self
            .file
            .iter()
            .filter_map(|p| match old.iter().find(|a| a.id == p.id) {
                Some(a) if self.held(&p.id) && !keep(p) => Some(a.clone()),
                None if self.held(&p.id) && !keep(p) => None,
                _ => Some(approved::protections_of(p)),
            })
            .collect();
        if list != old || matches!(self.kept, Kept::Missing) {
            approved.set_connections(&list)?;
            self.kept = Kept::Found(list);
        }
        Ok(())
    }
}

fn mtime(p: &Path) -> Option<crate::stamp::FileStamp> {
    crate::stamp::FileStamp::of(p)
}

#[derive(Default)]
struct Loaded {
    connections: Vec<ConnectionProfile>,
    skipped: Vec<toml::Table>,
    folders: Vec<ConnFolder>,
}

fn read(path: &Path) -> Result<Loaded> {
    match std::fs::read_to_string(path) {
        Ok(s) => parse_file(&s).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Each connection on its own: one this version cannot read is skipped (and kept), not the file.
fn parse_file(s: &str) -> Result<Loaded> {
    let mut table: toml::Table = toml::from_str(s)?;
    let mut out: Loaded = Default::default();
    // A folder that does not read is only not shown: the connections are what matters.
    if let Some(toml::Value::Array(list)) = table.remove("folder") {
        out.folders = list.into_iter().filter_map(|v| v.try_into::<ConnFolder>().inspect_err(|e| log::warn!("skipping an explorer folder: {e}")).ok()).collect();
        tidy_folders(&mut out.folders);
    }
    let Some(list) = table.remove("connection") else { return Ok(out) };
    let toml::Value::Array(list) = list else { anyhow::bail!("`connection` is not a list") };
    for v in list {
        match v.clone().try_into::<ConnectionProfile>() {
            Ok(c) => out.connections.push(c),
            Err(e) => {
                let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                log::warn!("skipping the connection {name:?}, which this version cannot read ({e}); it stays in the file");
                if let toml::Value::Table(t) = v {
                    out.skipped.push(t);
                }
            }
        }
    }
    Ok(out)
}

impl ConfigStore {
    /// The file taken as it is: for tests, and for what only reads names.
    pub fn open(dirs: &Dirs) -> Result<ConfigStore> {
        ConfigStore::open_with(dirs, Approved::off())
    }

    /// What an edit outside Dataman loosens waits for review against what `approved` keeps.
    pub fn open_with(dirs: &Dirs, approved: Arc<Approved>) -> Result<ConfigStore> {
        let path = dirs.connections_file();
        let _held = crate::stamp::hold(&path);
        let stamp = mtime(&path);
        let Loaded { connections: file, skipped, folders } = read(&path)?;
        let kept = approved.connections();
        let mut inner = Inner { connections: Vec::new(), held: Vec::new(), file, kept, skipped, folders, stamp };
        inner.resolve();
        // The first time (or the item gone): what loosens nothing is approved as it is.
        if matches!(inner.kept, Kept::Missing) {
            match inner.approve(&approved, |_| false) {
                Ok(()) => inner.resolve(),
                Err(e) => log::warn!("the approved rules could not be saved: {e:#}"),
            }
        }
        Ok(ConfigStore { path, approved, inner: Mutex::new(inner) })
    }

    pub fn connections(&self) -> Vec<ConnectionProfile> {
        self.inner.lock().connections.clone()
    }

    pub fn get(&self, id: &str) -> Option<ConnectionProfile> {
        self.inner.lock().connections.iter().find(|c| c.id == id).cloned()
    }

    /// What the file loosens or moves that was not done in Dataman, waiting for review.
    pub fn held(&self) -> Vec<ConnReview> {
        self.inner.lock().held.clone()
    }

    /// A file that does not parse (a hand edit half done) keeps the connections as they were.
    pub fn refresh(&self) -> bool {
        let mut g = self.inner.lock();
        let _held = crate::stamp::hold(&self.path);
        Self::sync(&self.path, &self.approved, &mut g)
    }

    fn sync(path: &Path, approved: &Approved, g: &mut Inner) -> bool {
        let stamp = mtime(path);
        if stamp == g.stamp {
            return false;
        }
        g.stamp = stamp;
        match read(path) {
            Ok(Loaded { connections: file, skipped, folders }) => {
                g.skipped = skipped;
                // Another process may have approved what it wrote.
                g.kept = approved.connections();
                let before = (std::mem::take(&mut g.connections), std::mem::take(&mut g.held), std::mem::replace(&mut g.folders, folders));
                g.file = file;
                g.resolve();
                (&g.connections, &g.held, &g.folders) != (&before.0, &before.1, &before.2)
            }
            Err(e) => {
                log::warn!("{e:#}");
                false
            }
        }
    }

    /// `f` changes the connections as they apply: what it changes is approved, what it leaves
    /// alone of a held connection stays in the file as the file had it, still held.
    fn change(&self, f: impl FnOnce(&mut Vec<ConnectionProfile>)) -> Result<()> {
        let mut g = self.inner.lock();
        let _held = crate::stamp::hold(&self.path);
        Self::sync(&self.path, &self.approved, &mut g);
        let before = g.connections.clone();
        let mut next = before.clone();
        f(&mut next);
        let untouched = |n: &ConnectionProfile| before.iter().find(|b| b.id == n.id).is_some_and(|b| approved::protections_of(b) == approved::protections_of(n));
        let mut file: Vec<ConnectionProfile> = next
            .iter()
            .map(|n| match g.file.iter().find(|p| p.id == n.id) {
                Some(raw) if g.held(&n.id) && untouched(n) => approved::with_approved(n, &approved::protections_of(raw)),
                _ => n.clone(),
            })
            .collect();
        // Held and never approved: not shown, so not `f`'s to change.
        let hidden: Vec<ConnectionProfile> = g.file.iter().filter(|p| g.held(&p.id) && !before.iter().any(|b| b.id == p.id) && !next.iter().any(|n| n.id == p.id)).cloned().collect();
        file.extend(hidden);
        let changed: Vec<String> = next.iter().filter(|n| !untouched(n)).map(|n| n.id.clone()).collect();
        if matches!(g.kept, Kept::Unreadable) && !changed.is_empty() {
            anyhow::bail!("the approved rules could not be read from the {}: a connection's protections cannot change now", crate::secret::STORE_NAME);
        }
        let old_file = std::mem::replace(&mut g.file, file);
        // Approved first: a write that fails after it leaves the file stricter than what is kept.
        if let Err(e) = g.approve(&self.approved, |p| changed.contains(&p.id)) {
            g.file = old_file;
            return Err(e.context(format!("saving the approved rules in the {}", crate::secret::STORE_NAME)));
        }
        // A connection gone leaves its folder.
        let Inner { folders, file, skipped, .. } = &mut *g;
        let skipped_id = |c: &str| skipped.iter().any(|t| t.get("id").and_then(|v| v.as_str()) == Some(c));
        for d in folders.iter_mut() {
            d.connections.retain(|c| file.iter().any(|p| p.id == *c) || skipped_id(c));
        }
        save(&self.path, &g.file, &g.skipped, &g.folders)?;
        g.stamp = mtime(&self.path);
        g.resolve();
        Ok(())
    }

    /// Applies what the file loosens, as `reviewed` showed it; refused when it changed since.
    pub fn accept(&self, reviewed: &[ConnReview]) -> Result<()> {
        let mut g = self.inner.lock();
        let _held = crate::stamp::hold(&self.path);
        Self::sync(&self.path, &self.approved, &mut g);
        if g.held != reviewed {
            anyhow::bail!("connections.toml changed since: review it again");
        }
        if !g.kept.writable() {
            anyhow::bail!("the approved rules could not be read from the {}", crate::secret::STORE_NAME);
        }
        let ids: Vec<String> = reviewed.iter().map(|r| r.id.clone()).collect();
        g.approve(&self.approved, |p| ids.contains(&p.id))?;
        // Written again, so that every process reads what is approved now.
        save(&self.path, &g.file, &g.skipped, &g.folders)?;
        g.stamp = mtime(&self.path);
        g.resolve();
        Ok(())
    }

    /// What the server told itself as on connect, kept by the saved connection `id` (none: one
    /// not saved, or of another engine since). Written only when it changed.
    pub fn note_flavor(&self, id: &str, info: &crate::server::ServerInfo) -> Result<bool> {
        let differs = |c: &ConnectionProfile| c.id == id && c.engine == info.engine && c.flavor != info.flavor;
        if !self.connections().iter().any(differs) {
            return Ok(false);
        }
        self.change(|g| {
            if let Some(c) = g.iter_mut().find(|c| differs(c)) {
                c.flavor = info.flavor;
            }
        })?;
        Ok(true)
    }

    pub fn upsert(&self, profile: ConnectionProfile) -> Result<()> {
        self.change(|g| match g.iter_mut().find(|c| c.id == profile.id) {
            Some(c) => *c = profile,
            None => g.push(profile),
        })
    }

    pub fn edit_all(&self, f: impl FnOnce(&mut Vec<ConnectionProfile>)) -> Result<()> {
        self.change(f)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.change(|g| g.retain(|c| c.id != id))
    }

    pub fn reorder(&self, ids: &[String]) -> Result<()> {
        self.change(|g| g.sort_by_key(|c| ids.iter().position(|i| *i == c.id).unwrap_or(usize::MAX)))
    }

    pub fn folders(&self) -> Vec<ConnFolder> {
        self.inner.lock().folders.clone()
    }

    /// The folders changed in one write. The connections are written back as the file has them:
    /// nothing of them changes, nothing is approved or held.
    pub fn edit_folders(&self, f: impl FnOnce(&mut Vec<ConnFolder>)) -> Result<()> {
        let mut g = self.inner.lock();
        let _held = crate::stamp::hold(&self.path);
        Self::sync(&self.path, &self.approved, &mut g);
        let mut folders = g.folders.clone();
        f(&mut folders);
        tidy_folders(&mut folders);
        if folders == g.folders {
            return Ok(());
        }
        save(&self.path, &g.file, &g.skipped, &folders)?;
        g.folders = folders;
        g.stamp = mtime(&self.path);
        Ok(())
    }
}

fn save(path: &Path, connections: &[ConnectionProfile], skipped: &[toml::Table], folders: &[ConnFolder]) -> Result<()> {
    let dir = path.parent().context("config path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let text = if skipped.is_empty() {
        toml::to_string_pretty(&File { connections: connections.to_vec(), folders: folders.to_vec() })?
    } else {
        let mut list: Vec<toml::Value> = connections.iter().map(toml::Value::try_from).collect::<Result<_, _>>()?;
        list.extend(skipped.iter().cloned().map(toml::Value::Table));
        let mut file = toml::Table::new();
        file.insert("connection".into(), toml::Value::Array(list));
        if !folders.is_empty() {
            file.insert("folder".into(), toml::Value::Array(folders.iter().map(toml::Value::try_from).collect::<Result<_, _>>()?));
        }
        toml::to_string_pretty(&file)?
    };
    // This user's only: a secret driver option stays in it.
    crate::os::atomic_write(path, text.as_bytes(), true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_validate_by_spec() {
        let mut p = ConnectionProfile { host: "h".into(), user: "u".into(), port: 0, ..ConnectionProfile::new("x") };
        p.normalize();
        assert_eq!((p.port, p.database.as_str()), (5432, "postgres"));
        assert!(p.validate().is_ok());
        let mut r = ConnectionProfile { engine: Engine::Redis, host: "h".into(), port: 0, database: "16".into(), sslmode: SslMode::Prefer, ..ConnectionProfile::new("r") };
        r.normalize();
        assert_eq!((r.port, r.sslmode), (6379, SslMode::Disable));
        assert!(r.validate().is_err());
        r.database = "15".into();
        assert!(r.validate().is_ok());
        let mut e = ConnectionProfile { engine: Engine::Elasticsearch, host: "h".into(), database: "x".into(), ..ConnectionProfile::new("e") };
        e.normalize();
        assert!(e.database.is_empty() && e.validate().is_ok());
        assert!(ConnectionProfile { host: "h".into(), user: String::new(), ..ConnectionProfile::new("n") }.validate().is_err(), "a SQL server needs a user");
        assert_eq!(ConnectionProfile::parse_options("a=1& b = 2&=x&c").len(), 2);
    }

    #[test]
    fn tls_defaults_follow_the_host() {
        for h in ["", "localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "::1", "[::1]", "/tmp", "db.localhost"] {
            assert!(is_local_host(h), "{h}");
            assert_eq!(default_sslmode(Engine::Postgres, h, false), SslMode::Prefer, "{h}");
            assert_eq!(default_sslmode(Engine::Redis, h, false), SslMode::Disable, "{h}");
        }
        for h in ["db.example.com", "10.0.0.5", "192.168.1.2", "localhost.evil.com", "::2", "host.docker.internal"] {
            assert_eq!(default_sslmode(Engine::ClickHouse, h, false), SslMode::VerifyFull, "{h}: its password crosses with every request");
            assert_eq!(default_sslmode(Engine::ClickHouse, h, true), SslMode::Prefer, "{h}: the tunnel is the crossing");
            assert!(!is_local_host(h), "{h}");
            for e in Engine::ALL.into_iter().filter(|e| !e.is_file()) {
                assert_eq!(default_sslmode(e, h, false), if e.spec().strict_tls { SslMode::VerifyFull } else { SslMode::Require }, "{h} {e:?}");
                // Through a tunnel the hop to the bastion is encrypted: as on this machine.
                assert_eq!(default_sslmode(e, h, true), if e.is_sql() { SslMode::Prefer } else { SslMode::Disable }, "{h} {e:?} tunnel");
            }
        }
        for h in ["aws.connect.psdb.cloud", "GCP.connect.psdb.cloud.", "us-east.connect.psdb.cloud"] {
            assert_eq!(default_sslmode(Engine::MySql, h, false), SslMode::VerifyFull, "{h}");
            assert_eq!(default_sslmode(Engine::MySql, h, true), SslMode::VerifyFull, "{h} tunnel");
        }
        assert_eq!(default_sslmode(Engine::MySql, "psdb.cloud.evil.example", false), SslMode::Require);
        assert_eq!(default_sslmode(Engine::MySql, "notpsdb.cloud", false), SslMode::Require);
    }

    #[test]
    fn production_red_yields_to_a_picked_color() {
        // An older file: the dialog wrote a colour nobody picked, production stays red.
        let old: ConnectionProfile = toml::from_str("id = \"a\"\nname = \"Prod\"\ncolor = \"blue\"\nenv = \"production\"\n").unwrap();
        assert!(old.prod_red() && !old.color_picked);
        let picked = ConnectionProfile { color_picked: true, ..old.clone() };
        assert!(!picked.prod_red());
        let text = toml::to_string(&picked).unwrap();
        assert!(text.contains("color_picked = true"), "{text}");
        assert!(!toml::to_string(&old).unwrap().contains("color_picked"));
        assert!(!ConnectionProfile { env: Environment::Staging, ..old }.prod_red());
    }

    #[test]
    #[cfg(unix)]
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
        let mut q = ConnectionProfile::new("staging");
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
    fn folders_are_optional_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        // Written before folders existed: loads as is, everything at the top level.
        let old = "[[connection]]\nid = \"a\"\nname = \"pg\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n";
        std::fs::write(dirs.connections_file(), old).unwrap();
        let store = ConfigStore::open(&dirs).unwrap();
        assert!(store.folders().is_empty());
        assert_eq!(store.connections().len(), 1);
        let b = ConnectionProfile::new("b");
        store.upsert(b.clone()).unwrap();
        let plain = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(!plain.contains("folder"), "{plain}");

        let mut work = ConnFolder::new("Work", &[]);
        work.color = Some("mint".into());
        work.connections = vec!["a".into(), b.id.clone()];
        let side = ConnFolder::new("Work", std::slice::from_ref(&work));
        assert_eq!(side.name, "Work 2", "names stay apart");
        store.edit_folders(|f| f.extend([work.clone(), side.clone()])).unwrap();
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("[[folder]]") && text.contains("color = \"mint\""), "{text}");
        let again = ConfigStore::open(&dirs).unwrap();
        assert_eq!(again.folders(), vec![work.clone(), side.clone()]);
        assert_eq!(again.connections(), store.connections(), "the connections are as they were");
        assert_eq!(folder_of(&again.folders(), &b.id).map(|d| d.name.as_str()), Some("Work"));

        // A connection in two folders is in the first; one deleted leaves its folder.
        store.edit_folders(|f| f[1].connections.push("a".into())).unwrap();
        assert!(store.folders()[1].connections.is_empty());
        store.remove(&b.id).unwrap();
        assert_eq!(ConfigStore::open(&dirs).unwrap().folders()[0].connections, vec!["a".to_string()]);
        store.reorder(&["a".into()]).unwrap();
        assert_eq!(ConfigStore::open(&dirs).unwrap().folders().len(), 2);

        // Without folders the file is written as before.
        store.edit_folders(Vec::clear).unwrap();
        store.upsert(b.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(dirs.connections_file()).unwrap(), plain);
    }

    #[test]
    fn folders_follow_other_writers_and_survive_unreadable_entries() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        let text = "[[connection]]\nid = \"a\"\nname = \"pg\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n\n\
                    [[connection]]\nid = \"b\"\nname = \"future\"\nengine = \"cockroach\"\nhost = \"h\"\nport = 2\nuser = \"u\"\ndatabase = \"d\"\n\n\
                    [[folder]]\nid = \"f1\"\nname = \"Prod\"\nconnections = [\"a\", \"b\"]\n\n\
                    [[folder]]\nname = \"no id: skipped\"\n";
        std::fs::write(dirs.connections_file(), text).unwrap();
        let app = ConfigStore::open(&dirs).unwrap();
        assert_eq!(app.folders().iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Prod"]);
        app.upsert(ConnectionProfile::new("c")).unwrap();
        let saved = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(saved.contains("cockroach") && saved.contains("name = \"Prod\""), "{saved}");
        assert_eq!(ConfigStore::open(&dirs).unwrap().folders()[0].connections, ["a", "b"], "a connection this version cannot read keeps its folder");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let other = ConfigStore::open(&dirs).unwrap();
        other.edit_folders(|f| f[0].color = Some("rose".into())).unwrap();
        assert!(app.refresh(), "a folder changed elsewhere is a change");
        assert_eq!(app.folders()[0].color.as_deref(), Some("rose"));
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
        let nobody = ConnectionProfile { user: String::new(), ..p.clone() };
        assert_eq!(nobody.url(), "postgresql://db.prod.internal:6543/app_db?sslmode=require", "no empty user before the host");
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
        assert_eq!(Engine::ALL.map(Engine::default_port), [5432, 3306, 1433, 8123, 0, 0, 27017, 9200, 9200, 6379]);
    }

    #[test]
    fn auth_roundtrip_and_older_files() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = ConfigStore::open(&dirs).unwrap();
        let pw = ConnectionProfile::new("pw");
        let mut az = ConnectionProfile::new("az");
        az.auth = Auth::EntraId { tenant: Some("contoso.onmicrosoft.com".into()) };
        let mut rds = ConnectionProfile::new("rds");
        (rds.engine, rds.auth) = (Engine::MySql, Auth::AwsIam { profile: Some("dev".into()), region: None });
        let mut gcp = ConnectionProfile::new("gcp");
        gcp.auth = Auth::GoogleIam { account: None };
        for p in [&pw, &az, &rds, &gcp] {
            store.upsert(p.clone()).unwrap();
        }
        let text = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(text.contains("[connection.auth]\nmethod = \"entra-id\"\ntenant = \"contoso.onmicrosoft.com\""), "{text}");
        assert!(text.contains("method = \"aws-iam\"\nprofile = \"dev\"") && text.contains("method = \"google-iam\""), "{text}");
        assert_eq!(text.matches("method =").count(), 3, "a password stays out of the file");
        assert!(!text.contains("region"), "{text}");
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections(), vec![pw, az, rds, gcp]);
        // A file of an older version: a password.
        std::fs::write(dirs.connections_file(), "[[connection]]\nid = \"a\"\nname = \"n\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n").unwrap();
        assert_eq!(ConfigStore::open(&dirs).unwrap().connections()[0].auth, Auth::Password);
        // A method of a newer version: the connection is kept in the file, not used.
        let newer = "[[connection]]\nid = \"k\"\nname = \"krb\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n\n[connection.auth]\nmethod = \"kerberos\"\n";
        std::fs::write(dirs.connections_file(), newer).unwrap();
        let s = ConfigStore::open(&dirs).unwrap();
        assert!(s.connections().is_empty());
        s.upsert(ConnectionProfile::new("other")).unwrap();
        assert!(std::fs::read_to_string(dirs.connections_file()).unwrap().contains("kerberos"));
    }

    #[test]
    fn a_token_never_goes_without_tls() {
        let mut p = ConnectionProfile::new("x");
        p.sslmode = SslMode::Disable;
        assert_eq!(p.tls_mode(), SslMode::Disable);
        for a in [AuthKind::EntraId, AuthKind::AwsIam, AuthKind::GoogleIam] {
            p.auth = a.fresh();
            for (m, want) in [(SslMode::Disable, SslMode::Require), (SslMode::Prefer, SslMode::Require), (SslMode::Require, SslMode::Require), (SslMode::VerifyFull, SslMode::VerifyFull)] {
                p.sslmode = m;
                assert_eq!(p.tls_mode(), want, "{a:?} {m:?}");
            }
        }
        assert_eq!(Engine::SqlServer.auth_kinds(), [AuthKind::Password, AuthKind::EntraId]);
        assert!(Engine::MySql.supports(AuthKind::GoogleIam) && !Engine::Redis.supports(AuthKind::EntraId) && !Engine::Sqlite.supports(AuthKind::AwsIam));
        assert_eq!(AuthKind::parse("Entra-ID"), Some(AuthKind::EntraId));
        assert_eq!(Auth::AwsIam { profile: Some("dev".into()), region: Some("eu-west-1".into()) }.describe(), "AWS IAM (profile dev, region eu-west-1)");
    }

    #[test]
    fn old_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        let old = "[[connection]]\nid = \"a1\"\nname = \"old\"\nhost = \"127.0.0.1\"\nport = 5490\nuser = \"app\"\ndatabase = \"app\"\nmetrics_url = \"http://127.0.0.1:9187/metrics\"\n";
        std::fs::write(dirs.connections_file(), old).unwrap();
        let store = ConfigStore::open(&dirs).unwrap();
        let c = store.connections();
        assert_eq!((c.len(), c[0].name.as_str(), c[0].port), (1, "old", 5490));
        store.upsert(c[0].clone()).unwrap();
        assert!(!std::fs::read_to_string(dirs.connections_file()).unwrap().contains("metrics_url"));
    }

    #[test]
    fn unreadable_connections_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        let text = "[[connection]]\nid = \"a\"\nname = \"pg\"\nhost = \"h\"\nport = 1\nuser = \"u\"\ndatabase = \"d\"\n\n\
                    [[connection]]\nid = \"b\"\nname = \"future\"\nengine = \"cockroach\"\nhost = \"h\"\nport = 2\nuser = \"u\"\ndatabase = \"d\"\n";
        std::fs::write(dirs.connections_file(), text).unwrap();
        let store = ConfigStore::open(&dirs).unwrap();
        assert_eq!(store.connections().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["pg"]);
        store.upsert(ConnectionProfile::new_file(Engine::Sqlite, "~/data/app.sqlite")).unwrap();
        let saved = std::fs::read_to_string(dirs.connections_file()).unwrap();
        assert!(saved.contains("engine = \"cockroach\"") && saved.contains("name = \"future\""), "{saved}");
        let again = ConfigStore::open(&dirs).unwrap().connections();
        assert_eq!(again.len(), 2);
        assert_eq!((again[1].engine, again[1].name.as_str(), again[1].path.as_deref()), (Engine::Sqlite, "app", Some("~/data/app.sqlite")));
    }

    #[test]
    fn file_databases() {
        for e in [Engine::Sqlite, Engine::DuckDb] {
            assert!(e.is_file() && e.is_sql());
            assert_eq!(Engine::parse(e.slug()), Some(e));
            assert_eq!(default_sslmode(e, "", false), SslMode::Disable);
        }
        assert_eq!(Engine::parse("sqlite3"), Some(Engine::Sqlite));
        assert_eq!(Engine::parse("duck"), Some(Engine::DuckDb));
        let mut p = ConnectionProfile::new_file(Engine::DuckDb, "/data/my db.duckdb");
        assert_eq!(p.name, "my db");
        // On Windows `/data` is on the current drive (connstr's tests have the drive's URLs).
        #[cfg(unix)]
        assert_eq!(p.url(), "duckdb:///data/my%20db.duckdb");
        p.read_only = true;
        #[cfg(unix)]
        assert_eq!(p.url(), "duckdb:///data/my%20db.duckdb?mode=ro");
        assert_eq!(p.summary(), "/data/my db.duckdb");
        let home = crate::os::home().unwrap().display().to_string();
        assert_eq!(expand_home("~/x.db"), PathBuf::from(&home).join("x.db"));
        assert_eq!(tilde(&format!("{home}/x.db")), format!("~{}x.db", std::path::MAIN_SEPARATOR));
    }

    #[test]
    fn the_server_flavor_is_kept_and_older_files_load_as_they_were() {
        use crate::server::Flavor;
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        // A file of a version before flavors, and one with a flavor this version does not know.
        let old = "[[connection]]\nid = \"a\"\nname = \"tsdb\"\nhost = \"h\"\nport = 5432\nuser = \"u\"\n\n[[connection]]\nid = \"b\"\nname = \"later\"\nflavor = \"cockroachdb\"\nhost = \"h\"\nport = 26257\nuser = \"u\"\n";
        std::fs::write(dirs.connections_file(), old).unwrap();
        let store = ConfigStore::open(&dirs).unwrap();
        let got = store.connections();
        assert_eq!(got.iter().map(|p| (p.id.as_str(), p.flavor)).collect::<Vec<_>>(), [("a", Flavor::Standard), ("b", Flavor::Standard)]);
        assert_eq!((got[0].logo(), got[0].server_label()), (Engine::Postgres.logo(), "PostgreSQL"));
        let timescale = crate::ServerInfo::of(Engine::Postgres, Flavor::Timescale, "TimescaleDB 2.31.0 (PostgreSQL 17.11)".into(), "17.11");
        assert!(store.note_flavor("a", &timescale).unwrap());
        assert!(!store.note_flavor("a", &timescale).unwrap(), "written only when it changed");
        assert!(!store.note_flavor("gone", &timescale).unwrap(), "only a saved connection");
        let mysql = crate::ServerInfo::of(Engine::MySql, Flavor::MariaDb, "MariaDB 11".into(), "11.4");
        assert!(!store.note_flavor("b", &mysql).unwrap(), "only a server of its engine");
        let again = ConfigStore::open(&dirs).unwrap().get("a").unwrap();
        assert_eq!((again.flavor, again.logo(), again.server_label()), (Flavor::Timescale, "logos/timescale.svg", "TimescaleDB"));
        assert!(std::fs::read_to_string(dirs.connections_file()).unwrap().contains("flavor = \"timescaledb\""));
        // Told otherwise on a later connect: that one.
        assert!(store.note_flavor("a", &crate::ServerInfo::of(Engine::Postgres, Flavor::Standard, "PostgreSQL 17.11".into(), "17.11")).unwrap());
        assert_eq!(ConfigStore::open(&dirs).unwrap().get("a").unwrap().flavor, Flavor::Standard);
        assert!(!std::fs::read_to_string(dirs.connections_file()).unwrap().contains("flavor = \"\""), "none is left out of the file");
        // Only shown: nothing reconnects for it, and a form's save drops one of another engine.
        assert!(ConnectionProfile { flavor: Flavor::Timescale, ..again.clone() }.same_but_agents(&again));
        let mut wrong = ConnectionProfile { engine: Engine::MySql, flavor: Flavor::Timescale, ..again };
        wrong.normalize();
        assert_eq!(wrong.flavor, Flavor::Standard);
    }
}
