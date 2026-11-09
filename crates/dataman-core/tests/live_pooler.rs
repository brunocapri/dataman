//! Through connection poolers. PgBouncer in transaction mode gives each transaction whichever
//! server connection is free; with two server connections and eight clients, every client's
//! statements land on both, interleaved with the others'. Uncontended, a pooler hands a client
//! the same server connection again and hides what breaks: every check here runs its clients at
//! once.
//!
//! The e2e compose file runs `pgbouncer-tx` (transaction mode, no protocol-level prepared
//! statements, as Supavisor and PgBouncer before 1.21), `pgbouncer-tx-ps` (transaction mode,
//! `max_prepared_statements = 1000`, as Neon's), `pgbouncer-old` (1.18, which leaves a `SET` on
//! the server connection for the next client) and `pgbouncer-session`, all in front of `pg`.
//!
//! Supavisor runs them too when `DATAMAN_SUPAVISOR` says where (see `supavisor_transaction_mode`).
//!
//! A real pooled endpoint (Neon's `-pooler` host, Supabase's port 6543) runs the same checks
//! when `DATAMAN_POOLED_URL` holds its connection string (password included); the objects they
//! make live in the schema `dataman_pool_check` and are dropped at the end.

mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::config::{ConnectionProfile, SslMode};
use dataman_core::pooler::PoolMode;
use dataman_core::{ResultSet, Session};

const CLIENTS: usize = 8;
const STATEMENTS: usize = 40;
const SCHEMA: &str = "dataman_pool_check";

fn bouncer(var: &str, default: &str) -> ConnectionProfile {
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (host, port) = v.rsplit_once(':').unwrap();
    ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: "postgres".into(), database: "dataman".into(), sslmode: SslMode::Disable, ..ConnectionProfile::new("pool") }
}

fn tx() -> ConnectionProfile {
    bouncer("DATAMAN_PGBOUNCER_TX", "127.0.0.1:55433")
}

fn tx_ps() -> ConnectionProfile {
    bouncer("DATAMAN_PGBOUNCER_TX_PS", "127.0.0.1:55434")
}

/// PgBouncer 1.18: no protocol-level prepared statements, and a `SET` stays on the server
/// connection for the next client (later versions carry the parameters the server reports).
fn tx_old() -> ConnectionProfile {
    bouncer("DATAMAN_PGBOUNCER_OLD", "127.0.0.1:55436")
}

fn session_mode() -> ConnectionProfile {
    bouncer("DATAMAN_PGBOUNCER_SESSION", "127.0.0.1:55435")
}

/// The real endpoint's profile and password, from its connection string.
fn real() -> Option<(ConnectionProfile, Option<String>)> {
    let url = std::env::var("DATAMAN_POOLED_URL").ok()?;
    let spec = dataman_core::connstr::parse(&url).expect("DATAMAN_POOLED_URL is no connection string");
    Some(spec.into_profile())
}

fn open(e: &Env, p: ConnectionProfile, pw: Option<&str>) -> Option<Arc<Connection>> {
    match e.core.block_on(e.core.connect(p.clone(), pw.map(str::to_string))) {
        Ok(c) => Some(c),
        Err(err) => {
            eprintln!("skipping: cannot reach {} ({err:#})", p.summary());
            None
        }
    }
}

fn local(e: &Env, p: ConnectionProfile) -> Option<Arc<Connection>> {
    open(e, p, Some(&pg_password()))
}

fn wait(e: &Env, rs: &Arc<ResultSet>) {
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
}

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = s.execute(sql);
    wait(e, &rs);
    rs
}

fn ok(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

fn failed(e: &Env, s: &Arc<Session>, sql: &str) -> String {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Failed, "{sql} ran");
    rs.status().error.map(|i| i.message).unwrap_or_default()
}

/// The checks' SQL in the schema of this run (the tests run at once, each in its own).
fn sch(k: &str, sql: &str) -> String {
    sql.replace("dataman_pool_check", k)
}

fn text(rs: &ResultSet, row: usize, col: usize) -> String {
    rs.snapshot().text(row, col).map(|s| s.into_owned()).unwrap_or_default()
}

