//! MySQL: DDL generated from the catalog runs again; edits on odd tables.

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
fn ddl_round_trips() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    exec_quiet(&e, &c0, &["DROP DATABASE IF EXISTS dataman_sw6", "DROP DATABASE IF EXISTS dataman_sw6b", "CREATE DATABASE dataman_sw6", "CREATE DATABASE dataman_sw6b"]);
    let Some(c) = mysql_on(&e, mysql_profile("dataman_sw6")) else { return };
    let s = c.session(None);
    let tables = [
        "CREATE TABLE `we``ird name` (`id col` INT NOT NULL, `sel;ect` VARCHAR(10) DEFAULT 'it''s', PRIMARY KEY (`id col`))",
        "CREATE TABLE parent (a INT NOT NULL, b VARCHAR(20) NOT NULL, PRIMARY KEY (a, b), UNIQUE KEY uq_b (b), KEY ix_pre (b(5)))",
        "CREATE TABLE child (id INT AUTO_INCREMENT PRIMARY KEY, pa INT NOT NULL, pb VARCHAR(20) NOT NULL, \
            CONSTRAINT fk_child FOREIGN KEY (pa, pb) REFERENCES parent (a, b) ON DELETE RESTRICT ON UPDATE CASCADE) ENGINE=InnoDB",
        "CREATE TABLE gen (id INT PRIMARY KEY, w INT, h INT, area INT AS (w * h) STORED, vol INT GENERATED ALWAYS AS (w + h) VIRTUAL, \
            doc JSON, dk VARCHAR(20) AS (JSON_UNQUOTE(JSON_EXTRACT(doc, '$.k'))) VIRTUAL, \
            created TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, \
            d2 DATETIME(3) DEFAULT CURRENT_TIMESTAMP(3), e INT DEFAULT (w + 1), \
            st ENUM('a''b','c,d','e') NOT NULL DEFAULT 'c,d', flags SET('x','y') DEFAULT 'x,y', \
            note TEXT COMMENT 'it''s a \"note\"; ok', num DECIMAL(10,2) UNSIGNED ZEROFILL, \
            CONSTRAINT chk_w CHECK (w >= 0), INDEX ix_desc (w DESC, h), INDEX ix_fn ((w + h)), INDEX ix_json ((CAST(doc->'$.k' AS CHAR(10)) COLLATE utf8mb4_bin)), \
            FULLTEXT KEY ft_note (note)) COMMENT='table; comment' ROW_FORMAT=DYNAMIC",
        "CREATE TABLE sp (id INT PRIMARY KEY, g GEOMETRY NOT NULL SRID 0, p POINT, SPATIAL INDEX sx (g))",
        "CREATE TABLE nokey (a INT, b VARCHAR(10), c TEXT)",
        "CREATE TABLE part (id INT NOT NULL, d DATE NOT NULL, PRIMARY KEY (id, d)) PARTITION BY RANGE (YEAR(d)) (PARTITION p0 VALUES LESS THAN (2020), PARTITION p1 VALUES LESS THAN MAXVALUE)",
        "CREATE VIEW v_gen AS SELECT id, area FROM gen WHERE w > 1",
    ];
    for t in tables {
        run(&e, &s, t);
    }
    let c2 = mysql_on(&e, mysql_profile("dataman_sw6b")).unwrap();
    let s2 = c2.session(None);
    let rels = e.core.block_on(c.relations("dataman_sw6", "dataman_sw6")).unwrap();
    assert_eq!(rels.len(), 8, "{rels:?}");
    // Parents before children.
    let mut order: Vec<&str> = rels.iter().map(|r| r.name.as_str()).collect();
    order.sort_by_key(|n| (*n == "child", *n == "v_gen"));
    let mut bad = Vec::new();
    for n in order {
        if n == "v_gen" {
            let v = e.core.block_on(c.view_definition("dataman_sw6", "dataman_sw6", n)).unwrap().unwrap();
            eprintln!("VIEW\n{v}");
            let rs = s2.execute(v.replace("dataman_sw6.", ""));
            e.core.block_on(rs.wait_finished());
            continue;
        }
        let ddl = e.core.block_on(c.table_ddl("dataman_sw6", "dataman_sw6", n)).unwrap();
        eprintln!("{ddl}\n");
        let sqls = dataman_core::sql::split_in(&ddl, dataman_core::sql::Dialect::MySql);
        for st in sqls {
            let sql = &ddl[st.range];
            let rs = s2.execute(sql);
            let ph = e.core.block_on(rs.wait_finished());
            if ph != Phase::Done {
                bad.push(format!("{n}: {:?}\n{sql}", rs.status().error));
            }
        }
    }
    exec_quiet(&e, &c0, &["DROP DATABASE dataman_sw6", "DROP DATABASE dataman_sw6b"]);
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}

