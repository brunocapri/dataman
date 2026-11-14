//! Connections saved elsewhere, brought into Dataman's file: `pg_service.conf` and `~/.pgpass`,
//! and what DBeaver and TablePlus saved. Every source reads into `Importable`s; a connection
//! that cannot come in stays on the list with the reason, and what one comes in without is said
//! on its row.

mod dbeaver;
mod tableplus;

use std::path::{Path, PathBuf};

use crate::config::{ConnectionProfile, Engine, Environment, SshSettings};
use crate::engine::{Clients, SPECS};
use crate::server::{FLAVORS, Flavor};
use crate::{connstr, pgpass, pgservice};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportSource {
    Service,
    Pgpass,
    DBeaver,
    TablePlus,
}

impl ImportSource {
    pub fn label(self) -> &'static str {
        match self {
            ImportSource::Service => "service",
            ImportSource::Pgpass => "pgpass",
            ImportSource::DBeaver => "DBeaver",
            ImportSource::TablePlus => "TablePlus",
        }
    }
}

#[derive(Clone)]
pub struct Importable {
    pub source: ImportSource,
    pub profile: ConnectionProfile,
    pub password: Option<String>,
    pub ssh_password: Option<String>,
    /// Passwords the source keeps in its own Keychain items, read on import: macOS asks the user
    /// then, and only for the connections picked.
    pub keychain: Option<Keychained>,
    pub exists: bool,
    /// Why it cannot come in: its row says so and cannot be picked.
    pub refused: Option<String>,
    /// What it comes in without, or what changed on the way (verify-ca read as verify-full).
    pub notes: Vec<String>,
}

/// Another app's Keychain items: its service, and the accounts of the database's and the SSH
/// tunnel's passwords.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keychained {
    pub service: &'static str,
    pub password: Option<String>,
    pub ssh_password: Option<String>,
}

impl std::fmt::Debug for Importable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Importable")
            .field("source", &self.source)
            .field("profile", &self.profile)
            .field("password", &crate::secret_shown(&self.password))
            .field("ssh_password", &crate::secret_shown(&self.ssh_password))
            .field("keychain", &self.keychain)
            .field("exists", &self.exists)
            .field("refused", &self.refused)
            .field("notes", &self.notes)
            .finish()
    }
}

impl Importable {
    pub fn new(source: ImportSource, profile: ConnectionProfile, password: Option<String>) -> Importable {
        Importable { source, profile, password, ssh_password: None, keychain: None, exists: false, refused: None, notes: Vec::new() }
    }

    fn refused(source: ImportSource, name: impl Into<String>, why: impl Into<String>) -> Importable {
        Importable { refused: Some(why.into()), ..Importable::new(source, ConnectionProfile::new(name), None) }
    }

    /// Can be picked: it reads, and Dataman has none like it.
    pub fn pickable(&self) -> bool {
        !self.exists && self.refused.is_none()
    }
}

/// Where each source is looked for: the defaults, or a test's files.
pub struct Places {
    pub pgpass: Option<PathBuf>,
    pub services: bool,
    pub dbeaver: Vec<PathBuf>,
    pub tableplus: Vec<PathBuf>,
}

impl Places {
    pub fn here(pgpass: Option<PathBuf>) -> Places {
        Places { pgpass, services: true, dbeaver: dbeaver::workspaces(), tableplus: tableplus::files() }
    }
}

/// The same server, as the same login, to the same database: one of them is enough.
fn same_target(a: &ConnectionProfile, b: &ConnectionProfile) -> bool {
    let tunnel = |s: &Option<SshSettings>| s.as_ref().map(|s| (s.host.clone(), s.port, s.user.clone()));
    a.engine == b.engine && (&a.host, a.port, &a.user, &a.database, &a.path) == (&b.host, b.port, &b.user, &b.database, &b.path) && tunnel(&a.ssh) == tunnel(&b.ssh)
}

