//! MySQL: result sets of calls and multi-statement texts, odd statements.

mod common;

use common::*;
use dataman_core::{ResultSet, Session};

fn all(e: &Env, s: &Arc<Session>, sql: &str) -> Vec<Arc<ResultSet>> {
    let first = s.execute(sql);
    let mut out = vec![first];
    loop {
        let cur = out.last().unwrap().clone();
        cur.fetch_all();
        e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), cur.wait_finished()).await.expect("timeout") });
        match cur.next_result() {
            Some(n) => out.push(n),
            None => return out,
        }
    }
}

fn shape(rs: &[Arc<ResultSet>]) -> Vec<String> {
    rs.iter()
        .map(|r| {
            let cols: Vec<String> = r.columns().map(|c| c.iter().map(|c| c.name.clone()).collect()).unwrap_or_default();
            let (m, _) = r.messages();
            format!("{:?} {:?} rows={} aff={:?} msgs={:?} err={}", r.phase(), cols, r.rows(), r.status().affected, m.iter().take(2).collect::<Vec<_>>(), r.status().error.as_ref().map(|e| e.message.clone()).unwrap_or_default())
        })
        .collect()
}

#[test]
fn procedure_result_sets_each_get_a_result() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    exec_quiet(&e, &c0, &["DROP DATABASE IF EXISTS dataman_mr2", "CREATE DATABASE dataman_mr2"]);
    let Some(c) = mysql_on(&e, mysql_profile("dataman_mr2")) else { return };
    let s = c.session(None);
    exec(&e, &c, &["CREATE PROCEDURE p3(OUT y INT) BEGIN SELECT 1 AS a; SELECT 2 AS b, 3 AS c; SET y = 7; SELECT 'z' AS d UNION ALL SELECT 'w'; END"]);
    let rs = all(&e, &s, "CALL p3(@y)");
    let got = shape(&rs);
    assert_eq!(rs.len(), 3, "{got:#?}");
    assert!(got.iter().all(|l| l.starts_with("Done")), "{got:#?}");
    assert_eq!((rs[0].rows(), rs[1].rows(), rs[2].rows()), (1, 1, 2));
    assert_eq!(rs[2].snapshot().text(1, 0).as_deref(), Some("w"));
    // The next statement runs on the connection as usual.
    let rs = all(&e, &s, "SELECT @y AS y");
    assert_eq!(rs[0].snapshot().text(0, 0).as_deref(), Some("7"));
    exec_quiet(&e, &c0, &["DROP DATABASE dataman_mr2"]);
}

#[test]
fn result_sets() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    exec_quiet(&e, &c0, &["DROP DATABASE IF EXISTS dataman_mr", "CREATE DATABASE dataman_mr"]);
    let Some(c) = mysql_on(&e, mysql_profile("dataman_mr")) else { return };
    let s = c.session(None);
    for sql in [
        "CREATE TABLE t (a INT PRIMARY KEY, b VARCHAR(10))",
        "CREATE PROCEDURE p(IN x INT, OUT y INT, INOUT z INT) BEGIN SET y = x * 2; SET z = z + 1; SELECT x AS a; SELECT y AS b, z AS c; END",
        "CREATE PROCEDURE p_noresult() BEGIN DECLARE i INT DEFAULT 0; END",
        "CREATE PROCEDURE p_err() BEGIN SELECT 1 AS ok; SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'custom failure'; END",
        "CREATE PROCEDURE p_warn() BEGIN SELECT 1 AS a; SELECT CAST('abc' AS SIGNED) AS w; END",
    ] {
        for r in all(&e, &s, sql) {
            assert_eq!(r.phase(), Phase::Done, "{sql}: {:?}", r.status().error);
        }
    }
    for (name, sql) in [
        ("call-out", "SET @z = 5; CALL p(4, @y, @z)"),
        ("call-single", "CALL p(4, @y, @z)"),
        ("call-noresult", "CALL p_noresult()"),
        ("call-err", "CALL p_err()"),
        ("call-warn", "CALL p_warn()"),
        ("multi-text", "SELECT 1 AS a; SELECT 2 AS b"),
        ("insert-multi", "INSERT INTO t VALUES (1, 'a'), (2, 'b'); SELECT * FROM t"),
        ("empty-stmt", ";"),
        ("select-warn", "SELECT CAST('abc' AS SIGNED) AS w"),
        ("show", "SHOW WARNINGS"),
        ("set", "SET @a := 1, @b := 2"),
        ("select-var", "SELECT @a, @b, @@sql_mode, @@version"),
        ("describe", "DESCRIBE t"),
        ("show-create", "SHOW CREATE TABLE t"),
        ("show-status", "SHOW STATUS LIKE 'Threads%'"),
        ("explain", "EXPLAIN SELECT * FROM t WHERE a = 1"),
        ("explain-analyze", "EXPLAIN ANALYZE SELECT * FROM t"),
        ("lock", "LOCK TABLES t WRITE"),
        ("unlock", "UNLOCK TABLES"),
        ("use", "USE dataman_mr"),
        ("load-data", "LOAD DATA LOCAL INFILE '/nonexistent.csv' INTO TABLE t"),
        ("handler", "HANDLER t OPEN"),
        ("kill-self", "KILL QUERY CONNECTION_ID()"),
        ("analyze", "ANALYZE TABLE t"),
        ("optimize", "OPTIMIZE TABLE t"),
        ("check", "CHECK TABLE t"),
        ("select-into-outfile", "SELECT * FROM t INTO OUTFILE '/tmp/x.txt'"),
        ("do-sleep", "DO SLEEP(0.01)"),
        ("select-dup-cols", "SELECT 1 AS x, 2 AS x, 3, 'a' `weird col`"),
        ("big-int", "SELECT 18446744073709551615 AS u, -9223372036854775808 AS i, 1e308 AS f, 0.1 + 0.2 AS d, CAST(1 AS DECIMAL(65,30)) AS dd"),
        ("null-only", "SELECT NULL AS n"),
        ("long-text", "SELECT REPEAT('é', 100000) AS big"),
        ("json", "SELECT JSON_OBJECT('a', 1, 'b', JSON_ARRAY(1, NULL, 'x')) AS j, CAST('[1,2]' AS JSON) AS j2"),
        ("empty-result", "SELECT * FROM t WHERE a < 0"),
    ] {
        let rs = all(&e, &s, sql);
        eprintln!("{name}: {} sets", rs.len());
        for l in shape(&rs) {
            eprintln!("    {l}");
        }
    }
    exec_quiet(&e, &c0, &["DROP DATABASE dataman_mr"]);
}
