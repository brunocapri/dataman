//! Statements the agent SQL analyzer must read the same way in every layer, per dialect, and the
//! variations of them that change nothing for the server (shared with dataman-mcp's tests).
#![allow(dead_code)]

use dataman_core::sql::{Dialect, TokenKind, tokenize_in};

pub const DIALECTS: [Dialect; 3] = [Dialect::Postgres, Dialect::MySql, Dialect::TSql];

/// Texts with no statement at all.
pub const EMPTY: &[&str] = &["", "   ", "\n\t", ";", ";;", " ; ; ", "-- only a comment", "/* only */", "/* a */ ; -- b"];

pub const POSTGRES: &[&str] = &[
    "SELECT 1",
    "select * from users where id = 1",
    "SeLeCt Id FrOm UsErS",
    "SELECT 'a;b' AS x",
    "SELECT 'it''s; DELETE FROM t'",
    "SELECT E'it\\'s; DELETE FROM t'",
    "SELECT e'\\\\'",
    "SELECT $$a; DELETE FROM t$$",
    "SELECT $tag$ $$; DROP TABLE t; $$ $tag$",
    "SELECT U&'d\\0061t\\+000061'",
    "SELECT \"weird;name\" FROM \"Users\"",
    "SELECT \"a\"\"b\" FROM t",
    "SELECT 1 /* a /* nested; DELETE FROM t */ still; */",
    "SELECT 1 -- ; DELETE FROM t\n",
    "SELECT x FROM t WHERE y = $1",
    "SELECT a::text FROM t",
    "SELECT * FROM t FOR UPDATE",
    "SELECT * FROM t FOR NO KEY UPDATE",
    "SELECT * FROM t FOR SHARE",
    "SELECT pg_sleep(1)",
    "SELECT pg_terminate_backend(42)",
    "SELECT \"pg_terminate_backend\"(42)",
    "SELECT nextval('s')",
    "SELECT set_config('standard_conforming_strings', 'off', false)",
    "SELECT lar_backup_create('x')",
    "SELECT * INTO new_t FROM t",
    "SELECT a INTO TEMP x FROM t",
    "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM d",
    "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
    "WITH x AS (SELECT 1) SELECT * FROM x",
    "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM x WHERE n < 3) SELECT * FROM x",
    "SELECT (WITH i AS (INSERT INTO t VALUES (1) RETURNING id) SELECT id FROM i)",
    "EXPLAIN SELECT 1",
    "EXPLAIN ANALYZE SELECT 1",
    "EXPLAIN ANALYZE DELETE FROM t",
    "EXPLAIN ANALYSE VERBOSE UPDATE t SET a = 1",
    "EXPLAIN (ANALYZE, COSTS OFF) DELETE FROM t WHERE id = 1",
    "EXPLAIN (ANALYZE OFF) DELETE FROM t",
    "EXPLAIN (FORMAT JSON, ANALYZE true) UPDATE t SET a = 1 WHERE id = 2",
    "TABLE users",
    "VALUES (1), (2)",
    "SHOW search_path",
    "INSERT INTO t (a) VALUES ('x;y')",
    "INSERT INTO t SELECT * FROM u",
    "INSERT INTO t (a) VALUES (1) RETURNING *",
    "UPDATE t SET a = 1 WHERE id = 2",
    "UPDATE t SET a = 1",
    "UPDATE t SET a = 1 WHERE 1 = 1",
    "UPDATE t SET a = 1 WHERE id = id",
    "DELETE FROM t WHERE id = 3",
    "DELETE FROM t",
    "DELETE FROM t WHERE true",
    "DELETE FROM ONLY t WHERE id = -1",
    "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a",
    "TRUNCATE t",
    "DROP TABLE t",
    "CREATE TABLE x (a int)",
    "ALTER TABLE t DROP COLUMN a",
    "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT 2; END",
    "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql",
    "DO $$ BEGIN DELETE FROM t; END $$",
    "CALL p(1)",
    "COPY t FROM '/tmp/x'",
    "COPY (SELECT 1) TO STDOUT",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SET search_path = public",
    "SET ROLE admin",
    "RESET ROLE",
    "VACUUM t",
    "LISTEN ch",
    "NOTIFY ch",
    "LOCK TABLE t",
    "GRANT SELECT ON t TO x",
    "PREPARE p AS DELETE FROM t",
    "EXECUTE p",
    "DEALLOCATE p",
    "DECLARE c CURSOR FOR SELECT 1",
    "FETCH c",
    "SELECT * FROM \"Ünïcödé\" WHERE \"ñ\" = 'é'",
    "SELECT 'ü' || 'ß' AS \"→\"",
    "SELECT\u{a0}1",
    "SELECT 1\u{2003}FROM t",
    "SELECT\u{b}1",
];

