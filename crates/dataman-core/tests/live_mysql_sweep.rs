//! MySQL sweep: big valid scripts through Dataman's splitter and session, every type decoded.

mod common;

use common::*;
use dataman_core::lint::{LintContext, lint};
use dataman_core::sql::{Dialect, split_in};
use dataman_core::{ResultSet, Session};

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(120), rs.wait_finished()).await.expect("timeout") });
    rs
}

/// Splits like the app and runs each statement; returns the failures `(statement, error)`.
fn script(e: &Env, s: &Arc<Session>, src: &str) -> Vec<(String, String)> {
    let mut bad = Vec::new();
    for st in split_in(src, Dialect::MySql) {
        let sql = &src[st.range];
        let rs = run(e, s, sql);
        if rs.phase() != Phase::Done {
            bad.push((sql.chars().take(160).collect(), format!("{:?}", rs.status().error)));
        }
    }
    bad
}

fn fresh(e: &Env, db: &str) -> Option<Arc<Connection>> {
    let c = mysql_on(e, mysql_profile(""))?;
    exec_quiet(e, &c, &[&format!("DROP DATABASE IF EXISTS {db}"), &format!("CREATE DATABASE {db} DEFAULT CHARACTER SET utf8mb4")]);
    mysql_on(e, mysql_profile(db))
}

fn one(e: &Env, s: &Arc<Session>, sql: &str) -> Vec<Option<String>> {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    let snap = rs.snapshot();
    (0..snap.columns.len()).map(|c| snap.text(0, c).map(|s| s.into_owned())).collect()
}

const ROUTINES: &str = r#"
-- routines without DELIMITER
CREATE TABLE t1 (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50), n INT DEFAULT 0, g INT AS (n * 2) STORED);
CREATE TABLE log1 (id INT AUTO_INCREMENT PRIMARY KEY, msg TEXT);
CREATE PROCEDURE p_loop(IN lim INT, OUT total INT)
BEGIN
  DECLARE i INT DEFAULT 0;
  DECLARE done INT DEFAULT FALSE;
  DECLARE cur CURSOR FOR SELECT id FROM t1;
  DECLARE CONTINUE HANDLER FOR NOT FOUND SET done = TRUE;
  SET total = 0;
  lbl: WHILE i < lim DO
    SET i = i + 1;
    IF i = 3 THEN
      ITERATE lbl;
    ELSEIF i > 8 THEN
      LEAVE lbl;
    ELSE
      SET total = total + i;
    END IF;
  END WHILE lbl;
  REPEAT
    SET i = i - 1;
  UNTIL i <= 0 END REPEAT;
  loop2: LOOP
    SET i = i + 1;
    IF i > 2 THEN LEAVE loop2; END IF;
  END LOOP loop2;
  CASE
    WHEN total > 100 THEN SET total = 100;
    ELSE SET total = total + 0;
  END CASE;
  OPEN cur;
  rd: LOOP
    FETCH cur INTO i;
    IF done THEN LEAVE rd; END IF;
  END LOOP;
  CLOSE cur;
  SELECT total AS t, CASE WHEN total > 1 THEN 'big' ELSE 'small' END AS sz;
  SELECT 2 AS second;
END;
CREATE FUNCTION f_sq(x INT) RETURNS INT DETERMINISTIC
BEGIN
  DECLARE r INT;
  SET r = x * x;
  RETURN r;
END;
CREATE FUNCTION f_one() RETURNS INT DETERMINISTIC RETURN 1;
CREATE TRIGGER trg_ins AFTER INSERT ON t1 FOR EACH ROW
BEGIN
  INSERT INTO log1 (msg) VALUES (CONCAT('ins ', NEW.name));
  IF NEW.n > 100 THEN
    INSERT INTO log1 (msg) VALUES ('big');
  END IF;
END;
CREATE TRIGGER trg_one BEFORE UPDATE ON t1 FOR EACH ROW SET NEW.n = NEW.n + 1;
CREATE EVENT ev_x ON SCHEDULE EVERY 1 DAY DISABLE DO
BEGIN
  DELETE FROM log1 WHERE id < 0;
  DELETE FROM log1 WHERE id < -1;
END;
CREATE DEFINER=CURRENT_USER PROCEDURE p_hand()
BEGIN
  DECLARE EXIT HANDLER FOR SQLEXCEPTION
  BEGIN
    GET DIAGNOSTICS CONDITION 1 @e = MESSAGE_TEXT;
    ROLLBACK;
    SELECT @e AS err;
  END;
  START TRANSACTION;
  INSERT INTO t1 (name) VALUES ('a');
  INSERT INTO t1 (id) VALUES (1), (1);
  COMMIT;
