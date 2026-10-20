//! HOME points at a scratch directory with its own `~/.ssh/config`, so the user's files are
//! never read or written.

use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use dataman_core::config::{ConnectionProfile, Dirs, SshAuth, SshSettings, SslMode};
use dataman_core::result::Phase;
use dataman_core::{Connection, Core, HostKeyPrompt, MemorySecrets, SshError};

struct Setup {
    keys: PathBuf,
    ssh: (String, u16),
    home: PathBuf,
}

fn setup() -> Option<&'static Setup> {
    static S: OnceLock<Option<Setup>> = OnceLock::new();
    S.get_or_init(|| {
        let keys = PathBuf::from(std::env::var_os("DATAMAN_SSH_KEYS")?);
        let addr = std::env::var("DATAMAN_SSH").unwrap_or_else(|_| "127.0.0.1:2299".into());
        let (h, p) = addr.rsplit_once(':')?;
        let ssh = (h.to_string(), p.parse().ok()?);
        if TcpStream::connect_timeout(&addr.parse().ok()?, Duration::from_secs(1)).is_err() {
            eprintln!("skipping: no SSH server at {addr}");
            return None;
        }
        let home = std::env::temp_dir().join(format!("dataman-ssh-home-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".ssh")).ok()?;
        let config = format!(
            "Host dataman-test\n  HostName {h}\n  Port {p}\n  User dataman\n  IdentityFile {k}\n\nHost dataman-jump\n  HostName 127.0.0.1\n  Port 2222\n  User dataman\n  ProxyJump dataman-test\n  IdentityFile {k}\n",
            k = keys.join("k_ed").display()
        );
        std::fs::write(home.join(".ssh/config"), config).ok()?;
        // SAFETY: set once, before any test reads the environment.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("SSH_AUTH_SOCK");
        }
        Some(Setup { keys, ssh, home })
    })
    .as_ref()
}

struct Env {
    core: Core,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(Dirs::at(dir.path().join("dataman")), Arc::new(MemorySecrets::default())).unwrap();
    Env { core, _dir: dir }
}

fn sslmode() -> SslMode {
    let m = std::env::var("DATAMAN_SSH_SSLMODE").unwrap_or_else(|_| "require".into());
    SslMode::ALL.into_iter().find(|s| s.label() == m).unwrap_or_else(|| panic!("DATAMAN_SSH_SSLMODE={m}"))
}

fn db_profile(ssh: SshSettings) -> ConnectionProfile {
    let db = std::env::var("DATAMAN_SSH_DB").unwrap_or_else(|_| "dataman-ssh-pg:5432".into());
    let (host, port) = db.rsplit_once(':').unwrap();
    ConnectionProfile {
        host: host.into(),
        port: port.parse().unwrap(),
        user: "postgres".into(),
        database: "postgres".into(),
        sslmode: sslmode(),
        ssh: Some(ssh),
        ..ConnectionProfile::new("tunnel")
    }
}

fn ssh_error(e: &anyhow::Error) -> Option<SshError> {
    e.chain().find_map(|c| c.downcast_ref::<SshError>()).cloned()
}

fn connect(e: &Env, p: &ConnectionProfile) -> anyhow::Result<Arc<Connection>> {
    e.core.block_on(e.core.connect(p.clone(), Some("dataman".into())))
}

fn fails(e: &Env, p: &ConnectionProfile) -> anyhow::Error {
    connect(e, p).err().expect("the connection should have failed")
}

fn connect_trusting(e: &Env, p: &ConnectionProfile) -> Arc<Connection> {
    for _ in 0..4 {
        match connect(e, p) {
            Ok(c) => return c,
            Err(err) => match ssh_error(&err) {
                Some(SshError::UnknownHostKey(prompt)) => trust(e, prompt),
                _ => panic!("{err:#}"),
            },
        }
    }
    panic!("too many host-key prompts")
}

fn trust(e: &Env, prompt: HostKeyPrompt) {
    assert!(prompt.fingerprint.starts_with("SHA256:"), "{prompt:?}");
    e.core.block_on(e.core.trust_host_key(prompt)).unwrap();
}

fn one(e: &Env, c: &Arc<Connection>, sql: &str) -> String {
    let rs = c.session(None).execute(sql);
    let phase = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(20), rs.wait_finished()).await.expect("timeout") });
    assert_eq!(phase, Phase::Done, "{sql}: {:?}", rs.status().error);
    rs.snapshot().text(0, 0).map(|s| s.to_string()).unwrap_or_default()
}

fn key_auth(s: &Setup, key: &str) -> SshSettings {
    SshSettings { host: s.ssh.0.clone(), port: Some(s.ssh.1), user: "dataman".into(), auth: SshAuth::Key, key_file: s.keys.join(key).display().to_string() }
}

/// Killing the server's sessions (below) ends every test's tunnel: that test runs alone, the
/// others share.
static SSHD: RwLock<()> = RwLock::new(());