pub const MYSQL: &[&str] = &[
    "SELECT 1",
    "select * from users where id = 1",
    "SELECT 'a;b', \"c;d\"",
    "SELECT 'it\\'s; DELETE FROM t'",
    "SELECT \"it\\\"s; DELETE FROM t\"",
    "SELECT 'a\\\\'",
    "SELECT 'it''s'",
    "SELECT `weird;name` FROM `t``x`",
    "SELECT 1 # ; DELETE FROM t\n",
    "SELECT 1 -- ; DELETE FROM t\n",
    "SELECT 1 --1",
    "SELECT 1 /* ; DELETE FROM t */",
    "SELECT /*!40001 SQL_NO_CACHE */ 1",
    "SELECT 1 /*! FOR UPDATE */",
    "SELECT 1 /*!; DELETE FROM t */",
    "SELECT 1 /*!99999 ; DELETE FROM t */",
    "DELETE FROM t WHERE id = 1 /*! OR 1=1 */",
    "DELETE FROM t /*!99999 WHERE id = 1 */",
    "/*!50000 DROP TABLE t */",
    "/*!40101 SET NAMES utf8mb4 */",
    "/*M!100000 SET x = 1 */",
    "SELECT /*+ MAX_EXECUTION_TIME(1000) */ * FROM t",
    "SELECT * FROM t FOR UPDATE",
    "SELECT * FROM t LOCK IN SHARE MODE",
    "SELECT GET_LOCK('x', 1)",
    "SELECT SLEEP(1)",
    "SELECT * INTO OUTFILE '/tmp/x' FROM t",
    "SELECT a INTO @v FROM t",
    "SELECT * FROM t INTO @x",
    "SELECT @@version, @a",
    "SELECT _utf8mb4'x' COLLATE utf8mb4_bin",
    "WITH d AS (SELECT 1) SELECT * FROM d",
    "EXPLAIN SELECT 1",
    "EXPLAIN ANALYZE SELECT 1",
    "EXPLAIN FORMAT=TREE DELETE FROM t",
    "DESCRIBE t",
    "DESC t",
    "SHOW TABLES",
    "INSERT INTO t VALUES (1)",
    "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 2",
    "REPLACE INTO t VALUES (1)",
    "UPDATE t SET a = 1 WHERE id = 2",
    "UPDATE t SET a = 1",
    "UPDATE LOW_PRIORITY t SET a = 1 WHERE 1",
    "DELETE FROM t WHERE id = 3",
    "DELETE FROM t",
    "DELETE QUICK FROM t WHERE id = 3 LIMIT 1",
    "LOAD DATA INFILE 'x' INTO TABLE t",
    "CREATE PROCEDURE p() BEGIN SELECT 1; DELETE FROM t; END",
    "CREATE TABLE x (a int)",
    "SET @a = 1",
    "SET @a = 1, sql_mode = ''",
    "SET SESSION sql_mode = ''",
    "DO SLEEP(1)",
    "HANDLER t OPEN",
    "CALL p()",
    "START TRANSACTION",
    "RENAME TABLE a TO b",
    "TRUNCATE t",
    "DROP TABLE t",
    "SELECT * FROM `Ünïcödé` WHERE `ñ` = 'é'",
    "SELECT\u{a0}1",
];