/// The objects the checks read and write, made through the connection under test.
fn setup(e: &Env, c: &Arc<Connection>, k: &str) {
    let s = c.session(None);
    for sql in [
        &sch(k, "CREATE SCHEMA IF NOT EXISTS dataman_pool_check"),
        &sch(k, "DROP TABLE IF EXISTS dataman_pool_check.writes, dataman_pool_check.items"),
        &sch(k, "DROP TYPE IF EXISTS dataman_pool_check.mood, dataman_pool_check.pair"),
        &sch(k, "DROP DOMAIN IF EXISTS dataman_pool_check.positive"),
        &sch(k, "CREATE TYPE dataman_pool_check.mood AS ENUM ('calm', 'busy')"),
        &sch(k, "CREATE TYPE dataman_pool_check.pair AS (a int, b text)"),
        &sch(k, "CREATE DOMAIN dataman_pool_check.positive AS int CHECK (VALUE > 0)"),
        &sch(k, "CREATE TABLE dataman_pool_check.items (id int PRIMARY KEY, mood dataman_pool_check.mood, p dataman_pool_check.pair, n dataman_pool_check.positive, tags text[])"),
        &sch(k, "INSERT INTO dataman_pool_check.items SELECT g, CASE WHEN g % 2 = 0 THEN 'calm' ELSE 'busy' END::dataman_pool_check.mood, ROW(g, 'x' || g)::dataman_pool_check.pair, g, ARRAY['a', 'b'] FROM generate_series(1, 50) g"),
        &sch(k, "CREATE TABLE dataman_pool_check.writes (who text, at timestamptz DEFAULT now())"),
        // A write the guard cannot see: only the server's read-only transaction stops it.
        &sch(k, "CREATE OR REPLACE FUNCTION dataman_pool_check.note(who text) RETURNS int LANGUAGE sql VOLATILE AS $$ INSERT INTO dataman_pool_check.writes (who) VALUES (who) RETURNING 1 $$"),
    ] {
        ok(e, &s, sql);
    }
}

fn teardown(e: &Env, c: &Arc<Connection>, k: &str) {
    ok(e, &c.session(None), &sch(k, "DROP SCHEMA dataman_pool_check CASCADE"));
}

/// Eight tabs at once, forty different statements each (user types among them, so the client
/// looks their definitions up too, and a parameter): none may fail.
fn statements_under_contention(e: &Env, c: &Arc<Connection>, k: &str) {
    let sessions: Vec<Arc<Session>> = (0..CLIENTS).map(|_| c.session(None)).collect();
    let mut runs = Vec::new();
    for i in 0..STATEMENTS {
        for (n, s) in sessions.iter().enumerate() {
            let sql = match i % 4 {
                0 => format!("SELECT id, mood, p, n, tags, {n} AS client FROM dataman_pool_check.items WHERE id <= {}", i + 1).replace("dataman_pool_check", k),
                1 => format!("SELECT count(*) + {i} FROM dataman_pool_check.items WHERE mood = 'calm'").replace("dataman_pool_check", k),
                2 => format!("SELECT ({i} + {n})::dataman_pool_check.positive, 'busy'::dataman_pool_check.mood").replace("dataman_pool_check", k),
                _ => format!("SELECT id FROM dataman_pool_check.items WHERE id = $1 AND {i} = {i}").replace("dataman_pool_check", k),
            };
            let rs = if i % 4 == 3 { s.execute_with(sql, vec![Some(format!("{}", n + 1))]) } else { s.execute(sql) };
            runs.push(rs);
        }
        for rs in runs.drain(..) {
            wait(e, &rs);
            assert_eq!(rs.phase(), Phase::Done, "{}: {:?}", rs.sql, rs.status().error);
            assert!(rs.rows() > 0, "{}", rs.sql);
        }
    }
}