fn shared() -> RwLockReadGuard<'static, ()> {
    SSHD.read().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn tunnel_end_to_end() {
    let Some(s) = setup() else { return };
    let _g = shared();
    let e = env();
    let p = db_profile(key_auth(s, "k_ed"));

    let err = fails(&e, &p);
    let Some(SshError::UnknownHostKey(prompt)) = ssh_error(&err) else { panic!("{err:#}") };
    assert_eq!((prompt.hostname.as_str(), prompt.port), (s.ssh.0.as_str(), s.ssh.1));
    trust(&e, prompt);
    let c = connect(&e, &p).unwrap();
    assert!(c.tunneled());
    let tls = sslmode() != SslMode::Disable;
    assert_eq!(c.tls_in_use(), tls);
    assert_eq!(one(&e, &c, "SELECT current_setting('ssl')"), "on");
    assert_eq!(one(&e, &c, "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()"), tls.to_string());

    let session = c.session(None);
    let rs = session.execute("SELECT pg_sleep(30)");
    std::thread::sleep(Duration::from_millis(400));
    let t = Instant::now();
    session.cancel();
    let phase = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("cancel timed out") });
    assert_eq!(phase, Phase::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());

    let sessions: Vec<_> = (0..4).map(|_| c.session(None)).collect();
    let results: Vec<_> = sessions.iter().map(|s| s.execute("SELECT 1")).collect();
    for rs in &results {
        assert_eq!(e.core.block_on(rs.wait_finished()), Phase::Done);
    }

    c.close();
    std::thread::sleep(Duration::from_millis(200));
    let rs = c.session(None).execute("SELECT 1");
    assert_eq!(e.core.block_on(rs.wait_finished()), Phase::Failed);

    let (info, _) = e.core.block_on(e.core.test_connection(p.clone(), Some("dataman".into()), None)).unwrap();
    assert!(info.version.starts_with("PostgreSQL"));
}

#[test]
fn tunnel_reconnects_after_the_session_drops() {
    let Some(s) = setup() else { return };
    let Ok(container) = std::env::var("DATAMAN_SSH_CONTAINER") else { return };
    let _alone = SSHD.write().unwrap_or_else(|e| e.into_inner());
    let e = env();
    let c = connect_trusting(&e, &db_profile(key_auth(s, "k_ed")));
    assert_eq!(one(&e, &c, "SELECT 1"), "1");
    let killed = std::process::Command::new("docker")
        // `[s]` keeps the pattern from matching this very shell's command line.
        .args(["exec", &container, "sh", "-c", "pkill -f '[s]shd-session: dataman' || pkill -f '[s]shd: dataman'"])
        .status()
        .unwrap();
    assert!(killed.success());
    std::thread::sleep(Duration::from_millis(800));
    let t = Instant::now();
    assert_eq!(one(&e, &c, "SELECT 'again'"), "again");
    eprintln!("reconnected in {:?}", t.elapsed());
}

#[test]
fn auth_methods_and_errors() {
    let Some(s) = setup() else { return };
    let _g = shared();
    let e = env();
    connect_trusting(&e, &db_profile(key_auth(s, "k_ed")));

    let rsa = db_profile(key_auth(s, "k_rsa"));
    let err = fails(&e, &rsa);
    assert!(matches!(ssh_error(&err), Some(SshError::SecretNeeded { what: "key passphrase", .. })), "{err:#}");
    let (info, _) = e.core.block_on(e.core.test_connection(rsa.clone(), Some("dataman".into()), Some("s3cret pp".into()))).unwrap();
    assert!(!info.version.is_empty());
    let err = e.core.block_on(e.core.test_connection(rsa.clone(), Some("dataman".into()), Some("wrong".into()))).unwrap_err();
    assert!(matches!(ssh_error(&err), Some(SshError::Auth { .. })), "{err:#}");

    let pw = db_profile(SshSettings { auth: SshAuth::Password, key_file: String::new(), ..key_auth(s, "") });
    let err = fails(&e, &pw);
    assert!(matches!(ssh_error(&err), Some(SshError::SecretNeeded { what: "password", .. })), "{err:#}");
    let err = e.core.block_on(e.core.test_connection(pw.clone(), Some("dataman".into()), Some("nope".into()))).unwrap_err();
    let msg = format!("{err:#}");
    assert!(matches!(ssh_error(&err), Some(SshError::Auth { .. })), "{msg}");
    assert!(msg.starts_with(&format!("SSH dataman@{}:{} · authentication failed", s.ssh.0, s.ssh.1)), "{msg}");
    e.core.block_on(e.core.test_connection(pw.clone(), Some("dataman".into()), Some("pw-dataman".into()))).unwrap();
    e.core.save_connection(pw.clone(), None, Some("pw-dataman")).unwrap();
    assert!(connect(&e, &pw).is_ok());

    let ed = db_profile(key_auth(s, "k_ed"));
    let err = e.core.block_on(e.core.connect(ed, Some("bad".into()))).map(|_| ()).unwrap_err();
    assert!(ssh_error(&err).is_none() && format!("{err:#}").contains("password"), "{err:#}");

    let mut nowhere = db_profile(key_auth(s, "k_ed"));
    nowhere.host = "no-such-host.invalid".into();
    let err = fails(&e, &nowhere);
    assert!(matches!(ssh_error(&err), Some(SshError::Forward { .. })), "{err:#}");
    eprintln!("unreachable database: {err:#}");

    let mut down = key_auth(s, "k_ed");
    down.port = Some(1);
    let err = fails(&e, &db_profile(down));
    assert!(matches!(ssh_error(&err), Some(SshError::Connect { .. })), "{err:#}");
}

