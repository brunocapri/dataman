//! The editor session's own socket (and the ReadyForQuery sniffer on it) over TLS and through
//! an SSH tunnel, for every sslmode: commit and rollback of a typed BEGIN, a failed transaction, the
//! CancelRequest on a socket of the session's own, and a read-only connection. Each test skips
//! unless its servers answer:
//!
//! ```sh
//! # A CA and a server certificate for verify-full (SAN localhost and diskman-ssh-pg, no IP).
//! T=/tmp/diskman-tls; mkdir -p $T/certs $T/keys
//! cd $T/certs && openssl genrsa -out ca.key 2048 && openssl req -new -key ca.key -subj /CN=ca -out ca.csr
//! printf 'basicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign\n' > ca.ext
//! openssl x509 -req -in ca.csr -signkey ca.key -days 30 -extfile ca.ext -out ca.crt
//! openssl genrsa -out server.key 2048 && openssl req -new -key server.key -subj /CN=pg -out server.csr
//! printf 'subjectAltName=DNS:localhost,DNS:diskman-ssh-pg\n' > server.ext
//! openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.crt
//! docker network create diskman-ssh-net
//! docker run -d --name diskman-ssh-pg --network diskman-ssh-net -p 127.0.0.1:54340:5432 \
//!   -e POSTGRES_PASSWORD=diskman -v $T/certs:/certs:ro --entrypoint bash postgres:18 -c \
//!   'mkdir -p /etc/ssl/s && install -o postgres -m 600 /certs/server.key /etc/ssl/s/ && install -m 644 /certs/server.crt /etc/ssl/s/ &&
//!    exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/etc/ssl/s/server.crt -c ssl_key_file=/etc/ssl/s/server.key'
//! # Debian's self-signed snakeoil certificate, for the verify-full refusal.
//! docker run -d --name diskman-ssh-snakeoil -p 127.0.0.1:54341:5432 -e POSTGRES_PASSWORD=diskman postgres:18 \
//!   -c ssl=on -c ssl_cert_file=/etc/ssl/certs/ssl-cert-snakeoil.pem -c ssl_key_file=/etc/ssl/private/ssl-cert-snakeoil.key
//! # The sshd of tests/live_ssh.rs (diskman-sshd image, 127.0.0.1:2299, keys in $T/keys).
//! DISKMAN_TLS_PG=127.0.0.1:54340 DISKMAN_TLS_CA=$T/certs/ca.crt DISKMAN_TLS_SNAKEOIL=127.0.0.1:54341 \
//!   DISKMAN_SSH_KEYS=$T/keys cargo test --release -p diskman-core --test live_tls
//! ```
//!
//! Direct tests connect to `localhost` (in the certificate); through the tunnel the database is
//! `diskman-ssh-pg:5432`, reachable only from the SSH server's network. `DISKMAN_TLS_CA` becomes
//! `SSL_CERT_FILE`, the only trust store verify-full reads then.

use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use diskman_core::config::{ConnectionProfile, Dirs, SshAuth, SshSettings, SslMode};
use diskman_core::result::Phase;
use diskman_core::{Connection, Core, MemorySecrets, ResultSet, Session, SshError, TxEnd, TxStatus};

struct Setup {
    /// The TLS server, seen directly.
    direct: Option<(String, u16)>,
    /// The SSH server and its keys.
    ssh: Option<((String, u16), PathBuf)>,
    /// A CA that signed the server's certificate (verify-full).
    ca: bool,
    snakeoil: Option<(String, u16)>,
}

fn answers(addr: &str) -> Option<(String, u16)> {
    let (h, p) = addr.rsplit_once(':')?;
    let port: u16 = p.parse().ok()?;
    let sa = format!("{}:{port}", if h == "localhost" { "127.0.0.1" } else { h });
    if TcpStream::connect_timeout(&sa.parse().ok()?, Duration::from_secs(1)).is_err() {
        eprintln!("skipping: nothing at {addr}");
        return None;
    }
    Some((h.to_string(), port))
}

fn setup() -> &'static Setup {
    static S: OnceLock<Setup> = OnceLock::new();
    S.get_or_init(|| {
        let direct = std::env::var("DISKMAN_TLS_PG").ok().and_then(|a| answers(&a)).map(|(_, p)| ("localhost".to_string(), p));
        let ssh = std::env::var_os("DISKMAN_SSH_KEYS").and_then(|k| {
            let addr = std::env::var("DISKMAN_SSH").unwrap_or_else(|_| "127.0.0.1:2299".into());
            answers(&addr).map(|a| (a, PathBuf::from(k)))
        });
        let snakeoil = std::env::var("DISKMAN_TLS_SNAKEOIL").ok().and_then(|a| answers(&a));
        let ca = std::env::var_os("DISKMAN_TLS_CA");
        // SAFETY: set once, before any test connects (every test calls `setup` first).
        unsafe {
            if let Some(ca) = &ca {
                std::env::set_var("SSL_CERT_FILE", ca);
                std::env::remove_var("SSL_CERT_DIR");
            }
            // The user's ~/.ssh and agent are never read.
            let home = std::env::temp_dir().join(format!("diskman-tls-home-{}", std::process::id()));
            let _ = std::fs::create_dir_all(home.join(".ssh"));
            std::env::set_var("HOME", &home);
            std::env::remove_var("SSH_AUTH_SOCK");
        }
        Setup { direct, ssh, ca: ca.is_some(), snakeoil }
    })
}

