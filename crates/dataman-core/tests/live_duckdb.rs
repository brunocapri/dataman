//! DuckDB on a temporary file: no server, so these always run.

mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::config::{ConnectionProfile, Engine};
use dataman_core::edit::{CellValue, ChangeSet, Editability, KeySource, Outcome};
use dataman_core::schema::{ConstraintKind, RelKind};
use dataman_core::sql::Dialect;
use dataman_core::value::Kind;
use dataman_core::{ResultSet, Session, TxStatus};

const SEED: &str = include_str!("../../../seeds/duckdb.sql");

struct Db {
    e: Env,
    c: Arc<Connection>,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn profile(path: &std::path::Path) -> ConnectionProfile {
    ConnectionProfile::new_file(Engine::DuckDb, path.to_string_lossy())
}

fn open(seed: bool) -> Db {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shop.duckdb");
    dataman_core::import::create_database(Engine::DuckDb, &path).unwrap();
    let e = env();
    let c = e.core.block_on(e.core.connect(profile(&path), None)).unwrap();
    let db = Db { e, c, path, _dir: dir };
    if seed {
        let s = db.c.session(None);
        ok(&db.e, &s, SEED);
    }
    db
}

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
    rs
}

fn ok(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

fn texts(rs: &ResultSet, row: usize) -> Vec<Option<String>> {
    let snap = rs.snapshot();
    (0..snap.columns.len()).map(|c| snap.text(row, c).map(|s| s.into_owned())).collect()
}

fn col(rs: &ResultSet, c: usize) -> Vec<Option<String>> {
    let snap = rs.snapshot();
    (0..snap.rows()).map(|r| snap.text(r, c).map(|s| s.into_owned())).collect()
}

fn s(v: &str) -> Option<String> {
    Some(v.into())
}

#[test]
fn connects_and_decodes_every_type() {
    let d = open(true);
    let (e, c) = (&d.e, &d.c);
    assert_eq!((c.engine(), c.dialect()), (Engine::DuckDb, Dialect::DuckDb));
    assert!(c.info.version.starts_with("DuckDB 1."), "{}", c.info.version);
    assert_eq!(c.info.database, "shop");
    assert_eq!(c.schema.search_path("shop"), ["main"]);
    assert!(c.caps().schemas && !c.caps().drop_database && c.caps().truncate);
    let sess = c.session(None);
    let rs = ok(
        e,
        &sess,
        "SELECT 1::TINYINT a, 2::SMALLINT b, 3 c, 4::BIGINT d, 170141183460469231731687303715884105727::HUGEINT h, 18446744073709551615::UBIGINT u, \
         7::UTINYINT ut, 1.5::FLOAT f4, 2.25::DOUBLE f8, 12.30::DECIMAL(10,2) dm, true bo, 'x''y' t, '\\xCA\\xFE'::BLOB bl, \
         DATE '2024-02-29' dt, TIME '13:14:15.5' tm, TIMESTAMP '2024-01-02 03:04:05.123456' ts, TIMESTAMP_NS '2024-01-02 03:04:05.123456789' tns, \
         TIMESTAMPTZ '2024-01-02 03:04:05+00' tz, INTERVAL 1 DAY + INTERVAL 2 HOUR iv, '6ba7b810-9dad-11d1-80b4-00c04fd430c8'::UUID id, \
         [1, 2, NULL] l, {'a': 1, 'b': [true]} st, MAP {'k': 1} m, 'gold'::tier en, NULL::INTEGER nul",
    );
    let kinds: Vec<Kind> = rs.columns().unwrap().iter().map(|c| c.kind).collect();
    use Kind::*;
    assert_eq!(
        kinds,
        [Int, Int, Int, Int, Numeric, Text, Int, Float4, Float, Numeric, Bool, Text, Bytes, Date, Time, Timestamp, Timestamp, TimestampTz, Other, Text, Json, Json, Json, Text, Int]
    );
    let r = texts(&rs, 0);
    assert_eq!(r[..4], [s("1"), s("2"), s("3"), s("4")]);
    assert_eq!(r[4], s("170141183460469231731687303715884105727"));
    assert_eq!(r[5], s("18446744073709551615"), "past i64: as it is, as text");
    assert_eq!(rs.columns().unwrap()[5].kind, Text);
    assert_eq!(r[6..13], [s("7"), s("1.5"), s("2.25"), s("12.30"), s("true"), s("x'y"), s("\\xcafe")]);
    assert_eq!(r[13..16], [s("2024-02-29"), s("13:14:15.5"), s("2024-01-02 03:04:05.123456")]);
    assert_eq!(r[16], s("2024-01-02 03:04:05.123456"), "nanoseconds to the grid's µs");
    assert!(r[17].as_deref().is_some_and(|t| t.starts_with("2024-01-02")), "{:?}", r[17]);
    assert_eq!(r[18], s("1 day 02:00:00"));
    assert_eq!(r[19], s("6ba7b810-9dad-11d1-80b4-00c04fd430c8"));
    assert_eq!(r[20], s("[1,2,null]"));
    assert_eq!(r[21], s("{\"a\":1,\"b\":[true]}"));
    assert_eq!(r[22], s("{\"k\":1}"));
    assert_eq!((r[23].as_deref(), r[24].as_deref()), (Some("gold"), None));
    let names: Vec<String> = rs.columns().unwrap().iter().map(|c| c.type_name.clone()).collect();
    assert_eq!((names[4].as_str(), names[16].as_str(), names[21].as_str()), ("HUGEINT", "TIMESTAMP_NS", "STRUCT"));
    // Parameters, numbered or not.
    let rs = sess.execute_with("SELECT name FROM customer WHERE id = $1 OR email = $2 ORDER BY id LIMIT $3", vec![s("2"), s("ana@example.com"), s("5")]);
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(col(&rs, 0), [s("Ana Lima"), s("Bruno Costa")], "{:?}", rs.status().error);
    let rs = ok(e, &sess, "INSERT INTO note VALUES ('a'), ('b')");
    assert_eq!(rs.status().affected, Some(2), "a DML's count, not a result");
    assert!(rs.columns().is_none_or(|c| c.is_empty()));
}

#[test]
fn one_instance_per_file() {
    let d = open(true);
    let e = env();
    // Another connection to the same file sees the same database (one instance, cloned).
    let other = e.core.block_on(e.core.connect(profile(&d.path), None)).unwrap();
    ok(&d.e, &d.c.session(None), "INSERT INTO note VALUES ('shared')");
    assert_eq!(col(&ok(&e, &other.session(None), "SELECT count(*) FROM note"), 0), [s("3")]);
    let mut ro = profile(&d.path);
    ro.read_only = true;
    let err = e.core.block_on(e.core.connect(ro.clone(), None)).err().map(|e| format!("{e:#}")).unwrap_or_default();
    assert!(err.contains("already open read-write"), "{err}");
    let mut off = profile(&d.path);
    off.options.insert(dataman_core::DUCKDB_EXTERNAL_ACCESS.into(), "false".into());
    assert!(e.core.block_on(e.core.connect(off, None)).is_err(), "another file-access mode");
    drop(other);
    d.c.close();
    let path = d.path.clone();
    drop(d.c);
    drop(d.e);
    let e2 = env();
    // The closed connection's threads let go of the instance a moment later.
    let mut c = e2.core.block_on(e2.core.connect(ro.clone(), None));
    for _ in 0..40 {
        if c.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        c = e2.core.block_on(e2.core.connect(ro.clone(), None));
    }
    let c = c.unwrap();
    let rs = run(&e2, &c.session(None), "DELETE FROM note");
    assert_eq!(rs.phase(), Phase::Failed, "a read-only instance");
    assert_eq!(col(&ok(&e2, &c.session(None), "SELECT count(*) FROM note"), 0), [s("3")]);
    let _ = path;
}

#[test]
fn transactions_are_tracked() {
    let d = open(true);
    let (e, c) = (&d.e, &d.c);
    let sess = c.session(None);
    assert_eq!(sess.tx().status, TxStatus::Idle);
    ok(e, &sess, "BEGIN TRANSACTION");
    assert_eq!(sess.tx().status, TxStatus::Open);
    ok(e, &sess, "DELETE FROM note");
    assert_eq!(run(e, &sess, "SELECT * FROM nope").phase(), Phase::Failed);
    assert_eq!(sess.tx().status, TxStatus::Failed, "an error aborts DuckDB's transaction");
    ok(e, &sess, "ROLLBACK");
    assert_eq!(sess.tx().status, TxStatus::Idle);
    assert_eq!(col(&ok(e, &sess, "SELECT count(*) FROM note"), 0), [s("2")]);
    ok(e, &sess, "BEGIN; INSERT INTO note VALUES ('x');");
    assert_eq!(sess.tx().status, TxStatus::Open);
    assert_eq!(e.core.block_on(sess.end_transaction(true)).unwrap(), dataman_core::TxEnd::Committed);
    assert_eq!(col(&ok(e, &sess, "SELECT count(*) FROM note"), 0), [s("3")]);
}

#[test]
fn a_long_query_is_interrupted() {
    let d = open(false);
    let (e, c) = (&d.e, &d.c);
    let sess = c.session(None);
    let rs = sess.execute("SELECT count(*) FROM range(1000000000000) a");
    std::thread::sleep(Duration::from_millis(300));
    sess.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("the interrupt ends it") });
    assert_eq!(rs.phase(), Phase::Cancelled, "{:?}", rs.status().error);
    assert_eq!(col(&ok(e, &sess, "SELECT 42"), 0), [s("42")]);
}

