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
//! and the audit, which are each person's.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::{ConfigStore, ConnectionProfile, Dirs};
use crate::favorites::{Favorite, FavoritesStore, Folder};
use crate::format::KeywordCase;
use crate::privacy::TypeSettings;
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
}

impl ProjectFile {
    pub fn to_text(&self) -> String {
        format!("{HEADER}{}", toml::to_string_pretty(self).unwrap_or_default())
    }

    pub fn parse(text: &str) -> Result<ProjectFile> {
        Ok(toml::from_str(text)?)
    }

    fn connection_ids(&self) -> BTreeSet<String> {
        self.connections.iter().map(|p| p.id.clone()).collect()
    }
}

/// Each person's own, never in the file.
const PERSONAL: [&str; 2] = ["save_password", "agent_made"];

/// A driver option that names a secret (a key's passphrase, a token…) stays local too.
pub fn secret_option(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["pass", "secret", "token", "credential", "apikey", "api_key", "private"].iter().any(|w| k.contains(w))
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
    p.options.retain(|k, _| !secret_option(k));
    p.path = p.path.as_deref().map(|path| to_project_path(path, root));
    if !agents {
        (p.mcp, p.mcp_row_limit, p.mcp_allowed, p.mcp_yolo, p.mcp_rules) = (None, None, Vec::new(), false, Default::default());
    }
    p
}

/// The project's connection over this person's copy: theirs is everything shared, ours what is
/// personal (the saved password choice, secret options, the agents' rules when not shared).
fn merged(mine: Option<&ConnectionProfile>, theirs: &ConnectionProfile, agents: bool, root: &Path) -> ConnectionProfile {
    let mut p = theirs.clone();
    p.path = theirs.path.as_deref().map(|path| from_project_path(path, root));
    p.save_password = mine.is_some_and(|m| m.save_password);
    p.agent_made = mine.is_some_and(|m| m.agent_made);
    p.options.retain(|k, _| !secret_option(k));
    if let Some(m) = mine {
        p.options.extend(m.options.iter().filter(|(k, _)| secret_option(k)).map(|(k, v)| (k.clone(), v.clone())));
        if !agents {
            (p.mcp, p.mcp_row_limit, p.mcp_allowed, p.mcp_yolo, p.mcp_rules) = (m.mcp, m.mcp_row_limit, m.mcp_allowed.clone(), m.mcp_yolo, m.mcp_rules.clone());
        }
    }
    p
}

