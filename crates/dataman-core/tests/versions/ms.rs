//! SQL Server's releases. See live_versions.rs. What a release lacks is asserted as a clean
//! failure, never skipped.

use super::my::{cells, sets, strs};
use super::*;
use dataman_core::Session;
use dataman_core::bounded::{Limits, Mode};

fn mssql() -> Option<Target> {
    target(Engine::SqlServer)
}

/// SQL Server's major version: 14 is 2017, 15 2019, 16 2022, 17 2025.
fn major(t: &Target) -> u32 {
    t.c.info.release.0
}

/// A database of the test's own, empty, on a session that stays in it.
fn scratch(t: &Target, name: &str) -> Arc<Session> {
    t.ok(&format!(
        "IF DB_ID(N'{name}') IS NOT NULL BEGIN ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}]; END"
    ));
    t.ok(&format!("CREATE DATABASE [{name}]"));
    t.c.session(Some(name))
}

const SALES: &str = "CREATE TABLE sales (region NVARCHAR(8), product NVARCHAR(8), qty INT, price DECIMAL(10,2))";
const SALES_ROWS: &str = "INSERT INTO sales VALUES (N'east',N'a',1,1.50),(N'east',N'b',2,2.25),(N'west',N'a',3,1.50),(N'west',N'a',4,1.75),(N'north',N'c',NULL,9.99)";

#[test]
fn cube_grouping_sets_pivot_and_string_agg() {
    let Some(t) = mssql() else { return };
    let s = scratch(&t, "dataman_v_agg");
    t.on(&s, SALES);
    t.on(&s, SALES_ROWS);
    let rows = cells(&t.on(
        &s,
        "SELECT region, product, SUM(qty) AS q, GROUPING_ID(region, product) AS g FROM sales \
         GROUP BY GROUPING SETS ((region, product), (region), ()) ORDER BY g, region, product",
    ));
    let got: Vec<Vec<&str>> = rows.iter().map(|r| strs(r)).collect();
    assert_eq!(got.last().unwrap(), &["NULL", "NULL", "10", "3"]);
    assert_eq!(got.len(), 4 + 3 + 1);
    let rows = cells(&t.on(&s, "SELECT COUNT(*) FROM (SELECT region, product FROM sales GROUP BY CUBE (region, product)) c"));
    assert_eq!(rows[0][0].as_deref(), Some("11"), "4 pairs, 3 regions, 3 products, the total");
    let rows = cells(&t.on(
        &s,
        "SELECT region, [a], [b], [c] FROM (SELECT region, product, qty FROM sales) s PIVOT (SUM(qty) FOR product IN ([a], [b], [c])) p ORDER BY region",
    ));
    let got: Vec<Vec<&str>> = rows.iter().map(|r| strs(r)).collect();
    assert_eq!(got, [["east", "1", "2", "NULL"], ["north", "NULL", "NULL", "NULL"], ["west", "7", "NULL", "NULL"]]);
    let rows = cells(&t.on(&s, "SELECT k, v FROM (SELECT 1 AS id, 10 AS x, 20 AS y) s UNPIVOT (v FOR k IN (x, y)) u ORDER BY k"));
    assert_eq!(rows.iter().map(|r| strs(r).join("=")).collect::<Vec<_>>(), ["x=10", "y=20"]);
    // STRING_AGG is 2017's.
    let agg = "SELECT region, STRING_AGG(product, N'|') WITHIN GROUP (ORDER BY product DESC) FROM sales GROUP BY region ORDER BY region";
    let rows = cells(&t.on(&s, agg));
    assert_eq!(rows.iter().map(|r| strs(r).join(":")).collect::<Vec<_>>(), ["east:b|a", "north:c", "west:a|a"]);
    let rows = cells(&t.on(
        &s,
        "SELECT qty, SUM(qty) OVER (ORDER BY qty ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), LAG(qty, 1, -1) OVER (ORDER BY qty), \
                PERCENT_RANK() OVER (ORDER BY qty), PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY qty) OVER () \
         FROM sales WHERE qty IS NOT NULL ORDER BY qty",
    ));
    let got: Vec<Vec<&str>> = rows.iter().map(|r| strs(r)).collect();
    assert_eq!(got[0], ["1", "3", "-1", "0", "2.5"]);
    assert_eq!(got[3], ["4", "7", "3", "1", "2.5"]);
    let rows = cells(&t.on(&s, "WITH f(n, a, b) AS (SELECT 1, CAST(0 AS BIGINT), CAST(1 AS BIGINT) UNION ALL SELECT n + 1, b, a + b FROM f WHERE n < 90) SELECT MAX(a) FROM f OPTION (MAXRECURSION 100)"));
    assert_eq!(rows[0][0].as_deref(), Some("1779979416004714189"));
}