#[test]
fn catalog_and_ddl() {
    let d = open(true);
    let (e, c) = (&d.e, &d.c);
    let db = "shop";
    assert_eq!(e.core.block_on(c.databases()).unwrap(), ["shop"]);
    assert_eq!(e.core.block_on(c.schemas(db)).unwrap(), ["main", "sales"]);
    let rels = e.core.block_on(c.relations(db, "main")).unwrap();
    let names: Vec<(&str, RelKind)> = rels.iter().map(|r| (r.name.as_str(), r.kind)).collect();
    assert_eq!(names, [("customer", RelKind::Table), ("note", RelKind::Table), ("orders", RelKind::Table), ("v_order_customer", RelKind::View)]);
    assert_eq!(e.core.block_on(c.relations(db, "sales")).unwrap()[0].name, "region");
    let cols = e.core.block_on(c.columns(db, "main", "customer")).unwrap();
    assert!(cols[0].primary_key && cols[0].type_name == "INTEGER" && cols[0].default.as_deref().is_some_and(|d| d.contains("nextval")), "{:?}", cols[0]);
    let name = cols.iter().find(|c| c.name == "name").unwrap();
    assert_eq!(name.nullable, Some(false));
    let idx = e.core.block_on(c.indexes(db, "main", "orders")).unwrap();
    assert!(idx.iter().any(|i| i.primary && i.columns == ["id"]), "{idx:?}");
    let st = idx.iter().find(|i| i.name == "orders_status_idx").unwrap();
    assert_eq!(st.columns, ["status", "customer_id"]);
    let cons = e.core.block_on(c.table_extras(db, "main", "orders")).unwrap().constraints.unwrap();
    assert!(cons.iter().any(|k| k.kind == ConstraintKind::Foreign), "{cons:?}");
    assert!(cons.iter().any(|k| k.kind == ConstraintKind::Primary), "{cons:?}");
    let counts = e.core.block_on(c.object_counts(db, "main")).unwrap();
    assert_eq!((counts.functions, counts.triggers), (None, None));
    assert!(e.core.block_on(c.rel_stats(db, "main")).unwrap().is_empty());
    let ddl = e.core.block_on(c.table_ddl(db, "main", "orders")).unwrap();
    assert!(ddl.starts_with("-- main.orders: as stored in shop\nCREATE TABLE orders("), "{ddl}");
    assert!(ddl.contains("CREATE INDEX orders_status_idx"), "{ddl}");
    let v = e.core.block_on(c.view_definition(db, "main", "v_order_customer")).unwrap().unwrap();
    assert!(v.contains("CREATE VIEW v_order_customer AS"), "{v}");
    let g = e.core.block_on(c.relationships(db, "main")).unwrap();
    let l = g.links.iter().find(|l| l.from_table == "orders").unwrap();
    assert_eq!((l.from_columns.clone(), l.to_table.as_str(), l.to_columns.clone()), (vec!["customer_id".to_string()], "customer", vec!["id".to_string()]));
    assert_eq!(e.core.block_on(c.primary_key(db, "main", "customer")).unwrap(), ["id"]);
    assert_eq!(e.core.block_on(c.estimate_rows(db, "main", "customer")).unwrap(), None);
    e.core.block_on(c.preload(db)).unwrap();
    let snap = c.schema.snapshot(db);
    assert_eq!(snap.dialect, Dialect::DuckDb);
    assert!(snap.resolve(Some("sales"), "region").is_some());
    let cat = e.core.block_on(c.catalog(db)).unwrap();
    assert!(cat.functions_named(None, "read_csv").count() >= 1);
    // An ATTACHed catalog is the instance's: it shows after a reload.
    let other = d.path.with_file_name("more.duckdb");
    ok(e, &c.session(None), &format!("ATTACH '{}' AS more; CREATE TABLE more.main.x (a INT)", other.display()));
    c.schema.invalidate(None);
    assert_eq!(e.core.block_on(c.databases()).unwrap(), ["shop", "more"]);
    assert_eq!(e.core.block_on(c.relations("more", "main")).unwrap()[0].name, "x");
}

