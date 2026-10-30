//! Open Project: a folder's `.dataman/project.toml`, the settings a team versions with its code.
//! What the file holds is the project's own choice (`[share]`): connections (never a password or
//! a secret: each person's stay in their own keychain), the favorites saved on them, the agents'
//! rules and sensitive data types, and the SQL format. Dataman keeps the file and the local
//! files (connections.toml, settings.toml, favorites.toml) in step: a change on either side
//! reaches the other, and where both changed the project wins.
//!
//! Which connections are the project's: those in the file, and those created while the project
//! is open (not by an agent); Settings › Project adds or removes any. The favorites are those
//! saved on the project's connections. A connection or favorite the file drops is only no longer
//! shared: the local copy stays (a favorite another person deleted goes, when its connection is
//! still the project's).
//!
//! ```toml
//! version = 1
//!
//! [share]              # each part on by default; Settings › Project changes them
//! connections = true   # no password, no saved-password choice, no option naming a secret
//! favorites = true     # those saved on the project's connections, with their folders
//! agents = true        # each connection's rules for agents, and the sensitive data types
//! editor = true        # keyword case, indent, review edits before commit
//!
//! [editor]
//! keyword_case = "upper"
//! indent = 2
//! confirm_commit = true
//!
//! [agents.sensitive_data]
//!
//! [[connection]]       # as in connections.toml; a SQLite or DuckDB file inside the
//! id = "…"             # folder is written relative to it
//! name = "Shop"
//! engine = "sqlite"
//! path = "data/shop.sqlite"
//!
//! [[favorite]]         # as in favorites.toml
//! ```
//!
//! Never there either: the theme, fonts, zoom, keys, export folder, MCP on or off, the mask key
//! and the audit, which are each person's; nor what an agent made or changed.
//!
//! The file is a teammate's (or anyone's who can write to the repository), so it never loosens
//! a protection or moves a connection by itself: a file that would (a higher MCP tier, YOLO, an
//! allowed statement, less privacy, a sensitive data type off, production or read-only off,
//! weaker TLS, another host, port, user, SSH tunnel or option, a database file outside the
//! folder, a new connection to what a guarded one already reaches, or one named like one of
//! this person's) waits for their review ([`Review`]): nothing of it is applied, and nothing
//! written over it, until they accept that very file. A connection moved that way leaves its
//! saved password and SSH secret behind, as in MCP's update_connection.
//!
//! The file is the folder's own: a `.dataman` or `project.toml` that is a symbolic link, a
//! read-only one, one of another version, over 2 MB, with ids that repeat or are not plain, or
//! with control or invisible characters is not synced (Settings › Project says why). One gone
//! since (a checkout) is not written back; one changed during a sync is read again first.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::agents_config::{self, Change, ConnRules};
use crate::config::{ConfigStore, ConnectionProfile, Dirs, Environment, SslMode};
use crate::favorites::{Favorite, FavoritesStore, Folder};
use crate::format::KeywordCase;
use crate::privacy::TypeSettings;
use crate::secret::SecretStore;
use crate::settings::{Settings, SettingsStore};
use crate::stamp::FileStamp;

pub const DIR: &str = ".dataman";
pub const FILE: &str = "project.toml";
pub const VERSION: u32 = 1;

const HEADER: &str = "# Dataman project: the settings this folder's team shares (File › Open Project).\n# Never a password or a secret: each person's stay in their own keychain.\n\n";

pub fn file_in(root: &Path) -> PathBuf {
    root.join(DIR).join(FILE)
}

/// What the project shares; each part left out is neither written nor read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Share {
    pub connections: bool,
    pub favorites: bool,
    pub agents: bool,
    pub editor: bool,
}

impl Default for Share {
    fn default() -> Self {
        Share { connections: true, favorites: true, agents: true, editor: true }
    }
}

/// Settings › Editor's SQL format and the review before a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorPrefs {
    pub keyword_case: KeywordCase,
    pub indent: u8,
    pub confirm_commit: bool,
}

impl Default for EditorPrefs {
    fn default() -> Self {
        EditorPrefs::of(&Settings::default())
    }
}

impl EditorPrefs {
    pub fn of(s: &Settings) -> EditorPrefs {
        EditorPrefs { keyword_case: s.keyword_case, indent: s.indent, confirm_commit: s.confirm_commit }
    }

    fn set(self, s: &mut Settings) {
        (s.keyword_case, s.indent, s.confirm_commit) = (self.keyword_case, self.indent, self.confirm_commit);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsPrefs {
    #[serde(default)]
    pub sensitive_data: TypeSettings,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub version: u32,
    #[serde(default)]
    pub share: Share,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<EditorPrefs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentsPrefs>,
    #[serde(default, rename = "connection", skip_serializing_if = "Vec::is_empty", serialize_with = "ser_conns", deserialize_with = "de_conns")]
    pub connections: Vec<ConnectionProfile>,
    #[serde(default, rename = "folder", skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<Folder>,
    #[serde(default, rename = "favorite", skip_serializing_if = "Vec::is_empty")]
    pub favorites: Vec<Favorite>,
    /// Connections this version cannot read (a newer engine): written back as they are.
    #[serde(skip)]
    pub unread: Vec<toml::Table>,
}

impl ProjectFile {
    pub fn to_text(&self) -> String {
        let mut text = toml::to_string_pretty(self).unwrap_or_default();
        if !self.unread.is_empty()
            && let Ok(mut t) = toml::from_str::<toml::Table>(&text)
        {
            let list = t.entry("connection").or_insert_with(|| toml::Value::Array(Vec::new()));
            if let Some(a) = list.as_array_mut() {
                a.extend(self.unread.iter().cloned().map(toml::Value::Table));
            }
            text = toml::to_string_pretty(&t).unwrap_or(text);
        }
        format!("{HEADER}{text}")
    }

    /// The file, or why it is not used: another version, too large, or ids that are not
    /// unique or not plain.
    pub fn parse(text: &str) -> Result<ProjectFile> {
        anyhow::ensure!(text.len() <= MAX_BYTES, "the file is {} KB, {} KB at most", text.len() / 1024, MAX_BYTES / 1024);
        let mut f: ProjectFile = toml::from_str(text)?;
        anyhow::ensure!(f.version == VERSION, "version {} is not one this Dataman reads (it reads {VERSION})", f.version);
        let raw: toml::Table = toml::from_str(text)?;
        f.unread = raw.get("connection").and_then(|v| v.as_array()).into_iter().flatten().filter(|v| (*v).clone().try_into::<ConnectionProfile>().is_err()).filter_map(|v| v.as_table().cloned()).collect();
        // What is shown is what is dialled: no control, bidi or zero-width character.
        for p in &f.connections {
            let ssh = p.ssh.iter().flat_map(|s| [&s.host, &s.user, &s.key_file]);
            let fields = [&p.name, &p.host, &p.user, &p.database].into_iter().chain(p.path.as_ref()).chain(ssh).chain(p.options.iter().flat_map(|(k, v)| [k, v]));
            for v in fields {
                anyhow::ensure!(!v.chars().any(invisible), "the connection {} holds a control or invisible character ({v:?})", p.id);
            }
        }
        for name in f.favorites.iter().map(|x| &x.name).chain(f.folders.iter().map(|d| &d.name)) {
            anyhow::ensure!(!name.chars().any(invisible), "the name {name:?} holds a control or invisible character");
        }
        let plain = |id: &str| !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let mut seen = BTreeSet::new();
        for id in f.connections.iter().map(|p| &p.id) {
            anyhow::ensure!(plain(id), "the connection id {id:?} is not plain letters, digits, - or _");
            anyhow::ensure!(seen.insert(id.as_str()), "the connection id {id} is used twice");
        }
        let mut seen = BTreeSet::new();
        for id in f.favorites.iter().map(|x| &x.id).chain(f.folders.iter().map(|d| &d.id)) {
            anyhow::ensure!(plain(id), "the id {id:?} is not plain letters, digits, - or _");
            anyhow::ensure!(seen.insert(id.as_str()), "the id {id} is used twice");
        }
        Ok(f)
    }

    fn connection_ids(&self) -> BTreeSet<String> {
        self.connections.iter().map(|p| p.id.clone()).collect()
    }
}

/// Control characters, bidi controls and zero-width characters: text shown as one thing and
/// used as another.
fn invisible(c: char) -> bool {
    c.is_control() || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{061c}' | '\u{feff}' | '\u{00ad}')
}

/// A project's file is a few KB; one far larger is not read (nor parsed on every change).
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Each person's own, never in the file.
const PERSONAL: [&str; 3] = ["save_password", "agent_made", "from_project"];

/// A driver option that names a secret (a key's passphrase, a token…) stays local too.
pub fn secret_option(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["pass", "pwd", "secret", "token", "credential", "apikey", "api_key", "private", "bearer", "jwt", "sas", "authmechanismproperties"].iter().any(|w| k.contains(w))
}

fn ser_conns<S: serde::Serializer>(list: &[ConnectionProfile], s: S) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::{Error as _, SerializeSeq as _};
    let mut seq = s.serialize_seq(Some(list.len()))?;
    for p in list {
        let mut t = toml::Table::try_from(p).map_err(S::Error::custom)?;
        for k in PERSONAL {
            t.remove(k);
        }
        seq.serialize_element(&t)?;
    }
    seq.end()
}

/// Each connection on its own: one this version cannot read is left out, not the file.
fn de_conns<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<ConnectionProfile>, D::Error> {
    let list = Vec::<toml::Value>::deserialize(d)?;
    Ok(list
        .into_iter()
        .filter_map(|v| match v.clone().try_into::<ConnectionProfile>() {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!("project: skipping the connection {:?} ({e})", v.get("name").and_then(|n| n.as_str()).unwrap_or("?"));
                None
            }
        })
        .collect())
}

