//! SQL Server: grid edits of every column type, keys, quoting.

mod common;

use common::*;
use dataman_core::edit::{CellValue, ChangeSet, Editability, Outcome};

fn run(e: &Env, s: &Arc<dataman_core::Session>, sql: &str) -> Arc<dataman_core::ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

#[test]
fn edit_every_type() {
    let e = env();
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, "dataman_msw6");
    exec(&e, &c0, &["CREATE DATABASE dataman_msw6"]);
    let Some(c) = mssql_on(&e, mssql_profile("dataman_msw6")) else { return };
    let db = "dataman_msw6";
    let s = c.session(None);
    for sql in [
        "CREATE TABLE dbo.[we]]ird ] ([id col] INT IDENTITY(1,1) PRIMARY KEY, bt BIT, ti TINYINT, bi BIGINT, r REAL, f FLOAT, d1 DECIMAL(38,10), mo MONEY, sm SMALLMONEY, \
          dt DATETIME, dt2 DATETIME2(7), sdt SMALLDATETIME, d DATE, t TIME(7), dto DATETIMEOFFSET(7), \
          ch CHAR(5), nv NVARCHAR(20), vb VARBINARY(10), uid UNIQUEIDENTIFIER, x XML, [na;me] NVARCHAR(30), vm NVARCHAR(MAX))",
        "INSERT INTO dbo.[we]]ird ] (bt, ti, bi, r, f, d1, mo, sm, dt, dt2, sdt, d, t, dto, ch, nv, vb, uid, x, [na;me], vm) VALUES \
         (1, 200, 123456789012, 0.1, 0.30000000000000004, 12.3456789012, 12.3456, 1.5, '2024-01-02 03:04:05.003', '2024-01-02 03:04:05.1234567', '2024-01-02 03:04:00', '2024-01-02', '03:04:05.1234567', '2024-01-02 03:04:05.1234567 +05:30', \
          'ab', N'x''y', 0xCAFE, 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', '<a/>', N'semi;colon', N'long')",
    ] {
        run(&e, &s, sql);
    }
    let rs = run(&e, &s, "SELECT * FROM dbo.[we]]ird ]");
    let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    let snap = rs.snapshot();
    let cols = rs.columns().unwrap();
    let mut bad = Vec::new();
    for (ix, col) in cols.iter().enumerate() {
        if col.name == "id col" || col.name == "x" {
            continue;
        }
        let old = snap.text(0, ix).map(|s| s.into_owned());
        // Edit each column to itself's text (no change of meaning), and to something else.
        let new = match col.name.as_str() {
            "bt" => "false",
            "ti" => "7",
            "bi" => "5",
            "r" => "2.5",
            "f" => "2.5",
            "d1" => "7.5",
            "mo" | "sm" => "7.5",
            "dt" => "2025-01-01 00:00:00.003",
            "dt2" => "2025-01-01 00:00:00.1234567",
            "sdt" => "2025-01-01 00:00:00",
            "d" => "2025-01-01",
            "t" => "01:02:03.1234567",
            "dto" => "2025-01-01 00:00:00 +01:00",
            "ch" => "zz",
            "nv" | "na;me" | "vm" => "it's \\ \"q\"; --",
            "vb" => "\\xbeef",
            "uid" => "00000000-0000-0000-0000-000000000001",
            _ => continue,
        };
        let mut cs = ChangeSet::new();
        cs.edit(0, ix, CellValue::text(old.clone().unwrap()), CellValue::text(new));
        let plan = cs.plan(&t, &snap).unwrap();
        let r = e.core.block_on(c.apply_changes(Some(db), plan.clone())).unwrap();
        let ok = r.committed && r.outcomes.iter().all(|o| matches!(&o.outcome, Outcome::Applied { rows: 1 }));
        if !ok {
            bad.push(format!("{} ({:?}): {}\n   {:?}", col.name, old, plan[0].sql, r.outcomes.iter().map(|o| format!("{:?}", o.outcome)).collect::<Vec<_>>()));
        }
    }
    // Every column edited again from what it shows now (the text the grid shows is the guard).
    let rs = run(&e, &s, "SELECT * FROM dbo.[we]]ird ]");
    let snap = rs.snapshot();
    for (ix, col) in cols.iter().enumerate() {
        if col.name == "id col" || col.name == "x" {
            continue;
        }
        let Some(now) = snap.text(0, ix).map(|x| x.into_owned()) else { continue };
        let mut cs = ChangeSet::new();
        cs.edit(0, ix, CellValue::text(now.clone()), CellValue::text(now.clone()));
        let plan = cs.plan(&t, &snap).unwrap();
        if plan.is_empty() {
            continue;
        }
        let r = e.core.block_on(c.apply_changes(Some(db), plan.clone())).unwrap();
        let ok = r.committed && r.outcomes.iter().all(|o| matches!(&o.outcome, Outcome::Applied { .. }));
        if !ok {
            bad.push(format!("same-value {} ({:?}): {}\n   {:?}", col.name, now, plan[0].sql, r.outcomes.iter().map(|o| format!("{:?}", o.outcome)).collect::<Vec<_>>()));
        }
    }
    mssql_drop(&e, db);
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}

