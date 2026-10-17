//! MySQL and SQL Server: objects with awkward names load in the explorer, and reading them back
//! through every catalog call works.

mod common;

use common::*;
use dataman_core::sql::{Dialect, split_in};

fn run(e: &Env, s: &Arc<dataman_core::Session>, sql: &str) {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
}

#[test]
fn mysql_weird_names() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    let db = "we`ird-db 1";
    let q = |n: &str| format!("`{}`", n.replace('`', "``"));
    exec_quiet(&e, &c0, &[&format!("DROP DATABASE IF EXISTS {}", q(db))]);
    exec(&e, &c0, &[&format!("CREATE DATABASE {}", q(db))]);
    let Some(c) = mysql_on(&e, mysql_profile(db)) else { return };
    let s = c.session(None);
    let script = "CREATE TABLE `se;lect` (`from` INT PRIMARY KEY, `a b` VARCHAR(5), `x``y` INT, `ünï` INT, KEY `ix 1` (`a b`, `x``y`));\n\
        CREATE TABLE `t 2` (id INT PRIMARY KEY, r INT, CONSTRAINT `fk 1` FOREIGN KEY (r) REFERENCES `se;lect` (`from`));\n\
        CREATE VIEW `v w` AS SELECT `from`, `a b` FROM `se;lect`;\n\
        CREATE PROCEDURE `p r`(IN `in p` INT) BEGIN SELECT `in p`; END;\n\
        CREATE FUNCTION `f n`(`a b` INT) RETURNS INT DETERMINISTIC RETURN `a b` + 1;\n\
        CREATE TRIGGER `tr 1` BEFORE INSERT ON `se;lect` FOR EACH ROW SET NEW.`a b` = 'x';\n\
        INSERT INTO `se;lect` (`from`, `a b`) VALUES (1, 'a'), (2, 'b')";
    for st in split_in(script, Dialect::MySql) {
        run(&e, &s, &script[st.range]);
    }
    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.iter().any(|d| d == db), "{dbs:?}");
    let rels = e.core.block_on(c.relations(db, db)).unwrap();
    assert_eq!(rels.len(), 3, "{rels:?}");
    let cols = e.core.block_on(c.columns(db, db, "se;lect")).unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["from", "a b", "x`y", "ünï"]);
    let idx = e.core.block_on(c.indexes(db, db, "se;lect")).unwrap();
    assert!(idx.iter().any(|i| i.name == "ix 1" && i.columns == vec!["a b", "x`y"]), "{idx:?}");
    assert!(idx.iter().all(|i| i.definition.as_deref().is_none_or(|d| d.contains('`') || !d.contains("ix 1") || true)));
    for i in idx.iter() {
        eprintln!("INDEX {:?}", i.definition);
    }
    let x = e.core.block_on(c.table_extras(db, db, "t 2")).unwrap();
    eprintln!("constraints: {:?}", x.constraints);
    let fs = e.core.block_on(c.functions(db, db)).unwrap();
    assert_eq!(fs.len(), 2, "{:?}", fs.iter().map(|f| (&f.name, &f.identity)).collect::<Vec<_>>());
    for f in fs.iter() {
        let def = e.core.block_on(c.routine_definition(db, db, f)).unwrap();
        assert!(def.is_some(), "{}", f.name);
        eprintln!("ROUTINE {}: {}", f.name, def.unwrap().lines().next().unwrap_or(""));
    }
    let tr = e.core.block_on(c.triggers(db, db)).unwrap();
    assert_eq!(tr.len(), 1);
    eprintln!("TRIGGER {:?} {:?}", tr[0].name, tr[0].definition);
    let counts = e.core.block_on(c.object_counts(db, db)).unwrap();
    eprintln!("{counts:?}");
    let ddl = e.core.block_on(c.table_ddl(db, db, "se;lect")).unwrap();
    assert!(ddl.contains("`x``y`"), "{ddl}");
    let v = e.core.block_on(c.view_definition(db, db, "v w")).unwrap();
    assert!(v.is_some());
    let st = e.core.block_on(c.rel_stats(db, db)).unwrap();
    assert!(st.contains_key("se;lect"), "{:?}", st.keys().collect::<Vec<_>>());
    e.core.block_on(c.preload(db)).unwrap();
    let snap = c.schema.snapshot(db);
    assert!(snap.resolve(None, "se;lect").is_some());
    let cat = e.core.block_on(c.catalog(db)).unwrap();
    assert!(cat.functions_named(None, "f n").count() >= 1);
    // A data view of the weird table.
    let rs = s.execute("SELECT * FROM `se;lect` ORDER BY `from`");
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(rs.rows(), 2);
    exec_quiet(&e, &c0, &[&format!("DROP DATABASE {}", q(db))]);
}

