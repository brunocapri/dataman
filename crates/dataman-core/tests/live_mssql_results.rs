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
            format!("{:?} {:?} rows={} aff={:?} msgs={:?} err={}", r.phase(), cols, r.rows(), r.status().affected, { let (m, more) = r.messages(); (m.iter().take(3).cloned().collect::<Vec<_>>(), m.len(), more) }, r.status().error.as_ref().map(|e| e.message.clone()).unwrap_or_default())
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