/// The project's file as this person's side would write it: `members` are its connections.
pub fn export(local: &Local, share: Share, members: &BTreeSet<String>, root: &Path) -> ProjectFile {
    let connections: Vec<ConnectionProfile> = if share.connections {
        local.connections.iter().filter(|p| members.contains(&p.id)).map(|p| shareable(p, share.agents, root)).collect()
    } else {
        Vec::new()
    };
    let favorites: Vec<Favorite> = if share.favorites {
        local.favorites.iter().filter(|f| f.connection_id.as_ref().is_some_and(|c| connections.iter().any(|p| p.id == *c))).cloned().collect()
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
}

/// The file `theirs` over `local`. `base` is the file as it was when both sides last agreed
/// (none the first time): a favorite in it that `theirs` dropped was deleted.
pub fn plan(local: &Local, theirs: &ProjectFile, base: Option<&ProjectFile>, root: &Path) -> Changes {
    let mut c = Changes::default();
    for t in &theirs.connections {
        let mine = local.connections.iter().find(|p| p.id == t.id);
        let p = merged(mine, t, theirs.share.agents, root);
        if mine != Some(&p) {
            c.connections.push(p);
        }
    }
    for d in &theirs.folders {
        if local.folders.iter().find(|x| x.id == d.id) != Some(d) {
            c.folders.push(d.clone());
        }
    }
    for f in &theirs.favorites {
        if local.favorites.iter().find(|x| x.id == f.id) != Some(f) {
            c.favorites.push(f.clone());
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
    c.editor = theirs.editor.filter(|e| *e != EditorPrefs::of(&local.settings));
    c.sensitive_data = theirs.agents.as_ref().map(|a| a.sensitive_data.clone()).filter(|t| *t != local.settings.agent_types);
    c
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Changes::default()
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
            written: false,
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
}

#[derive(Serialize, Deserialize)]
struct Base {
    root: String,
    project: ProjectFile,
}

pub struct ProjectStore {
    base_file: PathBuf,
    state: Mutex<State>,
}

fn read(path: &Path) -> Result<Option<ProjectFile>> {
    match std::fs::read_to_string(path) {
        Ok(s) => ProjectFile::parse(&s).with_context(|| format!("reading {}", path.display())).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write(path: &Path, f: &ProjectFile) -> Result<()> {
    let dir = path.parent().context("project path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{FILE}.{}", std::process::id()));
    std::fs::write(&tmp, f.to_text())?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

impl Dirs {
    /// The open project's file as it was at the last sync (to tell deletions from additions).
    pub fn project_base_file(&self) -> PathBuf {
        self.config.join("project-base.toml")
    }
}

impl ProjectStore {
    pub fn open(dirs: &Dirs) -> ProjectStore {
        ProjectStore { base_file: dirs.project_base_file(), state: Mutex::new(State::default()) }
    }

    fn read_base(&self, root: &Path) -> Option<ProjectFile> {
        let text = std::fs::read_to_string(&self.base_file).ok()?;
        let b: Base = toml::from_str(&text).ok()?;
        (Path::new(&b.root) == root).then_some(b.project)
    }

    fn write_base(&self, root: &Path, project: &ProjectFile) {
        let b = Base { root: root.display().to_string(), project: project.clone() };
        let r = toml::to_string_pretty(&b).map_err(anyhow::Error::from).and_then(|text| Ok(std::fs::write(&self.base_file, text)?));
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
        *self.state.lock() = State::default();
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
        Some(Info {
            share: base.map_or_else(Share::default, |b| b.share),
            connections: base.map(ProjectFile::connection_ids).unwrap_or_default(),
            problem: g.problem.clone(),
            root,
        })
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
        let stamp = FileStamp::of(&path);
        let theirs = if stamp.is_some() && stamp == g.stamp {
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
        g.problem = None;
        let mut local = Local::read(st);
        let mut out = Synced::default();
        if let Some(t) = theirs.as_ref().filter(|t| g.base.as_ref() != Some(*t)) {
            let changes = plan(&local, t, g.base.as_ref(), &root);
            out = changes.write(st)?;
            changes.apply_to(&mut local);
        }
        let mut share = theirs.as_ref().map_or_else(Share::default, |t| t.share);
        let mut members = theirs.as_ref().map(ProjectFile::connection_ids).unwrap_or_default();
        let ids: BTreeSet<String> = local.connections.iter().map(|p| p.id.clone()).collect();
        if let Some(known) = &g.known {
            members.extend(local.connections.iter().filter(|p| !known.contains(&p.id) && !p.agent_made).map(|p| p.id.clone()));
        }
        g.known = Some(ids);
        edit(&mut share, &mut members);
        let mine = export(&local, share, &members, &root);
        if theirs.as_ref() != Some(&mine) {
            write(&path, &mine)?;
            out.written = true;
        }
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
                _dir: dir,
            }
        }

        fn st(&self) -> Stores<'_> {
            Stores { config: &self.config, settings: &self.settings, favorites: &self.favorites }
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
        p.agent_made = true;
        p.options.insert("authSource".into(), "admin".into());
        p.options.insert("sslPassword".into(), "hunter2".into());
        p.options.insert("api_token".into(), "tok-123".into());
        let local = Local { connections: vec![p.clone()], ..Local::default() };
        let f = export(&local, Share::default(), &BTreeSet::from([p.id.clone()]), Path::new("/nowhere"));
        let text = f.to_text();
        for secret in ["hunter2", "tok-123", "save_password", "agent_made"] {
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
        theirs_conn.host = "shop.new.internal".into();
        theirs_conn.mcp_row_limit = Some(50);
        let theirs = ProjectFile {
            version: VERSION,
            share: Share { agents: false, ..Share::default() },
            editor: Some(EditorPrefs { keyword_case: KeywordCase::Lower, indent: 2, confirm_commit: false }),
            agents: None,
            connections: vec![shareable(&theirs_conn, false, root)],
            folders: Vec::new(),
            favorites: Vec::new(),
        };
        let c = plan(&local, &theirs, None, root);
        c.apply_to(&mut local);
        let got = &local.connections[0];
        assert_eq!(got.host, "shop.new.internal", "the project's address");
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

        // Bob changes the address; Alice's next sync takes it.
        let mut moved = bob.config.get(&shop.id).unwrap();
        moved.port = 6543;
        bob.config.upsert(moved).unwrap();
        assert!(bob.sync().written);
        assert!(alice.sync().connections);
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
}
