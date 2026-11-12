//! MySQL and the servers that speak its protocol (MariaDB, Percona Server, Vitess). See
//! live_versions.rs. What a release lacks is asserted as a clean failure, never skipped: a test
//! that returns early on an old server would pass on a broken one too.

use super::*;
use dataman_core::Session;
use dataman_core::bounded::{Limits, Mode};
use dataman_core::schema::{ConstraintKind, RelKind};
use dataman_core::server::Flavor;

fn mysql() -> Option<Target> {
    target(Engine::MySql)
}

impl Target {
    fn mariadb(&self) -> bool {
        self.c.info.is_mariadb()
    }

    /// At least this release (of MariaDB's numbering on MariaDB).
    fn at_least(&self, v: (u32, u32, u32)) -> bool {
        self.c.info.release >= v
    }

    fn vitess(&self) -> bool {
        self.c.info.flavor == Flavor::Vitess
    }

    /// A database of the test's own, empty, on a session that stays on it, dropped when the
    /// test ends. On Vitess each one is a keyspace of its own, which vttestserver drops halfway
    /// ("database not found", a closed connection): there they stay until the server goes.
    fn scratch(&self, name: &str) -> Scratch<'_> {
        self.ok(&format!("DROP DATABASE IF EXISTS {name}"));
        self.ok(&format!("CREATE DATABASE {name}"));
        Scratch { t: self, name: name.to_string(), s: self.c.session(Some(name)) }
    }

    pub(crate) fn on(&self, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
        let rs = s.execute(sql);
        rs.fetch_all();
        self.e.core.block_on(async { tokio::time::timeout(Duration::from_secs(120), rs.wait_finished()).await.expect("timeout") });
        assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
        let (messages, _) = rs.messages();
        assert!(!messages.iter().any(|m| m.contains("did not decode")), "{sql}: {messages:?}");
        rs
    }

    pub(crate) fn fails_on(&self, s: &Arc<Session>, sql: &str) -> ErrorInfo {
        let rs = s.execute(sql);
        rs.fetch_all();
        self.e.core.block_on(async { tokio::time::timeout(Duration::from_secs(120), rs.wait_finished()).await.expect("timeout") });
        assert_eq!(rs.phase(), Phase::Failed, "{sql} ran");
        rs.status().error.expect("an error")
    }
}

struct Scratch<'a> {
    t: &'a Target,
    name: String,
    s: Arc<Session>,
}

impl std::ops::Deref for Scratch<'_> {
    type Target = Arc<Session>;
    fn deref(&self) -> &Arc<Session> {
        &self.s
    }
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        if self.t.vitess() {
            return;
        }
        let rs = self.t.run(&format!("DROP DATABASE IF EXISTS {}", self.name));
        if rs.phase() != Phase::Done && !std::thread::panicking() {
            panic!("{} stays: {:?}", self.name, rs.status().error);
        }
    }
}

pub(crate) fn cells(rs: &ResultSet) -> Vec<Vec<Option<String>>> {
    let cols = rs.columns().map_or(0, |c| c.len());
    let s = rs.snapshot();
    (0..s.rows()).map(|r| (0..cols).map(|c| s.text(r, c).map(|t| t.into_owned())).collect()).collect()
}

pub(crate) fn strs(row: &[Option<String>]) -> Vec<&str> {
    row.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect()
}

/// Every result set a statement sent, the first one included.
pub(crate) fn sets(t: &Target, first: Arc<ResultSet>) -> Vec<Arc<ResultSet>> {
    let mut out = vec![first];
    loop {
        let cur = out.last().unwrap().clone();
        cur.fetch_all();
        t.e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), cur.wait_finished()).await.expect("timeout") });
        assert_eq!(cur.phase(), Phase::Done, "{}: {:?}", cur.sql, cur.status().error);
        match cur.next_result() {
            Some(n) => out.push(n),
            None => return out,
        }
    }
}

const SALES: &str = "CREATE TABLE sales (region VARCHAR(8), product VARCHAR(8), qty INT, price DECIMAL(10,2))";
const SALES_ROWS: &str = "INSERT INTO sales VALUES ('east','a',1,1.50),('east','b',2,2.25),('west','a',3,1.50),('west','a',4,1.75),('north','c',NULL,9.99)";

