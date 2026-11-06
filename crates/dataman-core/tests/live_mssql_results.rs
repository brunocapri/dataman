mod common;

use common::*;
use dataman_core::sql::{Dialect, batches_in};
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
            format!(
                "{:?} {:?} rows={} aff={:?} msgs={:?} err={}",
                r.phase(),
                cols,
                r.rows(),
                r.status().affected,
                {
                    let (m, more) = r.messages();
                    (m.iter().take(3).cloned().collect::<Vec<_>>(), m.len(), more)
                },
                r.status().error.as_ref().map(|e| e.message.clone()).unwrap_or_default()
            )
        })
        .collect()
}

#[test]
fn statement_errors_keep_their_place() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("master")) else { return };
    let s = c.session(None);
    let rs = all(&e, &s, "SELECT 1 AS first_ok; SELECT 1/0 AS boom; SELECT 2 AS later");
    let got = shape(&rs);
    let state: Vec<(Phase, Option<String>)> = rs.iter().map(|r| (r.phase(), r.status().error.map(|e| e.message))).collect();
    let failed = state.iter().position(|(p, _)| *p == Phase::Failed).unwrap_or(99);
    let later = rs.iter().position(|r| r.columns().is_some_and(|c| c.iter().any(|c| c.name == "later"))).unwrap();
    assert!(failed < later, "{got:#?}");
    assert!(state[failed].1.as_deref().is_some_and(|m| m.contains("Divide by zero")), "{got:#?}");
    assert_eq!(state.iter().filter(|(p, _)| *p == Phase::Failed).count(), 1, "{got:#?}");
    assert_eq!(rs[later].phase(), Phase::Done, "{got:#?}");
    assert_eq!(rs[0].snapshot().text(0, 0).as_deref(), Some("1"));
    let rs = all(&e, &s, "SELECT 1 AS a; SELECT 1/0 AS b");
    assert_eq!(rs.last().unwrap().phase(), Phase::Failed, "{:#?}", shape(&rs));
    assert_eq!(rs.iter().filter(|r| r.phase() == Phase::Failed).count(), 1);
    let rs = all(&e, &s, "UPDATE #nope SET a = 1; SELECT 5 AS five");
    let got = shape(&rs);
    assert_eq!(rs[0].phase(), Phase::Failed, "{got:#?}");
    let rs = all(&e, &s, "SELECT 7 AS seven");
    assert_eq!(rs[0].snapshot().text(0, 0).as_deref(), Some("7"));
}

#[test]
fn result_sets() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("master")) else { return };
    let s = c.session(None);
    for (name, sql) in [
        ("multi", "SELECT 1 AS a; SELECT 'x' AS b, 2 AS c; UPDATE #nope SET a = 1"),
        ("proc-like", "SET NOCOUNT ON; SELECT 1 AS a; PRINT 'hello'; SELECT 2 AS b"),
        ("nocount-off", "CREATE TABLE #t (a INT); INSERT #t VALUES (1),(2),(3); UPDATE #t SET a = a + 1; SELECT * FROM #t; DELETE #t WHERE a > 2"),
        ("raiserror-info", "RAISERROR('just info', 0, 1) WITH NOWAIT; SELECT 1 AS after_info"),
        ("error-mid", "SELECT 1 AS first_ok; SELECT 1/0 AS boom; SELECT 2 AS never"),
        ("error-batch-abort", "SELECT 1 AS first_ok; SELECT CAST('x' AS INT) AS boom; SELECT 2 AS never_maybe"),
        ("try-catch", "BEGIN TRY SELECT 1/0 END TRY BEGIN CATCH SELECT ERROR_MESSAGE() AS m END CATCH"),
        ("empty", "SELECT 1 AS a WHERE 1 = 0"),
        ("only-print", "PRINT 'only'"),
        ("exec-multi", "EXEC('SELECT 1 AS x; SELECT 2 AS y')"),
        ("go-repeat", "SELECT 1 AS r\nGO 3"),
        ("usp", "EXEC sp_who2 'active'"),
        ("sp_help", "EXEC sp_help 'sys.objects'"),
        ("dbcc", "DBCC CHECKDB ('master') WITH NO_INFOMSGS"),
        ("showplan-off", "SET STATISTICS TIME ON; SELECT 1 AS a; SET STATISTICS TIME OFF"),
        ("xml-for", "SELECT name FROM sys.databases FOR XML PATH('')"),
        ("json-for", "SELECT TOP 2 name FROM sys.databases FOR JSON AUTO"),
        ("print-loop", "DECLARE @i INT = 0; WHILE @i < 5000 BEGIN PRINT CONCAT('line ', @i); SET @i += 1; END; SELECT @i AS done"),
        ("colname-empty", "SELECT 1, 'a', GETDATE() AS d, 2 AS dup, 3 AS dup"),
    ] {
        let units = batches_in(sql, Dialect::TSql);
        for u in units {
            let b = &sql[u.range];
            let rs = all(&e, &s, b);
            eprintln!("{name}: {} sets", rs.len());
            for l in shape(&rs) {
                eprintln!("    {l}");
            }
        }
    }
}

