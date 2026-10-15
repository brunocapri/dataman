//! Tests against real servers. They run when the servers answer and skip otherwise:
//!
//! - `DISKMAN_PG`  (default `127.0.0.1:55432`, user `postgres`, the password in
//!   `DISKMAN_PG_PASSWORD`, default `diskman`, db `diskman`, loaded with `seeds/postgres.sql`)
//! - `DISKMAN_LAR` (default `127.0.0.1:5490`, user `lar`, db `lar`, no password)

use std::sync::Arc;
use std::time::{Duration, Instant};

use diskman_core::config::{ConnectionProfile, Dirs, SslMode};
use diskman_core::result::{CopyFormat, PREFETCH, Phase, SortDir};
use diskman_core::{Connection, Core, MemorySecrets, Outcome};

struct Env {
    core: Core,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(Dirs::at(dir.path().join("diskman")), Arc::new(MemorySecrets::default())).unwrap();
    Env { core, _dir: dir }
}

fn addr(var: &str, default: &str) -> (String, u16) {
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (h, p) = v.rsplit_once(':').unwrap();
    (h.into(), p.parse().unwrap())
}

fn pg_profile() -> ConnectionProfile {
    let (host, port) = addr("DISKMAN_PG", "127.0.0.1:55432");
    ConnectionProfile {
        host,
        port,
        user: "postgres".into(),
        database: "diskman".into(),
        sslmode: SslMode::Disable,
        ..ConnectionProfile::new("pg")
    }
}

fn lar_profile() -> ConnectionProfile {
    let (host, port) = addr("DISKMAN_LAR", "127.0.0.1:5490");
    ConnectionProfile { host, port, user: "lar".into(), database: "lar".into(), sslmode: SslMode::Disable, ..ConnectionProfile::new("lar") }
}

fn connect(e: &Env, p: ConnectionProfile, pw: Option<&str>) -> Option<Arc<Connection>> {
    match e.core.block_on(e.core.connect(p.clone(), pw.map(str::to_string))) {
        Ok(c) => Some(c),
        Err(err) => {
            eprintln!("skipping: cannot reach {} ({err:#})", p.summary());
            None
        }
    }
}

fn pg(e: &Env) -> Option<Arc<Connection>> {
    let pw = std::env::var("DISKMAN_PG_PASSWORD").unwrap_or_else(|_| "diskman".into());
    connect(e, pg_profile(), Some(&pw))
}

fn lar(e: &Env) -> Option<Arc<Connection>> {
    connect(e, lar_profile(), None)
}

fn finish(e: &Env, rs: &diskman_core::ResultSet) -> Phase {
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") })
}

#[test]
fn detects_servers() {
    let e = env();
    if let Some(c) = pg(&e) {
        assert!(!c.info.is_lar, "{}", c.info.version);
        assert!(c.info.version.starts_with("PostgreSQL"));
        assert_eq!(c.info.database, "diskman");
        assert!(c.info.extensions.is_empty());
    }
    if let Some(c) = lar(&e) {
        assert!(c.info.is_lar, "{}", c.info.version);
        assert_eq!(c.info.extensions, vec!["lar"]);
    }
}

