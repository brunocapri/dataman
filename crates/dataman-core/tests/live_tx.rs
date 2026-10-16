//! Transactions of an editor session against real servers: the status read off ReadyForQuery,
//! a typed BEGIN ended by COMMIT / ROLLBACK (the session's own or typed), a failed
//! transaction and its recovery, a paused result inside a transaction, the CancelRequest on
//! the session's own socket, and read-only connections (enforced by PostgreSQL, accepted but
//! not enforced by LAR: the client-side refusal is what holds there). Skips without servers.

mod common;

use common::*;
use dataman_core::config::{ConnectionProfile, SslMode};
use dataman_core::guard;
use dataman_core::{Session, TxEnd, TxStatus};

fn profile(var: &str, default: &str, user: &str, db: &str) -> ConnectionProfile {
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (host, port) = v.rsplit_once(':').unwrap();
    ConnectionProfile {
        host: host.into(),
        port: port.parse().unwrap(),
        user: user.into(),
        database: db.into(),
        sslmode: SslMode::Disable,
        ..ConnectionProfile::new("tx")
    }
}

fn pg_profile() -> ConnectionProfile {
    profile("DATAMAN_PG", "127.0.0.1:55432", "postgres", "dataman")
}

fn lar_profile() -> ConnectionProfile {
    profile("DATAMAN_LAR", "127.0.0.1:5490", "lar", "lar")
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

/// Runs one statement in `s` to its end.
fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<dataman_core::ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
    rs
}

fn ok(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<dataman_core::ResultSet> {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

fn count(e: &Env, c: &Arc<Connection>, sql: &str) -> String {
    let rs = query(e, c, sql);
    rs.snapshot().text(0, 0).map(|s| s.into_owned()).unwrap_or_default()
}

fn end(e: &Env, s: &Arc<Session>, commit: bool) -> TxEnd {
    e.core.block_on(s.end_transaction(commit)).expect("end transaction")
}

#[test]
fn begin_commit_and_rollback_on_postgres() {
    let e = env();
    let Some(c) = open(&e, pg_profile(), Some(&pg_password())) else { return };
    exec(&e, &c, &["DROP SCHEMA IF EXISTS dataman_tx CASCADE", "CREATE SCHEMA dataman_tx", "CREATE TABLE dataman_tx.t (id int)"]);
    let s = c.session(None);
    assert_eq!(s.tx().status, TxStatus::Idle);

    // Auto-commit: nothing stays open.
    ok(&e, &s, "INSERT INTO dataman_tx.t VALUES (1)");
    assert_eq!(s.tx().status, TxStatus::Idle);

    // BEGIN typed in the editor (not counted); the statements after it are; others do not
    // see the rows yet.
    ok(&e, &s, "BEGIN");
    assert_eq!((s.tx().status, s.tx().statements), (TxStatus::Open, 0));
    ok(&e, &s, "INSERT INTO dataman_tx.t VALUES (2)");
    let st = s.tx();
    assert_eq!((st.status, st.statements), (TxStatus::Open, 1));
    let since = st.since.expect("since");
    ok(&e, &s, "SELECT count(*) FROM dataman_tx.t");
    assert_eq!((s.tx().statements, s.tx().since), (2, Some(since)));
    assert_eq!(count(&e, &c, "SELECT count(*) FROM dataman_tx.t"), "1", "uncommitted rows are not visible elsewhere");
    assert_eq!(end(&e, &s, true), TxEnd::Committed);
    assert_eq!(s.tx().status, TxStatus::Idle);
    assert_eq!(count(&e, &c, "SELECT count(*) FROM dataman_tx.t"), "2");

    // Rollback.
    ok(&e, &s, "BEGIN");
    ok(&e, &s, "DELETE FROM dataman_tx.t");
    assert_eq!(s.tx().status, TxStatus::Open);
    assert_eq!(end(&e, &s, false), TxEnd::RolledBack);
    assert_eq!(count(&e, &c, "SELECT count(*) FROM dataman_tx.t"), "2");
    assert_eq!(end(&e, &s, true), TxEnd::NoTransaction);

    // COMMIT / ROLLBACK typed in the editor end it too.
    ok(&e, &s, "BEGIN");
    ok(&e, &s, "INSERT INTO dataman_tx.t VALUES (3)");
    assert_eq!((s.tx().status, s.tx().statements), (TxStatus::Open, 1));
    ok(&e, &s, "ROLLBACK");
    assert_eq!(s.tx().status, TxStatus::Idle);
    // What cannot run in a transaction runs as typed.
    ok(&e, &s, "VACUUM dataman_tx.t");
    assert_eq!(s.tx().status, TxStatus::Idle);

    // A closed session (a closed tab) rolls back what it had open.
    ok(&e, &s, "BEGIN");
    ok(&e, &s, "INSERT INTO dataman_tx.t VALUES (4)");
    drop(s);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(count(&e, &c, "SELECT count(*) FROM dataman_tx.t WHERE id = 4"), "0");
    exec(&e, &c, &["DROP SCHEMA dataman_tx CASCADE"]);
}

#[test]
fn failed_transaction_and_recovery() {
    let e = env();
    let Some(c) = open(&e, pg_profile(), Some(&pg_password())) else { return };
    let s = c.session(None);
    ok(&e, &s, "BEGIN");
    ok(&e, &s, "SELECT 1");
    let rs = run(&e, &s, "SELECT 1/0");
    assert_eq!(rs.phase(), Phase::Failed);
    assert_eq!(s.tx().status, TxStatus::Failed);
    assert_eq!(s.tx().statements, 2);
    // Everything else is refused until it ends.
    let rs = run(&e, &s, "SELECT 2");
    assert_eq!(rs.status().error.and_then(|e| e.code).as_deref(), Some("25P02"));
    assert!(guard::classify("ROLLBACK").ends_failed_tx());
    // A COMMIT of a failed transaction is a rollback.
    assert_eq!(end(&e, &s, true), TxEnd::RolledBack);
    assert_eq!(s.tx().status, TxStatus::Idle);
    ok(&e, &s, "SELECT 3");
    assert_eq!(s.tx().status, TxStatus::Idle, "autocommit");
    ok(&e, &s, "BEGIN");
    assert_eq!(s.tx().status, TxStatus::Open);

    // A savepoint recovers the transaction without ending it.
    ok(&e, &s, "SAVEPOINT a");
    assert_eq!(run(&e, &s, "SELECT 1/0").phase(), Phase::Failed);
    assert_eq!(s.tx().status, TxStatus::Failed);
    ok(&e, &s, "ROLLBACK TO SAVEPOINT a");
    assert_eq!(s.tx().status, TxStatus::Open);
    assert_eq!(end(&e, &s, false), TxEnd::RolledBack);
}

#[test]
fn paused_result_and_cancel_inside_a_transaction() {
    let e = env();
    let Some(c) = open(&e, pg_profile(), Some(&pg_password())) else { return };
    let s = c.session(None);
    ok(&e, &s, "BEGIN");
    // A big result pauses after the prefetch window; committing reads it to its end instead of
    // cancelling it (a cancel would abort the transaction).
    let rs = s.execute("SELECT g FROM generate_series(1, 200000) g");
    e.core.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut rx = rs.subscribe();
            while rs.phase() != Phase::Paused {
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("pause")
    });
    assert_eq!(s.tx().status, TxStatus::Open);
    assert_eq!(end(&e, &s, true), TxEnd::Committed);
    assert_eq!(rs.phase(), Phase::Done);
    assert!(rs.status().partial);

    // ⌘. on a running statement: the CancelRequest goes on a socket of the session's own.
    let t0 = Instant::now();
    let rs = s.execute("SELECT pg_sleep(20)");
    std::thread::sleep(Duration::from_millis(300));
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("cancelled") });
    assert_eq!(rs.phase(), Phase::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(10));
    ok(&e, &s, "SELECT 1");
}

