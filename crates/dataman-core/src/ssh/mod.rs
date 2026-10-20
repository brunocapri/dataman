//! SSH tunnels (russh, pure Rust): a local listener on `127.0.0.1:0` whose every accepted
//! socket becomes a `direct-tcpip` channel (or `direct-streamlocal` for a Unix socket) to the
//! database, as seen from the SSH server. Postgres connects to the listener, so TLS, the
//! CancelRequest and every later connection of the profile go through the same tunnel.
//!
//! The SSH session lives as long as the [`Tunnel`] (one per open connection), with keepalives.
//! When it drops, the next connection through the tunnel reconnects it; the listener (and so
//! the port the cancel tokens point at) never changes. [`Tunnel::close`] (or dropping it) ends
//! the session.

pub mod config;
pub mod known_hosts;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse, Msg};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelStream, MethodKind};

use crate::config::{ConnectionProfile, SshAuth, SshSettings};
use config::HostConfig;
use known_hosts::Status;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const KEEPALIVE: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------------------------
// Errors

/// An unknown host key: what to show before trusting it.
#[derive(Clone, Debug)]
pub struct HostKeyPrompt {
    /// `host` or `host:port`, as shown.
    pub host: String,
    pub hostname: String,
    pub port: u16,
    /// `ssh-ed25519`, `ecdsa-sha2-nistp256`, `ssh-rsa`…
    pub algorithm: String,
    /// `SHA256:…`, as `ssh` prints it.
    pub fingerprint: String,
    key: PublicKey,
}

impl PartialEq for HostKeyPrompt {
    fn eq(&self, o: &Self) -> bool {
        (&self.hostname, self.port, &self.fingerprint) == (&o.hostname, o.port, &o.fingerprint)
    }
}

impl HostKeyPrompt {
    /// A prompt for `hostname:port` presenting `key` (`ssh-ed25519 AAAA…`, as in known_hosts).
    pub fn from_openssh(hostname: &str, port: u16, key: &str) -> Option<HostKeyPrompt> {
        let key = PublicKey::from_openssh(key).ok()?;
        Some(HostKeyPrompt {
            host: shown(hostname, port),
            hostname: hostname.into(),
            port,
            algorithm: key.algorithm().as_str().to_string(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            key,
        })
    }

    /// Remembers the key in `file` (Dataman's own known_hosts).
    pub fn trust(&self, file: &Path) -> std::io::Result<()> {
        known_hosts::append(file, &self.hostname, self.port, &self.key)
    }
}

/// Why a tunnel could not be opened; the message is short and keeps the server's or the
/// library's own words.
#[derive(Clone, Debug, thiserror::Error)]
pub enum SshError {
    #[error("SSH {} · unknown host key {} {}", .0.host, .0.algorithm, .0.fingerprint)]
    UnknownHostKey(HostKeyPrompt),
    #[error("SSH {host} · host key changed ({file}:{line}) · {fingerprint}")]
    HostKeyChanged { host: String, fingerprint: String, file: String, line: usize },
    #[error("SSH {host} · host key revoked ({file}:{line})")]
    HostKeyRevoked { host: String, file: String, line: usize },
    #[error("SSH {host} · {message}")]
    Connect { host: String, message: String },
    #[error("SSH {user}@{host} · authentication failed{detail}")]
    Auth { host: String, user: String, detail: String },
    #[error("SSH {host} · {what} needed")]
    SecretNeeded { host: String, what: &'static str },
    #[error("SSH tunnel to {target} · {message}")]
    Forward { target: String, message: String },
}

fn shown(host: &str, port: u16) -> String {
    if port == 22 { host.to_string() } else { format!("{host}:{port}") }
}

fn russh_message(e: &russh::Error) -> String {
    match e {
        russh::Error::IO(io) => io.to_string(),
        russh::Error::Disconnect | russh::Error::HUP => "connection closed by the server".into(),
        russh::Error::ChannelOpenFailure(f) => channel_failure(f),
        e => e.to_string(),
    }
}

fn channel_failure(f: &russh::ChannelOpenFailure) -> String {
    match f {
        russh::ChannelOpenFailure::AdministrativelyProhibited => "forwarding prohibited by the server".into(),
        russh::ChannelOpenFailure::ConnectFailed => "connect failed".into(),
        russh::ChannelOpenFailure::UnknownChannelType => "channel type not supported by the server".into(),
        russh::ChannelOpenFailure::ResourceShortage => "resource shortage".into(),
        russh::ChannelOpenFailure::Other { reason, .. } if !reason.is_empty() => reason.clone(),
        russh::ChannelOpenFailure::Other { code, .. } => format!("open failed ({code})"),
    }
}

// ---------------------------------------------------------------------------------------------
// What to connect to

/// How one hop authenticates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HopAuth {
    /// The agent's keys (only those of `keys` with IdentitiesOnly), then the unencrypted `keys`.
    Agent { keys: Vec<PathBuf>, only: bool },
    Key { path: PathBuf, passphrase: Option<String> },
    Password(Option<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: HopAuth,
}

/// Where the database is, from the last hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Tcp(String, u16),
    /// The socket file (`/var/run/postgresql/.s.PGSQL.5432`).
    Unix(String),
}

impl Target {
    pub fn of(p: &ConnectionProfile) -> Target {
        if p.is_socket() {
            Target::Unix(format!("{}/.s.PGSQL.{}", p.host.trim_end_matches('/'), p.port))
        } else {
            Target::Tcp(p.host.clone(), p.port)
        }
    }

