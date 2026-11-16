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
    spec.ca_file = spec.ca_file.or(ca_file(row, &mut notes));
    let (ssh, kept_key) = match flag(row, "isOverSSH") {
        true => tunnel(row).map(|(s, kept)| (Some(s), kept))?,
        false => (None, None),
    };
    let account = |what: &str| id.map(|id| format!("{id}_{what}"));
    let keychain = Keychained {
        service: KEYCHAIN_SERVICE,
        password: account("database").filter(|_| number(row, "DatabasePasswordMode").unwrap_or(SAVED) == SAVED),
        ssh_password: account("server").filter(|_| ssh.as_ref().is_some_and(|s| s.auth == SshAuth::Password) && number(row, "ServerPasswordMode").unwrap_or(SAVED) == SAVED),
        private_key: account("private_key_data").filter(|_| kept_key.is_some()),
    };
    if let Some(k) = kept_key.filter(|_| keychain.private_key.is_none()) {
        let k = if k.is_empty() { k } else { format!(" {k}") };
        notes.push(format!("SSH key{k} stays in TablePlus: ~/.ssh's keys are tried"));
    }
    let (mut profile, password) = server_profile(spec, name, env, &mut notes);
    profile.ssh = ssh;
    let keychain = (keychain.password.is_some() || keychain.ssh_password.is_some() || keychain.private_key.is_some()).then_some(keychain);
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

/// `TlsKeyPaths`: the files `TlsKeyName` names, in its order (client key, client certificate,
/// CA by default). The CA's is kept; a client certificate is not sent, and said.
fn ca_file(row: &Dictionary, notes: &mut Vec<String>) -> Option<String> {
    let paths: Vec<&str> = row.get("TlsKeyPaths").and_then(Value::as_array).map(|a| a.iter().map(|p| p.as_string().map_or("", str::trim)).collect()).unwrap_or_default();
    let ca = text(row, "TlsKeyName").and_then(|n| n.split(',').position(|n| n.trim() == "Certificate Authority")).unwrap_or(2);
    if paths.iter().enumerate().any(|(i, p)| i != ca && !p.is_empty()) {
        notes.push("client certificate left behind".into());
    }
    paths.get(ca).filter(|p| !p.is_empty()).map(|p| p.to_string())
}

/// The tunnel, and the name of a key TablePlus keeps for itself (a bare name, not a path).
fn tunnel(row: &Dictionary) -> Result<(SshSettings, Option<String>), String> {
    let host = text(row, "ServerAddress").ok_or("its SSH tunnel names no host")?;
    let (auth, key_file, kept) = if flag(row, "isUsePrivateKey") {
        match text(row, "ServerPrivateKeyName") {
            Some(k) if k.starts_with('/') || k.starts_with('~') => (SshAuth::Key, k.to_string(), None),
            k => (SshAuth::Key, String::new(), Some(k.unwrap_or("").to_string())),
        }
    } else if number(row, "ServerPasswordMode") == Some(NO_PASSWORD) {
        (SshAuth::Agent, String::new(), None)
    } else {
        (SshAuth::Password, String::new(), None)
    };
    let ssh = SshSettings { host: host.to_string(), port: port_of(row, "ServerPort")?, user: text(row, "ServerUser").unwrap_or("").to_string(), auth, key_file };
    Ok((ssh, kept))
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
        let loose = by(&items, "CA under require");
        assert_eq!((loose.profile.sslmode, loose.profile.ca_file.as_str()), (SslMode::Require, ""));
        assert_eq!(loose.notes, ["CA certificate left behind: require checks none"]);
    }

    #[test]
    fn a_ca_certificate_and_servers_that_speak_postgres() {
        let items = found();
        let v = by(&items, "Verified");
        assert_eq!((v.profile.sslmode, v.profile.ca_file.as_str()), (SslMode::VerifyFull, "/certs/pay-ca.pem"));
        assert!(v.notes.is_empty(), "{:?}", v.notes);
        // MongoDB names its files in another order.
        let m = by(&items, "Mongo string");
        assert_eq!((m.profile.sslmode, m.profile.ca_file.as_str()), (SslMode::VerifyFull, "/c/mongo-ca.pem"));
        assert_eq!(m.notes, ["client certificate left behind"]);
        let w = by(&items, "Warehouse");
        assert_eq!((w.profile.engine, w.profile.port, w.profile.sslmode, w.profile.env), (Engine::Postgres, 5439, SslMode::Require, Environment::Production));
    }

    #[test]
    fn what_the_keychain_keeps_comes_in_on_import() {
        let dir = tempfile::tempdir().unwrap();
        let mut items: Vec<Importable> = found().into_iter().filter(|i| ["Mongo string", "Key in TablePlus"].contains(&i.profile.name.as_str())).collect();
        let kept = by(&items, "Key in TablePlus");
        assert!(kept.notes.is_empty(), "read on import, not left behind: {:?}", kept.notes);
        assert_eq!(kept.keychain.as_ref().unwrap().private_key.as_deref(), Some("FEDCBA98-7654-3210-FEDC-BA9876543210_private_key_data"));
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let unread = super::super::read_keychain(
            &mut items,
            |_, account| {
                Ok(Some(match account {
                    "D0D0D0D0-1234-5678-9ABC-DEF012345678_database" => "mongodb://app:s3cret@m1.internal:27018,m2.internal:27018/orders?authSource=admin&replicaSet=rs0".into(),
                    "FEDCBA98-7654-3210-FEDC-BA9876543210_private_key_data" => data_encoding::BASE64.encode(pem.as_bytes()),
                    other => format!("pw-{other}"),
                }))
            },
            dir.path(),
        );
        assert!(unread.is_empty(), "{unread:?}");
        // The string saved as the password says where, and with what login.
        let m = by(&items, "Mongo string");
        assert_eq!((m.profile.host.as_str(), m.profile.port, m.profile.user.as_str(), m.profile.database.as_str()), ("m1.internal:27018,m2.internal:27018", 27017, "app", "orders"));
        assert_eq!((m.password.as_deref(), m.profile.options.get("replicaSet").map(String::as_str)), (Some("s3cret"), Some("rs0")));
        assert_eq!((m.profile.sslmode, m.profile.env), (SslMode::VerifyFull, Environment::Staging), "TLS the string does not name stays the row's");
        assert_eq!(m.profile.ssh.as_ref().map(|s| s.host.as_str()), Some("jump.example.com"));
        let k = by(&items, "Key in TablePlus");
        let file = &k.profile.ssh.as_ref().unwrap().key_file;
        assert_eq!(std::fs::read_to_string(file).unwrap(), format!("{pem}\n"));
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(file).unwrap().permissions()) & 0o777, 0o600);
        assert_eq!(k.password.as_deref(), Some("pw-FEDCBA98-7654-3210-FEDC-BA9876543210_database"));
    }

    #[test]
    fn a_kept_key_that_is_no_key_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut items: Vec<Importable> = found().into_iter().filter(|i| i.profile.name == "Key in TablePlus").collect();
        let unread = super::super::read_keychain(&mut items, |_, a| Ok(a.ends_with("_private_key_data").then(|| "not a key".to_string())), dir.path());
        assert_eq!(unread, [("Key in TablePlus".to_string(), "its SSH key is not a private key".to_string())]);
        assert_eq!(items[0].profile.ssh.as_ref().unwrap().key_file, "");
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
