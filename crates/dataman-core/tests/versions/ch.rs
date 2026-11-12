//! ClickHouse. See live_versions.rs.

use super::*;
use dataman_core::bounded::{Limits, Mode};

fn cells(rows: &[Vec<Option<String>>]) -> Vec<Vec<&str>> {
    rows.iter().map(|r| r.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect()).collect()
}

impl Target {
    /// Each value as the grid shows it next to the server's own text for it (`toString`).
    fn ch_shown_as_server_says(&self, exprs: &[&str]) {
        let mut bad = Vec::new();
        for x in exprs {
            let rows = self.rows(&format!("SELECT {x}, toString({x})"));
            if rows[0][0] != rows[0][1] {
                bad.push(format!("{x}: shows {:?}, the server says {:?}", rows[0][0], rows[0][1]));
            }
        }
        assert!(bad.is_empty(), "{}\n{}", self.c.info.version, bad.join("\n"));
    }
}

#[test]
fn combinators_state_and_merge() {
    let Some(t) = target(Engine::ClickHouse) else { return };
    let rows = t.rows(
        "SELECT k, sumIf(v, v > 1), countIf(v % 2 = 0), avgOrNull(v), groupArray(v), uniqExact(v), argMax(v, -v), quantileExact(0.5)(v)
         FROM (SELECT number % 2 AS k, number AS v FROM numbers(6)) GROUP BY k ORDER BY k",
    );
    assert_eq!(cells(&rows), [["0", "6", "3", "2", "[0,2,4]", "3", "0", "2"], ["1", "8", "0", "3", "[1,3,5]", "3", "1", "3"]]);
    // A state is binary: merged, it is the number again.
    let rows = t.rows("SELECT sumMerge(s), uniqMerge(u) FROM (SELECT sumState(number) AS s, uniqState(number % 3) AS u FROM numbers(10))");
    assert_eq!(cells(&rows), [["45", "3"]]);
    let rows = t.rows("SELECT k, sumArray(a), groupUniqArrayArray(a) FROM (SELECT 1 AS k, [1, 2, 2] AS a UNION ALL SELECT 1, [3]) GROUP BY k");
    assert_eq!(rows[0][0].as_deref(), Some("1"));
    assert_eq!(rows[0][1].as_deref(), Some("8"));
    let rows = t.rows("SELECT number % 3 AS k, count() FROM numbers(7) GROUP BY k WITH ROLLUP ORDER BY k");
    assert_eq!(rows.len(), 4, "the rollup's total row is a row: {rows:?}");
}

/// WITH TOTALS sends its total row after a blank line in TabSeparated: it is one more row, not
/// the end of the result nor a broken row.
#[test]
fn with_totals_comes_back_whole() {
    let Some(t) = target(Engine::ClickHouse) else { return };
    let rows = t.rows("SELECT number % 2 AS k, sum(number) FROM numbers(5) GROUP BY k WITH TOTALS ORDER BY k");
    assert_eq!(cells(&rows), [["0", "6"], ["1", "4"], ["0", "10"]]);
}

#[test]
fn nested_types_and_low_cardinality() {
    let Some(t) = target(Engine::ClickHouse) else { return };
    t.ch_shown_as_server_says(&[
        "[[1, 2], [3]]",
        "[['a\\'b', 'c\\td'], []]",
        "map('k', [1, 2], 'é', [])",
        "tuple(1, 'x', [NULL, 2])",
        "CAST(tuple(1, 'a'), 'Tuple(n UInt8, s String)')",
        "[tuple(1, map('a', NULL))]",
        "toLowCardinality('lc')",
        "[toLowCardinality('a'), toLowCardinality('b')]",
        "CAST([1, NULL], 'Array(Nullable(Int8))')",
        "toFixedString('ab', 4)",
        "toIPv4('10.0.0.1')",
        "toIPv6('::1')",
        "toUUID('00000000-0000-0000-0000-000000000001')",
        "CAST('b', 'Enum8(\\'a\\' = 1, \\'b\\' = 2)')",
    ]);
    let rows = t.rows("SELECT n.a, n.b FROM (SELECT [1, 2] AS `n.a`, ['x', 'y'] AS `n.b`) ARRAY JOIN n");
    assert_eq!(cells(&rows), [["1", "x"], ["2", "y"]]);
}

#[test]
fn numbers_at_their_extremes() {
    let Some(t) = target(Engine::ClickHouse) else { return };
    t.ch_shown_as_server_says(&[
        "toDecimal256('-123456789012345678901234567890.1234567890123456789', 20)",
        "toDecimal128('0.00000000000000000001', 20)",
        "toDecimal32(-0.5, 2)",
        "toUInt64(18446744073709551615)",
        "toInt128('-170141183460469231731687303715884105728')",
        "toUInt256('115792089237316195423570985008687907853269984665640564039457584007913129639935')",
        "nan",
        "-inf",
        "toFloat32(0.1)",
        "1e308 * 10",
        "toFloat64(-0.)",
    ]);
}

