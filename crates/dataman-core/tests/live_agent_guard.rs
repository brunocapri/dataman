//! What the agents' classifier reads must be what the server runs: a statement the lexer sees
//! as one write may not reach the server as several (a string the server closes earlier than
//! the lexer, a session setting that changes how strings are read).

mod common;
use common::*;

use dataman_core::bounded::{Limits, Mode};
use dataman_core::guard::{WriteClass, agent_write_class};

fn count(e: &Env, c: &Arc<Connection>, table: &str) -> i64 {
    let rs = query(e, c, &format!("SELECT COUNT(*) FROM {table}"));
    rs.snapshot().text(0, 0).unwrap().parse().unwrap()
}

/// Runs `sql` as the agent tools would (a SafeWrite in a write transaction, a read in a
/// read-only one) and returns the probe's row count after it.
fn agent_run(e: &Env, c: &Arc<Connection>, db: &str, sql: &str) -> Option<String> {
    let mode = match agent_write_class(sql, c.dialect()) {
        WriteClass::Read => Mode::Read,
        WriteClass::SafeWrite => Mode::Write,
        WriteClass::Destructive { .. } => return None,
    };
    let r = e.core.block_on(c.bounded(Some(db), sql, vec![], mode, Limits::default(), std::future::pending()));
    Some(match r {
        Ok(_) => "ran".into(),
        Err(err) => err.to_string(),
    })
}

#[test]
fn postgres_strings_without_standard_conforming() {
    let e = env();
    let Some(admin) = pg(&e) else { return };
    // A role whose sessions read strings with backslash escapes (no database to drop after).
    exec_quiet(&e, &admin, &["DROP SCHEMA IF EXISTS dataman_guard CASCADE", "DROP ROLE IF EXISTS dataman_guard"]);
    exec(&e, &admin, &[
        "CREATE ROLE dataman_guard LOGIN PASSWORD 'guard'",
        "CREATE SCHEMA dataman_guard AUTHORIZATION dataman_guard",
        "ALTER ROLE dataman_guard SET standard_conforming_strings = off",
        "ALTER ROLE dataman_guard SET search_path = dataman_guard",
    ]);
    let mut p = admin.profile.clone();
    p.user = "dataman_guard".into();
    let c = e.core.block_on(e.core.connect(p, Some("guard".into()))).unwrap();
    let db = c.database().to_string();
    exec(&e, &c, &["CREATE TABLE probe (n int)", "INSERT INTO probe VALUES (1)", "CREATE TABLE scratch (s text)"]);
    for sql in [
        r"INSERT INTO scratch VALUES ('\''); DELETE FROM probe; --')",
        r"INSERT INTO scratch VALUES ('\'' || ''); DROP TABLE probe; --')",
        r"SELECT '\''; DELETE FROM probe; --'",
    ] {
        let ran = agent_run(&e, &c, &db, sql);
        assert_eq!(count(&e, &c, "probe"), 1, "{sql} ({ran:?}) changed the probe");
    }
    c.close();
    exec_quiet(&e, &admin, &["DROP SCHEMA IF EXISTS dataman_guard CASCADE", "DROP ROLE IF EXISTS dataman_guard"]);
}

#[test]
fn postgres_one_statement_per_prepare() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec(&e, &c, &["DROP TABLE IF EXISTS guard_probe", "CREATE TABLE guard_probe (n int)", "INSERT INTO guard_probe VALUES (1)"]);
    // A string setting changed inside the call reaches the next statement of the same call.
    let sql = r"SELECT set_config('standard_conforming_strings', 'off', true); INSERT INTO guard_probe VALUES (length('\'')); DELETE FROM guard_probe; --'))";
    let ran = agent_run(&e, &c, &c.database().to_string(), sql);
    let rs = query(&e, &c, "SELECT COUNT(*) FROM guard_probe WHERE n = 1");
    assert_eq!(rs.snapshot().text(0, 0).as_deref(), Some("1"), "{ran:?}");
    exec(&e, &c, &["DROP TABLE guard_probe"]);
}