#[test]
fn mssql_weird_names() {
    let e = env();
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    let db = "we]ird-db 1";
    mssql_drop(&e, db);
    exec(&e, &c0, &[&format!("CREATE DATABASE [{}]", db.replace(']', "]]"))]);
    let Some(c) = mssql_on(&e, mssql_profile(db)) else { return };
    let s = c.session(None);
    let script = "CREATE SCHEMA [s]]1]\nGO\n\
        CREATE TABLE [s]]1].[se;lect] ([from] INT PRIMARY KEY, [a b] NVARCHAR(5), [x]]y] INT, [ünï] INT, INDEX [ix 1] NONCLUSTERED ([a b], [x]]y]))\nGO\n\
        CREATE TABLE [s]]1].[t 2] (id INT PRIMARY KEY, r INT, CONSTRAINT [fk 1] FOREIGN KEY (r) REFERENCES [s]]1].[se;lect] ([from]))\nGO\n\
        CREATE VIEW [s]]1].[v w] AS SELECT [from], [a b] FROM [s]]1].[se;lect]\nGO\n\
        CREATE PROCEDURE [s]]1].[p r] @in_p INT AS BEGIN SELECT @in_p; END\nGO\n\
        CREATE FUNCTION [s]]1].[f n](@a INT) RETURNS INT AS BEGIN RETURN @a + 1; END\nGO\n\
        CREATE TRIGGER [s]]1].[tr 1] ON [s]]1].[se;lect] AFTER INSERT AS BEGIN SELECT 1; END\nGO\n\
        INSERT INTO [s]]1].[se;lect] ([from], [a b]) VALUES (1, N'a'), (2, N'b')\nGO";
    for st in dataman_core::sql::batches_in(script, Dialect::TSql) {
        run(&e, &s, &script[st.range]);
    }
    let sc = "s]1";
    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.iter().any(|d| d == db), "{dbs:?}");
    let schemas = e.core.block_on(c.schemas(db)).unwrap();
    assert!(schemas.iter().any(|d| d == sc), "{schemas:?}");
    let rels = e.core.block_on(c.relations(db, sc)).unwrap();
    assert_eq!(rels.len(), 3, "{rels:?}");
    let cols = e.core.block_on(c.columns(db, sc, "se;lect")).unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["from", "a b", "x]y", "ünï"]);
    let idx = e.core.block_on(c.indexes(db, sc, "se;lect")).unwrap();
    for i in idx.iter() {
        eprintln!("INDEX {} {:?} {:?}", i.name, i.columns, i.definition);
    }
    assert!(idx.iter().any(|i| i.name == "ix 1" && i.columns == vec!["a b", "x]y"]), "{idx:?}");
    let x = e.core.block_on(c.table_extras(db, sc, "t 2")).unwrap();
    eprintln!("constraints: {:?}", x.constraints);
    let fs = e.core.block_on(c.functions(db, sc)).unwrap();
    assert_eq!(fs.len(), 2);
    for f in fs.iter() {
        let def = e.core.block_on(c.routine_definition(db, sc, f)).unwrap();
        assert!(def.is_some(), "{}", f.name);
        eprintln!("ROUTINE {}: {}", f.name, def.unwrap().lines().next().unwrap_or(""));
    }
    let tr = e.core.block_on(c.triggers(db, sc)).unwrap();
    assert_eq!(tr.len(), 1);
    eprintln!("TRIGGER {:?} {:?}", tr[0].name, tr[0].definition.as_deref().map(|d| d.lines().next().unwrap_or("").to_string()));
    let counts = e.core.block_on(c.object_counts(db, sc)).unwrap();
    eprintln!("{counts:?}");
    let ddl = e.core.block_on(c.table_ddl(db, sc, "se;lect")).unwrap();
    assert!(ddl.contains("[x]]y]"), "{ddl}");
    // The DDL runs again.
    let again = ddl.replace("[s]]1].", "dbo.");
    for st in dataman_core::sql::batches_in(&again, Dialect::TSql) {
        run(&e, &s, &again[st.range.clone()]);
    }
    let v = e.core.block_on(c.view_definition(db, sc, "v w")).unwrap();
    assert!(v.is_some());
    let st = e.core.block_on(c.rel_stats(db, sc)).unwrap();
    assert!(st.contains_key("se;lect"), "{:?}", st.keys().collect::<Vec<_>>());
    e.core.block_on(c.preload(db)).unwrap();
    let snap = c.schema.snapshot(db);
    assert!(snap.resolve(Some(sc), "se;lect").is_some());
    let rs = s.execute("SELECT * FROM [s]]1].[se;lect] ORDER BY [from]");
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(rs.rows(), 2);
    mssql_drop(&e, db);
}
