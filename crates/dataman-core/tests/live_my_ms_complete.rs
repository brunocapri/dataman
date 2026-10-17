//! Autocomplete, signature help, definitions and lint at every cursor of real scripts: never a
//! panic (char boundaries, unicode), on MySQL and SQL Server.

mod common;

use common::*;
use dataman_core::complete::{complete, definition_at, signature_help};
use dataman_core::lint::{LintContext, lint};
use dataman_core::sql::{Dialect, tokenize_in};

const MY: &str = "-- héllo 日本語 😀\nCREATE TABLE `tö;1` (id INT, `nämé` VARCHAR(5));\nSELECT t.`nämé`, COUNT(*) FROM `tö;1` t JOIN dataman_cx.o ON o.id = t.id WHERE t.id IN (SELECT id FROM o) AND t.`nämé` = 'ünï\\'cödé; 😀' GROUP BY 1 HAVING COUNT(*) > 1 ORDER BY 2 DESC LIMIT 5;\nINSERT INTO o (id, v) VALUES (1, 'x') ON DUPLICATE KEY UPDATE v = VALUES(v);\nUPDATE o SET v = CONCAT(v, '日本') WHERE id = 1;\nDELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 'é'; END//\nDELIMITER ;\nCALL p();\nSELECT DATE_ADD(NOW(), INTERVAL 1 DAY), JSON_EXTRACT(v, '$.a') FROM o";

const MS: &str = "-- héllo 日本語 😀\nCREATE TABLE [tö;1] (id INT, [nämé] NVARCHAR(5]]x))\nGO\nSELECT t.[nämé], COUNT(*) FROM [tö;1] t JOIN dbo.o ON o.id = t.id WHERE t.id IN (SELECT id FROM o) AND t.[nämé] = N'ünï''cödé; 😀' GROUP BY t.[nämé] HAVING COUNT(*) > 1 ORDER BY 2 DESC\nINSERT INTO o (id, v) OUTPUT inserted.id VALUES (1, 'x')\nUPDATE o SET v = v + N'日本' WHERE id = 1\nGO\nDECLARE @x INT = 1; SELECT DATEADD(day, @x, GETDATE()), JSON_VALUE(v, '$.a') FROM o CROSS APPLY dbo.f(o.id) AS f";

fn sweep(src: &str, snap: &dataman_core::schema::SchemaSnapshot, d: Dialect) -> usize {
    let tokens = tokenize_in(src, d);
    let mut n = 0;
    for (cursor, _) in src.char_indices().chain(std::iter::once((src.len(), ' '))) {
        let c = complete(src, cursor, snap);
        n += c.items.len();
        let _ = signature_help(src, &tokens, cursor, snap);
        let _ = definition_at(src, cursor, snap);
        // Every prefix of the script is a text being typed.
        let _ = lint(&src[..cursor], &LintContext { schema: Some(snap), ..Default::default() });
    }
    n
}

#[test]
fn mysql_cursor_sweep() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    exec_quiet(&e, &c0, &["DROP DATABASE IF EXISTS dataman_cx", "CREATE DATABASE dataman_cx", "CREATE TABLE dataman_cx.o (id INT PRIMARY KEY, v JSON)"]);
    let Some(c) = mysql_on(&e, mysql_profile("dataman_cx")) else { return };
    let _ = e.core.block_on(c.preload("dataman_cx"));
    let snap = c.schema.snapshot("dataman_cx");
    assert!(sweep(MY, &snap, Dialect::MySql) > 100);
    exec_quiet(&e, &c0, &["DROP DATABASE dataman_cx"]);
}

#[test]
fn mssql_cursor_sweep() {
    let e = env();
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, "dataman_cx");
    exec(&e, &c0, &["CREATE DATABASE dataman_cx"]);
    let Some(c) = mssql_on(&e, mssql_profile("dataman_cx")) else { return };
    exec(&e, &c, &["CREATE TABLE dbo.o (id INT PRIMARY KEY, v NVARCHAR(MAX))", "CREATE FUNCTION dbo.f (@a INT) RETURNS TABLE AS RETURN (SELECT @a AS a)"]);
    let _ = e.core.block_on(c.preload("dataman_cx"));
    let snap = c.schema.snapshot("dataman_cx");
    assert!(sweep(MS, &snap, Dialect::TSql) > 100);
    mssql_drop(&e, "dataman_cx");
}
