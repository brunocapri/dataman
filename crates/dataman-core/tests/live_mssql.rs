mod common;

use common::*;
use dataman_core::config::{Engine, SslMode};
use dataman_core::result::PREFETCH;
use dataman_core::sql::Dialect;
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

fn results(e: &Env, first: &Arc<ResultSet>) -> Vec<Arc<ResultSet>> {
    let mut out = vec![first.clone()];
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

fn texts(rs: &ResultSet, row: usize) -> Vec<Option<String>> {
    let snap = rs.snapshot();
    (0..snap.columns.len()).map(|c| snap.text(row, c).map(|s| s.into_owned())).collect()
}

fn s1(v: &str) -> Option<String> {
    Some(v.to_string())
}

/// A port that answers at once and passes nothing on for `delay`, like the gateway of a paused
/// serverless database, then forwards to `target`.
fn stalling_proxy(target: (String, u16), delay: Duration) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let target = target.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let mut server = tokio::net::TcpStream::connect(target).await.unwrap();
                    tokio::io::copy_bidirectional(&mut client, &mut server).await.ok();
                });
            }
        });
    });
    port
}

#[test]
fn a_server_slow_to_log_in_still_connects() {
    let e = env();
    let p = mssql_profile("master");
    if mssql_on(&e, p.clone()).is_none() {
        return;
    }
    let port = stalling_proxy((p.host.clone(), p.port), Duration::from_secs(15));
    let slow = dataman_core::config::ConnectionProfile { host: "127.0.0.1".into(), port, ..p };
    let c = e.core.block_on(e.core.connect(slow, Some(mssql_password()))).expect("a login that takes 15 s connects");
    assert_eq!(c.info.user, "sa");
}

#[test]
fn a_host_that_does_not_answer_gives_up_soon() {
    let e = env();
    // TEST-NET-1: nothing answers there; some networks say so at once, others drop the packets.
    let p = dataman_core::config::ConnectionProfile { host: "192.0.2.1".into(), port: 1433, ..mssql_profile("master") };
    let started = Instant::now();
    let r = e.core.block_on(e.core.connect(p, Some(mssql_password())));
    assert!(r.is_err());
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
}

