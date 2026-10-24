//! Agents' settings as a file, to keep or to carry to another Mac: General, the Sensitive data
//! types and every connection's rules for agents. Never a password, the mask key, the audit or a
//! connection's address. An import is checked whole first, summarized (what loosens protection
//! flagged), then written all at once.

use serde::{Deserialize, Serialize};

use crate::config::{ConfigStore, ConnectionProfile, Engine, McpAccess};
use crate::privacy::{self, AgentRules, AgentView, ItemKind, Mask, TypeSettings, VisibleMode};
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

/// The largest file read: thousands of connections' rules fit well under it.
pub const MAX_FILE: u64 = 1024 * 1024;
const MAX_CONNECTIONS: usize = 10_000;
/// Per connection: visible items, sensitive columns, allowed statements (the summary compares
/// sensitive entries pairwise).
const MAX_ENTRIES: usize = 1000;

fn too_large() -> String {
    format!("Larger than {} MB: not a Dataman agents file.", MAX_FILE / (1024 * 1024))
}

/// Reads an agents file, refusing one too large before reading it.
pub fn read_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_FILE {
        return Err(std::io::Error::other(too_large()));
    }
    let mut text = String::new();
    file.take(MAX_FILE + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE {
        return Err(std::io::Error::other(too_large()));
    }
    Ok(text)
}

