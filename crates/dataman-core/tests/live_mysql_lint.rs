//! MySQL: valid statements must never get a lint error.

mod common;

use common::*;
use dataman_core::lint::{LintContext, Severity, lint};
use dataman_core::sql::Dialect;

const VALID: &[&str] = &[
    "SELECT * FROM t1 WHERE name = \"abc\" AND n IN (1, 2)",
    "SELECT n AS x, COUNT(*) c FROM t1 GROUP BY x HAVING c > 1 ORDER BY x DESC LIMIT 1, 2",
    "SELECT t1.*, l.msg FROM t1 JOIN log1 l ON l.id = t1.id LEFT JOIN log1 l2 USING (id)",
    "SELECT dataman_sw4.t1.name FROM dataman_sw4.t1",
    "SELECT TABLE_NAME, TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
    "INSERT INTO t1 SET name = 'a', n = 1",
    "INSERT INTO t1 (name, n) VALUES ('a', 1) AS new ON DUPLICATE KEY UPDATE n = new.n",
    "INSERT INTO t1 (name, n) VALUES ('a', 1) ON DUPLICATE KEY UPDATE n = VALUES(n)",
    "UPDATE t1 JOIN log1 ON log1.id = t1.id SET t1.n = 1, log1.msg = 'x' WHERE t1.id > 0",
    "UPDATE t1 SET n = n + 1 WHERE id = 1 ORDER BY id LIMIT 1",
    "DELETE t1 FROM t1 JOIN log1 ON log1.id = t1.id WHERE log1.id < 0",
    "DELETE FROM t1 WHERE id = 99 LIMIT 1",
    "REPLACE INTO log1 (id, msg) VALUES (1, 'r')",
    "INSERT INTO log1 (msg) SELECT name FROM t1 WHERE n > 0",
    "SELECT * FROM t1 FOR UPDATE",
    "SELECT * FROM t1 USE INDEX (PRIMARY) WHERE id = 1",
    "SELECT STRAIGHT_JOIN * FROM t1 a, log1 b WHERE a.id = b.id",
    "SELECT GROUP_CONCAT(DISTINCT name ORDER BY name SEPARATOR ',') FROM t1",
    "SELECT NOW() - INTERVAL 1 DAY, DATE_ADD(NOW(), INTERVAL 2 HOUR), CAST(n AS CHAR) FROM t1",
    "SELECT @@version, @@session.sql_mode, @v := 1, @`my var` := 2",
    "SELECT name INTO @nm FROM t1 LIMIT 1",
    "SELECT jt.a, jt.b FROM JSON_TABLE('[{\"a\":1,\"b\":2}]', '$[*]' COLUMNS (a INT PATH '$.a', b INT PATH '$.b')) AS jt",
    "SELECT * FROM (VALUES ROW(1, 2), ROW(3, 4)) AS v(a, b)",
    "SELECT * FROM t1 a, LATERAL (SELECT msg FROM log1 WHERE log1.id = a.id LIMIT 1) l",
    "WITH c (x, y) AS (SELECT id, name FROM t1) SELECT c.x, c.y FROM c",
    "WITH a AS (SELECT id FROM t1), b AS (SELECT id FROM a) SELECT * FROM b",
    "SELECT id, SUM(n) OVER (PARTITION BY name ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) s FROM t1",
    "SELECT ORDER_X.id FROM (SELECT id FROM t1) AS ORDER_X",
    "TABLE t1 ORDER BY id LIMIT 3",
    "SELECT * FROM `t1` WHERE `name` LIKE 'a%' ESCAPE '|'",
    "SELECT `t1`.`id`, `t1`.name FROM `dataman_sw4`.`t1`",
    "SELECT id FROM t1 WHERE name REGEXP '^a' AND n BETWEEN 1 AND 3 AND name IS NOT NULL",
    "SELECT IF(n > 1, 'a', 'b'), IFNULL(name, ''), COALESCE(n, 0) FROM t1",
    "SELECT n DIV 2, n MOD 3, n XOR 1, !n, n <=> 1 FROM t1",
    "SELECT CONVERT(name USING utf8mb4), name COLLATE utf8mb4_bin, _utf8mb4'x', x'4142', 0x4142, b'01' FROM t1",
    "SELECT 1 FROM DUAL",
    "SELECT * FROM t1 WHERE (id, n) IN ((1, 2), (3, 4))",
    "SELECT * FROM t1 WHERE EXISTS (SELECT 1 FROM log1 WHERE log1.id = t1.id)",
    "SELECT a.id FROM t1 a WHERE a.n > ALL (SELECT n FROM t1)",
    "SELECT id FROM t1 UNION ALL SELECT id FROM log1 ORDER BY id",
    "(SELECT id FROM t1) UNION (SELECT id FROM log1) ORDER BY 1 LIMIT 2",
    "SELECT DISTINCT ROW_NUMBER() OVER () FROM t1",
    "SELECT id, name FROM t1 ORDER BY FIELD(name, 'b', 'a'), RAND()",
    "SELECT JSON_ARRAYAGG(name), JSON_OBJECTAGG(id, name) FROM t1",
    "SELECT MATCH(msg) AGAINST('x' IN BOOLEAN MODE) FROM log1",
    "SELECT * FROM t1 t JOIN log1 l ON t.id = l.id AND l.msg <> '' NATURAL JOIN (SELECT 1 AS id) z",
    "CREATE TABLE t9 LIKE t1",
    "CREATE TABLE t8 AS SELECT * FROM t1",
    "CREATE TEMPORARY TABLE tmp1 (a INT)",
    "SELECT * FROM tmp1",
    "CREATE VIEW v1 AS SELECT id, name FROM t1",
    "SELECT v1.name FROM v1",
    "ALTER TABLE t1 ADD COLUMN extra VARCHAR(10), ADD INDEX ix_extra (extra), ALGORITHM=INPLACE, LOCK=NONE",
    "DROP TABLE IF EXISTS t8, t9",
    "SHOW CREATE TABLE t1",
    "DESCRIBE t1",
    "EXPLAIN SELECT * FROM t1",
    "EXPLAIN ANALYZE SELECT * FROM t1 WHERE id = 1",
    "SET NAMES utf8mb4; SET SESSION sql_mode = 'ANSI_QUOTES'",
    "SELECT name FROM t1 WHERE id = ? AND n = ?",
    "LOCK TABLES t1 WRITE; UNLOCK TABLES",
    "ANALYZE TABLE t1",
    "SELECT n AS `select`, n AS 'str alias', n AS \"dq alias\" FROM t1",
    "SELECT MOD(n, 3), n MOD 3, n DIV 2 FROM t1 WHERE n DIV 2 = 1",
    "SELECT CAST(name AS CHAR CHARACTER SET utf8mb4), CONVERT(name, CHAR) FROM t1",
    "SELECT TIMESTAMPDIFF(SECOND, NOW(), NOW()), EXTRACT(YEAR_MONTH FROM NOW()), DATE_FORMAT(NOW(), '%Y') FROM t1",
    "SELECT * FROM t1 LIMIT 5 OFFSET 2",
    "SELECT SQL_CALC_FOUND_ROWS * FROM t1",
    "SELECT DISTINCTROW name FROM t1",
    "SELECT CURRENT_TIMESTAMP(6), UTC_TIMESTAMP, CURDATE(), SYSDATE(), LOCALTIME, CURRENT_USER()",
    "SELECT * FROM t1 WHERE name = BINARY 'a'",
    "INSERT IGNORE INTO t1 (name) VALUES ('a')",
    "SELECT 1 + 1 AS `a b`",
    "CALL p(1, @out)",
    "SELECT * FROM t1 WHERE JSON_CONTAINS(name, '1')",
    "SELECT CASE WHEN n > 1 THEN 'a' WHEN n < 0 THEN 'b' ELSE 'c' END AS k, CASE n WHEN 1 THEN 1 END FROM t1",
    "CREATE PROCEDURE p2(IN a INT) BEGIN DECLARE x INT DEFAULT 0; SELECT n INTO x FROM t1 WHERE id = a; SELECT x; END",
    "CREATE TRIGGER tg1 BEFORE INSERT ON log1 FOR EACH ROW SET NEW.msg = UPPER(NEW.msg)",
    "SELECT t1.g FROM t1 WHERE t1.g > 1",
    "SELECT x.id FROM t1 AS x INNER JOIN t1 AS y ON x.id = y.id WHERE y.name <> x.name",
    "SELECT COUNT(*) FROM t1 WHERE n > (SELECT AVG(n) FROM t1 i WHERE i.name = t1.name)",
    "SELECT name, (SELECT COUNT(*) FROM log1 WHERE log1.id = t1.id) AS cnt FROM t1 ORDER BY cnt",
];