#[test]
fn edits_by_key_and_explain() {
    let d = open(true);
    let (e, c) = (&d.e, &d.c);
    let sess = c.session(None);
    let db = "shop";
    let edit = |sql: &str, row: usize, column: &str, to: &str| -> (KeySource, Vec<Outcome>) {
        let rs = ok(e, &sess, sql);
        let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
            Editability::Editable(t) => t,
            Editability::ReadOnly(why) => panic!("{sql}: {why}"),
        };
        let snap = rs.snapshot();
        let ix = rs.columns().unwrap().iter().position(|c| c.name == column).unwrap();
        let mut cs = ChangeSet::new();
        cs.edit(row, ix, CellValue::text(snap.text(row, ix).map(|x| x.into_owned()).unwrap_or_default()), CellValue::text(to));
        let plan = cs.plan(&t, &snap).unwrap();
        let r = e.core.block_on(c.apply_changes(Some(db), plan)).unwrap();
        assert!(r.committed, "{sql}: {:?}", r.error);
        (t.key_source, r.outcomes.into_iter().map(|o| o.outcome).collect())
    };
    let (src, out) = edit("SELECT * FROM customer ORDER BY id", 1, "name", "Bruno C.");
    assert_eq!((src, out), (KeySource::PrimaryKey, vec![Outcome::Applied { rows: 1 }]));
    edit("SELECT id, balance FROM customer ORDER BY id", 0, "balance", "7.25");
    edit("SELECT id, photo FROM customer ORDER BY id", 0, "photo", "\\xbeef");
    edit("SELECT id, created_at FROM customer ORDER BY id", 2, "created_at", "2025-05-06 07:08:09");
    let r = texts(&ok(e, &sess, "SELECT name, balance, hex(photo) FROM customer WHERE id IN (1, 2) ORDER BY id"), 0);
    assert_eq!(r, [s("Ana Lima"), s("7.25"), s("BEEF")]);
    assert_eq!(col(&ok(e, &sess, "SELECT created_at FROM customer WHERE id = 3"), 0), [s("2025-05-06 07:08:09")]);
    edit("SELECT * FROM sales.region", 0, "name", "Europa");
    let rs = ok(e, &sess, "SELECT * FROM note");
    assert!(matches!(e.core.block_on(c.edit_target(db, &rs, None)).unwrap(), Editability::ReadOnly(_)));

    let t = dataman_core::explain::Target::of(&c.info);
    let sql = dataman_core::explain::sql("SELECT * FROM orders WHERE status = 'paid'", false, t);
    assert_eq!(sql, "EXPLAIN SELECT * FROM orders WHERE status = 'paid'");
    let rs = ok(e, &sess, &sql);
    let names: Vec<String> = rs.columns().unwrap().iter().map(|c| c.name.clone()).collect();
    let rows: Vec<Vec<Option<String>>> = (0..rs.rows()).map(|r| texts(&rs, r)).collect();
    let text = dataman_core::explain::Plan::source_of(&names, &rows);
    assert!(text.contains("SCAN") || text.contains("Scan"), "{text}");
}

