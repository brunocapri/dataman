//! Diskman's core: everything but the UI.
//!
//! The core owns a tokio runtime on its own threads. Every operation returns immediately with a
//! [`Task`] (a one-shot channel) or a shared [`result::ResultSet`]; the UI awaits them on its own
//! executor and never blocks.

pub mod activity;
pub mod backend;
pub mod batch;
pub mod bounded;
pub mod complete;
pub mod config;
pub mod conn;
pub mod crash;
pub mod connstr;
pub mod dataview;
pub mod ddl;
pub mod edit;
pub mod estimate;
pub mod explain;
pub mod export;
pub mod favorites;
pub mod find;
pub mod fknav;
pub mod format;
pub mod guard;
pub mod history;
pub mod import;
pub mod jsontree;
pub mod lar;
pub mod lint;
pub mod order;
pub mod own;
pub mod params;
pub mod parse_check;
pub mod pgpass;
pub mod pgservice;
pub mod relations;
pub mod result;
pub mod rows;
pub mod schema;
pub mod secret;
pub mod server;
pub mod session;
pub mod settings;
pub mod sql;
pub mod sqlgen;
pub mod ssh;
pub mod tx;
pub mod update;
pub mod value;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::oneshot;

pub use backend::{Caps, server_error};
pub use config::{ConfigStore, ConnectionProfile, Dirs, Engine, Environment, McpAccess, SshAuth, SshSettings, SslMode};
pub use sql::Dialect;
pub use conn::TxEnd;
pub use tx::{TxState, TxStatus};
pub use ssh::{HostKeyPrompt, SshError};
pub use conn::{Connection, Session};
pub use favorites::{Favorite, FavoritesStore, NameMatch};
pub use history::{History, HistoryEntry, Outcome};
pub use result::{Phase, ResultSet};
pub use secret::{Keychain, MemorySecrets, SecretStore};
pub use server::{ServerExtension, ServerInfo};
pub use settings::{Settings, SettingsStore, ThemePref, Translucency, UiFont};

/// The result of a background operation, delivered over a one-shot channel.
#[must_use]
pub struct Task<T>(oneshot::Receiver<Result<T>>);

impl<T> Future for Task<T> {
    type Output = Result<T>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T>> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(Ok(r)) => Poll::Ready(r),
            Poll::Ready(Err(_)) => Poll::Ready(Err(anyhow!("the background task was dropped"))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Task<T> {
    /// Takes the result if it is already there.
    pub fn try_take(&mut self) -> Option<Result<T>> {
        self.0.try_recv().ok()
    }
}

pub(crate) fn spawn<T, F>(rt: &Handle, fut: F) -> Task<T>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    Task(rx)
}

pub struct Core {
    rt: Runtime,
    pub dirs: Dirs,
    pub config: ConfigStore,
    pub settings: SettingsStore,
    pub secrets: Arc<dyn SecretStore>,
    pub history: Arc<History>,
    pub favorites: Arc<FavoritesStore>,
    pub extensions: Vec<Arc<dyn ServerExtension>>,
    /// Where to look passwords up when a connection has none saved (`~/.pgpass`, or
    /// `PGPASSFILE`); `None` turns the lookup off.
    pub pgpass: Option<PathBuf>,
    /// SSH passwords and passphrases typed this session but not saved (memory only).
    ssh_typed: Arc<Mutex<HashMap<String, String>>>,
}

/// Finds a connection's secrets off the UI thread: keychain reads and the pgpass file.
#[derive(Clone)]
struct Secrets {
    store: Arc<dyn SecretStore>,
    pgpass: Option<PathBuf>,
    ssh_typed: Arc<Mutex<HashMap<String, String>>>,
}

impl Secrets {
    /// The given password, the saved one, or a matching `~/.pgpass` line (PostgreSQL's only).
    fn password(&self, p: &ConnectionProfile, given: Option<String>) -> Result<Option<String>> {
        if given.is_some() {
            return Ok(given);
        }
        if p.save_password
            && let Some(pw) = self.store.get(&p.id)?
        {
            return Ok(Some(pw));
        }
        if !p.engine.is_postgres() {
            return Ok(None);
        }
        Ok(self.pgpass.as_deref().and_then(|f| pgpass::lookup(f, &p.host, p.port, &p.database, &p.user)))
    }

