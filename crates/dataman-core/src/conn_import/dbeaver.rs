//! DBeaver keeps a workspace per install, a folder per project in it, and each project's
//! connections in `.dbeaver/data-sources*.json`. Users and passwords are beside them, in the
//! `credentials-config*.json` of the same suffix, encrypted with a key DBeaver ships.

use std::path::{Path, PathBuf};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use serde_json::{Map, Value};

use super::{ImportSource, Importable, engine_named, entries, file_profile, port, server_profile};
use crate::config::{Engine, Environment, SshAuth, SshSettings};
use crate::connstr::{self, ConnSpec};

/// Not a secret: DBeaver's own (`DefaultSecureStorage`), the same in every install.
const KEY: [u8; 16] = [0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a];

const SOURCE: ImportSource = ImportSource::DBeaver;

/// Each place an install keeps its workspace; Linux's Flatpak and Snap keep theirs in their
/// sandboxes.
pub fn workspaces() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if cfg!(target_os = "macos") {
        roots.extend(crate::os::home().map(|h| h.join("Library")));
    } else if cfg!(windows) {
        roots.extend(crate::os::app_data());
    } else {
        roots.extend(crate::os::data_home());
        if let Some(h) = crate::os::home() {
            roots.push(h.join(".var/app/io.dbeaver.DBeaverCommunity/data"));
            roots.push(h.join("snap/dbeaver-ce/current/.local/share"));
        }
    }
    roots.into_iter().map(|r| r.join("DBeaverData").join("workspace6")).collect()
}

pub fn read(workspaces: &[PathBuf]) -> Vec<Importable> {
    let mut out = Vec::new();
    for root in workspaces.iter().filter(|r| r.is_dir()) {
        let projects = match entries(root) {
            Ok(p) => p,
            Err(e) => {
                out.push(Importable::refused(SOURCE, root.display().to_string(), format!("could not be read: {e}")));
                continue;
            }
        };
        for dir in projects.iter().map(|p| p.join(".dbeaver")).filter(|d| d.is_dir()) {
            let files = match entries(&dir) {
                Ok(f) => f,
                Err(e) => {
                    out.push(Importable::refused(SOURCE, dir.display().to_string(), format!("could not be read: {e}")));
                    continue;
                }
            };
            for file in &files {
                let Some(suffix) = file.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("data-sources")).filter(|s| s.ends_with(".json")) else { continue };
                read_file(file, &dir.join(format!("credentials-config{suffix}")), &mut out);
            }
        }
    }
    out
}

/// A credentials file that does not open costs the logins, not the connections: those come in,
/// each saying why it has none.
fn read_file(data_sources: &Path, credentials: &Path, out: &mut Vec<Importable>) {
    let (creds, creds_note) = match std::fs::read(credentials) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Value::Null, None),
        Err(e) => (Value::Null, Some(format!("user and password not read: {e}"))),
        Ok(bytes) => match decrypt(&bytes) {
            Ok(v) => (v, None),
            Err(e) => (Value::Null, Some(format!("user and password not read: {} {e}", credentials.display()))),
        },
    };
    let doc = std::fs::read(data_sources).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()));
    let doc = match doc {
        Ok(d) => d,
        Err(e) => return out.push(Importable::refused(SOURCE, data_sources.display().to_string(), format!("could not be read: {e}"))),
    };
    let Some(conns) = doc.get("connections").and_then(Value::as_object) else { return };
    for (id, conn) in conns {
        let name = text(conn, "name").unwrap_or(id);
        let mut it = match connection(name, conn, creds.get(id)) {
            Ok(it) => it,
            Err(why) => Importable::refused(SOURCE, name, why),
        };
        it.notes.extend(creds_note.clone().filter(|_| it.refused.is_none()));
        out.push(it);
    }
}

