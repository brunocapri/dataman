//! A database login just for agents: Dataman creates it with the user's own login, keeps its
//! password in the Keychain (account `agent:<connection id>`), and the MCP server connects as it
//! for every agent call on the connection. The connection's rules for agents (the MCP tier,
//! read-only, Privacy and Permissions) are compiled into what the login is granted, so the
//! database refuses what the rules refuse too, even if Dataman's own checks had a gap.
//!
//! The login is a ceiling, never a replacement: every check Dataman makes stays on top (row
//! limits, the real-WHERE rule, approvals, value masking). What the database cannot hold is said
//! per rule ([`Who`]). It is granted only what exists now: a table created later is not granted
//! until the next sync (no default privileges, so a new table matching a hidden pattern is never
//! granted by itself). Dataman marks what it creates and never changes or drops a login, role,
//! schema or view it did not create. Statements an agent may run only after the user's Allow
//! (DDL, TRUNCATE…) run on the user's login, so the agent's credential can never change
//! structure.

pub mod hash;
pub mod live;
pub mod mongo;
pub mod ms;
pub mod my;
pub mod pg;
pub mod redis;
pub mod search;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{ConnectionProfile, Engine, McpAccess};
use crate::privacy::{AgentRules, AgentView, Mask, Names, RedisCats};

/// What marks a login, role, schema or view as Dataman's.
pub const MARK: &str = "dataman-managed v1";

pub const PREFIX: &str = "dataman_agent_";

/// The mark Dataman puts on a login (or the place of its views) for connection `conn`.
pub fn mark_for(conn: &str) -> String {
    format!("{MARK} conn={conn}")
}

/// Whether `comment` is Dataman's mark for exactly this connection (another connection whose id
/// starts the same, `conn=<id>x`, is not this one).
pub fn marked_for(comment: &str, conn: &str) -> bool {
    comment.trim() == mark_for(conn)
}

/// Engines with logins: SQLite and DuckDB have none (Dataman guards them inside the engine).
pub fn supported(e: Engine) -> bool {
    !e.is_file()
}

/// Why the option is not there for a file database.
pub const FILE_NOTE: &str = "SQLite and DuckDB have no logins. Dataman already opens them read-only for read agents and checks each statement inside the engine (SQLite's authorizer), so there is nothing to create.";

/// `dataman_agent_` and eight letters or digits of the connection's id (another id, another
/// login: PostgreSQL's roles are the whole server's).
pub fn login_name(conn_id: &str) -> String {
    let plain: String = conn_id.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).take(8).collect();
    if plain.len() >= 6 {
        return format!("{PREFIX}{plain}");
    }
    format!("{PREFIX}{}", &hash::sha256_hex(conn_id.as_bytes())[..8])
}

/// Where the login's views go (a schema, a database, a name prefix): named after it, never the
/// login's own name (PostgreSQL's search path starts with `"$user"`, which would resolve the
/// agent's unqualified names to the views).
pub fn space_of(user: &str) -> String {
    format!("{user}_views")
}

/// The Keychain account of the agent login's password.
pub fn secret_account(conn_id: &str) -> String {
    format!("agent:{conn_id}")
}

/// A password made for "I'll run it myself", until Verify finds the login.
pub fn pending_account(conn_id: &str) -> String {
    format!("agent-pending:{conn_id}")
}

/// Where a connection reaches, as the agent login keeps it: the login's password only ever goes
/// there (a connection moved since, by a hand edit or a project, refuses agents).
pub fn server_of(p: &ConnectionProfile) -> String {
    let server = format!("{}://{}:{}", p.engine.slug(), p.host.trim().to_ascii_lowercase(), p.port);
    // Through a tunnel, the host is the bastion's to resolve: another bastion is another server.
    match &p.ssh {
        Some(s) => format!("{server} via ssh://{}@{}:{}", s.user.trim(), s.host.trim().to_ascii_lowercase(), s.port.unwrap_or(22)),
        None => server,
    }
}

