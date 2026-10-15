//! The activity monitor against real servers: the sessions with this process's backends marked,
//! a blocking scenario (one session holds a row lock in an open transaction, another waits on
//! it) read back as a tree, the waiter cancelled and the holder terminated, table health with
//! the bloat estimate, `pg_stat_statements` absent or not loaded (both handled), and what LAR
//! reports. Skips without servers.

mod common;

use common::*;
use diskman_core::activity::{self, Signal, State, Statements};
use diskman_core::{ResultSet, Session, TxStatus};

const SCHEMA: &str = "diskman_activity_t";

fn wait(e: &Env, rs: &Arc<ResultSet>) -> Phase {
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") })
}

fn run(e: &Env, s: &Arc<Session>, sql: &str) {
    let rs = s.execute(sql);
    assert_eq!(wait(e, &rs), Phase::Done, "{sql}: {:?}", rs.status().error);
}

/// Polls `f` until it gives something (10 s).
fn until<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..100 {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn sessions_mark_this_process() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    run(&e, &s, "SELECT 1");
    let pid = s.backend_pid().expect("the BackendKeyData was seen");
    let rows = e.core.block_on(c.activity_sessions(c.database())).unwrap();
    let me = rows.iter().find(|r| r.pid == pid).expect("the tab's session is listed");
    assert_eq!(me.app.as_deref(), Some("Diskman"));
    assert_eq!(me.state_kind(), State::Idle);
    assert!(c.owns_backend(pid), "an editor session is this app's");
    let monitor = rows.iter().find(|r| r.is_self).expect("the monitor's own backend");
    assert_eq!(monitor.state_kind(), State::Active);
    assert!(c.owns_backend(monitor.pid), "the metadata client is this app's too");
    let background = rows.iter().find(|r| r.is_background()).expect("a background process");
    assert!(!c.owns_backend(background.pid));
    assert!(rows.iter().all(|r| r.blocked_by.is_empty() || r.state_kind() == State::Active));
    // An idle session reads the same twice (ages are whole seconds and may tick): nothing to
    // redraw. Only this test's own session: the other tests of this file run in parallel on the
    // same server, and their sessions come and go between the two reads.
    let again = e.core.block_on(c.activity_sessions(c.database())).unwrap();
    let me_again = again.iter().find(|r| r.pid == pid).expect("still listed");
    let still = |r: &activity::SessionRow| (r.pid, r.state.clone(), r.query.clone(), r.app.clone(), r.blocked_by.clone());
    assert_eq!(still(me), still(me_again));
    // A dropped session's mark goes with its connection.
    drop(s);
    assert!(until(|| (!c.owns_backend(pid)).then_some(())).is_some(), "the mark outlived the session");
}

#[test]
fn blocking_tree_cancel_and_terminate() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE")]);
    exec(&e, &c, &[
        &format!("CREATE SCHEMA {SCHEMA}"),
        &format!("CREATE TABLE {SCHEMA}.acct (id int PRIMARY KEY, balance int)"),
        &format!("INSERT INTO {SCHEMA}.acct VALUES (1, 100), (2, 200)"),
    ]);
    let db = c.database().to_string();

    // The holder: a row lock in an open transaction.
    let holder = c.session(None);
    run(&e, &holder, "BEGIN");
    run(&e, &holder, &format!("UPDATE {SCHEMA}.acct SET balance = balance - 1 WHERE id = 1"));
    assert_eq!(holder.tx().status, TxStatus::Open);
    let hp = holder.backend_pid().unwrap();
    // The waiter: the same row.
    let waiter = c.session(None);
    run(&e, &waiter, "SELECT 1");
    let wp = waiter.backend_pid().unwrap();
    let blocked = waiter.execute(format!("UPDATE {SCHEMA}.acct SET balance = 0 WHERE id = 1"));

    let edges = until(|| {
        let edges = e.core.block_on(c.activity_locks(&db)).ok()?;
        edges.iter().any(|x| x.waiter == wp && x.blocker == hp).then_some(edges)
    })
    .expect("the wait shows in pg_locks");
    let edge = edges.iter().find(|x| x.waiter == wp).unwrap();
    eprintln!("edge: {edge:?}");
    assert_eq!(edge.locktype.as_deref(), Some("transactionid"), "a row lock is a wait on the holder's transaction");
    assert_eq!(edge.mode.as_deref(), Some("ShareLock"));
    assert!(edge.relation.as_deref().is_some_and(|r| r.ends_with("acct")), "the relation comes from the tuple lock: {edge:?}");
    assert!(edge.held.as_deref().is_some_and(|h| h.contains("ExclusiveLock")), "{edge:?}");
    assert!(edge.wait.is_some_and(|w| (0..60).contains(&w)));
    let tree = activity::blocking_tree(&edges);
    let top = tree.iter().position(|r| r.pid == hp && r.depth == 0).expect("the holder at the top");
    assert_eq!(tree[top + 1].pid, wp);
    assert_eq!(tree[top + 1].depth, 1);
    assert_eq!(tree[top].behind, 1);
    assert!(!tree.iter().any(|r| r.cycle));

    let sessions = e.core.block_on(c.activity_sessions(&db)).unwrap();
    let h = sessions.iter().find(|r| r.pid == hp).unwrap();
    let w = sessions.iter().find(|r| r.pid == wp).unwrap();
    assert_eq!(h.state_kind(), State::IdleInTx);
    assert!(h.xact_age.is_some());
    assert_eq!(w.state_kind(), State::Active);
    assert_eq!(w.wait_type.as_deref(), Some("Lock"));
    assert_eq!(w.blocked_by, vec![hp]);
    assert!(c.owns_backend(hp) && c.owns_backend(wp));

    // Cancel the waiter: its statement ends cancelled (57014), its session stays.
    assert!(e.core.block_on(c.signal_backend(&db, wp, Signal::Cancel)).unwrap());
    assert_eq!(wait(&e, &blocked), Phase::Cancelled);
    run(&e, &waiter, "SELECT 1");
    assert!(until(|| e.core.block_on(c.activity_locks(&db)).ok().filter(|x| x.is_empty()).map(|_| ())).is_some(), "no wait left");

    // Terminate the holder: its session goes, the transaction rolls back.
    assert!(e.core.block_on(c.signal_backend(&db, hp, Signal::Terminate)).unwrap());
    let gone = until(|| {
        let s = e.core.block_on(c.activity_sessions(&db)).ok()?;
        (!s.iter().any(|r| r.pid == hp)).then_some(())
    });
    assert!(gone.is_some(), "the holder's backend is gone");
    assert!(until(|| (holder.tx().status == TxStatus::Idle).then_some(())).is_some(), "the session saw its connection end");
    assert!(until(|| (!c.owns_backend(hp)).then_some(())).is_some());
    let rs = query(&e, &c, &format!("SELECT balance FROM {SCHEMA}.acct WHERE id = 1"));
    assert_eq!(rs.snapshot().text(0, 0).as_deref(), Some("100"), "rolled back");
    // A signal to a backend that is gone reports false.
    assert!(!e.core.block_on(c.signal_backend(&db, hp, Signal::Cancel)).unwrap());

    // Table health: the scratch table, analyzed, with its sizes.
    exec(&e, &c, &[&format!("ANALYZE {SCHEMA}.acct")]);
    let tables = e.core.block_on(c.activity_tables(&db, true)).unwrap();
    let t = tables.iter().find(|t| t.schema == SCHEMA && t.name == "acct").expect("the table is listed");
    eprintln!("table: {t:?}");
    assert!(t.table_bytes.is_some_and(|b| b > 0) && t.index_bytes.is_some_and(|b| b > 0));
    assert!(t.analyze_age.is_some_and(|a| a < 60));
    assert!(t.live.is_some() && t.dead.is_some());
    let template = activity::maintenance_template(&t.schema, &t.name, activity::Maintenance::VacuumAnalyze);
    exec(&e, &c, &[&template]);

    drop((holder, waiter));
    exec(&e, &c, &[&format!("DROP SCHEMA {SCHEMA} CASCADE")]);
}