struct Env {
    core: Core,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(Dirs::at(dir.path().join("diskman")), Arc::new(MemorySecrets::default())).unwrap();
    Env { core, _dir: dir }
}

#[derive(Clone, Copy, Debug)]
enum Route {
    Direct,
    Ssh,
}

/// The profile for `route` and `mode`, or `None` (skip).
fn profile(route: Route, mode: SslMode) -> Option<ConnectionProfile> {
    let s = setup();
    if mode == SslMode::VerifyFull && !s.ca {
        eprintln!("skipping verify-full: no DISKMAN_TLS_CA");
        return None;
    }
    let base = |host: String, port: u16| ConnectionProfile {
        host,
        port,
        user: "postgres".into(),
        database: "postgres".into(),
        sslmode: mode,
        ..ConnectionProfile::new(&format!("tls-{route:?}-{}", mode.label()))
    };
    match route {
        Route::Direct => s.direct.clone().map(|(h, p)| base(h, p)),
        Route::Ssh => {
            let ((h, p), keys) = s.ssh.clone()?;
            let db = std::env::var("DISKMAN_SSH_DB").unwrap_or_else(|_| "diskman-ssh-pg:5432".into());
            let (dh, dp) = db.rsplit_once(':').unwrap();
            let ssh = SshSettings { host: h, port: Some(p), user: "diskman".into(), auth: SshAuth::Key, key_file: keys.join("k_ed").display().to_string() };
            Some(ConnectionProfile { ssh: Some(ssh), ..base(dh.into(), dp.parse().unwrap()) })
        }
    }
}

fn ssh_error(e: &anyhow::Error) -> Option<SshError> {
    e.chain().find_map(|c| c.downcast_ref::<SshError>()).cloned()
}

/// Connects, trusting the SSH server's host key on first contact.
fn connect(e: &Env, p: &ConnectionProfile) -> anyhow::Result<Arc<Connection>> {
    for _ in 0..3 {
        match e.core.block_on(e.core.connect(p.clone(), Some("diskman".into()))) {
            Err(err) => match ssh_error(&err) {
                Some(SshError::UnknownHostKey(prompt)) => e.core.block_on(e.core.trust_host_key(prompt)).unwrap(),
                _ => return Err(err),
            },
            ok => return ok,
        }
    }
    anyhow::bail!("host key prompts")
}

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") });
    rs
}

fn ok(e: &Env, s: &Arc<Session>, sql: &str) -> String {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    if rs.rows() == 0 {
        return String::new();
    }
    rs.snapshot().text(0, 0).map(|s| s.into_owned()).unwrap_or_default()
}

fn end(e: &Env, s: &Arc<Session>, commit: bool) -> TxEnd {
    e.core.block_on(s.end_transaction(commit)).expect("end transaction")
}