/// A read-only tab's function call that writes is refused by the server on every transaction,
/// while another client of the same pooler, not read-only, writes through it all along.
fn read_only_under_contention(e: &Env, read_only: &Arc<Connection>, writer: &Arc<Connection>, k: &str) {
    let ro: Vec<Arc<Session>> = (0..CLIENTS / 2).map(|_| read_only.session(None)).collect();
    let rw: Vec<Arc<Session>> = (0..CLIENTS / 2).map(|_| writer.session(None)).collect();
    ok(e, &rw[0], &sch(k, "TRUNCATE dataman_pool_check.writes"));
    let rounds = 10;
    for _ in 0..rounds {
        let mut runs = Vec::new();
        for s in &ro {
            runs.push((true, s.execute(sch(k, "SELECT dataman_pool_check.note('read-only')"))));
        }
        for s in &rw {
            runs.push((false, s.execute(sch(k, "SELECT dataman_pool_check.note('writer')"))));
        }
        for (refused, rs) in runs {
            wait(e, &rs);
            if refused {
                assert_eq!(rs.phase(), Phase::Failed, "a read-only tab wrote");
                assert_eq!(rs.status().error.and_then(|i| i.code).as_deref(), Some("25006"), "refused by the server's read-only transaction");
            } else {
                assert_eq!(rs.phase(), Phase::Done, "the writer is not read-only: {:?}", rs.status().error);
            }
        }
    }
    let s = &rw[0];
    assert_eq!(text(&ok(e, s, &sch(k, "SELECT count(*) FROM dataman_pool_check.writes WHERE who = 'read-only'")), 0, 0), "0");
    assert_eq!(text(&ok(e, s, &sch(k, "SELECT count(*) FROM dataman_pool_check.writes WHERE who = 'writer'")), 0, 0), (rounds * rw.len()).to_string());

    // A transaction the read-only tab opens is read-only too, and nothing runs after its end
    // in the same text.
    let t = &ro[0];
    ok(e, t, "BEGIN");
    assert!(failed(e, t, &sch(k, "SELECT dataman_pool_check.note('read-only')")).contains("read-only"));
    ok(e, t, "ROLLBACK");
    ok(e, t, "START TRANSACTION READ WRITE");
    assert!(failed(e, t, &sch(k, "SELECT dataman_pool_check.note('read-only')")).contains("read-only"));
    ok(e, t, "ROLLBACK");
    assert!(failed(e, t, &sch(k, "COMMIT; SELECT dataman_pool_check.note('read-only')")).contains("runs alone"));
    assert_eq!(text(&ok(e, s, &sch(k, "SELECT count(*) FROM dataman_pool_check.writes WHERE who = 'read-only'")), 0, 0), "0");

    // An agent's write on the read-only database: the guard sees a read, the server refuses.
    let r = e.core.block_on(read_only.bounded(None, &sch(k, "SELECT dataman_pool_check.note('agent')"), vec![], Mode::Write, Limits::default(), std::future::pending()));
    assert!(matches!(&r, Err(Stop::Server(i)) if i.code.as_deref() == Some("25006")), "{r:?}");
}

/// Agents' reads at once with tabs: read-only transactions, and their binds through the pooler.
fn agents_under_contention(e: &Env, c: &Arc<Connection>, k: &str) {
    let c2 = c.clone();
    let reads = (0..STATEMENTS).map(|i| {
        let c = c2.clone();
        async move {
            let sql = format!("SELECT id, mood, p FROM dataman_pool_check.items WHERE id <= $1 AND {i} >= 0").replace("dataman_pool_check", k);
            c.bounded(None, &sql, vec![Some("5".into())], Mode::Read, Limits::default(), std::future::pending()).await
        }
    });
    let out = e.core.block_on(futures_util::future::join_all(reads));
    for r in out {
        let o = r.expect("an agent's read");
        assert_eq!(o.rows.len(), 5);
    }
    let r = e.core.block_on(c.bounded(None, "SET search_path = public", vec![], Mode::Write, Limits::default(), std::future::pending()));
    assert!(matches!(&r, Err(Stop::Refused(_))), "{r:?}");

    // Another client leaves a search path on the server connections (a function's set_config
    // escapes the session-state refusal, as any other client's SET would): an agent's names
    // still resolve by the login's own search path, the one its checks read.
    let leak = "DO $$ BEGIN PERFORM pg_catalog.set_config('search_path', 'pg_catalog', false); END $$";
    let others: Vec<Arc<Session>> = (0..CLIENTS).map(|_| c.session(None)).collect();
    let runs: Vec<_> = others.iter().map(|s| s.execute(leak)).collect();
    for rs in &runs {
        wait(e, rs);
    }
    let want = text(&ok(e, &others[0], "SELECT pg_catalog.current_setting('search_path')"), 0, 0);
    let reads = (0..CLIENTS).map(|_| {
        let c = c2.clone();
        async move { c.bounded(None, "SELECT pg_catalog.current_setting('search_path')", vec![], Mode::Read, Limits::default(), std::future::pending()).await }
    });
    for r in e.core.block_on(futures_util::future::join_all(reads)) {
        let o = r.expect("an agent's read");
        assert_eq!(o.rows[0][0], serde_json::json!("\"$user\", public"), "an agent read under the search path another client left ({want})");
    }
}