#[test]
fn binary_decoding_matches_server_text() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    // Every column formatted from binary must equal the server's own text output.
    let sql = "SELECT 1::int2, 2::int4, 3::int8, 12345.6789::numeric(12,4), -0.5::numeric, 'NaN'::numeric, \
        'x'::text, ''::text, NULL::text, true, false, 'ab '::char(3), 'n'::name, \
        '2024-01-02'::date, 'infinity'::date, '03:04:05.5'::time, '2024-01-02 03:04:05.123'::timestamp, \
        interval '1 year 2 mons 3 days 04:05:06', interval '-00:00:01.5', \
        'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, '{\"a\": [1, 2]}'::jsonb, '{\"b\":1}'::json, \
        ARRAY[1,2,NULL], ARRAY['a b','c','',NULL], ARRAY[[1,2],[3,4]], '{}'::int[], \
        '192.168.0.1/24'::inet, '10.0.0.1'::inet, '10.0.0.0/8'::cidr, '\\xdeadbeef'::bytea, \
        ROW(1,'x y',NULL), 'ok'::regclass::oid, 42::oid, '12:34:56+02'::timetz, '08:00:2b:01:02:03'::macaddr, \
        'a'::\"char\", '0/16B3748'::pg_lsn, 1e300::numeric, 0.000001::numeric, \
        0.1::float4, 1.5e-7::float4, 1.5e-7::float8, 1e300::float8, 1234567::float4, 12345.678::float8, ROW('a\\b', 'x\"y')";
    let sql = sql.replace("'ok'::regclass::oid", "'pg_class'::regclass::oid");
    let s = c.session(None);
    let bin = s.execute(sql.clone());
    assert_eq!(finish(&e, &bin), Phase::Done, "{:?}", bin.status().error);
    assert!(!bin.status().text_mode);
    // The same statement twice in one string forces the simple (text) protocol.
    let txt = s.execute(format!("{sql}; SELECT 1"));
    assert_eq!(finish(&e, &txt), Phase::Done, "{:?}", txt.status().error);
    assert!(txt.status().text_mode);
    let (b, t) = (bin.snapshot(), txt.snapshot());
    assert_eq!(b.rows(), 1);
    assert_eq!(t.rows(), 1);
    for col in 0..b.columns.len() {
        let mut bv = b.text(0, col).map(|s| s.into_owned());
        let tv = t.text(0, col).map(|s| s.into_owned());
        // Booleans print as true/false here, t/f in psql's text format.
        if let Some(v) = &bv
            && b.columns[col].type_name == "bool"
        {
            bv = Some(if v == "true" { "t".into() } else { "f".into() });
        }
        assert_eq!(bv, tv, "column {} ({})", col, b.columns[col].type_name);
    }
}

#[test]
fn batches_on_demand_and_fetch_all() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    let t0 = Instant::now();
    let rs = s.execute("SELECT g, md5(g::text) AS h, g % 7 = 0 AS seven FROM generate_series(1, 1000000) g");
    // It pauses after the prefetch window, with the stream open.
    e.core.block_on(async {
        let mut rx = rs.subscribe();
        while rs.phase() != Phase::Paused {
            rx.changed().await.unwrap();
        }
    });
    let first_pause = t0.elapsed();
    // Recorded in the history at the first pause, not only when (if ever) it ends.
    let h = e.core.history.load(&c.profile.id, 10).unwrap();
    assert_eq!(h.len(), 1);
    assert!(h[0].rows.unwrap() >= PREFETCH as u64);
    let loaded = rs.rows();
    assert!(loaded >= PREFETCH && loaded < PREFETCH + 2 * diskman_core::result::CHUNK, "{loaded}");
    assert!(rs.status().first_row.is_some());
    // Paused, the clock stops: the wait is not running time.
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(rs.status().paused_time() >= std::time::Duration::from_millis(400), "{:?}", rs.status().paused_time());
    rs.request_rows(500_000);
    e.core.block_on(async {
        let mut rx = rs.subscribe();
        while rs.rows() < 500_000 || rs.phase() != Phase::Paused {
            rx.changed().await.unwrap();
        }
    });
    rs.fetch_all();
    assert_eq!(finish(&e, &rs), Phase::Done);
    let all = t0.elapsed();
    let ran = rs.status().elapsed.unwrap();
    assert!(ran + std::time::Duration::from_millis(400) <= all, "ran {ran:?}, wall {all:?}");
    assert_eq!(rs.rows(), 1_000_000);
    let snap = rs.snapshot();
    assert_eq!(snap.text(999_999, 0).as_deref(), Some("1000000"));
    assert_eq!(snap.text(6, 2).as_deref(), Some("true"));
    eprintln!("1M rows x 3 cols: first pause after {first_pause:?}, all rows after {all:?}");

    let t = Instant::now();
    rs.sort(Some((1, SortDir::Desc)));
    eprintln!("sort 1M text rows: {:?}", t.elapsed());
    let snap = rs.snapshot();
    assert!(snap.text(0, 1) >= snap.text(1, 1));
    let tsv = rs.copy(0..2, &[0, 1], CopyFormat::Tsv, true);
    assert_eq!(tsv.lines().count(), 3);
}

