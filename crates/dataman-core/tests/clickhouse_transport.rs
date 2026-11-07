//! ClickHouse's HTTP path against fake servers on this machine: what reaches the wire, and what an
//! answer of any size costs. No database needed, so these always run. `0.0.0.0` is the way to a
//! target that is no loopback by name and still reaches this machine (not on Windows).
#![cfg(unix)]

mod common;
#[path = "common/fake_clickhouse.rs"]
mod fake;

use std::sync::OnceLock;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop, Truncation};
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::connstr;
use fake::{Answer, Reply, Wire, listen, plain_answers, plain_server, serve};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const USER: &str = "dbuser";
const PASSWORD: &str = "hunter22-never-in-clear";
const MIB: u64 = 1024 * 1024;
/// What may be read for a statement stopped at the row cap: the cap, what the reader takes ahead of
/// it (its in-flight bytes), and what the sockets hold.
const PAST_A_LONG_ROW: u64 = (128 + 64 + 64) * MIB;

struct Certs {
    trusted: rustls::ServerConfig,
    unknown: rustls::ServerConfig,
    _dir: tempfile::TempDir,
}

fn server_config(cert: Vec<u8>, key: Vec<u8>) -> rustls::ServerConfig {
    rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert)], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
        .unwrap()
}

/// A CA this process trusts (`SSL_CERT_FILE`, the configured way) and one no one here has heard of,
/// each with a certificate for every name the tests connect by.
fn certs() -> &'static Certs {
    static C: OnceLock<Certs> = OnceLock::new();
    C.get_or_init(|| {
        let leaf = |issuer: &rcgen::Issuer<'_, rcgen::KeyPair>| {
            let key = rcgen::KeyPair::generate().unwrap();
            let params = rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string(), "0.0.0.0".to_string()]).unwrap();
            let cert = params.signed_by(&key, issuer).unwrap();
            server_config(cert.der().to_vec(), key.serialize_der())
        };
        let ca = || {
            let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            let key = rcgen::KeyPair::generate().unwrap();
            let pem = params.clone().self_signed(&key).unwrap().pem();
            (pem, rcgen::Issuer::new(params, key))
        };
        let (pem, trusted_ca) = ca();
        let (_, unknown_ca) = ca();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ca.pem");
        std::fs::write(&file, pem).unwrap();
        // SAFETY: set once, before any client is built (every test asks for `certs` first).
        unsafe {
            std::env::set_var("SSL_CERT_FILE", &file);
            std::env::remove_var("SSL_CERT_DIR");
        }
        Certs { trusted: leaf(&trusted_ca), unknown: leaf(&unknown_ca), _dir: dir }
    })
}

fn tls_server(config: &rustls::ServerConfig) -> (u16, Arc<Wire>) {
    let wire = Arc::new(Wire::default());
    let w = wire.clone();
    let config = Arc::new(config.clone());
    let answer: Answer = Arc::new(plain_answers);
    let port = listen(move |sock| {
        let Ok(conn) = rustls::ServerConnection::new(config.clone()) else { return };
        serve(rustls::StreamOwned::new(conn, sock), &w, &answer);
    });
    (port, wire)
}

fn profile(host: &str, port: u16, mode: SslMode) -> ConnectionProfile {
    ConnectionProfile { host: host.into(), port, user: USER.into(), database: String::new(), sslmode: mode, engine: Engine::ClickHouse, ..ConnectionProfile::new("fake") }
}

fn connect(e: &Env, p: ConnectionProfile) -> anyhow::Result<Arc<Connection>> {
    e.core.block_on(e.core.connect(p, Some(PASSWORD.to_string())))
}

fn secret_in_clear(wire: &Wire) -> bool {
    let basic = data_encoding::BASE64.encode(format!("{USER}:{PASSWORD}").as_bytes());
    wire.holds(basic.as_bytes()) || wire.holds(PASSWORD.as_bytes()) || wire.holds(b"uthorization")
}

const OFF_THIS_MACHINE: &str = "0.0.0.0";
const HERE: &str = "127.0.0.1";

fn refused(e: &Env, p: ConnectionProfile) -> String {
    format!("{:#}", connect(e, p).err().expect("must not connect"))
}

#[test]
fn prefer_never_sends_the_password_in_clear_off_this_machine() {
    certs();
    let e = env();
    let (port, wire) = plain_server(plain_answers);
    let err = refused(&e, profile(OFF_THIS_MACHINE, port, SslMode::Prefer));
    assert!(wire.got_any(), "it did try: only TLS's first words");
    assert!(!secret_in_clear(&wire), "the password arrived in clear");
    assert!(err.contains("Disable"), "says how to choose it: {err}");
}

#[test]
fn prefer_falls_back_on_this_machine() {
    certs();
    let e = env();
    for host in [HERE, "localhost"] {
        let (port, wire) = plain_server(plain_answers);
        let c = connect(&e, profile(host, port, SslMode::Prefer)).unwrap();
        assert!(!c.tls_in_use(), "{host}");
        assert!(secret_in_clear(&wire), "{host}: nobody on the path");
    }
}