/// The agent login on a connection, as connections.toml keeps it (never its password).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLogin {
    pub user: String,
    /// [`server_of`] the connection when it was created.
    pub server: String,
    pub created: DateTime<Utc>,
    pub synced: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<DateTime<Utc>>,
    /// [`rules_hash`] of the rules it was granted for.
    pub rules: String,
    /// [`Plan::hash`] of what was granted.
    pub plan: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conn_limit: Option<u32>,
    /// The database its grants are in (another one on the server: none).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    /// What Dataman created besides the login, which Remove drops: `schema:…`, `database:…`,
    /// `view:…`, `role:…`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub made: Vec<String>,
    /// Tables agents read through a view (masked columns, or hidden ones where the engine
    /// hides them by view): Dataman sends their reads there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<ViewMap>,
    /// Counts for the card: tables granted, views, not grantable.
    #[serde(default)]
    pub granted: usize,
    #[serde(default)]
    pub not_granted: usize,
}

impl AgentLogin {
    pub fn rotation_due(&self, now: DateTime<Utc>) -> Option<i64> {
        let days = self.rotate_days? as i64;
        let from = self.rotated.unwrap_or(self.created);
        Some(days - (now - from).num_days())
    }

    pub fn view_for(&self, schema: &str, table: &str) -> Option<&ViewMap> {
        self.views.iter().find(|v| v.schema.eq_ignore_ascii_case(schema) && v.table.eq_ignore_ascii_case(table))
    }
}

/// A table and the view agents read it through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewMap {
    pub schema: String,
    pub table: String,
    /// As the engine writes it, quoted (`dataman_agent_x."public__users"`).
    pub view: String,
    /// Columns the view masks, and how.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masked: Vec<(String, Mask)>,
}

/// What a user picks in the review sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub rotate_days: Option<u32>,
    pub conn_limit: Option<u32>,
    pub verify: bool,
}

pub const ROTATE_DAYS: u32 = 90;
pub const CONN_LIMIT: u32 = 4;
/// Days past the rotation the password stays valid, when the engine can expire it: a rotation
/// missed by a few days (the Mac asleep) does not lock agents out.
pub const GRACE_DAYS: u32 = 14;

impl Default for Options {
    fn default() -> Self {
        Options { rotate_days: Some(ROTATE_DAYS), conn_limit: Some(CONN_LIMIT), verify: true }
    }
}

/// The rules as the database can hold them, engine-neutral.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub engine: Engine,
    /// The connection's id, in the mark.
    pub conn: String,
    pub user: String,
    /// Where Dataman's views go: a schema (PostgreSQL, SQL Server), a database (MySQL) or a
    /// name prefix (MongoDB). The login's own name.
    pub space: String,
    pub database: String,
    pub tier: McpAccess,
    /// Rows may be written (Write or Full, the database not read-only).
    pub write: bool,
    /// Rows may be read at all (not Structure only).
    pub rows: bool,
    pub activity: bool,
    /// Cancel others' statements (activity, Write or Full).
    pub kill: bool,
    pub code: bool,
    pub time_limit_s: u64,
    pub conn_limit: Option<u32>,
    pub expires_days: Option<u32>,
    pub rules: AgentRules,
    pub redis: RedisCats,
    /// Read-only, for the connection and per database (MySQL and MongoDB name databases where
    /// the others name schemas).
    pub read_only: bool,
    pub read_only_dbs: BTreeMap<String, bool>,
}

impl Spec {
    /// Rows may be written in `database`.
    pub fn write_in(&self, database: &str) -> bool {
        self.tier >= McpAccess::Write && self.rows && !crate::config::read_only_in(self.read_only, &self.read_only_dbs, self.engine, database)
    }

    /// The database an object is in: its schema where databases are the schemas.
    pub fn database_of<'a>(&'a self, schema: &'a str) -> &'a str {
        if matches!(self.engine, Engine::MySql | Engine::MongoDb) { schema } else { &self.database }
    }
}