#[test]
fn connects_and_decodes_every_type() {
    let e = env();
    let Some(c) = mssql_seeded(&e, "dataman_t_types") else { return };
    assert_eq!(c.engine(), Engine::SqlServer);
    assert_eq!(c.dialect(), Dialect::TSql);
    assert!(c.info.version.starts_with("SQL Server ") || c.info.version.starts_with("Azure SQL"), "{}", c.info.version);
    assert_eq!(c.info.database, "dataman_t_types");
    assert_eq!(c.info.user, "sa");
    assert_eq!(c.info.search_path, vec!["dbo".to_string()]);
    assert!(c.caps().schemas && c.caps().functions && !c.caps().explain);
    assert!(c.tls_in_use());

    let s = c.session(None);
    let o = ok(&e, &s, "SELECT SESSIONPROPERTY('QUOTED_IDENTIFIER'), SESSIONPROPERTY('ANSI_NULLS'), SESSIONPROPERTY('CONCAT_NULL_YIELDS_NULL')");
    assert_eq!(texts(&o, 0), vec![s1("1"), s1("1"), s1("1")]);

    let rs = ok(
        &e,
        &s,
        "SELECT id, name, email, active, tier, balance, credit_limit, rate, created, seen, birth_date, at_time, legacy_dt, small_dt, code, photo, profile, score, weight, points, doc, row_version \
         FROM dbo.customer ORDER BY id",
    );
    let cols = rs.columns().unwrap();
    let kinds: Vec<Kind> = cols.iter().map(|c| c.kind).collect();
    use Kind::*;
    assert_eq!(
        kinds,
        vec![
            Int, Text, Text, Bool, Int, Numeric, Numeric, Numeric, Text, Text, Date, Text, Timestamp, Timestamp, Other, Bytes, Text, Float4,
            Float, Int, Text, Bytes
        ]
    );
    assert_eq!((cols[6].type_name.as_str(), cols[16].type_name.as_str(), cols[14].type_name.as_str()), ("money", "xml", "uniqueidentifier"));
    let r0 = texts(&rs, 0);
    assert_eq!(
        r0[..21],
        [
            s1("1"),
            s1("Ana Smith"),
            s1("ana@example.com"),
            s1("true"),
            s1("3"),
            s1("1520.50"),
            s1("5000.0000"),
            s1("2.5000"),
            s1("2026-01-05 09:15:00.25"),
            r0[9].clone(),
            s1("1990-04-12"),
            s1("08:30:00"),
            s1("2026-01-05 09:15:00.003"),
            s1("2026-01-05 09:15:00"),
            s1("6F9619FF-8B86-D011-B42D-00C04FC964FF"),
            s1("\\x89504e47"),
            s1("<profile city=\"Boston\"><points>120</points></profile>"),
            s1("0.1"),
            s1("72.5"),
            s1("9223372036854775807"),
            s1("12345678901"),
        ]
    );
    let snap = rs.snapshot();
    assert_eq!((snap.text(0, 9).as_deref(), snap.text(3, 9).as_deref()), (Some("2026-01-05 09:15:00 -03:00"), Some("2026-04-01 12:00:00 +09:00")), "datetimeoffset: as stored, with its offset");
    assert!(r0[21].as_deref().is_some_and(|v| v.starts_with("\\x") && v.len() == 18), "rowversion: 8 bytes {:?}", r0[21]);
    let r2 = texts(&rs, 2);
    assert_eq!((r2[3].as_deref(), r2[6].as_deref(), r2[9].as_deref(), r2[17].as_deref()), (Some("false"), Some("0.0001"), None, None));
    let r3 = texts(&rs, 3);
    assert_eq!((r3[1].as_deref(), r3[6].as_deref(), r3[12].as_deref(), r3[19].as_deref()), (Some("Diego O'Neil"), Some("-12.3456"), Some("1753-01-01 00:00:00"), Some("-9223372036854775808")));
    let r4 = texts(&rs, 4);
    assert_eq!((r4[1].as_deref(), r4[2].as_deref(), r4[17].as_deref(), r4[18].as_deref()), (Some("Élodie Zürich 東京"), None, Some("1e-07"), Some("1e+300")));
    rs.sort(Some((5, dataman_core::result::SortDir::Desc)));
    assert_eq!(rs.snapshot().text(0, 1).as_deref(), Some("Ana Smith"));

    let i = ok(&e, &s, "SELECT subtotal FROM dbo.item WHERE order_id = 1 ORDER BY line");
    assert_eq!((texts(&i, 0)[0].as_deref(), texts(&i, 1)[0].as_deref()), (Some("70.00"), Some("180.00")));
    assert_eq!(ok(&e, &s, "SELECT * FROM dbo.v_order_customer").rows(), 5);
    assert_eq!(texts(&ok(&e, &s, "SELECT target FROM sales.goal WHERE region = N'Southeast'"), 0), vec![s1("420000.5000")]);
    assert_eq!(ok(&e, &s, "SELECT * FROM sales.customers").rows(), 5);
    assert_eq!(ok(&e, &s, "SELECT * FROM sales.items_of(1)").rows(), 2);
    assert_eq!(texts(&ok(&e, &s, "SELECT dbo.customer_total(1)"), 0), vec![s1("330.50")]);
    let v = ok(&e, &s, "SELECT CAST(42 AS sql_variant), CAST(N'x' AS sql_variant), hierarchyid::Parse('/1/2/'), CAST(NULL AS sql_variant)");
    let t = texts(&v, 0);
    assert_eq!((t[0].as_deref(), t[1].as_deref(), t[3].as_deref()), (Some("42"), Some("x"), None));
    assert!(t[2].as_deref().is_some_and(|h| h.starts_with("\\x")));
    drop(s);
    drop(c);
    mssql_drop(&e, "dataman_t_types");
}