#[test]
fn prefer_takes_tls_when_there_is_tls() {
    let e = env();
    let (port, wire) = tls_server(&certs().unknown);
    let c = connect(&e, profile(OFF_THIS_MACHINE, port, SslMode::Prefer)).unwrap();
    assert!(c.tls_in_use());
    assert!(secret_in_clear(&wire), "the decrypted side has it");
}

#[test]
fn require_encrypts_and_checks_nothing() {
    let e = env();
    let (port, _wire) = tls_server(&certs().unknown);
    assert!(connect(&e, profile(OFF_THIS_MACHINE, port, SslMode::Require)).unwrap().tls_in_use());

    let (port, wire) = plain_server(plain_answers);
    let err = refused(&e, profile(OFF_THIS_MACHINE, port, SslMode::Require));
    assert!(!secret_in_clear(&wire), "the password arrived in clear");
    assert!(err.contains("8443"), "{err}");
}

#[test]
fn verify_full_refuses_what_no_one_vouches_for() {
    let e = env();
    let (port, wire) = tls_server(&certs().unknown);
    let err = refused(&e, profile(OFF_THIS_MACHINE, port, SslMode::VerifyFull));
    assert!(err.to_lowercase().contains("certificate"), "{err}");
    assert!(!wire.got_any(), "no request after a refused handshake");

    let (port, wire) = plain_server(plain_answers);
    refused(&e, profile(OFF_THIS_MACHINE, port, SslMode::VerifyFull));
    assert!(!secret_in_clear(&wire), "the password arrived in clear");

    let (port, _wire) = tls_server(&certs().trusted);
    assert!(connect(&e, profile(OFF_THIS_MACHINE, port, SslMode::VerifyFull)).unwrap().tls_in_use(), "the configured CA vouches");
}

#[test]
fn disable_is_there_when_chosen() {
    certs();
    let e = env();
    let (port, wire) = plain_server(plain_answers);
    let c = connect(&e, profile(OFF_THIS_MACHINE, port, SslMode::Disable)).unwrap();
    assert!(!c.tls_in_use());
    assert!(secret_in_clear(&wire));
}

#[test]
fn a_string_that_states_no_mode_verifies() {
    let e = env();
    let parse = |port: u16| connstr::parse_profile(&format!("clickhouse://{USER}:{PASSWORD}@{OFF_THIS_MACHINE}:{port}/d")).unwrap();

    let (port, wire) = plain_server(plain_answers);
    let (p, pw) = parse(port);
    certs();
    assert!(e.core.block_on(e.core.connect(p, pw)).is_err());
    assert!(!secret_in_clear(&wire), "the password arrived in clear");

    let (port, wire) = tls_server(&certs().unknown);
    let (p, pw) = parse(port);
    assert!(e.core.block_on(e.core.connect(p, pw)).is_err());
    assert!(!wire.got_any(), "a certificate no one vouches for is refused by default");

    let (port, _) = tls_server(&certs().trusted);
    let (p, pw) = parse(port);
    assert!(e.core.block_on(e.core.connect(p, pw)).unwrap().tls_in_use());
}

fn finished(e: &Env, rs: Arc<dataman_core::ResultSet>) -> Arc<dataman_core::ResultSet> {
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(120), rs.wait_finished()).await.expect("timeout") });
    rs
}

/// The server's own check is in `live_clickhouse`.
#[test]
fn bound_values_and_the_password_stay_out_of_every_url() {
    let e = env();
    let (port, wire) = plain_server(plain_answers);
    let c = connect(&e, profile(HERE, port, SslMode::Disable)).unwrap();
    let s = c.session(None);
    let rs = finished(&e, s.execute_with("SELECT {p:String}", vec![Some("bound-value".into())]));
    assert_eq!(rs.phase(), Phase::Done, "{:?}", rs.status().error);
    let requests = wire.requests.lock();
    let sent = requests.iter().find(|r| r.contains("{p:String}")).expect("the statement");
    assert!(sent.contains("bound-value"), "the value goes");
    assert!(sent.contains("name=\"param_p\""), "as a form's field");
    for r in requests.iter() {
        let url = r.lines().next().unwrap();
        assert!(!url.contains("bound-value") && !url.contains("param_") && !url.contains(PASSWORD), "{url}");
        assert!(r.contains("uthorization: Basic "), "the password's only place");
    }
}

#[test]
fn a_dropped_connection_says_nothing_of_the_request() {
    let e = env();
    let (port, _) = plain_server(|r| if r.contains("{p:String}") { Reply::Hangup } else { plain_answers(r) });
    let c = connect(&e, profile(HERE, port, SslMode::Disable)).unwrap();
    let s = c.session(None);
    let rs = finished(&e, s.execute_with("SELECT {p:String}", vec![Some("bound-value".into())]));
    assert_eq!(rs.phase(), Phase::Failed);
    let err = rs.status().error.unwrap().message;
    assert!(err.contains("cannot reach"), "{err}");
    for what in ["bound-value", "param_", "default_format", PASSWORD] {
        assert!(!err.contains(what), "{what} in {err}");
    }
}