/// The rules for the connection's database (`database`; empty: its default).
pub fn compile(p: &ConnectionProfile, database: &str, o: &Options) -> Spec {
    let database = if database.is_empty() { p.database.clone() } else { database.to_string() };
    let tier = match p.mcp_access() {
        McpAccess::Off => McpAccess::Read,
        t => t,
    };
    let rules = p.mcp_rules.clone();
    let rows = rules.view != AgentView::Structure;
    let write = tier >= McpAccess::Write && !p.read_only_for(&database) && rows;
    let activity = rules.activity_on();
    Spec {
        engine: p.engine,
        conn: p.id.clone(),
        user: login_name(&p.id),
        space: space_of(&login_name(&p.id)),
        database,
        tier,
        write,
        rows,
        activity,
        kill: activity && tier >= McpAccess::Write,
        code: rules.code,
        time_limit_s: rules.time_limit().as_secs(),
        conn_limit: o.conn_limit,
        expires_days: o.rotate_days.map(|d| d + GRACE_DAYS),
        redis: rules.redis.clone(),
        rules,
        read_only: p.read_only,
        read_only_dbs: p.read_only_databases.clone(),
    }
}

/// What of the rules the login depends on, hashed: another hash, the rules changed since.
pub fn rules_hash(p: &ConnectionProfile, database: &str) -> String {
    let s = compile(p, database, &Options { rotate_days: None, conn_limit: None, verify: false });
    let mut r = s.rules.clone();
    // Limits Dataman alone holds do not change what is granted.
    (r.read_rows, r.read_bytes, r.read_from) = (None, None, None);
    if s.engine != Engine::Redis {
        r.redis = RedisCats::default();
    }
    let text = format!("{:?}|{:?}|{}|{}|{}|{}|{}|{:?}", s.engine, s.tier.max(McpAccess::Read), s.write, s.rows, s.activity, s.kill, s.code, r);
    hash::sha256_hex(text.as_bytes())[..16].to_string()
}

/// A table, collection, index or key pattern on the server now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Obj {
    pub schema: String,
    pub name: String,
    pub kind: ObjKind,
    pub columns: Vec<Col>,
    /// The user's login can grant on it (owner, member of the owner, grant option, superuser).
    pub grantable: bool,
    pub owner: String,
    /// Sequences its columns take defaults from (PostgreSQL serials).
    pub sequences: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjKind {
    Table,
    View,
    Collection,
    Index,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Col {
    pub name: String,
    pub generated: bool,
}

impl Col {
    pub fn new(name: &str) -> Col {
        Col { name: name.into(), generated: false }
    }
}

/// What is on the server, as the user's login sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    pub objects: Vec<Obj>,
    /// What every login may read (granted to PUBLIC, SQL Server's `public`): the agent login
    /// reads it too, whatever it is granted.
    pub open: Vec<Open>,
    /// Roles every account gets (MySQL's `mandatory_roles`), the agent login's too.
    pub open_roles: Vec<String>,
}

/// A table every login may read, or some of its columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    pub schema: String,
    pub name: String,
    /// The columns every login may read (all of them where the table itself is granted).
    pub columns: Vec<String>,
    pub whole: bool,
}

/// What the rules hide or mask that every login may read anyway: the database lets the agent
/// login read it too (Dataman's own checks still keep it from agents). `revoke` is the SQL that
/// closes it (run by its owner).
pub fn exposed(spec: &Spec, inv: &Inventory, revoke: &dyn Fn(&Open, &[String]) -> String) -> Vec<NotGranted> {
    let (acc, hidden) = access(spec, inv);
    let mut out = Vec::new();
    for o in &inv.open {
        let label = if o.schema.is_empty() { o.name.clone() } else { format!("{}.{}", o.schema, o.name) };
        if o.schema.starts_with(PREFIX) {
            continue;
        }
        let is_hidden = hidden.iter().any(|h| *h == label || h.starts_with(&format!("{label} (")));
        if is_hidden || !spec.rows {
            let cols = if o.whole { Vec::new() } else { o.columns.clone() };
            out.push(NotGranted { object: label.clone(), reason: format!("{} to every login (PUBLIC)", if o.whole { "granted" } else { "some columns granted" }), sql: vec![revoke(o, &cols)] });
            continue;
        }
        let Some(a) = acc.iter().find(|a| a.schema == o.schema && a.name == o.name) else { continue };
        let kept: Vec<String> = a.hidden.iter().cloned().chain(a.masked.iter().map(|(c, _)| c.clone())).filter(|c| o.whole || o.columns.contains(c)).collect();
        if !kept.is_empty() {
            let names: Vec<String> = kept.iter().map(|c| format!("{label}.{c}")).collect();
            out.push(NotGranted { object: short_list(&names), reason: "hidden or masked, and granted to every login (PUBLIC)".into(), sql: vec![revoke(o, if o.whole { &[] } else { &kept })] });
        }
    }
    if !inv.open_roles.is_empty() {
        out.push(NotGranted {
            object: format!("roles {}", inv.open_roles.join(", ")),
            reason: "every account gets them (mandatory_roles) and may turn them on (SET ROLE)".into(),
            sql: vec!["SET PERSIST mandatory_roles = '';".into()],
        });
    }
    out
}