#[test]
fn agents_stay_in_bounds() {
    let d = open(true);
    let (e, c) = (&d.e, &d.c);
    let never = std::future::pending::<()>;
    let o = e.core.block_on(c.bounded(None, "SELECT id, name, tags, profile FROM customer ORDER BY id", vec![], Mode::Read, Limits::default(), never())).unwrap();
    assert_eq!(o.parts.len(), 1);
    for sql in ["DELETE FROM customer", "SELECT * FROM read_csv('/etc/hosts')", "SELECT * FROM '/etc/hosts'", "COPY customer TO '/tmp/x.csv'", "ATTACH '/tmp/y.duckdb'", "INSTALL httpfs", "SET enable_external_access = true"] {
        let r = e.core.block_on(c.bounded(None, sql, vec![], Mode::Read, Limits::default(), never()));
        assert!(matches!(r, Err(Stop::Refused(_))), "{sql}: {r:?}");
    }
    let r = e.core.block_on(c.bounded(None, "COPY customer TO '/tmp/dataman-x.csv'", vec![], Mode::Write, Limits::default(), never()));
    assert!(matches!(r, Err(Stop::Refused(_))), "files at any level: {r:?}");
    let o = e.core.block_on(c.bounded(None, "UPDATE orders SET notes = $1 WHERE id = $2", vec![s("x"), s("2")], Mode::Write, Limits::default(), never())).unwrap();
    assert!(o.committed);
    assert_eq!(col(&ok(e, &c.session(None), "SELECT notes FROM orders WHERE id = 2"), 0), [s("x")]);
    let lim = Limits { timeout: Duration::from_millis(300), ..Limits::default() };
    let r = e.core.block_on(c.bounded(None, "SELECT count(*) FROM range(1000000000000) a", vec![], Mode::Read, lim, never()));
    assert!(matches!(r, Err(Stop::Timeout(_))), "{r:?}");
}

