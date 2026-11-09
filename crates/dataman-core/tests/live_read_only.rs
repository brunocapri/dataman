//! A read-only PostgreSQL connection never writes, whatever a tab or an agent sends: not by
//! opening a read-write transaction, turning one read-write, or changing the session's default.
//! Every write here hides in a function (`dataman_ro_check.note`), which only the server's
//! read-only transaction can stop. Each check runs on the e2e `pg`, on PgBouncer in front of it
//! (transaction mode with and without prepared statements, session mode), on Supavisor when
//! `DATAMAN_SUPAVISOR` says where (as in live_pooler.rs), and on TimescaleDB and YugabyteDB when
//! `DATAMAN_TIMESCALE` / `DATAMAN_YUGABYTE` hold their connection strings (as in live_flavors.rs).
//! A server that does not answer skips its check.

mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::config::{ConnectionProfile, SslMode};
use dataman_core::{ResultSet, Session};

fn local(addr_var: &str, default: &str) -> Option<(ConnectionProfile, Option<String>)> {
    let v = std::env::var(addr_var).unwrap_or_else(|_| default.into());
    let (host, port) = v.rsplit_once(':')?;
    let p = ConnectionProfile { host: host.into(), port: port.parse().ok()?, user: "postgres".into(), database: "dataman".into(), sslmode: SslMode::Disable, ..ConnectionProfile::new("ro") };
    Some((p, Some(pg_password())))
}

fn flavor(var: &str) -> Option<(ConnectionProfile, Option<String>)> {
    let Ok(url) = std::env::var(var) else {
        eprintln!("skipping: {var} is not set");
        return None;
    };
    Some(dataman_core::connstr::parse_profile(&url).unwrap_or_else(|| panic!("{var} does not read as a connection string")))
}

/// The server's connection read-write and read-only, or None when it does not answer.
fn both(e: &Env, server: Option<(ConnectionProfile, Option<String>)>) -> Option<(Arc<Connection>, Arc<Connection>)> {
    let (p, pw) = server?;
    let rw = match e.core.block_on(e.core.connect(p.clone(), pw.clone())) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("skipping: cannot reach {} ({err:#})", p.summary());
            return None;
        }
    };
    let ro = e.core.block_on(e.core.connect(ConnectionProfile { read_only: true, ..p }, pw)).unwrap();
    Some((rw, ro))
}

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
    rs
}