#[test]
fn edits_on_odd_tables() {
    let e = env();
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, "dataman_msw7");
    exec(&e, &c0, &["CREATE DATABASE dataman_msw7"]);
    let Some(c) = mssql_on(&e, mssql_profile("dataman_msw7")) else { return };
    let db = "dataman_msw7";
    let s = c.session(None);
    for sql in [
        "CREATE SCHEMA [s;1]",
        "CREATE TABLE [s;1].comp (a INT NOT NULL, b NVARCHAR(10) NOT NULL, v NVARCHAR(MAX), n FLOAT, PRIMARY KEY (a, b))",
        "CREATE TABLE dbo.nokey (a INT, b VARCHAR(10))",
        "CREATE TABLE dbo.auto (id BIGINT IDENTITY PRIMARY KEY, t NVARCHAR(10) DEFAULT 'dflt', ts DATETIME2 DEFAULT SYSDATETIME(), rv ROWVERSION)",
        "CREATE TABLE dbo.onlyid (id INT IDENTITY PRIMARY KEY)",
        "CREATE UNIQUE INDEX ux_a ON dbo.nokey (a) WHERE a IS NOT NULL",
        "INSERT INTO [s;1].comp VALUES (1, N'x', N'v1', 1.5), (1, N'y', N'v2', NULL), (2, N'x', NULL, 3)",
    ] {
        run(&e, &s, sql);
    }
    let rs = run(&e, &s, "SELECT * FROM [s;1].comp");
    let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    let snap = rs.snapshot();
    let mut cs = ChangeSet::new();
    cs.edit(1, 2, CellValue::text("v2"), CellValue::text("new;v'1"));
    cs.edit(2, 3, CellValue::text("3"), CellValue::Null);
    cs.delete_row(0);
    let plan = cs.plan(&t, &snap).unwrap();
    for p in &plan {
        eprintln!("PLAN: {}", p.sql);
    }
    let r = e.core.block_on(c.apply_changes(Some(db), plan)).unwrap();
    assert!(r.committed, "{r:?}");
    assert_eq!(run(&e, &s, "SELECT COUNT(*) FROM [s;1].comp").snapshot().text(0, 0).as_deref(), Some("2"));
    for (table, expect_editable) in [("dbo.nokey", false), ("dbo.auto", true), ("dbo.onlyid", true)] {
        let rs = run(&e, &s, &format!("SELECT * FROM {table}"));
        let r = e.core.block_on(c.edit_target(db, &rs, None)).unwrap();
        eprintln!("{table}: editable={}", matches!(r, Editability::Editable(_)));
        let Editability::Editable(t) = r else {
            assert!(!expect_editable, "{table}");
            continue;
        };
        let snap = rs.snapshot();
        let mut cs = ChangeSet::new();
        let i = cs.insert_row(snap.columns.len());
        if table == "dbo.auto" {
            cs.set_inserted(i, 1, Some(CellValue::text("o'k")));
        }
        let plan = cs.plan(&t, &snap).unwrap();
        for p in &plan {
            eprintln!("INS: {}", p.sql);
        }
        let r = e.core.block_on(c.apply_changes(Some(db), plan)).unwrap();
        assert!(r.committed, "{table}: {r:?}");
    }
    mssql_drop(&e, db);
}
