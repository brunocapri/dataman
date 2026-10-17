//! SQL Server: valid statements must never get a lint error.

mod common;

use common::*;
use dataman_core::lint::{LintContext, Severity, lint};
use dataman_core::sql::Dialect;

const VALID: &[&str] = &[
    "USE master\nGO\nSELECT * FROM dbo.spt_values WHERE type = 'P'",
    "SELECT * FROM dbo.t1 WHERE name = N'x' AND n IN (1, 2)",
    "SELECT [id], [we]]ird col], t.[name] FROM [dbo].[t1] AS t",
    "SELECT TOP 5 PERCENT * FROM dbo.t1 ORDER BY id",
    "SELECT TOP (3) WITH TIES id FROM dbo.t1 ORDER BY n",
    "SELECT DATEADD(dd, 1, GETDATE()), DATEDIFF(ss, '2020-01-01', GETDATE()), DATEPART(dayofyear, GETDATE()), DATEPART(weekday, GETDATE()), DATENAME(mm, GETDATE()), DATEDIFF(ms, GETDATE(), GETDATE()), DATEADD(yyyy, 1, GETDATE()), DATEDIFF(hh, GETDATE(), GETDATE()), DATEPART(mi, GETDATE()), DATEPART(qq, GETDATE()), DATEDIFF_BIG(nanosecond, GETDATE(), GETDATE()), DATEPART(tzoffset, SYSDATETIMEOFFSET()), DATEPART(iso_week, GETDATE()), DATEDIFF(microsecond, GETDATE(), GETDATE())",
    "SELECT DATEADD(day, 1, at), DATEDIFF(month, at, GETDATE()), DATEPART(year, at) FROM dbo.log1",
    "SELECT IIF(n > 1, 'a', 'b'), TRY_CAST(name AS INT), TRY_CONVERT(INT, name), CONVERT(VARCHAR(10), GETDATE(), 120), FORMAT(GETDATE(), 'yyyy-MM-dd') FROM dbo.t1",
    "SELECT name FROM dbo.t1 WITH (NOLOCK) WHERE id = 1",
    "SELECT name FROM dbo.t1 t WITH (INDEX(1), NOLOCK) WHERE id = 1 OPTION (RECOMPILE, MAXDOP 1)",
    "SELECT STUFF((SELECT ',' + name FROM dbo.t1 FOR XML PATH('')), 1, 1, '')",
    "SELECT id, name FROM dbo.t1 FOR JSON AUTO, ROOT('r')",
    "SELECT id, name FROM dbo.t1 FOR JSON PATH, WITHOUT_ARRAY_WRAPPER",
    "SELECT * FROM sys.objects WHERE type = 'U' ORDER BY name",
    "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = 'dbo'",
    "SELECT s.session_id, s.login_name FROM sys.dm_exec_sessions s WHERE s.is_user_process = 1",
    "SELECT OBJECT_NAME(object_id), name FROM sys.columns WHERE object_id = OBJECT_ID('dbo.t1')",
    "SELECT * FROM master.dbo.spt_values WHERE type = 'P'",
    "SELECT * FROM master..spt_values",
    "SELECT * FROM tempdb..sysobjects",
    "SELECT * FROM dbo.f_inline(1)",
    "SELECT t.id, f.name FROM dbo.t1 t CROSS APPLY dbo.f_inline(t.id) f",
    "SELECT t.id, f.name FROM dbo.t1 t OUTER APPLY (SELECT TOP 1 name FROM dbo.t1 x WHERE x.id = t.id) f",
    "SELECT * FROM OPENJSON('[{\"a\":1}]') WITH (a INT '$.a') j",
    "SELECT value FROM STRING_SPLIT('a,b', ',')",
    "SELECT id, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) rn, SUM(n) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING) s FROM dbo.t1",
    "SELECT name, COUNT(*) c FROM dbo.t1 GROUP BY name HAVING COUNT(*) > 1 ORDER BY c DESC",
    "SELECT name, COUNT(*) FROM dbo.t1 GROUP BY ROLLUP (name)",
    "SELECT name, STRING_AGG(CAST(id AS VARCHAR(10)), ',') WITHIN GROUP (ORDER BY id) FROM dbo.t1 GROUP BY name",
    "SELECT PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY n) OVER (PARTITION BY name) FROM dbo.t1",
    "INSERT INTO dbo.t1 (name, n) OUTPUT inserted.id, inserted.name VALUES (N'a', 1)",
    "INSERT dbo.t1 (name) SELECT name FROM dbo.t1",
    "INSERT INTO dbo.log1 DEFAULT VALUES",
    "UPDATE t SET t.n = t.n + 1 FROM dbo.t1 t JOIN dbo.log1 l ON l.id = t.id WHERE l.id > 0",
    "UPDATE TOP (1) dbo.t1 SET n = 1 WHERE id = 1",
    "UPDATE dbo.t1 SET n = 1 OUTPUT deleted.n, inserted.n WHERE id = 1",
    "DELETE FROM dbo.t1 OUTPUT deleted.id WHERE id = 99",
    "DELETE t FROM dbo.t1 t WHERE t.id = 99",
    "SELECT * INTO #t FROM dbo.t1",
    "SELECT * FROM #t",
    "SELECT * INTO ##g FROM dbo.t1",
    "SELECT * FROM ##g",
    "CREATE TABLE #c (a INT)",
    "INSERT INTO #c VALUES (1)",
    "DECLARE @v TABLE (a INT, b NVARCHAR(10)); INSERT INTO @v VALUES (1, 'x'); SELECT a, b FROM @v",
    "DECLARE @d DATE = '2020-01-01', @n INT; SELECT @n = COUNT(*) FROM dbo.t1 WHERE at > @d; SELECT @n",
    "SELECT @@VERSION, @@ROWCOUNT, @@SPID, @@TRANCOUNT",
    "WITH c AS (SELECT id, name FROM dbo.t1) SELECT c.id FROM c",
    "WITH a (x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM a WHERE x < 3) SELECT x FROM a OPTION (MAXRECURSION 5)",
    "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v (k, nm)",
    "SELECT * FROM dbo.t1 PIVOT (COUNT(id) FOR name IN ([a], [b])) AS p",
    "SELECT n, id FROM dbo.t1 UNPIVOT (val FOR col IN (n, id)) AS u",
    "SELECT id FROM dbo.t1 EXCEPT SELECT id FROM dbo.log1",
    "SELECT id FROM dbo.t1 INTERSECT SELECT id FROM dbo.log1 ORDER BY id",
    "SELECT id FROM dbo.t1 ORDER BY id OFFSET 5 ROWS FETCH NEXT 10 ROWS ONLY",
    "SELECT CASE WHEN n > 1 THEN 'a' ELSE 'b' END AS k FROM dbo.t1 ORDER BY k",
    "SELECT n AS [select], n AS 'alias', n AS \"dq\" FROM dbo.t1",
    "SELECT alias = n, x = name FROM dbo.t1",
    "SELECT id FROM dbo.t1 t1 WHERE EXISTS (SELECT 1 FROM dbo.log1 l WHERE l.id = t1.id)",
    "SELECT * FROM dbo.t1 a LEFT JOIN dbo.log1 b ON a.id = b.id AND b.msg IS NOT NULL FULL OUTER JOIN dbo.t1 c ON c.id = a.id",
    "SELECT NEXT VALUE FOR dbo.seq1",
    "SELECT $PARTITION.pf(1)",
    "SELECT CAST('<a/>' AS XML).value('(/a)[1]', 'varchar(10)')",
    "SELECT geography::Point(1, 2, 4326).ToString()",
    "SELECT HASHBYTES('SHA2_256', name), CHECKSUM(name), NEWID(), SCOPE_IDENTITY(), IDENT_CURRENT('dbo.t1') FROM dbo.t1",
    "SELECT COALESCE(NULL, name), NULLIF(n, 0), ISNULL(name, N'') FROM dbo.t1",
    "SELECT name COLLATE Latin1_General_CI_AS FROM dbo.t1",
    "SELECT 1 WHERE 1 = 1 AND 'a' LIKE '%[a-c]%' ESCAPE '\\'",
    "SELECT ISJSON(name), JSON_VALUE(name, '$.a'), JSON_QUERY(name, '$.b') FROM dbo.t1",
    "SELECT GROUPING(name), GROUPING_ID(name, n) FROM dbo.t1 GROUP BY CUBE (name, n)",
    "SELECT * FROM dbo.v1",
    "ALTER TABLE dbo.t1 ADD extra INT NULL",
    "ALTER TABLE dbo.t1 ADD CONSTRAINT df_x DEFAULT 0 FOR extra",
    "EXEC dbo.p_multi @x = 1, @out = NULL",
    "EXEC sp_help 'dbo.t1'",
    "TRUNCATE TABLE dbo.log1",
    "SET IDENTITY_INSERT dbo.t1 ON",
    "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
    "USE master",
    "BACKUP DATABASE x TO DISK = 'x.bak'",
    "DBCC CHECKIDENT ('dbo.t1', NORESEED)",
    "UPDATE STATISTICS dbo.t1",
    "SELECT name FROM dbo.t1 WHERE name = @name",
    "SELECT ID FROM DBO.T1 WHERE NAME = N'x'",
    "SELECT o.[order], o.[type], o.value, o.total FROM sales.orders o JOIN dbo.t1 t ON t.id = o.id",
    "SELECT [order], type, value FROM sales.orders ORDER BY 1",
    "SELECT * FROM [srv].db.dbo.t",
    "SELECT * FROM dbo.t1 AS t TABLESAMPLE (10 PERCENT)",
    "SELECT * FROM dbo.t1 FOR SYSTEM_TIME AS OF '2020-01-01'",
    "SELECT COUNT_BIG(*), SUM(DISTINCT n), AVG(CAST(n AS FLOAT)), STDEV(n) FROM dbo.t1",
    "SELECT TOP 1 @v = name FROM dbo.t1",
    "UPDATE dbo.t1 SET @v = name, n = n + 1 WHERE id = 1",
    "UPDATE dbo.t1 SET name = s.name FROM dbo.t1 INNER JOIN (SELECT id, msg AS name FROM dbo.log1) s ON s.id = dbo.t1.id",
    "INSERT INTO dbo.t1 (name) EXEC dbo.p_multi 1",
    "INSERT INTO dbo.t1 (name) OUTPUT inserted.id INTO @t (id) VALUES ('x')",
    "DELETE FROM dbo.t1 FROM dbo.t1 JOIN dbo.log1 ON log1.id = t1.id",
    "SELECT x.name FROM (SELECT name FROM dbo.t1) x",
    "SELECT * FROM dbo.t1 CROSS JOIN dbo.log1",
    "SELECT 1 AS [a], 2 [b], 3 'c', 4 AS \"d\"",
    "SELECT * FROM OPENQUERY(srv, 'select 1')",
    "SELECT * FROM OPENROWSET(BULK 'f.csv', SINGLE_CLOB) AS r",
    "SELECT id FROM dbo.t1 WHERE n = (SELECT TOP 1 n FROM dbo.t1 ORDER BY id DESC) AND name LIKE N'%é%'",
    "SELECT CASE WHEN n = 1 THEN 'x' END AS k FROM dbo.t1 ORDER BY CASE WHEN n = 1 THEN 0 ELSE 1 END, id",
    "SELECT DATEADD(DAY, -1, CAST(GETDATE() AS DATE)) AS y, EOMONTH(GETDATE()), DATEFROMPARTS(2020, 1, 1)",
    "SELECT name, SUM(n) OVER (PARTITION BY name) AS s, RANK() OVER (ORDER BY SUM(n) DESC) FROM dbo.t1 GROUP BY name, n",
    "SELECT LEFT(name, 2), RIGHT(name, 2), REPLACE(name, 'a', 'b'), SUBSTRING(name, 1, 2), LEN(name), UPPER(name), LTRIM(RTRIM(name)), CHARINDEX('a', name), CONCAT(name, 'x'), CONCAT_WS(',', name, n) FROM dbo.t1",
    "SELECT * FROM dbo.t1 WHERE id IN (SELECT id FROM dbo.log1) AND NOT EXISTS (SELECT 1 FROM dbo.log1 WHERE msg = name)",
    "SELECT t.id FROM dbo.t1 t INNER LOOP JOIN dbo.log1 l ON t.id = l.id OPTION (HASH JOIN)",
    "SELECT GETDATE() AT TIME ZONE 'UTC' AT TIME ZONE 'Pacific Standard Time'",
    "SELECT CAST(name AS VARCHAR(10)) AS n1, CAST(n AS DECIMAL(10, 2)), CONVERT(DATETIME2(3), at, 121) FROM dbo.t1 JOIN dbo.log1 ON 1 = 1",
    "SELECT * FROM dbo.t1 WHERE n BETWEEN 1 AND 3 OR name IS NULL",
    "SELECT TRIM(name), TRANSLATE(name, 'a', 'b'), STRING_ESCAPE(name, 'json') FROM dbo.t1",
    "SELECT [t1].[id] FROM [dbo].[t1] [t1] WHERE [t1].[id] = 1",
];

