//! Vitess through its vtgate, and PlanetScale, which runs it. Each test skips itself when nothing
//! answers. vttestserver (amd64 only), from the repository's root:
//!
//! ```text
//! docker run --rm -d --platform linux/amd64 -p 127.0.0.1:33577:33577 -v "$PWD/e2e/vitess/unsharded:/schema:ro" \
//!   vitess/vttestserver:mysql80 /vt/bin/vttestserver --port=33574 --mysql_bind_host=0.0.0.0 \
//!   --keyspaces=dataman --num_shards=1 --schema_dir=/schema
//! ```
//!
//! and a sharded keyspace the same way from `e2e/vitess/sharded`, with `--num_shards=2` and
//! `-p 127.0.0.1:33587:33577`. DATAMAN_VITESS and DATAMAN_VITESS_SHARDED move them.
//! PlanetScale: DATAMAN_PLANETSCALE_URL, as its Connect page shows it (password included), on a
//! branch the test only reads.

mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::config::SslMode;
use dataman_core::schema::{ConstraintKind, RelKind};
use dataman_core::server::Flavor;
use dataman_core::sql::Dialect;

const UNSHARDED: (&str, &str) = ("DATAMAN_VITESS", "127.0.0.1:33577");
const SHARDED: (&str, &str) = ("DATAMAN_VITESS_SHARDED", "127.0.0.1:33587");

fn vitess(e: &Env, at: (&str, &str), read_only: bool) -> Option<Arc<Connection>> {
    let mut p = vitess_profile(at.0, at.1);
    p.read_only = read_only;
    let c = connect_with(e, p, None)?;
    assert_eq!(c.info.flavor, Flavor::Vitess, "{} answers as {}", at.1, c.info.version);
    Some(c)
}

fn never() -> std::future::Pending<()> {
    std::future::pending()
}

/// A sharded keyspace routes only what its VSchema names (`e2e/vitess/sharded`): `vt_items`.
fn setup(e: &Env, c: &Arc<Connection>, sharded: bool) {
    exec_quiet(e, c, &["DROP VIEW IF EXISTS vt_v", "DROP TABLE IF EXISTS vt_child", "DROP TABLE IF EXISTS vt_parent", "DROP TABLE IF EXISTS vt_items"]);
    exec(
        e,
        c,
        &[
            "CREATE TABLE vt_items (id BIGINT PRIMARY KEY, name VARCHAR(50), email VARCHAR(80), code VARCHAR(90) AS (CONCAT('x', email)) STORED, KEY vt_items_name (name))",
            "INSERT INTO vt_items (id, name, email) VALUES (1, 'a', 'a@x.io'), (2, 'b', 'b@x.io'), (3, 'c', 'c@x.io')",
        ],
    );
    if !sharded {
        exec(
            e,
            c,
            &[
                "CREATE TABLE vt_parent (id INT PRIMARY KEY)",
                "CREATE TABLE vt_child (id INT PRIMARY KEY, pid INT, CONSTRAINT vt_child_fk FOREIGN KEY (pid) REFERENCES vt_parent (id))",
                "CREATE VIEW vt_v AS SELECT id, email FROM vt_items",
            ],
        );
    }
}

