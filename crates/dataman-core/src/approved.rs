//! The rules for agents and the server as this person last set them in Dataman, kept where only
//! Dataman reads them: the Keychain (the platform's keyring on Linux). connections.toml and
//! settings.toml are files any program of this user may write, an agent with a shell among
//! them, so what an edit made there loosens or moves (a higher MCP tier, YOLO, an allowed
//! statement, less privacy, a sensitive data type off, production or read-only off, weaker TLS,
//! another host, port, user, SSH tunnel, option or file, agent access on…) is not applied until
//! this person accepts it in Dataman: until then the connection keeps what was approved, and one
//! never approved is left out. What only tightens is taken as it is.
//!
//! A write Dataman makes itself (the app, an agent through MCP within its own rules, a project
//! or an import once reviewed) is approved with it: what that write changed, and nothing it left
//! as it was, so an edit of the file it did not make stays held.
//!
//! Nothing approved yet (the first run of this version, or a keyring item gone) is as if every
//! connection were new: those that would loosen something wait for review. Without a lasting
//! keyring (none on this Linux session, a test's memory store) nothing can be kept, and the files
//! are taken as they are. On Linux any program of this user can read the keyring: this stops a
//! hand edit there, not a determined one.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agents_config::{self, Change};
use crate::config::ConnectionProfile;
use crate::privacy::TypeSettings;
use crate::project::{self, ConnReview};
use crate::secret::SecretStore;
use crate::settings::Settings;

const CONNECTIONS: &str = "approved-connections";
const SETTINGS: &str = "approved-settings";
const VERSION: u32 = 1;

/// Where the approved rules are kept; none: nowhere lasting, so the files are taken as they are.
pub struct Approved {
    store: Option<Arc<dyn SecretStore>>,
}

impl Approved {
    pub fn kept_in(secrets: Arc<dyn SecretStore>) -> Arc<Approved> {
        let store = secrets.durable().then_some(secrets);
        if store.is_none() {
            log::info!("no lasting keyring: edits of connections.toml and settings.toml are taken as they are");
        }
        Arc::new(Approved { store })
    }

    /// Kept in `secrets` even when it does not last (the tests').
    pub fn always_in(secrets: Arc<dyn SecretStore>) -> Arc<Approved> {
        Arc::new(Approved { store: Some(secrets) })
    }

    pub fn off() -> Arc<Approved> {
        Arc::new(Approved { store: None })
    }

    pub fn is_on(&self) -> bool {
        self.store.is_some()
    }

    pub(crate) fn connections(&self) -> Kept<Vec<ConnectionProfile>> {
        self.load(CONNECTIONS)
    }

    pub(crate) fn set_connections(&self, list: &[ConnectionProfile]) -> Result<()> {
        self.save(CONNECTIONS, &list.iter().map(protections_of).collect::<Vec<_>>())
    }

    pub(crate) fn settings(&self) -> Kept<SettingsRules> {
        self.load(SETTINGS)
    }

    pub(crate) fn set_settings(&self, rules: &SettingsRules) -> Result<()> {
        self.save(SETTINGS, rules)
    }

    fn load<T: for<'de> Deserialize<'de>>(&self, account: &str) -> Kept<T> {
        let Some(store) = &self.store else { return Kept::Off };
        #[derive(Deserialize)]
        struct Wrapped<T> {
            version: u32,
            value: T,
        }
        match store.get(account) {
            Ok(None) => Kept::Missing,
            Ok(Some(text)) => match serde_json::from_str::<Wrapped<T>>(&text) {
                Ok(w) if w.version == VERSION => Kept::Found(w.value),
                Ok(w) => {
                    log::warn!("the approved rules ({account}) are of another version ({}): nothing is approved over them", w.version);
                    Kept::Unreadable
                }
                Err(e) => {
                    log::warn!("the approved rules ({account}) do not read: {e}");
                    Kept::Unreadable
                }
            },
            Err(e) => {
                log::warn!("the approved rules ({account}) could not be read: {e:#}");
                Kept::Unreadable
            }
        }
    }

    fn save<T: Serialize>(&self, account: &str, value: &T) -> Result<()> {
        let Some(store) = &self.store else { return Ok(()) };
        #[derive(Serialize)]
        struct Wrapped<'a, T> {
            version: u32,
            value: &'a T,
        }
        store.set(account, &serde_json::to_string(&Wrapped { version: VERSION, value })?)
    }
}