#[test]
fn cancel_and_supersede() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    let rs = s.execute("SELECT pg_sleep(30)");
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    s.cancel();
    assert_eq!(finish(&e, &rs), Phase::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    // The session is still usable.
    let rs = s.execute("SELECT 42");
    assert_eq!(finish(&e, &rs), Phase::Done);
    assert_eq!(rs.snapshot().text(0, 0).as_deref(), Some("42"));

    // A paused big result is cancelled when the next statement runs in the same session.
    let big = s.execute("SELECT g FROM generate_series(1, 5000000) g");
    e.core.block_on(async {
        let mut rx = big.subscribe();
        while big.phase() != Phase::Paused {
            rx.changed().await.unwrap();
        }
    });
    let t = Instant::now();
    let next = s.execute("SELECT 'next'");
    assert_eq!(finish(&e, &next), Phase::Done);
    assert_eq!(finish(&e, &big), Phase::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(next.snapshot().text(0, 0).as_deref(), Some("next"));
}

#[test]
fn errors_commands_and_history() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    let rs = s.execute("SELECT * FROM no_such_table");
    assert_eq!(finish(&e, &rs), Phase::Failed);
    let err = rs.status().error.unwrap();
    assert_eq!(err.code.as_deref(), Some("42P01"));
    assert_eq!(err.position, Some(15));

    for (sql, affected) in [
        ("CREATE TEMP TABLE diskman_t (id int PRIMARY KEY, v text)", Some(0)),
        ("INSERT INTO diskman_t VALUES (1, 'a'), (2, NULL), (3, '')", Some(3)),
        ("UPDATE diskman_t SET v = 'b' WHERE id > 1", Some(2)),
    ] {
        let rs = s.execute(sql);
        assert_eq!(finish(&e, &rs), Phase::Done, "{sql}: {:?}", rs.status().error);
        assert_eq!(rs.status().affected, affected, "{sql}");
    }
    let rs = s.execute("SELECT v FROM diskman_t ORDER BY id");
    finish(&e, &rs);
    let snap = rs.snapshot();
    assert_eq!(snap.text(0, 0).as_deref(), Some("a"));

    let h = e.core.history.load(&c.profile.id, 100).unwrap();
    assert_eq!(h.len(), 5);
    assert_eq!(h[0].sql, "SELECT v FROM diskman_t ORDER BY id");
    assert_eq!(h[0].rows, Some(3));
    assert_eq!(h.last().unwrap().outcome, Outcome::Error);
    assert_eq!(h[1].rows, Some(2));
}

#[test]
fn introspection_postgres() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.contains(&"diskman".to_string()), "{dbs:?}");
    let schemas = e.core.block_on(c.schemas("diskman")).unwrap();
    assert_eq!(schemas[0], "public");
    assert!(schemas.contains(&"pg_catalog".to_string()));
    let rels = e.core.block_on(c.relations("diskman", "public")).unwrap();
    assert!(rels.iter().any(|r| r.name == "orders"), "{rels:?}");
    let cols = e.core.block_on(c.columns("diskman", "public", "orders")).unwrap();
    assert!(cols.iter().any(|c| c.name == "id"));
    eprintln!("orders: {cols:?}");
    let idx = e.core.block_on(c.indexes("diskman", "public", "orders")).unwrap();
    eprintln!("orders indexes: {idx:?}");
    e.core.block_on(c.preload("diskman")).unwrap();
    let snap = c.schema.snapshot("diskman");
    let i = snap.resolve(None, "customer").unwrap();
    assert!(!snap.columns_of(i).is_empty());
    // The explorer's columns after the preload still carry the key.
    let cols = e.core.block_on(c.columns("diskman", "public", "customer")).unwrap();
    assert!(cols.iter().any(|c| c.primary_key), "{cols:?}");
    assert!(c.schema.notes().is_empty(), "{:?}", c.schema.notes());
}