#[test]
fn lint_valid_statements() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    exec_quiet(&e, &c0, &["DROP DATABASE IF EXISTS dataman_sw4", "CREATE DATABASE dataman_sw4"]);
    let Some(c) = mysql_on(&e, mysql_profile("dataman_sw4")) else { return };
    exec(
        &e,
        &c,
        &[
            "CREATE TABLE t1 (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50), n INT DEFAULT 0, g INT AS (n * 2) STORED)",
            "CREATE TABLE log1 (id INT AUTO_INCREMENT PRIMARY KEY, msg TEXT, FULLTEXT(msg))",
        ],
    );
    let db = c.database().to_string();
    let _ = e.core.block_on(c.preload(&db));
    let snap = c.schema.snapshot(&db);
    let mut known = std::collections::HashSet::new();
    let mut bad = Vec::new();
    for src in VALID {
        let l = lint(src, &LintContext { schema: Some(&snap), known: Some(&known), ..Default::default() });
        for x in l.iter().filter(|x| matches!(x.severity, Severity::Error)) {
            bad.push(format!("{src}\n    -> {} @ {:?}", x.message, &src[x.range.clone()]));
        }
        known.extend(dataman_core::lint::created_names_in(src, Dialect::MySql));
    }
    exec_quiet(&e, &c0, &["DROP DATABASE dataman_sw4"]);
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}

/// Reads are never classified as writes (a read-only connection refuses writes by this).
#[test]
fn reads_are_reads() {
    let mut bad = Vec::new();
    for src in VALID {
        let up = src.trim_start().to_ascii_uppercase();
        let read = ["SELECT", "WITH", "TABLE ", "SHOW", "DESCRIBE", "EXPLAIN", "VALUES", "("].iter().any(|v| up.starts_with(v));
        let f = dataman_core::guard::classify_in(src, Dialect::MySql);
        let writes_inside = up.contains(" INTO ") || up.contains("ANALYZE") || up.contains("NEXT VALUE FOR");
        if read && !writes_inside && f.is_write() {
            bad.push(format!("{src} -> {:?} {}", f.effect, f.verb));
        }
        if read && !writes_inside {
            if let Some(why) = dataman_core::guard::read_only_refusal_in(src, Dialect::MySql) {
                bad.push(format!("{src} refused: {why}"));
            }
        }
    }
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}