#[test]
fn batches_results_and_errors() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("tempdb")) else { return };
    let s = c.session(None);

    let rs = s.execute("SELECT 1 AS a; SELECT 2 AS b, N'ü' AS c\nSELECT TOP (0) 1 AS d");
    let all = results(&e, &rs);
    assert_eq!(all.len(), 3);
    assert_eq!(all.iter().map(|r| r.phase()).collect::<Vec<_>>(), vec![Phase::Done; 3]);
    assert_eq!(all[1].columns().unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["b", "c"]);
    assert_eq!(texts(&all[1], 0), vec![s1("2"), s1("ü")]);
    assert_eq!(all[2].rows(), 0);
    assert!(Arc::ptr_eq(&s.current().unwrap(), &all[2]), "the session follows the last result");

    let rs = s.execute("CREATE TABLE #t (a int)\nGO\nINSERT #t VALUES (1)\nGO 3\nSELECT COUNT(*) AS n FROM #t\nGO");
    let all = results(&e, &rs);
    assert_eq!(all.len(), 1, "commands without rows add no result");
    assert_eq!(texts(&all[0], 0), vec![s1("3")]);
    let u = ok(&e, &s, "UPDATE #t SET a = 2 WHERE a = 1");
    assert_eq!(u.status().affected, Some(3));
    let u = ok(&e, &s, "INSERT #t VALUES (5)\nGO 4");
    assert_eq!(u.status().affected, Some(4), "GO n adds its runs' counts");
    ok(&e, &s, "CREATE TABLE #p (id int, v nvarchar(10)); INSERT #p VALUES (1, N'one'), (2, N'two')");
    ok(&e, &s, "CREATE PROCEDURE #two_sets AS BEGIN SELECT * FROM #p ORDER BY id; SELECT COUNT(*) AS n FROM #p; END");
    let all = results(&e, &s.execute("EXEC #two_sets"));
    assert_eq!((all.len(), all[0].rows(), texts(&all[1], 0)), (2, 2, vec![s1("2")]));

    let sql = "SELECT 1 AS one\nGO\n-- the second batch\nSELECT 2\n  FROM WHERE x = 1\nGO\nSELECT 3";
    let rs = s.execute(sql);
    let all = results(&e, &rs);
    assert_eq!(all.len(), 2, "the first batch's result, then the error");
    assert_eq!((all[0].phase(), all[1].phase()), (Phase::Done, Phase::Failed));
    let err = all[1].status().error.unwrap();
    assert_eq!(err.code.as_deref(), Some("156"), "{err:?}");
    let p = err.position.unwrap() as usize;
    assert_eq!(&sql[p - 1..p + 4], "WHERE", "the token the error names, on its line: {err:?}");
    assert!(err.detail.as_deref().is_some_and(|d| d.contains("Level 15") && d.contains("line 2")), "{err:?}");
    let all = results(&e, &s.execute("SELECT 1 AS a\nSELECT 1 / 0"));
    assert_eq!(all.last().unwrap().status().error.unwrap().code.as_deref(), Some("8134"));
    assert_eq!(all[0].rows(), 1);
    assert_eq!(texts(&ok(&e, &s, "SELECT COUNT(*) FROM #t"), 0), vec![s1("7")]);
    assert_eq!(ok(&e, &s, "-- nothing\n/* at all */").columns(), None);
}

#[test]
fn print_messages() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("tempdb")) else { return };
    let s = c.session(None);
    let msgs = |rs: &ResultSet| rs.messages().0.to_vec();

    assert_eq!(msgs(&ok(&e, &s, "PRINT 'hello'")), ["hello"]);
    let rs = ok(&e, &s, "DECLARE @i int = 1\nWHILE @i <= 3 BEGIN PRINT CONCAT('step ', @i); SET @i += 1 END\nGO\nRAISERROR('warn %d', 10, 1, 5)\nGO 2");
    assert_eq!(msgs(&rs), ["step 1", "step 2", "step 3", "warn 5", "warn 5"]);
    ok(&e, &s, "CREATE PROCEDURE #speak AS BEGIN PRINT N'inside ü' END");
    assert_eq!(msgs(&ok(&e, &s, "EXEC #speak")), ["inside ü"]);
    let all = results(&e, &s.execute("PRINT 'a'; SELECT 1 AS x; PRINT 'b'; SELECT 2 AS y; PRINT 'c'"));
    assert_eq!(all.len(), 2);
    assert_eq!((msgs(&all[0]), msgs(&all[1])), (vec!["a".to_string(), "b".into()], vec!["c".to_string()]));
    assert_eq!(texts(&all[1], 0), vec![s1("2")]);
    let f = run(&e, &s, "PRINT 'before'; SELECT 1 / 0");
    assert_eq!((f.phase(), msgs(&f)), (Phase::Failed, vec!["before".to_string()]));
    assert_eq!(texts(&ok(&e, &s, "SELECT 7"), 0), vec![s1("7")]);
}