/// What the keyring holds. Unreadable (refused, of another version): read as nothing approved,
/// and never written over.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kept<T> {
    Off,
    Missing,
    Unreadable,
    Found(T),
}

impl<T> Kept<T> {
    pub(crate) fn found(&self) -> Option<&T> {
        match self {
            Kept::Found(v) => Some(v),
            _ => None,
        }
    }

    pub(crate) fn on(&self) -> bool {
        !matches!(self, Kept::Off)
    }

    pub(crate) fn writable(&self) -> bool {
        matches!(self, Kept::Missing | Kept::Found(_))
    }
}

/// A connection's protections: all of it but its name, its color and whether its password is
/// saved, which are only this person's to see.
pub fn protections_of(p: &ConnectionProfile) -> ConnectionProfile {
    ConnectionProfile { name: String::new(), color: None, save_password: false, ..p.clone() }
}

/// `p` with the protections approved for it.
pub(crate) fn with_approved(p: &ConnectionProfile, approved: &ConnectionProfile) -> ConnectionProfile {
    ConnectionProfile { id: p.id.clone(), name: p.name.clone(), color: p.color.clone(), save_password: p.save_password, ..approved.clone() }
}

/// What `next` has over what was approved for it (none approved: a connection this person did
/// not have), when it loosens or moves anything; none when it only stays or tightens.
pub(crate) fn review(approved: Option<&ConnectionProfile>, next: &ConnectionProfile, others: &[ConnectionProfile]) -> Option<ConnReview> {
    let review = |new, moved, changes: Vec<Change>| ConnReview { id: next.id.clone(), name: next.name.clone(), new, moved, changes };
    let Some(old) = approved else {
        let changes = project::arrival(next, others, None);
        return changes.iter().any(|c| c.loosens).then(|| review(true, false, changes));
    };
    let (old, now) = (protections_of(old), protections_of(next));
    if old == now {
        return None;
    }
    let moved = project::target(&old) != project::target(&now) || old.path != now.path;
    let mut changes = if moved { project::address_changes(&old, &now) } else { Vec::new() };
    changes.extend(project::protections(&old, &now));
    let mut push = |what: &str, from: String, to: String, loosens: bool| {
        if from != to {
            changes.push(Change { what: what.into(), from, to, loosens });
        }
    };
    let yes_no = |b: bool| String::from(if b { "yes" } else { "no" });
    // Either one keeps a `~/.pgpass` line that matches any host from going there.
    push("Made by an agent", yes_no(old.agent_made), yes_no(now.agent_made), old.agent_made && !now.agent_made);
    push("From a project", yes_no(old.from_project), yes_no(now.from_project), old.from_project && !now.from_project);
    push("Database", old.database.clone(), now.database.clone(), false);
    // Who agents connect as: another login (or none, so the user's own) is this person's call.
    let login = |p: &ConnectionProfile| p.agent_login.as_ref().map(|l| (l.user.clone(), l.server.clone(), l.views.clone(), l.database.clone()));
    let shown = |p: &ConnectionProfile| match &p.agent_login {
        Some(l) => l.user.clone(),
        None if p.agents_refused => "none (agents refused)".into(),
        None => "your login".into(),
    };
    if login(&old) != login(&now) {
        let tighter = now.agent_login.is_none() && now.agents_refused;
        push("Agent login", shown(&old), shown(&now), !tighter);
    }
    push("Agents refused", yes_no(old.agents_refused), yes_no(now.agents_refused), old.agents_refused && !now.agents_refused && now.agent_login.is_none());
    // Whatever none of the lines above names (a secret option, a field of a newer version) is
    // not taken unseen.
    let mut named = ConnectionProfile {
        env: old.env,
        sslmode: old.sslmode,
        read_only: old.read_only,
        read_only_databases: old.read_only_databases.clone(),
        mcp: old.mcp,
        mcp_row_limit: old.mcp_row_limit,
        mcp_allowed: old.mcp_allowed.clone(),
        mcp_yolo: old.mcp_yolo,
        mcp_rules: old.mcp_rules.clone(),
        agent_made: old.agent_made,
        from_project: old.from_project,
        database: old.database.clone(),
        agents_refused: old.agents_refused,
        // Its own line above; its bookkeeping (when it was synced, verified, rotated) is not a
        // protection.
        agent_login: old.agent_login.clone(),
        ..now.clone()
    };
    if moved {
        (named.engine, named.host, named.port, named.user, named.ssh, named.path) = (old.engine, old.host.clone(), old.port, old.user.clone(), old.ssh.clone(), old.path.clone());
        named.options.retain(|k, _| project::secret_option(k));
        named.options.extend(old.options.iter().filter(|(k, _)| !project::secret_option(k)).map(|(k, v)| (k.clone(), v.clone())));
    }
    if named != old {
        push("Other settings", "as approved".into(), "changed outside Dataman".into(), true);
    }
    (moved || changes.iter().any(|c| c.loosens)).then(|| review(false, moved, changes))
}

