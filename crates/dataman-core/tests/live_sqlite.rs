//! SQLite on a temporary file: no server, so these always run.

mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::config::{ConnectionProfile, Engine};
use dataman_core::dataview::{Filter, Op, TableView};
use dataman_core::edit::{CellValue, ChangeSet, Editability, KeySource, Outcome};
use dataman_core::schema::{ConstraintKind, RelKind};
use dataman_core::sql::Dialect;
use dataman_core::value::Kind;
use dataman_core::{ResultSet, Session, TxStatus};

const SEED: &str = include_str!("../../../seeds/sqlite.sql");

struct Db {
    e: Env,
    c: Arc<Connection>,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn open(seed: bool) -> Db {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shop.sqlite");
    std::fs::File::create(&path).unwrap();
    let e = env();
    let c = e.core.block_on(e.core.connect(ConnectionProfile::new_file(Engine::Sqlite, path.to_string_lossy()), None)).unwrap();
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

#[test]
fn connects_and_decodes_types() {
    let d = open(true); let (e, c) = (&d.e, d.c.clone());
    assert_eq!((c.engine(), c.dialect()), (Engine::Sqlite, Dialect::Sqlite));
    assert!(c.info.version.starts_with("SQLite 3."), "{}", c.info.version);
    assert_eq!((c.info.database.as_str(), c.database()), ("main", "main"));
    assert!(!c.caps().schemas && !c.caps().drop_database && !c.caps().truncate && c.caps().explain);

    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT id, name, balance, active, profile, photo, created_at, label, 1 + 1 AS two, 0.5 AS half, NULL AS empty FROM customer ORDER BY id");
    let kinds: Vec<Kind> = rs.columns().unwrap().iter().map(|c| c.kind).collect();
    assert_eq!(kinds, [Kind::Int, Kind::Text, Kind::Numeric, Kind::Bool, Kind::Json, Kind::Bytes, Kind::Text, Kind::Text, Kind::Int, Kind::Float, Kind::Other]);
    let r0 = texts(&rs, 0);
    assert_eq!(r0[..6], [Some("1".into()), Some("Ana Lima".into()), Some("1520.5".into()), Some("true".into()), Some("{\"city\": \"Lisbon\"}".into()), Some("\\xcafe".into())]);
    assert_eq!(r0[7].as_deref(), Some("Ana Lima <ana@example.com>"), "generated column");
    assert_eq!((r0[8].as_deref(), r0[9].as_deref(), r0[10].as_deref()), (Some("2"), Some("0.5"), None));
    let named = rs.named_origins().unwrap();
    assert_eq!(named[1].as_ref().map(|o| (o.schema.as_str(), o.table.as_str(), o.column.as_str())), Some(("main", "customer", "name")));
    assert!(named[8].is_none());
}

/// SQLite stores what it is given: a text in an INTEGER column stays text, and the grid shows
/// each cell as it is in the file, never a silent 0.
#[test]
fn mixed_storage_classes_show_as_stored() {
    let d = open(false); let (e, c) = (&d.e, d.c.clone());
    let s = c.session(None);
    ok(&e, &s, "CREATE TABLE m (n INTEGER, r REAL, b BOOLEAN, x BLOB)");
    ok(&e, &s, "INSERT INTO m VALUES (1, 1.5, 1, X'00'), ('abc', 'zz', 7, 'text'), (1.5, 2, 0, NULL), (NULL, NULL, NULL, NULL)");
    let rs = ok(&e, &s, "SELECT n, r, b, x FROM m");
    assert_eq!(col(&rs, 0), [Some("1".into()), Some("abc".into()), Some("1.5".into()), None]);
    assert_eq!(col(&rs, 1), [Some("1.5".into()), Some("zz".into()), Some("2".into()), None]);
    assert_eq!(col(&rs, 2), [Some("1".into()), Some("7".into()), Some("0".into()), None]);
    assert_eq!(col(&rs, 3), [Some("\\x00".into()), Some("text".into()), None, None]);
    let kinds: Vec<Kind> = rs.columns().unwrap().iter().map(|c| c.kind).collect();
    assert_eq!(kinds, [Kind::Text, Kind::Text, Kind::Text, Kind::Text], "each column turned to text");
    // Late in a long result too: the earlier chunks are rewritten.
    ok(&e, &s, "CREATE TABLE big (n INTEGER)");
    ok(&e, &s, "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 5000) INSERT INTO big SELECT i FROM c");
    ok(&e, &s, "INSERT INTO big VALUES ('late')");
    let rs = ok(&e, &s, "SELECT n FROM big ORDER BY rowid");
    let all = col(&rs, 0);
    assert_eq!((all.len(), all[0].as_deref(), all[4999].as_deref(), all[5000].as_deref()), (5001, Some("1"), Some("5000"), Some("late")));
    // STRICT tables hold their types.
    ok(&e, &s, "CREATE TABLE st (i INTEGER, t TEXT, r REAL, a ANY) STRICT");
    assert_eq!(run(&e, &s, "INSERT INTO st VALUES ('x', 1, 2, 3)").phase(), Phase::Failed);
    ok(&e, &s, "INSERT INTO st VALUES (1, 'a', 2.5, 'any')");
    let rs = ok(&e, &s, "SELECT * FROM st");
    assert_eq!(texts(&rs, 0), [Some("1".into()), Some("a".into()), Some("2.5".into()), Some("any".into())]);
}

#[test]
fn transactions_are_watched() {
    let d = open(true); let (e, c) = (&d.e, d.c.clone());
    let s = c.session(None);
    assert_eq!(s.tx().status, TxStatus::Idle);
    ok(&e, &s, "BEGIN");
    assert_eq!(s.tx().status, TxStatus::Open);
    ok(&e, &s, "DELETE FROM note");
    ok(&e, &s, "ROLLBACK");
    assert_eq!(s.tx().status, TxStatus::Idle);
    assert_eq!(col(&ok(&e, &s, "SELECT count(*) FROM note"), 0), [Some("2".into())]);
    ok(&e, &s, "BEGIN IMMEDIATE; INSERT INTO note VALUES ('x');");
    assert_eq!(s.tx().status, TxStatus::Open);
    let end = e.core.block_on(s.end_transaction(true)).unwrap();
    assert_eq!(end, dataman_core::TxEnd::Committed);
    assert_eq!(s.tx().status, TxStatus::Idle);
    assert_eq!(col(&ok(&e, &s, "SELECT count(*) FROM note"), 0), [Some("3".into())]);
    // A rolled-back statement (Explain Analyze, an agent's read) leaves no trace.
    let rs = s.execute_opts("DELETE FROM note", Vec::new(), true);
    e.core.block_on(rs.wait_finished());
    assert_eq!(col(&ok(&e, &s, "SELECT count(*) FROM note"), 0), [Some("3".into())]);
    // Parameters bind by position, numbers as numbers.
    let rs = s.execute_with("SELECT name FROM customer WHERE id = ?1 OR email = ?2 LIMIT ?3", vec![Some("2".into()), Some("ana@example.com".into()), Some("5".into())]);
    rs.fetch_all();
    e.core.block_on(rs.wait_finished());
    assert_eq!(rs.rows(), 2, "{:?}", rs.status().error);
}

#[test]
fn an_endless_query_is_interrupted() {
    let d = open(false); let (e, c) = (&d.e, d.c.clone());
    let s = c.session(None);
    let rs = s.execute("WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c) SELECT count(*) FROM c");
    std::thread::sleep(Duration::from_millis(300));
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await.expect("the interrupt ends it") });
    assert_eq!(rs.phase(), Phase::Cancelled, "{:?}", rs.status().error);
    assert_eq!(col(&ok(&e, &s, "SELECT 42"), 0), [Some("42".into())], "the connection still works");
}