#[test]
fn an_empty_file_becomes_a_new_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("touched.duckdb");
    std::fs::File::create(&path).unwrap();
    assert_eq!(dataman_core::import::detect_database(&path), Some(Engine::DuckDb), "by its extension");
    let e = env();
    let mut ro = profile(&path);
    ro.read_only = true;
    let err = e.core.block_on(e.core.connect(ro, None)).err().expect("read-only writes nothing");
    assert!(format!("{err:#}").contains("is empty"), "{err:#}");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "left as it was");
    let c = e.core.block_on(e.core.connect(profile(&path), None)).unwrap();
    ok(&e, &c.session(None), "CREATE TABLE t (a INT); INSERT INTO t VALUES (1)");
    assert_eq!(dataman_core::import::detect_database(&path), Some(Engine::DuckDb), "by its header now");
    assert_eq!(col(&ok(&e, &c.session(None), "SELECT a FROM t"), 0), [s("1")]);
}

#[test]
fn another_file_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shop.duckdb");
    std::fs::write(&path, b"SQLite format 3\0and then some").unwrap();
    let e = env();
    let err = e.core.block_on(e.core.connect(profile(&path), None)).err().expect("not DuckDB");
    assert!(format!("{err:#}").contains("is not a DuckDB database file"), "{err:#}");
}

#[test]
fn file_access_can_be_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("closed.duckdb");
    dataman_core::import::create_database(Engine::DuckDb, &path).unwrap();
    let e = env();
    let mut p = profile(&path);
    p.options.insert(dataman_core::DUCKDB_EXTERNAL_ACCESS.into(), "false".into());
    let c = e.core.block_on(e.core.connect(p, None)).unwrap();
    let sess = c.session(None);
    let rs = run(&e, &sess, "SELECT count(*) FROM read_text('/etc/hosts')");
    assert_eq!(rs.phase(), Phase::Failed);
    assert!(rs.status().error.unwrap().message.contains("disabled by configuration"));
    assert_eq!(run(&e, &sess, "SET enable_external_access = true").phase(), Phase::Failed, "locked");
    ok(&e, &sess, "CREATE TABLE t (a INT); INSERT INTO t VALUES (1)");
}