impl Inventory {
    /// Every login may read `column` of `schema.name` (any column when `None`: the table).
    pub fn open_to_all(&self, schema: &str, name: &str, column: Option<&str>) -> bool {
        self.open.iter().any(|o| o.schema == schema && o.name == name && (o.whole || column.is_none_or(|c| o.columns.iter().any(|x| x == c))))
    }
}

/// What [`exposed`] found, on the plan and in its rules: the database does not hold those.
pub(crate) fn with_exposed(mut plan: Plan, exposed: Vec<NotGranted>) -> Plan {
    if exposed.is_empty() {
        return plan;
    }
    for r in plan.rules.iter_mut().filter(|r| r.who == Who::Database && (r.rule.ends_with(" hidden") || r.rule.ends_with(" masked") || r.rule.starts_with("Structure only") || r.rule.starts_with("Rows:"))) {
        r.who = Who::Partly;
    }
    let names: Vec<String> = exposed.iter().map(|e| e.object.clone()).collect();
    plan.rules.push(line(format!("{} readable by every login", short_list(&names)), "granted to PUBLIC: the agent login reads it too, only Dataman keeps it from agents", Who::Dataman));
    plan.exposed = exposed;
    plan
}

/// One table's (collection's, index's) access, from the rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    pub schema: String,
    pub name: String,
    pub kind: ObjKind,
    /// Readable as they are, from the table itself.
    pub plain: Vec<String>,
    pub hidden: Vec<String>,
    pub masked: Vec<(String, Mask)>,
    /// No column kept: the whole table is granted.
    pub whole: bool,
    pub write: bool,
    pub grantable: bool,
    pub sequences: Vec<String>,
    pub owner: String,
}

impl Access {
    pub fn label(&self) -> String {
        if self.schema.is_empty() { self.name.clone() } else { format!("{}.{}", self.schema, self.name) }
    }

    /// Every column agents may name (the masked ones too, which a view shows masked).
    pub fn visible(&self) -> Vec<String> {
        let mut v = self.plain.clone();
        v.extend(self.masked.iter().map(|(c, _)| c.clone()));
        v
    }
}

/// The rules over what exists now: the objects agents may use, and those hidden from them.
pub fn access(spec: &Spec, inv: &Inventory) -> (Vec<Access>, Vec<String>) {
    let names = Names::of(spec.engine);
    let r = &spec.rules;
    let masked_on = r.view == AgentView::Masked;
    let (mut out, mut hidden) = (Vec::new(), Vec::new());
    for o in &inv.objects {
        let schema = (!o.schema.is_empty()).then_some(o.schema.as_str());
        let label = if o.schema.is_empty() { o.name.clone() } else { format!("{}.{}", o.schema, o.name) };
        // Dataman's own views are never granted as tables.
        if o.schema.starts_with(PREFIX) || o.name.starts_with(PREFIX) {
            continue;
        }
        let visible = match names {
            Names::Qualified => r.visible.schema_visible(&o.schema, names) && r.visible.table_visible(schema, &o.name, names),
            Names::Flat => r.visible.table_visible(None, &o.name, names),
        };
        if !visible {
            hidden.push(label);
            continue;
        }
        let mut a = Access {
            schema: o.schema.clone(),
            name: o.name.clone(),
            kind: o.kind,
            plain: Vec::new(),
            hidden: Vec::new(),
            masked: Vec::new(),
            whole: true,
            write: spec.write_in(spec.database_of(&o.schema)) && o.kind != ObjKind::View,
            grantable: o.grantable,
            sequences: o.sequences.clone(),
            owner: o.owner.clone(),
        };
        for c in &o.columns {
            if r.visible.column_hidden(schema, &o.name, &c.name, names) {
                a.hidden.push(c.name.clone());
            } else if let Some(m) = masked_on.then(|| r.sensitive_mask(schema, &o.name, &c.name, names)).flatten() {
                a.masked.push((c.name.clone(), m));
            } else {
                a.plain.push(c.name.clone());
            }
        }
        // A computed column may carry what it is computed from: in a table that keeps
        // something, it is kept too (fail closed).
        if !a.hidden.is_empty() || !a.masked.is_empty() {
            let computed: Vec<String> = o.columns.iter().filter(|c| c.generated).map(|c| c.name.clone()).collect();
            a.plain.retain(|c| !computed.contains(c));
            a.masked.retain(|(c, _)| !computed.contains(c));
            for g in computed {
                if !a.hidden.contains(&g) {
                    a.hidden.push(g);
                }
            }
        }
        a.whole = a.hidden.is_empty() && a.masked.is_empty();
        if !o.columns.is_empty() && a.plain.is_empty() && a.masked.is_empty() {
            hidden.push(format!("{label} (every column hidden)"));
            continue;
        }
        out.push(a);
    }
    (out, hidden)
}