/// What a session would leave on a shared server connection is refused, said why; what ends
/// with the transaction runs.
fn session_state_refused(e: &Env, c: &Arc<Connection>) {
    let s = c.session(None);
    for sql in ["SET search_path = pg_catalog", "LISTEN dataman_pool", "CREATE TEMP TABLE t (a int)", "PREPARE q AS SELECT 1", "SELECT pg_advisory_lock(7)"] {
        assert!(failed(e, &s, sql).contains("transaction mode"), "{sql}");
    }
    ok(e, &s, "BEGIN");
    ok(e, &s, "SET LOCAL search_path = pg_catalog");
    assert_eq!(text(&ok(e, &s, "SHOW search_path"), 0, 0), "pg_catalog");
    ok(e, &s, "CREATE TEMP TABLE t (a int) ON COMMIT DROP");
    ok(e, &s, "COMMIT");
    assert_eq!(s.backend_pid(), None, "the pooler's number is no server process");
}

/// A cancel reaches the server connection running the statement, through the pooler.
fn cancel(e: &Env, c: &Arc<Connection>, k: &str) {
    let s = c.session(None);
    let watcher = c.session(None);
    // Named after the run's schema: the tests at once each wait for their own.
    let rs = s.execute(sch(k, "SELECT pg_sleep(30) AS dataman_pool_check"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while text(&ok(e, &watcher, &sch(k, "SELECT count(*) FROM pg_stat_activity WHERE state = 'active' AND query = 'SELECT pg_sleep(30) AS dataman_pool_check'")), 0, 0) != "1" {
        assert!(Instant::now() < deadline, "the statement never started");
    }
    let started = Instant::now();
    s.cancel();
    wait(e, &rs);
    assert_eq!(rs.phase(), Phase::Cancelled, "{:?}", rs.status().error);
    assert!(started.elapsed() < Duration::from_secs(10));
    ok(e, &s, "SELECT 1");
}

/// Results the grid has paused hold no server connection of the pooler's past their end: they
/// are read ahead (up to a cap), so other clients get the connections. Read ahead or not, every
/// row arrives once, in order.
fn paused_results_free_the_pool(e: &Env, c: &Arc<Connection>) {
    let tabs: Vec<Arc<Session>> = (0..CLIENTS).map(|_| c.session(None)).collect();
    let big = "SELECT g, repeat('x', 100) AS pad FROM generate_series(1, 200000) g";
    let paused: Vec<_> = tabs.iter().map(|s| s.execute(big)).collect();
    let deadline = Instant::now() + Duration::from_secs(30);
    while paused.iter().any(|r| r.phase() != Phase::Paused) {
        assert!(Instant::now() < deadline, "{:?}", paused.iter().map(|r| r.phase()).collect::<Vec<_>>());
        std::thread::sleep(Duration::from_millis(10));
    }
    let other = c.session(None);
    let rs = other.execute("SELECT 1");
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(20), rs.wait_finished()).await.expect("a paused result keeps the pooler's server connections") });
    assert_eq!(rs.phase(), Phase::Done);
    for rs in &paused {
        wait(e, rs);
        assert_eq!((rs.phase(), rs.rows()), (Phase::Done, 200000));
        let snap = rs.snapshot();
        assert!([0, 9999, 10000, 123456, 199999].iter().all(|&r| snap.text(r, 0).as_deref() == Some(&(r + 1).to_string())));
    }
    // Past the cap a result waits for the grid again, and still ends whole.
    let rs = tabs[0].execute("SELECT g, repeat('y', 1000) AS pad FROM generate_series(1, 120000) g");
    wait(e, &rs);
    assert_eq!((rs.phase(), rs.rows()), (Phase::Done, 120000));
    assert_eq!(rs.snapshot().text(119999, 0).as_deref(), Some("120000"));
}