pub const TSQL: &[&str] = &[
    "SELECT 1",
    "select * from users where id = 1",
    "SELECT N'a;b'",
    "SELECT 'it''s; DELETE FROM t'",
    "SELECT [weird;name] FROM [t]]x]",
    "SELECT \"quoted\" FROM t",
    "SELECT 1 /* a /* nested */ ; DELETE FROM t */",
    "SELECT 1 -- ; DELETE FROM t\n",
    "SELECT TOP 10 * FROM t",
    "SELECT * FROM t WITH (UPDLOCK)",
    "SELECT * FROM t WITH (NOLOCK)",
    "SELECT NEXT VALUE FOR s",
    "SELECT * INTO #tmp FROM t",
    "SELECT * INTO new_t FROM t",
    "SELECT 1 DISABLE TRIGGER ALL ON t",
    "SELECT 1 DELETE FROM t",
    "SELECT 1 SET CONTEXT_INFO 0x01",
    "SELECT * FROM t WHERE a = 1 UPDATE t SET a = 2 WHERE id = 1",
    "SELECT @@ROWCOUNT, @x",
    "WITH d AS (SELECT 1 AS x) SELECT * FROM d",
    "WITH d AS (SELECT 1 AS x) DELETE FROM t WHERE id IN (SELECT x FROM d)",
    "INSERT INTO t VALUES (1)",
    "INSERT INTO t EXEC p",
    "INSERT INTO t SELECT * FROM u",
    "INSERT INTO t OUTPUT inserted.id VALUES (1)",
    "UPDATE t SET a = 1 WHERE id = 2",
    "UPDATE TOP (5) t SET a = 1",
    "DELETE TOP (5) FROM t WHERE id > 1",
    "DELETE FROM t",
    "MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a;",
    "TRUNCATE TABLE t",
    "DROP TABLE t",
    "EXEC sp_help 't'",
    "EXEC sp_who",
    "EXEC dbo.do_things",
    "EXECUTE ('DELETE FROM t')",
    "DECLARE @x int",
    "DECLARE @x int = 1 SELECT @x",
    "SET NOCOUNT ON",
    "SET ROWCOUNT 0",
    "SET @x = 1",
    "PRINT 'hi'",
    "BEGIN TRAN",
    "COMMIT",
    "IF 1 = 1 SELECT 1",
    "IF 1 = 1 DELETE FROM t",
    "WHILE 1 = 0 SELECT 1",
    "WAITFOR DELAY '00:00:01'",
    "UPDATE STATISTICS t",
    "DBCC CHECKDB",
    "CREATE PROCEDURE p AS SELECT 1",
    "SELECT * FROM [Ünïcödé] WHERE [ñ] = N'é'",
    "SELECT\u{a0}1",
];

/// Multi-statement texts the splitters must agree on.
pub const SCRIPTS: &[(Dialect, &str)] = &[
    (Dialect::Postgres, "SELECT 1; DELETE FROM t"),
    (Dialect::Postgres, "SELECT 1;\nSELECT 2;\n"),
    (Dialect::Postgres, "SELECT 'a;'; SELECT $x$;$x$"),
    (Dialect::Postgres, "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END; DELETE FROM t"),
    (Dialect::MySql, "SELECT 1; DELETE FROM t"),
    (Dialect::MySql, "DELIMITER //\nSELECT 1 //\nDELETE FROM t //"),
    (Dialect::MySql, "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; END //\nDELIMITER ;\nSELECT 2"),
    (Dialect::MySql, "/*!40101 SET NAMES utf8mb4 */;\n/*!40014 SET FOREIGN_KEY_CHECKS=0 */;\nSELECT 1"),
    (Dialect::MySql, "SELECT 1 /*!; DELETE FROM t */"),
    (Dialect::MySql, "SELECT 1 /*! ; SELECT 2 */; SELECT 3"),
    (Dialect::MySql, "CREATE PROCEDURE p() BEGIN SELECT 1; END; DELETE FROM t"),
    (Dialect::TSql, "SELECT 1\nGO\nDELETE FROM t"),
    (Dialect::TSql, "SELECT 1\nGO 2\nSELECT 2"),
    (Dialect::TSql, "SELECT 1 SELECT 2; SELECT 3"),
    (Dialect::TSql, "CREATE PROCEDURE p AS BEGIN SELECT 1; DELETE FROM t; END\nGO\nSELECT 1"),
    (Dialect::TSql, "-- x\nGO\n"),
];

pub fn corpus(d: Dialect) -> &'static [&'static str] {
    match d {
        Dialect::Postgres => POSTGRES,
        Dialect::MySql => MYSQL,
        Dialect::TSql => TSQL,
    }
}

/// Every text of a dialect: its statements, the empty ones and its scripts.
pub fn texts(d: Dialect) -> Vec<String> {
    let mut out: Vec<String> = corpus(d).iter().chain(EMPTY).map(|s| s.to_string()).collect();
    out.extend(SCRIPTS.iter().filter(|(x, _)| *x == d).map(|(_, s)| s.to_string()));
    out
}

/// The whitespace tokens that can change without changing what the server reads: not on a line
/// a client command owns (`GO`, `DELIMITER`), and not the line break that ends a line comment.
fn spaces(src: &str, d: Dialect) -> Vec<std::ops::Range<usize>> {
    let toks = tokenize_in(src, d);
    let owns_line = |line: &str| {
        let w = line.trim().split_whitespace().next().unwrap_or("");
        w.eq_ignore_ascii_case("GO") || w.eq_ignore_ascii_case("DELIMITER")
    };
    toks.iter()
        .enumerate()
        .filter(|(_, t)| t.kind == TokenKind::Whitespace)
        .filter(|(i, t)| {
            let line_start = src[..t.start].rfind('\n').map_or(0, |p| p + 1);
            let line_end = src[t.end..].find('\n').map_or(src.len(), |p| t.end + p);
            let after_comment = i.checked_sub(1).is_some_and(|p| toks[p].kind == TokenKind::LineComment);
            !owns_line(&src[line_start..line_end]) && !after_comment && !src[t.range()].contains('\n')
        })
        .map(|(_, t)| t.range())
        .collect()
}

