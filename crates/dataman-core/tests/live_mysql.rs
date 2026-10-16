//! MySQL against a real server (`DATAMAN_MYSQL`, see `common`): connecting, the types decoded
//! into the grid, errors with their position, streaming with pauses, cancel, transactions from
//! the OK packets, parameters, scripts with DELIMITER, read-only connections. Skips without a
//! server.

mod common;

use common::*;
use dataman_core::config::Engine;
use dataman_core::result::PREFETCH;
use dataman_core::sql::{Dialect, split_in};
use dataman_core::value::Kind;
use dataman_core::{ResultSet, Session, TxEnd, TxStatus};

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

fn texts(rs: &ResultSet, row: usize) -> Vec<Option<String>> {
    let snap = rs.snapshot();
    (0..snap.columns.len()).map(|c| snap.text(row, c).map(|s| s.into_owned())).collect()
}

#[test]
fn connects_and_decodes_every_type() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "dataman_t_types") else { return };
    assert_eq!(c.engine(), Engine::MySql);
    assert_eq!(c.dialect(), Dialect::MySql);
    assert!(c.info.version.starts_with("MySQL ") || c.info.version.starts_with("MariaDB "), "{}", c.info.version);
    assert_eq!(c.info.database, "dataman_t_types");
    assert_eq!(c.info.user, "root");
    assert!(!c.caps().schemas);

    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT id, name, tier, balance, created, profile, photo, code, tags, birth_date, at_time, yr, flags, score, weight FROM customer ORDER BY id");
    let cols = rs.columns().unwrap();
    let kinds: Vec<Kind> = cols.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        vec![
            Kind::Int,
            Kind::Text,
            Kind::Text,
            Kind::Numeric,
            Kind::Timestamp,
            Kind::Json,
            Kind::Bytes,
            Kind::Numeric,
            Kind::Text,
            Kind::Date,
            Kind::Time,
            Kind::Int,
            Kind::Int,
            Kind::Float4,
            Kind::Float
        ]
    );
    assert_eq!(cols[2].type_name, "enum");
    assert_eq!(cols[7].type_name, "bigint unsigned");
    let s1 = |v: &str| Some(v.to_string());
    assert_eq!(
        texts(&rs, 0),
        vec![
            s1("1"),
            s1("Ana Smith"),
            s1("gold"),
            s1("1520.50"),
            s1("2026-01-05 09:15:00.25"),
            s1("{\"city\": \"Boston\", \"points\": 120}"),
            s1("\\x89504e47"),
            s1("18446744073709551615"),
            s1("vip,new"),
            s1("1990-04-12"),
            s1("08:30:00"),
            s1("1990"),
            s1("5"),
            s1("0.1"),
            s1("72.5")
        ]
    );
    let r1 = texts(&rs, 1);
    assert_eq!((r1[10].as_deref(), r1[12].as_deref()), (Some("-12:00:00"), Some("255")));
    let r2 = texts(&rs, 2);
    assert_eq!((r2[5].as_deref(), r2[8].as_deref(), r2[10].as_deref(), r2[13].as_deref()), (None, Some(""), Some("838:59:59"), None));
    let r4 = texts(&rs, 4);
    assert_eq!((r4[1].as_deref(), r4[13].as_deref(), r4[14].as_deref()), (Some("Eva \\ Brown"), Some("1e-07"), Some("1e+300")));
    // Numbers stay native: sorting by a column sorts numbers.
    rs.sort(Some((3, dataman_core::result::SortDir::Desc)));
    assert_eq!(rs.snapshot().text(0, 1).as_deref(), Some("Ana Smith"));

    // The text protocol (a statement the server will not prepare) decodes the same way.
    let a = ok(&e, &s, "SELECT * FROM customer WHERE id = 4");
    let b = ok(&e, &s, "SELECT * FROM customer WHERE id = 4; DO 0");
    assert_eq!(texts(&a, 0), texts(&b, 0));
    assert_eq!(texts(&a, 0)[8].as_deref(), Some("9223372036854775808"), "past i64");

    // Zero and impossible dates stay as the server wrote them.
    let z = ok(&e, &s, "SELECT d, dt FROM odd_dates ORDER BY id");
    assert_eq!(texts(&z, 0), vec![s1("0000-00-00"), s1("0000-00-00 00:00:00")]);
    assert_eq!(texts(&z, 1), vec![s1("2024-02-30"), s1("2024-02-30 10:00:00")]);
    assert_eq!(texts(&z, 2), vec![s1("2024-02-29"), s1("2024-02-29 23:59:59")]);
    exec_quiet(&e, &c, &["DROP DATABASE dataman_t_types"]);
}