#[test]
fn catalog_and_ddl() {
    let d = open(true); let (e, c) = (&d.e, d.c.clone());
    let db = "main";
    assert_eq!(e.core.block_on(c.databases()).unwrap(), ["main"]);
    assert_eq!(e.core.block_on(c.schemas(db)).unwrap(), ["main"]);
    let rels = e.core.block_on(c.relations(db, db)).unwrap();
    let names: Vec<(&str, RelKind)> = rels.iter().map(|r| (r.name.as_str(), r.kind)).collect();
    assert_eq!(names, [("customer", RelKind::Table), ("item", RelKind::Table), ("note", RelKind::Table), ("orders", RelKind::Table), ("v_order_customer", RelKind::View)], "no sqlite_sequence");
    let cols = e.core.block_on(c.columns(db, db, "customer")).unwrap();
    assert!(cols[0].primary_key && cols[0].type_name == "INTEGER");
    let label = cols.iter().find(|c| c.name == "label").unwrap();
    assert_eq!(label.default.as_deref(), Some("GENERATED (name || ' <' || coalesce(email, '?') || '>')"));
    let tier = cols.iter().find(|c| c.name == "tier").unwrap();
    assert_eq!((tier.nullable, tier.default.as_deref()), (Some(false), Some("'bronze'")));
    let idx = e.core.block_on(c.indexes(db, db, "orders")).unwrap();
    let st = idx.iter().find(|i| i.name == "orders_status_idx").unwrap();
    assert_eq!(st.columns, ["status", "customer_id"]);
    assert_eq!(st.definition.as_deref(), Some("CREATE INDEX orders_status_idx ON orders (status, customer_id)"));
    let idx = e.core.block_on(c.indexes(db, db, "customer")).unwrap();
    assert!(idx.iter().any(|i| i.unique && i.columns == ["email"] && i.definition.is_none()), "{idx:?}");

    let counts = e.core.block_on(c.object_counts(db, db)).unwrap();
    assert_eq!((counts.triggers, counts.functions, counts.sequences, counts.types), (Some(1), None, None, None));
    let tr = e.core.block_on(c.triggers(db, db)).unwrap();
    assert_eq!((tr[0].name.as_str(), tr[0].table.as_str(), tr[0].events.as_str()), ("orders_touch", "orders", "after update"));
    assert!(e.core.block_on(c.functions(db, db)).unwrap().is_empty());
    assert!(e.core.block_on(c.sequences(db, db)).unwrap().is_empty());
    assert!(e.core.block_on(c.rel_stats(db, db)).unwrap().is_empty(), "no pg_class here");
    let x = e.core.block_on(c.table_extras(db, db, "orders")).unwrap();
    let cons = x.constraints.unwrap();
    assert!(cons.iter().any(|k| k.kind == ConstraintKind::Primary && k.definition.as_deref() == Some("PRIMARY KEY (id)")), "{cons:?}");
    assert!(cons.iter().any(|k| k.kind == ConstraintKind::Foreign && k.definition.as_deref() == Some("FOREIGN KEY (customer_id) REFERENCES customer(id) ON DELETE CASCADE")), "{cons:?}");
    assert_eq!(x.triggers.unwrap().len(), 1);
    let cons = e.core.block_on(c.table_extras(db, db, "customer")).unwrap().constraints.unwrap();
    assert!(cons.iter().any(|k| k.kind == ConstraintKind::Unique && k.definition.as_deref() == Some("UNIQUE (email)")), "{cons:?}");
    assert!(e.core.block_on(c.sequence_value(db, db, "x")).is_err());

    let ddl = e.core.block_on(c.table_ddl(db, db, "orders")).unwrap();
    assert!(ddl.starts_with("-- main.orders: as stored in main\nCREATE TABLE orders ("), "{ddl}");
    assert!(ddl.contains("CREATE INDEX orders_status_idx") && ddl.contains("CREATE TRIGGER orders_touch"), "{ddl}");
    let v = e.core.block_on(c.view_definition(db, db, "v_order_customer")).unwrap().unwrap();
    assert!(v.contains("CREATE VIEW v_order_customer AS"), "{v}");
    let src = e.core.block_on(c.view_sources(db, db, "v_order_customer")).unwrap();
    let mut src: Vec<&str> = src.iter().map(|s| s.name.as_str()).collect();
    src.sort();
    assert_eq!(src, ["customer", "orders"]);

    let g = e.core.block_on(c.relationships(db, db)).unwrap();
    let l = g.links.iter().find(|l| l.from_table == "item").unwrap();
    assert_eq!((l.from_columns.clone(), l.to_table.as_str(), l.to_columns.clone()), (vec!["order_id".to_string()], "orders", vec!["id".to_string()]), "a bare REFERENCES points at the key");
    assert!(g.links.iter().any(|l| l.from_table == "orders" && l.to_table == "customer"));
    assert_eq!(e.core.block_on(c.primary_key(db, db, "item")).unwrap(), ["order_id", "line"]);
    assert_eq!(e.core.block_on(c.estimate_rows(db, db, "customer")).unwrap(), None);

    e.core.block_on(c.preload(db)).unwrap();
    let snap = c.schema.snapshot(db);
    assert_eq!(snap.dialect, Dialect::Sqlite);
    let r = snap.resolve(None, "Customer").expect("case-insensitive");
    assert!(snap.columns_of(r).iter().any(|c| c.name == "balance"));
    assert!(c.schema.search_path(db) == ["main"], "never PostgreSQL's public");
    let cat = e.core.block_on(c.catalog(db)).unwrap();
    assert!(cat.functions_named(None, "json_extract").count() >= 1);
}