#[test]
fn rollup_group_concat_and_variables() {
    let Some(t) = mysql() else { return };
    let s = t.scratch("dataman_v_agg");
    t.on(&s, SALES);
    t.on(&s, SALES_ROWS);
    let rs = t.on(&s, "SELECT region, SUM(qty), COUNT(qty), GROUP_CONCAT(DISTINCT product ORDER BY product DESC SEPARATOR '|'), SUM(qty * price) FROM sales GROUP BY region WITH ROLLUP");
    let rows = cells(&rs);
    let got: Vec<Vec<&str>> = rows.iter().map(|r| strs(r)).collect();
    assert_eq!(
        got,
        [
            ["east", "3", "2", "b|a", "6.00"],
            ["north", "NULL", "0", "c", "NULL"],
            ["west", "7", "2", "a", "11.50"],
            ["NULL", "10", "4", "c|b|a", "17.50"],
        ]
    );
    // GROUPING() is MySQL 8.0.1's; MariaDB has none.
    let grouping = "SELECT region, product, GROUPING(region), GROUPING(product), SUM(qty) FROM sales GROUP BY region, product WITH ROLLUP";
    if t.mariadb() || !t.at_least((8, 0, 1)) {
        let err = t.fails_on(&s, grouping);
        assert!(err.code.is_some(), "{err:?}");
    } else {
        let rows = cells(&t.on(&s, grouping));
        assert_eq!(strs(rows.last().unwrap()), ["NULL", "NULL", "1", "1", "10"]);
        assert_eq!(rows.len(), 4 + 3 + 1, "{rows:?}");
    }
    t.on(&s, "SET @total = 0, @label = 'sum: '");
    let rows = cells(&t.on(&s, "SELECT CONCAT(@label, SUM(qty)), @total IS NULL FROM sales"));
    assert_eq!(strs(&rows[0]), ["sum: 10", "0"]);
    // A user variable comes back as the type it holds.
    t.on(&s, "SET @d = 1.25, @i = 7, @b = X'00FF'");
    let rs = t.on(&s, "SELECT @d * 2, @i + 1, HEX(@b), @nothing");
    let row = cells(&rs).remove(0);
    // A DECIMAL variable's scale is the server's own (MySQL 8 sends 30 digits, others 2).
    assert_eq!(row[0].as_deref().map(|d| d.trim_end_matches('0')), Some("2.5"));
    assert_eq!(strs(&row[1..]), ["8", "00FF", "NULL"]);
}