#[test]
fn streams_with_pauses() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("tempdb")) else { return };
    let s = c.session(None);
    let sql = "WITH n AS (SELECT TOP (100000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_columns a CROSS JOIN sys.all_columns b) \
               SELECT i, CONCAT(N'row ', i) AS t, i * 0.5 AS h FROM n";
    let rs = s.execute(sql);
    e.core.block_on(async {
        let mut rx = rs.subscribe();
        while rs.phase() != Phase::Paused {
            tokio::time::timeout(Duration::from_secs(30), rx.changed()).await.expect("pause").unwrap();
        }
    });
    let loaded = rs.rows();
    assert!((PREFETCH..PREFETCH + 2000).contains(&loaded), "{loaded}");
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.unwrap() });
    assert_eq!((rs.phase(), rs.rows()), (Phase::Done, 100_000));
    assert_eq!(texts(&rs, 99_999), vec![s1("100000"), s1("row 100000"), s1("50000.0")]);
    assert_eq!(texts(&ok(&e, &s, "SELECT 7"), 0), vec![s1("7")]);
}

#[test]
fn cancel_keeps_the_session() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("tempdb")) else { return };
    let s = c.session(None);
    ok(&e, &s, "CREATE TABLE #keep (a int); INSERT #keep VALUES (1)");
    let rs = s.execute("WAITFOR DELAY '00:00:30'; SELECT 1");
    std::thread::sleep(Duration::from_millis(400));
    let t = Instant::now();
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("cancel took too long") });
    assert_eq!(rs.phase(), Phase::Cancelled);
    eprintln!("WAITFOR cancelled after {:?}", t.elapsed());
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(texts(&ok(&e, &s, "SELECT COUNT(*) FROM #keep"), 0), vec![s1("1")]);
    for i in 0..12u64 {
        let rs = s.execute("SELECT a.object_id, b.name, CONCAT(a.name, N'·', b.name) FROM sys.all_columns a CROSS JOIN sys.all_columns b");
        if i % 3 != 2 {
            rs.fetch_all();
        }
        std::thread::sleep(Duration::from_millis(40 + 37 * i));
        s.cancel();
        e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("cancel took too long") });
        assert_eq!(rs.phase(), Phase::Cancelled, "round {i}");
        assert_eq!(texts(&ok(&e, &s, &format!("SELECT {i} + COUNT(*) FROM #keep")), 0), vec![s1(&(i + 1).to_string())], "round {i}");
    }
}

#[test]
fn transactions_follow_the_server() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("tempdb")) else { return };
    let s = c.session(None);
    ok(&e, &s, "CREATE TABLE #tx (a int PRIMARY KEY)");
    assert_eq!(s.tx().status, TxStatus::Idle);
    ok(&e, &s, "BEGIN TRANSACTION");
    assert_eq!(s.tx().status, TxStatus::Open);
    ok(&e, &s, "INSERT #tx VALUES (1)");
    assert_eq!(run(&e, &s, "INSERT #tx VALUES (1)").status().error.unwrap().code.as_deref(), Some("2627"));
    assert_eq!(s.tx().status, TxStatus::Open);
    ok(&e, &s, "SAVE TRANSACTION sp");
    assert_eq!(s.tx().status, TxStatus::Open);
    let end = e.core.block_on(s.end_transaction(true)).unwrap();
    assert_eq!((end, s.tx().status), (TxEnd::Committed, TxStatus::Idle));
    assert_eq!(texts(&ok(&e, &s, "SELECT COUNT(*) FROM #tx"), 0), vec![s1("1")]);
    ok(&e, &s, "BEGIN TRANSACTION; INSERT #tx VALUES (5)");
    assert_eq!(s.tx().status, TxStatus::Open);
    assert_eq!(run(&e, &s, "INSERT #tx VALUES ('x')").status().error.unwrap().code.as_deref(), Some("245"));
    assert_eq!(s.tx().status, TxStatus::Idle, "rolled back by the server");
    assert_eq!(texts(&ok(&e, &s, "SELECT COUNT(*) FROM #tx"), 0), vec![s1("1")]);
    ok(&e, &s, "SET XACT_ABORT ON; BEGIN TRANSACTION; INSERT #tx VALUES (2)");
    assert_eq!(run(&e, &s, "SELECT 1 / 0").phase(), Phase::Failed);
    assert_eq!(s.tx().status, TxStatus::Idle, "XACT_ABORT rolls back at once");
    ok(&e, &s, "SET XACT_ABORT OFF");
    ok(&e, &s, "BEGIN TRANSACTION");
    ok(&e, &s, "INSERT #tx VALUES (3)");
    assert_eq!(s.tx().status, TxStatus::Open);
    assert_eq!(e.core.block_on(s.end_transaction(false)).unwrap(), TxEnd::RolledBack);
    assert_eq!(texts(&ok(&e, &s, "SELECT COUNT(*) FROM #tx"), 0), vec![s1("1")]);
    assert_eq!(s.tx().status, TxStatus::Idle);
    assert!(s.backend_pid().is_some_and(|p| p > 0));
}

