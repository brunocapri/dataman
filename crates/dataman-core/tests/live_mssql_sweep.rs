mod common;

use common::*;
use dataman_core::lint::{LintContext, Severity, lint};
use dataman_core::sql::{Dialect, batches_in, split_in};
use dataman_core::{ResultSet, Session};

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(120), rs.wait_finished()).await.expect("timeout") });
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

fn script(e: &Env, s: &Arc<Session>, src: &str) -> Vec<(String, String)> {
    let mut bad = Vec::new();
    for st in batches_in(src, Dialect::TSql) {
        let sql = &src[st.range];
        let rs = run(e, s, sql);
        let all = results(e, &rs);
        for r in &all {
            if r.phase() != Phase::Done {
                bad.push((sql.chars().take(200).collect(), format!("{:?}", r.status().error)));
            }
        }
    }
    bad
}

fn fresh(e: &Env, db: &str) -> Option<Arc<Connection>> {
    let c = mssql_on(e, mssql_profile("master"))?;
    mssql_drop(e, db);
    exec(e, &c, &[&format!("CREATE DATABASE [{db}]")]);
    mssql_on(e, mssql_profile(db))
}

fn cell(e: &Env, s: &Arc<Session>, sql: &str) -> Option<String> {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs.snapshot().text(0, 0).map(|s| s.into_owned())
}

const SCRIPT: &str = r#"
SET NOCOUNT ON;
CREATE TABLE dbo.t1 (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(50), n INT DEFAULT 0, [we]]ird col] INT NULL)
GO
CREATE TABLE dbo.log1 (id INT IDENTITY PRIMARY KEY, msg NVARCHAR(MAX), at DATETIME2 DEFAULT SYSDATETIME())
go
-- GO inside a comment
/* and GO
GO
in a block comment /* nested */ still comment */
INSERT INTO dbo.t1 (name, n) VALUES (N'go', 1), (N'GO', 2), ('a;b', 3), (N'it''s', 4)
SELECT 'GO' AS word, N'
GO
' AS multiline, [we]]ird col] FROM dbo.t1
GO 3
CREATE PROCEDURE dbo.p_multi @x INT, @out INT OUTPUT
AS
BEGIN
  SET NOCOUNT ON;
  SELECT @x AS a;
  SELECT @x * 2 AS b, 'two' AS s;
  SET @out = @x + 100;
  PRINT 'printing from the proc';
  RAISERROR('info msg %d', 0, 1, @x) WITH NOWAIT;
END
GO
CREATE FUNCTION dbo.f_scalar (@a INT) RETURNS INT
AS
BEGIN
  RETURN @a * 2;
END
GO
CREATE FUNCTION dbo.f_inline (@a INT) RETURNS TABLE
AS RETURN (SELECT id, name FROM dbo.t1 WHERE id > @a)
GO
CREATE FUNCTION dbo.f_multi (@a INT) RETURNS @r TABLE (id INT, name NVARCHAR(50))
AS
BEGIN
  INSERT INTO @r SELECT id, name FROM dbo.t1 WHERE id > @a;
  RETURN;
END
GO
CREATE VIEW dbo.v1 AS SELECT id, name FROM dbo.t1
GO
CREATE TRIGGER dbo.trg1 ON dbo.t1 AFTER INSERT, UPDATE
AS
BEGIN
  SET NOCOUNT ON;
  INSERT INTO dbo.log1 (msg) SELECT CONCAT(N'ins ', name) FROM inserted;
END
GO
DECLARE @out INT;
EXEC dbo.p_multi @x = 5, @out = @out OUTPUT;
SELECT @out AS out_val;
SELECT dbo.f_scalar(4) AS d, * FROM dbo.f_inline(1);
SELECT * FROM dbo.f_multi(0)
GO
BEGIN TRY
  SELECT 1/0;
END TRY
BEGIN CATCH
  SELECT ERROR_NUMBER() AS num, ERROR_MESSAGE() AS msg;
  PRINT 'caught; with semi';
END CATCH
GO
BEGIN TRY
  BEGIN TRAN;
  INSERT INTO dbo.t1 (name) VALUES (N'tx');
  SAVE TRAN sp1;
  INSERT INTO dbo.t1 (name) VALUES (N'tx2');
  ROLLBACK TRAN sp1;
  COMMIT TRAN;
END TRY
BEGIN CATCH
  IF @@TRANCOUNT > 0 ROLLBACK;