#[test]
fn windows_and_recursive_ctes_or_a_clean_no() {
    let Some(t) = mysql() else { return };
    let s = t.scratch("dataman_v_win");
    t.on(&s, SALES);
    t.on(&s, SALES_ROWS);
    let windows = "SELECT region, qty, \
           SUM(qty) OVER (PARTITION BY region ORDER BY qty ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
           ROW_NUMBER() OVER w, RANK() OVER w, COALESCE(LAG(qty) OVER w, -1), \
           FIRST_VALUE(product) OVER (ORDER BY qty DESC ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
           NTILE(2) OVER w, CUME_DIST() OVER w \
         FROM sales WHERE qty IS NOT NULL WINDOW w AS (ORDER BY qty) ORDER BY qty";
    let recursive = "WITH RECURSIVE fib (n, a, b) AS (SELECT 1, CAST(0 AS SIGNED), CAST(1 AS SIGNED) UNION ALL SELECT n + 1, b, a + b FROM fib WHERE n < 40) SELECT n, a FROM fib ORDER BY n DESC LIMIT 1";
    // MySQL 8.0, MariaDB 10.2.
    if !t.mariadb() && !t.at_least((8, 0, 0)) {
        for sql in [windows, recursive] {
            let err = t.fails_on(&s, sql);
            assert_eq!(err.code.as_deref(), Some("1064"), "{sql}: {err:?}");
        }
        return;
    }
    let rows = cells(&t.on(&s, windows));
    let got: Vec<Vec<&str>> = rows.iter().map(|r| strs(r)).collect();
    assert_eq!(got[0][..8], ["east", "1", "1", "1", "1", "-1", "b", "1"]);
    assert_eq!(got[3][..8], ["west", "4", "7", "4", "4", "3", "a", "2"]);
    // CUME_DIST is a DOUBLE on MySQL, a DECIMAL on MariaDB.
    assert_eq!(got.iter().map(|r| r[8].parse::<f64>().unwrap()).collect::<Vec<_>>(), [0.25, 0.5, 0.75, 1.0]);
    let rows = cells(&t.on(&s, recursive));
    assert_eq!(strs(&rows[0]), ["40", "63245986"]);
    // A frame over a range of values.
    let rows = cells(&t.on(&s, "SELECT qty, SUM(qty) OVER (ORDER BY qty RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM sales WHERE qty IS NOT NULL ORDER BY qty"));
    assert_eq!(rows.iter().map(|r| r[1].clone().unwrap()).collect::<Vec<_>>(), ["3", "6", "9", "7"]);
}

#[test]
fn json_functions_by_release() {
    let Some(t) = mysql() else { return };
    let s = t.scratch("dataman_v_json");
    t.on(&s, SALES);
    t.on(&s, SALES_ROWS);
    // JSON_ARRAYAGG/JSON_OBJECTAGG: MySQL 5.7.22, MariaDB 10.5.
    let agg = "SELECT JSON_ARRAYAGG(qty), JSON_OBJECTAGG(product, qty) FROM sales WHERE region = 'east'";
    if t.at_least(if t.mariadb() { (10, 5, 0) } else { (5, 7, 22) }) {
        let rows = cells(&t.on(&s, agg));
        let arr: serde_json::Value = serde_json::from_str(rows[0][0].as_deref().unwrap()).unwrap();
        let obj: serde_json::Value = serde_json::from_str(rows[0][1].as_deref().unwrap()).unwrap();
        assert_eq!((arr, obj), (serde_json::json!([1, 2]), serde_json::json!({"a": 1, "b": 2})));
    } else {
        t.fails_on(&s, agg);
    }
    // JSON_TABLE: MySQL 8.0.4, MariaDB 10.6; vtgate plans none.
    let table = "SELECT jt.id, jt.name, jt.tag FROM JSON_TABLE('[{\"id\": 1, \"name\": \"x\", \"tags\": [\"p\", \"q\"]}, {\"id\": 2, \"name\": null}]', \
                 '$[*]' COLUMNS (id INT PATH '$.id', name VARCHAR(10) PATH '$.name', NESTED PATH '$.tags[*]' COLUMNS (tag VARCHAR(5) PATH '$'))) AS jt ORDER BY jt.id, jt.tag";
    if t.at_least(if t.mariadb() { (10, 6, 0) } else { (8, 0, 4) }) && !t.vitess() {
        let rows = cells(&t.on(&s, table));
        let got: Vec<Vec<&str>> = rows.iter().map(|r| strs(r)).collect();
        assert_eq!(got, [["1", "x", "p"], ["1", "x", "q"], ["2", "NULL", "NULL"]]);
    } else {
        t.fails_on(&s, table);
    }
    let rows = cells(&t.on(&s, "SELECT JSON_EXTRACT('{\"a\": [1, {\"b\": \"é\"}]}', '$.a[1].b'), JSON_UNQUOTE(JSON_EXTRACT('{\"a\": \"x\\\\ny\"}', '$.a'))"));
    assert_eq!(strs(&rows[0]), ["\"é\"", "x\ny"]);
}

#[test]
fn a_call_with_several_result_sets() {
    let Some(t) = mysql() else { return };
    let s = t.scratch("dataman_v_call");
    t.on(&s, SALES);
    t.on(&s, SALES_ROWS);
    t.on(&s, "CREATE PROCEDURE two_sets(IN r VARCHAR(8)) BEGIN SELECT product, qty FROM sales WHERE region = r ORDER BY product; SELECT COUNT(*) AS n, SUM(price) FROM sales; END");
    // vtgate refuses a procedure's several sets (and says so).
    if t.vitess() {
        assert_eq!(t.fails_on(&s, "CALL two_sets('east')").code.as_deref(), Some("1235"));
        return;
    }
    let all = sets(&t, s.execute("CALL two_sets('east')"));
    assert_eq!(all.len(), 2, "{:?}", all.iter().map(|r| r.columns()).collect::<Vec<_>>());
    assert_eq!(cells(&all[0]).iter().map(|r| strs(r).join(",")).collect::<Vec<_>>(), ["a,1", "b,2"]);
    assert_eq!(strs(&cells(&all[1])[0]), ["5", "16.99"]);
    // A statement batch: two selects and the DML between them.
    let all = sets(&t, s.execute("SELECT 1 AS a; UPDATE sales SET qty = qty + 1 WHERE region = 'east'; SELECT SUM(qty) AS b FROM sales"));
    let last = cells(all.last().unwrap());
    assert_eq!(strs(&last[0]), ["12"], "{} sets", all.len());
}

#[test]
fn every_type_family_decodes() {
    let Some(t) = mysql() else { return };
    let s = t.scratch("dataman_v_types");
    let json = if t.mariadb() { "LONGTEXT CHECK (JSON_VALID(j))" } else { "JSON" };
    t.on(
        &s,
        &format!(
            "CREATE TABLE ty (id INT PRIMARY KEY, d65 DECIMAL(65,30), neg DECIMAL(5,2), b3 BIT(3), b64 BIT(64), y YEAR, e ENUM('a','b'), st SET('x','y','z'), \
               g GEOMETRY, vb VARBINARY(4), bn BINARY(2), dt DATETIME(6), ts TIMESTAMP(3) NULL, tm TIME(2), d DATE, f FLOAT, dbl DOUBLE, \
               ub BIGINT UNSIGNED, ti TINYINT, mi MEDIUMINT UNSIGNED, j {json}, cs VARCHAR(10) COLLATE utf8mb4_bin, tx TEXT, bl BLOB)"
        ),
    );
    t.on(
        &s,
        "INSERT INTO ty VALUES (1, '12345678901234567890123456789012345.123456789012345678901234567890', -999.99, b'101', b'1111111111111111111111111111111111111111111111111111111111111111', \
           2155, 'b', 'z,x', ST_GeomFromText('POINT(1 2)'), X'00FF', 'ab', '2024-02-29 23:59:59.999999', '2038-01-19 03:14:07.999', '-838:59:59.00', '1000-01-01', \
           1.5, 1e300, 18446744073709551615, -128, 16777215, '{\"a\": [1, 2]}', 'AbC', 'tëxt', X'DEADBEEF'), \
         (2, NULL, NULL, NULL, NULL, NULL, NULL, '', NULL, X'', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
    );
    let rs = t.on(&s, "SELECT * FROM ty ORDER BY id");
    let rows = cells(&rs);
    assert_eq!(
        strs(&rows[0]),
        [
            "1",
            "12345678901234567890123456789012345.123456789012345678901234567890",
            "-999.99",
            "5",
            "18446744073709551615",
            "2155",
            "b",
            "x,z",
            "\\x000000000101000000000000000000f03f0000000000000040",
            "\\x00ff",
            "\\x6162",
            "2024-02-29 23:59:59.999999",
            "2038-01-19 03:14:07.999",
            "-838:59:59",
            "1000-01-01",
            "1.5",
            "1e+300",
            "18446744073709551615",
            "-128",
            "16777215",
            "{\"a\": [1, 2]}",
            "AbC",
            "tëxt",
            "\\xdeadbeef",
        ]
    );
    let mut nulls = vec!["NULL"; 24];
    nulls[0] = "2";
    nulls[7] = "";
    nulls[9] = "\\x";
    assert_eq!(strs(&rows[1]), nulls);
    // The same values through the text protocol (a statement the server won't prepare).
    let rs = t.on(&s, "SELECT * FROM ty ORDER BY id; DO 0");
    assert_eq!(cells(&rs), rows, "text and binary protocol differ");
    // A binary collation compares bytes.
    assert_eq!(cells(&t.on(&s, "SELECT COUNT(*) FROM ty WHERE cs = 'abc'"))[0][0].as_deref(), Some("0"));
    assert_eq!(cells(&t.on(&s, "SELECT ST_AsText(g), ST_SRID(g) FROM ty WHERE id = 1"))[0], [Some("POINT(1 2)".into()), Some("0".into())]);
}

#[test]
fn zero_dates_and_odd_values() {
    let Some(t) = mysql() else { return };
    let s = t.scratch("dataman_v_zero");
    t.on(&s, "SET SESSION sql_mode = ''");
    t.on(&s, "CREATE TABLE z (d DATE, dt DATETIME, ts TIMESTAMP NULL, y YEAR)");
    t.on(&s, "INSERT INTO z VALUES ('0000-00-00', '0000-00-00 00:00:00', '0000-00-00 00:00:00', 0), ('2024-02-00', '2024-00-10 10:00:00', NULL, 1901)");
    let rows = cells(&t.on(&s, "SELECT * FROM z"));
    assert_eq!(strs(&rows[0]), ["0000-00-00", "0000-00-00 00:00:00", "0000-00-00 00:00:00", "0"]);
    assert_eq!(strs(&rows[1]), ["2024-02-00", "2024-00-10 10:00:00", "NULL", "1901"]);
    let rows = cells(&t.on(&s, "SELECT 0x41, x'', CHAR(0), 1/0, CAST(-1 AS UNSIGNED), -0.0, 1e-300 * 1e-300, NULL, ''"));
    assert_eq!(strs(&rows[0]), ["\\x41", "\\x", "\\x00", "NULL", "18446744073709551615", "0.0", "0", "NULL", ""]);
    let rows = cells(&t.on(&s, "SELECT 1 AS a, 2 AS a, 'x' AS `select`"));
    assert_eq!((strs(&rows[0]), t.on(&s, "SELECT 1 AS a, 2 AS a").columns().unwrap().len()), (vec!["1", "2", "x"], 2));
    let err = t.fails_on(&s, "SELECT CAST(0 AS UNSIGNED) - 1");
    assert_eq!(err.code.as_deref(), Some("1690"), "{err:?}");
    let err = t.fails_on(&s, "SELECT 9223372036854775807 + 1");
    assert_eq!(err.code.as_deref(), Some("1690"), "{err:?}");
}

#[test]
fn constraints_invisible_and_generated_columns_in_the_catalog() {
    let Some(t) = mysql() else { return };
    let db = "dataman_v_cat";
    let s = t.scratch(db);
    // CHECK constraints are kept from MySQL 8.0.16 and MariaDB 10.2.1; earlier servers parse and drop them.
    let checks = t.at_least(if t.mariadb() { (10, 2, 1) } else { (8, 0, 16) });
    // INVISIBLE columns: MySQL 8.0.23, MariaDB 10.3.3.
    let invisible = t.at_least(if t.mariadb() { (10, 3, 3) } else { (8, 0, 23) });
    let hidden = if invisible { " INVISIBLE" } else { "" };
    t.on(
        &s,
        &format!(
            "CREATE TABLE parent (id INT PRIMARY KEY, code VARCHAR(8) NOT NULL UNIQUE, n INT, secret INT{hidden} DEFAULT 7, \
               doubled INT AS (n * 2) STORED, CONSTRAINT n_positive CHECK (n > 0))"
        ),
    );
    t.on(&s, "CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, CONSTRAINT child_parent FOREIGN KEY (parent_id) REFERENCES parent (id) ON DELETE SET NULL)");
    let x = t.e.core.block_on(t.c.table_extras(db, db, "parent")).unwrap();
    let cons = x.constraints.expect("the constraints were read");
    let kinds: Vec<(ConstraintKind, &str)> = cons.iter().map(|c| (c.kind, c.name.as_str())).collect();
    let mut want = vec![(ConstraintKind::Primary, "PRIMARY"), (ConstraintKind::Unique, "code")];
    if checks {
        want.push((ConstraintKind::Check, "n_positive"));
    }
    want.sort();
    let mut got = kinds.clone();
    got.sort();
    assert_eq!(got, want, "{cons:?}");
    if checks {
        let def = cons.iter().find(|c| c.kind == ConstraintKind::Check).unwrap().definition.clone().unwrap();
        assert!(def.starts_with("CHECK (") && def.contains("> 0"), "{def}");
    }
    let x = t.e.core.block_on(t.c.table_extras(db, db, "child")).unwrap();
    let fk = x.constraints.unwrap().into_iter().find(|c| c.kind == ConstraintKind::Foreign).expect("the foreign key");
    assert_eq!(fk.definition.as_deref(), Some("FOREIGN KEY (parent_id) REFERENCES parent(id) ON DELETE SET NULL"));
    let cols = t.e.core.block_on(t.c.columns(db, db, "parent")).unwrap();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "code", "n", "secret", "doubled"], "an invisible column is still the table's");
    let doubled = cols.iter().find(|c| c.name == "doubled").unwrap();
    assert!(doubled.default.as_deref().is_some_and(|d| d.starts_with("GENERATED (") && d.contains('2')), "{doubled:?}");
    assert_eq!(cols.iter().find(|c| c.name == "secret").unwrap().default.as_deref(), Some("7"));
    let rels = t.e.core.block_on(t.c.relations(db, db)).unwrap();
    assert_eq!(rels.iter().map(|r| (r.name.as_str(), r.kind)).collect::<Vec<_>>(), [("child", RelKind::Table), ("parent", RelKind::Table)]);
    let ddl = t.e.core.block_on(t.c.table_ddl(db, db, "parent")).unwrap();
    assert!(ddl.contains("CREATE TABLE `parent`"), "{ddl}");
    // What SELECT * shows: no invisible column.
    t.on(&s, "INSERT INTO parent (id, code, n) VALUES (1, 'a', 5)");
    let rs = t.on(&s, "SELECT * FROM parent");
    let shown = if invisible { vec!["1", "a", "5", "10"] } else { vec!["1", "a", "5", "7", "10"] };
    assert_eq!(strs(&cells(&rs)[0]), shown);
}

