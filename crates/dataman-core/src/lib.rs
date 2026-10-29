pub mod activity;
pub mod agent_login;
pub mod agents_config;
pub mod approved;
pub mod agent_nosql;
pub mod agent_sql;
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
pub mod groups;
pub mod guard;
pub mod history;
pub mod http;
pub mod import;
pub mod jsontree;
pub mod layout;
pub mod lar;
pub mod lint;
pub mod nosql;
pub mod os;
pub mod order;
pub mod own;
pub mod params;
pub mod parse_check;
pub mod mask;
pub mod pgpass;
mod pgtypes;
pub mod pgservice;
pub mod pins;
pub mod privacy;
pub mod project;
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
pub mod stamp;
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
pub use backend::duck::EXTERNAL_ACCESS as DUCKDB_EXTERNAL_ACCESS;
pub use config::{ConfigStore, ConnectionProfile, Dirs, Engine, Environment, MCP_ADVANCED, McpAccess, SshAuth, SshSettings, SslMode, default_sslmode, is_local_host};
pub use sql::Dialect;
pub use conn::TxEnd;
pub use tx::{TxState, TxStatus};
pub use ssh::{HostKeyPrompt, SshError};
pub use conn::{Connection, Session};
pub use favorites::{Favorite, FavoritesStore, NameMatch};
pub use history::{History, HistoryEntry, Outcome};
pub use pins::PinStore;
pub use result::{Phase, ResultSet};
pub use secret::{Keychain, MemorySecrets, SecretStore};
pub use server::{ServerExtension, ServerInfo};
pub use settings::{Settings, SettingsStore, ThemePref, Translucency, UiFont};

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
    /// The rules for agents and the server as approved in Dataman (see [`approved`]).
    pub approved: Arc<approved::Approved>,
    pub history: Arc<History>,
    pub favorites: Arc<FavoritesStore>,
    pub pins: PinStore,
    pub project: project::ProjectStore,
    pub extensions: Vec<Arc<dyn ServerExtension>>,
    pub pgpass: Option<PathBuf>,
    // Memory only: never written to disk or the keychain. Kept with the tunnel it was typed
    // for: one moved elsewhere (an open project's file, an agent) does not get it.
    ssh_typed: Arc<Mutex<HashMap<String, (Option<SshSettings>, String)>>>,
}

// Off the UI thread: it reads the keychain and the pgpass file.
#[derive(Clone)]
struct Secrets {
    store: Arc<dyn SecretStore>,
    pgpass: Option<PathBuf>,
    ssh_typed: Arc<Mutex<HashMap<String, (Option<SshSettings>, String)>>>,
}

impl Secrets {
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
        // A project's tunnel is a teammate's choice too: no pgpass line goes through it.
        if p.from_project && p.ssh.is_some() {
            return Ok(None);
        }
        let lookup = if p.agent_made || p.from_project { pgpass::lookup_named_host } else { pgpass::lookup };
        Ok(self.pgpass.as_deref().and_then(|f| lookup(f, &p.host, p.port, &p.database, &p.user)))
    }

    fn ssh(&self, p: &ConnectionProfile, given: Option<String>) -> Result<Option<String>> {
        if p.ssh.as_ref().is_none_or(|s| s.auth == SshAuth::Agent) {
            return Ok(None);
        }
        if given.is_some() {
            return Ok(given);
        }
        if let Some((_, s)) = self.ssh_typed.lock().get(&p.id).filter(|(ssh, _)| *ssh == p.ssh) {
            return Ok(Some(s.clone()));
        }
        if p.save_password {
            return self.store.get(&p.ssh_secret_id());
        }
        Ok(None)
    }
}