END CATCH
GO
CREATE TABLE #tmp (a INT, b NVARCHAR(10));
INSERT #tmp VALUES (1, 'x'), (2, 'y')
SELECT * FROM #tmp
DECLARE @tv TABLE (k INT, v NVARCHAR(10))
INSERT INTO @tv SELECT a, b FROM #tmp
SELECT * FROM @tv
SELECT * INTO ##glob FROM #tmp
DROP TABLE #tmp
GO
SELECT * FROM ##glob
DROP TABLE ##glob
GO
WITH c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM c WHERE n < 5) SELECT SUM(n) AS s FROM c OPTION (MAXRECURSION 10)
SELECT id, name, ROW_NUMBER() OVER (ORDER BY id) rn, LAG(id) OVER (ORDER BY id) prev FROM dbo.t1
GO
DISABLE TRIGGER dbo.trg1 ON dbo.t1;
GO
MERGE dbo.t1 AS tgt
USING (VALUES (1, N'merged'), (100, N'new')) AS src (id, nm) ON tgt.id = src.id
WHEN MATCHED THEN UPDATE SET name = src.nm
WHEN NOT MATCHED BY TARGET THEN INSERT (name) VALUES (src.nm)
OUTPUT $action, inserted.id, inserted.name, deleted.name;
GO
DECLARE @sql NVARCHAR(MAX) = N'SELECT @r = COUNT(*) FROM dbo.t1; SELECT 1 AS one';
DECLARE @r INT;
EXEC sp_executesql @sql, N'@r INT OUTPUT', @r = @r OUTPUT;
SELECT @r AS cnt
GO
DECLARE @i INT = 0
WHILE @i < 3
BEGIN
  SET @i += 1
  IF @i = 2 CONTINUE
  PRINT 'i=' + CAST(@i AS VARCHAR(10))
END
SELECT CASE WHEN @i = 3 THEN 'three' ELSE 'other' END AS c
GO
SELECT TOP (2) WITH TIES id FROM dbo.t1 ORDER BY id
SELECT id FROM dbo.t1 ORDER BY id OFFSET 1 ROWS FETCH NEXT 2 ROWS ONLY
SELECT * FROM (SELECT id, name FROM dbo.t1) AS s PIVOT (COUNT(id) FOR name IN ([a;b], [go])) p
SELECT value FROM STRING_SPLIT('a,b,c', ',')
SELECT * FROM OPENJSON('[{"a":1,"b":"x"},{"a":2,"b":"y"}]') WITH (a INT '$.a', b NVARCHAR(10) '$.b')
SELECT t.name, c.value FROM dbo.t1 t CROSS APPLY (SELECT TOP 1 value FROM STRING_SPLIT(t.name, 'a')) c
GO
CREATE SEQUENCE dbo.seq1 START WITH 10 INCREMENT BY 5;
SELECT NEXT VALUE FOR dbo.seq1 AS nv
GO
IF OBJECT_ID('dbo.nothing', 'U') IS NOT NULL DROP TABLE dbo.nothing
ELSE PRINT 'none'
GO
EXEC('SELECT 1 AS dyn; SELECT 2 AS dyn2')
SELECT 'last' AS l
"#;