#[test]
fn the_agent_read_path_bounds_and_types() {
    let Some(t) = mysql() else { return };
    let db = "dataman_v_mcp";
    let s = t.scratch(db);
    t.on(&s, SALES);
    t.on(&s, SALES_ROWS);
    let read = |sql: &str, lim: Limits| t.e.core.block_on(t.c.bounded(Some(db), sql, vec![], Mode::Read, lim, std::future::pending())).unwrap();
    let o = read("SELECT region, SUM(qty) AS q, SUM(price) AS p FROM sales GROUP BY region WITH ROLLUP", Limits::default());
    assert_eq!(o.rows.len(), 4);
    // SUM of integers is a DECIMAL: a string, as every exact number past an i64 is.
    assert_eq!(o.rows[3], [serde_json::Value::Null, serde_json::json!("10"), serde_json::json!("16.99")]);
    let o = read("SELECT qty FROM sales ORDER BY qty IS NULL, qty", Limits { max_rows: 2, ..Default::default() });
    assert_eq!((o.rows.len(), o.truncated.is_some()), (2, true));
    let o = read("SELECT CAST(18446744073709551615 AS UNSIGNED) AS big, b'101' + 0 AS bits, CAST('2024-01-02' AS DATE) AS d", Limits::default());
    assert_eq!(o.rows[0], [serde_json::json!("18446744073709551615"), serde_json::json!(5), serde_json::json!("2024-01-02")]);
}