/// The keys the file may hold. A key this Dataman does not know (a typo, or a newer version's
/// setting) refuses the file: ignoring it could drop a protection the file meant to give.
enum Shape {
    Any,
    Fields(&'static [(&'static str, Shape)]),
    Each(&'static Shape),
    Builtins(&'static Shape),
}

const ON_MASK: Shape = Shape::Fields(&[("on", Shape::Any), ("mask", Shape::Any)]);
const CUSTOM: Shape = Shape::Fields(&[("id", Shape::Any), ("name", Shape::Any), ("pattern", Shape::Any), ("mask", Shape::Any), ("keep", Shape::Any), ("validator", Shape::Any), ("ignore_case", Shape::Any), ("on", Shape::Any)]);
const TYPES: Shape = Shape::Fields(&[("builtin", Shape::Builtins(&ON_MASK)), ("custom", Shape::Each(&CUSTOM))]);
const ITEM: Shape = Shape::Fields(&[("name", Shape::Any), ("kind", Shape::Any)]);
const SENSITIVE: Shape = Shape::Fields(&[("col", Shape::Any), ("how", Shape::Any), ("on", Shape::Any)]);
const REDIS: Shape = Shape::Fields(&[("read", Shape::Any), ("write", Shape::Any), ("keyspace", Shape::Any), ("scripting", Shape::Any), ("pubsub", Shape::Any)]);
const RULES: Shape = Shape::Fields(&[
    ("view", Shape::Any),
    ("visible", Shape::Fields(&[("mode", Shape::Any), ("items", Shape::Each(&ITEM))])),
    ("sensitive", Shape::Each(&SENSITIVE)),
    ("structure", Shape::Any),
    ("code", Shape::Any),
    ("activity", Shape::Any),
    ("redis", REDIS),
    ("time_limit_s", Shape::Any),
    ("read_rows", Shape::Any),
    ("read_bytes", Shape::Any),
    ("read_from", Shape::Any),
]);
const CONN: Shape = Shape::Fields(&[
    ("id", Shape::Any),
    ("name", Shape::Any),
    ("engine", Shape::Any),
    ("database", Shape::Any),
    ("mcp", Shape::Any),
    ("row_limit", Shape::Any),
    ("allowed", Shape::Any),
    ("yolo", Shape::Any),
    ("rules", RULES),
]);
const FILE: Shape = Shape::Fields(&[("version", Shape::Any), ("general", Shape::Fields(&[("agent_access", Shape::Any)])), ("sensitive_data", TYPES), ("connection", Shape::Each(&CONN))]);

/// Unknown keys and NUL characters, with where they are. Wrong types are left to serde.
fn check_shape(v: &toml::Value, shape: &Shape, at: &str, errs: &mut Vec<String>) {
    let here = |k: &str| if at.is_empty() { k.to_string() } else { format!("{at} › {k}") };
    let nul = |s: &str| s.contains('\0');
    match (shape, v) {
        (Shape::Fields(fields), toml::Value::Table(t)) => {
            for (k, x) in t {
                match fields.iter().find(|(n, _)| n == k) {
                    Some((_, s)) => check_shape(x, s, &here(k), errs),
                    None if at.is_empty() => errs.push(format!("“{}” is not a setting this Dataman knows (a typo, or a newer Dataman's).", k.escape_debug())),
                    None => errs.push(format!("{at}: “{}” is not a setting this Dataman knows (a typo, or a newer Dataman's).", k.escape_debug())),
                }
            }
        }
        (Shape::Each(s), toml::Value::Array(a)) => {
            for (i, x) in a.iter().enumerate() {
                let name = x.get("name").and_then(|n| n.as_str()).filter(|n| !n.trim().is_empty() && !nul(n)).map(str::to_string);
                check_shape(x, s, &here(&name.unwrap_or_else(|| (i + 1).to_string())), errs);
            }
        }
        (Shape::Builtins(s), toml::Value::Table(t)) => {
            for (k, x) in t {
                if privacy::builtin(k).is_none() {
                    errs.push(format!("{at}: “{}” is not a sensitive data type this Dataman knows.", k.escape_debug()));
                }
                check_shape(x, s, &here(k), errs);
            }
        }
        (_, toml::Value::String(s)) if nul(s) => errs.push(format!("{at}: a NUL character.")),
        // A list of strings (allowed): one level, never deeper.
        (Shape::Any, toml::Value::Array(a)) if a.iter().any(|x| x.as_str().is_some_and(nul)) => errs.push(format!("{at}: a NUL character.")),
        _ => {}
    }
}

/// The file, or everything wrong with it; nothing of it is used when anything is.
pub fn parse(text: &str) -> Result<AgentsFile, Vec<String>> {
    if text.len() as u64 > MAX_FILE {
        return Err(vec![too_large()]);
    }
    let table: toml::Table = toml::from_str(text).map_err(|e| vec![format!("Not a Dataman agents file: {}", e.message())])?;
    match table.get("version") {
        None => return Err(vec!["Not a Dataman agents file: it has no version.".into()]),
        Some(v) if v.as_integer() != Some(VERSION.into()) => return Err(vec![format!("Version {v} is not one this Dataman reads (it reads {VERSION}).")]),
        Some(_) => {}
    }
    let mut errs = Vec::new();
    let table = toml::Value::Table(table);
    check_shape(&table, &FILE, "", &mut errs);
    if !errs.is_empty() {
        return Err(errs);
    }
    let f: AgentsFile = table.try_into().map_err(|e: toml::de::Error| vec![format!("Not a Dataman agents file: {}", e.message())])?;
    if f.connections.len() > MAX_CONNECTIONS {
        errs.push(format!("{} connections, {MAX_CONNECTIONS} at most.", f.connections.len()));
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
    let mut ids = std::collections::HashSet::new();
    for c in &f.connections {
        let at = if c.name.trim().is_empty() { c.id.clone() } else { c.name.clone() };
        if c.id.trim().is_empty() && c.name.trim().is_empty() {
            errs.push("A connection with neither an id nor a name.".into());
        }
        if !c.id.is_empty() && !ids.insert(c.id.as_str()) {
            errs.push(format!("{at}: its id {} is used twice.", c.id));
        }
        if c.rules.visible.items.len() > MAX_ENTRIES || c.rules.sensitive.len() > MAX_ENTRIES || c.allowed.len() > MAX_ENTRIES {
            errs.push(format!("{at}: more than {MAX_ENTRIES} visible items, sensitive columns or allowed statements."));
            continue;
        }
        for a in &c.allowed {
            if let Err(e) = crate::guard::allowlist_entry(a, c.engine.dialect()) {
                errs.push(format!("{at}: allowed statement “{}”: {e}.", a.trim()));
            }
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
    /// Why the file cannot be imported here (nothing of it is written).
    pub problems: Vec<String>,
}

impl Plan {
    pub fn loosens(&self) -> bool {
        self.general.iter().chain(&self.types).chain(self.conns.iter().flat_map(|c| &c.changes)).any(|c| c.loosens)
    }

    pub fn changes(&self) -> usize {
        self.general.len() + self.types.len() + self.conns.iter().map(|c| c.changes.len()).sum::<usize>()
    }
}

/// The connection here a file entry is for: by its id, else by its name and engine (and its
/// database, when the file names one). `Err` holds how many share that name (0: none).
fn find<'a>(c: &ConnRules, profiles: &'a [ConnectionProfile]) -> Result<(&'a ConnectionProfile, bool), usize> {
    if let Some(p) = profiles.iter().find(|p| p.id == c.id) {
        return Ok((p, false));
    }
    let same: Vec<&ConnectionProfile> = profiles.iter().filter(|p| p.name == c.name && p.engine == c.engine && (c.database.is_empty() || p.database == c.database)).collect();
    match same.as_slice() {
        [p] => Ok((p, true)),
        _ => Err(same.len()),
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

pub fn conn_changes(p: &ConnectionProfile, c: &ConnRules) -> Vec<Change> {
    let mut out = Vec::new();
    let mut push = |what: &str, from: String, to: String, loosens: bool| {
        if from != to {
            out.push(Change { what: what.into(), from, to, loosens });
        }
    };
    let next = ConnectionProfile { mcp: c.mcp, ..p.clone() };
    let (a, b) = (p.mcp_access(), next.mcp_access());
    // Unset, the tier follows the environment (off once marked production); set, it stays.
    let tier = |q: &ConnectionProfile| match q.mcp {
        Some(m) => m.label().to_string(),
        None => format!("{} (default)", q.mcp_access().label()),
    };
    push("MCP tier", tier(p), tier(&next), b > a || p.mcp.is_none() && c.mcp.is_some_and(|m| m != McpAccess::Off));
    let (r0, r1) = (&p.mcp_rules, &c.rules);
    push("The agent sees", r0.view.label().into(), r1.view.label().into(), shielding(r1.view) < shielding(r0.view));
    // Visible to agents. Tables and schemas listed are hidden (Everything except) or all that
    // shows (Only these); columns listed are hidden in either mode. Names match ignoring case.
    let (m0, m1) = (r0.visible.mode, r1.visible.mode);
    let lists = |v: &privacy::Visible| v.items.iter().any(|i| i.kind != ItemKind::Column);
    if m0 != m1 {
        // Only these after Everything except shows what it lists, which the old list may have hidden.
        push("Visible to agents", m0.label().into(), m1.label().into(), m1 == VisibleMode::Except || lists(&r0.visible) && lists(&r1.visible));
    }
    let keys = |v: &privacy::Visible| v.items.iter().map(|i| (i.kind, i.name.to_lowercase(), i.name.clone())).collect::<Vec<_>>();
    let (v0, v1) = (keys(&r0.visible), keys(&r1.visible));
    let has = |v: &[(ItemKind, String, String)], k: ItemKind, n: &str| v.iter().any(|(k2, n2, _)| *k2 == k && n2 == n);
    for (kind, _, n) in v0.iter().filter(|(k, l, _)| !has(&v1, *k, l)) {
        let loosens = *kind == ItemKind::Column || m0 == VisibleMode::Except && m1 == VisibleMode::Except;
        push(&format!("Visible to agents › {n}"), format!("listed ({})", kind.label()), "not listed".into(), loosens);
    }
    for (kind, _, n) in v1.iter().filter(|(k, l, _)| !has(&v0, *k, l)) {
        push(&format!("Visible to agents › {n}"), "not listed".into(), format!("listed ({})", kind.label()), *kind != ItemKind::Column && m1 == VisibleMode::Only);
    }
    // Sensitive columns: the first entry that matches a column is its mask (names ignoring case).
    let mask_of = |r: &AgentRules, col: &str| r.sensitive.iter().find(|s| s.col.to_lowercase() == col && s.on).map(|s| s.how);
    let cols: Vec<(String, String)> = r0.sensitive.iter().chain(&r1.sensitive).map(|s| (s.col.to_lowercase(), s.col.clone())).fold(Vec::new(), |mut v, c| {
        if !v.iter().any(|(l, _)| *l == c.0) {
            v.push(c);
        }
        v
    });
    for (low, col) in &cols {
        let (m0, m1) = (mask_of(r0, low), mask_of(r1, low));
        let label = |m: Option<Mask>| m.map_or("not sensitive".to_string(), |m| m.label().to_string());
        let loosens = match (m0, m1) {
            (Some(_), None) => true,
            (Some(a), Some(b)) => strength(b) < strength(a),
            _ => false,
        };
        push(&format!("Sensitive column › {col}"), label(m0), label(m1), loosens);
    }
    // An entry with a weaker mask before one with a stronger can take columns from it (`*mail*`
    // before `users.email`): each such pair the file brings is a loosening.
    let weaker_first = |r: &AgentRules| {
        let on: Vec<&privacy::SensitiveCol> = r.sensitive.iter().filter(|s| s.on).collect();
        let mut pairs = Vec::new();
        for (i, a) in on.iter().enumerate() {
            for b in &on[i + 1..] {
                if strength(a.how) < strength(b.how) && !a.col.eq_ignore_ascii_case(&b.col) {
                    pairs.push((a.col.to_lowercase(), a.how, b.col.to_lowercase(), b.how, a.col.clone(), b.col.clone()));
                }
            }
        }
        pairs
    };
    let before = weaker_first(r0);
    for (al, ah, bl, bh, a, b) in weaker_first(r1) {
        if !before.iter().any(|x| (&x.0, x.1, &x.2, x.3) == (&al, ah, &bl, bh)) {
            push(&format!("Sensitive column › {b}"), bh.label().into(), format!("{} where {a}, before it, matches", ah.label()), true);
        }
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
    // Reads moved toward the primary: the user kept agents' load (and reads) off it.
    let rf = |r: &AgentRules| r.read_from.map_or("default".to_string(), |f| f.label().to_string());
    let away = |r: &AgentRules| match r.read_from {
        Some(privacy::ReadFrom::Secondary) => 2,
        Some(privacy::ReadFrom::SecondaryPreferred) => 1,
        _ => 0,
    };
    push("Read from", rf(r0), rf(r1), away(r1) < away(r0));
    for a in c.allowed.iter().filter(|a| !p.mcp_allowed.contains(a)) {
        push("Allowed without asking", "—".into(), a.clone(), true);
    }
    for a in p.mcp_allowed.iter().filter(|a| !c.allowed.contains(a)) {
        push("Allowed without asking", a.clone(), "—".into(), false);
    }
    push("YOLO", yes_no(p.mcp_yolo), yes_no(c.yolo), c.yolo && !p.mcp_yolo);
    out
}

pub fn type_changes(t0: &TypeSettings, t1: &TypeSettings) -> Vec<Change> {
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
                let rule = |t: &privacy::CustomType| {
                    let keep = if t.mask == Mask::Partial { format!(" (last {})", t.keep) } else { String::new() };
                    format!("{} · {}{keep} · {}{}", t.pattern, t.mask.label(), t.validator.label(), if t.ignore_case { " · any case" } else { "" })
                };
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
    // Each connection here is written once: two entries for it would show two summaries and
    // write only the last.
    let mut seen = std::collections::HashMap::<&str, usize>::new();
    for (i, c) in f.connections.iter().enumerate() {
        match find(c, profiles) {
            Ok((p, by_name)) => {
                if let Some(j) = seen.insert(&p.id, i) {
                    plan.problems.push(format!("{}: entries {} and {} of the file are both for this connection.", p.name, j + 1, i + 1));
                    continue;
                }
                let changes = conn_changes(p, c);
                if !changes.is_empty() || by_name {
                    plan.conns.push(ConnPlan { id: p.id.clone(), name: p.name.clone(), by_name, changes });
                }
            }
            Err(n) => {
                let db = if c.database.is_empty() { String::new() } else { format!(", {}", c.database) };
                let why = if n > 1 { format!(": {n} connections here have that name") } else { String::new() };
                plan.not_found.push(format!("{} ({}{db}){why}", c.name, c.engine.label()));
            }
        }
    }
    plan
}

/// A connection's part the import writes.
type Agent = (Option<McpAccess>, Option<u64>, Vec<String>, bool, AgentRules);

fn agent_of(p: &ConnectionProfile) -> Agent {
    (p.mcp, p.mcp_row_limit, p.mcp_allowed.clone(), p.mcp_yolo, p.mcp_rules.clone())
}

fn set_agent(p: &mut ConnectionProfile, a: Agent) {
    (p.mcp, p.mcp_row_limit, p.mcp_allowed, p.mcp_yolo, p.mcp_rules) = a;
}

/// Writes the file's rules; returns each touched connection's id, its part before and after.
fn apply_conns(profiles: &mut [ConnectionProfile], f: &AgentsFile) -> Vec<(String, Agent, Agent)> {
    let ids: Vec<(usize, usize)> = f.connections.iter().enumerate().filter_map(|(i, c)| find(c, profiles).ok().and_then(|(p, _)| profiles.iter().position(|x| x.id == p.id)).map(|at| (i, at))).collect();
    let mut touched = Vec::new();
    for (i, at) in ids {
        let (c, p) = (&f.connections[i], &mut profiles[at]);
        let before = agent_of(p);
        set_agent(p, (c.mcp, c.row_limit, c.allowed.clone(), c.yolo, c.rules.clone()));
        touched.push((p.id.clone(), before, agent_of(p)));
    }
    touched
}

/// Puts back what `apply_conns` wrote, where it is still what it wrote: anything changed
/// since (another window, an agent, a project sync) stays.
fn restore(profiles: &mut [ConnectionProfile], touched: Vec<(String, Agent, Agent)>) {
    for (id, before, after) in touched {
        if let Some(p) = profiles.iter_mut().find(|p| p.id == id)
            && agent_of(p) == after
        {
            set_agent(p, before);
        }
    }
}

fn apply_settings(s: &mut Settings, f: &AgentsFile) {
    s.mcp_enabled = f.general.agent_access;
    s.agent_types = f.sensitive_data.clone();
}

const STALE: &str = "Agents settings changed since this summary was made, so nothing was imported. Open the file again to see what it would change now.";

/// Writes the file's settings as `reviewed` (the summary the user approved, from `plan`)
/// showed them, all or nothing: when the connections or the settings no longer give that
/// summary, nothing is written. The connections first (one file, renamed into place), then the
/// settings; when those cannot be written, the connections' agent parts go back as they were.
/// Two files cannot be replaced at once: only a second failure, putting them back, leaves the
/// connections imported (the error says so).
pub fn import(config: &ConfigStore, settings: &SettingsStore, f: &AgentsFile, reviewed: &Plan) -> anyhow::Result<Plan> {
    import_with(config, settings, f, reviewed, || {})
}

fn import_with(config: &ConfigStore, settings: &SettingsStore, f: &AgentsFile, reviewed: &Plan, between: impl FnOnce()) -> anyhow::Result<Plan> {
    if !reviewed.problems.is_empty() {
        anyhow::bail!("{}", reviewed.problems.join(" "));
    }
    let s0 = settings.get();
    let mut done: Option<(Plan, Vec<(String, Agent, Agent)>)> = None;
    // The plan again, under the connections' lock and from the file on disk: what is written is
    // what it shows.
    config.edit_all(|g| {
        let now = plan(&s0, g, f);
        if now == *reviewed {
            let touched = apply_conns(g, f);
            done = Some((now, touched));
        }
    })?;
    let Some((now, touched)) = done else { anyhow::bail!(STALE) };
    between();
    let mut moved = false;
    let saved = settings.update(|s| {
        if (s.mcp_enabled, &s.agent_types) != (s0.mcp_enabled, &s0.agent_types) {
            moved = true;
        } else {
            apply_settings(s, f);
        }
    });
    if moved || saved.is_err() {
        let back = config.edit_all(|g| restore(g, touched));
        if saved.is_err() && !moved {
            // The settings were changed in memory before the write failed.
            let _ = settings.update(|s| {
                if (s.mcp_enabled, &s.agent_types) == (f.general.agent_access, &f.sensitive_data) {
                    (s.mcp_enabled, s.agent_types) = (s0.mcp_enabled, s0.agent_types.clone());
                }
            });
        }
        let why = match saved {
            Err(e) => format!("{e:#}"),
            Ok(()) => STALE.to_string(),
        };
        return Err(match back {
            Ok(()) => anyhow::anyhow!(why),
            Err(e) => anyhow::anyhow!("{why} The connections' rules were written and could not be put back: {e:#}"),
        });
    }
    Ok(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::{CustomType, SensitiveCol, VisibleItem};

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
        let done = import(&config, &settings, &f, &plan(&settings.get(), &config.connections(), &f)).unwrap();
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
        import(&config, &settings, &f, &p).unwrap();
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

    // ---- Audit: every way a file can give agents more ----

    fn rules_of(p: &ConnectionProfile) -> ConnRules {
        export(&Settings::default(), std::slice::from_ref(p)).connections.remove(0)
    }

    fn changes_after(p: &ConnectionProfile, edit: impl FnOnce(&mut ConnRules)) -> Vec<Change> {
        let mut c = rules_of(p);
        edit(&mut c);
        conn_changes(p, &c)
    }

    fn item(name: &str, kind: ItemKind) -> VisibleItem {
        VisibleItem { name: name.into(), kind }
    }

    fn col(c: &str, how: Mask) -> SensitiveCol {
        SensitiveCol { col: c.into(), how, on: true }
    }

    #[test]
    fn every_loosening_of_a_connection_is_flagged() {
        let loosens = |p: &ConnectionProfile, edit: &dyn Fn(&mut ConnRules)| {
            let ch = changes_after(p, edit);
            (ch.iter().any(|c| c.loosens), ch)
        };
        let mut cases: Vec<(&str, ConnectionProfile, Box<dyn Fn(&mut ConnRules)>)> = Vec::new();
        let mut p = ConnectionProfile::new("a");
        // Columns listed are hidden in either mode: one taken off an Only these list shows.
        p.mcp_rules.visible = privacy::Visible { mode: VisibleMode::Only, items: vec![item("public.orders", ItemKind::Table), item("public.orders.card", ItemKind::Column)] };
        cases.push(("Only these: a hidden column taken off", p.clone(), Box::new(|c| c.rules.visible.items.retain(|i| i.kind != ItemKind::Column))));
        p.mcp_rules.visible = privacy::Visible { mode: VisibleMode::Except, items: vec![item("users", ItemKind::Table)] };
        cases.push(("the same name, a table now a column", p.clone(), Box::new(|c| c.rules.visible.items[0].kind = ItemKind::Column)));
        p.mcp_rules.visible = privacy::Visible { mode: VisibleMode::Except, items: vec![item("secret", ItemKind::Table), item("x", ItemKind::Table)] };
        cases.push(("Everything except [secret, x] to Only these [x]: x shows", p.clone(), Box::new(|c| c.rules.visible = privacy::Visible { mode: VisibleMode::Only, items: vec![item("x", ItemKind::Table)] })));
        let mut m = ConnectionProfile::new("m");
        m.mcp_rules.view = AgentView::Masked;
        m.mcp_rules.sensitive = vec![col("users.email", Mask::Hidden), col("*mail*", Mask::Partial)];
        cases.push(("sensitive entries reordered: the first match wins", m.clone(), Box::new(|c| c.rules.sensitive.reverse())));
        m.mcp_rules.sensitive = vec![col("users.email", Mask::Hash)];
        cases.push(("a weaker entry put before a stronger one", m.clone(), Box::new(|c| c.rules.sensitive.insert(0, col("*mail*", Mask::Partial)))));
        cases.push(("a sensitive column turned off", m.clone(), Box::new(|c| c.rules.sensitive[0].on = false)));
        cases.push(("a sensitive column renamed", m.clone(), Box::new(|c| c.rules.sensitive[0].col = "users.e_mail".into())));
        let mut r = ConnectionProfile::new("r");
        r.engine = Engine::MongoDb;
        r.mcp_rules.read_from = Some(privacy::ReadFrom::Secondary);
        cases.push(("reads moved to the primary", r.clone(), Box::new(|c| c.rules.read_from = None)));
        let dev = ConnectionProfile::new("dev");
        cases.push(("the tier pinned: it no longer turns off when the connection is marked production", dev.clone(), Box::new(|c| c.mcp = Some(McpAccess::Read))));
        let mut prod = ConnectionProfile::new("prod");
        prod.env = crate::config::Environment::Production;
        cases.push(("production, default (off) to read", prod.clone(), Box::new(|c| c.mcp = Some(McpAccess::Read))));
        let mut lim = ConnectionProfile::new("lim");
        lim.mcp_row_limit = Some(10);
        lim.mcp_rules.time_limit_s = Some(10);
        lim.mcp_rules.read_rows = Some(20);
        lim.mcp_rules.read_bytes = Some(16 * 1024);
        cases.push(("rows per write back to the default", lim.clone(), Box::new(|c| c.row_limit = None)));
        cases.push(("time per call back to the default", lim.clone(), Box::new(|c| c.rules.time_limit_s = None)));
        cases.push(("rows per read back to the default", lim.clone(), Box::new(|c| c.rules.read_rows = None)));
        cases.push(("size per read back to the default", lim.clone(), Box::new(|c| c.rules.read_bytes = None)));
        let mut w = ConnectionProfile::new("w");
        w.mcp = Some(McpAccess::Write);
        w.mcp_allowed = vec!["DELETE FROM sessions WHERE id = ?".into()];
        cases.push(("an allowed statement broadened", w.clone(), Box::new(|c| c.allowed = vec!["DELETE FROM ?".into()])));
        cases.push(("YOLO", w.clone(), Box::new(|c| c.yolo = true)));
        let mut red = ConnectionProfile::new("red");
        red.engine = Engine::Redis;
        red.mcp_rules.redis = privacy::RedisCats { read: true, write: false, keyspace: false, scripting: false, pubsub: false };
        cases.push(("Redis @scripting", red.clone(), Box::new(|c| c.rules.redis.scripting = true)));
        let mut s = ConnectionProfile::new("s");
        (s.mcp_rules.structure, s.mcp_rules.code, s.mcp_rules.activity) = (false, false, false);
        cases.push(("code", s.clone(), Box::new(|c| c.rules.code = true)));
        cases.push(("activity", s.clone(), Box::new(|c| c.rules.activity = true)));
        let mut fails = Vec::new();
        for (what, p, edit) in &cases {
            let (l, ch) = loosens(p, edit.as_ref());
            if !l {
                fails.push(format!("{what}: {ch:?}"));
            }
        }
        assert!(fails.is_empty(), "not flagged:\n{}", fails.join("\n"));
        // Neither way round is a loosening when it only hides more.
        let mut t = ConnectionProfile::new("t");
        t.mcp_rules.visible = privacy::Visible { mode: VisibleMode::Only, items: vec![item("public.orders", ItemKind::Table)] };
        assert!(!changes_after(&t, |c| c.rules.visible.items.push(item("public.orders.card", ItemKind::Column))).iter().any(|c| c.loosens), "a column hidden in Only these");
        t.mcp_rules.visible = privacy::Visible { mode: VisibleMode::Except, items: Vec::new() };
        assert!(!changes_after(&t, |c| c.rules.visible = privacy::Visible { mode: VisibleMode::Only, items: Vec::new() }).iter().any(|c| c.loosens), "Only these, nothing listed");
        assert!(changes_after(&t, |c| c.rules.visible.items.push(item("Users", ItemKind::Table))).iter().all(|c| !c.loosens));
        t.mcp_rules.visible.items.push(item("Users", ItemKind::Table));
        assert!(changes_after(&t, |c| c.rules.visible.items[0].name = "users".into()).is_empty(), "names match ignoring case");
    }

    #[test]
    fn every_loosening_of_the_sensitive_types_is_flagged() {
        let mut t0 = TypeSettings::default();
        let mut ct = CustomType::new("Code", r"\bC-[0-9]{6}\b", Mask::Partial);
        ct.ignore_case = true;
        t0.custom.push(ct);
        let flagged = |edit: &dyn Fn(&mut TypeSettings)| {
            let mut t1 = t0.clone();
            edit(&mut t1);
            type_changes(&t0, &t1).iter().any(|c| c.loosens)
        };
        assert!(flagged(&|t| t.custom[0].keep = 12), "Partial keeps more characters");
        assert!(flagged(&|t| t.custom[0].ignore_case = false), "the pattern no longer ignores case");
        assert!(flagged(&|t| t.custom[0].pattern = r"\bC-[0-9]{7}\b".into()));
        assert!(flagged(&|t| t.custom[0].on = false));
        assert!(flagged(&|t| t.custom.clear()));
        assert!(flagged(&|t| t.set_builtin("card", Some(false), None)));
        assert!(flagged(&|t| t.set_builtin("email", None, Some(Mask::Partial))) || privacy::builtin("email").is_some_and(|b| b.mask == Mask::Partial));
        assert!(!flagged(&|t| t.custom.push(CustomType { on: false, ..CustomType::new("Off", "x+", Mask::Hidden) })), "a type added off gives nothing");
    }

    #[test]
    fn unknown_settings_are_refused() {
        let base = "version = 1\n[[connection]]\nid = \"a\"\nname = \"a\"\nengine = \"postgres\"\n";
        for (text, word) in [
            (format!("{base}yollo = true\n"), "yollo"),
            (format!("{base}[connection.rules]\nvieww = \"structure\"\n"), "vieww"),
            (format!("{base}[connection.rules.redis]\ndangerous = true\n"), "dangerous"),
            (format!("extra = 1\n{base}"), "extra"),
            ("version = 1\n[general]\nagent_access = false\nmcp_http = true\n".to_string(), "mcp_http"),
            ("version = 1\n[sensitive_data.builtin.passport]\non = true\n".to_string(), "passport"),
            ("version = 1\n[[sensitive_data.custom]]\nid = \"t\"\nname = \"t\"\npattern = \"x+\"\nignorecase = true\n".to_string(), "ignorecase"),
        ] {
            let errs = parse(&text).expect_err(&text);
            assert!(errs.iter().any(|e| e.contains(word)), "{word}: {errs:?}");
        }
        assert!(parse("[general]\nagent_access = true\n").unwrap_err()[0].contains("version"));
        assert!(parse(&format!("version = 1\n[[connection]]\nid = \"a\"\nname = \"a\\u0000b\"\nengine = \"postgres\"\n")).is_err(), "a NUL in a name");
    }

    #[test]
    fn one_connection_is_written_once() {
        let (_d, _dirs, config, settings) = setup();
        rich(&config, &settings);
        let mut f = export(&settings.get(), &config.connections());
        let twin = f.connections[0].clone();
        f.connections.push(twin.clone());
        assert!(parse(&to_text(&f)).unwrap_err().iter().any(|e| e.contains("twice")), "the same id twice");
        f.connections.pop();
        let mut by_name = twin;
        by_name.id = "from-elsewhere".into();
        by_name.rules.view = AgentView::Data;
        f.connections.push(by_name);
        let p = plan(&settings.get(), &config.connections(), &f);
        assert!(!p.problems.is_empty(), "one by id, one by name, the same connection: {p:?}");
        assert!(import(&config, &settings, &f, &p).is_err());
        let mut same = ConnectionProfile::new("prod-pg");
        same.database = "postgres".into();
        config.upsert(same).unwrap();
        let mut g = export(&settings.get(), &config.connections());
        g.connections.truncate(1);
        g.connections[0].id = "unknown-here".into();
        let p = plan(&settings.get(), &config.connections(), &g);
        assert!(p.conns.is_empty() && p.not_found.iter().any(|n| n.contains("2 connections")), "{p:?}");
    }

    #[test]
    fn a_name_match_needs_the_same_database() {
        let (_d, _dirs, config, settings) = setup();
        rich(&config, &settings);
        let mut f = export(&settings.get(), &config.connections());
        f.connections.truncate(1);
        (f.connections[0].id, f.connections[0].database) = ("elsewhere".into(), "billing".into());
        let p = plan(&settings.get(), &config.connections(), &f);
        assert!(p.conns.is_empty() && p.not_found.len() == 1, "{p:?}");
    }

    #[test]
    fn what_was_reviewed_is_what_is_written() {
        let (_d, _dirs, config, settings) = setup();
        rich(&config, &settings);
        let mut f = export(&settings.get(), &config.connections());
        f.connections[0].rules.view = AgentView::Structure;
        let reviewed = plan(&settings.get(), &config.connections(), &f);
        // Meanwhile: another window, an agent's update_connection, a project sync.
        let mut p = config.connections().into_iter().find(|p| p.name == "prod-pg").unwrap();
        p.mcp_rules.read_rows = Some(20);
        config.upsert(p).unwrap();
        let before = agent_parts(&config);
        let r = import(&config, &settings, &f, &reviewed);
        assert!(r.as_ref().is_err_and(|e| e.to_string().contains("changed")), "{r:?}");
        assert_eq!(agent_parts(&config), before, "nothing written");
        settings.update(|s| s.mcp_enabled = true).unwrap();
        let reviewed = plan(&settings.get(), &config.connections(), &f);
        settings.update(|s| s.agent_types.set_builtin("card", Some(false), None)).unwrap();
        assert!(import(&config, &settings, &f, &reviewed).is_err(), "a settings change meanwhile");
        assert_eq!(agent_parts(&config), before);
        let reviewed = plan(&settings.get(), &config.connections(), &f);
        assert_eq!(import(&config, &settings, &f, &reviewed).unwrap(), reviewed);
    }

    #[test]
    fn a_failed_settings_write_puts_back_only_what_the_import_wrote() {
        let (_d, dirs, config, settings) = setup();
        rich(&config, &settings);
        let mut f = export(&settings.get(), &config.connections());
        f.connections[0].rules.view = AgentView::Data;
        f.general.agent_access = true;
        let reviewed = plan(&settings.get(), &config.connections(), &f);
        let (s0, a0) = (settings.get(), agent_parts(&config));
        // The settings file cannot be replaced: a directory stands where it goes.
        let _ = std::fs::remove_file(dirs.settings_file());
        std::fs::create_dir_all(dirs.settings_file().join("x")).unwrap();
        let r = import_with(&config, &settings, &f, &reviewed, || {
            let mut p = config.connections().into_iter().find(|p| p.name == "orders-mongo").unwrap();
            p.host = "moved.example".into();
            p.mcp_rules.read_rows = Some(50);
            config.upsert(p).unwrap();
            let mut q = config.connections().into_iter().find(|p| p.name == "prod-pg").unwrap();
            q.color = Some("#ff0000".into());
            config.upsert(q).unwrap();
        });
        assert!(r.is_err());
        let now = config.connections();
        let mongo = now.iter().find(|p| p.name == "orders-mongo").unwrap();
        assert_eq!((mongo.host.as_str(), mongo.mcp_rules.read_rows), ("moved.example", Some(50)), "a change made meanwhile stays");
        let pg = now.iter().find(|p| p.name == "prod-pg").unwrap();
        assert_eq!((pg.color.as_deref(), &pg.mcp_rules), (Some("#ff0000"), &a0[0].5), "the import's own part goes back, the rest stays");
        assert_eq!((settings.get().mcp_enabled, settings.get().agent_types), (s0.mcp_enabled, s0.agent_types), "not left in memory either");
    }

    /// Every field set (struct literals, no `..`: a new field does not compile until it is here),
    /// so every key is written and must be one the file's shape knows.
    #[test]
    fn the_shape_knows_every_field() {
        let rules = AgentRules {
            view: AgentView::Masked,
            visible: privacy::Visible { mode: VisibleMode::Only, items: vec![item("public.t", ItemKind::Table)] },
            sensitive: vec![SensitiveCol { col: "t.c".into(), how: Mask::Hash, on: false }],
            structure: false,
            code: false,
            activity: false,
            redis: privacy::RedisCats { read: false, write: false, keyspace: false, scripting: false, pubsub: false },
            time_limit_s: Some(10),
            read_rows: Some(20),
            read_bytes: Some(1024),
            read_from: Some(privacy::ReadFrom::Secondary),
        };
        let custom = CustomType { id: "t_x".into(), name: "X".into(), pattern: "x+".into(), mask: Mask::Partial, keep: 2, validator: privacy::Validator::Luhn, ignore_case: true, on: false };
        let f = AgentsFile {
            version: VERSION,
            general: General { agent_access: false },
            sensitive_data: TypeSettings { builtin: [("card".to_string(), privacy::BuiltinOverride { on: Some(false), mask: Some(Mask::Hash) })].into(), custom: vec![custom] },
            connections: vec![ConnRules { id: "a".into(), name: "a".into(), engine: Engine::Postgres, database: "d".into(), mcp: Some(McpAccess::Full), row_limit: Some(5), allowed: vec!["DELETE FROM t WHERE id = ?".into()], yolo: true, rules }],
        };
        let text = to_text(&f);
        assert_eq!(parse(&text), Ok(f), "{text}");
    }

    #[test]
    fn size_and_shape() {
        let big = format!("version = 1\n{}", "# padding padding padding padding padding\n".repeat(60_000));
        assert!(parse(&big).unwrap_err()[0].contains("MB"), "too large");
        let deep = format!("version = 1\nx = {}{}\n", "[".repeat(200_000), "]".repeat(200_000));
        assert!(parse(&deep[..deep.len().min(MAX_FILE as usize)]).is_err());
        let mut many = String::from("version = 1\n");
        for i in 0..5000 {
            many.push_str(&format!("[[connection]]\nid = \"id{i}\"\nname = \"conn é ✓ {i}\"\nengine = \"postgres\"\n[connection.rules]\nread_rows = 20\n"));
        }
        let f = parse(&many).unwrap();
        let t = std::time::Instant::now();
        let p = plan(&Settings::default(), &[ConnectionProfile::new("conn é ✓ 7")], &f);
        assert!(t.elapsed().as_secs() < 5 && p.conns.len() == 1 && p.not_found.len() == 4999);
        let mut sens = String::from("version = 1\n[[connection]]\nid = \"a\"\nname = \"a\"\nengine = \"postgres\"\n");
        for i in 0..5000 {
            sens.push_str(&format!("[[connection.rules.sensitive]]\ncol = \"c{i}\"\nhow = \"partial\"\n"));
        }
        assert!(parse(&sens).is_err(), "too many sensitive columns");
    }

    #[test]
    fn the_file_holds_no_address() {
        let mut p = ConnectionProfile::new("addr");
        (p.host, p.port, p.user, p.database) = ("db.secret-host.example".into(), 6543, "owner_login".into(), "shop".into());
        p.path = Some("/Volumes/private/where.db".into());
        p.ssh = Some(crate::config::SshSettings { host: "bastion.secret.example".into(), port: Some(2222), user: "jumper".into(), auth: Default::default(), key_file: "/Users/x/.ssh/id_secret".into() });
        p.options.insert("sslrootcert".into(), "/certs/secret-root.pem".into());
        p.options.insert("application_name".into(), "secret_app".into());
        p.save_password = true;
        let text = to_text(&export(&Settings::default(), &[p]));
        for s in ["secret-host", "6543", "owner_login", "where.db", "bastion", "2222", "jumper", "id_secret", "secret-root", "secret_app", "save_password", "host", "port", "user", "ssh", "path", "options"] {
            assert!(!text.contains(s), "{s} in {text}");
        }
        assert!(text.contains("database = \"shop\""), "the database's name is how a connection is told apart");
    }
}