#[test]
fn edits_on_odd_tables() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    exec_quiet(&e, &c0, &["DROP DATABASE IF EXISTS dataman_sw7", "CREATE DATABASE dataman_sw7"]);
    let Some(c) = mysql_on(&e, mysql_profile("dataman_sw7")) else { return };
    let db = "dataman_sw7";
    let s = c.session(None);
    for t in [
        "CREATE TABLE `we``ird` (`id col` INT NOT NULL, `na;me` VARCHAR(30), `x` INT, PRIMARY KEY (`id col`))",
        "CREATE TABLE comp (a INT NOT NULL, b VARCHAR(10) NOT NULL, v TEXT, n DOUBLE, PRIMARY KEY (a, b))",
        "CREATE TABLE nokey (a INT, b VARCHAR(10), c TEXT)",
        "CREATE TABLE auto (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, t VARCHAR(10) DEFAULT 'dflt', ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
        "INSERT INTO `we``ird` VALUES (1, 'a', 1), (2, 'b;c', 2)",
        "INSERT INTO comp VALUES (1, 'x', 'v1', 1.5), (1, 'y', 'v2', NULL), (2, 'x', NULL, 3)",
        "INSERT INTO nokey VALUES (1, 'a', 'c1'), (1, 'a', 'c1'), (NULL, NULL, NULL)",
    ] {
        run(&e, &s, t);
    }
    for (table, edit_col, old, new) in [
        ("`we``ird`", "na;me", "b;c", "it's \\ \"q\""),
        ("comp", "v", "v1", "new;v'1"),
        ("comp", "n", "1.5", "2.25"),
        ("nokey", "b", "a", "z"),
    ] {
        let rs = run(&e, &s, &format!("SELECT * FROM {table}"));
        let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
            Editability::Editable(t) => t,
            Editability::ReadOnly(why) => {
                eprintln!("{table}: read-only: {why}");
                continue;
            }
        };
        let snap = rs.snapshot();
        let col = rs.columns().unwrap().iter().position(|c| c.name == edit_col).unwrap();
        let row = (0..rs.rows()).find(|&r| snap.text(r, col).as_deref() == Some(old)).unwrap();
        let mut cs = ChangeSet::new();
        cs.edit(row, col, CellValue::text(old), CellValue::text(new));
        let plan = cs.plan(&t, &snap).unwrap();
        eprintln!("{table}: {} (key {:?})", plan[0].sql, t.key_source);
        let r = e.core.block_on(c.apply_changes(Some(db), plan)).unwrap();
        assert!(r.committed, "{table}: {r:?}");
        let after = run(&e, &s, &format!("SELECT * FROM {table}"));
        let a = after.snapshot();
        assert!((0..after.rows()).any(|r| a.text(r, col).as_deref() == Some(new)), "{table}: edit not applied");
    }
    // Inserts and deletes.
    let rs = run(&e, &s, "SELECT * FROM auto");
    let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    let snap = rs.snapshot();
    let mut cs = ChangeSet::new();
    let i = cs.insert_row(snap.columns.len());
    cs.set_inserted(i, 1, Some(CellValue::text("o'k")));
    let i2 = cs.insert_row(snap.columns.len());
    let _ = i2;
    let plan = cs.plan(&t, &snap).unwrap();
    for p in &plan {
        eprintln!("INS: {}", p.sql);
    }
    let r = e.core.block_on(c.apply_changes(Some(db), plan)).unwrap();
    assert!(r.committed, "{r:?}");
    assert_eq!(run(&e, &s, "SELECT COUNT(*) FROM auto").snapshot().text(0, 0).as_deref(), Some("2"));
    let rs = run(&e, &s, "SELECT * FROM comp");
    let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    let snap = rs.snapshot();
    let mut cs = ChangeSet::new();
    cs.delete_row(1);
    let plan = cs.plan(&t, &snap).unwrap();
    eprintln!("DEL: {}", plan[0].sql);
    let r = e.core.block_on(c.apply_changes(Some(db), plan)).unwrap();
    assert!(r.committed && r.outcomes.iter().all(|o| matches!(&o.outcome, Outcome::Applied { .. })), "{r:?}");
    assert_eq!(run(&e, &s, "SELECT COUNT(*) FROM comp").snapshot().text(0, 0).as_deref(), Some("2"));
    exec_quiet(&e, &c0, &["DROP DATABASE dataman_sw7"]);
}