#[test]
fn status_through_sslmode_prefer() {
    // The server declines TLS: the sniffer skips its one-byte answer and reads the frames.
    let e = env();
    let mut p = pg_profile();
    p.sslmode = SslMode::Prefer;
    let Some(c) = open(&e, p, Some(&pg_password())) else { return };
    let s = c.session(None);
    ok(&e, &s, "BEGIN");
    assert_eq!(s.tx().status, TxStatus::Open);
    ok(&e, &s, "ROLLBACK");
    assert_eq!(s.tx().status, TxStatus::Idle);
}

/// One database of a writable connection made read-only (its own setting): PostgreSQL refuses
/// writes there (25006, `default_transaction_read_only`) and not on another database; turned
/// off and on again while connected, the same session follows before its next statement;
/// grid edits and bounded (MCP) writes refuse there too.
#[test]
fn read_only_per_database_on_postgres() {
    use dataman_core::bounded::{Limits, Mode, Stop};
    let e = env();
    let Some(w) = open(&e, pg_profile(), Some(&pg_password())) else { return };
    exec(&e, &w, &["DROP TABLE IF EXISTS public.dataman_ro_db", "CREATE TABLE public.dataman_ro_db (id int PRIMARY KEY)"]);
    let mut p = pg_profile();
    p.set_read_only_for("dataman", true);
    let Some(c) = open(&e, p, Some(&pg_password())) else { return };
    assert!(c.read_only_for("dataman") && !c.read_only_for("postgres") && !c.read_only_for("DATAMAN"));
    let code = |rs: &dataman_core::ResultSet| rs.status().error.and_then(|e| e.code);
    let s = c.session(None);
    let rs = run(&e, &s, "INSERT INTO public.dataman_ro_db VALUES (1)");
    assert_eq!(code(&rs).as_deref(), Some("25006"), "{:?}", rs.status().error);
    let other = c.session(Some("postgres"));
    ok(&e, &other, "CREATE TEMP TABLE dataman_ro_other (a int)");
    ok(&e, &other, "INSERT INTO dataman_ro_other VALUES (1)");
    let rs = query(&e, &c, "SELECT * FROM public.dataman_ro_db");
    let ed = e.core.block_on(c.edit_target("dataman", &rs, None)).unwrap();
    assert_eq!(ed, dataman_core::edit::Editability::ReadOnly("the database is read-only".into()));
    let bounded = |sql: &str| e.core.block_on(c.bounded(Some("dataman"), sql, vec![], Mode::Write, Limits::default(), std::future::pending::<()>()));
    assert!(matches!(bounded("INSERT INTO public.dataman_ro_db VALUES (9)"), Err(Stop::Refused(_))));
    // Writable again while connected: the same session, no reconnect.
    c.set_read_only_for("dataman", false);
    ok(&e, &s, "INSERT INTO public.dataman_ro_db VALUES (1)");
    assert!(bounded("INSERT INTO public.dataman_ro_db VALUES (9)").is_ok());
    // A toggle inside a transaction waits for its end (a rollback would undo the SET).
    ok(&e, &s, "BEGIN");
    c.set_read_only_for("dataman", true);
    ok(&e, &s, "INSERT INTO public.dataman_ro_db VALUES (2)");
    ok(&e, &s, "COMMIT");
    let rs = run(&e, &s, "INSERT INTO public.dataman_ro_db VALUES (3)");
    assert_eq!(code(&rs).as_deref(), Some("25006"));
    assert_eq!(count(&e, &w, "SELECT count(*) FROM public.dataman_ro_db"), "3");
    drop((s, other, c));
    exec(&e, &w, &["DROP TABLE public.dataman_ro_db"]);
}