fn suite(e: &Env, c: &Arc<Connection>, read_only: &Arc<Connection>, k: &str) {
    setup(e, c, k);
    statements_under_contention(e, c, k);
    agents_under_contention(e, c, k);
    read_only_under_contention(e, read_only, c, k);
    session_state_refused(e, c);
    cancel(e, c, k);
    paused_results_free_the_pool(e, c);
    teardown(e, c, k);
}

fn read_only(p: &ConnectionProfile) -> ConnectionProfile {
    ConnectionProfile { read_only: true, ..p.clone() }
}

#[test]
fn transaction_mode_without_prepared_statements() {
    let e = env();
    let Some(c) = local(&e, tx()) else { return };
    let ro = local(&e, read_only(&tx())).unwrap();
    assert_eq!(c.pooler.map(|p| p.mode), Some(PoolMode::Transaction), "PgBouncer answers with its own BackendKeyData");
    suite(&e, &c, &ro, "dataman_pool_tx");
}

#[test]
fn transaction_mode_with_prepared_statements() {
    let e = env();
    let Some(c) = local(&e, tx_ps()) else { return };
    let ro = local(&e, read_only(&tx_ps())).unwrap();
    assert_eq!(c.pooler.map(|p| p.mode), Some(PoolMode::Transaction));
    suite(&e, &c, &ro, "dataman_pool_tx_ps");
}

#[test]
fn transaction_mode_on_an_old_pgbouncer() {
    let e = env();
    let Some(c) = local(&e, tx_old()) else { return };
    let ro = local(&e, read_only(&tx_old())).unwrap();
    assert_eq!(c.pooler.map(|p| p.mode), Some(PoolMode::Transaction));
    suite(&e, &c, &ro, "dataman_pool_old");
}

/// `DATAMAN_SUPAVISOR` (host:port of Supavisor's transaction port, a tenant `dataman` whose
/// user `postgres` pools 2 connections to `pg`; `DATAMAN_SUPAVISOR_PASSWORD`): Supabase's
/// shared pooler. Not in the compose file: it needs a metadata database and its tenant made
/// through its API (`PUT /api/tenants/dataman`). Supavisor 2.10 keeps no protocol-level
/// prepared statements in transaction mode and leaves a session's SET for the next client.
#[test]
fn supavisor_transaction_mode() {
    let Some(addr) = std::env::var("DATAMAN_SUPAVISOR").ok() else {
        eprintln!("skipping: DATAMAN_SUPAVISOR is not set");
        return;
    };
    let e = env();
    let p = ConnectionProfile { user: "postgres.dataman".into(), ..bouncer("DATAMAN_SUPAVISOR", &addr) };
    let pw = std::env::var("DATAMAN_SUPAVISOR_PASSWORD").unwrap_or_else(|_| pg_password());
    let c = open(&e, p.clone(), Some(&pw)).expect("Supavisor at DATAMAN_SUPAVISOR");
    let ro = open(&e, read_only(&p), Some(&pw)).unwrap();
    assert_eq!(c.pooler.map(|p| p.mode), Some(PoolMode::Transaction), "Supavisor answers with its own BackendKeyData");
    suite(&e, &c, &ro, "dataman_pool_supavisor");
}

/// A pooler on a host that does not say its mode counts as transaction mode: the same checks.
#[test]
fn session_mode_on_an_unknown_host() {
    let e = env();
    let Some(c) = local(&e, session_mode()) else { return };
    let ro = local(&e, read_only(&session_mode())).unwrap();
    assert_eq!(c.pooler.map(|p| p.mode), Some(PoolMode::Transaction));
    suite(&e, &c, &ro, "dataman_pool_session");
}

#[test]
fn direct_connection_has_no_pooler() {
    let e = env();
    let Some(c) = local(&e, pg_profile()) else { return };
    assert_eq!(c.pooler, None);
    let s = c.session(None);
    ok(&e, &s, "SET search_path = pg_catalog");
    assert_eq!(text(&ok(&e, &s, "SHOW search_path"), 0, 0), "pg_catalog", "a direct session keeps its settings");
    assert!(s.backend_pid().is_some());
}