/// The first block is the IV; the rest is AES-128-CBC with PKCS#5 padding, over JSON.
fn decrypt(bytes: &[u8]) -> Result<Value, String> {
    if bytes.len() < 32 {
        return Err("is too short to be DBeaver's".into());
    }
    let (iv, data) = bytes.split_at(16);
    let plain = cbc::Decryptor::<aes::Aes128>::new_from_slices(&KEY, iv).map_err(|e| e.to_string())?.decrypt_padded_vec_mut::<Pkcs7>(data).map_err(|_| "does not decrypt with DBeaver's key".to_string())?;
    serde_json::from_slice(&plain).map_err(|e| e.to_string())
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// A value of a handler's or the driver's properties: DBeaver writes some as strings, some not.
fn prop(props: Option<&Map<String, Value>>, key: &str) -> Option<String> {
    let (_, v) = props?.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?;
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn is_true(v: Option<String>) -> bool {
    v.is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn enabled(handler: &Value) -> bool {
    handler.get("enabled").and_then(Value::as_bool) == Some(true)
}

fn connection(name: &str, conn: &Value, creds: Option<&Value>) -> Result<Importable, String> {
    // One made from a template names the template's driver; the database is the original's.
    let named = |original: &str, key: &str| text(conn, original).or_else(|| text(conn, key)).unwrap_or("");
    let (provider, driver) = (named("original-provider", "provider"), named("original-driver", "driver"));
    // DBeaver files Sybase's drivers under its SQL Server provider.
    if driver.to_ascii_lowercase().contains("sybase") {
        return Err("Sybase is not an engine Dataman connects to".into());
    }
    let (engine, flavor) = engine_named(|c| c.dbeaver, driver)
        .or_else(|| engine_named(|c| c.dbeaver, provider))
        .ok_or_else(|| format!("{} is not an engine Dataman connects to", if provider == "generic" || provider.is_empty() { driver } else { provider }))?;
    let cfg = conn.get("configuration").unwrap_or(&Value::Null);
    let env = match text(cfg, "type") {
        Some("prod") => Environment::Production,
        Some("test") => Environment::Staging,
        Some("dev") => Environment::Dev,
        _ => Environment::None,
    };
    let url = text(cfg, "url");
    if engine.is_file() {
        let path = text(cfg, "database").or_else(|| url.and_then(|u| u.strip_prefix("jdbc:")?.split_once(':')).map(|(_, p)| p)).ok_or("it names no database file")?;
        return Ok(Importable::new(SOURCE, file_profile(engine, path, name, env), None));
    }
    let login = creds.and_then(|c| c.get("#connection"));
    let user = login.and_then(|l| text(l, "user")).or_else(|| text(cfg, "user"));
    let password = login.and_then(|l| text(l, "password")).or_else(|| text(cfg, "password"));
    let by_url = text(cfg, "configurationType") == Some("URL") || (text(cfg, "host").is_none() && url.is_some());
    let mut spec = if by_url {
        let url = url.ok_or("it names no host")?;
        let spec = connstr::parse(url).ok_or_else(|| format!("its URL {url} could not be read"))?;
        if spec.engine != engine {
            return Err(format!("its URL {url} is not a {} URL", engine.spec().label));
        }
        spec
    } else {
        ConnSpec {
            engine,
            host: Some(text(cfg, "host").ok_or("it names no host")?.to_string()),
            port: port(text(cfg, "port").unwrap_or(""))?,
            database: text(cfg, "database").map(str::to_string),
            ..Default::default()
        }
    };
    spec.flavor = flavor;
    spec.user = spec.user.or(user.map(str::to_string));
    spec.password = spec.password.or(password.map(str::to_string));
    let mut notes = Vec::new();
    if spec.user.is_none() {
        notes.push("no user saved: the engine's default".into());
    }
    let handlers = cfg.get("handlers").and_then(Value::as_object);
    let props = cfg.get("properties").and_then(Value::as_object);
    if let Some(mode) = tls(engine, handlers, props, &mut notes) {
        spec.sslmode = Some(mode);
    }
    if engine == Engine::SqlServer {
        for key in ["encrypt", "trustServerCertificate"] {
            if let Some(v) = prop(props, key) {
                spec.set_mssql(&key.to_ascii_lowercase(), &v);
            }
        }
    }
    let ssh_handler = handlers.and_then(|h| h.get("ssh_tunnel")).filter(|h| enabled(h));
    let ssh_login = creds.and_then(|c| c.get("network/ssh_tunnel"));
    let ssh = ssh_handler.map(|h| tunnel(h, ssh_login)).transpose()?;
    let (mut profile, password) = server_profile(spec, name, env);
    let ssh_password = ssh.as_ref().and(ssh_login.and_then(|l| text(l, "password")).map(str::to_string));
    profile.ssh = ssh;
    Ok(Importable { ssh_password, notes, ..Importable::new(SOURCE, profile, password) })
}

/// The SSL tab (a handler named `*_ssl`, ClickHouse's `clickhouse-ssl`), else the driver's
/// `sslmode`. MySQL's tab says only whether TLS is required and whether the certificate is
/// checked. None: nothing says, and the engine's default applies.
fn tls(engine: Engine, handlers: Option<&Map<String, Value>>, props: Option<&Map<String, Value>>, notes: &mut Vec<String>) -> Option<crate::config::SslMode> {
    use crate::config::SslMode;
    let tab = handlers.and_then(|h| h.iter().find(|(id, h)| (id.ends_with("_ssl") || id.ends_with("-ssl")) && enabled(h))).map(|(_, h)| h.get("properties").and_then(Value::as_object));
    let tab_prop = |k: &str| tab.and_then(|p| prop(p, k));
    if tab_prop("ssl.client.cert").is_some() || tab_prop("ssl.client.key").is_some() {
        notes.push("client certificate left behind".into());
    }
    if tab_prop("ssl.ca.cert").is_some() || prop(props, "sslrootcert").is_some() {
        notes.push("CA certificate left behind: the system's are trusted".into());
    }
    let mut named = |mode: String| {
        connstr::sslmode(&mode.to_ascii_lowercase().replace('_', "-")).unwrap_or_else(|| {
            notes.push(format!("SSL mode {mode} read as verify-full"));
            SslMode::VerifyFull
        })
    };
    if engine == Engine::ClickHouse {
        // Its tab is strict or none; on, it defaults to strict.
        return Some(match (tab, tab_prop("ssl.mode")) {
            (None, _) if !is_true(prop(props, "ssl")) => SslMode::Disable,
            (_, Some(m)) if m.eq_ignore_ascii_case("none") => SslMode::Require,
            _ => SslMode::VerifyFull,
        });
    }
    if let Some(mode) = tab_prop("sslMode").or_else(|| prop(props, "sslmode")) {
        return Some(named(mode));
    }
    if is_true(tab_prop("ssl.verify.server")) {
        return Some(SslMode::VerifyFull);
    }
    if is_true(tab_prop("ssl.require")) {
        return Some(SslMode::Require);
    }
    tab.map(|_| SslMode::Prefer)
}

fn tunnel(handler: &Value, login: Option<&Value>) -> Result<SshSettings, String> {
    let props = handler.get("properties").and_then(Value::as_object);
    let p = |k: &str| prop(props, k);
    if p("jumpServer.count").and_then(|c| c.parse::<u32>().ok()).is_some_and(|c| c > 0) || is_true(p("jumpServerEnabled")) {
        return Err("its SSH tunnel goes through a jump server, which Dataman does not".into());
    }
    let host = p("host").ok_or("its SSH tunnel names no host")?;
    let (auth, key_file) = match p("authType").as_deref() {
        Some("PUBLIC_KEY") => (SshAuth::Key, p("keyPath").unwrap_or_default()),
        Some("AGENT") => (SshAuth::Agent, String::new()),
        _ => (SshAuth::Password, String::new()),
    };
    let user = login.and_then(|l| text(l, "user")).or_else(|| text(handler, "user")).map(str::to_string).or_else(|| p("user")).unwrap_or_default();
    Ok(SshSettings { host, port: port(&p("port").unwrap_or_default())?, user, auth, key_file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SslMode, SshAuth};
    use crate::server::Flavor;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/conn_import/testdata/dbeaver/workspace6")
    }

    fn found() -> Vec<Importable> {
        read(&[fixture()])
    }

    fn by<'a>(items: &'a [Importable], name: &str) -> &'a Importable {
        items.iter().find(|i| i.profile.name == name).unwrap_or_else(|| panic!("{name} in {items:?}"))
    }

    #[test]
    fn decrypts_the_credentials_dbeaver_wrote() {
        let bytes = std::fs::read(fixture().join("General/.dbeaver/credentials-config.json")).unwrap();
        let v = decrypt(&bytes).unwrap();
        assert_eq!(v["postgres-jdbc-18c8d1b2a3f-6e5e3c0f1a1d2b3c"]["#connection"]["user"], "app");
        assert!(decrypt(&bytes[..40]).is_err());
    }

    #[test]
    fn a_postgres_connection_with_its_login_tls_and_tunnel() {
        let items = found();
        let it = by(&items, "Orders (prod)");
        let p = &it.profile;
        assert_eq!((p.engine, p.host.as_str(), p.port, p.user.as_str(), p.database.as_str()), (Engine::Postgres, "db.internal", 5433, "app", "orders"));
        assert_eq!((p.sslmode, p.env), (SslMode::VerifyFull, Environment::Production));
        assert_eq!(it.password.as_deref(), Some("orders-pw"));
        let ssh = p.ssh.as_ref().unwrap();
        assert_eq!((ssh.host.as_str(), ssh.port, ssh.user.as_str(), ssh.auth, ssh.key_file.as_str()), ("bastion.example.com", Some(2222), "deploy", SshAuth::Key, "~/.ssh/id_ed25519"));
        assert_eq!(it.ssh_password.as_deref(), Some("key-passphrase"));
        assert!(it.refused.is_none() && it.notes.is_empty(), "{it:?}");
    }

    #[test]
    fn mariadb_files_and_urls() {
        let items = found();
        let m = by(&items, "Shop");
        assert_eq!((m.profile.engine, m.profile.flavor, m.profile.port, m.profile.sslmode), (Engine::MySql, Flavor::MariaDb, 3306, SslMode::Require));
        assert_eq!((m.profile.user.as_str(), m.password.as_deref()), ("shop", None));
        let lite = by(&items, "Local lite");
        assert_eq!((lite.profile.engine, lite.profile.path.as_deref()), (Engine::Sqlite, Some("/Users/me/data/app.sqlite")));
        let u = by(&items, "By URL");
        assert_eq!((u.profile.host.as_str(), u.profile.port, u.profile.database.as_str(), u.profile.user.as_str()), ("pg.example.com", 6432, "analytics", "reader"));
        assert_eq!(u.password.as_deref(), Some("url-pw"));
        let ms = by(&items, "Warehouse");
        assert_eq!((ms.profile.engine, ms.profile.sslmode), (Engine::SqlServer, SslMode::Require));
    }

    #[test]
    fn what_cannot_come_in_says_why() {
        let items = found();
        assert!(by(&items, "Oracle HR").refused.as_deref().unwrap().contains("oracle"));
        assert!(by(&items, "Behind two hops").refused.as_deref().unwrap().contains("jump server"));
        assert!(by(&items, "No host").refused.as_deref().unwrap().contains("no host"));
        let shop = by(&items, "Shop");
        assert!(shop.notes.iter().any(|n| n.contains("client certificate")), "{:?}", shop.notes);
    }

    #[test]
    fn credentials_that_do_not_decrypt_cost_the_logins_and_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("General/.dbeaver");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::copy(fixture().join("General/.dbeaver/data-sources.json"), project.join("data-sources.json")).unwrap();
        std::fs::write(project.join("credentials-config.json"), [7u8; 48]).unwrap();
        let items = read(&[dir.path().to_path_buf()]);
        let it = by(&items, "Orders (prod)");
        assert_eq!(it.password, None);
        assert!(it.notes.iter().any(|n| n.starts_with("user and password not read")), "{:?}", it.notes);
        std::fs::write(project.join("data-sources.json"), "{ not json").unwrap();
        let items = read(&[dir.path().to_path_buf()]);
        assert_eq!(items.len(), 1);
        assert!(items[0].refused.as_deref().unwrap().starts_with("could not be read"), "{items:?}");
    }
}