#[test]
fn parameters_are_bound() {
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("tempdb")) else { return };
    let s = c.session(None);
    let rs = s.execute_with("SELECT @P1 + 1 AS n, @P2 AS t, @P3 AS z", vec![Some("41".into()), Some("o'k ü".into()), None]);
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(rs.phase(), Phase::Done, "{:?}", rs.status().error);
    assert_eq!(texts(&rs, 0), vec![s1("42"), s1("o'k ü"), None]);
}

#[test]
fn tls_modes() {
    let e = env();
    if mssql_on(&e, mssql_profile("master")).is_none() {
        return;
    }
    for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require] {
        let p = dataman_core::ConnectionProfile { sslmode: mode, ..mssql_profile("master") };
        let c = e.core.block_on(e.core.connect(p, Some(mssql_password()))).unwrap_or_else(|err| panic!("{mode:?}: {err:#}"));
        assert_eq!(c.tls_in_use(), mode != SslMode::Disable, "{mode:?}");
    }
    let p = dataman_core::ConnectionProfile { sslmode: SslMode::VerifyFull, ..mssql_profile("master") };
    assert!(e.core.block_on(e.core.connect(p, Some(mssql_password()))).is_err());
    let Err(err) = e.core.block_on(e.core.connect(mssql_profile("master"), Some("nope".into()))) else { panic!("connected with a wrong password") };
    let info = dataman_core::server_error(&err).unwrap();
    assert_eq!(info.code.as_deref(), Some("18456"), "{info:?}");
}

#[test]
fn bench_fetch_all() {
    if std::env::var("DATAMAN_MSSQL_BENCH").is_err() {
        return;
    }
    let e = env();
    let Some(c) = mssql_on(&e, mssql_profile("tempdb")) else { return };
    let s = c.session(None);
    let sql = "WITH n AS (SELECT TOP (1000000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_columns a CROSS JOIN sys.all_columns b CROSS JOIN sys.all_columns c) \
               SELECT i, i * 2 AS j, CONCAT(N'r', i) AS t FROM n";
    let mut times = Vec::new();
    for _ in 0..7 {
        let t = Instant::now();
        let rs = s.execute(sql);
        rs.fetch_all();
        e.core.block_on(rs.wait_finished());
        assert_eq!(rs.rows(), 1_000_000);
        times.push(t.elapsed());
    }
    times.sort();
    eprintln!("mssql 1M x 3 fetch-all: median {:?} (min {:?}, max {:?})", times[3], times[0], times[6]);
}