/// Fields the rules name outright on a collection or index (no `*`), added to those sampled:
/// a field no sampled document had is still projected out.
pub(crate) fn with_rule_fields(spec: &Spec, inv: &mut Inventory) {
    let names = Names::of(spec.engine);
    let r = &spec.rules;
    for o in &mut inv.objects {
        let schema = (!o.schema.is_empty()).then_some(o.schema.as_str());
        let mut cands: Vec<String> = r.visible.hidden_column_patterns(schema, &o.name, names);
        cands.extend(r.sensitive.iter().filter(|s| s.on).map(|s| s.col.rsplit_once('.').map_or(s.col.clone(), |(_, c)| c.to_string())));
        for c in cands.into_iter().filter(|c| !c.contains('*') && !c.is_empty()) {
            let kept = r.visible.column_hidden(schema, &o.name, &c, names) || (r.view == AgentView::Masked && r.sensitive_mask(schema, &o.name, &c, names).is_some());
            if kept && !o.columns.iter().any(|x| x.name == c) {
                o.columns.push(Col::new(&c));
            }
        }
    }
}

/// Who holds a rule: the database itself, the database in part, or Dataman alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Who {
    Database,
    Partly,
    Dataman,
}

impl Who {
    pub fn label(self) -> &'static str {
        match self {
            Who::Database => "DATABASE",
            Who::Partly => "PARTLY",
            Who::Dataman => "DATAMAN",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleLine {
    pub rule: String,
    pub how: String,
    pub who: Who,
}

pub(crate) fn line(rule: impl Into<String>, how: impl Into<String>, who: Who) -> RuleLine {
    RuleLine { rule: rule.into(), how: how.into(), who }
}

/// What one step runs, as the engine takes it.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Sql(String),
    /// A command on a MongoDB database.
    Mongo { db: String, cmd: serde_json::Value },
    /// A Redis command's arguments.
    Redis(Vec<String>),
    /// An HTTP call on Elasticsearch or OpenSearch.
    Http { method: String, path: String, body: Option<serde_json::Value> },
    /// A comment in the script, nothing run.
    Note,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub action: Action,
    /// As the review shows it: never a password, nor a hash of one.
    pub shown: String,
    /// As "Copy SQL" gives it: runnable. Holds the password's hash where the engine takes
    /// one, the password itself only where it cannot (MongoDB).
    pub copy: String,
    /// The comment over its group in the script.
    pub group: String,
}

impl Step {
    pub(crate) fn sql(group: &str, sql: String) -> Step {
        Step { shown: format!("{sql};"), copy: format!("{sql};"), action: Action::Sql(sql), group: group.into() }
    }

    pub(crate) fn secret_sql(group: &str, sql: String, shown: String) -> Step {
        Step { shown: format!("{shown};"), copy: format!("{sql};"), action: Action::Sql(sql), group: group.into() }
    }

    pub(crate) fn note(group: &str, text: String) -> Step {
        Step { shown: text.clone(), copy: text, action: Action::Note, group: group.into() }
    }
}

/// An object agents may use that the user's login cannot grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotGranted {
    pub object: String,
    pub reason: String,
    /// What its owner could run to include it.
    pub sql: Vec<String>,
}