/// `pg_stat_statements`: not created here (this container), and created but not loaded (a
/// scratch database); with the library loaded, rows.
#[test]
fn statements_presence() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let here = e.core.block_on(c.activity_statements(c.database())).unwrap();
    eprintln!("pg_stat_statements in {}: {}", c.database(), match &here {
        Statements::Rows(r) => format!("{} rows", r.len()),
        other => format!("{other:?}"),
    });
    assert!(matches!(here, Statements::NotInstalled | Statements::NotLoaded | Statements::Rows(_)), "{here:?}");

    let db = "diskman_pgss_t";
    exec_quiet(&e, &c, &[&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")]);
    exec(&e, &c, &[&format!("CREATE DATABASE {db}")]);
    let s = c.session(Some(db));
    let ext = s.execute("CREATE EXTENSION pg_stat_statements");
    if wait(&e, &ext) == Phase::Done {
        let st = e.core.block_on(c.activity_statements(db)).unwrap();
        eprintln!("created in {db}: {st:?}");
        match st {
            Statements::NotLoaded => {}
            Statements::Rows(rows) => {
                assert!(rows.iter().all(|r| r.calls > 0));
                e.core.block_on(c.reset_statements(db)).unwrap();
            }
            other => panic!("{other:?}"),
        }
    } else {
        eprintln!("pg_stat_statements is not available on this server: {:?}", ext.status().error);
    }
    drop(s);
    exec(&e, &c, &[&format!("DROP DATABASE {db} WITH (FORCE)")]);
}

/// LAR has no `pg_stat_activity`, `pg_locks` nor `pg_stat_statements`, and its
/// `pg_stat_user_tables` is empty: the monitor hides it all.
#[test]
fn lar_reports_little() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    assert!(c.info.is_lar);
    let db = c.database().to_string();
    let sessions = e.core.block_on(c.activity_sessions(&db));
    let locks = e.core.block_on(c.activity_locks(&db));
    let tables = e.core.block_on(c.activity_tables(&db, true));
    let statements = e.core.block_on(c.activity_statements(&db));
    eprintln!("LAR: sessions {:?}, locks {:?}, tables {:?}, statements {:?}", sessions.as_ref().map(Vec::len).map_err(|e| e.to_string()), locks.as_ref().map(Vec::len).map_err(|e| e.to_string()), tables.as_ref().map(Vec::len).map_err(|e| e.to_string()), statements.as_ref().map_err(|e| e.to_string()));
    assert!(sessions.is_err(), "no pg_stat_activity");
    assert!(locks.is_err(), "no pg_locks");
    assert!(tables.is_ok_and(|t| t.is_empty()), "pg_stat_user_tables answers, empty");
    assert!(!matches!(statements, Ok(Statements::Rows(_))));
    // The connection still works after the failed queries.
    let s = c.session(None);
    run(&e, &s, "SELECT 1");
}
