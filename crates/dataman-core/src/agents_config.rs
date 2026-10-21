//! Agents' settings as a file, to keep or to carry to another Mac: General, the Sensitive data
//! types and every connection's rules for agents. Never a password, the mask key, the audit or a
//! connection's address. An import is checked whole first, summarized (what loosens protection
//! flagged), then written all at once.

use serde::{Deserialize, Serialize};

use crate::config::{ConfigStore, ConnectionProfile, Engine, McpAccess};
use crate::privacy::{self, AgentRules, AgentView, Mask, TypeSettings, VisibleMode};
use crate::settings::{MCP_MAX_ROWS, MCP_WRITE_ROW_LIMIT, Settings, SettingsStore};

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentsFile {
    pub version: u32,
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub sensitive_data: TypeSettings,
    #[serde(default, rename = "connection", skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<ConnRules>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct General {
    pub agent_access: bool,
}

impl Default for General {
    fn default() -> Self {
        General { agent_access: true }
    }
}

/// One connection's rules, found here by its id, else by its name and engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnRules {
    pub id: String,
    pub name: String,
    pub engine: Engine,
    #[serde(default)]
    pub database: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpAccess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub yolo: bool,
    #[serde(default)]
    pub rules: AgentRules,
}

pub fn export(settings: &Settings, profiles: &[ConnectionProfile]) -> AgentsFile {
    AgentsFile {
        version: VERSION,
        general: General { agent_access: settings.mcp_enabled },
        sensitive_data: settings.agent_types.clone(),
        connections: profiles
            .iter()
            .map(|p| ConnRules {
                id: p.id.clone(),
                name: p.name.clone(),
                engine: p.engine,
                database: p.database.clone(),
                mcp: p.mcp,
                row_limit: p.mcp_row_limit,
                allowed: p.mcp_allowed.clone(),
                yolo: p.mcp_yolo,
                rules: p.mcp_rules.clone(),
            })
            .collect(),
    }
}

pub fn to_text(f: &AgentsFile) -> String {
    let body = toml::to_string_pretty(f).unwrap_or_default();
    format!("# Dataman agents settings: General, Sensitive data and each connection's rules for agents.\n# No passwords, keys or connection addresses.\n\n{body}")
}

/// The file, or everything wrong with it; nothing of it is used when anything is.
pub fn parse(text: &str) -> Result<AgentsFile, Vec<String>> {
    let f: AgentsFile = toml::from_str(text).map_err(|e| vec![format!("Not a Dataman agents file: {}", e.message())])?;
    let mut errs = Vec::new();
    if f.version != VERSION {
        errs.push(format!("Version {} is not one this Dataman reads (it reads {VERSION}).", f.version));
    }
    if f.sensitive_data.custom.len() > privacy::MAX_CUSTOM {
        errs.push(format!("Sensitive data: {} custom types, {} at most.", f.sensitive_data.custom.len(), privacy::MAX_CUSTOM));
    }
    for (i, c) in f.sensitive_data.custom.iter().enumerate() {
        let name = if c.name.trim().is_empty() { format!("custom type {}", i + 1) } else { c.name.clone() };
        if c.name.trim().is_empty() {
            errs.push(format!("Sensitive data › {name}: no name."));
        }
        if f.sensitive_data.custom[..i].iter().any(|o| o.id == c.id) {
            errs.push(format!("Sensitive data › {name}: its id {} is used twice.", c.id));
        }
        if let Err(e) = privacy::check_pattern(&c.pattern) {
            errs.push(format!("Sensitive data › {name}: {e}"));
        }
    }
    for c in &f.connections {
        let at = if c.name.trim().is_empty() { c.id.clone() } else { c.name.clone() };
        if c.id.trim().is_empty() && c.name.trim().is_empty() {
            errs.push("A connection with neither an id nor a name.".into());
        }
        let r = &c.rules;
        let bad = |v: Option<u64>, max: u64| v.is_some_and(|n| n == 0 || n > max);
        if bad(r.time_limit_s, 24 * 3600) {
            errs.push(format!("{at}: time per call must be 1 s to 24 h."));
        }
        if bad(r.read_rows, 1_000_000) {
            errs.push(format!("{at}: rows per read must be 1 to 1,000,000."));
        }
        if bad(r.read_bytes, 64 * 1024 * 1024) {
            errs.push(format!("{at}: size per read must be 1 byte to 64 MB."));
        }
        if bad(c.row_limit, 100_000_000) {
            errs.push(format!("{at}: rows per write must be at least 1."));
        }
        if r.sensitive.iter().any(|s| s.col.trim().is_empty()) || r.visible.items.iter().any(|i| i.name.trim().is_empty()) {
            errs.push(format!("{at}: an empty column or table name."));
        }
        if c.allowed.iter().any(|a| a.trim().is_empty()) {
            errs.push(format!("{at}: an empty allowed statement."));
        }
    }
    if errs.is_empty() { Ok(f) } else { Err(errs) }
}