/// Every transaction scenario on one route and sslmode.
fn scenarios(route: Route, mode: SslMode) {
    let Some(p) = profile(route, mode) else { return };
    let e = env();
    let c = connect(&e, &p).unwrap_or_else(|err| panic!("{route:?} {}: {err:#}", mode.label()));
    let tls = mode != SslMode::Disable;
    assert_eq!(c.tls_in_use(), tls, "metadata connection");
    let table = format!("public.tls_{}_{}", format!("{route:?}").to_lowercase(), mode.label().replace('-', "_"));
    let admin = c.session(None);
    ok(&e, &admin, &format!("DROP TABLE IF EXISTS {table}"));
    ok(&e, &admin, &format!("CREATE TABLE {table} (id int)"));

    // The editor session's own socket negotiated what the sslmode asks for.
    let s = c.session(None);
    assert_eq!(ok(&e, &s, "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()"), tls.to_string(), "session socket");

    // A typed BEGIN: the status off ReadyForQuery, COMMIT and ROLLBACK.
    ok(&e, &s, "BEGIN");
    ok(&e, &s, &format!("INSERT INTO {table} VALUES (1)"));
    assert_eq!((s.tx().status, s.tx().statements), (TxStatus::Open, 1));
    assert_eq!(ok(&e, &admin, &format!("SELECT count(*) FROM {table}")), "0", "not visible before COMMIT");
    assert_eq!(end(&e, &s, true), TxEnd::Committed);
    assert_eq!(s.tx().status, TxStatus::Idle);
    ok(&e, &s, "BEGIN");
    ok(&e, &s, &format!("DELETE FROM {table}"));
    assert_eq!(end(&e, &s, false), TxEnd::RolledBack);
    assert_eq!(ok(&e, &admin, &format!("SELECT count(*) FROM {table}")), "1");

    // A failed transaction: refused until it ends; its COMMIT is a rollback.
    ok(&e, &s, "BEGIN");
    ok(&e, &s, "SELECT 1");
    assert_eq!(run(&e, &s, "SELECT 1/0").phase(), Phase::Failed);
    assert_eq!(s.tx().status, TxStatus::Failed);
    let rs = run(&e, &s, "SELECT 2");
    assert_eq!(rs.status().error.and_then(|e| e.code).as_deref(), Some("25P02"));
    assert_eq!(end(&e, &s, true), TxEnd::RolledBack);
    assert_eq!(s.tx().status, TxStatus::Idle);

    // The CancelRequest on a socket of the session's own (TLS and tunnel included), inside a
    // transaction: the statement stops, the transaction fails, a rollback recovers it.
    ok(&e, &s, "BEGIN");
    let rs = s.execute("SELECT pg_sleep(20)");
    std::thread::sleep(Duration::from_millis(400));
    let t = Instant::now();
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("cancel timed out") });
    assert_eq!(rs.phase(), Phase::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(3), "cancel took {:?}", t.elapsed());
    assert_eq!(s.tx().status, TxStatus::Failed);
    assert_eq!(end(&e, &s, false), TxEnd::RolledBack);
    assert_eq!(ok(&e, &s, "SELECT 'after'"), "after");
    assert_eq!(s.tx().status, TxStatus::Idle);

    // Read-only: the server refuses the write on the session's socket.
    let ro = ConnectionProfile { read_only: true, ..p.clone() };
    let rc = connect(&e, &ro).unwrap();
    let rs_ = rc.session(None);
    let rs = run(&e, &rs_, &format!("INSERT INTO {table} VALUES (2)"));
    assert_eq!(rs.status().error.and_then(|e| e.code).as_deref(), Some("25006"));
    assert_eq!(ok(&e, &rs_, &format!("SELECT count(*) FROM {table}")), "1");
    drop(rs_);
    rc.close();

    ok(&e, &admin, &format!("DROP TABLE {table}"));
    c.close();
}

macro_rules! matrix {
    ($($name:ident: $route:ident, $mode:ident;)*) => {$(
        #[test]
        fn $name() {
            scenarios(Route::$route, SslMode::$mode);
        }
    )*};
}

matrix! {
    direct_disable: Direct, Disable;
    direct_prefer: Direct, Prefer;
    direct_require: Direct, Require;
    direct_verify_full: Direct, VerifyFull;
    ssh_disable: Ssh, Disable;
    ssh_prefer: Ssh, Prefer;
    ssh_require: Ssh, Require;
    ssh_verify_full: Ssh, VerifyFull;
}

/// verify-full checks the name: `127.0.0.1` is not in the certificate, for the metadata
/// connection and the session alike.
#[test]
fn verify_full_refuses_a_name_not_in_the_certificate() {
    let Some(mut p) = profile(Route::Direct, SslMode::VerifyFull) else { return };
    p.host = "127.0.0.1".into();
    let e = env();
    let err = format!("{:#}", connect(&e, &p).err().expect("must fail"));
    assert!(err.to_lowercase().contains("certificate"), "{err}");
    eprintln!("verify-full, wrong name: {err}");
}

/// Debian's snakeoil certificate is self-signed for the image's build host: require works (no
/// verification), verify-full refuses it.
#[test]
fn snakeoil_certificate() {
    let s = setup();
    let Some((h, port)) = s.snakeoil.clone() else { return };
    let e = env();
    let p = ConnectionProfile {
        host: h,
        port,
        user: "postgres".into(),
        database: "postgres".into(),
        sslmode: SslMode::Require,
        ..ConnectionProfile::new("snakeoil")
    };
    let c = connect(&e, &p).unwrap();
    let session = c.session(None);
    assert_eq!(ok(&e, &session, "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()"), "true");
    ok(&e, &session, "BEGIN");
    assert_eq!(session.tx().status, TxStatus::Open);
    assert_eq!(end(&e, &session, false), TxEnd::RolledBack);
    let err = format!("{:#}", connect(&e, &ConnectionProfile { sslmode: SslMode::VerifyFull, ..p }).err().expect("must fail"));
    assert!(err.to_lowercase().contains("certificate"), "{err}");
    eprintln!("verify-full, snakeoil: {err}");
}