#[test]
fn money_and_times_keep_every_digit() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("master")) else { return };
    let s = c.session(None);
    let rs = all(
        &e,
        &s,
        "CREATE TABLE #m (a MONEY NOT NULL, b SMALLMONEY NOT NULL, c MONEY NULL, d DATETIME2 NOT NULL, t TIME NOT NULL, d3 DATETIME2(3) NULL);
         INSERT #m VALUES (922337203685477.5807, -214748.3648, -922337203685477.5808, '2026-01-01 00:00:00.0000001', '12:00:00.1234567', '2026-01-01 00:00:00.25'), (9007199254740.9931, 0.0001, NULL, '9999-12-31 23:59:59.9999999', '23:59:59.9999999', NULL);
         SELECT a, b, c, d, t, d3 FROM #m;
         SELECT CAST(CAST(9007199254740.9931 AS MONEY) AS SQL_VARIANT) AS v",
    );
    let set = |name: &str| rs.iter().find(|r| r.columns().is_some_and(|c| c.iter().any(|c| c.name == name))).unwrap().snapshot();
    let row = |r: usize| {
        let snap = set("a");
        (0..6).map(|col| snap.text(r, col).map(|t| t.into_owned())).collect::<Vec<_>>()
    };
    let some = |v: &[&str]| v.iter().map(|t| Some(t.to_string())).collect::<Vec<_>>();
    assert_eq!(
        row(0),
        some(&["922337203685477.5807", "-214748.3648", "-922337203685477.5808", "2026-01-01 00:00:00.0000001", "12:00:00.1234567", "2026-01-01 00:00:00.25"])
    );
    let r1 = row(1);
    assert_eq!((r1[0].as_deref(), r1[1].as_deref(), r1[2].as_deref()), (Some("9007199254740.9931"), Some("0.0001"), None));
    assert_eq!((r1[3].as_deref(), r1[4].as_deref(), r1[5].as_deref()), (Some("9999-12-31 23:59:59.9999999"), Some("23:59:59.9999999"), None));
    assert_eq!(set("v").text(0, 0).as_deref(), Some("9007199254740.9931"), "money in a sql_variant");
}

#[test]
fn numbered_placeholders_bind_their_values() {
    use dataman_core::bounded::{Limits, Mode, Stop};
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("master")) else { return };
    let run = |sql: &str, params: &[&str]| {
        let params = params.iter().map(|p| Some(p.to_string())).collect();
        e.core.block_on(c.bounded(Some("master"), sql, params, Mode::Read, Limits::default(), std::future::pending::<()>()))
    };
    let o = run("SELECT name, $9.50 AS price FROM sys.databases WHERE database_id = $1 OR database_id = $2 ORDER BY database_id", &["3", "4"]).unwrap();
    assert_eq!(o.rows, vec![vec![serde_json::json!("model"), serde_json::json!("9.5000")], vec![serde_json::json!("msdb"), serde_json::json!("9.5000")]]);
    let o = run("SELECT name FROM sys.databases WHERE database_id = @P1", &["3"]).unwrap();
    assert_eq!(o.rows, vec![vec![serde_json::json!("model")]]);
    let o = run("SELECT $1 AS literal", &[]).unwrap();
    assert_eq!(o.rows, vec![vec![serde_json::json!("1.0000")]], "without values it is T-SQL's money literal");
    for (sql, params, why) in
        [("SELECT 1", &["3"][..], "no placeholder"), ("SELECT $2", &["3"][..], "uses $2"), ("SELECT $2", &["3", "4"][..], "never uses $1")]
    {
        assert!(matches!(run(sql, params), Err(Stop::Refused(r)) if r.contains(why)), "{sql}");
    }
}

#[test]
fn exports_type_datetime2_and_time() {
    use arrow_array::{Array as _, Time64MicrosecondArray, TimestampMicrosecondArray};
    use dataman_core::export::{ExportFormat, ExportOptions};
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("master")) else { return };
    let dir = tempfile::tempdir().unwrap();
    let sql = "SELECT CAST('2026-01-01 00:00:00.1234567' AS DATETIME2) AS d, CAST('12:00:00.5' AS TIME) AS t, CAST(NULL AS DATETIME2) AS n";
    let export = |format, name: &str| {
        let path = dir.path().join(name);
        let mut o = ExportOptions::new(format);
        o.dialect = Dialect::TSql;
        o.table = "x".into();
        e.core.block_on(c.export_query(Some("master"), sql.to_string(), &path, o, None).task).unwrap();
        path
    };
    let csv = std::fs::read_to_string(export(ExportFormat::Csv, "x.csv")).unwrap();
    assert_eq!(csv.lines().nth(1), Some("2026-01-01 00:00:00.1234567,12:00:00.5,"), "{csv}");
    let insert = std::fs::read_to_string(export(ExportFormat::SqlInsert, "x.sql")).unwrap();
    assert!(insert.contains("N'2026-01-01T00:00:00.1234567', N'12:00:00.5', NULL"), "{insert}");
    let file = std::fs::File::open(export(ExportFormat::Parquet, "x.parquet")).unwrap();
    let mut rows = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file).unwrap().build().unwrap();
    let batch = rows.next().unwrap().unwrap();
    let d = batch.column(0).as_any().downcast_ref::<TimestampMicrosecondArray>().expect("a timestamp");
    assert_eq!(d.value(0), 1_767_225_600_123_456);
    let t = batch.column(1).as_any().downcast_ref::<Time64MicrosecondArray>().expect("a time");
    assert_eq!(t.value(0), 43_200_500_000);
    assert!(batch.column(2).is_null(0));
}