/// One line of the summary: what changes, from what to what, and whether agents get more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub what: String,
    pub from: String,
    pub to: String,
    pub loosens: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnPlan {
    pub id: String,
    pub name: String,
    /// Found by name and engine: the file's id is not one of this Mac's.
    pub by_name: bool,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub general: Vec<Change>,
    pub types: Vec<Change>,
    pub conns: Vec<ConnPlan>,
    /// Connections of the file with no match here: left out.
    pub not_found: Vec<String>,
}

impl Plan {
    pub fn loosens(&self) -> bool {
        self.general.iter().chain(&self.types).chain(self.conns.iter().flat_map(|c| &c.changes)).any(|c| c.loosens)
    }

    pub fn changes(&self) -> usize {
        self.general.len() + self.types.len() + self.conns.iter().map(|c| c.changes.len()).sum::<usize>()
    }
}

fn find<'a>(c: &ConnRules, profiles: &'a [ConnectionProfile]) -> Option<(&'a ConnectionProfile, bool)> {
    if let Some(p) = profiles.iter().find(|p| p.id == c.id) {
        return Some((p, false));
    }
    let mut same = profiles.iter().filter(|p| p.name == c.name && p.engine == c.engine);
    match (same.next(), same.next()) {
        (Some(p), None) => Some((p, true)),
        _ => None,
    }
}

fn yes_no(b: bool) -> String {
    (if b { "yes" } else { "no" }).into()
}

/// How much a mask keeps from the agent: Hidden keeps all, Partial the least.
fn strength(m: Mask) -> u8 {
    match m {
        Mask::Hidden => 2,
        Mask::Hash => 1,
        Mask::Partial => 0,
    }
}

fn shielding(v: AgentView) -> u8 {
    match v {
        AgentView::Structure => 2,
        AgentView::Masked => 1,
        AgentView::Data => 0,
    }
}

fn bytes(n: u64) -> String {
    if n >= 1024 * 1024 { format!("{} MB", n / (1024 * 1024)) } else if n >= 1024 { format!("{} KB", n / 1024) } else { format!("{n} B") }
}