#[test]
fn ssh_config_alias_proxyjump_and_agent() {
    let Some(s) = setup() else { return };
    let _g = shared();
    let e = env();
    let alias = db_profile(SshSettings { host: "dataman-test".into(), auth: SshAuth::Key, ..Default::default() });
    let c = connect_trusting(&e, &alias);
    assert_eq!(one(&e, &c, "SELECT 1"), "1");
    c.close();

    let jump = db_profile(SshSettings { host: "dataman-jump".into(), auth: SshAuth::Key, ..Default::default() });
    let c = connect_trusting(&e, &jump);
    assert_eq!(one(&e, &c, "SELECT 'jumped'"), "jumped");
    c.close();

    let sock = s.home.join("agent.sock");
    let _ = std::fs::remove_file(&sock);
    let out = std::process::Command::new("ssh-agent").arg("-a").arg(&sock).output().unwrap();
    let pid = String::from_utf8_lossy(&out.stdout).split("SSH_AGENT_PID=").nth(1).and_then(|r| r.split(';').next().map(str::to_string));
    let added = std::process::Command::new("ssh-add").arg(s.keys.join("k_ed")).env("SSH_AUTH_SOCK", &sock).output().unwrap();
    assert!(added.status.success(), "{added:?}");
    // SAFETY: only this test uses the agent.
    unsafe { std::env::set_var("SSH_AUTH_SOCK", &sock) };
    let agent = db_profile(SshSettings { host: s.ssh.0.clone(), port: Some(s.ssh.1), user: "dataman".into(), auth: SshAuth::Agent, key_file: String::new() });
    let r = connect(&e, &agent);
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill").arg(pid).status();
    }
    let c = r.unwrap();
    assert_eq!(one(&e, &c, "SELECT 'agent'"), "agent");
}

#[test]
fn changed_host_key_is_refused() {
    let Some(s) = setup() else { return };
    let _g = shared();
    let e = env();
    std::fs::create_dir_all(&e.core.dirs.config).unwrap();
    let name = if s.ssh.1 == 22 { s.ssh.0.clone() } else { format!("[{}]:{}", s.ssh.0, s.ssh.1) };
    std::fs::write(
        e.core.dirs.known_hosts_file(),
        format!("{name} ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ\n"),
    )
    .unwrap();
    let err = fails(&e, &db_profile(key_auth(s, "k_ed")));
    match ssh_error(&err) {
        Some(SshError::HostKeyChanged { line, .. }) => assert_eq!(line, 1),
        _ => panic!("{err:#}"),
    }
    assert!(format!("{err:#}").contains("host key changed"), "{err:#}");
}

#[test]
fn unix_socket_host() {
    let (pg_host, pg_port) = ("127.0.0.1", 55432u16);
    if TcpStream::connect_timeout(&format!("{pg_host}:{pg_port}").parse().unwrap(), Duration::from_secs(1)).is_err() {
        eprintln!("skipping: no PostgreSQL at {pg_host}:{pg_port}");
        return;
    }
    let e = env();
    let dir = tempfile::Builder::new().prefix("pgs").tempdir_in("/tmp").unwrap();
    let path = dir.path().join(format!(".s.PGSQL.{pg_port}"));
    let listener = e.core.block_on(async { tokio::net::UnixListener::bind(&path) }).unwrap();
    e.core.handle().spawn(async move {
        while let Ok((mut a, _)) = listener.accept().await {
            tokio::spawn(async move {
                if let Ok(mut b) = tokio::net::TcpStream::connect((pg_host, pg_port)).await {
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
            });
        }
    });
    let p = ConnectionProfile {
        host: dir.path().display().to_string(),
        port: pg_port,
        user: "postgres".into(),
        database: "dataman".into(),
        sslmode: SslMode::Require,
        ..ConnectionProfile::new("socket")
    };
    let pw = std::env::var("DATAMAN_PG_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let c = e.core.block_on(e.core.connect(p.clone(), Some(pw))).unwrap();
    assert!(!c.tls_in_use());
    assert_eq!(one(&e, &c, "SELECT 'socket'"), "socket");
    let session = c.session(None);
    let rs = session.execute("SELECT pg_sleep(30)");
    std::thread::sleep(Duration::from_millis(300));
    session.cancel();
    assert_eq!(e.core.block_on(rs.wait_finished()), Phase::Cancelled);
    assert!(p.url().contains("@%2F"), "{}", p.url());
}