async fn open_tunnel(p: &ConnectionProfile, ssh_secret: Option<String>, app_known_hosts: PathBuf) -> Result<Option<Arc<ssh::Tunnel>>> {
    match ssh::TunnelSpec::resolve(p, ssh_secret, &app_known_hosts) {
        Some(spec) => Ok(Some(ssh::Tunnel::open(spec).await?)),
        None => Ok(None),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportSource {
    Service,
    Pgpass,
}

#[derive(Clone, Debug)]
pub struct Importable {
    pub source: ImportSource,
    pub profile: ConnectionProfile,
    pub password: Option<String>,
    pub exists: bool,
}

impl Core {
    pub fn new(dirs: Dirs, secrets: Arc<dyn SecretStore>) -> Result<Core> {
        let approved = approved::Approved::kept_in(secrets.clone());
        Core::with_approved(dirs, secrets, approved)
    }

    pub fn with_approved(dirs: Dirs, secrets: Arc<dyn SecretStore>, approved: Arc<approved::Approved>) -> Result<Core> {
        if let Err(e) = dirs.ensure_private() {
            log::warn!("{}: {e}", dirs.config.display());
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("dataman-core")
            .enable_all()
            .build()?;
        // Before any TLS: drivers that build their own TLS config take the process' provider.
        backend::install_tls_provider();
        let config = ConfigStore::open_with(&dirs, approved.clone())?;
        let settings = SettingsStore::open_with(&dirs, approved.clone());
        settings::migrate_mcp_limits(&dirs, &config, &settings);
        Ok(Core {
            rt,
            config,
            settings,
            history: Arc::new(History::new(dirs.history_dir())),
            favorites: Arc::new(FavoritesStore::open(&dirs)?),
            pins: PinStore::open(&dirs),
            project: project::ProjectStore::open(&dirs),
            dirs,
            secrets,
            approved,
            extensions: server::default_extensions(),
            pgpass: pgpass::default_path(),
            ssh_typed: Default::default(),
        })
    }

    /// The local files an open project syncs with.
    pub fn stores(&self) -> project::Stores<'_> {
        project::Stores { config: &self.config, settings: &self.settings, favorites: &self.favorites, secrets: &*self.secrets }
    }

    pub fn handle(&self) -> Handle {
        self.rt.handle().clone()
    }

    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    fn resolver(&self) -> Secrets {
        Secrets { store: self.secrets.clone(), pgpass: self.pgpass.clone(), ssh_typed: self.ssh_typed.clone() }
    }

    // Reads the keychain and a file: not for the UI thread.
    #[cfg(test)]
    pub fn password_for(&self, profile: &ConnectionProfile, given: Option<String>) -> Result<Option<String>> {
        self.resolver().password(profile, given)
    }

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
                self.ssh_typed.lock().insert(profile.id.clone(), (profile.ssh.clone(), s.to_string()));
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
            if !profile.engine.is_sql() {
                let t = std::time::Instant::now();
                let r = nosql::connect(&profile, password.as_deref(), tunnel.as_deref()).await;
                let rtt = t.elapsed();
                if let Some(t) = tunnel {
                    t.close();
                }
                return Ok((r?.1, rtt));
            }
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

    pub fn trust_host_key(&self, prompt: HostKeyPrompt) -> Task<()> {
        let file = self.dirs.known_hosts_file();
        spawn(&self.handle(), async move { Ok(prompt.trust(&file)?) })
    }

    pub fn importable(&self) -> Task<Vec<Importable>> {
        let saved = self.config.connections();
        let pgpass = self.pgpass.clone();
        spawn(&self.handle(), async move { Ok(importable(&saved, pgpass.as_deref())) })
    }

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
        p.save_password = true;
        core.save_connection(p.clone(), Some("saved"), None).unwrap();
        assert_eq!(core.password_for(&p, None).unwrap().as_deref(), Some("saved"));
        p.database = "other".into();
        p.save_password = false;
        assert_eq!(core.password_for(&p, None).unwrap().as_deref(), Some("any"));

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
        p.save_password = false;
        core.save_connection(p.clone(), None, Some("typed")).unwrap();
        assert_eq!(secrets.get(&p.ssh_secret_id()).unwrap(), None);
        assert_eq!(core.resolver().ssh(&p, None).unwrap().as_deref(), Some("typed"));
        // Moved to another bastion (an open project's file): the typed secret stays behind.
        let mut moved = p.clone();
        moved.ssh.as_mut().unwrap().host = "bastion.attacker.example".into();
        assert_eq!(core.resolver().ssh(&moved, None).unwrap(), None);
        core.remove_connection(&p.id).unwrap();
        assert_eq!(core.resolver().ssh(&p, None).unwrap(), None);
    }

    #[test]
    fn a_projects_connection_takes_no_pgpass_wildcard_nor_a_line_through_its_tunnel() {
        let dir = tempfile::tempdir().unwrap();
        let pass = dir.path().join("pgpass");
        std::fs::write(&pass, "db.example:5432:*:bruno:named\n*:*:*:*:any\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&pass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
        let mut core = Core::new(Dirs::at(dir.path().join("cfg")), Arc::new(MemorySecrets::default())).unwrap();
        core.pgpass = Some(pass);
        let mut p = ConnectionProfile::new("x");
        (p.host, p.port, p.user, p.from_project) = ("evil.example".into(), 5432, "bruno".into(), true);
        assert_eq!(core.password_for(&p, None).unwrap(), None, "the * line");
        p.host = "db.example".into();
        assert_eq!(core.password_for(&p, None).unwrap().as_deref(), Some("named"));
        p.ssh = Some(SshSettings { host: "bastion.attacker.example".into(), auth: SshAuth::Agent, ..Default::default() });
        assert_eq!(core.password_for(&p, None).unwrap(), None, "through the project's tunnel");
    }
}