END;
INSERT INTO t1 (name, n) VALUES ('x', 1), ('y;z', 2), ('it''s', 3), ("dq;", 4), ('back\\slash\'; semi', 5);
CALL p_loop(10, @tot);
SELECT @tot;
CALL p_hand();
SELECT f_sq(4), f_one();
SET @a := 5, @b := 6;
SELECT @a + @b AS s, @c := 9 AS c;
PREPARE st FROM 'SELECT ? + ? AS s';
SET @x = 1, @y = 2;
EXECUTE st USING @x, @y;
DEALLOCATE PREPARE st;
SET @q = 'INSERT INTO log1 (msg) VALUES (''p;q'')';
PREPARE st2 FROM @q;
EXECUTE st2;
DEALLOCATE PREPARE st2;
# a hash comment; with semicolon
SELECT 1; -- trailing comment; here
SELECT 2 /* inline ; comment */ AS two;
/*! SELECT 3 AS three */;
/*!50700 SELECT 4 AS four */;
SELECT `weird;name` FROM (SELECT 1 AS `weird;name`) d;
INSERT INTO t1 (name, n) VALUES ('d', 1) ON DUPLICATE KEY UPDATE n = n + 1;
WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 5) SELECT SUM(n) FROM c;
SELECT id, name, ROW_NUMBER() OVER (PARTITION BY n ORDER BY id) rn, LAG(id) OVER w AS prev FROM t1 WINDOW w AS (ORDER BY id);
SELECT * FROM JSON_TABLE('[{"a":1,"b":"x;y"},{"a":2,"b":"z"}]', '$[*]' COLUMNS (a INT PATH '$.a', b VARCHAR(10) PATH '$.b')) jt;
SELECT JSON_OBJECT('k', 1, 'arr', JSON_ARRAY(1, 2, 'x')) AS j, JSON_EXTRACT('{"a":[1,2]}', '$.a[1]') AS e, JSON_UNQUOTE(JSON_EXTRACT('{"a":"b"}', '$.a')) AS u;
START TRANSACTION;
UPDATE t1 SET n = n + 1 WHERE id = 1;
SAVEPOINT sp1;
DELETE FROM t1 WHERE id = 2;
ROLLBACK TO SAVEPOINT sp1;
COMMIT;
BEGIN;
UPDATE t1 SET n = 0;
ROLLBACK;
SELECT COUNT(*) FROM log1
"#;

const DELIM: &str = r#"
DELIMITER $$
CREATE PROCEDURE d1()
BEGIN
  SELECT 1;
  SELECT 2;
END$$
CREATE FUNCTION d2() RETURNS INT DETERMINISTIC
BEGIN
  RETURN 7;
END $$
DELIMITER ;
SELECT d2();
DELIMITER //
CREATE TRIGGER d3 BEFORE INSERT ON log1 FOR EACH ROW BEGIN SET NEW.msg = UPPER(NEW.msg); END//
DELIMITER ;
CALL d1();
DELIMITER ;;
CREATE PROCEDURE d4() BEGIN SELECT 'a;;b'; END;;
DELIMITER ;
CALL d4();
INSERT INTO log1 (msg) VALUES ('hello')
"#;

#[test]
fn routine_scripts_without_delimiter() {
    let e = env();
    let Some(c) = fresh(&e, "dataman_sw1") else { return };
    let s = c.session(None);
    let src = ROUTINES;
    let stmts = split_in(src, Dialect::MySql);
    for st in &stmts {
        eprintln!("--- {}", src[st.range.clone()].lines().next().unwrap_or(""));
    }
    let bad = script(&e, &s, src);
    for b in &bad {
        eprintln!("FAIL {:?}", b);
    }
    assert!(bad.is_empty(), "{bad:#?}");
    exec_quiet(&e, &c, &["DROP DATABASE dataman_sw1"]);
}

#[test]
fn big_scripts_split_fast() {
    let mut my = String::new();
    let mut ms = String::new();
    for i in 0..20000 {
        my += &format!("INSERT INTO t1 (name, n) VALUES ('row {i}; with semi', {i}) ON DUPLICATE KEY UPDATE n = n + 1;\n");
        ms += &format!("INSERT INTO t1 (name, n) VALUES (N'row {i}; with semi', {i});\n");
        if i % 1000 == 999 {
            ms += "GO\n";
        }
    }
    let t = Instant::now();
    assert_eq!(split_in(&my, Dialect::MySql).len(), 20000);
    assert_eq!(dataman_core::sql::batches_in(&ms, Dialect::TSql).len(), 20);
    assert_eq!(split_in(&ms, Dialect::TSql).len(), 20000);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    let t = Instant::now();
    lint(&my, &LintContext::default());
    lint(&ms, &LintContext::default());
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn delimiter_scripts() {
    let e = env();
    let Some(c) = fresh(&e, "dataman_sw2") else { return };
    let s = c.session(None);
    exec(&e, &c, &["CREATE TABLE log1 (id INT AUTO_INCREMENT PRIMARY KEY, msg TEXT)"]);
    let bad = script(&e, &s, DELIM);
    assert!(bad.is_empty(), "{bad:#?}");
    assert_eq!(one(&e, &s, "SELECT msg FROM log1")[0].as_deref(), Some("HELLO"));
    exec_quiet(&e, &c, &["DROP DATABASE dataman_sw2"]);
}

#[test]
fn lint_does_not_reject_valid_scripts() {
    let e = env();
    let Some(c) = fresh(&e, "dataman_sw3") else { return };
    let s = c.session(None);
    let _ = script(&e, &s, ROUTINES);
    let db = c.database().to_string();
    e.core.block_on(c.preload(&db));
    let snap = c.schema.snapshot(&db);
    for src in [ROUTINES, DELIM] {
        let l = lint(src, &LintContext { schema: Some(&snap), ..Default::default() });
        for x in &l {
            eprintln!("LINT {:?} {} -> {:?}", x.severity, x.message, &src[x.range.clone()]);
        }
        let errs: Vec<_> = l.iter().filter(|x| matches!(x.severity, dataman_core::lint::Severity::Error)).collect();
        assert!(errs.is_empty(), "{errs:?}");
    }
    exec_quiet(&e, &c, &["DROP DATABASE dataman_sw3"]);
}