fn conn_changes(p: &ConnectionProfile, c: &ConnRules) -> Vec<Change> {
    let mut out = Vec::new();
    let mut push = |what: &str, from: String, to: String, loosens: bool| {
        if from != to {
            out.push(Change { what: what.into(), from, to, loosens });
        }
    };
    let next = ConnectionProfile { mcp: c.mcp, ..p.clone() };
    let (a, b) = (p.mcp_access(), next.mcp_access());
    push("MCP tier", a.label().into(), b.label().into(), b > a);
    let (r0, r1) = (&p.mcp_rules, &c.rules);
    push("The agent sees", r0.view.label().into(), r1.view.label().into(), shielding(r1.view) < shielding(r0.view));
    // Visible to agents: what an Everything except list stops hiding, or an Only these list
    // starts showing, reaches agents.
    if r0.visible.mode != r1.visible.mode {
        push("Visible to agents", r0.visible.mode.label().into(), r1.visible.mode.label().into(), r1.visible.mode == VisibleMode::Except);
    }
    let names = |v: &privacy::Visible| v.items.iter().map(|i| i.name.clone()).collect::<Vec<_>>();
    let (v0, v1) = (names(&r0.visible), names(&r1.visible));
    for n in v0.iter().filter(|n| !v1.contains(n)) {
        let loosens = r1.visible.mode == VisibleMode::Except && r0.visible.mode == VisibleMode::Except;
        push(&format!("Visible to agents › {n}"), "listed".into(), "not listed".into(), loosens);
    }
    for n in v1.iter().filter(|n| !v0.contains(n)) {
        push(&format!("Visible to agents › {n}"), "not listed".into(), "listed".into(), r1.visible.mode == VisibleMode::Only);
    }
    let mask_of = |r: &AgentRules, col: &str| r.sensitive.iter().find(|s| s.col == col && s.on).map(|s| s.how);
    let cols: Vec<String> = r0.sensitive.iter().chain(&r1.sensitive).map(|s| s.col.clone()).fold(Vec::new(), |mut v, c| {
        if !v.contains(&c) {
            v.push(c);
        }
        v
    });
    for col in cols {
        let (m0, m1) = (mask_of(r0, &col), mask_of(r1, &col));
        let label = |m: Option<Mask>| m.map_or("not sensitive".to_string(), |m| m.label().to_string());
        let loosens = match (m0, m1) {
            (Some(_), None) => true,
            (Some(a), Some(b)) => strength(b) < strength(a),
            _ => false,
        };
        push(&format!("Sensitive column › {col}"), label(m0), label(m1), loosens);
    }
    for (what, a, b) in [("Agents may change structure", r0.structure, r1.structure), ("Agents may run code", r0.code, r1.code), ("Agents may see server activity", r0.activity, r1.activity)] {
        push(what, yes_no(a), yes_no(b), b && !a);
    }
    let rc = |r: &AgentRules| [r.redis.read, r.redis.write, r.redis.keyspace, r.redis.scripting, r.redis.pubsub];
    for (i, what) in ["Redis @read", "Redis @write", "Redis @keyspace", "Redis @scripting", "Redis @pubsub"].iter().enumerate() {
        let (a, b) = (rc(r0)[i], rc(r1)[i]);
        push(what, yes_no(a), yes_no(b), b && !a);
    }
    let time = |r: &AgentRules| r.time_limit_s.unwrap_or(privacy::DEFAULT_TIME_LIMIT_S);
    push("Time per call", format!("{} s", time(r0)), format!("{} s", time(r1)), time(r1) > time(r0));
    let rows = |r: &AgentRules| r.read_rows.unwrap_or(MCP_MAX_ROWS);
    push("Rows per read", rows(r0).to_string(), rows(r1).to_string(), rows(r1) > rows(r0));
    let size = |r: &AgentRules| r.read_bytes.unwrap_or(privacy::DEFAULT_READ_BYTES);
    push("Size per read", bytes(size(r0)), bytes(size(r1)), size(r1) > size(r0));
    let (w0, w1) = (p.mcp_row_limit.unwrap_or(MCP_WRITE_ROW_LIMIT), c.row_limit.unwrap_or(MCP_WRITE_ROW_LIMIT));
    push("Rows per write", w0.to_string(), w1.to_string(), w1 > w0);
    let rf = |r: &AgentRules| r.read_from.map_or("default".to_string(), |f| f.label().to_string());
    push("Read from", rf(r0), rf(r1), false);
    for a in c.allowed.iter().filter(|a| !p.mcp_allowed.contains(a)) {
        push("Allowed without asking", "—".into(), a.clone(), true);
    }
    for a in p.mcp_allowed.iter().filter(|a| !c.allowed.contains(a)) {
        push("Allowed without asking", a.clone(), "—".into(), false);
    }
    push("YOLO", yes_no(p.mcp_yolo), yes_no(c.yolo), c.yolo && !p.mcp_yolo);
    out
}