    fn shown(&self) -> String {
        match self {
            Target::Tcp(h, p) => format!("{h}:{p}"),
            Target::Unix(s) => s.clone(),
        }
    }
}

/// A resolved tunnel: the jump hosts (ProxyJump), then the SSH host, then the database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelSpec {
    pub hops: Vec<Hop>,
    pub target: Target,
    pub known_hosts: Vec<PathBuf>,
}

/// `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`: what ssh tries without an IdentityFile.
fn default_keys(home: &Path) -> Vec<PathBuf> {
    ["id_ed25519", "id_ecdsa", "id_rsa"].iter().map(|n| home.join(".ssh").join(n)).collect()
}

fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(s),
    }
}

impl TunnelSpec {
    /// From the profile's SSH settings, `~/.ssh/config` and the secret (password or
    /// passphrase) the user gave or saved.
    pub fn resolve(p: &ConnectionProfile, secret: Option<String>, app_known_hosts: &Path) -> Option<TunnelSpec> {
        let s = p.ssh.as_ref()?;
        let home = config::home().unwrap_or_else(|| PathBuf::from("/"));
        Some(Self::resolve_with(s, secret, Target::of(p), known_hosts::default_files(app_known_hosts), &home, &crate::connstr::local_user(), config::resolve_default))
    }

    /// [`resolve`](Self::resolve) with the ssh config lookup and the environment given.
    pub fn resolve_with(
        s: &SshSettings,
        secret: Option<String>,
        target: Target,
        known_hosts: Vec<PathBuf>,
        home: &Path,
        local_user: &str,
        lookup: impl Fn(&str) -> HostConfig,
    ) -> TunnelSpec {
        let alias = s.host.trim();
        let hc = lookup(alias);
        let host = hc.real_host(alias);
        let port = s.port.or(hc.port).unwrap_or(22);
        let user = if s.user.trim().is_empty() { hc.user.clone().unwrap_or_else(|| local_user.to_string()) } else { s.user.trim().to_string() };
        let ids = |hc: &HostConfig, user: &str, host: &str, port: u16| {
            let v = hc.identity_paths(home, local_user, user, host, port);
            if v.is_empty() { default_keys(home) } else { v }
        };
        let auth = match s.auth {
            SshAuth::Agent => HopAuth::Agent { keys: ids(&hc, &user, &host, port), only: hc.identities_only == Some(true) },
            SshAuth::Key => {
                let path = if s.key_file.trim().is_empty() {
                    let c = ids(&hc, &user, &host, port);
                    c.iter().find(|p| p.exists()).cloned().unwrap_or_else(|| c[0].clone())
                } else {
                    expand_tilde(s.key_file.trim(), home)
                };
                HopAuth::Key { path, passphrase: secret }
            }
            SshAuth::Password => HopAuth::Password(secret),
        };
        let mut hops: Vec<Hop> = hc
            .jumps()
            .into_iter()
            .map(|j| {
                let jc = lookup(&j.host);
                let host = jc.real_host(&j.host);
                let port = j.port.or(jc.port).unwrap_or(22);
                let user = j.user.clone().or_else(|| jc.user.clone()).unwrap_or_else(|| local_user.to_string());
                let auth = HopAuth::Agent { keys: ids(&jc, &user, &host, port), only: jc.identities_only == Some(true) };
                Hop { host, port, user, auth }
            })
            .collect();
        hops.push(Hop { host, port, user, auth });
        TunnelSpec { hops, target, known_hosts }
    }
}

