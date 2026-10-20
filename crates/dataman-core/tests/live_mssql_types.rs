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
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, "dataman_msw5");
    exec(&e, &c0, &["CREATE DATABASE dataman_msw5"]);
    let Some(c) = mssql_on(&e, mssql_profile("dataman_msw5")) else { return };
    let sess = c.session(None);
    for sql in [
        "CREATE TABLE ty (id INT PRIMARY KEY, bt BIT, ti TINYINT, si SMALLINT, bi BIGINT, r REAL, f FLOAT, d1 DECIMAL(38,10), d2 DECIMAL(38,0), d3 NUMERIC(18,6), mo MONEY, sm SMALLMONEY, \
          dt DATETIME, dt2 DATETIME2(7), sdt SMALLDATETIME, d DATE, t TIME(7), dto DATETIMEOFFSET(7), \
          ch CHAR(5), vc VARCHAR(20), nc NCHAR(5), nv NVARCHAR(20), nvm NVARCHAR(MAX), txt TEXT, ntxt NTEXT, \
          bin BINARY(4), vb VARBINARY(10), vbm VARBINARY(MAX), img IMAGE, uid UNIQUEIDENTIFIER, x XML, sv SQL_VARIANT, hid HIERARCHYID, geo GEOGRAPHY, gm GEOMETRY, rv ROWVERSION, js NVARCHAR(MAX), cmp AS (si * 2), vcm VARCHAR(MAX))",
        "INSERT INTO ty (id, bt, ti, si, bi, r, f, d1, d2, d3, mo, sm, dt, dt2, sdt, d, t, dto, ch, vc, nc, nv, nvm, txt, ntxt, bin, vb, vbm, img, uid, x, sv, hid, geo, gm, js, vcm) VALUES \
          (1, 1, 255, -32768, -9223372036854775808, 3.4E38, -1.7976931348623157E308, 1234567890123456789012345678.1234567890, 99999999999999999999999999999999999999, -123456.123456, -922337203685477.5808, 214748.3647, \
           '1753-01-01 00:00:00.003', '9999-12-31 23:59:59.9999999', '2079-06-06 23:59', '0001-01-01', '23:59:59.9999999', '2024-02-29 12:34:56.1234567 -05:30', \
           'abc', 'vc', N'日本', N'héllo 😀', REPLICATE(CAST(N'é' AS NVARCHAR(MAX)), 10000), 'text', N'ntext', 0xDEADBEEF, 0x00, 0xCAFEBABE, 0x0102, 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', '<r a=\"1\"><c>x</c></r>', CAST(CAST(42 AS INT) AS SQL_VARIANT), '/1/2/', \
           geography::Point(47.65, -122.34, 4326), geometry::STGeomFromText('LINESTRING(0 0, 1 1)', 0), N'{\"a\":1}', REPLICATE(CAST('x' AS VARCHAR(MAX)), 100000)), \
          (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
          (3, 0, 0, 32767, 9223372036854775807, 1.17E-38, 2.2250738585072014E-308, -0.0000000001, -99999999999999999999999999999999999999, 0.000001, 0, -214748.3648, \
           '9999-12-31 23:59:59.997', '0001-01-01 00:00:00', '1900-01-01', '9999-12-31', '00:00:00', '0001-01-02 00:00:00 +14:00', '', '', N'', N'', N'', '', N'', 0x, 0x, 0x, 0x, '00000000-0000-0000-0000-000000000000', '<r/>', CAST('str' AS SQL_VARIANT), '/', NULL, NULL, N'', '')",
        "INSERT INTO ty (id, sv) VALUES (4, CAST(CAST(1.5 AS DECIMAL(5,2)) AS SQL_VARIANT)), (5, CAST(CAST('2020-01-01' AS DATE) AS SQL_VARIANT)), (6, CAST(CAST(0xAB AS VARBINARY(1)) AS SQL_VARIANT)), (7, CAST(CAST(N'uni' AS NVARCHAR(10)) AS SQL_VARIANT)), (8, CAST(NEWID() AS SQL_VARIANT))",
    ] {
        run(&e, &sess, sql);
    }
    let rs = run(&e, &sess, "SELECT * FROM ty ORDER BY id");
    let snap = rs.snapshot();
    let cols = rs.columns().unwrap();
    for (ix, col) in cols.iter().enumerate() {
        let t = |r: usize| snap.text(r, ix).map(|s| { let s = s.into_owned(); if s.len() > 60 { format!("{}...({})", &s[..s.char_indices().nth(40).map_or(s.len(), |x| x.0)], s.len()) } else { s } });
        eprintln!("{:6} {:?} | {:?} | {:?} | {:?}", col.name, col.kind, t(0), t(1), t(2));
    }
    let sv = cols.iter().position(|c| c.name == "sv").unwrap();
    for r in 3..8 {
        eprintln!("sv row {}: {:?}", r + 1, snap.text(r, sv));
    }
    mssql_drop(&e, "dataman_msw5");
}