/// This person's side: what the local files hold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Local {
    pub connections: Vec<ConnectionProfile>,
    pub settings: Settings,
    pub favorites: Vec<Favorite>,
    pub folders: Vec<Folder>,
}

/// The local files a project syncs with.
#[derive(Clone, Copy)]
pub struct Stores<'a> {
    pub config: &'a ConfigStore,
    pub settings: &'a SettingsStore,
    pub favorites: &'a FavoritesStore,
    /// Where a moved connection's saved password is forgotten.
    pub secrets: &'a dyn SecretStore,
}

impl Local {
    pub fn read(st: Stores) -> Local {
        Local { connections: st.config.connections(), settings: st.settings.get(), favorites: st.favorites.list(), folders: st.favorites.folders() }
    }
}

/// A file database inside the project is written relative to it, so it opens on every clone.
fn to_project_path(path: &str, root: &Path) -> String {
    let full = crate::config::expand_home(path);
    match full.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => path.to_string(),
    }
}

/// `..` resolved without touching the disk.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Whether a file connection's database is somewhere other than inside the project's folder:
/// written outside it, or reached through a symlink (the deepest part that exists, resolved).
pub fn file_outside(path: &str, root: &Path) -> bool {
    if path.chars().any(char::is_control) {
        return true;
    }
    let full = lexical(&crate::config::expand_home(path));
    if !full.starts_with(root) {
        return true;
    }
    let mut at = full.as_path();
    loop {
        if let Ok(c) = std::fs::canonicalize(at) {
            return !c.starts_with(root);
        }
        match at.parent() {
            Some(p) => at = p,
            None => return true,
        }
    }
}

fn from_project_path(path: &str, root: &Path) -> String {
    let p = path.trim();
    if p.starts_with('/') || p.starts_with('~') || p.is_empty() {
        return path.to_string();
    }
    crate::config::tilde(&root.join(p).to_string_lossy())
}

fn shareable(p: &ConnectionProfile, agents: bool, root: &Path) -> ConnectionProfile {
    let mut p = p.clone();
    p.save_password = false;
    p.agent_made = false;
    p.from_project = false;
    p.options.retain(|k, _| !secret_option(k));
    p.path = p.path.as_deref().map(|path| to_project_path(path, root));
    if !agents {
        (p.mcp, p.mcp_row_limit, p.mcp_allowed, p.mcp_yolo, p.mcp_rules) = (None, None, Vec::new(), false, Default::default());
    }
    p
}

/// The project's connection over this person's copy: theirs is everything shared, ours what is
/// personal (the saved password choice, secret options, the agents' rules when not shared).
/// A moved connection keeps neither its saved password nor its secret options.
fn merged(mine: Option<&ConnectionProfile>, theirs: &ConnectionProfile, agents: bool, root: &Path) -> ConnectionProfile {
    let mut p = theirs.clone();
    p.path = theirs.path.as_deref().map(|path| from_project_path(path, root));
    p.agent_made = false;
    p.options.retain(|k, _| !secret_option(k));
    match mine {
        Some(m) => {
            if !agents {
                (p.mcp, p.mcp_row_limit, p.mcp_allowed, p.mcp_yolo, p.mcp_rules) = (m.mcp, m.mcp_row_limit, m.mcp_allowed.clone(), m.mcp_yolo, m.mcp_rules.clone());
            }
            let moved = moves(m, &p, root);
            p.save_password = m.save_password && !moved;
            p.from_project = m.from_project || moved;
            if !moved {
                p.options.extend(m.options.iter().filter(|(k, _)| secret_option(k)).map(|(k, v)| (k.clone(), v.clone())));
            }
        }
        None => {
            if !agents {
                (p.mcp, p.mcp_row_limit, p.mcp_allowed, p.mcp_yolo, p.mcp_rules) = (None, None, Vec::new(), false, Default::default());
            }
            p.save_password = false;
            p.from_project = true;
        }
    }
    p
}

/// What reaches a server, and with what: a change sends this person's password elsewhere.
pub(crate) fn target(p: &ConnectionProfile) -> impl PartialEq + '_ {
    let options: Vec<(&String, &String)> = p.options.iter().filter(|(k, _)| !secret_option(k)).collect();
    (p.engine, p.host.to_ascii_lowercase(), p.port, &p.user, &p.ssh, options)
}

/// To another server, tunnel or option, or to a database file outside the folder.
fn moves(old: &ConnectionProfile, new: &ConnectionProfile, root: &Path) -> bool {
    if old.engine.is_file() && new.engine.is_file() && old.engine == new.engine {
        // An option of a file database is what it may reach (DuckDB's external access).
        return old.path != new.path && file_outside(new.path.as_deref().unwrap_or(""), root) || target(old) != target(new);
    }
    target(old) != target(new)
}

fn env_rank(e: Environment) -> u8 {
    match e {
        Environment::None => 0,
        Environment::Dev => 1,
        Environment::Staging => 2,
        Environment::Production => 3,
    }
}

fn ssl_rank(m: SslMode) -> u8 {
    match m {
        SslMode::Disable => 0,
        SslMode::Prefer => 1,
        SslMode::Require => 2,
        SslMode::VerifyFull => 3,
    }
}

fn yes_no(b: bool) -> String {
    (if b { "yes" } else { "no" }).into()
}

fn ssh_text(p: &ConnectionProfile) -> String {
    p.ssh.as_ref().map(|s| format!("{}@{}:{} ({})", s.user, s.host, s.port.unwrap_or(22), s.auth.label())).unwrap_or_else(|| "none".into())
}

/// What `new` gives agents and the server over `old`, each line flagged when it loosens.
pub(crate) fn protections(old: &ConnectionProfile, new: &ConnectionProfile) -> Vec<Change> {
    let mut out = Vec::new();
    let mut push = |what: String, from: String, to: String, loosens: bool| {
        if from != to {
            out.push(Change { what, from, to, loosens });
        }
    };
    push("Environment".into(), old.env.label().into(), new.env.label().into(), env_rank(new.env) < env_rank(old.env));
    push("TLS".into(), old.sslmode.label().into(), new.sslmode.label().into(), ssl_rank(new.sslmode) < ssl_rank(old.sslmode));
    push("Read-only".into(), yes_no(old.read_only), yes_no(new.read_only), old.read_only && !new.read_only);
    let dbs: BTreeSet<&String> = old.read_only_databases.keys().chain(new.read_only_databases.keys()).collect();
    for db in dbs {
        let ro = |p: &ConnectionProfile| crate::config::read_only_in(p.read_only, &p.read_only_databases, p.engine, db);
        push(format!("Read-only › {db}"), yes_no(ro(old)), yes_no(ro(new)), ro(old) && !ro(new));
    }
    let (a, b) = (old.mcp_access(), new.mcp_access());
    push("MCP tier".into(), a.label().into(), b.label().into(), b > a);
    let rules = ConnRules {
        id: new.id.clone(),
        name: new.name.clone(),
        engine: new.engine,
        database: new.database.clone(),
        mcp: new.mcp,
        row_limit: new.mcp_row_limit,
        allowed: new.mcp_allowed.clone(),
        yolo: new.mcp_yolo,
        rules: new.mcp_rules.clone(),
    };
    out.extend(agents_config::conn_changes(old, &rules).into_iter().filter(|c| c.what != "MCP tier"));
    out
}

/// A connection of this person's that already reaches what `p` does (the same server as the
/// same user, or the same file).
fn twin<'a>(local: &'a [ConnectionProfile], p: &ConnectionProfile) -> Option<&'a ConnectionProfile> {
    let canonical = |q: &ConnectionProfile| std::fs::canonicalize(q.file_path()).ok();
    local.iter().find(|q| {
        if q.id == p.id {
            return false;
        }
        if p.engine.is_file() || q.engine.is_file() {
            return p.engine.is_file() && q.engine.is_file() && canonical(p).is_some_and(|c| Some(c) == canonical(q));
        }
        let host = |h: &str| h.trim().trim_end_matches('.').to_ascii_lowercase();
        q.engine == p.engine && host(&q.host) == host(&p.host) && q.port == p.port && q.user == p.user
    })
}

/// What a connection of the file does on this side that is this person's to decide; none
/// when nothing in it loosens.
fn review_conn(mine: Option<&ConnectionProfile>, next: &ConnectionProfile, local: &[ConnectionProfile], root: &Path) -> Option<ConnReview> {
    let moved = mine.is_some_and(|m| moves(m, next, root));
    let changes = match mine {
        Some(old) => {
            let mut changes = if moved { address_changes(old, next) } else { Vec::new() };
            changes.extend(protections(old, next));
            changes
        }
        None => arrival(next, local, Some(root)),
    };
    changes.iter().any(|c| c.loosens).then(|| ConnReview { id: next.id.clone(), name: next.name.clone(), new: mine.is_none(), moved, changes })
}