#[test]
fn introspection_lar_degrades() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.contains(&"lar".to_string()), "{dbs:?}");
    let schemas = e.core.block_on(c.schemas("lar")).unwrap();
    assert!(schemas.contains(&"public".to_string()), "{schemas:?}");
    let rels = e.core.block_on(c.relations("lar", "public")).unwrap();
    assert!(rels.iter().any(|r| r.name == "demo_orders"), "{rels:?}");
    let cols = e.core.block_on(c.columns("lar", "public", "demo_orders")).unwrap();
    assert!(cols.iter().any(|c| c.name == "id" && c.type_name == "bigint"), "{cols:?}");
    let idx = e.core.block_on(c.indexes("lar", "public", "demo_orders")).unwrap();
    eprintln!("LAR demo_orders indexes: {idx:?}; notes: {:?}", c.schema.notes());
    e.core.block_on(c.preload("lar")).unwrap();
    let snap = c.schema.snapshot("lar");
    let i = snap.resolve(None, "demo_orders").unwrap();
    assert!(!snap.columns_of(i).is_empty());

    // Queries run, and decode, on LAR too.
    let s = c.session(None);
    let rs = s.execute("SELECT id, status, total FROM demo_orders LIMIT 5");
    assert_eq!(finish(&e, &rs), Phase::Done, "{:?}", rs.status().error);
    assert_eq!(rs.rows(), 5);
    eprintln!("LAR text_mode={} columns={:?}", rs.status().text_mode, rs.columns().unwrap());
    let rs = s.execute("SELECT version()");
    assert_eq!(finish(&e, &rs), Phase::Done);
    assert_eq!(rs.snapshot().text(0, 0).as_deref(), Some("PostgreSQL 16.6 (LAR)"));
}

#[test]
fn lar_big_result_and_cancel() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    let s = c.session(None);
    let t0 = Instant::now();
    let rs = s.execute("SELECT * FROM demo_orders");
    e.core.block_on(async {
        let mut rx = rs.subscribe();
        while !matches!(rs.phase(), Phase::Paused) && !rs.phase().is_finished() {
            rx.changed().await.unwrap();
        }
    });
    eprintln!("LAR: first {} rows of demo_orders in {:?} ({:?})", rs.rows(), t0.elapsed(), rs.phase());
    let t = Instant::now();
    s.cancel();
    let p = finish(&e, &rs);
    eprintln!("LAR: cancel of a paused result → {p:?} in {:?}", t.elapsed());
    let rs = s.execute("SELECT 1");
    assert_eq!(finish(&e, &rs), Phase::Done, "{:?}", rs.status().error);
}

#[test]
fn superseded_statement_never_runs() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let setup = c.session(None);
    for sql in ["DROP TABLE IF EXISTS diskman_abort_t", "CREATE TABLE diskman_abort_t (n int)"] {
        let rs = setup.execute(sql);
        assert_eq!(finish(&e, &rs), Phase::Done, "{:?}", rs.status().error);
    }
    // The INSERT waits behind pg_sleep and is replaced before it reaches the server.
    let s = c.session(None);
    let sleep = s.execute("SELECT pg_sleep(0.5)");
    let insert = s.execute("INSERT INTO diskman_abort_t VALUES (1)");
    let last = s.execute("SELECT count(*) FROM diskman_abort_t");
    assert_eq!(finish(&e, &last), Phase::Done, "{:?}", last.status().error);
    assert_eq!(finish(&e, &insert), Phase::Cancelled);
    assert_eq!(finish(&e, &sleep), Phase::Cancelled);
    assert_eq!(last.snapshot().text(0, 0).as_deref(), Some("0"));
    let rs = setup.execute("DROP TABLE diskman_abort_t");
    finish(&e, &rs);
}

#[test]
fn simple_protocol_shows_only_the_first_result() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    let rs = s.execute("SELECT 1 AS a; SELECT 2 AS b");
    assert_eq!(finish(&e, &rs), Phase::Done);
    assert!(rs.status().text_mode);
    assert_eq!(rs.rows(), 1);
    assert_eq!(rs.columns().unwrap()[0].name, "a");
    // A runtime error after the columns are known is still reported.
    let rs = s.execute("SELECT 1/0 AS x");
    assert_eq!(finish(&e, &rs), Phase::Failed);
    assert_eq!(rs.status().error.unwrap().code.as_deref(), Some("22012"));
}