/// `DATAMAN_POOLED_URL`: a real Neon or Supabase endpoint (`DATAMAN_POOLED_MODE` = transaction,
/// session or direct says what the host should be found to be; transaction by default).
#[test]
fn real_pooled_endpoint() {
    let Some((p, pw)) = real() else {
        eprintln!("skipping: DATAMAN_POOLED_URL is not set");
        return;
    };
    let e = env();
    let c = open(&e, p.clone(), pw.as_deref()).expect("the endpoint in DATAMAN_POOLED_URL");
    assert!(c.tls_in_use(), "a cloud endpoint is reached over TLS");
    let want = std::env::var("DATAMAN_POOLED_MODE").unwrap_or_else(|_| "transaction".into());
    let found = match c.pooler.map(|p| p.mode) {
        None => "direct",
        Some(PoolMode::Session) => "session",
        Some(PoolMode::Transaction) => "transaction",
    };
    assert_eq!(found, want, "{:?}", c.pooler);
    let ro = open(&e, read_only(&p), pw.as_deref()).unwrap();
    setup(&e, &c, SCHEMA);
    statements_under_contention(&e, &c, SCHEMA);
    agents_under_contention(&e, &c, SCHEMA);
    if found == "transaction" {
        read_only_under_contention(&e, &ro, &c, SCHEMA);
        session_state_refused(&e, &c);
    }
    cancel(&e, &c, SCHEMA);
    teardown(&e, &c, SCHEMA);
}

/// `DATAMAN_CLOUD_TLS=1` (needs the internet, no account): Neon's and Supabase's servers pass
/// verify-full, the mode their hosts are always dialed with. With no such login the server
/// refuses it, after the handshake: an error about the login, not the certificate.
#[test]
fn cloud_certificates_verify() {
    if std::env::var_os("DATAMAN_CLOUD_TLS").is_none() {
        eprintln!("skipping: DATAMAN_CLOUD_TLS is not set");
        return;
    }
    let e = env();
    for url in [
        "postgresql://dataman_check:x@ep-cool-darkness-123456-pooler.us-east-2.aws.neon.tech/neondb?sslmode=require&channel_binding=require",
        "postgresql://postgres.dataman0check0000:x@aws-0-us-east-1.pooler.supabase.com:6543/postgres",
        "postgresql://postgres.dataman0check0000:x@aws-0-us-east-1.pooler.supabase.com:5432/postgres",
    ] {
        let (p, pw) = dataman_core::connstr::parse(url).unwrap().into_profile();
        assert_eq!(p.tls_mode(), SslMode::VerifyFull);
        let err = match e.core.block_on(e.core.connect(p, pw)) {
            Ok(_) => panic!("{url}: no such login"),
            Err(err) => format!("{err:#}"),
        };
        assert!(!err.to_lowercase().contains("certificate"), "{url}: {err}");
        assert!(err.contains("db error") || err.contains("FATAL") || err.to_lowercase().contains("not found") || err.to_lowercase().contains("password"), "{url}: {err}");
        eprintln!("{url}: {err}");
    }
}

/// `channel_binding=require` binds SCRAM to the TLS session (Neon's strings ask for it): it
/// connects over TLS and refuses a server that cannot (no TLS).
#[test]
fn channel_binding_required() {
    let Some(addr) = std::env::var("DATAMAN_TLS_PG").ok() else {
        eprintln!("skipping: DATAMAN_TLS_PG is not set");
        return;
    };
    let e = env();
    let (host, port) = addr.rsplit_once(':').unwrap();
    let mut p = ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: "postgres".into(), database: "postgres".into(), sslmode: SslMode::Require, ..ConnectionProfile::new("cb") };
    p.options.insert("channel_binding".into(), "require".into());
    let pw = std::env::var("DATAMAN_TLS_PG_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let c = e.core.block_on(e.core.connect(p.clone(), Some(pw.clone()))).expect("SCRAM with channel binding over TLS");
    assert!(c.tls_in_use());
    let plain = ConnectionProfile { sslmode: SslMode::Disable, ..p };
    let err = e.core.block_on(e.core.connect(plain, Some(pw))).err().expect("channel binding without TLS");
    assert!(format!("{err:#}").contains("channel binding"), "{err:#}");
}
