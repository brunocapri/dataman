//! TablePlus (macOS) keeps every connection in one `Connections.plist`, an array of
//! dictionaries; the passwords it saved are in the Keychain, under its own service, one item per
//! connection's `ID` and what it is for.

use std::path::{Path, PathBuf};

use plist::{Dictionary, Value};

use super::{ImportSource, Importable, Keychained, engine_named, file_profile, port, server_profile};
use crate::config::{Engine, Environment, SshAuth, SshSettings, SslMode};
use crate::connstr::{self, ConnSpec};
use crate::engine::Clients;
use crate::server::FLAVORS;

const SOURCE: ImportSource = ImportSource::TablePlus;
const KEYCHAIN_SERVICE: &str = "com.tableplus.TablePlus";
/// `DatabasePasswordMode` and `ServerPasswordMode`: saved in the Keychain. Else asked each time
/// (1), or none (2).
const SAVED: i64 = 0;
const NO_PASSWORD: i64 = 2;

pub fn files() -> Vec<PathBuf> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let Some(home) = crate::os::home() else { return Vec::new() };
    ["com.tinyapp.TablePlus", "com.tinyapp.TablePlus-setapp"].iter().map(|app| home.join("Library/Application Support").join(app).join("Data/Connections.plist")).collect()
}

pub fn read(files: &[PathBuf]) -> Vec<Importable> {
    let mut out = Vec::new();
    for file in files.iter().filter(|f| f.is_file()) {
        read_file(file, &mut out);
    }
    out
}

fn read_file(file: &Path, out: &mut Vec<Importable>) {
    let rows = match Value::from_file(file) {
        Ok(Value::Array(rows)) => rows,
        Ok(_) => return out.push(Importable::refused(SOURCE, file.display().to_string(), "is not a list of connections")),
        Err(e) => return out.push(Importable::refused(SOURCE, file.display().to_string(), format!("could not be read: {e}"))),
    };
    for row in &rows {
        let Some(row) = row.as_dictionary() else {
            out.push(Importable::refused(SOURCE, file.display().to_string(), "holds a connection that is not a dictionary"));
            continue;
        };
        let name = text(row, "ConnectionName").or_else(|| text(row, "ID")).unwrap_or("TablePlus connection");
        out.push(connection(name, row).unwrap_or_else(|why| Importable::refused(SOURCE, name, why)));
    }
}

fn text<'a>(row: &'a Dictionary, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_string).map(str::trim).filter(|s| !s.is_empty())
}