pub fn importable(saved: &[ConnectionProfile], places: &Places) -> Vec<Importable> {
    let mut out: Vec<Importable> = Vec::new();
    if places.services {
        for s in pgservice::all() {
            let password = s.spec.password.clone();
            let (mut profile, _) = connstr::ConnSpec { service: Some(s.name.clone()), ..s.spec }.into_profile();
            profile.name = s.name;
            out.push(Importable::new(ImportSource::Service, profile, password));
        }
    }
    if let Some(f) = &places.pgpass
        && let Ok(entries) = pgpass::read(f)
    {
        for e in entries {
            if e.host == "*" {
                continue;
            }
            let mut p = ConnectionProfile::new("");
            p.host = e.host.clone();
            p.port = e.port.parse().unwrap_or(Engine::Postgres.default_port());
            p.user = if e.user == "*" { connstr::local_user() } else { e.user.clone() };
            p.database = if e.database == "*" { "postgres".into() } else { e.database.clone() };
            p.name = if e.database == "*" { format!("{}@{}", p.user, p.host) } else { format!("{}@{}", p.database, p.host) };
            if out.iter().any(|o| same_target(&o.profile, &p)) {
                continue;
            }
            out.push(Importable::new(ImportSource::Pgpass, p, None));
        }
    }
    for found in dbeaver::read(&places.dbeaver).into_iter().chain(tableplus::read(&places.tableplus)) {
        if found.refused.is_none() && out.iter().any(|o| o.refused.is_none() && same_target(&o.profile, &found.profile)) {
            continue;
        }
        out.push(found);
    }
    for it in &mut out {
        it.exists = it.refused.is_none() && saved.iter().any(|s| same_target(s, &it.profile));
    }
    out
}

/// The engine (and flavor) another client's name for a driver stands for: a flavor's name first,
/// as MariaDB's is its own while its engine is MySQL.
fn engine_named(names: impl Fn(&Clients) -> &'static [&'static str], name: &str) -> Option<(Engine, Flavor)> {
    let has = |c: &Clients| names(c).iter().any(|n| n.eq_ignore_ascii_case(name));
    FLAVORS.iter().find(|f| has(&f.clients)).map(|f| (f.engine, f.flavor)).or_else(|| SPECS.iter().find(|s| has(&s.clients)).map(|s| (s.engine, Flavor::Standard)))
}

/// A port as another client wrote it: none is the engine's default, and one that is not a port
/// refuses the connection rather than reach some other server.
fn port(text: &str) -> Result<Option<u16>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse().map(Some).map_err(|_| format!("its port {t} is not a port"))
}

/// The profile of a server connection read from another client, with Dataman's defaults for
/// what it left unsaid (as a connection string's).
fn server_profile(spec: connstr::ConnSpec, name: &str, env: Environment) -> (ConnectionProfile, Option<String>) {
    let (mut p, password) = spec.into_profile();
    p.name = name.to_string();
    p.env = env;
    (p, password)
}

fn file_profile(engine: Engine, path: &str, name: &str, env: Environment) -> ConnectionProfile {
    let mut p = ConnectionProfile::new_file(engine, path);
    p.name = name.to_string();
    p.env = env;
    p
}

/// The files directly in `dir`, sorted (the same list on every look).
fn entries(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut v = std::fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<Vec<_>>>()?;
    v.sort();
    Ok(v)
}