/// Where a connection now goes, line by line (each a loosening: the saved password was for the
/// old place).
pub(crate) fn address_changes(old: &ConnectionProfile, next: &ConnectionProfile) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut push = |what: &str, from: String, to: String| {
        if from != to {
            changes.push(Change { what: what.into(), from, to, loosens: true });
        }
    };
    push("Engine", old.engine.label().into(), next.engine.label().into());
    push("Host", old.host.clone(), next.host.clone());
    push("Port", old.port.to_string(), next.port.to_string());
    push("User", old.user.clone(), next.user.clone());
    push("SSH", ssh_text(old), ssh_text(next));
    push("File", old.path.clone().unwrap_or_default(), next.path.clone().unwrap_or_default());
    let keys: BTreeSet<&String> = old.options.keys().chain(next.options.keys()).filter(|k| !secret_option(k)).collect();
    for k in keys {
        let v = |p: &ConnectionProfile| p.options.get(k).cloned().unwrap_or_else(|| "—".into());
        push(&format!("Option › {k}"), v(old), v(next));
    }
    changes
}

/// What a connection this person did not have gives agents and the server: against a fresh one,
/// or against the connection of theirs that already reaches the same place. `root`: the
/// project's folder, outside which a database file is flagged.
pub(crate) fn arrival(next: &ConnectionProfile, local: &[ConnectionProfile], root: Option<&Path>) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut push = |what: &str, from: String, to: String, loosens: bool| {
        if from != to {
            changes.push(Change { what: what.into(), from, to, loosens });
        }
    };
    if let Some(root) = root
        && next.engine.is_file()
        && file_outside(next.path.as_deref().unwrap_or(""), root)
    {
        push("File", "—".into(), format!("{} (outside the project's folder)", next.path.clone().unwrap_or_default()), true);
    }
    if let Some(q) = local.iter().find(|q| q.id != next.id && q.name.trim().eq_ignore_ascii_case(next.name.trim())) {
        push("Name", "—".into(), format!("{}, as your connection {}", next.name, q.name), true);
    }
    // Agents name a connection by id first: an id that is one of this person's names
    // would take that name's calls.
    if let Some(q) = local.iter().find(|q| q.name.trim().eq_ignore_ascii_case(next.id.trim())) {
        push("Id", "—".into(), format!("{}, the name of your connection", q.name), true);
    }
    if !next.engine.is_file() && next.host.starts_with('/') {
        push("Host", "—".into(), format!("{} (a local socket: the server may let you in without a password)", next.host), true);
    }
    if let Some(v) = next.options.get("duckdb.external_access").filter(|v| v.as_str() != "false") {
        push("Option › duckdb.external_access", "—".into(), v.clone(), true);
    }
    // TLS off where it would be on by default is flagged; prefer is what the dialog offers.
    let tls = Some(crate::config::default_sslmode(next.engine, &next.host, next.ssh.is_some())).filter(|m| ssl_rank(*m) <= 1).unwrap_or(SslMode::Prefer);
    let fresh = ConnectionProfile { env: next.env, engine: next.engine, sslmode: tls, ..ConnectionProfile::new(next.name.clone()) };
    match twin(local, next) {
        Some(q) => {
            let looser: Vec<Change> = protections(q, next).into_iter().filter(|c| c.loosens).collect();
            if !looser.is_empty() {
                push("Reaches", "—".into(), format!("what your connection {} does, with less protection", q.name), true);
            }
            changes.extend(looser);
        }
        None => changes.extend(protections(&fresh, next).into_iter().filter(|c| c.loosens)),
    }
    changes
}

/// The project's file as this person's side would write it: `members` are its connections.
pub fn export(local: &Local, share: Share, members: &BTreeSet<String>, root: &Path) -> ProjectFile {
    let connections: Vec<ConnectionProfile> = if share.connections {
        local.connections.iter().filter(|p| members.contains(&p.id) && !p.agent_made).map(|p| shareable(p, share.agents, root)).collect()
    } else {
        Vec::new()
    };
    let favorites: Vec<Favorite> = if share.favorites {
        local.favorites.iter().filter(|f| !f.agent_made && f.connection_id.as_ref().is_some_and(|c| connections.iter().any(|p| p.id == *c))).cloned().collect()
    } else {
        Vec::new()
    };
    let folders = local.folders.iter().filter(|d| favorites.iter().any(|f| f.folder.as_ref() == Some(&d.id))).cloned().collect();
    ProjectFile {
        version: VERSION,
        share,
        editor: share.editor.then(|| EditorPrefs::of(&local.settings)),
        agents: share.agents.then(|| AgentsPrefs { sensitive_data: local.settings.agent_types.clone() }),
        connections,
        folders,
        favorites,
        unread: Vec::new(),
    }
}

/// What the project's file changes on this person's side.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changes {
    pub connections: Vec<ConnectionProfile>,
    pub favorites: Vec<Favorite>,
    pub removed_favorites: Vec<String>,
    pub folders: Vec<Folder>,
    pub editor: Option<EditorPrefs>,
    pub sensitive_data: Option<TypeSettings>,
    /// Moved to another server: their saved password and SSH secret are forgotten.
    pub moved: Vec<String>,
    /// What waits for this person's review before any of it is applied.
    pub review: Vec<ConnReview>,
    pub review_settings: Vec<Change>,
}

/// A connection the file adds or changes in a way that loosens protection or moves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnReview {
    pub id: String,
    pub name: String,
    /// Not on this Mac yet.
    pub new: bool,
    /// To another server or a file outside the folder: the saved password stays behind.
    pub moved: bool,
    pub changes: Vec<Change>,
}

/// A file that loosens protection or moves a connection, held until this person accepts it.
#[derive(Clone, Debug, PartialEq)]
pub struct Review {
    pub file: ProjectFile,
    pub conns: Vec<ConnReview>,
    pub settings: Vec<Change>,
    /// Dismissed: it stays held, without asking again, until the file changes.
    pub declined: bool,
}

impl Review {
    pub fn loosened(&self) -> usize {
        self.conns.iter().flat_map(|c| &c.changes).chain(&self.settings).filter(|c| c.loosens).count()
    }

    fn same(&self, other: &Review) -> bool {
        (&self.file, &self.conns, &self.settings) == (&other.file, &other.conns, &other.settings)
    }
}

/// The file `theirs` over `local`. `base` is the file as it was when both sides last agreed
/// (none the first time): a favorite in it that `theirs` dropped was deleted.
pub fn plan(local: &Local, theirs: &ProjectFile, base: Option<&ProjectFile>, root: &Path) -> Changes {
    let mut c = Changes::default();
    let conns = if theirs.share.connections { theirs.connections.as_slice() } else { &[] };
    for t in conns {
        let mine = local.connections.iter().find(|p| p.id == t.id);
        let p = merged(mine, t, theirs.share.agents, root);
        if mine != Some(&p) {
            if mine.is_some_and(|m| moves(m, &p, root)) {
                c.moved.push(p.id.clone());
            }
            c.review.extend(review_conn(mine, &p, &local.connections, root));
            c.connections.push(p);
        }
    }
    // Favorites of the project's connections only: never one of this person's own connections,
    // nor one that is theirs alone (an id taken from elsewhere).
    let shared: BTreeSet<&str> = conns.iter().map(|p| p.id.as_str()).collect();
    let theirs_fav = |f: &Favorite| theirs.share.favorites && f.connection_id.as_deref().is_some_and(|id| shared.contains(id));
    let favorites: Vec<&Favorite> = theirs.favorites.iter().filter(|f| theirs_fav(f)).filter(|f| local.favorites.iter().find(|x| x.id == f.id).is_none_or(|x| theirs_fav(x) && !x.agent_made)).collect();
    for d in &theirs.folders {
        if favorites.iter().any(|f| f.folder.as_ref() == Some(&d.id)) && local.folders.iter().find(|x| x.id == d.id) != Some(d) {
            c.folders.push(d.clone());
        }
    }
    for f in favorites {
        let f = Favorite { agent_made: false, ..f.clone() };
        if local.favorites.iter().find(|x| x.id == f.id) != Some(&f) {
            c.favorites.push(f);
        }
    }
    if let Some(base) = base {
        let shared = theirs.connection_ids();
        c.removed_favorites = base
            .favorites
            .iter()
            .filter(|f| !theirs.favorites.iter().any(|x| x.id == f.id))
            .filter(|f| f.connection_id.as_ref().is_some_and(|id| shared.contains(id)))
            .filter(|f| local.favorites.iter().any(|x| x.id == f.id))
            .map(|f| f.id.clone())
            .collect();
    }
    c.editor = theirs.editor.filter(|_| theirs.share.editor).filter(|e| *e != EditorPrefs::of(&local.settings));
    if let Some(e) = c.editor
        && local.settings.confirm_commit
        && !e.confirm_commit
    {
        c.review_settings.push(Change { what: "Review edits before commit".into(), from: "on".into(), to: "off".into(), loosens: true });
    }
    c.sensitive_data = theirs.agents.as_ref().filter(|_| theirs.share.agents).map(|a| a.sensitive_data.clone()).filter(|t| *t != local.settings.agent_types);
    if let Some(t) = &c.sensitive_data {
        c.review_settings.extend(agents_config::type_changes(&local.settings.agent_types, t).into_iter().filter(|c| c.loosens));
    }
    c
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Changes::default()
    }

    pub fn needs_review(&self) -> bool {
        !(self.review.is_empty() && self.review_settings.is_empty())
    }

    fn connections_into(&self, list: &mut Vec<ConnectionProfile>) {
        for p in &self.connections {
            match list.iter_mut().find(|c| c.id == p.id) {
                Some(c) => *c = p.clone(),
                None => list.push(p.clone()),
            }
        }
    }

    fn favorites_into(&self, items: &mut Vec<Favorite>, folders: &mut Vec<Folder>) {
        for d in &self.folders {
            match folders.iter_mut().find(|x| x.id == d.id) {
                Some(x) => *x = d.clone(),
                None => folders.push(d.clone()),
            }
        }
        items.retain(|f| !self.removed_favorites.contains(&f.id));
        for f in &self.favorites {
            match items.iter_mut().find(|x| x.id == f.id) {
                Some(x) => *x = f.clone(),
                None => items.push(f.clone()),
            }
        }
    }

    fn settings_into(&self, s: &mut Settings) {
        if let Some(e) = self.editor {
            e.set(s);
        }
        if let Some(t) = &self.sensitive_data {
            s.agent_types = t.clone();
        }
    }

    pub fn apply_to(&self, local: &mut Local) {
        self.connections_into(&mut local.connections);
        self.favorites_into(&mut local.favorites, &mut local.folders);
        self.settings_into(&mut local.settings);
    }

    /// Into the local files, each change on the file as it is on disk (another process may
    /// have written it since it was read).
    fn write(&self, st: Stores) -> Result<Synced> {
        let out = Synced {
            connections: !self.connections.is_empty(),
            favorites: !(self.favorites.is_empty() && self.removed_favorites.is_empty() && self.folders.is_empty()),
            settings: self.editor.is_some() || self.sensitive_data.is_some(),
            ..Synced::default()
        };
        if out.connections {
            st.config.edit_all(|list| self.connections_into(list))?;
        }
        if out.favorites {
            st.favorites.edit_all(|items, folders| self.favorites_into(items, folders))?;
        }
        if out.settings {
            st.settings.update(|s| self.settings_into(s))?;
        }
        for id in &self.moved {
            for key in [id.clone(), format!("{id}:ssh")] {
                if let Err(e) = st.secrets.delete(&key) {
                    log::warn!("project: forgetting the moved connection's secret: {e:#}");
                }
            }
        }
        Ok(out)
    }
}

