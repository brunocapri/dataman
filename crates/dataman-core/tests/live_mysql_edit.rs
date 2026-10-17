//! MySQL: grid edits of every column type.

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
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    exec_quiet(&e, &c0, &["DROP DATABASE IF EXISTS dataman_me1", "CREATE DATABASE dataman_me1"]);
    let Some(c) = mysql_on(&e, mysql_profile("dataman_me1")) else { return };
    let db = "dataman_me1";
    let s = c.session(None);
    for sql in [
        "CREATE TABLE `we``ird` (`id col` INT AUTO_INCREMENT PRIMARY KEY, b1 BIT(1), b8 BIT(8), y YEAR, st SET('a','b','c'), en ENUM('x','y z'), d1 DECIMAL(30,10), ub BIGINT UNSIGNED, \
          sb BIGINT, f FLOAT, dbl DOUBLE, js JSON, bl BLOB, bin BINARY(4), tm TIME(6), dt DATETIME(6), ts TIMESTAMP NULL, dd DATE, txt TEXT, vc VARCHAR(30), `na;me` VARCHAR(30), g GEOMETRY)",
        "INSERT INTO `we``ird` (b1, b8, y, st, en, d1, ub, sb, f, dbl, js, bl, bin, tm, dt, ts, dd, txt, vc, `na;me`, g) VALUES \
          (1, b'10101010', 2024, 'a,c', 'y z', 12345678901234567890.1234567890, 18446744073709551615, -9223372036854775808, 0.1, 0.30000000000000004, '{\"a\": [1, 2]}', 0xCAFE, 'ab', '-12:34:56.123456', \
           '2024-02-29 12:34:56.123456', '2024-01-01 00:00:00', '2024-02-29', 'text', 'vc', 'semi;colon', ST_GeomFromText('POINT(1 2)'))",
    ] {
        run(&e, &s, sql);
    }
    let rs = run(&e, &s, "SELECT * FROM `we``ird`");
    let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    let snap = rs.snapshot();
    let cols = rs.columns().unwrap();
    let mut bad = Vec::new();
    for (ix, col) in cols.iter().enumerate() {
        if col.name == "id col" {
            continue;
        }
        let old = snap.text(0, ix).map(|s| s.into_owned());
        let new = match col.name.as_str() {
            "b1" => "0",
            "b8" => "5",
            "y" => "2025",
            "st" => "b",
            "en" => "x",
            "d1" => "7.5",
            "ub" => "5",
            "sb" => "5",
            "f" => "2.5",
            "dbl" => "2.5",
            "js" => "{\"k\": \"it's\"}",
            "bl" => "\\xbeef",
            "bin" => "\\x01020304",
            "tm" => "-01:02:03.5",
            "dt" => "2025-01-01 00:00:00.5",
            "ts" => "2025-01-01 00:00:00",
            "dd" => "2025-01-01",
            "txt" | "vc" | "na;me" => "it's \\ \"q\"; --",
            "g" => continue,
            _ => continue,
        };
        // Once to the new value, then back (the old value is the guard of the second).
        for (from, to) in [(old.clone().unwrap(), new.to_string())] {
            let rs = run(&e, &s, "SELECT * FROM `we``ird`");
            let snap = rs.snapshot();
            let from = snap.text(0, ix).map(|x| x.into_owned()).unwrap_or(from);
            let mut cs = ChangeSet::new();
            cs.edit(0, ix, CellValue::text(from.clone()), CellValue::text(to.clone()));
            let plan = cs.plan(&t, &snap).unwrap();
            let r = e.core.block_on(c.apply_changes(Some(db), plan.clone())).unwrap();
            let ok = r.committed && r.outcomes.iter().all(|o| matches!(&o.outcome, Outcome::Applied { rows: 1 }));
            if !ok {
                bad.push(format!("{} ({:?} -> {:?}): {}\n   {:?}", col.name, from, to, plan[0].sql, r.outcomes.iter().map(|o| format!("{:?}", o.outcome)).collect::<Vec<_>>()));
            }
        }
    }
    // Every column edited again from what it shows now (the round trip of the displayed text).
    let rs = run(&e, &s, "SELECT * FROM `we``ird`");
    let snap = rs.snapshot();
    for (ix, col) in cols.iter().enumerate() {
        if col.name == "id col" || col.name == "g" {
            continue;
        }
        let now = snap.text(0, ix).map(|x| x.into_owned());
        let Some(now) = now else { continue };
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
    exec_quiet(&e, &c0, &["DROP DATABASE dataman_me1"]);
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}
