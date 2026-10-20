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
fn every_type() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("")) else { return };
    exec_quiet(&e, &c0, &["DROP DATABASE IF EXISTS dataman_sw5", "CREATE DATABASE dataman_sw5"]);
    let Some(c) = mysql_on(&e, mysql_profile("dataman_sw5")) else { return };
    let sess = c.session(None);
    for sql in [
        "SET SESSION sql_mode = ''",
        "CREATE TABLE ty (id INT PRIMARY KEY, b1 BIT(1), b8 BIT(8), b64 BIT(64), y YEAR, y2 YEAR, st SET('a','b','c'), en ENUM('x','y z'), \
          d1 DECIMAL(65,30), d2 DECIMAL(10,0), d3 DECIMAL(5,2), ub BIGINT UNSIGNED, sb BIGINT, ti TINYINT, uti TINYINT UNSIGNED, si SMALLINT, mi MEDIUMINT, umi MEDIUMINT UNSIGNED, ui INT UNSIGNED, \
          js JSON, bl BLOB, lbl LONGBLOB, bin BINARY(4), vbin VARBINARY(8), tm TIME, tm6 TIME(6), dt DATETIME, dt6 DATETIME(6), ts TIMESTAMP NULL, ts3 TIMESTAMP(3) NULL, dd DATE, \
          f FLOAT, dbl DOUBLE, txt TEXT, vc VARCHAR(20), ch CHAR(3), geo GEOMETRY, pt POINT, ls LINESTRING)",
        "INSERT INTO ty (id, b1, b8, b64, y, y2, st, en, d1, d2, d3, ub, sb, ti, uti, si, mi, umi, ui, js, bl, lbl, bin, vbin, tm, tm6, dt, dt6, ts, ts3, dd, f, dbl, txt, vc, ch, geo, pt, ls) VALUES \
          (1, b'1', b'10101010', b'1111111111111111111111111111111111111111111111111111111111111111', 2024, 99, 'a,c', 'y z', \
           12345678901234567890123456789012345.123456789012345678901234567890, 9999999999, -123.45, 18446744073709551615, -9223372036854775808, -128, 255, -32768, -8388608, 16777215, 4294967295, \
           '{\"a\": [1, 2, {\"b\": null}], \"s\": \"x;y\"}', 'abc', 0xDEADBEEF, 'ab', 'xyz', '-838:59:59', '-00:00:00.000001', '9999-12-31 23:59:59', '2024-02-29 12:34:56.789012', '2038-01-19 03:14:07', '2024-01-01 00:00:00.123', '1000-01-01', \
           3.5, -1.7976931348623157e308, 'héllo wörld 日本語 😀', 'vc', 'ch', ST_GeomFromText('POINT(1 2)'), ST_GeomFromText('POINT(3 4)'), ST_GeomFromText('LINESTRING(0 0, 1 1, 2 2)')), \
          (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
        "INSERT INTO ty (id, tm, tm6, dt, dt6, ts, dd, y) VALUES (3, '838:59:59', '123:45:59.891', '0000-00-00 00:00:00', '0000-00-00 00:00:00.000000', '0000-00-00 00:00:00', '0000-00-00', 0)",
        "INSERT INTO ty (id, dt, dd) VALUES (4, '2024-00-00 00:00:00', '2024-00-10')",
    ] {
        run(&e, &sess, sql);
    }
    let rs = run(&e, &sess, "SELECT * FROM ty ORDER BY id");
    let snap = rs.snapshot();
    let cols = rs.columns().unwrap();
    for (ix, col) in cols.iter().enumerate() {
        let t = |r: usize| snap.text(r, ix).map(|s| s.into_owned());
        eprintln!("{:6} {:?} | {:?} | {:?} | {:?} | {:?}", col.name, col.kind, t(0), t(1), t(2), t(3));
    }
    run(&e, &sess, "CREATE PROCEDURE sel() SELECT * FROM ty ORDER BY id");
    let via_call = run(&e, &sess, "CALL sel()");
    let a = via_call.snapshot();
    let mut bad = Vec::new();
    for (ix, col) in cols.iter().enumerate() {
        for r in 0..4 {
            let (x, y) = (snap.text(r, ix).map(|s| s.into_owned()), a.text(r, ix).map(|s| s.into_owned()));
            if x != y {
                bad.push(format!("{} row {}: binary {:?} vs text {:?}", col.name, r + 1, x, y));
            }
        }
    }
    let ck: Vec<_> = via_call.columns().unwrap().iter().zip(cols.iter()).filter(|(x, y)| x.kind != y.kind).map(|(x, y)| format!("{}: {:?} vs {:?}", x.name, x.kind, y.kind)).collect();
    exec_quiet(&e, &c0, &["DROP DATABASE dataman_sw5"]);
    assert!(bad.is_empty() && ck.is_empty(), "{bad:#?} {ck:#?}");
}