    /// The SSH password or key passphrase: given, typed earlier this session, or saved.
    fn ssh(&self, p: &ConnectionProfile, given: Option<String>) -> Result<Option<String>> {
        if p.ssh.as_ref().is_none_or(|s| s.auth == SshAuth::Agent) {
            return Ok(None);
        }
        if given.is_some() {
            return Ok(given);
        }
        if let Some(s) = self.ssh_typed.lock().get(&p.id) {
            return Ok(Some(s.clone()));
        }
        if p.save_password {
            return self.store.get(&p.ssh_secret_id());
        }
        Ok(None)
    }
}

/// The tunnel a profile needs, opened (or `None` without SSH).
async fn open_tunnel(p: &ConnectionProfile, ssh_secret: Option<String>, app_known_hosts: PathBuf) -> Result<Option<Arc<ssh::Tunnel>>> {
    match ssh::TunnelSpec::resolve(p, ssh_secret, &app_known_hosts) {
        Some(spec) => Ok(Some(ssh::Tunnel::open(spec).await?)),
        None => Ok(None),
    }
}

/// Where an importable connection comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportSource {
    Service,
    Pgpass,
}

/// A connection `~/.pg_service.conf` or `~/.pgpass` describes.
#[derive(Clone, Debug)]
pub struct Importable {
    pub source: ImportSource,
    pub profile: ConnectionProfile,
    /// A service's own password (pgpass lines keep theirs in the file: looked up on connect).
    pub password: Option<String>,
    /// A saved connection already has these host, port, user and database.
    pub exists: bool,
}