/// The tablets name a keyspace's database their own way (`vt_dataman_0`): what the catalog reads
/// back is the keyspace, or the tree, autocomplete and the agents' rules miss it.
fn catalog(at: (&str, &str), sharded: bool) {
    let e = env();
    let Some(c) = vitess(&e, at, false) else { return };
    let ks = c.database().to_string();
    assert!(c.info.version.starts_with("Vitess ") && c.info.major() == 8, "{}", c.info.version);
    assert!(!c.caps().read_only_held, "vtgate drops SET SESSION TRANSACTION READ ONLY");
    setup(&e, &c, sharded);

    e.core.block_on(c.preload(&ks)).unwrap();
    let cached = c.schema.cached_columns(&ks, &ks, "vt_items");
    assert!(cached.is_some_and(|cols| cols.iter().any(|c| c.name == "email")), "autocomplete has the keyspace's columns: {:?}", c.schema.notes());

    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.contains(&ks) && !dbs.iter().any(|d| d.starts_with("vt_") || d == "_vt"), "keyspaces, not the tablets' databases: {dbs:?}");
    let rels = e.core.block_on(c.relations(&ks, &ks)).unwrap();
    assert!(rels.iter().any(|r| r.name == "vt_items" && r.kind == RelKind::Table), "{rels:?}");
    let cols = e.core.block_on(c.columns(&ks, &ks, "vt_items")).unwrap();
    assert!(cols[0].name == "id" && cols[0].primary_key, "{cols:?}");
    assert!(cols.iter().any(|c| c.name == "code" && c.default.as_deref().is_some_and(|d| d.starts_with("GENERATED"))), "{cols:?}");
    let idx = e.core.block_on(c.indexes(&ks, &ks, "vt_items")).unwrap();
    assert!(idx.iter().any(|i| i.name == "vt_items_name" && i.columns == ["name"]), "{idx:?}");

    // Vitess refused the listing's correlated subquery (VT12001). Routines are made on the
    // tablets' MySQL, never through vtgate: none here.
    let fs = e.core.block_on(c.functions(&ks, &ks)).unwrap();
    let counts = e.core.block_on(c.object_counts(&ks, &ks)).unwrap();
    assert_eq!(counts.functions, Some(fs.len() as u64));
    e.core.block_on(c.triggers(&ks, &ks)).unwrap();

    // Masks follow a generated column to what it is computed from.
    let generated = e.core.block_on(c.generated_columns(&ks, never())).unwrap();
    // (vttestserver keeps every shard on one MySQL: the other shards' databases are seen too.)
    let code = generated.iter().find(|g| g.schema == ks && g.table == "vt_items" && g.column == "code");
    assert!(code.is_some_and(|g| g.from.iter().any(|f| f.eq_ignore_ascii_case("email"))), "{generated:?}");

    if !sharded {
        let x = e.core.block_on(c.table_extras(&ks, &ks, "vt_child")).unwrap();
        let fk = x.constraints.unwrap().into_iter().find(|c| c.kind == ConstraintKind::Foreign).unwrap();
        assert_eq!(fk.definition.as_deref(), Some("FOREIGN KEY (pid) REFERENCES vt_parent(id)"));
        let g = e.core.block_on(c.relationships(&ks, &ks)).unwrap();
        assert!(g.links.iter().any(|l| l.from_table == "vt_child" && l.to_schema == ks && l.to_table == "vt_parent"), "{:?}", g.links);
        // What the agents' rules check a view against: a hidden table must be found by its name.
        let src = e.core.block_on(c.view_sources(&ks, &ks, "vt_v")).unwrap();
        assert!(src.iter().any(|s| s.schema == ks && s.name == "vt_items"), "{src:?}");
    }
}

/// vtgate takes `SET SESSION TRANSACTION READ ONLY` and keeps nothing of it: an UPDATE on such a
/// session is stored. Its READ ONLY transactions the tablets do hold.
fn read_only(at: (&str, &str), sharded: bool) {
    let e = env();
    let Some(rw) = vitess(&e, at, false) else { return };
    let ro = vitess(&e, at, true).unwrap();
    let ks = rw.database().to_string();
    setup(&e, &rw, sharded);
    let name_of = |id: i64| query(&e, &rw, &format!("SELECT name FROM vt_items WHERE id = {id}")).snapshot().text(0, 0).map(|t| t.into_owned());

    // A tab refuses before sending what is not known to read.
    for sql in [
        "UPDATE vt_items SET name = 'x' WHERE id = 1",
        "CALL p()",
        "VEXPLAIN QUERIES UPDATE vt_items SET name = 'x' WHERE id = 1",
        "BEGIN",
        "START TRANSACTION",
        "SET autocommit = 0",
        "SELECT 1; COMMIT",
        "SELECT NEXT VALUE FROM vt_seq",
        "LOCK TABLES vt_items WRITE",
    ] {
        assert!(ro.read_only_refusal(&ks, sql).is_some(), "{sql}");
    }
    for sql in ["SELECT * FROM vt_items", "START TRANSACTION READ ONLY", "COMMIT", "SHOW VITESS_SHARDS"] {
        assert_eq!(ro.read_only_refusal(&ks, sql), None, "{sql}");
    }

    // What the session runs anyway runs in a READ ONLY transaction the server holds.
    let tab = ro.session(None);
    let run = |sql: &str| {
        let rs = tab.execute(sql);
        rs.fetch_all();
        (e.core.block_on(rs.wait_finished()), rs)
    };
    let (ph, rs) = run("UPDATE vt_items SET name = 'x' WHERE id = 1");
    assert_eq!(ph, Phase::Failed);
    let err = rs.status().error.unwrap();
    assert_eq!(err.code.as_deref(), Some("1792"));
    assert_eq!(err.message, "Cannot execute statement in a READ ONLY transaction.", "the server's message, without the statement's values");
    assert_eq!(name_of(1).as_deref(), Some("a"));
    assert!(!tab.tx().status.is_open(), "the transaction around a run ends with it");
    let rolled = tab.execute_opts("UPDATE vt_items SET name = 'x' WHERE id = 1", vec![], true);
    assert_eq!(e.core.block_on(rolled.wait_finished()), Phase::Failed, "a statement run to be rolled back is no write either");
    let (ph, rs) = run("SELECT name FROM vt_items ORDER BY id");
    assert_eq!((ph, rs.rows()), (Phase::Done, 3), "{:?}", rs.status().error);
    assert_eq!(run("START TRANSACTION READ ONLY").0, Phase::Done);
    assert!(tab.tx().status.is_open());
    assert_eq!(run("UPDATE vt_items SET name = 'x' WHERE id = 1").0, Phase::Failed);
    assert_eq!(run("ROLLBACK").0, Phase::Done);
    assert_eq!(name_of(1).as_deref(), Some("a"));

    // Agents: reads in a READ ONLY transaction of their own, and Vitess' sequences hand out
    // values inside one.
    let call = |c: &Arc<Connection>, sql: &str, mode: Mode| e.core.block_on(c.bounded(None, sql, vec![], mode, Limits::default(), never()));
    let o = call(&ro, "SELECT name FROM vt_items WHERE id = 1", Mode::Read).unwrap();
    assert_eq!(o.rows[0][0], serde_json::json!("a"));
    assert!(matches!(call(&ro, "UPDATE vt_items SET name = 'y' WHERE id = 1", Mode::Write), Err(Stop::Refused(_))));
    for c in [&ro, &rw] {
        assert!(matches!(call(c, "SELECT NEXT VALUE FROM vt_seq", Mode::Read), Err(Stop::Refused(r)) if r.contains("sequence")));
        assert!(matches!(call(c, "VEXPLAIN QUERIES SELECT 1", Mode::Read), Err(Stop::Refused(_))));
    }
    let shown = rw.session(None).execute_agent("SELECT NEXT 2 VALUES FROM vt_seq", vec![], "test");
    assert_eq!(e.core.block_on(shown.wait_finished()), Phase::Failed, "an agent's read in a tab too");
    let o = call(&rw, "UPDATE vt_items SET name = 'w' WHERE id = 2", Mode::Write).unwrap();
    assert!(o.committed);
    assert_eq!(name_of(2).as_deref(), Some("w"));
}