// ---------------------------------------------------------------------------------------------
// One hop

/// The russh handler: checks the server's key against known_hosts and says why it refused.
struct Client {
    host: String,
    port: u16,
    files: Vec<PathBuf>,
    refusal: Arc<Mutex<Option<SshError>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let host = shown(&self.host, self.port);
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.refusal.lock() = Some(SshError::Connect { host, message: "host certificates are not supported".into() });
            return Ok(false);
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let refusal = match known_hosts::check(&self.files, &self.host, self.port, key) {
            Status::Known => return Ok(true),
            Status::Unknown => SshError::UnknownHostKey(HostKeyPrompt {
                host,
                hostname: self.host.clone(),
                port: self.port,
                algorithm: key.algorithm().as_str().to_string(),
                fingerprint,
                key: key.clone(),
            }),
            Status::Changed { path, line } => SshError::HostKeyChanged { host, fingerprint, file: tilde(&path), line },
            Status::Revoked { path, line } => SshError::HostKeyRevoked { host, file: tilde(&path), line },
        };
        *self.refusal.lock() = Some(refusal);
        Ok(false)
    }
}

/// `~/.ssh/known_hosts` instead of the full path, in messages.
fn tilde(p: &Path) -> String {
    match config::home().and_then(|h| p.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

type Session = Handle<Client>;

/// The algorithm family as known_hosts writes it (`ssh-rsa` for every RSA hash).
fn family(a: &russh::keys::Algorithm) -> String {
    match a {
        russh::keys::Algorithm::Rsa { .. } => "ssh-rsa".into(),
        a => a.as_str().to_string(),
    }
}

async fn connect_hop(hop: &Hop, via: Option<&Session>, files: &[PathBuf]) -> Result<Session, SshError> {
    let host = shown(&hop.host, hop.port);
    let refusal = Arc::new(Mutex::new(None));
    let handler = Client { host: hop.host.clone(), port: hop.port, files: files.to_vec(), refusal: refusal.clone() };
    let mut cfg = client::Config { keepalive_interval: Some(KEEPALIVE), keepalive_max: 3, nodelay: true, ..Default::default() };
    // Ask first for a key type known_hosts already has for this host, as ssh does, so a host
    // known by its RSA key is not "unknown" because it also has an Ed25519 one.
    let known = known_hosts::known_types(files, &hop.host, hop.port);
    if !known.is_empty() {
        let mut keys = cfg.preferred.key.to_vec();
        keys.sort_by_key(|a| !known.contains(&family(a)));
        cfg.preferred.key = keys.into();
    }
    let cfg = Arc::new(cfg);
    let conn_err = |message: String| SshError::Connect { host: host.clone(), message };
    let fut = async {
        match via {
            None => {
                let tcp = tokio::net::TcpStream::connect((hop.host.as_str(), hop.port)).await.map_err(|e| conn_err(e.to_string()))?;
                let _ = tcp.set_nodelay(true);
                client::connect_stream(cfg, tcp, handler).await.map_err(|e| conn_err(russh_message(&e)))
            }
            Some(prev) => {
                let ch = prev.channel_open_direct_tcpip(hop.host.clone(), hop.port as u32, "127.0.0.1", 0).await.map_err(|e| conn_err(russh_message(&e)))?;
                client::connect_stream(cfg, ch.into_stream(), handler).await.map_err(|e| conn_err(russh_message(&e)))
            }
        }
    };
    let r = match tokio::time::timeout(CONNECT_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(conn_err("timed out".into())),
    };
    let mut session = match r {
        Ok(s) => s,
        Err(e) => return Err(refusal.lock().take().unwrap_or(e)),
    };
    match tokio::time::timeout(CONNECT_TIMEOUT, authenticate(&mut session, hop)).await {
        Ok(r) => r?,
        Err(_) => return Err(conn_err("authentication timed out".into())),
    }
    Ok(session)
}

fn methods(m: &russh::MethodSet) -> String {
    let names: Vec<&str> = m.iter().map(<&str>::from).filter(|n| *n != "none").collect();
    if names.is_empty() { String::new() } else { format!(" ({})", names.join(",")) }
}

async fn rsa_hash(s: &Session, key: &PublicKey) -> Option<HashAlg> {
    if !key.algorithm().is_rsa() {
        return None;
    }
    match s.best_supported_rsa_hash().await {
        Ok(Some(h)) => h,
        _ => Some(HashAlg::Sha512),
    }
}

async fn authenticate(s: &mut Session, hop: &Hop) -> Result<(), SshError> {
    let host = shown(&hop.host, hop.port);
    let user = hop.user.clone();
    let failed = |remaining: Option<&russh::MethodSet>| SshError::Auth {
        host: host.clone(),
        user: user.clone(),
        detail: remaining.map(methods).unwrap_or_default(),
    };
    let lib = |e: russh::Error| SshError::Connect { host: host.clone(), message: russh_message(&e) };
    let mut remaining: Option<russh::MethodSet> = None;
    match &hop.auth {
        HopAuth::Agent { keys, only } => {
            if let Ok(mut agent) = AgentClient::connect_env().await {
                let wanted: Vec<PublicKey> = if *only { keys.iter().filter_map(|k| public_half(k)).collect() } else { vec![] };
                for id in agent.request_identities().await.unwrap_or_default() {
                    let AgentIdentity::PublicKey { key, .. } = id else { continue };
                    if *only && !wanted.iter().any(|w| w.key_data() == key.key_data()) {
                        continue;
                    }
                    let hash = rsa_hash(s, &key).await;
                    match s.authenticate_publickey_with(user.clone(), key, hash, &mut agent).await {
                        Ok(AuthResult::Success) => return Ok(()),
                        Ok(AuthResult::Failure { remaining_methods, .. }) => remaining = Some(remaining_methods),
                        Err(e) => log::debug!("ssh agent signing failed: {e:?}"),
                    }
                }
            }
            // Then the identity files that need no passphrase.
            for path in keys.iter().filter(|p| p.exists()) {
                let Ok(key) = russh::keys::load_secret_key(path, None) else { continue };
                let key = Arc::new(key);
                let hash = rsa_hash(s, key.public_key()).await;
                match s.authenticate_publickey(user.clone(), PrivateKeyWithHashAlg::new(key, hash)).await.map_err(lib)? {
                    AuthResult::Success => return Ok(()),
                    AuthResult::Failure { remaining_methods, .. } => remaining = Some(remaining_methods),
                }
            }
            Err(failed(remaining.as_ref()))
        }
        HopAuth::Key { path, passphrase } => {
            let key = match russh::keys::load_secret_key(path, passphrase.as_deref()) {
                Ok(k) => Arc::new(k),
                Err(russh::keys::Error::KeyIsEncrypted) => return Err(SshError::SecretNeeded { host, what: "key passphrase" }),
                Err(russh::keys::Error::IO(e)) => {
                    return Err(SshError::Auth { host: host.clone(), user, detail: format!(" · {}: {e}", tilde(path)) });
                }
                Err(e) => return Err(SshError::Auth { host: host.clone(), user, detail: format!(" · {}: {e}", tilde(path)) }),
            };
            let hash = rsa_hash(s, key.public_key()).await;
            match s.authenticate_publickey(user.clone(), PrivateKeyWithHashAlg::new(key, hash)).await.map_err(lib)? {
                AuthResult::Success => Ok(()),
                AuthResult::Failure { remaining_methods, .. } => Err(failed(Some(&remaining_methods))),
            }
        }
        HopAuth::Password(pw) => {
            let Some(pw) = pw.as_deref() else { return Err(SshError::SecretNeeded { host, what: "password" }) };
            match s.authenticate_password(user.clone(), pw).await.map_err(lib)? {
                AuthResult::Success => return Ok(()),
                AuthResult::Failure { remaining_methods, .. } => remaining = Some(remaining_methods),
            }
            // Servers with only keyboard-interactive (PAM) ask for the same password there.
            if remaining.as_ref().is_some_and(|m| m.contains(&MethodKind::KeyboardInteractive)) {
                let mut r = s.authenticate_keyboard_interactive_start(user.clone(), None).await.map_err(lib)?;
                for _ in 0..4 {
                    match r {
                        KeyboardInteractiveAuthResponse::Success => return Ok(()),
                        KeyboardInteractiveAuthResponse::Failure { remaining_methods, .. } => {
                            remaining = Some(remaining_methods);
                            break;
                        }
                        KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                            let answers = prompts.iter().map(|_| pw.to_string()).collect();
                            r = s.authenticate_keyboard_interactive_respond(answers).await.map_err(lib)?;
                        }
                    }
                }
            }
            Err(failed(remaining.as_ref()))
        }
    }
}

/// The public half of an identity file (its `.pub`, or the key itself when unencrypted).
fn public_half(path: &Path) -> Option<PublicKey> {
    let mut pub_path = path.as_os_str().to_owned();
    pub_path.push(".pub");
    russh::keys::load_public_key(PathBuf::from(pub_path))
        .ok()
        .or_else(|| russh::keys::load_secret_key(path, None).ok().map(|k| k.public_key().clone()))
}

// ---------------------------------------------------------------------------------------------
// The tunnel

/// Every hop's session, the last one forwarding.
struct Chain(Vec<Session>);

impl Chain {
    fn last(&self) -> &Session {
        self.0.last().expect("a chain has at least one hop")
    }

    fn is_closed(&self) -> bool {
        self.0.iter().any(|s| s.is_closed())
    }

    async fn disconnect(&self) {
        for s in self.0.iter().rev() {
            let _ = s.disconnect(russh::Disconnect::ByApplication, "", "en").await;
        }
    }
}

async fn connect_chain(spec: &TunnelSpec) -> Result<Chain, SshError> {
    let mut sessions: Vec<Session> = Vec::with_capacity(spec.hops.len());
    for hop in &spec.hops {
        let s = connect_hop(hop, sessions.last(), &spec.known_hosts).await?;
        sessions.push(s);
    }
    Ok(Chain(sessions))
}

struct Shared {
    spec: TunnelSpec,
    chain: tokio::sync::Mutex<Option<Arc<Chain>>>,
    closed: AtomicBool,
    /// Why the last forwarded connection could not be opened (the client only sees its socket
    /// close).
    last_error: Mutex<Option<SshError>>,
}

impl Shared {
    /// The live session, reconnected if it dropped.
    async fn chain(&self) -> Result<Arc<Chain>, SshError> {
        let mut g = self.chain.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(SshError::Forward { target: self.spec.target.shown(), message: "tunnel closed".into() });
        }
        if let Some(c) = g.as_ref()
            && !c.is_closed()
        {
            return Ok(c.clone());
        }
        if g.take().is_some() {
            log::info!("ssh: session to {} dropped, reconnecting", self.spec.hops.last().map(|h| h.host.as_str()).unwrap_or(""));
        }
        let c = Arc::new(connect_chain(&self.spec).await?);
        *g = Some(c.clone());
        Ok(c)
    }

    async fn open_channel(&self, peer: SocketAddr) -> Result<ChannelStream<Msg>, SshError> {
        let target = self.spec.target.shown();
        let mut last = None;
        // A session that died without noticing yet gets one reconnect.
        for _ in 0..2 {
            let chain = self.chain().await?;
            let r = match &self.spec.target {
                Target::Tcp(h, p) => chain.last().channel_open_direct_tcpip(h.clone(), *p as u32, peer.ip().to_string(), peer.port() as u32).await,
                Target::Unix(path) => chain.last().channel_open_direct_streamlocal(path.clone()).await,
            };
            match r {
                Ok(ch) => return Ok(ch.into_stream()),
                Err(russh::Error::ChannelOpenFailure(f)) => return Err(SshError::Forward { target, message: channel_failure(&f) }),
                Err(e) => {
                    last = Some(russh_message(&e));
                    let mut g = self.chain.lock().await;
                    if g.as_ref().is_some_and(|c| Arc::ptr_eq(c, &chain)) {
                        *g = None;
                    }
                }
            }
        }
        Err(SshError::Forward { target, message: last.unwrap_or_default() })
    }
}