impl Core {
    pub fn new(dirs: Dirs, secrets: Arc<dyn SecretStore>) -> Result<Core> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("diskman-core")
            .enable_all()
            .build()?;
        // Before any TLS: drivers that build their own TLS config take the process' provider.
        backend::install_tls_provider();
        Ok(Core {
            rt,
            config: ConfigStore::open(&dirs)?,
            settings: SettingsStore::open(&dirs),
            history: Arc::new(History::new(dirs.history_dir())),
            favorites: Arc::new(FavoritesStore::open(&dirs)?),
            dirs,
            secrets,
            extensions: server::default_extensions(),
            pgpass: pgpass::default_path(),
            ssh_typed: Default::default(),
        })
    }

    pub fn handle(&self) -> Handle {
        self.rt.handle().clone()
    }

    /// Runs a future on the core runtime and blocks for its result (tests and tools only).
    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    fn resolver(&self) -> Secrets {
        Secrets { store: self.secrets.clone(), pgpass: self.pgpass.clone(), ssh_typed: self.ssh_typed.clone() }
    }

    /// The password to use: the given one, the saved one, or the one `~/.pgpass` has for it.
    /// Reads the keychain and a file: not for the UI thread ([`Core::connect`] and
    /// [`Core::test_connection`] resolve it in the background).
    pub fn password_for(&self, profile: &ConnectionProfile, given: Option<String>) -> Result<Option<String>> {
        self.resolver().password(profile, given)
    }

    /// Saves a profile, and its password and SSH secret in the keychain when they should be
    /// kept. An SSH secret that is not kept stays in memory for this session.
    pub fn save_connection(&self, profile: ConnectionProfile, password: Option<&str>, ssh_secret: Option<&str>) -> Result<()> {
        if profile.save_password {
            if let Some(p) = password {
                self.secrets.set(&profile.id, p)?;
            }
            if let Some(s) = ssh_secret {
                self.secrets.set(&profile.ssh_secret_id(), s)?;
            }
        } else {
            self.secrets.delete(&profile.id)?;
            self.secrets.delete(&profile.ssh_secret_id())?;
        }
        match ssh_secret {
            Some(s) if !profile.save_password => {
                self.ssh_typed.lock().insert(profile.id.clone(), s.to_string());
            }
            _ => {
                self.ssh_typed.lock().remove(&profile.id);
            }
        }
        self.config.upsert(profile)
    }

    pub fn remove_connection(&self, id: &str) -> Result<()> {
        self.secrets.delete(id)?;
        self.secrets.delete(&format!("{id}:ssh"))?;
        self.ssh_typed.lock().remove(id);
        self.config.remove(id)
    }

    /// Connects (through the SSH tunnel, if any), reads the server version, disconnects.
    /// Returns the server info and the round-trip time of `SELECT version()`. Missing secrets
    /// are looked up in the background (keychain, pgpass).
    pub fn test_connection(
        &self,
        profile: ConnectionProfile,
        password: Option<String>,
        ssh_secret: Option<String>,
    ) -> Task<(ServerInfo, std::time::Duration)> {
        let exts = self.extensions.clone();
        let secrets = self.resolver();
        let known = self.dirs.known_hosts_file();
        spawn(&self.handle(), async move {
            let password = secrets.password(&profile, password)?;
            let ssh_secret = secrets.ssh(&profile, ssh_secret)?;
            let tunnel = open_tunnel(&profile, ssh_secret, known).await?;
            let d = backend::dedicated(&profile, password.as_deref(), None, tunnel.as_deref()).await?;
            let t = std::time::Instant::now();
            let info = backend::server_info(&d.db(), &exts).await?;
            let rtt = t.elapsed();
            drop(d);
            if let Some(t) = tunnel {
                t.close();
            }
            Ok((info, rtt))
        })
    }

    /// Opens a connection: the SSH tunnel when the profile has one, a metadata session, the
    /// server info (LAR detection) and an empty schema cache. Editor sessions are opened later,
    /// one per tab.
    pub fn connect(&self, profile: ConnectionProfile, password: Option<String>) -> Task<Arc<Connection>> {
        let handle = self.handle();
        let history = self.history.clone();
        let exts = self.extensions.clone();
        let secrets = self.resolver();
        let known = self.dirs.known_hosts_file();
        spawn(&self.handle(), async move {
            let password = secrets.password(&profile, password)?;
            let ssh_secret = secrets.ssh(&profile, None)?;
            let tunnel = open_tunnel(&profile, ssh_secret, known).await?;
            Connection::open(handle, profile, password, history, exts, tunnel).await
        })
    }

    /// Trusts an SSH host key: written to Diskman's own known_hosts file.
    pub fn trust_host_key(&self, prompt: HostKeyPrompt) -> Task<()> {
        let file = self.dirs.known_hosts_file();
        spawn(&self.handle(), async move { Ok(prompt.trust(&file)?) })
    }

    /// The connections the service files and `~/.pgpass` describe, services first.
    pub fn importable(&self) -> Task<Vec<Importable>> {
        let saved = self.config.connections();
        let pgpass = self.pgpass.clone();
        spawn(&self.handle(), async move { Ok(importable(&saved, pgpass.as_deref())) })
    }

    /// Saves the chosen importable connections; returns their ids.
    pub fn import(&self, items: Vec<Importable>) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for mut it in items {
            it.profile.save_password = it.password.is_some();
            ids.push(it.profile.id.clone());
            self.save_connection(it.profile, it.password.as_deref(), None)?;
        }
        Ok(ids)
    }
}

fn same_target(a: &ConnectionProfile, b: &ConnectionProfile) -> bool {
    (&a.host, a.port, &a.user, &a.database) == (&b.host, b.port, &b.user, &b.database)
}

