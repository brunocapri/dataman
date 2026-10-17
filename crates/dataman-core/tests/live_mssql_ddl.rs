//! SQL Server: the catalog of odd objects and the DDL generated from it runs again.

mod common;

use common::*;

fn run(e: &Env, s: &Arc<dataman_core::Session>, sql: &str) -> Arc<dataman_core::ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

#[test]
fn ddl_round_trips() {
    let e = env();
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, "dataman_msw8");
    mssql_drop(&e, "dataman_msw8b");
    exec(&e, &c0, &["CREATE DATABASE dataman_msw8", "CREATE DATABASE dataman_msw8b"]);
    let Some(c) = mssql_on(&e, mssql_profile("dataman_msw8")) else { return };
    let db = "dataman_msw8";
    let s = c.session(None);
    for sql in [
        "CREATE SCHEMA [s;1]",
        "CREATE SCHEMA sales",
        "CREATE TABLE [s;1].[we]]ird name] ([id col] INT NOT NULL, [sel;ect] NVARCHAR(10) DEFAULT N'it''s', CONSTRAINT [pk we]]ird] PRIMARY KEY ([id col]))",
        "CREATE TABLE sales.parent (a INT NOT NULL, b NVARCHAR(20) NOT NULL, CONSTRAINT pk_parent PRIMARY KEY CLUSTERED (a, b), CONSTRAINT uq_b UNIQUE (b))",
        "CREATE TABLE sales.child (id INT IDENTITY(10,5) PRIMARY KEY, pa INT NOT NULL, pb NVARCHAR(20) NOT NULL, \
            CONSTRAINT fk_child FOREIGN KEY (pa, pb) REFERENCES sales.parent (a, b) ON DELETE CASCADE ON UPDATE NO ACTION)",
        "CREATE TABLE dbo.gen (id INT PRIMARY KEY, w INT NOT NULL, h INT NOT NULL, area AS (w * h) PERSISTED, vol AS (w + h), \
            created DATETIME2(3) CONSTRAINT df_created DEFAULT SYSDATETIME(), st NVARCHAR(5) CONSTRAINT ck_st CHECK (st IN (N'a', N'b')), \
            money_c MONEY NOT NULL DEFAULT 0, num DECIMAL(18,4) SPARSE NULL, doc NVARCHAR(MAX), xm XML, vb VARBINARY(MAX), uid UNIQUEIDENTIFIER DEFAULT NEWID(), \
            rv ROWVERSION, hid HIERARCHYID, geo GEOGRAPHY, CONSTRAINT ck_w CHECK (w >= 0))",
        "CREATE NONCLUSTERED INDEX ix_inc ON dbo.gen (w DESC, h) INCLUDE (st, doc) WHERE w > 5",
        "CREATE UNIQUE INDEX ux_filtered ON dbo.gen (st) WHERE st IS NOT NULL",
        "CREATE VIEW dbo.v_gen AS SELECT id, area FROM dbo.gen WHERE w > 1",
        "CREATE FUNCTION dbo.f_scalar (@a INT = 3) RETURNS INT AS BEGIN RETURN @a * 2; END",
        "CREATE FUNCTION dbo.f_inline (@a INT) RETURNS TABLE AS RETURN (SELECT id FROM dbo.gen WHERE id > @a)",
        "CREATE PROCEDURE dbo.p_out @x INT, @y INT OUTPUT, @z NVARCHAR(10) = N'dflt' AS BEGIN SET @y = @x; SELECT 1 AS a; END",
        "CREATE TRIGGER dbo.trg_gen ON dbo.gen AFTER INSERT AS BEGIN SET NOCOUNT ON; SELECT 1; END",
        "CREATE SEQUENCE dbo.seq1 AS BIGINT START WITH 10 INCREMENT BY 5",
        "CREATE SYNONYM dbo.syn1 FOR dbo.gen",
        "CREATE TYPE dbo.my_int FROM INT NOT NULL",
    ] {
        run(&e, &s, sql);
    }
    let schemas = e.core.block_on(c.schemas(db)).unwrap();
    eprintln!("schemas: {schemas:?}");
    let mut bad = Vec::new();
    let c2 = mssql_on(&e, mssql_profile("dataman_msw8b")).unwrap();
    let s2 = c2.session(None);
    for sql in ["CREATE SCHEMA [s;1]", "CREATE SCHEMA sales"] {
        run(&e, &s2, sql);
    }
    let mut tables: Vec<(String, String)> = Vec::new();
    for sc in schemas.iter().filter(|s| matches!(s.as_str(), "dbo" | "sales" | "s;1")) {
        for r in e.core.block_on(c.relations(db, sc)).unwrap_or_default() {
            eprintln!("rel {sc}.{} {:?}", r.name, r.kind);
            tables.push((sc.clone(), r.name));
        }
    }
    // Parents before children.
    tables.sort_by_key(|(sc, n)| (n == "child", n.starts_with("v_"), sc.clone()));
    for (sc, n) in &tables {
        if n.starts_with("v_") {
            let v = e.core.block_on(c.view_definition(db, sc, n)).unwrap().unwrap();
            eprintln!("VIEW {sc}.{n}\n{v}\n");
            continue;
        }
        let ddl = e.core.block_on(c.table_ddl(db, sc, n)).unwrap();
        eprintln!("{ddl}\n");
        // The DDL's batches (it may carry GO lines).
        for st in dataman_core::sql::batches_in(&ddl, dataman_core::sql::Dialect::TSql) {
            let sql = &ddl[st.range];
            let rs = s2.execute(sql);
            let ph = e.core.block_on(rs.wait_finished());
            if ph != Phase::Done {
                bad.push(format!("{sc}.{n}: {:?}\n{sql}", rs.status().error));
            }
        }
    }
    let fs = e.core.block_on(c.functions(db, "dbo")).unwrap();
    for f in fs.iter() {
        let def = e.core.block_on(c.routine_definition(db, "dbo", f)).unwrap();
        eprintln!("ROUTINE {} {:?} {:?}: {}", f.name, f.kind, f.identity, def.as_deref().unwrap_or("<none>").lines().next().unwrap_or(""));
        match def {
            Some(d) => {
                // Runs again in the copy (alter/create).
                for st in dataman_core::sql::batches_in(&d, dataman_core::sql::Dialect::TSql) {
                    let sql = &d[st.range];
                    let rs = s2.execute(sql);
                    let ph = e.core.block_on(rs.wait_finished());
                    if ph != Phase::Done {
                        bad.push(format!("routine {}: {:?}\n{sql}", f.name, rs.status().error));
                    }
                }
            }
            None => bad.push(format!("routine {}: no definition", f.name)),
        }
    }
    let tr = e.core.block_on(c.triggers(db, "dbo")).unwrap();
    eprintln!("triggers: {:?}", tr.iter().map(|t| (&t.name, &t.table, &t.events)).collect::<Vec<_>>());
    let counts = e.core.block_on(c.object_counts(db, "dbo")).unwrap();
    eprintln!("counts: {counts:?}");
    mssql_drop(&e, "dataman_msw8");
    mssql_drop(&e, "dataman_msw8b");
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}