#[test]
fn lint_valid_statements() {
    let e = env();
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, "dataman_msw3");
    exec(&e, &c0, &["CREATE DATABASE dataman_msw3"]);
    let Some(c) = mssql_on(&e, mssql_profile("dataman_msw3")) else { return };
    exec(
        &e,
        &c,
        &[
            "CREATE TABLE dbo.t1 (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(50), n INT DEFAULT 0, [we]]ird col] INT NULL)",
            "CREATE TABLE dbo.log1 (id INT IDENTITY PRIMARY KEY, msg NVARCHAR(MAX), at DATETIME2 DEFAULT SYSDATETIME())",
            "CREATE SCHEMA sales",
            "CREATE TABLE sales.orders (id INT, total DECIMAL(10,2), [order] INT, [type] NVARCHAR(10), value INT)",
            "CREATE VIEW dbo.v1 AS SELECT id, name FROM dbo.t1",
            "CREATE FUNCTION dbo.f_inline (@a INT) RETURNS TABLE AS RETURN (SELECT id, name FROM dbo.t1 WHERE id > @a)",
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
        known.extend(dataman_core::lint::created_names_in(src, Dialect::TSql));
    }
    // The server agrees these parse (PARSEONLY), except the one that is obsolete syntax.
    let all = VALID.join("\nGO\n");
    let pc = e.core.block_on(c.parse_check(&db, all.clone())).unwrap();
    for x in &pc {
        bad.push(format!("PARSE {} @ {:?}", x.message, &all[x.range.clone()]));
    }
    mssql_drop(&e, "dataman_msw3");
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}

/// Reads are never classified as writes (a read-only connection refuses writes by this).
#[test]
fn reads_are_reads() {
    let mut bad = Vec::new();
    for src in VALID {
        let up = src.trim_start().to_ascii_uppercase();
        let read = ["SELECT", "WITH", "TABLE ", "SHOW", "DESCRIBE", "EXPLAIN", "VALUES", "("].iter().any(|v| up.starts_with(v));
        let f = dataman_core::guard::classify_in(src, Dialect::TSql);
        let writes_inside = up.contains(" INTO ") || up.contains("ANALYZE") || up.contains("NEXT VALUE FOR");
        if read && !writes_inside && f.is_write() {
            bad.push(format!("{src} -> {:?} {}", f.effect, f.verb));
        }
        if read && !writes_inside {
            if let Some(why) = dataman_core::guard::read_only_refusal_in(src, Dialect::TSql) {
                bad.push(format!("{src} refused: {why}"));
            }
        }
    }
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}