#[test]
fn errors_point_at_the_statement() {
    let e = env();
    let Some(c) = mysql_on(&e, mysql_profile("")) else { return };
    let s = c.session(None);
    let sql = "SELECT 'é',\n       2\n  FROM WHERE x = 1";
    let rs = run(&e, &s, sql);
    assert_eq!(rs.phase(), Phase::Failed);
    let err = rs.status().error.unwrap();
    assert_eq!(err.code.as_deref(), Some("1064"));
    let p = err.position.unwrap() as usize;
    assert!(sql[sql.char_indices().nth(p - 1).unwrap().0..].starts_with("WHERE"), "{err:?}");
    let rs = run(&e, &s, "SELECT * FROM no_such_db.no_such_table");
    let err = rs.status().error.unwrap();
    assert!(matches!(err.code.as_deref(), Some("1049" | "1146")), "{err:?}");
    // The session is still usable.
    assert_eq!(ok(&e, &s, "SELECT 42").snapshot().text(0, 0).as_deref(), Some("42"));
    // A wrong password is the server's error.
    let bad = e.core.block_on(e.core.connect(mysql_profile(""), Some("wrong".into())));
    let info = dataman_core::server_error(&bad.err().unwrap()).unwrap();
    assert_eq!(info.code.as_deref(), Some("1045"));
}

#[test]
fn streams_pauses_and_fetches_all() {
    let e = env();
    let Some(c) = mysql_on(&e, mysql_profile("")) else { return };
    let s = c.session(None);
    ok(&e, &s, "SET SESSION cte_max_recursion_depth = 2000000");
    let t0 = Instant::now();
    let rs = s.execute(
        "WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 1000000) SELECT n, MD5(n) AS h, n % 7 = 0 AS seven FROM g",
    );
    e.core.block_on(async {
        let mut rx = rs.subscribe();
        while rs.phase() != Phase::Paused {
            rx.changed().await.unwrap();
        }
    });
    let first_pause = t0.elapsed();
    let loaded = rs.rows();
    assert!(loaded >= PREFETCH && loaded < PREFETCH + 2 * dataman_core::result::CHUNK, "{loaded}");
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(120), rs.wait_finished()).await.expect("timeout") });
    assert_eq!(rs.phase(), Phase::Done, "{:?}", rs.status().error);
    assert_eq!(rs.rows(), 1_000_000);
    let snap = rs.snapshot();
    assert_eq!(snap.text(999_999, 0).as_deref(), Some("1000000"));
    assert_eq!(snap.text(6, 2).as_deref(), Some("1"));
    eprintln!("MySQL 1M rows x 3 cols: first pause after {first_pause:?}, all rows after {:?}", t0.elapsed());

    // A paused result is cancelled when the next statement runs in the same session.
    let big = s.execute("WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 1000000) SELECT n FROM g");
    e.core.block_on(async {
        let mut rx = big.subscribe();
        while big.phase() != Phase::Paused {
            rx.changed().await.unwrap();
        }
    });
    let next = ok(&e, &s, "SELECT 'next'");
    assert_eq!(next.snapshot().text(0, 0).as_deref(), Some("next"));
    assert!(big.phase().is_finished());
}

#[test]
fn bench_fetch_all() {
    // `DATAMAN_MYSQL_BENCH=n`: n timed rounds of 1M rows x 3 columns to the end.
    let Some(n) = std::env::var("DATAMAN_MYSQL_BENCH").ok().and_then(|v| v.parse::<usize>().ok()) else { return };
    let e = env();
    let Some(c) = mysql_on(&e, mysql_profile("")) else { return };
    let s = c.session(None);
    ok(&e, &s, "SET SESSION cte_max_recursion_depth = 2000000");
    let mut times = Vec::new();
    for _ in 0..n {
        let t0 = Instant::now();
        let rs = s.execute("WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 1000000) SELECT n, MD5(n) AS h, n % 7 = 0 AS seven FROM g");
        rs.fetch_all();
        e.core.block_on(rs.wait_finished());
        assert_eq!(rs.rows(), 1_000_000);
        times.push(t0.elapsed());
    }
    times.sort();
    eprintln!("MySQL 1M x 3 fetch-all: median {:?} (min {:?}, max {:?})", times[n / 2], times[0], times[n - 1]);
}