/// The connections as they apply, and those held for review.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Resolved {
    pub connections: Vec<ConnectionProfile>,
    pub held: Vec<ConnReview>,
}

/// The file's connections over what was approved (none: nothing approved yet). A held one keeps
/// its approved protections, or is left out when none were.
pub(crate) fn resolve(file: &[ConnectionProfile], approved: Option<&[ConnectionProfile]>) -> Resolved {
    let approved_of = |id: &str| approved.and_then(|a| a.iter().find(|a| a.id == id));
    // What each other connection reaches as approved: what a new one is held against.
    let base: Vec<ConnectionProfile> = file.iter().map(|p| approved_of(&p.id).map_or_else(|| p.clone(), |a| with_approved(p, a))).collect();
    let mut out = Resolved::default();
    for (i, p) in file.iter().enumerate() {
        let others: Vec<ConnectionProfile> = base.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, q)| q.clone()).collect();
        let a = approved_of(&p.id);
        match review(a, p, &others) {
            None => out.connections.push(p.clone()),
            Some(r) => {
                if let Some(a) = a {
                    out.connections.push(with_approved(p, a));
                }
                out.held.push(r);
            }
        }
    }
    out
}

/// The settings' part of the rules for agents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsRules {
    pub agent_access: bool,
    pub sensitive_data: TypeSettings,
}

impl SettingsRules {
    pub fn of(s: &Settings) -> SettingsRules {
        SettingsRules { agent_access: s.mcp_enabled, sensitive_data: s.agent_types.clone() }
    }

    pub(crate) fn set(&self, s: &mut Settings) {
        s.mcp_enabled = self.agent_access;
        s.agent_types = self.sensitive_data.clone();
    }
}