/// What a sync changed: on this person's side (to show), and whether the file was written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Synced {
    pub connections: bool,
    pub settings: bool,
    pub favorites: bool,
    pub written: bool,
    /// A new review waits (see [`ProjectStore::review`]).
    pub review: bool,
}

impl Synced {
    pub fn local(&self) -> bool {
        self.connections || self.settings || self.favorites
    }
}

/// The open project as Settings shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    pub root: PathBuf,
    pub share: Share,
    pub connections: BTreeSet<String>,
    pub problem: Option<String>,
    pub review: Option<Review>,
}

#[derive(Default)]
struct State {
    root: Option<PathBuf>,
    /// The file as both sides last agreed on it.
    base: Option<ProjectFile>,
    stamp: Option<FileStamp>,
    /// The local connections at the last sync: one not among them was created since.
    known: Option<BTreeSet<String>>,
    problem: Option<String>,
    review: Option<Review>,
    /// The review this person accepted: the file it holds is applied once, as it was shown.
    accepted: Option<Review>,
    /// Open Project made the folder a project: its file is written even though it is not there.
    create: bool,
}

#[derive(Serialize, Deserialize)]
struct Base {
    root: String,
    project: ProjectFile,
}

pub struct ProjectStore {
    dirs: Dirs,
    state: Mutex<State>,
}

fn read(path: &Path) -> Result<Option<ProjectFile>> {
    let what = || format!("reading {}", path.display());
    match std::fs::metadata(path) {
        Ok(m) => anyhow::ensure!(m.len() <= MAX_BYTES as u64, "{}: the file is {} KB, {} KB at most", what(), m.len() / 1024, MAX_BYTES / 1024),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(what),
    }
    match std::fs::read_to_string(path) {
        Ok(s) => ProjectFile::parse(&s).with_context(what).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(what),
    }
}

/// A temporary file of this process's own, renamed into place: `false` when the file changed
/// since `read` (a pull during the sync), left for the next sync to read first.
fn write(path: &Path, f: &ProjectFile, read: Option<FileStamp>) -> Result<bool> {
    use std::io::Write as _;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().context("project path has no parent")?;
    if std::fs::metadata(path).is_ok_and(|m| m.permissions().readonly()) {
        anyhow::bail!("{} is read-only: not written", crate::config::tilde(&path.to_string_lossy()));
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{FILE}.{}.{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let r = (|| -> Result<bool> {
        std::fs::File::options().write(true).create_new(true).open(&tmp)?.write_all(f.to_text().as_bytes())?;
        if FileStamp::of(path) != read {
            return Ok(false);
        }
        std::fs::rename(&tmp, path)?;
        Ok(true)
    })();
    if !matches!(r, Ok(true)) {
        let _ = std::fs::remove_file(&tmp);
    }
    r.with_context(|| format!("writing {}", path.display()))
}

/// The other side's order (each person's sidebar is theirs): what is new comes last.
fn in_order_of<T>(list: &mut [T], theirs: &[T], id: impl Fn(&T) -> &str) {
    list.sort_by_key(|x| theirs.iter().position(|t| id(t) == id(x)).unwrap_or(usize::MAX));
}

/// What the open project holds, as another process (the MCP server) sees it: the connections
/// and favorites in its file, or in the last agreed copy. Agents change none of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shared {
    pub connections: BTreeSet<String>,
    pub favorites: BTreeSet<String>,
    /// Favorites saved on the project's connections join it at the next sync.
    pub favorites_follow: bool,
}

impl Shared {
    pub fn of(dirs: &Dirs, settings: &Settings) -> Shared {
        let Some(root) = settings.project.as_deref().filter(|_| settings.project_enabled).map(PathBuf::from) else { return Shared::default() };
        let mut out = Shared::default();
        let base = std::fs::read_to_string(dirs.project_base_file(&root)).ok().and_then(|t| toml::from_str::<Base>(&t).ok()).filter(|b| Path::new(&b.root) == root).map(|b| b.project);
        let file = read(&file_in(&root)).ok().flatten();
        for f in file.iter().chain(&base) {
            out.connections.extend(f.connection_ids());
            out.favorites.extend(f.favorites.iter().map(|x| x.id.clone()));
            out.favorites_follow |= f.share.connections && f.share.favorites;
        }
        if file.is_none() && base.is_none() {
            out.favorites_follow = true;
        }
        out
    }

    pub fn has_connection(&self, id: &str) -> bool {
        self.connections.contains(id)
    }

    pub fn has_favorite(&self, f: &Favorite) -> bool {
        self.favorites.contains(&f.id) || (self.favorites_follow && !f.agent_made && f.connection_id.as_ref().is_some_and(|c| self.connections.contains(c)))
    }
}

impl Dirs {
    /// A project's file as it was at its last sync (to tell deletions from additions), one per
    /// folder.
    pub fn project_base_file(&self, root: &Path) -> PathBuf {
        let h = root.as_os_str().as_encoded_bytes().iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3));
        self.config.join("projects").join(format!("{h:016x}.toml"))
    }
}

impl ProjectStore {
    pub fn open(dirs: &Dirs) -> ProjectStore {
        ProjectStore { dirs: dirs.clone(), state: Mutex::new(State::default()) }
    }

    fn read_base(&self, root: &Path) -> Option<ProjectFile> {
        let text = std::fs::read_to_string(self.dirs.project_base_file(root)).ok()?;
        let b: Base = toml::from_str(&text).ok()?;
        (Path::new(&b.root) == root).then_some(b.project)
    }

    /// Whole or not at all (a temporary file renamed into place), this user's only.
    fn write_base(&self, root: &Path, project: &ProjectFile) {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = self.dirs.project_base_file(root);
        let b = Base { root: root.display().to_string(), project: project.clone() };
        let r = (|| -> Result<()> {
            let text = toml::to_string_pretty(&b)?;
            let dir = path.parent().context("no parent")?;
            std::fs::create_dir_all(dir)?;
            let tmp = dir.join(format!(".{}.{}", std::process::id(), uuid::Uuid::new_v4().simple()));
            std::fs::File::options().write(true).create_new(true).mode(0o600).open(&tmp)?.write_all(text.as_bytes())?;
            std::fs::rename(&tmp, &path).inspect_err(|_| _ = std::fs::remove_file(&tmp))?;
            Ok(())
        })();
        if let Err(e) = r {
            log::warn!("project: {e:#}");
        }
    }

    /// The folder becomes the open project: a `.dataman` there is loaded over the local
    /// settings; without one it is created, sharing nothing of what is already here.
    pub fn open_project(&self, root: &Path, st: Stores) -> Result<(bool, Synced)> {
        anyhow::ensure!(st.settings.get().project_enabled, "projects are off in Settings › Project");
        let root = std::fs::canonicalize(root).with_context(|| format!("opening {}", root.display()))?;
        anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
        let created = !file_in(&root).exists();
        *self.state.lock() = State { root: Some(root.clone()), base: self.read_base(&root), create: created, ..State::default() };
        st.settings.update(|s| s.project = Some(root.display().to_string()))?;
        let synced = self.sync(st)?;
        Ok((created, synced))
    }

    /// What it brought stays; the files just stop syncing.
    pub fn close(&self, settings: &SettingsStore) -> Result<()> {
        *self.state.lock() = State::default();
        settings.update(|s| s.project = None)
    }

    pub fn info(&self, settings: &Settings) -> Option<Info> {
        let root = PathBuf::from(settings.project.as_deref().filter(|_| settings.project_enabled)?);
        let g = self.state.lock();
        let base = g.base.as_ref().filter(|_| g.root.as_deref() == Some(&root));
        let here = g.root.as_deref() == Some(&root);
        Some(Info {
            share: base.map_or_else(Share::default, |b| b.share),
            connections: base.map(ProjectFile::connection_ids).unwrap_or_default(),
            problem: g.problem.clone().filter(|_| here),
            review: g.review.clone().filter(|_| here),
            root,
        })
    }