#[test]
fn cancel_a_sleep() {
    let e = env();
    let Some(c) = mysql_on(&e, mysql_profile("")) else { return };
    let s = c.session(None);
    let rs = s.execute("SELECT SLEEP(30)");
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("timeout") });
    // SLEEP interrupted by KILL QUERY returns 1 (not an error) on MySQL; either way it is over fast.
    assert!(rs.phase().is_finished());
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    // A statement interrupted while it reads is reported as cancelled.
    ok(&e, &s, "SET SESSION cte_max_recursion_depth = 100000000");
    let rs = s.execute("WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 50000000) SELECT COUNT(*) FROM g");
    std::thread::sleep(Duration::from_millis(300));
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("timeout") });
    assert_eq!(rs.phase(), Phase::Cancelled, "{:?}", rs.status().error);
    assert_eq!(ok(&e, &s, "SELECT 7").snapshot().text(0, 0).as_deref(), Some("7"));
}

#[test]
fn transactions_from_the_status_flags() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "dataman_t_tx") else { return };
    let s = c.session(None);
    assert_eq!(s.tx().status, TxStatus::Idle);
    ok(&e, &s, "INSERT INTO item VALUES (2, 9, 'x', 1, 1)");
    assert_eq!(s.tx().status, TxStatus::Idle, "auto-commit");

    // A typed START TRANSACTION: the transaction stays open.
    ok(&e, &s, "START TRANSACTION");
    ok(&e, &s, "UPDATE customer SET balance = balance + 1 WHERE id = 1");
    assert_eq!(s.tx().status, TxStatus::Open);
    ok(&e, &s, "UPDATE customer SET balance = balance + 1 WHERE id = 2");
    assert_eq!(s.tx().statements, 2);
    // An error leaves MySQL's transaction open (it is not failed).
    let bad = run(&e, &s, "UPDATE customer SET nope = 1");
    assert_eq!(bad.phase(), Phase::Failed);
    assert_eq!(s.tx().status, TxStatus::Open);
    assert_eq!(e.core.block_on(s.end_transaction(false)).unwrap(), TxEnd::RolledBack);
    assert_eq!(s.tx().status, TxStatus::Idle);
    let balance = |id: i32| ok(&e, &c.session(None), &format!("SELECT balance FROM customer WHERE id = {id}")).snapshot().text(0, 0).map(|s| s.into_owned());
    assert_eq!(balance(1).as_deref(), Some("1520.50"), "rolled back");

    ok(&e, &s, "START TRANSACTION");
    ok(&e, &s, "UPDATE customer SET balance = 1 WHERE id = 3");
    assert_eq!(e.core.block_on(s.end_transaction(true)).unwrap(), TxEnd::Committed);
    assert_eq!(balance(3).as_deref(), Some("1.00"));

    // DDL commits on its own: the badge follows the server.
    ok(&e, &s, "START TRANSACTION");
    ok(&e, &s, "UPDATE customer SET balance = 2 WHERE id = 3");
    assert_eq!(s.tx().status, TxStatus::Open);
    ok(&e, &s, "CREATE TABLE tmp_ddl (x INT)");
    assert_eq!(s.tx().status, TxStatus::Idle);
    assert_eq!(balance(3).as_deref(), Some("2.00"), "the DDL committed the update");

    // Typed START TRANSACTION / COMMIT.
    ok(&e, &s, "START TRANSACTION");
    assert_eq!(s.tx().status, TxStatus::Open);
    ok(&e, &s, "COMMIT");
    assert_eq!(s.tx().status, TxStatus::Idle);
    exec_quiet(&e, &c, &["DROP DATABASE dataman_t_tx"]);
}

#[test]
fn parameters_scripts_and_procedures() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "dataman_t_script") else { return };
    let s = c.session(None);
    // `?` bound in order, as text the server converts; NULL as NULL.
    let rs = s.execute_with("SELECT name FROM customer WHERE id = ? OR email <=> ?", vec![Some("2".into()), None]);
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(rs.phase(), Phase::Done, "{:?}", rs.status().error);
    assert_eq!(texts(&rs, 0), vec![Some("Ben Lewis".to_string())]);
    assert_eq!(rs.rows(), 2, "Eva has no email");

    // A procedure created through DELIMITER, then CALLed: its first result set is shown.
    let script = "DELIMITER //\nCREATE PROCEDURE two()\nBEGIN\n  SELECT 1 AS a;\n  SELECT 2 AS b;\nEND //\nDELIMITER ;\nCALL two();\nCALL customer_orders(1)";
    let stmts: Vec<&str> = split_in(script, Dialect::MySql).into_iter().map(|st| &script[st.range]).collect();
    assert_eq!(stmts.len(), 3);
    ok(&e, &s, stmts[0]);
    let call = ok(&e, &s, stmts[1]);
    assert_eq!((call.columns().unwrap()[0].name.as_str(), call.rows()), ("a", 1));
    let call = ok(&e, &s, stmts[2]);
    assert_eq!(call.rows(), 2);
    assert_eq!(ok(&e, &s, "SELECT customer_total(1)").snapshot().text(0, 0).as_deref(), Some("330.50"));

    // Affected rows (matched rows, as PostgreSQL counts them).
    let up = ok(&e, &s, "UPDATE orders SET status = status WHERE customer_id = 1");
    assert_eq!(up.status().affected, Some(2));
    // The trigger filled the timestamp.
    ok(&e, &s, "INSERT INTO orders (customer_id, total) VALUES (5, 1)");
    assert!(ok(&e, &s, "SELECT created_at FROM orders WHERE customer_id = 5").snapshot().text(0, 0).is_some());
    exec_quiet(&e, &c, &["DROP DATABASE dataman_t_script"]);
}