#[test]
fn big_script_runs_cleanly() {
    let e = env();
    let Some(c) = fresh(&e, "dataman_msw1") else { return };
    let s = c.session(None);
    let src_owned = SCRIPT.replace("##glob", "##glob_a");
    let src = src_owned.as_str();
    for st in batches_in(src, Dialect::TSql) {
        eprintln!("--- {}", src[st.range.clone()].lines().next().unwrap_or(""));
    }
    let bad = script(&e, &s, src);
    drop(s);
    drop(c);
    mssql_drop(&e, "dataman_msw1");
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn lint_and_parse_check_accept_valid_scripts() {
    let e = env();
    let Some(c) = fresh(&e, "dataman_msw2") else { return };
    let s = c.session(None);
    let _ = script(&e, &s, &SCRIPT.replace("##glob", "##glob_b"));
    let db = c.database().to_string();
    let _ = e.core.block_on(c.preload(&db));
    let snap = c.schema.snapshot(&db);
    let l = lint(SCRIPT, &LintContext { schema: Some(&snap), ..Default::default() });
    let mut bad = Vec::new();
    for x in l.iter().filter(|x| matches!(x.severity, Severity::Error)) {
        bad.push(format!("LINT {} @ {:?}", x.message, &SCRIPT[x.range.clone()]));
    }
    let pc = e.core.block_on(c.parse_check(&db, SCRIPT.to_string())).unwrap();
    for x in &pc {
        bad.push(format!("PARSE {} @ {:?}", x.message, &SCRIPT[x.range.clone()]));
    }
    drop(s);
    drop(c);
    mssql_drop(&e, "dataman_msw2");
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
    let _ = split_in;
}

const SSMS: &str = r#"
USE [master]
GO
/****** Object:  Database [dataman_msw4]    Script Date: 1/1/2026 ******/
CREATE DATABASE [dataman_msw4]
GO
ALTER DATABASE [dataman_msw4] SET COMPATIBILITY_LEVEL = 150
GO
ALTER DATABASE [dataman_msw4] SET ANSI_NULL_DEFAULT OFF
GO
USE [dataman_msw4]
GO
/****** Object:  Table [dbo].[Customers]    Script Date: 1/1/2026 ******/
SET ANSI_NULLS ON
GO
SET QUOTED_IDENTIFIER ON
GO
CREATE TABLE [dbo].[Customers](
	[Id] [int] IDENTITY(1,1) NOT NULL,
	[Name] [nvarchar](50) NOT NULL,
	[Notes] [nvarchar](max) NULL,
 CONSTRAINT [PK_Customers] PRIMARY KEY CLUSTERED
(
	[Id] ASC
)WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF, IGNORE_DUP_KEY = OFF, ALLOW_ROW_LOCKS = ON, ALLOW_PAGE_LOCKS = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = OFF) ON [PRIMARY]
) ON [PRIMARY] TEXTIMAGE_ON [PRIMARY]
GO
SET IDENTITY_INSERT [dbo].[Customers] ON
GO
INSERT [dbo].[Customers] ([Id], [Name], [Notes]) VALUES (1, N'Ana', N'line1
GO
line3 ''quoted''')
GO
INSERT [dbo].[Customers] ([Id], [Name], [Notes]) VALUES (2, N'Bob', NULL)
GO
SET IDENTITY_INSERT [dbo].[Customers] OFF
GO
ALTER TABLE [dbo].[Customers] ADD  CONSTRAINT [DF_Customers_Notes]  DEFAULT (N'n/a') FOR [Notes]
GO
/****** Object:  StoredProcedure [dbo].[GetCustomer]    Script Date: 1/1/2026 ******/
SET ANSI_NULLS ON
GO
SET QUOTED_IDENTIFIER ON
GO
IF NOT EXISTS (SELECT * FROM sys.objects WHERE object_id = OBJECT_ID(N'[dbo].[GetCustomer]') AND type in (N'P', N'PC'))
BEGIN
EXEC dbo.sp_executesql @statement = N'CREATE PROCEDURE [dbo].[GetCustomer]
	@Id int
AS
BEGIN
	SET NOCOUNT ON;
	SELECT * FROM dbo.Customers WHERE Id = @Id;
	-- GO in a comment
END
'
END
GO
EXEC sys.sp_addextendedproperty @name=N'MS_Description', @value=N'The customers' , @level0type=N'SCHEMA',@level0name=N'dbo', @level1type=N'TABLE',@level1name=N'Customers'
GO
EXEC [dbo].[GetCustomer] @Id = 1
GO
USE [master]
GO
ALTER DATABASE [dataman_msw4] SET  READ_WRITE
GO
"#;

#[test]
fn ssms_script_runs() {
    let e = env();
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, "dataman_msw4");
    let s = c0.session(None);
    let bad = script(&e, &s, SSMS);
    let n = batches_in(SSMS, Dialect::TSql).len();
    let check = cell(&e, &s, "SELECT COUNT(*) FROM [dataman_msw4].dbo.Customers WHERE Notes LIKE N'line1%line3%'");
    mssql_drop(&e, "dataman_msw4");
    assert!(bad.is_empty(), "{n} batches: {bad:#?}");
    assert_eq!(check.as_deref(), Some("1"));
}

#[test]
fn no_phantom_params() {
    let mut bad = Vec::new();
    for st in batches_in(SCRIPT, Dialect::TSql) {
        let sql = &SCRIPT[st.range];
        if let Some(p) = dataman_core::params::detect_tsql(sql) {
            bad.push(format!("{:?} in {}", p.params.iter().map(|p| p.key.clone()).collect::<Vec<_>>(), sql.lines().next().unwrap_or("")));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}