fn ok(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

fn first(rs: &ResultSet) -> String {
    rs.snapshot().text(0, 0).map(|s| s.into_owned()).unwrap_or_default()
}

fn setup(e: &Env, rw: &Arc<Connection>, k: &str) {
    let s = rw.session(None);
    for sql in [
        format!("DROP SCHEMA IF EXISTS {k} CASCADE"),
        format!("CREATE SCHEMA {k}"),
        format!("CREATE TABLE {k}.writes (who text)"),
        format!("CREATE FUNCTION {k}.note(who text) RETURNS int LANGUAGE sql VOLATILE AS $$ INSERT INTO {k}.writes VALUES (who) RETURNING 1 $$"),
    ] {
        ok(e, &s, &sql);
    }
}

fn written(e: &Env, rw: &Arc<Connection>, k: &str) -> String {
    first(&ok(e, &rw.session(None), &format!("SELECT coalesce(string_agg(who, ','), '') FROM {k}.writes")))
}

/// What a tab could send, one statement at a time in one tab, to make its next write commit.
const TAB_WAYS: &[&[&str]] = &[
    &["BEGIN READ WRITE", "SELECT dataman_ro_check.note('begin read write')", "COMMIT"],
    &["START TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE", "SELECT dataman_ro_check.note('start read write')", "COMMIT"],
    &["BEGIN", "SET TRANSACTION READ WRITE", "SELECT dataman_ro_check.note('set transaction')", "COMMIT"],
    &["BEGIN", "SET LOCAL transaction_read_only = off", "SELECT dataman_ro_check.note('set local')", "COMMIT"],
    &["BEGIN", "SAVEPOINT a", "SET TRANSACTION READ WRITE", "SELECT dataman_ro_check.note('savepoint')", "RELEASE a", "COMMIT"],
    &["BEGIN", "COMMIT AND CHAIN", "SET TRANSACTION READ WRITE", "SELECT dataman_ro_check.note('chain')", "COMMIT"],
    &["SET TRANSACTION READ WRITE; SELECT dataman_ro_check.note('one text')"],
    &["BEGIN; SET TRANSACTION READ WRITE; SELECT dataman_ro_check.note('one transaction'); COMMIT"],
    &["SET default_transaction_read_only = off", "SELECT dataman_ro_check.note('session default')"],
    &["SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE", "SELECT dataman_ro_check.note('characteristics')"],
    &["SELECT set_config('default_transaction_read_only', 'off', false)", "SELECT dataman_ro_check.note('set_config')"],
    &["DO $$ BEGIN PERFORM set_config('default_transaction_read_only', 'off', false); END $$", "SELECT dataman_ro_check.note('do')"],
    &["RESET ALL", "SELECT dataman_ro_check.note('reset all')"],
    &["RESET default_transaction_read_only", "SELECT dataman_ro_check.note('reset')"],
];

fn tab_never_writes(e: &Env, rw: &Arc<Connection>, ro: &Arc<Connection>, k: &str) {
    setup(e, rw, k);
    for way in TAB_WAYS {
        let s = ro.session(None);
        for sql in *way {
            run(e, &s, &sql.replace("dataman_ro_check", k));
        }
        run(e, &s, "ROLLBACK");
        assert_eq!(written(e, rw, k), "", "a read-only tab wrote: {way:?}");
    }
    // Reads, and a transaction of the tab's own, still run.
    let s = ro.session(None);
    ok(e, &s, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    ok(e, &s, &format!("SELECT count(*) FROM {k}.writes"));
    ok(e, &s, "SAVEPOINT a");
    ok(e, &s, "ROLLBACK TO SAVEPOINT a");
    ok(e, &s, "COMMIT");
    assert_eq!(first(&ok(e, &s, "SELECT 1; SELECT 2")), "1");
    if !ro.per_transaction() {
        // A session setting outlives the statement that made it.
        ok(e, &s, "SET search_path = pg_catalog");
        assert_eq!(first(&ok(e, &s, "SHOW search_path")), "pg_catalog");
    }
    ok(e, &rw.session(None), &format!("DROP SCHEMA {k} CASCADE"));
}

/// An agent's write (as an allowed statement gets it through the MCP tiers) on a read-only database.
fn agent_never_writes(e: &Env, rw: &Arc<Connection>, ro: &Arc<Connection>, k: &str) {
    setup(e, rw, k);
    let ways = [
        "SET LOCAL transaction_read_only = off; SELECT dataman_ro_check.note('set local')",
        "SET TRANSACTION READ WRITE; SELECT dataman_ro_check.note('set transaction')",
        "SELECT set_config('default_transaction_read_only', 'off', false)",
        "SELECT dataman_ro_check.note('after set_config')",
    ];
    for mode in [Mode::Write, Mode::Rollback, Mode::Autocommit] {
        for sql in ways {
            let sql = sql.replace("dataman_ro_check", k);
            let r = e.core.block_on(ro.bounded(None, &sql, vec![], mode, Limits::default(), std::future::pending()));
            assert!(r.is_err() || sql.contains("set_config"), "{mode:?} {sql}: {r:?}");
            if mode == Mode::Autocommit {
                assert!(matches!(&r, Err(Stop::Refused(_))), "{sql}: {r:?}");
            }
        }
    }
    assert_eq!(written(e, rw, k), "", "an agent wrote on a read-only connection");
    ok(e, &rw.session(None), &format!("DROP SCHEMA {k} CASCADE"));
}

fn both_checks(server: Option<(ConnectionProfile, Option<String>)>, k: &str) {
    let e = env();
    let Some((rw, ro)) = both(&e, server) else { return };
    tab_never_writes(&e, &rw, &ro, k);
    agent_never_writes(&e, &rw, &ro, &format!("{k}_agent"));
}

#[test]
fn direct() {
    both_checks(local("DATAMAN_PG", "127.0.0.1:55432"), "dataman_ro_direct");
}

#[test]
fn pgbouncer_transaction_mode() {
    both_checks(local("DATAMAN_PGBOUNCER_TX", "127.0.0.1:55433"), "dataman_ro_tx");
}

#[test]
fn pgbouncer_transaction_mode_with_prepared_statements() {
    both_checks(local("DATAMAN_PGBOUNCER_TX_PS", "127.0.0.1:55434"), "dataman_ro_tx_ps");
}

#[test]
fn pgbouncer_session_mode() {
    both_checks(local("DATAMAN_PGBOUNCER_SESSION", "127.0.0.1:55435"), "dataman_ro_session");
}

#[test]
fn supavisor_transaction_mode() {
    let Some(addr) = std::env::var("DATAMAN_SUPAVISOR").ok() else {
        eprintln!("skipping: DATAMAN_SUPAVISOR is not set");
        return;
    };
    let server = local("DATAMAN_SUPAVISOR", &addr).map(|(p, _)| {
        let pw = std::env::var("DATAMAN_SUPAVISOR_PASSWORD").unwrap_or_else(|_| pg_password());
        (ConnectionProfile { user: "postgres.dataman".into(), ..p }, Some(pw))
    });
    both_checks(server, "dataman_ro_supavisor");
}

#[test]
fn timescaledb() {
    both_checks(flavor("DATAMAN_TIMESCALE"), "dataman_ro_timescale");
}

#[test]
fn yugabytedb() {
    both_checks(flavor("DATAMAN_YUGABYTE"), "dataman_ro_yugabyte");
}