/// An open tunnel: `local_addr()` reaches the database.
pub struct Tunnel {
    local: SocketAddr,
    shared: Arc<Shared>,
    accept: tokio::task::AbortHandle,
    rt: tokio::runtime::Handle,
}

impl Tunnel {
    /// Connects the SSH session (so its errors show now) and starts listening. Must run on a
    /// tokio runtime.
    pub async fn open(spec: TunnelSpec) -> Result<Arc<Tunnel>, SshError> {
        let target = spec.target.shown();
        let shared = Arc::new(Shared { spec, chain: tokio::sync::Mutex::new(None), closed: AtomicBool::new(false), last_error: Mutex::new(None) });
        shared.chain().await?;
        let io = |e: std::io::Error| SshError::Forward { target: target.clone(), message: e.to_string() };
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(io)?;
        let local = listener.local_addr().map_err(io)?;
        let s = shared.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((mut sock, peer)) = listener.accept().await else { break };
                let s = s.clone();
                tokio::spawn(async move {
                    let _ = sock.set_nodelay(true);
                    match s.open_channel(peer).await {
                        Ok(mut ch) => {
                            let _ = tokio::io::copy_bidirectional(&mut sock, &mut ch).await;
                        }
                        Err(e) => {
                            log::warn!("{e}");
                            *s.last_error.lock() = Some(e);
                        }
                    }
                });
            }
        })
        .abort_handle();
        log::info!("ssh: tunnel {local} → {target}");
        Ok(Arc::new(Tunnel { local, shared, accept, rt: tokio::runtime::Handle::current() }))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Reconnects the SSH session if it dropped, with the reason when it cannot.
    pub async fn ensure(&self) -> Result<(), SshError> {
        *self.shared.last_error.lock() = None;
        self.shared.chain().await.map(|_| ())
    }

    /// Why a connection through the tunnel failed, if the tunnel is what failed it (the SSH
    /// server could not reach the database, say).
    pub fn take_error(&self) -> Option<SshError> {
        self.shared.last_error.lock().take()
    }

    /// Stops listening and ends the SSH session (every connection through it closes).
    pub fn close(&self) {
        if self.shared.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.accept.abort();
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            if let Some(c) = shared.chain.lock().await.take() {
                c.disconnect().await;
            }
        });
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_from_settings_and_ssh_config() {
        let cfg = |alias: &str| -> HostConfig {
            let text = "Host prod-db\n  HostName 10.1.2.3\n  User deploy\n  Port 2222\n  ProxyJump ops@gate:2200\n  IdentityFile ~/.ssh/prod\n  IdentitiesOnly yes\nHost gate\n  HostName gate.example\n  User ignored\n";
            config::resolve(text, alias, Path::new("/home/me/.ssh"))
        };
        let home = Path::new("/home/me");
        let s = SshSettings { host: "prod-db".into(), auth: SshAuth::Agent, ..Default::default() };
        let t = TunnelSpec::resolve_with(&s, None, Target::Tcp("db.internal".into(), 5432), vec![], home, "me", cfg);
        assert_eq!(t.hops.len(), 2);
        assert_eq!((t.hops[0].host.as_str(), t.hops[0].port, t.hops[0].user.as_str()), ("gate.example", 2200, "ops"));
        assert_eq!((t.hops[1].host.as_str(), t.hops[1].port, t.hops[1].user.as_str()), ("10.1.2.3", 2222, "deploy"));
        assert_eq!(t.hops[1].auth, HopAuth::Agent { keys: vec![PathBuf::from("/home/me/.ssh/prod")], only: true });
        assert_eq!(t.hops[0].auth, HopAuth::Agent { keys: default_keys(home), only: false });

        // The profile's own fields win over the alias'.
        let s = SshSettings { host: "prod-db".into(), port: Some(22), user: "bruno".into(), auth: SshAuth::Key, key_file: "~/.ssh/other".into() };
        let t = TunnelSpec::resolve_with(&s, Some("pp".into()), Target::Tcp("db".into(), 5432), vec![], home, "me", cfg);
        let last = t.hops.last().unwrap();
        assert_eq!((last.port, last.user.as_str()), (22, "bruno"));
        assert_eq!(last.auth, HopAuth::Key { path: PathBuf::from("/home/me/.ssh/other"), passphrase: Some("pp".into()) });

        // Not an alias: the name as written, port 22, the local user.
        let s = SshSettings { host: "plain.example".into(), auth: SshAuth::Password, ..Default::default() };
        let t = TunnelSpec::resolve_with(&s, Some("pw".into()), Target::Tcp("db".into(), 5432), vec![], home, "me", cfg);
        assert_eq!(t.hops, vec![Hop { host: "plain.example".into(), port: 22, user: "me".into(), auth: HopAuth::Password(Some("pw".into())) }]);

        let mut p = ConnectionProfile::new("x");
        (p.host, p.port) = ("/var/run/postgresql".into(), 5433);
        assert_eq!(Target::of(&p), Target::Unix("/var/run/postgresql/.s.PGSQL.5433".into()));
    }
}