#[test]
fn mariadb_sequences_versioned_tables_and_its_own_types() {
    let Some(t) = mysql() else { return };
    if !t.mariadb() {
        return;
    }
    let db = "dataman_v_maria";
    let s = t.scratch(db);
    t.on(&s, "CREATE SEQUENCE seq START WITH 10 INCREMENT BY 5");
    let rows = cells(&t.on(&s, "SELECT NEXTVAL(seq), NEXTVAL(seq), LASTVAL(seq)"));
    assert_eq!(strs(&rows[0]), ["10", "15", "15"]);
    t.on(&s, "CREATE TABLE hist (id INT PRIMARY KEY, v VARCHAR(10)) WITH SYSTEM VERSIONING");
    t.on(&s, "INSERT INTO hist VALUES (1, 'old')");
    t.on(&s, "UPDATE hist SET v = 'new' WHERE id = 1");
    let rows = cells(&t.on(&s, "SELECT v, YEAR(row_end) > 2037 FROM hist FOR SYSTEM_TIME ALL ORDER BY row_start"));
    assert_eq!(rows.iter().map(|r| strs(r)).collect::<Vec<_>>(), [["old", "0"], ["new", "1"]]);
    let rels = t.e.core.block_on(t.c.relations(db, db)).unwrap();
    let names: Vec<&str> = rels.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["hist", "seq"], "{rels:?}");
    let cols = t.e.core.block_on(t.c.columns(db, db, "hist")).unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "v"], "row_start and row_end are invisible");
    // INET6 (10.5), UUID (10.7), INET4 (10.10): text on the wire, typed in the catalog.
    let mut types = vec![("i6", "INET6", "'::ffff:1.2.3.4'", "::ffff:1.2.3.4")];
    if t.at_least((10, 7, 0)) {
        types.push(("u", "UUID", "'123e4567-e89b-12d3-a456-426655440000'", "123e4567-e89b-12d3-a456-426655440000"));
    }
    if t.at_least((10, 10, 0)) {
        types.push(("i4", "INET4", "'10.0.0.1'", "10.0.0.1"));
    }
    let defs: Vec<String> = types.iter().map(|(c, ty, ..)| format!("{c} {ty}")).collect();
    t.on(&s, &format!("CREATE TABLE own (id INT PRIMARY KEY, {})", defs.join(", ")));
    let vals: Vec<&str> = types.iter().map(|(_, _, v, _)| *v).collect();
    t.on(&s, &format!("INSERT INTO own VALUES (1, {})", vals.join(", ")));
    let rows = cells(&t.on(&s, "SELECT * FROM own"));
    let want: Vec<&str> = std::iter::once("1").chain(types.iter().map(|(.., shown)| *shown)).collect();
    assert_eq!(strs(&rows[0]), want);
    let cols = t.e.core.block_on(t.c.columns(db, db, "own")).unwrap();
    assert_eq!(cols.iter().skip(1).map(|c| c.type_name.to_ascii_uppercase()).collect::<Vec<_>>(), types.iter().map(|(_, ty, ..)| ty.to_string()).collect::<Vec<_>>());
    // RETURNING (10.5) gives the rows a write touched.
    let rows = cells(&t.on(&s, "INSERT INTO hist VALUES (2, 'x'), (3, 'y') RETURNING id, UPPER(v)"));
    assert_eq!(rows.iter().map(|r| strs(r).join(",")).collect::<Vec<_>>(), ["2,X", "3,Y"]);
    let rows = cells(&t.on(&s, "DELETE FROM hist WHERE id > 1 RETURNING id"));
    assert_eq!(rows.len(), 2);
    // INTERSECT and EXCEPT (10.3).
    let rows = cells(&t.on(&s, "SELECT 1 UNION SELECT 2 EXCEPT SELECT 2 INTERSECT SELECT 2"));
    assert_eq!(rows, [[Some("1".to_string())]]);
}