fn replace(src: &str, ranges: &[std::ops::Range<usize>], with: &str) -> String {
    let mut out = String::new();
    let mut at = 0;
    for r in ranges {
        out.push_str(&src[at..r.start]);
        out.push_str(with);
        at = r.end;
    }
    out.push_str(&src[at..]);
    out
}

fn recase(src: &str, d: Dialect, f: impl Fn(usize, &str) -> String) -> String {
    let mut out = String::new();
    let mut k = 0;
    for t in tokenize_in(src, d) {
        let s = t.text(src);
        if matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && s.is_ascii() {
            out.push_str(&f(k, s));
            k += 1;
        } else {
            out.push_str(s);
        }
    }
    out
}

/// Texts the server reads exactly as `src`: other spacing, comments for spaces, other case.
pub fn variations(src: &str, d: Dialect) -> Vec<String> {
    let sp = spaces(src, d);
    let mut out = vec![
        replace(src, &sp, "/**/"),
        replace(src, &sp, "/* ; */"),
        replace(src, &sp, "\n"),
        replace(src, &sp, "\t \r\n  "),
        replace(src, &sp, " -- ;\n"),
        format!("  \n{src}\n  "),
        format!("/* lead */ {src} -- tail\n"),
        recase(src, d, |_, s| s.to_ascii_uppercase()),
        recase(src, d, |_, s| s.to_ascii_lowercase()),
        recase(src, d, |k, s| s.chars().enumerate().map(|(i, c)| if (i + k) % 2 == 0 { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() }).collect()),
    ];
    match d {
        Dialect::MySql => out.push(replace(src, &sp, " # ;\n")),
        _ => out.push(replace(src, &sp, "/* a /* ; */ b */")),
    }
    out.retain(|v| v != src);
    out
}

/// Two statements joined so the server reads both, one after the other.
pub fn joins(a: &str, b: &str, d: Dialect) -> Vec<String> {
    let mut out = vec![format!("{a}\n;\n{b}"), format!("{a}\n; ;{b}")];
    if d == Dialect::TSql {
        out.push(format!("{a}\nGO\n{b}"));
    }
    out
}

/// The significant tokens of `src` (comments and spacing aside).
pub fn sig(src: &str, d: Dialect) -> Vec<String> {
    tokenize_in(src, d).into_iter().filter(|t| !t.kind.is_trivia()).map(|t| t.text(src).to_string()).collect()
}

/// An allowlist entry made of a statement: its values become `?`.
pub fn entry_of(src: &str, d: Dialect) -> String {
    let mut out = String::new();
    for t in tokenize_in(src, d) {
        match t.kind {
            TokenKind::String | TokenKind::Number | TokenKind::DollarString => out.push('?'),
            TokenKind::LineComment => out.push('\n'),
            TokenKind::BlockComment => out.push(' '),
            _ => out.push_str(t.text(src)),
        }
    }
    out.trim().trim_end_matches(';').trim().to_string()
}

/// A statement that fills an entry's `?` with `value`.
pub fn fill(entry: &str, value: &str) -> String {
    entry.replace('?', value)
}

/// Statements that try to pass for an allowed entry: more SQL in its values, after it, or inside
/// comments the server runs.
pub fn impostors(entry: &str, d: Dialect) -> Vec<String> {
    let mut out = vec![
        fill(entry, "1 OR 1=1"),
        fill(entry, "(SELECT 1)"),
        fill(entry, "1; DELETE FROM t"),
        fill(entry, "id"),
        fill(entry, "1 --"),
        format!("{} OR 1=1", fill(entry, "1")),
        format!("{} AND 1=1", fill(entry, "1")),
        format!("{}; DROP TABLE t", fill(entry, "1")),
        format!("{} RETURNING *", fill(entry, "1")),
    ];
    if d == Dialect::MySql {
        out.push(format!("{} /*! OR 1=1 */", fill(entry, "1")));
        out.push(format!("{} /*!99999 OR 1=1 */", fill(entry, "1")));
        out.push(fill(entry, "1 /*!; DROP TABLE t */"));
    }
    if d == Dialect::TSql {
        out.push(format!("{} DELETE FROM t", fill(entry, "1")));
    }
    out
}