#[test]
fn mysql_strings_without_backslash_escapes() {
    let e = env();
    let Some(root) = mysql_on(&e, mysql_profile("")) else { return };
    exec(&e, &root, &["DROP DATABASE IF EXISTS dataman_guard", "CREATE DATABASE dataman_guard"]);
    let before = query(&e, &root, "SELECT @@GLOBAL.sql_mode").snapshot().text(0, 0).unwrap().into_owned();
    exec(&e, &root, &["SET GLOBAL sql_mode = CONCAT(@@GLOBAL.sql_mode, ',NO_BACKSLASH_ESCAPES,ANSI_QUOTES')"]);
    let restore = format!("SET GLOBAL sql_mode = '{before}'");
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let c = mysql_on(&e, mysql_profile("dataman_guard")).unwrap();
        exec(&e, &c, &["CREATE TABLE probe (n int)", "INSERT INTO probe VALUES (1)", "CREATE TABLE scratch (s text)"]);
        for sql in [
            r"INSERT INTO scratch VALUES ('\'); DELETE FROM probe; -- ')",
            r#"INSERT INTO scratch VALUES ("\"); DELETE FROM probe; -- ")"#,
            r"SELECT '\'; DELETE FROM probe; -- '",
        ] {
            let ran = agent_run(&e, &c, "dataman_guard", sql);
            assert_eq!(count(&e, &c, "probe"), 1, "{sql} ({ran:?}) changed the probe");
        }
        // A session setting the call changes itself.
        let sql = r"SET @a = 1, sql_mode = 'NO_BACKSLASH_ESCAPES'; INSERT INTO scratch VALUES ('\'); DELETE FROM probe; -- ')";
        let ran = agent_run(&e, &c, "dataman_guard", sql);
        assert_eq!(count(&e, &c, "probe"), 1, "{sql} ({ran:?}) changed the probe");
    }));
    exec(&e, &root, &[&restore, "DROP DATABASE IF EXISTS dataman_guard"]);
    if let Err(p) = run {
        std::panic::resume_unwind(p);
    }
}

/// `execute` on a write tier runs whatever is not destructive in a write transaction that
/// commits: an EXPLAIN that analyzes runs its statement there.
#[test]
fn postgres_explain_analyze_options() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec(&e, &c, &["DROP TABLE IF EXISTS guard_explain", "CREATE TABLE guard_explain (n int)", "INSERT INTO guard_explain VALUES (1), (2)"]);
    let db = c.database().to_string();
    for sql in ["EXPLAIN (ANALYZE, COSTS OFF) DELETE FROM guard_explain", "EXPLAIN (ANALYZE TRUE, TIMING FALSE) DELETE FROM guard_explain", "EXPLAIN (FORMAT JSON, ANALYZE) DELETE FROM guard_explain"] {
        let class = agent_write_class(sql, c.dialect());
        if !class.is_destructive() {
            let _ = e.core.block_on(c.bounded(Some(&db), sql, vec![], Mode::Write, Limits::default(), std::future::pending()));
        }
        assert_eq!(count(&e, &c, "guard_explain"), 2, "{sql} ran as {class:?}");
    }
    exec(&e, &c, &["DROP TABLE guard_explain"]);
}

/// A password written into a statement is kept masked in the history.
#[test]
fn history_masks_passwords() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec_quiet(&e, &c, &["ALTER ROLE dataman_no_such_role PASSWORD 'hunter4'"]);
    let h = e.core.history.load(&c.profile.id, 20).unwrap();
    let sql: Vec<&str> = h.iter().map(|x| x.sql.as_str()).collect();
    assert!(sql.contains(&"ALTER ROLE dataman_no_such_role PASSWORD '••••••'") && !sql.iter().any(|s| s.contains("hunter4")), "{sql:?}");
}