/// Where the login cannot be created by Dataman (a managed service), or the feature is off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Manual {
    /// MongoDB Atlas: users and roles only through Atlas.
    Atlas,
    /// Elasticsearch or OpenSearch without security: there are no users.
    NoSecurity,
    /// Redis without ACL (before 6) or a managed Redis that blocks it.
    NoAcl(String),
}

/// What the user's login could do, before the plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// The user's login as the server names it.
    pub login: String,
    pub version: String,
    /// The user's login can create the agent login.
    pub can_create: bool,
    /// Short lines for the card, each good or a warning.
    pub checks: Vec<(bool, String)>,
    /// What the login lacks to create it (empty when it can).
    pub lacks: Vec<String>,
    pub manual: Option<Manual>,
    /// A login named as Dataman's already exists: Some(true) marked as Dataman's (this
    /// connection's), Some(false) not, which Dataman never touches.
    pub exists: Option<bool>,
    pub tls: bool,
    pub notes: Vec<String>,
    /// PostgreSQL's major version, MySQL's flavor and the like, for the emitters.
    pub flavor: Flavor,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flavor {
    /// The server's major version (PostgreSQL, MySQL or MariaDB, Redis).
    pub major: u32,
    pub mariadb: bool,
    /// Azure SQL Database (contained users).
    pub azure: bool,
    pub opensearch: bool,
    /// The user's login can grant anything (superuser, sysadmin).
    pub superuser: bool,
    /// The user's login can grant activity (PostgreSQL: ADMIN on pg_read_all_stats).
    pub activity: bool,
}

/// A grant as the SQL engines keep it: one privilege on one object, so what the server has and
/// what the rules want compare line by line.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Grant {
    /// On the database itself (PostgreSQL's CONNECT).
    Database(String),
    /// On a schema (PostgreSQL's USAGE).
    Schema { schema: String, privilege: String },
    Table { schema: String, name: String, privilege: String },
    Column { schema: String, name: String, column: String, privilege: String },
    /// PostgreSQL's USAGE on a sequence.
    Sequence { schema: String, name: String },
    /// A role it is a member of (PostgreSQL's predefined roles, SQL Server's database roles).
    Member(String),
    /// Server-wide (MySQL's `*.*`, SQL Server's server permissions).
    Global(String),
    /// A default for its sessions (PostgreSQL's `ALTER ROLE … SET`).
    Setting { name: String, value: String },
    Limit(u32),
    /// Dataman's view in its own schema, by the hash of its definition.
    View { name: String, hash: String },
}

/// A view Dataman creates for a table agents read masked (or narrowed, on SQL Server).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewDef {
    pub name: String,
    pub create: String,
    pub hash: String,
    pub map: ViewMap,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub engine: Option<Engine>,
    pub user: String,
    pub space: String,
    pub steps: Vec<Step>,
    /// Run as one transaction.
    pub one_tx: bool,
    pub rules: Vec<RuleLine>,
    pub not_granted: Vec<NotGranted>,
    /// Hidden or masked, yet every login may read it: see [`exposed`].
    pub exposed: Vec<NotGranted>,
    pub lacks: Vec<String>,
    pub notes: Vec<String>,
    pub made: Vec<String>,
    pub views: Vec<ViewMap>,
    pub granted: usize,
    pub hidden: Vec<String>,
    pub manual: Option<Manual>,
    pub hash: String,
    /// The grants as lines, for the diff with what the server has.
    pub lines: Vec<String>,
}

impl Plan {
    /// The script as the review shows it, grouped by purpose.
    pub fn shown(&self) -> String {
        self.text(false)
    }

    /// The script as "Copy SQL" gives it.
    pub fn copy(&self) -> String {
        self.text(true)
    }