/// Services (`PGSERVICEFILE` or `~/.pg_service.conf`, then `PGSYSCONFDIR`) and pgpass lines
/// as profiles. A pgpass `*` host cannot be a connection; `*` elsewhere takes libpq's default.
fn importable(saved: &[ConnectionProfile], pgpass_file: Option<&std::path::Path>) -> Vec<Importable> {
    let mut out: Vec<Importable> = Vec::new();
    for s in pgservice::all() {
        let password = s.spec.password.clone();
        let (mut profile, _) = connstr::ConnSpec { service: Some(s.name.clone()), ..s.spec }.into_profile();
        profile.name = s.name;
        out.push(Importable { source: ImportSource::Service, profile, password, exists: false });
    }
    if let Some(f) = pgpass_file
        && let Ok(entries) = pgpass::read(f)
    {
        for e in entries {
            if e.host == "*" {
                continue;
            }
            let mut p = ConnectionProfile::new("");
            p.host = e.host.clone();
            p.port = e.port.parse().unwrap_or(5432);
            p.user = if e.user == "*" { connstr::local_user() } else { e.user.clone() };
            p.database = if e.database == "*" { "postgres".into() } else { e.database.clone() };
            p.name = if e.database == "*" { format!("{}@{}", p.user, p.host) } else { format!("{}@{}", p.database, p.host) };
            if out.iter().any(|o| same_target(&o.profile, &p)) {
                continue;
            }
            out.push(Importable { source: ImportSource::Pgpass, profile: p, password: None, exists: false });
        }
    }
    for it in &mut out {
        it.exists = saved.iter().any(|s| same_target(s, &it.profile));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgpass_fills_missing_passwords_and_imports() {
        let dir = tempfile::tempdir().unwrap();
        let pass = dir.path().join("pgpass");
        std::fs::write(&pass, "db.example:5432:app:bruno:from-pgpass\n*:*:*:*:any\nlocal:*:*:me:x\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&pass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
        let mut core = Core::new(Dirs::at(dir.path().join("cfg")), Arc::new(MemorySecrets::default())).unwrap();
        core.pgpass = Some(pass.clone());
        let mut p = ConnectionProfile::new("x");
        (p.host, p.port, p.user, p.database) = ("db.example".into(), 5432, "bruno".into(), "app".into());
        assert_eq!(core.password_for(&p, None).unwrap().as_deref(), Some("from-pgpass"));
        assert_eq!(core.password_for(&p, Some("typed".into())).unwrap().as_deref(), Some("typed"));
        // A saved password wins over pgpass.
        p.save_password = true;
        core.save_connection(p.clone(), Some("saved"), None).unwrap();
        assert_eq!(core.password_for(&p, None).unwrap().as_deref(), Some("saved"));
        p.database = "other".into();
        p.save_password = false;
        assert_eq!(core.password_for(&p, None).unwrap().as_deref(), Some("any"));

        // Import: the `*` host line is skipped, `*` user/db take defaults; the saved one is marked.
        let mut saved = p.clone();
        saved.database = "app".into();
        let items = importable(&[saved], Some(&pass));
        let names: Vec<(&str, bool)> = items.iter().filter(|i| i.source == ImportSource::Pgpass).map(|i| (i.profile.name.as_str(), i.exists)).collect();
        assert_eq!(names, vec![("app@db.example", true), ("me@local", false)]);
    }

    #[test]
    fn ssh_secrets_never_reach_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let core = Core::new(Dirs::at(dir.path().join("cfg")), secrets.clone()).unwrap();
        let mut p = ConnectionProfile::new("tunnel");
        p.ssh = Some(SshSettings { host: "bastion".into(), auth: SshAuth::Password, ..Default::default() });
        p.save_password = true;
        core.save_connection(p.clone(), Some("db-pw"), Some("ssh-pw")).unwrap();
        assert_eq!(secrets.get(&p.ssh_secret_id()).unwrap().as_deref(), Some("ssh-pw"));
        let text = std::fs::read_to_string(core.dirs.connections_file()).unwrap();
        assert!(!text.contains("ssh-pw") && !text.contains("db-pw"), "{text}");
        assert!(text.contains("[connection.ssh]"), "{text}");
        assert_eq!(core.resolver().ssh(&p, None).unwrap().as_deref(), Some("ssh-pw"));
        // Not kept: out of the keychain, in memory for this session only.
        p.save_password = false;
        core.save_connection(p.clone(), None, Some("typed")).unwrap();
        assert_eq!(secrets.get(&p.ssh_secret_id()).unwrap(), None);
        assert_eq!(core.resolver().ssh(&p, None).unwrap().as_deref(), Some("typed"));
        core.remove_connection(&p.id).unwrap();
        assert_eq!(core.resolver().ssh(&p, None).unwrap(), None);
    }
}