    /// The file waiting for this person's review, if any.
    pub fn review(&self) -> Option<Review> {
        self.state.lock().review.clone()
    }

    /// Applies the reviewed file, as it was shown: when the file or this side changed since,
    /// a new review takes its place instead.
    pub fn accept(&self, st: Stores, reviewed: &Review) -> Result<Synced> {
        {
            let mut g = self.state.lock();
            anyhow::ensure!(g.review.as_ref().is_some_and(|r| r.same(reviewed)), "the project's file changed since: review it again");
            g.accepted = Some(reviewed.clone());
        }
        self.sync(st)
    }

    /// Kept held without asking again, until the file changes.
    pub fn decline(&self) {
        if let Some(r) = self.state.lock().review.as_mut() {
            r.declined = true;
        }
    }

    pub fn sync(&self, st: Stores) -> Result<Synced> {
        self.run(st, |_, _| {})
    }

    /// Changes what the project shares (Settings › Project), then syncs.
    pub fn edit(&self, st: Stores, f: impl FnOnce(&mut Share, &mut BTreeSet<String>)) -> Result<Synced> {
        self.run(st, f)
    }

    fn run(&self, st: Stores, edit: impl FnOnce(&mut Share, &mut BTreeSet<String>)) -> Result<Synced> {
        let settings = st.settings.get();
        let Some(root) = settings.project.as_deref().filter(|_| settings.project_enabled).map(PathBuf::from) else { return Ok(Synced::default()) };
        let mut g = self.state.lock();
        if g.root.as_deref() != Some(&root) {
            *g = State { base: self.read_base(&root), root: Some(root.clone()), ..State::default() };
        }
        if !root.is_dir() {
            g.problem = Some(format!("{} is not there", crate::config::tilde(&root.to_string_lossy())));
            return Ok(Synced::default());
        }
        let path = file_in(&root);
        // Read and written only as the folder's own: a link would take the sync elsewhere.
        if let Some(link) = [root.join(DIR), path.clone()].into_iter().find(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())) {
            let e = anyhow::anyhow!("{} is a symbolic link: the project is not synced through it", crate::config::tilde(&link.to_string_lossy()));
            g.problem = Some(format!("{e:#}"));
            return Err(e);
        }
        let stamp = FileStamp::of(&path);
        let theirs = if stamp.is_some_and(|s| s.settled()) && stamp == g.stamp {
            g.base.clone()
        } else {
            // A file that does not parse (a merge half done) is never written over.
            match read(&path) {
                Ok(t) => t,
                Err(e) => {
                    g.problem = Some(format!("{e:#}"));
                    return Err(e);
                }
            }
        };
        // Gone since (a checkout of a branch without it, a delete): never written back unasked.
        if theirs.is_none() && !g.create {
            g.problem = Some(format!("{} is not there: Open Project creates it again", crate::config::tilde(&path.to_string_lossy())));
            return Ok(Synced::default());
        }
        g.problem = None;
        let mut local = Local::read(st);
        let mut out = Synced::default();
        if let Some(t) = theirs.as_ref().filter(|t| g.base.as_ref() != Some(*t)) {
            let changes = plan(&local, t, g.base.as_ref(), &root);
            if changes.needs_review() {
                let mut review = Review { file: t.clone(), conns: changes.review.clone(), settings: changes.review_settings.clone(), declined: false };
                if !g.accepted.as_ref().is_some_and(|a| a.same(&review)) {
                    // Held whole: nothing applied, nothing written over the file.
                    let fresh = g.review.as_ref().is_none_or(|r| r.file != *t);
                    review.declined = !fresh && g.review.as_ref().is_some_and(|r| r.declined);
                    g.review = Some(review);
                    g.accepted = None;
                    return Ok(Synced { review: fresh, ..Synced::default() });
                }
            }
            out = changes.write(st)?;
            changes.apply_to(&mut local);
        }
        (g.review, g.accepted) = (None, None);
        let mut share = theirs.as_ref().map_or_else(Share::default, |t| t.share);
        let mut members = theirs.as_ref().map(ProjectFile::connection_ids).unwrap_or_default();
        let ids: BTreeSet<String> = local.connections.iter().map(|p| p.id.clone()).collect();
        if let Some(known) = &g.known {
            members.extend(local.connections.iter().filter(|p| !known.contains(&p.id) && !p.agent_made).map(|p| p.id.clone()));
        }
        edit(&mut share, &mut members);
        let mut mine = export(&local, share, &members, &root);
        if let Some(t) = &theirs {
            in_order_of(&mut mine.connections, &t.connections, |p| &p.id);
            in_order_of(&mut mine.favorites, &t.favorites, |f| &f.id);
            in_order_of(&mut mine.folders, &t.folders, |d| &d.id);
            mine.unread = t.unread.clone();
        }
        if theirs.as_ref() != Some(&mine) {
            match write(&path, &mine, stamp) {
                Ok(true) => out.written = true,
                // Changed meanwhile: the next sync reads it first.
                Ok(false) => return Ok(out),
                Err(e) => {
                    g.problem = Some(format!("{e:#}"));
                    return Err(e);
                }
            }
        }
        g.known = Some(ids);
        g.create = false;
        if g.base.as_ref() != Some(&mine) {
            self.write_base(&root, &mine);
            g.base = Some(mine);
        }
        g.stamp = FileStamp::of(&path);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Engine;

    fn conn(name: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::new(name);
        p.host = format!("{name}.db.internal");
        p
    }

    struct Machine {
        _dir: tempfile::TempDir,
        config: ConfigStore,
        settings: SettingsStore,
        favorites: FavoritesStore,
        project: ProjectStore,
        secrets: crate::secret::MemorySecrets,
    }

    impl Machine {
        fn new() -> Machine {
            let dir = tempfile::tempdir().unwrap();
            let dirs = Dirs::at(dir.path().join("dataman"));
            std::fs::create_dir_all(&dirs.config).unwrap();
            Machine {
                config: ConfigStore::open(&dirs).unwrap(),
                settings: SettingsStore::open(&dirs),
                favorites: FavoritesStore::open(&dirs).unwrap(),
                project: ProjectStore::open(&dirs),
                secrets: Default::default(),
                _dir: dir,
            }
        }

        fn st(&self) -> Stores<'_> {
            Stores { config: &self.config, settings: &self.settings, favorites: &self.favorites, secrets: &self.secrets }
        }