#[test]
fn parse_check_reports_the_servers_syntax_errors() {
    let e = env();
    let db = "dataman_t_parse";
    let Some(c) = mssql_seeded(&e, db) else { return };
    let check = |sql: &str| {
        let t0 = std::time::Instant::now();
        let l = e.core.block_on(c.parse_check(db, sql.to_string())).unwrap();
        (l, t0.elapsed())
    };
    let sql = "SELECT * FRM dbo.orders";
    let (l, first) = check(sql);
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(l[0].message.contains("Incorrect syntax"), "{l:?}");
    assert!(sql[l[0].range.clone()].eq_ignore_ascii_case("dbo.orders") || &sql[l[0].range.clone()] == "FRM", "{:?}", &sql[l[0].range.clone()]);
    let (ok, warm) = check("SELECT * FROM dbo.orders WHERE id = 1");
    assert!(ok.is_empty(), "{ok:?}");
    let (ok, _) = check("SELECT * FROM dbo.orders WHERE id = @id");
    assert!(ok.is_empty(), "{ok:?}");
    eprintln!("parse check: first {first:?} (connects), then {warm:?}");
    let script = "SELECT 1\nGO\nSELECT 2\nSELECT * FROM WHERE x = 1\nGO\n";
    let (l, _) = check(script);
    assert_eq!(l.len(), 1, "{l:?}");
    let line = script[..l[0].range.start].matches('\n').count();
    assert_eq!(line, 3, "{l:?}");
    let (l, _) = check("INSERT INTO dbo.customer (name) VALUES (N'parse only')");
    assert!(l.is_empty(), "{l:?}");
    let n = query(&e, &c, "SELECT COUNT(*) FROM dbo.customer WHERE name = N'parse only'").snapshot().text(0, 0).map(|s| s.into_owned());
    assert_eq!(n.as_deref(), Some("0"));
    exec(&e, &c, &["CREATE TABLE dbo.HierarchyLevels (Id uniqueidentifier PRIMARY KEY, ParentLevelId uniqueidentifier NULL, IsDeleted bit NOT NULL DEFAULT 0, PathToRoot nvarchar(4000) NULL, IsRoot bit NOT NULL DEFAULT 0, OrganizationId uniqueidentifier NULL)"]);
    let cte = "WITH Roots AS (\n    SELECT CAST(RootId AS uniqueidentifier) AS RootId FROM (VALUES\n        ('902ebf67-556c-1b71-4858-3a1e21c5bd74'),\n        ('06d0eb4d-6c89-d5f7-1c2e-3a2009634fba')\n    ) v(RootId)\n),\nByParent AS (\n    SELECT h.Id FROM HierarchyLevels h JOIN Roots r ON r.RootId = h.Id WHERE h.IsDeleted = 0\n    UNION ALL\n    SELECT c.Id FROM HierarchyLevels c JOIN ByParent p ON c.ParentLevelId = p.Id WHERE c.IsDeleted = 0\n),\nByPath AS (\n    SELECT h.Id FROM HierarchyLevels h\n    WHERE h.IsDeleted = 0\n      AND EXISTS (SELECT 1 FROM Roots r WHERE h.PathToRoot LIKE '%' + CONVERT(nvarchar(36), r.RootId) + '%')\n)\nSELECT\n    (SELECT COUNT(DISTINCT Id) FROM ByParent) AS ProgramsByParent,\n    (SELECT COUNT(*) FROM ByPath) AS ProgramsByPath,\n    (SELECT COUNT(*) FROM ByPath WHERE Id NOT IN (SELECT Id FROM ByParent)) AS OnlyInPath,\n    (SELECT COUNT(*) FROM ByParent WHERE Id NOT IN (SELECT Id FROM ByPath)) AS OnlyInParent,\n    (SELECT COUNT(*) FROM HierarchyLevels h\n        WHERE h.IsDeleted = 0 AND h.IsRoot = 0\n          AND h.OrganizationId = '74A17135-94CF-62A5-BDE1-3A16C41F541A'\n          AND h.Id NOT IN (SELECT Id FROM ByPath)) AS OrgProgramsOutOfScope\nOPTION (MAXRECURSION 100);";
    let (l, _) = check(cte);
    assert!(l.is_empty(), "{l:?}");
    drop(c);
    mssql_drop(&e, db);
}