#[test]
fn the_data_view_and_explain() {
    let d = open(true); let (e, c) = (&d.e, d.c.clone());
    let s = c.session(None);
    let mut v = TableView::new("main", "customer", vec!["id".into()], 2);
    v.dialect = Dialect::Sqlite;
    v.page = 1;
    assert_eq!(texts(&ok(&e, &s, &v.sql()), 0)[0].as_deref(), Some("3"));
    v.page = 0;
    v.filters.push(Filter::new("tier", Kind::Text, Op::In, "silver, bronze"));
    v.filters.push(Filter::new("name", Kind::Text, Op::Contains, "o'n"));
    v.filters.push(Filter::new("email", Kind::Text, Op::IsNull, ""));
    let f = ok(&e, &s, &v.sql());
    assert_eq!((f.rows(), texts(&f, 0)[1].as_deref()), (1, Some("Diego O'Neil")));
    let pct = Filter::new("name", Kind::Text, Op::Contains, "50%").sql_in(Dialect::Sqlite);
    assert_eq!(ok(&e, &s, &format!("SELECT count(*) FROM customer WHERE {pct}")).rows(), 1);
    assert_eq!(col(&ok(&e, &s, &format!("SELECT count(*) FROM customer WHERE {pct}")), 0), [Some("0".into())], "% is escaped");
    let n = e.core.block_on(c.count_rows("main", v.count_sql()).task).unwrap();
    assert_eq!(n, 1);

    let t = dataman_core::explain::Target::of(&c.info);
    let sql = dataman_core::explain::sql("SELECT * FROM orders WHERE status = 'paid'", true, t);
    assert_eq!(sql, "EXPLAIN QUERY PLAN SELECT * FROM orders WHERE status = 'paid'");
    let rs = ok(&e, &s, &sql);
    let names: Vec<String> = rs.columns().unwrap().iter().map(|c| c.name.clone()).collect();
    let snap = rs.snapshot();
    let rows: Vec<Vec<Option<String>>> = (0..snap.rows()).map(|r| texts(&rs, r)).collect();
    let text = dataman_core::explain::Plan::source_of(&names, &rows);
    assert!(text.contains("orders_status_idx"), "{text}");
}