/// SHOW WARNINGS, SHOW ERRORS and GET DIAGNOSTICS read what the statement before them left: no
/// statement of Dataman's own may run between them (a prepare that fails leaves its own error).
#[test]
fn warnings_and_errors_are_the_previous_statements() {
    let Some(t) = mysql() else { return };
    // vtgate answers SHOW WARNINGS from its own session: a SELECT it evaluates itself leaves none.
    if t.vitess() {
        return;
    }
    let s = t.c.session(None);
    t.on(&s, "SELECT CAST('12abc' AS SIGNED)");
    let rows = cells(&t.on(&s, "SHOW WARNINGS"));
    assert_eq!(rows.iter().map(|r| r[1].clone().unwrap()).collect::<Vec<_>>(), ["1292"], "{rows:?}");
    assert_eq!(cells(&t.on(&s, "SHOW COUNT(*) WARNINGS"))[0][0].as_deref(), Some("1"));
    t.fails_on(&s, "SELECT * FROM mysql.no_such_table");
    let rows = cells(&t.on(&s, "SHOW ERRORS"));
    assert_eq!(rows.iter().map(|r| r[1].clone().unwrap()).collect::<Vec<_>>(), ["1146"], "{rows:?}");
    // After an error the transaction's state is asked for without clearing it: a DDL that fails
    // has committed first.
    let s = t.scratch("dataman_v_diag");
    t.on(&s, "CREATE TABLE d (id INT)");
    t.on(&s, "START TRANSACTION");
    t.on(&s, "INSERT INTO d VALUES (1)");
    assert_eq!(s.tx().status, dataman_core::TxStatus::Open);
    t.fails_on(&s, "CREATE TABLE d (id INT)");
    assert_eq!(s.tx().status, dataman_core::TxStatus::Idle, "the failed DDL committed");
    assert_eq!(cells(&t.on(&s, "SHOW ERRORS"))[0][1].as_deref(), Some("1050"));
    t.on(&s, "SELECT CAST('7x' AS SIGNED)");
    t.on(&s, "GET DIAGNOSTICS CONDITION 1 @code = MYSQL_ERRNO");
    assert_eq!(cells(&t.on(&s, "SELECT @code"))[0][0].as_deref(), Some("1292"));
}