/// The statement's target decides, not the database the connection is on.
#[test]
fn a_read_only_database_holds_against_a_write_from_another() {
    use dataman_core::bounded::{Limits, Mode, Stop};
    let e = env();
    let pid = std::process::id();
    let (w, ro) = (format!("dataman_t_w_{pid}"), format!("dataman_t_ro_{pid}"));
    let Some(admin) = mssql_on(&e, mssql_profile("master")) else { return };
    for d in [&w, &ro] {
        mssql_drop(&e, d);
        exec(&e, &admin, &[&format!("CREATE DATABASE [{d}]")]);
        exec(&e, &admin, &[&format!("CREATE TABLE [{d}].dbo.items (id int)"), &format!("INSERT INTO [{d}].dbo.items VALUES (1)")]);
    }
    let count = |d: &str| query(&e, &admin, &format!("SELECT count(*) FROM [{d}].dbo.items")).snapshot().text(0, 0).map(|t| t.into_owned());
    let mut p = mssql_profile(&w);
    p.set_read_only_for(&ro, true);
    let Some(c) = mssql_on(&e, p) else { return };
    let call = |sql: &str| e.core.block_on(c.bounded(Some(&w), sql, vec![], Mode::Write, Limits::default(), std::future::pending::<()>()));
    for sql in [
        format!("INSERT INTO {ro}.dbo.items VALUES (2)"),
        format!("UPDATE [{ro}].dbo.items SET id = 9"),
        format!("DELETE FROM {ro}..items"),
        format!("MERGE {ro}.dbo.items t USING (SELECT 1 AS id) s ON t.id = s.id WHEN MATCHED THEN DELETE;"),
        format!("USE {ro}; INSERT INTO dbo.items VALUES (2)"),
        format!("USE {ro}\nDELETE FROM dbo.items"),
        format!("SELECT 1 AS a UPDATE {ro}.dbo.items SET id = 9"),
        format!("INSERT INTO dbo.items VALUES (2); DELETE FROM {}.dbo.items", ro.to_uppercase()),
    ] {
        assert!(matches!(call(&sql), Err(Stop::Refused(r)) if r.contains("read-only")), "{sql}");
    }
    assert_eq!(count(&ro).as_deref(), Some("1"));
    assert_eq!(count(&w).as_deref(), Some("1"), "nothing of a refused call runs");
    call(&format!("INSERT INTO dbo.items SELECT id FROM {ro}.dbo.items")).unwrap();
    assert_eq!(count(&w).as_deref(), Some("2"));

    let tab = c.session(None);
    let use_ro = tab.execute(format!("USE [{ro}]"));
    e.core.block_on(use_ro.wait_finished());
    assert_eq!(tab.current_database(), ro);
    assert!(c.read_only_refusal(&tab.current_database(), "INSERT INTO dbo.items VALUES (2)").is_some());
    drop(tab);
    drop(c);
    mssql_drop(&e, &w);
    mssql_drop(&e, &ro);
}

#[test]
fn read_only_per_database() {
    use dataman_core::bounded::{Limits, Mode, Stop};
    let e = env();
    let db = "dataman_t_rodb";
    let Some(_seed) = mssql_seeded(&e, db) else { return };
    let mut p = mssql_profile(db);
    p.set_read_only_for(db, true);
    assert!(!p.read_only_everywhere(), "no ApplicationIntent=ReadOnly for one database");
    let Some(c) = mssql_on(&e, p) else { return };
    assert!(c.read_only_for(db) && c.read_only_for(&db.to_uppercase()) && !c.read_only_for("tempdb"));
    let write = "UPDATE dbo.customer SET name = name WHERE id = 1";
    assert!(dataman_core::guard::read_only_refusal_in(write, Dialect::TSql).is_some());
    let bounded = |db: &str| e.core.block_on(c.bounded(Some(db), write, vec![], Mode::Write, Limits::default(), std::future::pending::<()>()));
    assert!(matches!(bounded(db), Err(Stop::Refused(r)) if r.contains("read-only")));
    let rs = query(&e, &c, "SELECT id, name FROM dbo.customer");
    let ed = e.core.block_on(c.edit_target(db, &rs, None)).unwrap();
    assert_eq!(ed, dataman_core::edit::Editability::ReadOnly("the database is read-only".into()));
    c.set_read_only_for(db, false);
    assert!(bounded(db).is_ok());
    drop(c);
    mssql_drop(&e, db);
}