/// One test per keyspace: both parts make the same tables.
#[test]
fn unsharded() {
    catalog(UNSHARDED, false);
    read_only(UNSHARDED, false);
}

#[test]
fn sharded() {
    catalog(SHARDED, true);
    read_only(SHARDED, true);
}

/// Detection, TLS and the read-only hold on PlanetScale itself. Reads only: an UPDATE whose WHERE
/// matches no row is all it sends that could write.
#[test]
fn planetscale() {
    let Ok(url) = std::env::var("DATAMAN_PLANETSCALE_URL") else {
        eprintln!("skipping: DATAMAN_PLANETSCALE_URL is not set");
        return;
    };
    let e = env();
    let (mut p, password) = dataman_core::connstr::parse_profile(&url).expect("a mysql:// URL");
    assert_eq!(p.sslmode, SslMode::VerifyFull, "PlanetScale's certificates are checked");
    p.read_only = true;
    let c = connect_with(&e, p, password.as_deref()).expect("PlanetScale answers");
    assert_eq!(c.info.flavor, Flavor::Vitess, "{}", c.info.version);
    assert!(c.tls_in_use());
    let db = c.database().to_string();
    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.contains(&db) && !dbs.iter().any(|d| d == "_vt"), "{dbs:?}");
    let rels = e.core.block_on(c.relations(&db, &db)).unwrap();
    e.core.block_on(c.functions(&db, &db)).unwrap();
    e.core.block_on(c.object_counts(&db, &db)).unwrap();
    e.core.block_on(c.triggers(&db, &db)).unwrap();
    e.core.block_on(c.preload(&db)).unwrap();
    assert!(c.schema.notes().is_empty(), "{:?}", c.schema.notes());
    assert!(c.read_only_refusal(&db, "UPDATE t SET a = 1 WHERE id = 1").is_some());
    let o = e.core.block_on(c.bounded(None, "SELECT 1", vec![], Mode::Read, Limits::default(), never())).unwrap();
    assert_eq!(o.rows.len(), 1);
    if let Some(t) = rels.iter().find(|r| r.kind == RelKind::Table) {
        let cols = e.core.block_on(c.columns(&db, &db, &t.name)).unwrap();
        let (q, col) = (Dialect::MySql, Dialect::MySql.quote_ident(&cols[0].name));
        let rs = c.session(None).execute(format!("UPDATE {} SET {col} = {col} WHERE 1 = 0", q.quote_ident(&t.name)));
        assert_eq!(e.core.block_on(rs.wait_finished()), Phase::Failed, "the server holds the transaction it runs in");
        assert!(rs.status().error.is_some_and(|err| err.message.contains("READ ONLY")), "{:?}", rs.status().error);
    }
}