#[test]
fn functions_by_release() {
    let Some(t) = mssql() else { return };
    // 2022: GREATEST/LEAST, GENERATE_SERIES, DATE_BUCKET, IS DISTINCT FROM.
    let modern = "SELECT GREATEST(1, 5, 3), LEAST(N'b', N'a'), (SELECT COUNT(*) FROM GENERATE_SERIES(1, 10)), \
                  DATE_BUCKET(WEEK, 1, CAST('2024-01-10' AS DATE)), CASE WHEN NULL IS NOT DISTINCT FROM NULL THEN 1 ELSE 0 END";
    if major(&t) >= 16 {
        assert_eq!(strs(&cells(&t.ok(modern))[0]), ["5", "a", "10", "2024-01-08", "1"]);
    } else {
        let err = t.failed(modern);
        assert!(err.code.as_deref().is_some_and(|c| c == "195" || c == "102" || c == "208"), "{err:?}");
    }
    // TRIM, CONCAT_WS and TRANSLATE are 2017's.
    assert_eq!(strs(&cells(&t.ok("SELECT TRIM(N'  x  '), CONCAT_WS(N',', 1, NULL, 2), TRANSLATE(N'2*[3+4]', N'[]', N'()')"))[0]), ["x", "1,2", "2*(3+4)"]);
}

#[test]
fn for_json_and_xml_come_back_whole() {
    let Some(t) = mssql() else { return };
    let s = scratch(&t, "dataman_v_json");
    t.on(&s, "CREATE TABLE doc (id INT PRIMARY KEY, name NVARCHAR(100))");
    t.on(&s, "INSERT INTO doc SELECT TOP (500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)), REPLICATE(N'é', 20) FROM sys.all_objects");
    // The server cuts a long FOR JSON into rows of 2033 characters each: read whole, they are one document.
    let rs = t.on(&s, "SELECT id, name FROM doc ORDER BY id FOR JSON PATH");
    let text: String = cells(&rs).into_iter().map(|r| r[0].clone().unwrap()).collect();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {} rows", rs.rows()));
    assert_eq!(v.as_array().map(|a| a.len()), Some(500));
    let rs = t.on(&s, "SELECT TOP 2 id FROM doc ORDER BY id FOR XML PATH('r'), ROOT('x')");
    let xml: String = cells(&rs).into_iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(xml, "<x><r><id>1</id></r><r><id>2</id></r></x>");
    let rows = cells(&t.on(&s, "SELECT JSON_VALUE(N'{\"a\":[1,{\"b\":\"é\"}]}', '$.a[1].b'), ISJSON(N'{}'), JSON_QUERY(N'{\"a\":[1,2]}', '$.a')"));
    assert_eq!(strs(&rows[0]), ["é", "1", "[1,2]"]);
    let rows = cells(&t.on(&s, "SELECT j.id, j.tag FROM OPENJSON(N'[{\"id\":1,\"tag\":\"x\"},{\"id\":2}]') WITH (id INT, tag NVARCHAR(5)) j"));
    assert_eq!(rows.iter().map(|r| strs(r).join(",")).collect::<Vec<_>>(), ["1,x", "2,NULL"]);
}