        fn sync(&self) -> Synced {
            self.project.sync(self.st()).unwrap()
        }
    }

    fn project_dir() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        (d, root)
    }

    #[test]
    fn the_file_never_holds_a_secret() {
        let mut p = conn("shop");
        p.save_password = true;
        p.from_project = true;
        p.options.insert("authSource".into(), "admin".into());
        p.options.insert("sslPassword".into(), "hunter2".into());
        p.options.insert("api_token".into(), "tok-123".into());
        let local = Local { connections: vec![p.clone()], ..Local::default() };
        let f = export(&local, Share::default(), &BTreeSet::from([p.id.clone()]), Path::new("/nowhere"));
        let text = f.to_text();
        for secret in ["hunter2", "tok-123", "save_password", "agent_made", "from_project"] {
            assert!(!text.contains(secret), "{secret} in\n{text}");
        }
        assert!(text.contains("authSource") && text.contains("shop.db.internal"), "{text}");
        let back = ProjectFile::parse(&text).unwrap();
        assert_eq!(back, f, "round trip");
    }

    #[test]
    fn the_project_wins_on_what_it_shares_and_the_rest_stays_personal() {
        let root = Path::new("/team/app");
        let mut mine = conn("shop");
        mine.save_password = true;
        mine.options.insert("sslpassword".into(), "mine".into());
        mine.mcp_row_limit = Some(5);
        let mut local = Local { connections: vec![mine.clone(), conn("personal")], ..Local::default() };
        local.settings.indent = 4;
        let mut theirs_conn = mine.clone();
        theirs_conn.name = "Shop (team)".into();
        theirs_conn.mcp_row_limit = Some(50);
        let theirs = ProjectFile {
            version: VERSION,
            share: Share { agents: false, ..Share::default() },
            editor: Some(EditorPrefs { keyword_case: KeywordCase::Lower, indent: 2, confirm_commit: false }),
            agents: None,
            connections: vec![shareable(&theirs_conn, false, root)],
            folders: Vec::new(),
            favorites: Vec::new(),
            unread: Vec::new(),
        };
        let c = plan(&local, &theirs, None, root);
        c.apply_to(&mut local);
        assert!(c.moved.is_empty() && c.review.is_empty(), "a new name moves nothing: {c:?}");
        let got = &local.connections[0];
        assert_eq!(got.name, "Shop (team)", "the project's name");
        assert!(got.save_password, "the saved password stays this person's choice");
        assert_eq!(got.options.get("sslpassword").map(String::as_str), Some("mine"));
        assert_eq!(got.mcp_row_limit, Some(5), "agents not shared: the local rules stay");
        assert_eq!(local.connections[1].name, "personal");
        assert_eq!((local.settings.keyword_case, local.settings.indent, local.settings.confirm_commit), (KeywordCase::Lower, 2, false));
        assert!(plan(&local, &theirs, Some(&theirs), root).is_empty(), "applied: nothing left to change");
        // Editor not shared: the local format is left alone.
        let quiet = ProjectFile { editor: None, share: Share { editor: false, ..theirs.share }, ..theirs.clone() };
        local.settings.indent = 4;
        assert_eq!(plan(&local, &quiet, None, root).editor, None);
    }

    #[test]
    fn file_databases_inside_the_project_are_relative() {
        let (_d, root) = project_dir();
        let path = crate::config::tilde(&root.join("data/shop.sqlite").to_string_lossy());
        let p = ConnectionProfile::new_file(Engine::Sqlite, path.clone());
        let outside = ConnectionProfile::new_file(Engine::DuckDb, "/elsewhere/x.duckdb");
        let local = Local { connections: vec![p.clone(), outside.clone()], ..Local::default() };
        let f = export(&local, Share::default(), &BTreeSet::from([p.id.clone(), outside.id.clone()]), &root);
        assert_eq!(f.connections[0].path.as_deref(), Some("data/shop.sqlite"));
        assert_eq!(f.connections[1].path.as_deref(), Some("/elsewhere/x.duckdb"));
        let c = plan(&Local::default(), &f, None, &root);
        assert_eq!(c.connections[0].path.as_deref(), Some(path.as_str()), "resolved in this clone");
    }

    #[test]
    fn favorites_follow_the_shared_connections_and_deletions_travel() {
        let root = Path::new("/team/app");
        let shared = conn("shared");
        let own = conn("own");
        let folder = Folder { id: "f1".into(), name: "Reports".into() };
        let mut a = Favorite::new("top", "SELECT 1");
        (a.connection_id, a.folder) = (Some(shared.id.clone()), Some("f1".into()));
        let mut b = Favorite::new("mine", "SELECT 2");
        b.connection_id = Some(own.id.clone());
        let loose = Favorite::new("loose", "SELECT 3");
        let local = Local {
            connections: vec![shared.clone(), own.clone()],
            favorites: vec![a.clone(), b.clone(), loose],
            folders: vec![folder.clone(), Folder { id: "f2".into(), name: "Empty".into() }],
            ..Local::default()
        };
        let f = export(&local, Share::default(), &BTreeSet::from([shared.id.clone()]), root);
        assert_eq!(f.favorites, vec![a.clone()]);
        assert_eq!(f.folders, vec![folder]);
        assert!(export(&local, Share { favorites: false, ..Share::default() }, &BTreeSet::from([shared.id.clone()]), root).favorites.is_empty());
        // Someone deleted it: it goes here too, but only with what was agreed before.
        let theirs = ProjectFile { favorites: Vec::new(), ..f.clone() };
        assert_eq!(plan(&local, &theirs, Some(&f), root).removed_favorites, vec![a.id.clone()]);
        assert!(plan(&local, &theirs, None, root).removed_favorites.is_empty());
        // Its connection left the project: the favorite is only no longer shared.
        let unshared = ProjectFile { favorites: Vec::new(), connections: Vec::new(), ..f.clone() };
        assert!(plan(&local, &unshared, Some(&f), root).removed_favorites.is_empty());
    }

    #[test]
    fn explorer_folders_stay_on_this_machine() {
        let (_d, root) = project_dir();
        let alice = Machine::new();
        alice.project.open_project(&root, alice.st()).unwrap();
        let shop = conn("shop");
        alice.config.upsert(shop.clone()).unwrap();
        assert!(alice.sync().written);
        let before = std::fs::read_to_string(file_in(&root)).unwrap();
        let mut d = crate::config::ConnFolder::new("Clients", &[]);
        d.color = Some("mint".into());
        d.connections.push(shop.id.clone());
        alice.config.edit_folders(|f| f.push(d)).unwrap();
        let s = alice.sync();
        assert!(!s.written && !s.review && !s.local(), "{s:?}");
        assert_eq!(std::fs::read_to_string(file_in(&root)).unwrap(), before);
        assert!(alice.project.review().is_none());
        // A teammate's sync leaves Alice's folders alone.
        let bob = Machine::new();
        bob.project.open_project(&root, bob.st()).unwrap();
        assert!(bob.config.folders().is_empty());
        assert!(!alice.sync().written);
        assert_eq!(alice.config.folders()[0].connections, vec![shop.id]);
    }

    #[test]
    fn a_team_shares_through_the_folder() {
        let (_d, root) = project_dir();
        let alice = Machine::new();
        let personal = conn("personal");
        alice.config.upsert(personal.clone()).unwrap();
        let (created, _) = alice.project.open_project(&root, alice.st()).unwrap();
        assert!(created && file_in(&root).exists());
        assert!(ProjectFile::parse(&std::fs::read_to_string(file_in(&root)).unwrap()).unwrap().connections.is_empty(), "what was there is not shared");
        // Created while the project is open: the project's.
        let shop = conn("shop");
        alice.config.upsert(shop.clone()).unwrap();
        let mut made = conn("by-agent");
        made.agent_made = true;
        alice.config.upsert(made).unwrap();
        let mut fav = Favorite::new("orders", "SELECT * FROM orders");
        fav.connection_id = Some(shop.id.clone());
        alice.favorites.add(fav.clone()).unwrap();
        alice.settings.update(|s| s.keyword_case = KeywordCase::Lower).unwrap();
        assert!(alice.sync().written);
        let text = std::fs::read_to_string(file_in(&root)).unwrap();
        assert!(text.contains("shop.db.internal") && !text.contains("personal") && !text.contains("by-agent") && text.contains("orders"), "{text}");
        assert!(!alice.sync().written, "nothing changed, nothing written");

        // Bob opens the same folder: he gets the project's settings over his own.
        let bob = Machine::new();
        bob.settings.update(|s| s.indent = 4).unwrap();
        let (created, synced) = bob.project.open_project(&root, bob.st()).unwrap();
        assert!(!created && synced.connections && synced.favorites && synced.settings);
        assert_eq!(bob.config.connections().iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["shop"]);
        assert!(!bob.config.connections()[0].save_password);
        assert_eq!(bob.favorites.list(), vec![fav.clone()]);
        assert_eq!((bob.settings.get().keyword_case, bob.settings.get().indent), (KeywordCase::Lower, 2));

        // Bob changes the address; Alice's next sync asks her first, then takes it.
        let mut moved = bob.config.get(&shop.id).unwrap();
        moved.port = 6543;
        bob.config.upsert(moved).unwrap();
        assert!(bob.sync().written);
        assert!(alice.sync().review);
        assert_eq!(alice.config.get(&shop.id).unwrap().port, shop.port, "held");
        assert!(alice.project.accept(alice.st(), &alice.project.review().unwrap()).unwrap().connections);
        assert_eq!(alice.config.get(&shop.id).unwrap().port, 6543);
        // Alice deletes the favorite; after Bob's sync it is gone there too.
        alice.favorites.remove(&fav.id).unwrap();
        assert!(alice.sync().written);
        assert!(bob.sync().favorites);
        assert!(bob.favorites.list().is_empty());

        // Settings › Project: Alice shares her personal connection and stops sharing the format.
        alice.project.edit(alice.st(), |share, members| {
            share.editor = false;
            members.insert(personal.id.clone());
        })
        .unwrap();
        let f = ProjectFile::parse(&std::fs::read_to_string(file_in(&root)).unwrap()).unwrap();
        assert_eq!((f.editor, f.connections.len()), (None, 2));
        let info = alice.project.info(&alice.settings.get()).unwrap();
        assert!(!info.share.editor && info.connections.contains(&personal.id) && info.root == root);
        bob.settings.update(|s| s.indent = 4).unwrap();
        bob.sync();
        assert_eq!(bob.settings.get().indent, 4, "the format is Bob's again");
        assert!(bob.config.get(&personal.id).is_some());

        // Closing keeps what came; a deleted connection stays shared on nobody's side.
        bob.project.close(&bob.settings).unwrap();
        assert_eq!(bob.sync(), Synced::default());
        assert_eq!(bob.config.connections().len(), 2);
    }

    #[test]
    fn a_file_that_does_not_parse_is_left_alone() {
        let (_d, root) = project_dir();
        let m = Machine::new();
        m.project.open_project(&root, m.st()).unwrap();
        let broken = "version = 1\n<<<<<<< HEAD\n[[connection]]\n";
        std::fs::write(file_in(&root), broken).unwrap();
        assert!(m.project.sync(m.st()).is_err());
        assert_eq!(std::fs::read_to_string(file_in(&root)).unwrap(), broken);
        assert!(m.project.info(&m.settings.get()).unwrap().problem.is_some());
    }

    #[test]
    fn projects_off_sync_nothing() {
        let (_d, root) = project_dir();
        let m = Machine::new();
        m.project.open_project(&root, m.st()).unwrap();
        m.settings.update(|s| s.project_enabled = false).unwrap();
        std::fs::remove_file(file_in(&root)).unwrap();
        m.config.upsert(conn("x")).unwrap();
        assert_eq!(m.sync(), Synced::default());
        assert!(!file_in(&root).exists());
        assert!(m.project.info(&m.settings.get()).is_none());
        assert!(m.project.open_project(&root, m.st()).is_err());
    }

    #[test]
    fn a_hand_edited_file_loads_what_it_can() {
        let text = "version = 1\n[share]\nfavorites = false\n[editor]\nindent = 4\n\n[[connection]]\nid = \"a\"\nname = \"ok\"\n\n[[connection]]\nid = \"b\"\nname = \"future\"\nengine = \"cassandra\"\n";
        let f = ProjectFile::parse(text).unwrap();
        assert_eq!(f.share, Share { favorites: false, ..Share::default() });
        assert_eq!(f.editor, Some(EditorPrefs { indent: 4, ..EditorPrefs::default() }));
        assert_eq!(f.connections.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["ok"]);
    }

    // A hostile project's file: each loosening, each move, waits for this person's review.

    use crate::config::{Environment, McpAccess, SshAuth, SshSettings, SslMode};
    use crate::privacy::{AgentView, ItemKind, Mask, SensitiveCol, VisibleItem};

    fn guarded(name: &str) -> ConnectionProfile {
        let mut p = conn(name);
        (p.env, p.read_only, p.sslmode, p.save_password) = (Environment::Production, true, SslMode::Require, true);
        p.mcp = Some(McpAccess::Read);
        p.mcp_rules.view = AgentView::Masked;
        p.mcp_rules.structure = false;
        p.mcp_rules.sensitive.push(SensitiveCol { col: "users.email".into(), how: Mask::Hidden, on: true });
        p.mcp_rules.visible.items.push(VisibleItem { name: "salaries".into(), kind: ItemKind::Table });
        p.mcp_rules.read_rows = Some(50);
        p.read_only_databases.insert("ledger".into(), true);
        p
    }

    /// A machine with the project open and `p` shared from it, its secrets saved.
    fn shared(p: &ConnectionProfile) -> (tempfile::TempDir, PathBuf, Machine) {
        let (d, root) = project_dir();
        let m = Machine::new();
        m.project.open_project(&root, m.st()).unwrap();
        m.config.upsert(p.clone()).unwrap();
        m.sync();
        m.secrets.set(&p.id, "hunter2").unwrap();
        m.secrets.set(&p.ssh_secret_id(), "ssh-secret").unwrap();
        assert!(on_disk(&root).connections.iter().any(|c| c.id == p.id), "shared");
        (d, root, m)
    }

    fn on_disk(root: &Path) -> ProjectFile {
        ProjectFile::parse(&std::fs::read_to_string(file_in(root)).unwrap()).unwrap()
    }

    fn hostile(root: &Path, f: impl FnOnce(&mut ProjectFile)) -> String {
        let mut file = on_disk(root);
        f(&mut file);
        let text = file.to_text();
        std::fs::write(file_in(root), &text).unwrap();
        text
    }

    #[test]
    fn every_loosening_or_move_from_the_file_waits_for_review() {
        type Edit = fn(&mut ConnectionProfile);
        let cases: Vec<(&str, Edit)> = vec![
            ("MCP read → write", |p| p.mcp = Some(McpAccess::Write)),
            ("MCP read → full", |p| p.mcp = Some(McpAccess::Full)),
            ("YOLO", |p| p.mcp_yolo = true),
            ("an allowed statement", |p| p.mcp_allowed.push("DELETE FROM users WHERE id = ?".into())),
            ("rows per write", |p| p.mcp_row_limit = Some(1_000_000)),
            ("masked → all data", |p| p.mcp_rules.view = AgentView::Data),
            ("a sensitive column dropped", |p| p.mcp_rules.sensitive.clear()),
            ("a sensitive column off", |p| p.mcp_rules.sensitive[0].on = false),
            ("a hidden table shown", |p| p.mcp_rules.visible.items.clear()),
            ("structure changes allowed", |p| p.mcp_rules.structure = true),
            ("rows per read", |p| p.mcp_rules.read_rows = Some(1_000_000)),
            ("production → none", |p| p.env = Environment::None),
            ("production → none, the tier by default", |p| (p.env, p.mcp) = (Environment::None, None)),
            ("read-only off", |p| p.read_only = false),
            ("a read-only database opened", |p| _ = p.read_only_databases.insert("ledger".into(), false)),
            ("TLS require → disable", |p| p.sslmode = SslMode::Disable),
            ("another host", |p| p.host = "db.attacker.example".into()),
            ("the host's case only, then a port", |p| (p.host, p.port) = (p.host.to_uppercase(), 15432)),
            ("another user", |p| p.user = "admin".into()),
            ("another engine", |p| p.engine = Engine::MySql),
            ("an SSH tunnel", |p| p.ssh = Some(SshSettings { host: "bastion.attacker.example".into(), port: None, user: "x".into(), auth: SshAuth::Agent, key_file: String::new() })),
            ("an option", |p| _ = p.options.insert("hostaddr".into(), "203.0.113.9".into())),
        ];
        for (what, edit) in cases {
            let prod = guarded("prod");
            let (_d, root, m) = shared(&prod);
            let before = m.config.get(&prod.id).unwrap();
            let text = hostile(&root, |f| edit(f.connections.iter_mut().find(|c| c.id == prod.id).unwrap()));
            let s = m.sync();
            assert!(s.review && !s.local() && !s.written, "{what}: {s:?}");
            let r = m.project.review().unwrap_or_else(|| panic!("{what}: no review"));
            assert!(r.loosened() > 0 && r.conns[0].id == prod.id, "{what}: {r:?}");
            assert_eq!(m.config.get(&prod.id).unwrap(), before, "{what}: applied before the review");
            assert_eq!(std::fs::read_to_string(file_in(&root)).unwrap(), text, "{what}: the file was written over");
            assert_eq!(m.secrets.get(&prod.id).unwrap().as_deref(), Some("hunter2"), "{what}");
            let again = m.sync();
            assert!(!again.review && !again.local() && m.project.review().is_some(), "{what}: held, not asked twice");
            assert!(m.project.info(&m.settings.get()).unwrap().review.is_some(), "{what}: Settings › Project shows it");
        }
    }

    #[test]
    fn what_only_tightens_or_names_syncs_without_asking() {
        let prod = guarded("prod");
        let (_d, root, m) = shared(&prod);
        hostile(&root, |f| {
            let c = &mut f.connections[0];
            (c.name, c.color, c.database) = ("Prod (team)".into(), Some("red".into()), "orders".into());
            (c.mcp, c.sslmode, c.mcp_rules.read_rows) = (Some(McpAccess::Off), SslMode::VerifyFull, Some(10));
        });
        let s = m.sync();
        assert!(s.connections && !s.review, "{s:?}");
        let got = m.config.get(&prod.id).unwrap();
        assert_eq!((got.name.as_str(), got.mcp, got.sslmode, got.save_password), ("Prod (team)", Some(McpAccess::Off), SslMode::VerifyFull, true));
        assert_eq!(m.secrets.get(&prod.id).unwrap().as_deref(), Some("hunter2"));
    }

    #[test]
    fn an_accepted_move_leaves_the_saved_password_behind() {
        let prod = guarded("prod");
        let (_d, root, m) = shared(&prod);
        hostile(&root, |f| f.connections[0].host = "db.elsewhere.example".into());
        assert!(m.sync().review);
        let r = m.project.review().unwrap();
        assert!(r.conns[0].moved && r.conns[0].changes.iter().any(|c| c.what == "Host" && c.loosens), "{r:?}");
        m.project.decline();
        assert!(m.project.review().unwrap().declined && !m.sync().review, "declined: held without asking again");
        let s = m.project.accept(m.st(), &m.project.review().unwrap()).unwrap();
        assert!(s.connections && !s.review, "{s:?}");
        let got = m.config.get(&prod.id).unwrap();
        assert_eq!(got.host, "db.elsewhere.example");
        assert!(!got.save_password && got.from_project, "{got:?}");
        assert_eq!(m.secrets.get(&prod.id).unwrap(), None, "the password stays behind");
        assert_eq!(m.secrets.get(&prod.ssh_secret_id()).unwrap(), None, "and the SSH secret");
        assert!(m.project.review().is_none() && !m.sync().local());
    }

    #[test]
    fn an_accepted_review_applies_only_the_file_it_showed() {
        let prod = guarded("prod");
        let (_d, root, m) = shared(&prod);
        hostile(&root, |f| f.connections[0].mcp = Some(McpAccess::Write));
        assert!(m.sync().review);
        let shown = m.project.review().unwrap();
        // Changed again before the click: the accepted review is not this file.
        hostile(&root, |f| (f.connections[0].mcp, f.connections[0].mcp_yolo) = (Some(McpAccess::Full), true));
        assert!(m.sync().review, "a new review");
        assert!(m.project.accept(m.st(), &shown).is_err());
        assert_eq!(m.config.get(&prod.id).unwrap().mcp, Some(McpAccess::Read));
        let now = m.project.review().unwrap();
        assert!(now.conns[0].changes.iter().any(|c| c.what == "YOLO"), "{now:?}");
        m.project.accept(m.st(), &now).unwrap();
        let got = m.config.get(&prod.id).unwrap();
        assert!(got.mcp == Some(McpAccess::Full) && got.mcp_yolo);
    }

    #[test]
    fn sensitive_types_and_the_commit_review_wait_too() {
        let (_d, root) = project_dir();
        let m = Machine::new();
        m.settings.update(|s| s.agent_types.custom.push(crate::privacy::CustomType::new("Customer code", r"\bCUST-[0-9]{6}\b", Mask::Hash))).unwrap();
        m.project.open_project(&root, m.st()).unwrap();
        let types = m.settings.get().agent_types;
        hostile(&root, |f| f.agents = Some(AgentsPrefs::default()));
        assert!(m.sync().review);
        assert!(m.project.review().unwrap().settings.iter().any(|c| c.what.contains("Customer code") && c.loosens));
        assert_eq!(m.settings.get().agent_types, types);
        hostile(&root, |f| {
            f.agents = Some(AgentsPrefs { sensitive_data: types.clone() });
            f.editor.as_mut().unwrap().confirm_commit = false;
        });
        m.sync();
        assert!(m.project.review().unwrap().settings.iter().any(|c| c.what == "Review edits before commit"));
        assert!(m.settings.get().confirm_commit);
    }

    #[test]
    fn new_connections_that_reach_further_wait_for_review() {
        let (_d, root) = project_dir();
        let mut off = conn("vault");
        off.mcp = Some(McpAccess::Off);
        let local = Local { connections: vec![off.clone()], ..Local::default() };
        let file = |p: ConnectionProfile| ProjectFile { connections: vec![p], ..export(&Local::default(), Share::default(), &BTreeSet::new(), &root) };
        let held = |p: ConnectionProfile| plan(&local, &file(p), None, &root).review;
        // A plain new connection, a new SQLite file inside the folder: no review.
        assert!(held(conn("plain")).is_empty());
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/shop.sqlite"), b"").unwrap();
        assert!(held(ConnectionProfile::new_file(Engine::Sqlite, "data/shop.sqlite")).is_empty());
        // More than a new connection gets.
        let mut full = conn("wide");
        (full.mcp, full.mcp_yolo) = (Some(McpAccess::Full), true);
        assert!(!held(full).is_empty(), "full + YOLO");
        // The vault's server and user, without its MCP off.
        let mut twin = conn("other");
        (twin.host, twin.port, twin.user) = (off.host.clone(), off.port, off.user.clone());
        let r = held(twin);
        assert!(r.first().is_some_and(|r| r.changes.iter().any(|c| c.what == "Reaches")), "{r:?}");
        // Named like this person's connection.
        let mut named = conn("x");
        named.name = "VAULT ".into();
        assert!(!held(named).is_empty());
        // TLS off to a remote server.
        let mut clear = conn("clear");
        clear.sslmode = SslMode::Disable;
        assert!(!held(clear).is_empty());
        // Files outside the folder: absolute, ~, .., through a symlink.
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("chat.db");
        std::fs::write(&secret, b"").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("data/link.sqlite")).unwrap();
        for path in [secret.display().to_string(), "~/Library/Messages/chat.db".into(), "data/../../chat.db".into(), "data/link.sqlite".into(), "data/x\n.sqlite".into()] {
            let r = held(ConnectionProfile::new_file(Engine::Sqlite, path.clone()));
            assert!(r.first().is_some_and(|r| r.changes.iter().any(|c| c.what == "File" && c.loosens)), "{path}: {r:?}");
        }
        // An id that is one of this person's names; a local socket.
        let mut by_id = conn("y");
        by_id.id = "Vault".into();
        assert!(held(by_id).first().is_some_and(|r| r.changes.iter().any(|c| c.what == "Id")));
        let mut socket = conn("sock");
        socket.host = "/var/run/postgresql".into();
        assert!(!held(socket).is_empty());
        // DuckDB with the file access its connections are made without.
        let mut duck = ConnectionProfile::new_file(Engine::DuckDb, "data/shop.sqlite");
        duck.options.insert("duckdb.external_access".into(), "true".into());
        assert!(!held(duck.clone()).is_empty());
        let mut closed = duck.clone();
        closed.options.insert("duckdb.external_access".into(), "false".into());
        let mut opened = closed.clone();
        opened.options.remove("duckdb.external_access");
        let local2 = Local { connections: vec![ConnectionProfile { path: Some(crate::config::tilde(&root.join("data/shop.sqlite").to_string_lossy())), ..closed }], ..Local::default() };
        let c = plan(&local2, &file(opened), None, &root);
        assert!(!c.review.is_empty(), "file access turned back on: {c:?}");
        // agents not shared: a new connection takes none of the file's agent rules.
        let mut f = file({
            let mut p = conn("quiet");
            p.mcp = Some(McpAccess::Full);
            p
        });
        f.share.agents = false;
        let c = plan(&local, &f, None, &root);
        assert!(c.review.is_empty() && c.connections[0].mcp.is_none() && c.connections[0].from_project, "{c:?}");
    }

    #[test]
    fn the_file_changes_favorites_of_its_own_connections_only() {
        let root = Path::new("/team/app");
        let mine = conn("mine");
        let team = conn("team");
        let mut fav = Favorite::new("cleanup", "SELECT 1");
        fav.connection_id = Some(mine.id.clone());
        let local = Local { connections: vec![mine.clone(), team.clone()], favorites: vec![fav.clone()], ..Local::default() };
        let mut evil = fav.clone();
        evil.sql = "DELETE FROM orders".into();
        let mut stray = Favorite::new("stray", "DROP TABLE users");
        stray.connection_id = Some(mine.id.clone());
        let mut takeover = evil.clone();
        takeover.connection_id = Some(team.id.clone());
        let file = ProjectFile { connections: vec![shareable(&team, true, root)], favorites: vec![evil, stray, takeover], ..export(&Local::default(), Share::default(), &BTreeSet::new(), root) };
        let c = plan(&local, &file, None, root);
        assert!(c.favorites.is_empty(), "{:?}", c.favorites);
        // An agent's favorite and connection never join the project.
        let mut by_agent = Favorite::new("agent's", "SELECT 2");
        (by_agent.connection_id, by_agent.agent_made) = (Some(team.id.clone()), true);
        let mut made = conn("made");
        made.agent_made = true;
        let local = Local { connections: vec![team.clone(), made.clone()], favorites: vec![by_agent], ..Local::default() };
        let f = export(&local, Share::default(), &BTreeSet::from([team.id.clone(), made.id.clone()]), root);
        assert_eq!((f.connections.len(), f.favorites.len()), (1, 0), "{f:?}");
    }

    #[test]
    fn files_this_version_does_not_read_are_refused_whole() {
        let ok = "version = 1\n[[connection]]\nid = \"a\"\nname = \"a\"\n";
        assert!(ProjectFile::parse(ok).is_ok());
        for (bad, why) in [
            ("version = 2\n", "version 2"),
            ("[share]\n", "version"),
            ("version = 1\n[[connection]]\nid = \"a\"\nname = \"a\"\n[[connection]]\nid = \"a\"\nname = \"b\"\nhost = \"evil\"\n", "used twice"),
            ("version = 1\n[[connection]]\nid = \"a:ssh\"\nname = \"a\"\n", "not plain"),
            ("version = 1\n[[favorite]]\nid = \"\"\nname = \"a\"\nsql = \"\"\n", "not plain"),
        ] {
            let e = format!("{:#}", ProjectFile::parse(bad).unwrap_err());
            assert!(e.contains(why), "{bad:?}: {e}");
        }
        let huge = format!("version = 1\n# {}\n", "x".repeat(MAX_BYTES));
        assert!(format!("{:#}", ProjectFile::parse(&huge).unwrap_err()).contains("at most"));
    }

    #[test]
    fn a_linked_dataman_folder_or_file_is_never_synced_through() {
        for linked in [DIR, FILE] {
            let (_d, root) = project_dir();
            let m = Machine::new();
            m.project.open_project(&root, m.st()).unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            let target = elsewhere.path().join("target");
            if linked == DIR {
                std::fs::remove_dir_all(root.join(DIR)).unwrap();
                std::fs::create_dir(&target).unwrap();
                std::os::unix::fs::symlink(&target, root.join(DIR)).unwrap();
            } else {
                std::fs::write(&target, "version = 1\n").unwrap();
                std::fs::remove_file(file_in(&root)).unwrap();
                std::os::unix::fs::symlink(&target, file_in(&root)).unwrap();
            }
            m.config.upsert(conn("new")).unwrap();
            assert!(m.project.sync(m.st()).is_err(), "{linked}");
            assert!(m.project.info(&m.settings.get()).unwrap().problem.is_some_and(|p| p.contains("symbolic link")));
            let inside: Vec<_> = std::fs::read_dir(elsewhere.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
            assert_eq!(inside, ["target"], "{linked}: nothing written there");
            if linked == FILE {
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "version = 1\n");
                assert!(std::fs::symlink_metadata(file_in(&root)).unwrap().file_type().is_symlink(), "the link stays");
            } else {
                assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn a_file_gone_since_is_not_written_back() {
        let (_d, root) = project_dir();
        let m = Machine::new();
        m.project.open_project(&root, m.st()).unwrap();
        m.config.upsert(conn("team")).unwrap();
        assert!(m.sync().written);
        std::fs::remove_file(file_in(&root)).unwrap();
        m.config.upsert(conn("later")).unwrap();
        assert_eq!(m.sync(), Synced::default());
        assert!(!file_in(&root).exists(), "a checkout without it stays without it");
        assert!(m.project.info(&m.settings.get()).unwrap().problem.is_some_and(|p| p.contains("not there")));
        let (created, _) = m.project.open_project(&root, m.st()).unwrap();
        assert!(created && file_in(&root).exists(), "Open Project creates it again");
    }

    #[test]
    fn what_other_processes_see_as_the_project() {
        let (_d, root) = project_dir();
        let m = Machine::new();
        m.project.open_project(&root, m.st()).unwrap();
        let team = conn("team");
        m.config.upsert(team.clone()).unwrap();
        m.sync();
        let dirs = Dirs::at(m._dir.path().join("dataman"));
        let sh = Shared::of(&dirs, &m.settings.get());
        assert!(sh.has_connection(&team.id) && !sh.has_connection("nope"));
        let mut f = Favorite::new("x", "SELECT 1");
        assert!(!sh.has_favorite(&f));
        f.connection_id = Some(team.id.clone());
        assert!(sh.has_favorite(&f));
        m.project.close(&m.settings).unwrap();
        assert_eq!(Shared::of(&dirs, &m.settings.get()), Shared::default());
    }
}