#[test]
fn read_only_on_postgres() {
    let e = env();
    let Some(w) = open(&e, pg_profile(), Some(&pg_password())) else { return };
    exec(&e, &w, &["DROP TABLE IF EXISTS public.dataman_ro", "CREATE TABLE public.dataman_ro (id int PRIMARY KEY)"]);
    let mut p = pg_profile();
    p.read_only = true;
    let Some(c) = open(&e, p, Some(&pg_password())) else { return };
    // The client refuses before the server sees it…
    assert!(guard::read_only_refusal("INSERT INTO public.dataman_ro VALUES (1)").is_some());
    // …and PostgreSQL refuses too: the session started read-only.
    let s = c.session(None);
    let rs = run(&e, &s, "INSERT INTO public.dataman_ro VALUES (1)");
    assert_eq!(rs.status().error.and_then(|e| e.code).as_deref(), Some("25006"), "read_only_sql_transaction");
    ok(&e, &s, "SELECT * FROM public.dataman_ro");
    // Grid edits: the target is read-only, and applying is refused anyway.
    let rs = query(&e, &c, "SELECT * FROM public.dataman_ro");
    let ed = e.core.block_on(c.edit_target("dataman", &rs, None)).unwrap();
    assert_eq!(ed, dataman_core::edit::Editability::ReadOnly("the database is read-only".into()));
    let report = e.core.block_on(c.apply_changes(None, Vec::new())).unwrap();
    assert!(!report.committed && report.error.is_some_and(|m| m.contains("read-only")));
    exec(&e, &w, &["DROP TABLE public.dataman_ro"]);
}

#[test]
fn transactions_and_read_only_on_lar() {
    let e = env();
    let Some(w) = open(&e, lar_profile(), None) else { return };
    exec_quiet(&e, &w, &["DROP TABLE dataman_tx_lar"]);
    exec(&e, &w, &["CREATE TABLE dataman_tx_lar (id int, v text)"]);
    // LAR reports the transaction status too.
    let s = w.session(None);
    ok(&e, &s, "BEGIN");
    ok(&e, &s, "INSERT INTO dataman_tx_lar VALUES (1, 'a')");
    assert_eq!((s.tx().status, s.tx().statements), (TxStatus::Open, 1));
    assert_eq!(end(&e, &s, true), TxEnd::Committed);
    ok(&e, &s, "BEGIN");
    ok(&e, &s, "INSERT INTO dataman_tx_lar VALUES (2, 'b')");
    assert_eq!(end(&e, &s, false), TxEnd::RolledBack);
    assert_eq!(count(&e, &w, "SELECT count(*) FROM dataman_tx_lar"), "1");
    ok(&e, &s, "BEGIN");
    assert_eq!(run(&e, &s, "SELECT 1/0").phase(), Phase::Failed);
    assert_eq!(s.tx().status, TxStatus::Failed);
    assert_eq!(end(&e, &s, false), TxEnd::RolledBack);
    assert_eq!(s.tx().status, TxStatus::Idle);
    drop(s);

    // Read-only: LAR accepts `SET default_transaction_read_only = on` and does not enforce it.
    // The client-side refusal is what keeps a read-only LAR connection read-only.
    let mut p = lar_profile();
    p.read_only = true;
    let Some(c) = open(&e, p, None) else { return };
    let s = c.session(None);
    let rs = run(&e, &s, "INSERT INTO dataman_tx_lar VALUES (3, 'c')");
    eprintln!("LAR, read-only session, INSERT: {:?} {:?}", rs.phase(), rs.status().error.map(|e| e.message));
    assert!(guard::read_only_refusal("INSERT INTO dataman_tx_lar VALUES (3, 'c')").is_some());
    exec(&e, &w, &["DROP TABLE dataman_tx_lar"]);
}