#[test]
fn output_messages_and_several_result_sets() {
    let Some(t) = mssql() else { return };
    let s = scratch(&t, "dataman_v_out");
    t.on(&s, SALES);
    t.on(&s, SALES_ROWS);
    let rows = cells(&t.on(&s, "UPDATE sales SET qty = qty * 10 OUTPUT deleted.qty, inserted.qty WHERE region = N'east'"));
    let mut got: Vec<String> = rows.iter().map(|r| strs(r).join("->")).collect();
    got.sort();
    assert_eq!(got, ["1->10", "2->20"]);
    t.on(&s, "CREATE PROCEDURE two_sets AS BEGIN SET NOCOUNT ON; PRINT N'first'; SELECT region FROM sales WHERE qty > 5 ORDER BY region; RAISERROR(N'between %d', 0, 1, 7) WITH NOWAIT; SELECT COUNT(*) AS n FROM sales; END");
    let all = sets(&t, s.execute("EXEC two_sets"));
    assert_eq!(all.len(), 2);
    assert_eq!(cells(&all[0]).iter().map(|r| strs(r).join("")).collect::<Vec<_>>(), ["east", "east"], "east's after the UPDATE");
    assert_eq!(cells(&all[1])[0][0].as_deref(), Some("5"));
    let messages: Vec<String> = all.iter().flat_map(|r| r.messages().0.iter().cloned().collect::<Vec<_>>()).collect();
    assert!(messages.iter().any(|m| m.contains("first")) && messages.iter().any(|m| m.contains("between 7")), "{messages:?}");
    // An error after a set: the set stays, the error is the statement's.
    let rs = s.execute("SELECT 1 AS a; SELECT 1 / 0 AS b");
    let first = rs.clone();
    first.fetch_all();
    t.e.core.block_on(first.wait_finished());
    let second = first.next_result().expect("the second statement's result");
    second.fetch_all();
    t.e.core.block_on(second.wait_finished());
    assert_eq!((first.phase(), second.phase()), (Phase::Done, Phase::Failed));
    assert_eq!(second.status().error.and_then(|e| e.code).as_deref(), Some("8134"));
}

#[test]
fn temporal_tables_and_odd_values() {
    let Some(t) = mssql() else { return };
    let s = scratch(&t, "dataman_v_temporal");
    t.on(
        &s,
        "CREATE TABLE hist (id INT PRIMARY KEY, v NVARCHAR(10), vf DATETIME2 GENERATED ALWAYS AS ROW START HIDDEN, vt DATETIME2 GENERATED ALWAYS AS ROW END HIDDEN, \
         PERIOD FOR SYSTEM_TIME (vf, vt)) WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = dbo.hist_history))",
    );
    t.on(&s, "INSERT INTO hist (id, v) VALUES (1, N'old')");
    t.on(&s, "UPDATE hist SET v = N'new' WHERE id = 1");
    let rows = cells(&t.on(&s, "SELECT v, YEAR(vt) FROM hist FOR SYSTEM_TIME ALL ORDER BY vf"));
    assert_eq!(rows.iter().map(|r| r[0].clone().unwrap()).collect::<Vec<_>>(), ["old", "new"]);
    assert_eq!(rows[1][1].as_deref(), Some("9999"));
    assert_eq!(cells(&t.on(&s, "SELECT * FROM hist"))[0].len(), 2, "hidden period columns stay out of SELECT *");
    let rels = t.e.core.block_on(t.c.relations("dataman_v_temporal", "dbo")).unwrap();
    let mut names: Vec<&str> = rels.iter().map(|r| r.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["hist", "hist_history"]);
    let rows = cells(&t.on(
        &s,
        "SELECT CAST(922337203685477.5807 AS MONEY), CAST(-214748.3648 AS SMALLMONEY), CAST('2024-02-29 23:59:59.9999999 +14:00' AS DATETIMEOFFSET(7)), \
                CAST('00:00:00.0000001' AS TIME(7)), CAST(0x0102 AS VARBINARY(MAX)), CAST(N'' AS NVARCHAR(MAX)), CAST(1 AS BIT), \
                CAST('1E3B4C5D-0000-0000-0000-000000000001' AS UNIQUEIDENTIFIER), CAST(12345678901234567890123456789012345678 AS DECIMAL(38,0)), \
                CAST(-1.5 AS REAL), CAST(N'x' AS SQL_VARIANT), hierarchyid::Parse('/1/2/').ToString(), geography::Point(47.65, -122.34, 4326).STAsText()",
    ));
    assert_eq!(
        strs(&rows[0]),
        [
            "922337203685477.5807",
            "-214748.3648",
            "2024-02-29 23:59:59.9999999 +14:00",
            "00:00:00.0000001",
            "\\x0102",
            "",
            "true",
            "1E3B4C5D-0000-0000-0000-000000000001",
            "12345678901234567890123456789012345678",
            "-1.5",
            "x",
            "/1/2/",
            "POINT (-122.34 47.65)",
        ]
    );
}