    fn text(&self, copy: bool) -> String {
        let comment = match self.engine {
            Some(Engine::MongoDb) => "//",
            Some(Engine::Redis | Engine::Elasticsearch | Engine::OpenSearch) => "#",
            _ => "--",
        };
        let mut out = String::new();
        let mut group = "";
        let tx = self.one_tx && self.engine.is_some_and(Engine::is_sql);
        if tx {
            out.push_str(if self.engine == Some(Engine::SqlServer) { "BEGIN TRANSACTION;\n" } else { "BEGIN;\n" });
        }
        for s in &self.steps {
            if s.group != group {
                if !out.is_empty() && !out.ends_with("BEGIN;\n") && !out.ends_with("TRANSACTION;\n") {
                    out.push('\n');
                }
                if !s.group.is_empty() {
                    out.push_str(&format!("{comment} {}\n", s.group));
                }
                group = &s.group;
            }
            let text = if copy { &s.copy } else { &s.shown };
            if s.action == Action::Note {
                for l in text.lines() {
                    out.push_str(&format!("{comment} {l}\n"));
                }
            } else {
                out.push_str(text);
                out.push('\n');
            }
        }
        if tx {
            out.push_str("COMMIT;\n");
        }
        if !self.not_granted.is_empty() {
            let lines: Vec<&String> = self.not_granted.iter().flat_map(|n| &n.sql).collect();
            if !lines.is_empty() {
                out.push_str(&format!("\n{comment} needs a login that can grant on them, such as their owner (not run):\n"));
                for l in lines {
                    out.push_str(&format!("{comment} {l}\n"));
                }
            }
        }
        if !self.exposed.is_empty() {
            out.push_str(&format!("\n{comment} hidden or masked, yet every login may read them (the agent's too); to close that, their owner runs (not run):\n"));
            for l in self.exposed.iter().flat_map(|n| &n.sql) {
                out.push_str(&format!("{comment} {l}\n"));
            }
        }
        out
    }

    pub fn steps_run(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.action != Action::Note)
    }

    pub(crate) fn seal(mut self) -> Plan {
        let mut lines = self.lines.clone();
        lines.sort();
        lines.dedup();
        self.lines = lines;
        let text = format!("{}|{}|{:?}", self.user, self.lines.join("\n"), self.views);
        self.hash = hash::sha256_hex(text.as_bytes())[..16].to_string();
        self
    }
}

/// Table and column grants of one object as one privilege list (`SELECT ("a", "b"), DELETE`),
/// as a script reads best; a sync keeps them one per line.
pub(crate) fn privilege_list(grants: &[Grant], schema: &str, name: &str, q: &dyn Fn(&str) -> String) -> String {
    let mut parts = Vec::new();
    for v in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
        if grants.iter().any(|g| matches!(g, Grant::Table { schema: s, name: n, privilege: p } if s == schema && n == name && p == v)) {
            parts.push(v.to_string());
            continue;
        }
        let cols: Vec<String> = grants
            .iter()
            .filter_map(|g| match g {
                Grant::Column { schema: s, name: n, column, privilege: p } if s == schema && n == name && p == v => Some(q(column)),
                _ => None,
            })
            .collect();
        if !cols.is_empty() {
            parts.push(format!("{v} ({})", cols.join(", ")));
        }
    }
    parts.join(", ")
}

/// The objects table and column grants are on, in order; whether each has columns.
pub(crate) fn granted_objects(grants: &[Grant]) -> Vec<(String, String, bool)> {
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for g in grants {
        let (s, n, col) = match g {
            Grant::Table { schema, name, .. } => (schema, name, false),
            Grant::Column { schema, name, .. } => (schema, name, true),
            _ => continue,
        };
        match out.iter_mut().find(|(a, b, _)| a == s && b == n) {
            Some(o) => o.2 |= col,
            None => out.push((s.clone(), n.clone(), col)),
        }
    }
    out
}

/// A change the sync makes, for the drift card: `+` granted, `-` revoked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub add: bool,
    pub line: String,
}

/// The lines of `want` against what the server has.
pub fn diff(have: &[String], want: &[String]) -> Vec<Change> {
    let mut out: Vec<Change> = have.iter().filter(|l| !want.contains(l)).map(|l| Change { add: false, line: l.clone() }).collect();
    out.extend(want.iter().filter(|l| !have.contains(l)).map(|l| Change { add: true, line: l.clone() }));
    out
}