/// A number TablePlus wrote as an integer, or as its text.
fn number(row: &Dictionary, key: &str) -> Option<i64> {
    match row.get(key)? {
        Value::Integer(i) => i.as_signed(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn flag(row: &Dictionary, key: &str) -> bool {
    match row.get(key) {
        Some(Value::Boolean(b)) => *b,
        Some(Value::Integer(i)) => i.as_signed() == Some(1),
        _ => false,
    }
}

/// A port TablePlus wrote as text or as an integer.
fn port_of(row: &Dictionary, key: &str) -> Result<Option<u16>, String> {
    match row.get(key) {
        Some(Value::Integer(i)) => i.as_unsigned().and_then(|p| u16::try_from(p).ok()).map(Some).ok_or_else(|| format!("its port {i} is not a port")),
        _ => port(text(row, key).unwrap_or("")),
    }
}

fn connection(name: &str, row: &Dictionary) -> Result<Importable, String> {
    let driver = text(row, "Driver").ok_or("it names no driver")?;
    let (engine, flavor) = engine_named(|c| c.tableplus, driver).ok_or_else(|| format!("{driver} is not an engine Dataman connects to"))?;
    let env = match text(row, "Enviroment").or_else(|| text(row, "Environment")).map(str::to_ascii_lowercase).as_deref() {
        Some("production") => Environment::Production,
        Some("staging" | "testing") => Environment::Staging,
        Some("development") => Environment::Dev,
        _ => Environment::None,
    };
    let id = text(row, "ID");
    if engine.is_file() {
        let path = text(row, "DatabasePath").or_else(|| text(row, "DatabaseHost")).ok_or("it names no database file")?;
        return Ok(Importable::new(SOURCE, file_profile(engine, path, name, env), None));
    }
    let host = text(row, "DatabaseHost").ok_or("it names no host")?;
    let mut notes = Vec::new();
    // A MongoDB connection may hold its whole URL in the host field.
    let mut spec = match connstr::parse(host).filter(|s| engine == Engine::MongoDb && s.engine == engine) {
        Some(spec) => spec,
        None => ConnSpec { engine, host: Some(host.to_string()), port: port_of(row, "DatabasePort")?, ..Default::default() },
    };
    spec.flavor = flavor;
    spec.user = spec.user.or(text(row, "DatabaseUser").map(str::to_string));
    spec.database = spec.database.or(text(row, "DatabaseName").map(str::to_string));
    let clients = FLAVORS.iter().find(|f| f.flavor == flavor && f.engine == engine).map_or(engine.spec().clients, |f| f.clients);
    spec.sslmode = spec.sslmode.or(tls(&clients, number(row, "tLSMode"), &mut notes));
    if row.get("TlsKeyPaths").and_then(Value::as_array).is_some_and(|a| a.iter().any(|p| p.as_string().is_some_and(|p| !p.trim().is_empty()))) {
        notes.push("TLS certificate files left behind".into());
    }
    let ssh = if flag(row, "isOverSSH") { Some(tunnel(row, &mut notes)?) } else { None };
    let account = |what: &str| id.map(|id| format!("{id}_{what}"));
    let keychain = Keychained {
        service: KEYCHAIN_SERVICE,
        password: account("database").filter(|_| number(row, "DatabasePasswordMode").unwrap_or(SAVED) == SAVED),
        ssh_password: account("server").filter(|_| ssh.as_ref().is_some_and(|s| s.auth == SshAuth::Password) && number(row, "ServerPasswordMode").unwrap_or(SAVED) == SAVED),
    };
    let (mut profile, password) = server_profile(spec, name, env);
    profile.ssh = ssh;
    let keychain = (keychain.password.is_some() || keychain.ssh_password.is_some()).then_some(keychain);
    Ok(Importable { keychain, notes, ..Importable::new(SOURCE, profile, password) })
}

/// `tLSMode` indexes the driver's own list of modes. None: the driver's are not known, and the
/// engine's default applies.
fn tls(clients: &Clients, mode: Option<i64>, notes: &mut Vec<String>) -> Option<SslMode> {
    let modes = clients.tableplus_tls;
    if modes.is_empty() {
        return None;
    }
    let mode = mode.unwrap_or(0);
    usize::try_from(mode).ok().and_then(|i| modes.get(i).copied()).or_else(|| {
        notes.push(format!("TLS mode {mode} read as verify-full"));
        Some(SslMode::VerifyFull)
    })
}

fn tunnel(row: &Dictionary, notes: &mut Vec<String>) -> Result<SshSettings, String> {
    let host = text(row, "ServerAddress").ok_or("its SSH tunnel names no host")?;
    let (auth, key_file) = if flag(row, "isUsePrivateKey") {
        // A path is a key on disk; a bare name is a key TablePlus keeps for itself.
        match text(row, "ServerPrivateKeyName") {
            Some(k) if k.starts_with('/') || k.starts_with('~') => (SshAuth::Key, k.to_string()),
            Some(k) => {
                notes.push(format!("SSH key {k} stays in TablePlus: ~/.ssh's keys are tried"));
                (SshAuth::Key, String::new())
            }
            None => (SshAuth::Key, String::new()),
        }
    } else if number(row, "ServerPasswordMode") == Some(NO_PASSWORD) {
        (SshAuth::Agent, String::new())
    } else {
        (SshAuth::Password, String::new())
    };
    Ok(SshSettings { host: host.to_string(), port: port_of(row, "ServerPort")?, user: text(row, "ServerUser").unwrap_or("").to_string(), auth, key_file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Flavor;

    fn found() -> Vec<Importable> {
        read(&[Path::new(env!("CARGO_MANIFEST_DIR")).join("src/conn_import/testdata/tableplus/Connections.plist")])
    }

    fn by<'a>(items: &'a [Importable], name: &str) -> &'a Importable {
        items.iter().find(|i| i.profile.name == name).unwrap_or_else(|| panic!("{name} in {items:?}"))
    }

    #[test]
    fn a_postgres_connection_over_ssh_with_its_passwords_in_the_keychain() {
        let items = found();
        let it = by(&items, "Billing prod");
        let p = &it.profile;
        assert_eq!((p.engine, p.host.as_str(), p.port, p.user.as_str(), p.database.as_str()), (Engine::Postgres, "billing.internal", 5432, "billing", "billing"));
        assert_eq!((p.sslmode, p.env), (SslMode::Require, Environment::Production));
        let ssh = p.ssh.as_ref().unwrap();
        assert_eq!((ssh.host.as_str(), ssh.port, ssh.user.as_str(), ssh.auth), ("bastion.example.com", Some(22), "ubuntu", SshAuth::Password));
        let k = it.keychain.as_ref().unwrap();
        assert_eq!((k.service, k.password.as_deref(), k.ssh_password.as_deref()), (KEYCHAIN_SERVICE, Some("6D5B8A2C-1F3E-4B7A-9C0D-2E4F6A8B0C1D_database"), Some("6D5B8A2C-1F3E-4B7A-9C0D-2E4F6A8B0C1D_server")));
        assert!(it.password.is_none() && it.notes.is_empty(), "{it:?}");
    }

    #[test]
    fn mariadb_redis_sqlite_and_a_key_on_disk() {
        let items = found();
        let m = by(&items, "Shop");
        assert_eq!((m.profile.engine, m.profile.flavor, m.profile.port, m.profile.sslmode, m.profile.env), (Engine::MySql, Flavor::MariaDb, 3307, SslMode::VerifyFull, Environment::Dev));
        assert!(m.keychain.is_none(), "asked each time: {m:?}");
        let ssh = m.profile.ssh.as_ref().unwrap();
        assert_eq!((ssh.auth, ssh.key_file.as_str()), (SshAuth::Key, "~/.ssh/id_ed25519"));
        let r = by(&items, "Cache");
        assert_eq!((r.profile.engine, r.profile.host.as_str(), r.profile.port, r.profile.database.as_str()), (Engine::Redis, "127.0.0.1", 6379, "2"));
        let lite = by(&items, "Notes");
        assert_eq!((lite.profile.engine, lite.profile.path.as_deref()), (Engine::Sqlite, Some("/Users/me/notes.db")));
    }

    #[test]
    fn what_cannot_come_in_says_why() {
        let items = found();
        assert!(by(&items, "Events").refused.as_deref().unwrap().contains("Cassandra"));
        let odd = by(&items, "Odd TLS");
        assert_eq!(odd.profile.sslmode, SslMode::VerifyFull);
        assert!(odd.notes.iter().any(|n| n.contains("TLS mode 9")), "{:?}", odd.notes);
        let kept = by(&items, "Key in TablePlus");
        assert!(kept.notes.iter().any(|n| n.contains("stays in TablePlus")), "{:?}", kept.notes);
    }

    #[test]
    fn a_file_that_is_not_tableplus_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("Connections.plist");
        std::fs::write(&f, "not a plist").unwrap();
        let items = read(&[f]);
        assert_eq!(items.len(), 1);
        assert!(items[0].refused.as_deref().unwrap().starts_with("could not be read"), "{items:?}");
    }
}