/// SQL Server 2025's own types: `json` and `vector` reach the client as their own TDS types (or
/// as nvarchar/varbinary to an older client); either way a value comes back, never a dead session.
#[test]
fn json_and_vector_types_on_2025() {
    let Some(t) = mssql() else { return };
    if major(&t) < 17 {
        let err = t.failed("SELECT CAST(N'{}' AS JSON)");
        assert!(err.code.is_some(), "{err:?}");
        return;
    }
    let s = scratch(&t, "dataman_v_2025");
    t.on(&s, "CREATE TABLE j (id INT PRIMARY KEY, d JSON, v VECTOR(3))");
    t.on(&s, "INSERT INTO j VALUES (1, N'{\"a\": [1, 2]}', CAST(N'[1.5, 2, -3]' AS VECTOR(3)))");
    let rows = cells(&t.on(&s, "SELECT id, d, v FROM j"));
    let d: serde_json::Value = serde_json::from_str(rows[0][1].as_deref().expect("the json value")).unwrap();
    assert_eq!(d, serde_json::json!({"a": [1, 2]}));
    assert!(rows[0][2].is_some(), "{rows:?}");
    assert_eq!(cells(&t.on(&s, "SELECT 7"))[0][0].as_deref(), Some("7"), "the session lives on");
}

#[test]
fn the_agent_read_path_on_odd_types() {
    let Some(t) = mssql() else { return };
    let db = "dataman_v_mcp";
    let s = scratch(&t, db);
    t.on(&s, SALES);
    t.on(&s, SALES_ROWS);
    let read = |sql: &str, lim: Limits| t.e.core.block_on(t.c.bounded(Some(db), sql, vec![], Mode::Read, lim, std::future::pending())).unwrap();
    let o = read("SELECT region, SUM(qty) AS q FROM sales GROUP BY ROLLUP (region) ORDER BY GROUPING(region), region", Limits::default());
    assert_eq!(o.rows.len(), 4);
    assert_eq!(o.rows[3], [serde_json::Value::Null, serde_json::json!(10)]);
    let o = read("SELECT CAST(1 AS BIT) AS b, CAST(2.5 AS MONEY) AS m, NEWID() AS g, CAST('2024-01-02' AS DATE) AS d", Limits::default());
    assert_eq!(o.rows[0][0], serde_json::json!(true));
    assert_eq!(o.rows[0][1], serde_json::json!("2.5000"));
    assert!(o.rows[0][2].as_str().is_some_and(|g| g.len() == 36), "{:?}", o.rows[0][2]);
    assert_eq!(o.rows[0][3], serde_json::json!("2024-01-02"));
    let o = read("SELECT qty FROM sales ORDER BY qty", Limits { max_rows: 2, ..Default::default() });
    assert_eq!((o.rows.len(), o.truncated.is_some()), (2, true));
}