/// The URL has a request's settings even with no value in it: an error while reading must not show
/// it either.
#[test]
fn a_cut_answer_says_nothing_of_the_request() {
    let e = env();
    let (port, _) = plain_server(|r| if r.contains("SELECT 'cut'") { Reply::Short("a\nString\nfirst\n") } else { plain_answers(r) });
    let c = connect(&e, profile(HERE, port, SslMode::Disable)).unwrap();
    let s = c.session(None);
    let rs = finished(&e, s.execute("SELECT 'cut'"));
    assert_eq!(rs.phase(), Phase::Failed);
    let err = rs.status().error.unwrap().message;
    assert!(err.contains("ended the result early"), "{err}");
    assert!(!err.contains("default_format"), "{err}");
}

fn until(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(10), "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_long_row_ends_the_statement_and_the_rows_before_it_stay() {
    let e = env();
    let (port, wire) = plain_server(|r| if r.contains("SELECT 'long'") { Reply::Endless { status: 200, start: "a\nString\nfirst\nsecond\n" } } else { plain_answers(r) });
    let c = connect(&e, profile(HERE, port, SslMode::Disable)).unwrap();
    let s = c.session(None);
    let rs = finished(&e, s.execute("SELECT 'long'"));
    assert_eq!(rs.phase(), Phase::Failed);
    assert!(rs.status().error.unwrap().message.contains("larger than 128 MiB"));
    assert_eq!(rs.rows(), 2, "what was read stays");
    assert_eq!(rs.snapshot().text(1, 0).as_deref(), Some("second"));
    assert!(wire.sent() < PAST_A_LONG_ROW, "read on: {} MiB", wire.sent() / MIB);
    until("the statement is killed on the server", || wire.asked("KILL QUERY"));
    // The connection is still good for the next statement.
    let next = finished(&e, s.execute("SELECT 1"));
    assert_eq!(next.phase(), Phase::Done, "{:?}", next.status().error);
}

#[test]
fn an_agents_read_keeps_the_rows_before_a_long_one() {
    let e = env();
    let (port, wire) = plain_server(|r| {
        if r.contains("SELECT 'first-is-long'") {
            Reply::Endless { status: 200, start: "a\nString\n" }
        } else if r.contains("SELECT 'then-long'") {
            Reply::Endless { status: 200, start: "a\nString\nfirst\nsecond\n" }
        } else {
            plain_answers(r)
        }
    });
    let c = connect(&e, profile(HERE, port, SslMode::Disable)).unwrap();
    let read = |sql: &str| e.core.block_on(c.bounded(None, sql, vec![], Mode::Read, Limits::default(), std::future::pending::<()>()));
    let o = read("SELECT 'then-long'").unwrap();
    assert_eq!(o.rows.len(), 2);
    assert_eq!(o.truncated, Some(Truncation::Bytes), "the reply says it was cut");
    match read("SELECT 'first-is-long'") {
        Err(Stop::Server(i)) => assert!(i.message.contains("larger than 128 MiB"), "{}", i.message),
        r => panic!("{r:?}"),
    }
    assert!(wire.sent() < 2 * PAST_A_LONG_ROW, "read on: {} MiB", wire.sent() / MIB);
}

#[test]
fn a_failure_of_any_length_is_read_as_far_as_it_tells() {
    let e = env();
    // After a row, in the stream.
    let (port, wire) = plain_server(|r| {
        if r.contains("SELECT 'boom'") { Reply::Endless { status: 200, start: "a\nString\nfirst\n__exception__\r\nCode: 395. DB::Exception: boom. (X) (version 1)\n" } } else { plain_answers(r) }
    });
    let c = connect(&e, profile(HERE, port, SslMode::Disable)).unwrap();
    let s = c.session(None);
    let rs = finished(&e, s.execute("SELECT 'boom'"));
    assert_eq!(rs.phase(), Phase::Failed);
    assert!(rs.status().error.unwrap().message.contains("boom"));
    assert_eq!(rs.rows(), 1);
    assert!(wire.sent() < 32 * MIB, "read on: {} MiB", wire.sent() / MIB);

    // With an error's status, as the first answer of a connection.
    let (port, wire) = plain_server(|_| Reply::Endless { status: 500, start: "Code: 1. DB::Exception: nope. (X) " });
    let err = refused(&e, profile(HERE, port, SslMode::Disable));
    assert!(err.contains("nope"), "{err}");
    assert!(wire.sent() < 32 * MIB, "read on: {} MiB", wire.sent() / MIB);
}

#[test]
fn a_catalog_answer_has_a_cap_too() {
    let e = env();
    let (port, wire) = plain_server(|r| if r.contains("system.databases") { Reply::Endless { status: 200, start: "name\nString\n" } } else { plain_answers(r) });
    let c = connect(&e, profile(HERE, port, SslMode::Disable)).unwrap();
    assert!(e.core.block_on(c.databases()).unwrap().is_empty());
    assert!(c.schema.notes().iter().any(|n| n.contains("larger than 256 MiB")), "{:?}", c.schema.notes());
    assert!(wire.sent() < 256 * MIB + 64 * MIB, "read on: {} MiB", wire.sent() / MIB);
    until("the statement is killed on the server", || wire.asked("KILL QUERY"));
}