/// TLS: MySQL 8 serves it with its own self-signed certificate. `prefer` and `require` encrypt
/// without checking it, `verify-full` refuses it, `disable` stays plain; the session (and the
/// cancel's side connection) follow the profile.
#[test]
fn tls_modes() {
    let e = env();
    let mut p = mysql_profile("");
    p.sslmode = dataman_core::config::SslMode::Require;
    let Some(c) = mysql_on(&e, p.clone()) else { return };
    assert!(c.tls_in_use());
    let s = c.session(None);
    let rs = ok(&e, &s, "SHOW SESSION STATUS LIKE 'Ssl_cipher'");
    assert!(texts(&rs, 0)[1].as_deref().is_some_and(|v| !v.is_empty()), "{:?}", texts(&rs, 0));
    p.sslmode = dataman_core::config::SslMode::Prefer;
    assert!(mysql_on(&e, p.clone()).unwrap().tls_in_use());
    p.sslmode = dataman_core::config::SslMode::Disable;
    assert!(!mysql_on(&e, p.clone()).unwrap().tls_in_use());
    p.sslmode = dataman_core::config::SslMode::VerifyFull;
    let r = e.core.block_on(e.core.connect(p, Some(mysql_password())));
    assert!(r.is_err(), "a self-signed certificate does not verify");
}

#[test]
fn read_only_connections_refuse_writes() {
    let e = env();
    let Some(seed) = mysql_seeded(&e, "dataman_t_ro") else { return };
    let mut p = mysql_profile("dataman_t_ro");
    p.read_only = true;
    let Some(c) = mysql_on(&e, p) else { return };
    let s = c.session(None);
    // The server refuses it (SET SESSION TRANSACTION READ ONLY), whatever the client does.
    let rs = run(&e, &s, "UPDATE customer SET balance = 0");
    assert_eq!(rs.phase(), Phase::Failed);
    assert_eq!(rs.status().error.unwrap().code.as_deref(), Some("1792"));
    assert!(dataman_core::guard::read_only_refusal_in("REPLACE INTO customer (id, name) VALUES (1, 'x')", Dialect::MySql).is_some());
    assert_eq!(ok(&e, &s, "SELECT COUNT(*) FROM customer").snapshot().text(0, 0).as_deref(), Some("5"));
    drop(c);
    exec_quiet(&e, &seed, &["DROP DATABASE dataman_t_ro"]);
}

/// One database of a writable connection made read-only (its own setting): the server refuses
/// writes there (1792, the session's READ ONLY); turned off and on again while connected, the
/// same session follows before its next statement.
#[test]
fn read_only_per_database() {
    let e = env();
    let Some(seed) = mysql_seeded(&e, "dataman_t_rodb") else { return };
    let mut p = mysql_profile("dataman_t_rodb");
    p.set_read_only_for("dataman_t_rodb", true);
    let Some(c) = mysql_on(&e, p) else { return };
    assert!(c.read_only_for("dataman_t_rodb") && !c.read_only_for("dataman_t_other"));
    let s = c.session(None);
    let code = |rs: &ResultSet| rs.status().error.and_then(|e| e.code);
    let rs = run(&e, &s, "UPDATE customer SET balance = balance");
    assert_eq!(code(&rs).as_deref(), Some("1792"), "{:?}", rs.status().error);
    c.set_read_only_for("dataman_t_rodb", false);
    ok(&e, &s, "UPDATE customer SET balance = balance");
    c.set_read_only_for("dataman_t_rodb", true);
    let rs = run(&e, &s, "UPDATE customer SET balance = balance");
    assert_eq!(code(&rs).as_deref(), Some("1792"));
    drop((s, c));
    exec_quiet(&e, &seed, &["DROP DATABASE dataman_t_rodb"]);
}