/// What `next` changes over the approved rules (none: the defaults); empty when nothing loosens.
pub(crate) fn review_settings(approved: Option<&SettingsRules>, next: &SettingsRules) -> Vec<Change> {
    let defaults = SettingsRules::of(&Settings::default());
    let old = approved.unwrap_or(&defaults);
    let mut changes = Vec::new();
    if old.agent_access != next.agent_access {
        let on = |b: bool| String::from(if b { "on" } else { "off" });
        changes.push(Change { what: "Agent access".into(), from: on(old.agent_access), to: on(next.agent_access), loosens: next.agent_access });
    }
    changes.extend(agents_config::type_changes(&old.sensitive_data, &next.sensitive_data));
    if changes.iter().any(|c| c.loosens) { changes } else { Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigStore, Dirs, Environment, McpAccess};
    use crate::secret::MemorySecrets;
    use crate::settings::SettingsStore;

    struct Mac {
        _dir: tempfile::TempDir,
        dirs: Dirs,
        keychain: Arc<MemorySecrets>,
    }

    impl Mac {
        fn new() -> Mac {
            let dir = tempfile::tempdir().unwrap();
            let dirs = Dirs::at(dir.path().join("dataman"));
            Mac { _dir: dir, dirs, keychain: Arc::new(MemorySecrets::default()) }
        }

        fn approved(&self) -> Arc<Approved> {
            Approved::always_in(self.keychain.clone())
        }

        /// Dataman, as the app or the MCP server opens it.
        fn dataman(&self) -> ConfigStore {
            ConfigStore::open_with(&self.dirs, self.approved()).unwrap()
        }

        /// Any other program of this user, writing the file.
        fn outside(&self) -> ConfigStore {
            ConfigStore::open(&self.dirs).unwrap()
        }

        fn settings(&self) -> SettingsStore {
            SettingsStore::open_with(&self.dirs, self.approved())
        }
    }

    fn conn(id: &str) -> ConnectionProfile {
        ConnectionProfile { id: id.into(), host: "db.example.com".into(), port: 5432, user: "app".into(), database: "shop".into(), ..ConnectionProfile::new(id) }
    }

    fn held_ids(s: &ConfigStore) -> Vec<String> {
        s.held().into_iter().map(|h| h.id).collect()
    }

    #[test]
    fn an_edit_outside_dataman_that_loosens_waits() {
        let m = Mac::new();
        m.dataman().upsert(ConnectionProfile { mcp: Some(McpAccess::Read), ..conn("a") }).unwrap();
        m.outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Full), mcp_yolo: true, ..conn("a") }).unwrap();
        let d = m.dataman();
        assert_eq!(held_ids(&d), ["a"]);
        let a = d.get("a").unwrap();
        assert_eq!((a.mcp_access(), a.mcp_yolo), (McpAccess::Read, false), "the approved rules apply meanwhile");
        let h = &d.held()[0];
        assert!(!h.new && !h.moved && h.changes.iter().any(|c| c.what == "MCP tier" && c.loosens) && h.changes.iter().any(|c| c.what == "YOLO" && c.loosens), "{h:?}");

        d.accept(&d.held()).unwrap();
        assert!(d.held().is_empty());
        assert_eq!(m.dataman().get("a").unwrap().mcp_access(), McpAccess::Full, "accepted, for every process");
    }

    #[test]
    fn what_only_tightens_is_taken() {
        let m = Mac::new();
        m.dataman().upsert(ConnectionProfile { mcp: Some(McpAccess::Write), ..conn("a") }).unwrap();
        m.outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Read), env: Environment::Production, read_only: true, ..conn("a") }).unwrap();
        let d = m.dataman();
        assert!(d.held().is_empty());
        assert_eq!(d.get("a").unwrap().mcp_access(), McpAccess::Read);
        // Approved as it is now: going back up is a loosening again.
        d.edit_all(|_| {}).unwrap();
        m.outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Write), ..conn("a") }).unwrap();
        assert_eq!(held_ids(&m.dataman()), ["a"]);
        assert_eq!(m.dataman().get("a").unwrap().mcp_access(), McpAccess::Read);
    }

    #[test]
    fn a_connection_moved_outside_dataman_keeps_its_address() {
        let m = Mac::new();
        m.dataman().upsert(conn("a")).unwrap();
        for moved in [
            ConnectionProfile { host: "db.attacker.example".into(), ..conn("a") },
            ConnectionProfile { port: 6543, ..conn("a") },
            ConnectionProfile { options: [("sslrootcert".to_string(), "/tmp/x".to_string())].into(), ..conn("a") },
        ] {
            m.outside().upsert(moved).unwrap();
            let d = m.dataman();
            let h = d.held();
            assert!(h.len() == 1 && h[0].moved, "{h:?}");
            let a = d.get("a").unwrap();
            assert_eq!((a.host.as_str(), a.port, a.options.len()), ("db.example.com", 5432, 0), "the password only goes where it was approved");
        }
        // Nor what no line names (a secret option): taken unseen, it could reach anything.
        m.outside().upsert(ConnectionProfile { options: [("password".to_string(), "x".to_string())].into(), ..conn("a") }).unwrap();
        let h = m.dataman().held();
        assert!(h.len() == 1 && h[0].changes.iter().any(|c| c.what == "Other settings"), "{h:?}");
        // The name and the color are this person's only.
        m.outside().upsert(ConnectionProfile { name: "Renamed".into(), color: Some("red".into()), ..conn("a") }).unwrap();
        let d = m.dataman();
        assert!(d.held().is_empty());
        assert_eq!(d.get("a").unwrap().name, "Renamed");
    }

    #[test]
    fn a_connection_added_outside_dataman() {
        let m = Mac::new();
        m.dataman().upsert(ConnectionProfile { env: Environment::Production, ..conn("prod") }).unwrap();
        m.outside().upsert(ConnectionProfile { host: "dev.example.com".into(), ..conn("plain") }).unwrap();
        m.outside().upsert(ConnectionProfile { host: "dev.example.com".into(), mcp: Some(McpAccess::Write), mcp_allowed: vec!["DELETE FROM t".into()], ..conn("loose") }).unwrap();
        // The same server as the production one, without its protection.
        m.outside().upsert(ConnectionProfile { id: "twin".into(), name: "twin".into(), ..conn("prod") }).unwrap();
        let d = m.dataman();
        assert_eq!(held_ids(&d), ["loose", "twin"]);
        assert!(d.held().iter().all(|h| h.new));
        let ids: Vec<String> = d.connections().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["prod", "plain"], "never approved: left out until accepted");

        // Dataman's own writes keep them in the file, still held.
        d.reorder(&["plain".into(), "prod".into()]).unwrap();
        let file: Vec<String> = m.outside().connections().into_iter().map(|p| p.id).collect();
        assert_eq!(file, ["plain", "prod", "loose", "twin"]);
        let d = m.dataman();
        assert_eq!(held_ids(&d), ["loose", "twin"]);
        d.accept(&d.held()).unwrap();
        assert_eq!(m.dataman().get("loose").unwrap().mcp_allowed, ["DELETE FROM t"]);
    }

    #[test]
    fn dataman_s_own_writes_are_approved_and_leave_the_rest_held() {
        let m = Mac::new();
        m.dataman().edit_all(|l| l.extend([conn("a"), conn("b")])).unwrap();
        m.outside().upsert(ConnectionProfile { mcp_yolo: true, ..conn("a") }).unwrap();
        let d = m.dataman();
        // An agent through MCP, or the app: a change of b, and a rename of a.
        d.edit_all(|l| {
            l[1].mcp = Some(McpAccess::Write);
            l[0].name = "A".into();
        })
        .unwrap();
        let d = m.dataman();
        assert_eq!(held_ids(&d), ["a"], "a's YOLO, which Dataman did not make, still waits");
        assert!(m.outside().get("a").unwrap().mcp_yolo, "and stays in the file");
        assert_eq!(d.get("a").unwrap().name, "A");
        assert_eq!(d.get("b").unwrap().mcp_access(), McpAccess::Write);

        // Changed in Dataman over what is shown: that is what is approved, and the file's goes.
        let a = d.get("a").unwrap();
        d.upsert(ConnectionProfile { mcp: Some(McpAccess::Off), ..a }).unwrap();
        let d = m.dataman();
        assert!(d.held().is_empty());
        let a = d.get("a").unwrap();
        assert_eq!((a.mcp_access(), a.mcp_yolo), (McpAccess::Off, false));
    }

    #[test]
    fn nothing_approved_is_held_as_new() {
        let m = Mac::new();
        m.outside().edit_all(|l| l.extend([conn("plain"), ConnectionProfile { mcp: Some(McpAccess::Full), ..conn("full") }])).unwrap();
        // The first run of this version: what loosens nothing is approved as it is.
        let d = m.dataman();
        assert_eq!(held_ids(&d), ["full"]);
        assert!(m.keychain.get(CONNECTIONS).unwrap().is_some());
        d.accept(&d.held()).unwrap();
        assert!(m.dataman().held().is_empty());
        // The item gone (deleted by another program): as nothing approved, never as the file says.
        m.keychain.delete(CONNECTIONS).unwrap();
        let d = m.dataman();
        assert_eq!(held_ids(&d), ["full"]);
        assert!(d.get("full").is_none());
    }

    #[test]
    fn an_approved_item_that_does_not_read_is_never_written_over() {
        let m = Mac::new();
        m.dataman().upsert(ConnectionProfile { mcp: Some(McpAccess::Write), ..conn("a") }).unwrap();
        m.keychain.set(CONNECTIONS, "{\"version\":99,\"value\":[]}").unwrap();
        let d = m.dataman();
        assert_eq!(held_ids(&d), ["a"]);
        assert!(d.edit_all(|l| l.push(ConnectionProfile { mcp: Some(McpAccess::Full), ..conn("b") })).is_err());
        assert!(d.accept(&d.held()).is_err());
        assert_eq!(m.keychain.get(CONNECTIONS).unwrap().as_deref(), Some("{\"version\":99,\"value\":[]}"));
    }

    #[test]
    fn a_review_is_of_the_file_as_shown() {
        let m = Mac::new();
        m.dataman().upsert(conn("a")).unwrap();
        m.outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Write), ..conn("a") }).unwrap();
        let d = m.dataman();
        let shown = d.held();
        m.outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Full), ..conn("a") }).unwrap();
        assert!(d.accept(&shown).is_err());
        assert_eq!(m.dataman().get("a").unwrap().mcp_access(), McpAccess::Read);
    }

    #[test]
    fn without_a_lasting_keyring_the_file_is_taken() {
        let m = Mac::new();
        m.outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Full), ..conn("a") }).unwrap();
        let d = ConfigStore::open_with(&m.dirs, Approved::kept_in(m.keychain.clone())).unwrap();
        assert!(d.held().is_empty());
        assert_eq!(d.get("a").unwrap().mcp_access(), McpAccess::Full);
        assert!(m.keychain.get(CONNECTIONS).unwrap().is_none());
    }

    #[test]
    fn settings_edited_outside_dataman() {
        let m = Mac::new();
        m.settings().update(|s| s.mcp_enabled = false).unwrap();
        let outside = SettingsStore::open(&m.dirs);
        outside
            .update(|s| {
                s.mcp_enabled = true;
                s.agent_types.set_builtin("email", Some(false), None);
            })
            .unwrap();
        let s = m.settings();
        let held = s.held();
        assert!(held.iter().any(|c| c.what == "Agent access" && c.loosens), "{held:?}");
        assert!(!s.get().mcp_enabled, "the approved rules apply meanwhile");
        assert_eq!(s.get().agent_types, crate::privacy::TypeSettings::default());

        // Another setting changed in Dataman: the file keeps what is held.
        s.update(|x| x.zoom = 1.25).unwrap();
        let disk = SettingsStore::open(&m.dirs).get();
        assert!(disk.mcp_enabled && disk.zoom == 1.25);
        let s = m.settings();
        assert!(!s.get().mcp_enabled && s.get().zoom == 1.25);

        s.accept(&s.held()).unwrap();
        assert!(m.settings().get().mcp_enabled && m.settings().held().is_empty());
        // Tighter is taken as it is.
        SettingsStore::open(&m.dirs).update(|x| x.mcp_enabled = false).unwrap();
        assert!(!m.settings().get().mcp_enabled && m.settings().held().is_empty());
    }

    #[test]
    fn an_agent_login_changed_outside_dataman_waits() {
        let m = Mac::new();
        let mut a = conn("a");
        a.agent_login = Some(crate::agent_login::test_login("dataman_agent_a", &a));
        m.dataman().upsert(a.clone()).unwrap();
        let mine = || m.dataman().get("a").unwrap().agent_login.map(|l| l.user);
        for (what, edit) in [
            ("another login (agents would connect as root)", ConnectionProfile { agent_login: Some(crate::agent_login::test_login("root", &a)), ..a.clone() }),
            ("no login: agents back on the user's", ConnectionProfile { agent_login: None, ..a.clone() }),
        ] {
            m.outside().upsert(edit).unwrap();
            let d = m.dataman();
            let h = d.held();
            assert!(h.len() == 1 && h[0].changes.iter().any(|c| c.what == "Agent login" && c.loosens), "{what}: {h:?}");
            assert_eq!(mine().as_deref(), Some("dataman_agent_a"), "{what}: the approved one applies meanwhile");
        }
        // Its bookkeeping (verified, synced) is not a protection.
        m.outside().upsert(a.clone()).unwrap();
        let mut b = a.clone();
        b.agent_login.as_mut().unwrap().verified = Some(chrono::Utc::now());
        m.outside().upsert(b).unwrap();
        assert!(m.dataman().held().is_empty());
        // Refused after a Remove: only Dataman lets agents use the user's login again.
        let refused = ConnectionProfile { agent_login: None, agents_refused: true, ..a.clone() };
        m.dataman().upsert(refused.clone()).unwrap();
        m.outside().upsert(ConnectionProfile { agents_refused: false, ..refused.clone() }).unwrap();
        let d = m.dataman();
        assert!(d.held().iter().any(|h| h.changes.iter().any(|c| c.what == "Agents refused" && c.loosens)), "{:?}", d.held());
        assert!(d.get("a").unwrap().agents_refused);
        // An agent login added outside Dataman (its password is not in the Keychain anyway).
        m.outside().upsert(ConnectionProfile { agent_login: Some(crate::agent_login::test_login("dataman_agent_a", &a)), ..refused }).unwrap();
        assert!(m.dataman().held().iter().any(|h| h.changes.iter().any(|c| c.what == "Agent login")));
    }
}