fn type_changes(t0: &TypeSettings, t1: &TypeSettings) -> Vec<Change> {
    let mut out = Vec::new();
    let mut push = |what: String, from: String, to: String, loosens: bool| {
        if from != to {
            out.push(Change { what, from, to, loosens });
        }
    };
    let on = |b: bool| (if b { "on" } else { "off" }).to_string();
    for b in privacy::BUILTINS {
        let (a, z) = (t0.builtin_on(b), t1.builtin_on(b));
        push(format!("Sensitive data › {}", b.name), on(a), on(z), a && !z);
        let (m0, m1) = (t0.builtin_mask(b), t1.builtin_mask(b));
        push(format!("Sensitive data › {} mask", b.name), m0.label().into(), m1.label().into(), z && strength(m1) < strength(m0));
    }
    for c in &t0.custom {
        match t1.custom.iter().find(|x| x.id == c.id) {
            None => push(format!("Sensitive data › {}", c.name), on(c.on), "removed".into(), c.on),
            Some(x) => {
                push(format!("Sensitive data › {}", c.name), on(c.on), on(x.on), c.on && !x.on);
                // A pattern can match less than it did: count a change as a loosening.
                let rule = |t: &privacy::CustomType| format!("{} · {} · {}", t.pattern, t.mask.label(), t.validator.label());
                push(format!("Sensitive data › {} pattern", c.name), rule(c), rule(x), x.on && rule(c) != rule(x));
            }
        }
    }
    for x in t1.custom.iter().filter(|x| !t0.custom.iter().any(|c| c.id == x.id)) {
        push(format!("Sensitive data › {}", x.name), "—".into(), format!("added, {}", on(x.on)), false);
    }
    out
}

pub fn plan(settings: &Settings, profiles: &[ConnectionProfile], f: &AgentsFile) -> Plan {
    let mut plan = Plan::default();
    if settings.mcp_enabled != f.general.agent_access {
        let on = |b: bool| (if b { "on" } else { "off" }).to_string();
        plan.general.push(Change { what: "Agent access".into(), from: on(settings.mcp_enabled), to: on(f.general.agent_access), loosens: f.general.agent_access });
    }
    plan.types = type_changes(&settings.agent_types, &f.sensitive_data);
    for c in &f.connections {
        match find(c, profiles) {
            Some((p, by_name)) => {
                let changes = conn_changes(p, c);
                if !changes.is_empty() || by_name {
                    plan.conns.push(ConnPlan { id: p.id.clone(), name: p.name.clone(), by_name, changes });
                }
            }
            None => plan.not_found.push(format!("{} ({}{})", c.name, c.engine.label(), if c.database.is_empty() { String::new() } else { format!(", {}", c.database) })),
        }
    }
    plan
}

fn apply_conns(profiles: &mut [ConnectionProfile], f: &AgentsFile) {
    let ids: Vec<(usize, usize)> = f.connections.iter().enumerate().filter_map(|(i, c)| find(c, profiles).and_then(|(p, _)| profiles.iter().position(|x| x.id == p.id)).map(|at| (i, at))).collect();
    for (i, at) in ids {
        let (c, p) = (&f.connections[i], &mut profiles[at]);
        p.mcp = c.mcp;
        p.mcp_row_limit = c.row_limit;
        p.mcp_allowed = c.allowed.clone();
        p.mcp_yolo = c.yolo;
        p.mcp_rules = c.rules.clone();
    }
}

fn apply_settings(s: &mut Settings, f: &AgentsFile) {
    s.mcp_enabled = f.general.agent_access;
    s.agent_types = f.sensitive_data.clone();
}

