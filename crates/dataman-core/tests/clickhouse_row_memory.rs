//! Alone in its binary: it measures the process. What one row costs on this side, whatever the
//! server sends: a fake server's 1.6 GB line, and on the compose service a 1 GB cell.
#![cfg(unix)]

mod common;
#[path = "common/fake_clickhouse.rs"]
mod fake;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use fake::{Reply, plain_answers, plain_server};

/// The cap, what the reader takes ahead of it, the buffer's growth and the allocator's slack.
const MOST_KB: u64 = 512 * 1024;

fn profile(host: &str, port: u16, database: &str) -> ConnectionProfile {
    ConnectionProfile { host: host.into(), port, user: "default".into(), database: database.into(), sslmode: SslMode::Disable, engine: Engine::ClickHouse, ..ConnectionProfile::new("row") }
}

/// The most this process held while `f` ran, above what it held before.
fn peak_above_start<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let start = rss_kb();
    let (stop, peak) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(start)));
    let (s, p) = (stop.clone(), peak.clone());
    let sampler = std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            p.fetch_max(rss_kb(), Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let out = f();
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    (out, peak.load(Ordering::Relaxed).saturating_sub(start))
}

fn read(e: &Env, c: &Arc<Connection>, sql: &str, label: &str) -> Result<dataman_core::bounded::Outcome, Stop> {
    let limits = Limits { timeout: Duration::from_secs(300), ..Limits::default() };
    let (r, kb) = peak_above_start(|| e.core.block_on(c.bounded(None, sql, vec![], Mode::Read, limits, std::future::pending::<()>())));
    eprintln!("{label}: {} MB above the {} MB it started at", kb / 1024, rss_kb() / 1024);
    assert!(kb < MOST_KB, "{label} took {} MB", kb / 1024);
    r
}

#[test]
fn one_row_costs_what_the_cap_says() {
    let e = env();
    let (port, wire) = plain_server(|r| if r.contains("SELECT 'long'") { Reply::Endless { status: 200, start: "a\nString\n" } } else { plain_answers(r) });
    let c = e.core.block_on(e.core.connect(profile("127.0.0.1", port, ""), Some("x".into()))).unwrap();
    let r = read(&e, &c, "SELECT 'long'", "a fake server's endless line");
    assert!(matches!(&r, Err(Stop::Server(i)) if i.message.contains("larger than")), "{r:?}");
    assert!(wire.sent() < 256 * 1024 * 1024, "read on: {} MiB", wire.sent() / 1024 / 1024);

    let addr = std::env::var("DATAMAN_CLICKHOUSE").unwrap_or_else(|_| "127.0.0.1:18123".into());
    let (host, port) = addr.rsplit_once(':').unwrap();
    let pw = std::env::var("DATAMAN_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let Ok(c) = e.core.block_on(e.core.connect(profile(host, port.parse().unwrap(), ""), Some(pw))) else {
        assert!(std::env::var_os("DATAMAN_LIVE_REQUIRED").is_none(), "nothing at {addr}");
        eprintln!("skipping the server's cell: nothing at {addr}");
        return;
    };
    let r = read(&e, &c, "SELECT arrayStringConcat(arrayMap(x -> repeat('x', 1000000), range(1000)))", "a server's 1 GB cell");
    assert!(matches!(&r, Err(Stop::Server(i)) if i.message.contains("larger than")), "{r:?}");
}