#[test]
fn times_in_their_zones() {
    let Some(t) = target(Engine::ClickHouse) else { return };
    t.ch_shown_as_server_says(&[
        "toDateTime64('2024-06-30 23:30:00.123456', 6, 'Asia/Kathmandu')",
        "toDateTime64('1900-01-01 00:00:00', 3, 'UTC')",
        "toDateTime64('2262-04-11 23:47:16.854775', 6, 'UTC')",
        "toDateTime('2024-03-31 02:30:00', 'Europe/Berlin')",
        "toDateTime('1970-01-01 00:00:00', 'America/St_Johns')",
        "toDate32('1900-01-01')",
        "toDate('2149-06-06')",
        "toIntervalDay(3)",
    ]);
}

/// A statement's own clauses: its SETTINGS, its FORMAT (shown by lines), and system tables of
/// every release.
#[test]
fn settings_formats_and_system_tables() {
    let Some(t) = target(Engine::ClickHouse) else { return };
    assert_eq!(t.one("SELECT getSetting('max_threads') SETTINGS max_threads = 3").as_deref(), Some("3"));
    let rows = t.rows("SELECT number, toString(number) AS s FROM numbers(2) FORMAT JSONEachRow");
    let text: String = rows.iter().map(|r| r[0].clone().unwrap_or_default()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("\"number\":0") || text.contains("\"number\":\"0\""), "{text}");
    for q in [
        "SELECT name, engine FROM system.databases ORDER BY name",
        "SELECT name, type FROM system.columns WHERE database = 'system' AND table = 'one'",
        "SELECT * FROM system.settings WHERE name = 'max_threads'",
        "SELECT * FROM system.functions WHERE name = 'sumIf'",
        "SELECT * FROM system.processes",
        "SELECT * FROM system.parts LIMIT 1",
        "SELECT * FROM system.data_type_families WHERE name = 'Decimal256'",
    ] {
        t.ok(q);
    }
    assert_eq!(t.one("SELECT count() FROM system.one").as_deref(), Some("1"));
}

/// The catalog the explorer reads, a table of every column kind, and the MCP read path on it.
#[test]
fn catalog_and_bounded_reads() {
    let Some(t) = target(Engine::ClickHouse) else { return };
    t.ok("DROP DATABASE IF EXISTS dv_ch");
    t.ok("CREATE DATABASE dv_ch");
    t.ok(
        "CREATE TABLE dv_ch.t (id UInt64, lc LowCardinality(Nullable(String)), d256 Decimal256(10), at DateTime64(3, 'Asia/Kathmandu'), \
         m Map(String, Array(Nullable(Int32))), tup Tuple(a UInt8, b String), n Nested(x UInt8, y String), \
         twice UInt64 MATERIALIZED id * 2, al String ALIAS toString(id)) ENGINE = MergeTree ORDER BY id",
    );
    t.ok("INSERT INTO dv_ch.t (id, lc, d256, at, m, tup, `n.x`, `n.y`) VALUES (1, NULL, -1.5, '2024-01-01 00:00:00.5', map('a', [1, NULL]), (7, 'q'), [1, 2], ['a', 'b'])");
    let (e, c) = (&t.e, &t.c);
    let rels = e.core.block_on(c.relations("dv_ch", "dv_ch")).unwrap();
    assert_eq!(rels.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["t"]);
    let cols = e.core.block_on(c.columns("dv_ch", "dv_ch", "t")).unwrap();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "lc", "d256", "at", "m", "tup", "n.x", "n.y", "twice", "al"]);
    assert_eq!(cols[2].type_name, "Decimal(76, 10)");
    let generated = e.core.block_on(c.generated_columns("dv_ch", std::future::pending())).unwrap();
    let twice = generated.iter().find(|g| g.table == "t" && g.column == "twice").expect("MATERIALIZED read as generated");
    assert!(twice.from.contains(&"id".to_string()), "{twice:?}");
    let out = e.core.block_on(c.bounded(Some("dv_ch"), "SELECT id, lc, d256, at, m, tup, n.x FROM dv_ch.t", vec![], Mode::Read, Limits::default(), std::future::pending())).unwrap();
    assert_eq!(out.rows.len(), 1);
    let row: Vec<String> = out.rows[0].iter().map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)).collect();
    assert_eq!(row, ["1", "null", "-1.5", "2024-01-01 00:00:00.500", "{'a':[1,NULL]}", "(7,'q')", "[1,2]"]);
    let ddl = e.core.block_on(c.table_ddl("dv_ch", "dv_ch", "t")).unwrap();
    assert!(ddl.contains("MATERIALIZED") && ddl.contains("Decimal(76, 10)"), "{ddl}");
    t.ok("DROP DATABASE dv_ch");
}