/// Reads the Keychain items an import needs, for the connections picked, one by one: once the
/// user denies one, the others are not asked for and say so too.
pub(crate) fn read_keychain(items: &mut [Importable], read: impl Fn(&str, &str) -> anyhow::Result<Option<String>>) -> Vec<(String, String)> {
    let mut unread = Vec::new();
    let mut denied: Option<String> = None;
    for it in items {
        let Some(k) = it.keychain.take() else { continue };
        for (account, slot) in [(k.password, &mut it.password), (k.ssh_password, &mut it.ssh_password)] {
            let Some(account) = account else { continue };
            if let Some(why) = &denied {
                unread.push((it.profile.name.clone(), why.clone()));
                continue;
            }
            match read(k.service, &account) {
                Ok(found) => *slot = found,
                Err(e) => {
                    let why = format!("the Keychain did not give its password: {e:#}");
                    unread.push((it.profile.name.clone(), why.clone()));
                    denied = Some(why);
                }
            }
        }
    }
    unread
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engines_by_their_names_in_other_clients() {
        let db = |n: &str| engine_named(|c| c.dbeaver, n);
        let tp = |n: &str| engine_named(|c| c.tableplus, n);
        assert_eq!(db("postgresql"), Some((Engine::Postgres, Flavor::Standard)));
        assert_eq!(db("mariaDB"), Some((Engine::MySql, Flavor::MariaDb)));
        assert_eq!(db("sqlite_jdbc"), Some((Engine::Sqlite, Flavor::Standard)));
        assert_eq!(db("oracle"), None);
        assert_eq!(tp("MariaDB"), Some((Engine::MySql, Flavor::MariaDb)));
        assert_eq!(tp("SQLServer"), Some((Engine::SqlServer, Flavor::Standard)));
        assert_eq!(tp("Mongo"), Some((Engine::MongoDb, Flavor::Standard)));
        assert_eq!(tp("Cassandra"), None);
    }

    #[test]
    fn a_connection_dataman_has_is_not_offered_again() {
        let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/conn_import/testdata");
        let places = Places { pgpass: None, services: false, dbeaver: vec![testdata.join("dbeaver/workspace6")], tableplus: vec![testdata.join("tableplus/Connections.plist")] };
        let first = importable(&[], &places);
        assert!(first.iter().all(|i| !i.exists), "{first:?}");
        let orders = first.iter().find(|i| i.profile.name == "Orders (prod)").unwrap();
        let mut saved = orders.profile.clone();
        saved.name = "orders".into();
        let again = importable(&[saved.clone()], &places);
        let exists: Vec<&str> = again.iter().filter(|i| i.exists).map(|i| i.profile.name.as_str()).collect();
        assert_eq!(exists, ["Orders (prod)"]);
        // Through another bastion it is another connection.
        saved.ssh.as_mut().unwrap().host = "other-bastion".into();
        assert!(importable(&[saved], &places).iter().all(|i| !i.exists));
        // Two clients' copies of one connection are offered once.
        let both = Places { dbeaver: vec![testdata.join("dbeaver/workspace6"), testdata.join("dbeaver/workspace6")], ..places };
        assert_eq!(importable(&[], &both).iter().filter(|i| i.profile.name == "Orders (prod)").count(), 1);
    }

    #[test]
    fn a_denied_keychain_is_asked_once_and_said_for_each() {
        let mk = |name: &str| {
            let mut it = Importable::new(ImportSource::TablePlus, ConnectionProfile::new(name), None);
            it.keychain = Some(Keychained { service: "svc", password: Some(format!("{name}_database")), ssh_password: None });
            it
        };
        let asked = std::cell::RefCell::new(Vec::new());
        let mut items = vec![mk("a"), mk("b"), mk("c")];
        let unread = read_keychain(&mut items, |_, account| {
            asked.borrow_mut().push(account.to_string());
            match account {
                "a_database" => Ok(Some("pw-a".into())),
                _ => Err(anyhow::anyhow!("User canceled the operation.")),
            }
        });
        assert_eq!(*asked.borrow(), ["a_database", "b_database"]);
        assert_eq!(items[0].password.as_deref(), Some("pw-a"));
        assert_eq!((items[1].password.as_deref(), items[2].password.as_deref()), (None, None));
        let names: Vec<&str> = unread.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["b", "c"]);
        assert!(unread[1].1.contains("User canceled"), "{unread:?}");
    }
}