#[test]
fn edits_by_key() {
    let d = open(true); let (e, c) = (&d.e, d.c.clone());
    let s = c.session(None);
    let db = "main";
    let edit = |sql: &str, row: usize, column: &str, to: &str| -> (KeySource, Vec<Outcome>) {
        let rs = ok(&e, &s, sql);
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
    let (src, out) = edit("SELECT * FROM Customer ORDER BY id", 1, "name", "Bruno C.");
    assert_eq!((src, out), (KeySource::PrimaryKey, vec![Outcome::Applied { rows: 1 }]));
    let rs = ok(&e, &s, "SELECT email, name FROM customer ORDER BY id");
    assert!(matches!(e.core.block_on(c.edit_target(db, &rs, None)).unwrap(), Editability::ReadOnly(_)), "email is unique but nullable: no key");
    ok(&e, &s, "CREATE TABLE tag (code TEXT NOT NULL UNIQUE, label TEXT); INSERT INTO tag VALUES ('a', 'A'), ('b', 'B')");
    let (src, out) = edit("SELECT code, label FROM tag ORDER BY code", 1, "label", "Bee");
    assert_eq!((src, out), (KeySource::UniqueIndex, vec![Outcome::Applied { rows: 1 }]));
    edit("SELECT * FROM customer ORDER BY id", 0, "name", "Ana L.");
    let (_, out) = edit("SELECT * FROM item ORDER BY order_id, line", 2, "qty", "9");
    assert_eq!(out, vec![Outcome::Applied { rows: 1 }], "a composite key on a WITHOUT ROWID table");
    assert_eq!(col(&ok(&e, &s, "SELECT name FROM customer ORDER BY id"), 0)[..2], [Some("Ana L.".into()), Some("Bruno C.".into())]);
    let (_, out) = edit("SELECT id, balance, photo FROM customer ORDER BY id", 0, "photo", "\\xbeef");
    assert_eq!(out, vec![Outcome::Applied { rows: 1 }]);
    assert_eq!(col(&ok(&e, &s, "SELECT hex(photo) FROM customer WHERE id = 1"), 0), [Some("BEEF".into())]);

    let rs = ok(&e, &s, "SELECT * FROM note");
    assert!(matches!(e.core.block_on(c.edit_target(db, &rs, None)).unwrap(), Editability::ReadOnly(_)), "no key, no rowid fallback");
    let rs = ok(&e, &s, "SELECT * FROM v_order_customer");
    assert!(matches!(e.core.block_on(c.edit_target(db, &rs, None)).unwrap(), Editability::ReadOnly(_)));
    let rs = ok(&e, &s, "SELECT id, label FROM customer");
    let Editability::Editable(t) = e.core.block_on(c.edit_target(db, &rs, None)).unwrap() else { panic!() };
    assert!(t.generated[1] && !t.generated[0], "the generated column is read-only");
    // A constraint's error is explained.
    let rs = ok(&e, &s, "SELECT id, email FROM customer ORDER BY id");
    let Editability::Editable(t) = e.core.block_on(c.edit_target(db, &rs, None)).unwrap() else { panic!() };
    let snap = rs.snapshot();
    let mut cs = ChangeSet::new();
    cs.edit(1, 1, CellValue::text("bruno@example.com"), CellValue::text("ana@example.com"));
    let r = e.core.block_on(c.apply_changes(Some(db), cs.plan(&t, &snap).unwrap())).unwrap();
    assert!(!r.committed && r.error.as_deref().is_some_and(|e| e.starts_with("Duplicate value")), "{:?}", r.error);
}

#[test]
fn a_write_lock_makes_others_wait_then_fail() {
    let d = open(true); let (e, c) = (&d.e, d.c.clone());
    let a = c.session(None);
    let b = c.session(None);
    ok(&e, &a, "BEGIN IMMEDIATE");
    ok(&e, &a, "INSERT INTO note VALUES ('a')");
    let t0 = Instant::now();
    let rs = run(&e, &b, "INSERT INTO note VALUES ('b')");
    assert_eq!(rs.phase(), Phase::Failed);
    assert!(t0.elapsed() >= Duration::from_secs(4), "busy_timeout waits first");
    let err = rs.status().error.unwrap();
    assert_eq!(err.code.as_deref(), Some("SQLITE_BUSY"));
    assert!(dataman_core::edit::explain_error(&err).starts_with("The database is locked"));
    ok(&e, &a, "COMMIT");
    ok(&e, &b, "INSERT INTO note VALUES ('b')");
}

#[test]
fn agents_read_and_write_in_bounds() {
    let d = open(true); let (e, c) = (&d.e, d.c.clone());
    let never = std::future::pending::<()>;
    let o = e.core.block_on(c.bounded(None, "SELECT id, name FROM customer ORDER BY id", vec![], Mode::Read, Limits::default(), never())).unwrap();
    assert_eq!(o.parts.len(), 1);
    let r = e.core.block_on(c.bounded(None, "DELETE FROM customer", vec![], Mode::Read, Limits::default(), never()));
    assert!(matches!(r, Err(Stop::Refused(_))), "{r:?}");
    let o = e.core.block_on(c.bounded(None, "UPDATE orders SET notes = ? WHERE id = ?", vec![Some("x".into()), Some("2".into())], Mode::Write, Limits::default(), never())).unwrap();
    assert!(o.committed);
    let o = e.core.block_on(c.bounded(None, "SELECT notes FROM orders WHERE id = ?", vec![Some("2".into())], Mode::Read, Limits::default(), never())).unwrap();
    assert_eq!(o.parts.len(), 1);
    let lim = Limits { timeout: Duration::from_millis(300), ..Limits::default() };
    let r = e.core.block_on(c.bounded(None, "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c) SELECT count(*) FROM c", vec![], Mode::Read, lim, never()));
    assert!(matches!(r, Err(Stop::Timeout(_))), "{r:?}");
}

#[test]
fn a_missing_or_foreign_file_is_an_error() {
    let e = env();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.sqlite");
    let r = e.core.block_on(e.core.connect(ConnectionProfile::new_file(Engine::Sqlite, missing.to_string_lossy()), None));
    assert!(r.is_err_and(|e| format!("{e:#}").contains("does not exist")));
    assert!(!missing.exists(), "never created behind the user's back");
    let text = dir.path().join("notes.db");
    std::fs::write(&text, "not a database, just text that is long enough to be a header").unwrap();
    let r = e.core.block_on(e.core.connect(ConnectionProfile::new_file(Engine::Sqlite, text.to_string_lossy()), None));
    assert!(r.is_err_and(|e| format!("{e:#}").contains("not a SQLite database")));
}

#[test]
fn read_only_profiles_cannot_write() {
    let db = open(true);
    let e = env();
    let mut p = ConnectionProfile::new_file(Engine::Sqlite, db.path.to_string_lossy());
    p.read_only = true;
    let c = e.core.block_on(e.core.connect(p, None)).unwrap();
    let s = c.session(None);
    assert_eq!(col(&ok(&e, &s, "SELECT count(*) FROM customer"), 0), [Some("3".into())]);
    let rs = run(&e, &s, "DELETE FROM customer");
    assert_eq!(rs.phase(), Phase::Failed, "refused, or the file says no");
    assert_eq!(col(&ok(&e, &s, "SELECT count(*) FROM customer"), 0), [Some("3".into())]);

    // Read-only switched on the connection: the session reopens the file read-only, and back.
    let c = &db.c;
    let s = c.session(None);
    ok(&db.e, &s, "INSERT INTO note VALUES ('rw')");
    c.set_read_only_for("main", true);
    let rs = run(&db.e, &s, "INSERT INTO note VALUES ('ro')");
    assert_eq!(rs.phase(), Phase::Failed);
    c.set_read_only_for("main", false);
    ok(&db.e, &s, "INSERT INTO note VALUES ('rw again')");
    assert_eq!(col(&ok(&db.e, &s, "SELECT count(*) FROM note"), 0), [Some("4".into())]);
}