/// Writes the file's settings, all or nothing: the connections first (one file, renamed into
/// place), then the settings; when those cannot be written the connections go back as they were.
pub fn import(config: &ConfigStore, settings: &SettingsStore, f: &AgentsFile) -> anyhow::Result<Plan> {
    let before = config.connections();
    let plan = plan(&settings.get(), &before, f);
    config.edit_all(|g| apply_conns(g, f))?;
    if let Err(e) = settings.update(|s| apply_settings(s, f)) {
        let _ = config.edit_all(|g| *g = before);
        return Err(e);
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::{CustomType, ItemKind, SensitiveCol, VisibleItem};

    fn setup() -> (tempfile::TempDir, crate::Dirs, ConfigStore, SettingsStore) {
        let dir = tempfile::tempdir().unwrap();
        let dirs = crate::Dirs::at(dir.path());
        let config = ConfigStore::open(&dirs).unwrap();
        let settings = SettingsStore::open(&dirs);
        (dir, dirs, config, settings)
    }

    fn rich(config: &ConfigStore, settings: &SettingsStore) {
        let mut p = ConnectionProfile::new("prod-pg");
        p.host = "db.internal.example".into();
        p.user = "app_owner".into();
        p.mcp = Some(McpAccess::Write);
        p.mcp_row_limit = Some(10);
        p.mcp_allowed = vec!["UPDATE orders SET status = ? WHERE id = ?".into()];
        p.mcp_rules.view = AgentView::Masked;
        p.mcp_rules.sensitive.push(SensitiveCol { col: "users.email".into(), how: Mask::Hash, on: true });
        p.mcp_rules.visible.items.push(VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column });
        p.mcp_rules.structure = false;
        p.mcp_rules.read_rows = Some(100);
        config.upsert(p).unwrap();
        let mut m = ConnectionProfile::new("orders-mongo");
        m.engine = Engine::MongoDb;
        m.mcp_rules.code = false;
        config.upsert(m).unwrap();
        settings
            .update(|s| {
                s.mcp_enabled = false;
                s.agent_types.set_builtin("ip", Some(true), Some(Mask::Hidden));
                s.agent_types.custom.push(CustomType::new("Customer code", r"\bCUST-[0-9]{6}\b", Mask::Hash));
            })
            .unwrap();
    }

    fn agent_parts(config: &ConfigStore) -> Vec<(String, Option<McpAccess>, Option<u64>, Vec<String>, bool, AgentRules)> {
        config.connections().into_iter().map(|p| (p.name, p.mcp, p.mcp_row_limit, p.mcp_allowed, p.mcp_yolo, p.mcp_rules)).collect()
    }

    #[test]
    fn export_clear_import_comes_back_the_same() {
        let (_d, _dirs, config, settings) = setup();
        rich(&config, &settings);
        let (rules0, types0, access0) = (agent_parts(&config), settings.get().agent_types, settings.get().mcp_enabled);
        let text = to_text(&export(&settings.get(), &config.connections()));
        for p in config.connections() {
            config.upsert(ConnectionProfile { mcp: None, mcp_row_limit: None, mcp_allowed: Vec::new(), mcp_yolo: false, mcp_rules: Default::default(), ..p }).unwrap();
        }
        settings.update(|s| (s.mcp_enabled, s.agent_types) = (true, Default::default())).unwrap();
        assert_ne!(agent_parts(&config), rules0);
        let f = parse(&text).unwrap();
        let done = import(&config, &settings, &f).unwrap();
        assert!(done.changes() > 0 && done.not_found.is_empty());
        assert_eq!(agent_parts(&config), rules0, "{text}");
        assert_eq!((settings.get().agent_types, settings.get().mcp_enabled), (types0, access0));
        assert!(plan(&settings.get(), &config.connections(), &f).changes() == 0, "importing again changes nothing");
    }

    #[test]
    fn the_file_holds_no_secret() {
        let (_d, dirs, config, settings) = setup();
        rich(&config, &settings);
        let key = crate::privacy::HashKey::fixed(b"mask-key-secret-123");
        let text = to_text(&export(&settings.get(), &config.connections()));
        for secret in ["db.internal.example", "app_owner", "mask-key-secret-123", "password = "] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(!text.contains(&key.hash("x")[3..10]));
        assert!(text.contains("version = 1") && text.contains("prod-pg") && text.contains("engine = \"postgres\""), "{text}");
        let _ = dirs;
    }

    #[test]
    fn an_invalid_file_changes_nothing() {
        let (_d, _dirs, config, settings) = setup();
        rich(&config, &settings);
        let mut f = export(&settings.get(), &config.connections());
        f.sensitive_data.custom[0].pattern = "(?=x)abc".into();
        f.connections[0].rules.read_rows = Some(0);
        let errs = parse(&to_text(&f)).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("Customer code") && e.contains("Look-around")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("rows per read")), "{errs:?}");
        assert!(parse("version = 9\n").unwrap_err()[0].contains("Version 9"));
        assert!(parse("not toml [").unwrap_err()[0].starts_with("Not a Dataman agents file"));
        assert!(parse("version = 1\n[[connection]]\nid = \"a\"\nname = \"a\"\nengine = \"postgres\"\n[connection.rules]\nview = \"everything\"\n").is_err(), "an unknown mode");
    }

    #[test]
    fn connections_match_by_id_then_by_name_and_engine() {
        let (_d, _dirs, config, settings) = setup();
        rich(&config, &settings);
        let mut f = export(&settings.get(), &config.connections());
        for c in &mut f.connections {
            c.id = format!("{}-elsewhere", c.id);
        }
        f.connections[0].rules.read_rows = Some(500);
        f.connections.push(ConnRules { id: "zzz".into(), name: "gone".into(), engine: Engine::MySql, database: "shop".into(), mcp: None, row_limit: None, allowed: Vec::new(), yolo: false, rules: Default::default() });
        let mut wrong_engine = f.connections[1].clone();
        wrong_engine.engine = Engine::Redis;
        f.connections.push(wrong_engine);
        let p = plan(&settings.get(), &config.connections(), &f);
        assert!(p.conns.iter().all(|c| c.by_name), "{p:?}");
        assert_eq!(p.not_found, ["gone (MySQL, shop)".to_string(), "orders-mongo (Redis, postgres)".to_string()]);
        import(&config, &settings, &f).unwrap();
        let p = config.connections().into_iter().find(|p| p.name == "prod-pg").unwrap();
        assert_eq!(p.mcp_rules.read_rows, Some(500));
    }

    #[test]
    fn the_summary_flags_what_loosens() {
        let (_d, _dirs, config, settings) = setup();
        rich(&config, &settings);
        let mut f = export(&settings.get(), &config.connections());
        assert!(!plan(&settings.get(), &config.connections(), &f).loosens());
        let c = &mut f.connections[0];
        c.mcp = Some(McpAccess::Full);
        c.rules.view = AgentView::Data;
        c.rules.visible.items.clear();
        c.rules.sensitive[0].how = Mask::Partial;
        c.rules.read_rows = Some(500);
        c.yolo = true;
        c.allowed.push("DELETE FROM sessions".into());
        f.sensitive_data.set_builtin("email", Some(false), None);
        f.general.agent_access = true;
        let p = plan(&settings.get(), &config.connections(), &f);
        let loose: Vec<&str> = p.conns[0].changes.iter().filter(|c| c.loosens).map(|c| c.what.as_str()).collect();
        for want in ["MCP tier", "The agent sees", "Visible to agents › users.password_hash", "Sensitive column › users.email", "Rows per read", "YOLO", "Allowed without asking"] {
            assert!(loose.contains(&want), "{want} not flagged: {loose:?}");
        }
        assert!(p.types.iter().any(|c| c.what == "Sensitive data › E-mail" && c.loosens));
        assert!(p.general[0].loosens && p.loosens());
        let mut tight = export(&settings.get(), &config.connections());
        tight.connections[0].mcp = Some(McpAccess::Read);
        tight.connections[0].rules.view = AgentView::Structure;
        tight.connections[0].rules.read_rows = Some(20);
        let t = plan(&settings.get(), &config.connections(), &tight);
        assert!(t.changes() == 3 && !t.loosens(), "{t:?}");
    }
}