/// Rule lines every engine shares: what stays in Dataman whatever the database can do.
pub(crate) fn dataman_lines(spec: &Spec, prod: bool) -> Vec<RuleLine> {
    let mut v = Vec::new();
    if spec.rules.view == AgentView::Masked {
        v.push(line("Cards, JWTs, keys inside any value", "pattern masking on every reply", Who::Dataman));
    }
    if spec.write {
        v.push(line("Rows per write, a real WHERE", "checked before each statement runs", Who::Dataman));
    }
    v.push(line("Rows and size per read", "Dataman cuts each reply", Who::Dataman));
    if prod {
        v.push(line("Every write on production asks", "your Allow in the app", Who::Dataman));
    }
    if spec.tier == McpAccess::Full {
        v.push(line("DDL, TRUNCATE after your Allow", "runs on your login, never the agent's", Who::Dataman));
    }
    v
}

pub(crate) fn time_label(s: u64) -> String {
    if s < 60 { format!("{s} s per call") } else { format!("{} min per call", s / 60) }
}

/// The label for hidden objects in the rules list.
pub(crate) fn hidden_label(hidden: &[String]) -> String {
    let names: Vec<&str> = hidden.iter().take(3).map(String::as_str).collect();
    let more = hidden.len().saturating_sub(3);
    if more > 0 { format!("{} and {more} more hidden", names.join(", ")) } else { format!("{} hidden", names.join(", ")) }
}

/// Names in Dataman's space that are not views of a table.
pub(crate) const RESERVED: &[&str] = &["salt", "dataman_views"];

/// `schema__table` (`table` alone in the default schema), cut with a hash where too long for
/// the engine (`max` characters). A name another view of `taken` has (`a.b__c` and `a__b.c`,
/// case aside) or Dataman's own (`salt`) gets the hash of `schema.table` too: two tables never
/// share a view, and no view takes the salt's place.
pub(crate) fn view_name(schema: &str, table: &str, default_schema: &str, max: usize, taken: &[ViewDef]) -> String {
    let base = if schema.is_empty() || schema.eq_ignore_ascii_case(default_schema) { table.to_string() } else { format!("{schema}__{table}") };
    let clash = |n: &str| RESERVED.iter().any(|r| r.eq_ignore_ascii_case(n)) || taken.iter().any(|v| v.name.eq_ignore_ascii_case(n));
    if base.chars().count() <= max && !clash(&base) {
        return base;
    }
    let h = &hash::sha256_hex(format!("{schema}.{table}").as_bytes())[..8];
    let keep: String = base.chars().take(max.saturating_sub(9)).collect();
    format!("{keep}_{h}")
}

/// Hidden and masked columns, for the rules list.
pub(crate) fn column_lines(acc: &[Access], hidden_how: (&str, Who), masked_how: (&str, Who), rows: bool) -> Vec<RuleLine> {
    let mut v = Vec::new();
    let hid: Vec<String> = acc.iter().flat_map(|a| a.hidden.iter().map(move |c| format!("{}.{c}", a.name))).collect();
    if !hid.is_empty() && rows {
        v.push(line(format!("{} hidden", short_list(&hid)), hidden_how.0, hidden_how.1));
    }
    let masked: Vec<String> = acc.iter().flat_map(|a| a.masked.iter().map(move |(c, _)| format!("{}.{c}", a.name))).collect();
    if !masked.is_empty() && rows {
        v.push(line(format!("{} masked", short_list(&masked)), masked_how.0, masked_how.1));
    }
    v
}

pub fn short_list(v: &[String]) -> String {
    let more = v.len().saturating_sub(3);
    let head = v.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if more > 0 { format!("{head} +{more}") } else { head }
}

/// An agent login record for tests.
#[cfg(test)]
pub(crate) fn test_login(user: &str, p: &ConnectionProfile) -> AgentLogin {
    let now = Utc::now();
    AgentLogin {
        user: user.into(),
        server: server_of(p),
        created: now,
        synced: now,
        verified: None,
        rules: "r".into(),
        plan: "p".into(),
        rotate_days: Some(90),
        rotated: Some(now),
        conn_limit: Some(4),
        database: p.database.clone(),
        made: Vec::new(),
        views: Vec::new(),
        granted: 1,
        not_granted: 0,
    }
}

#[cfg(test)]
mod tests;
