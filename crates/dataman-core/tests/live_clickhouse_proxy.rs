//! Alone in its binary: it changes the environment, which reqwest reads as a client is built.

use dataman_core::config::{ConnectionProfile, Dirs, Engine, SslMode};
use dataman_core::{Core, MemorySecrets};
use std::sync::Arc;

/// A proxy nothing listens on: a connection that took it would fail, and with it the password
/// would leave for whoever does listen.
#[test]
fn a_proxy_in_the_environment_is_not_taken() {
    // SAFETY: the only test here, before anything else runs.
    unsafe {
        for var in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::set_var(var, "http://127.0.0.1:1");
        }
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
    }
    let addr = std::env::var("DATAMAN_CLICKHOUSE").unwrap_or_else(|_| "127.0.0.1:18123".into());
    if std::net::TcpStream::connect(&addr).is_err() {
        eprintln!("skipping: nothing at {addr}");
        return;
    }
    let (host, port) = addr.rsplit_once(':').unwrap();
    let p = ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: "default".into(), database: String::new(), sslmode: SslMode::Disable, engine: Engine::ClickHouse, ..ConnectionProfile::new("clickhouse") };
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(Dirs::at(dir.path().join("dataman")), Arc::new(MemorySecrets::default())).unwrap();
    let pw = std::env::var("DATAMAN_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let c = core.block_on(core.connect(p, Some(pw))).expect("straight to the server");
    assert!(c.info.version.starts_with("ClickHouse "));
}
