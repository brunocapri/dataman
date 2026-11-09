//! Servers that speak an engine's protocol and are products of their own: each one is told by
//! what it reports, behaves as its engine, and fails clean where it lacks something. Each test
//! reads its server's connection string (as pasted in the connection sheet) from an environment
//! variable and skips itself without one, or when the server does not answer:
//! `DATAMAN_TIMESCALE=postgres://postgres:pw@127.0.0.1:5432/postgres`, `DATAMAN_YUGABYTE`,
//! `DATAMAN_PERCONA`, `DATAMAN_MARIADB`, `DATAMAN_DRAGONFLY`, `DATAMAN_KEYDB`, `DATAMAN_GARNET`, `DATAMAN_FERRETDB`,
//! and `DATAMAN_MONGODB` (MongoDB itself, for what FerretDB is compared with).

mod common;

use common::*;
use dataman_core::agent_login::live;
use dataman_core::nosql::{Body, Output, Page};
use dataman_core::schema::ViewSourceKind;
use dataman_core::server::Flavor;
use dataman_core::{ConnectionProfile, Engine, ResultSet};

fn server(var: &str, read_only: bool) -> Option<(Env, Arc<Connection>)> {
    server_with(var, |p| p.read_only = read_only)
}

fn server_with(var: &str, change: impl FnOnce(&mut ConnectionProfile)) -> Option<(Env, Arc<Connection>)> {
    let Ok(url) = std::env::var(var) else {
        eprintln!("skipping: {var} is not set");
        return None;
    };
    let (mut p, pw) = dataman_core::connstr::parse_profile(&url).unwrap_or_else(|| panic!("{var} does not read as a connection string"));
    change(&mut p);
    let e = env();
    match e.core.block_on(e.core.connect(p.clone(), pw)) {
        Ok(c) => Some((e, c)),
        Err(err) => {
            eprintln!("skipping: cannot reach {} ({err:#})", p.summary());
            None
        }
    }
}

fn run(e: &Env, c: &Arc<Connection>, sql: &str) -> Arc<ResultSet> {
    let rs = c.session(None).execute(sql);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
    rs
}

fn ok(e: &Env, c: &Arc<Connection>, sql: &str) -> Arc<ResultSet> {
    let rs = run(e, c, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

fn failed(e: &Env, c: &Arc<Connection>, sql: &str) -> dataman_core::result::ErrorInfo {
    let rs = run(e, c, sql);
    assert_eq!(rs.phase(), Phase::Failed, "{sql} ran");
    rs.status().error.expect("an error")
}

fn first(rs: &ResultSet) -> Option<String> {
    rs.snapshot().text(0, 0).map(|s| s.into_owned())
}

/// The label names the flavor, and behavior still goes by the engine's release: a flavor's own
/// release number (YugabyteDB's 2026, Dragonfly's 2) never reaches it.
fn told(c: &Connection, flavor: Flavor, label: &str, engine_major: std::ops::RangeInclusive<u32>) {
    assert_eq!(c.info.flavor, flavor, "{}", c.info.version);
    assert!(c.info.version.starts_with(label), "{} is not {label}…", c.info.version);
    assert!(engine_major.contains(&c.info.major()), "{}: release {:?}", c.info.version, c.info.release);
    assert!(c.info.short_version().starts_with(label), "{}", c.info.short_version());
}

#[test]
fn timescale_is_told_by_its_extension_and_its_schemas_hide() {
    let Some((e, c)) = server("DATAMAN_TIMESCALE", false) else { return };
    told(&c, Flavor::Timescale, "TimescaleDB 2.", 13..=99);
    assert!(c.info.version.contains(" (PostgreSQL "), "{}", c.info.version);
    ok(&e, &c, "DROP TABLE IF EXISTS public.dataman_flavor_metrics");
    ok(&e, &c, "CREATE TABLE public.dataman_flavor_metrics (at timestamptz NOT NULL, v int)");
    ok(&e, &c, "SELECT create_hypertable('public.dataman_flavor_metrics', 'at')");
    ok(&e, &c, "INSERT INTO public.dataman_flavor_metrics VALUES (now(), 1), (now() - interval '30 days', 2)");

    let db = c.database().to_string();
    let schemas = e.core.block_on(c.schemas(&db)).unwrap();
    for internal in ["_timescaledb_internal", "_timescaledb_catalog", "timescaledb_information"] {
        assert!(schemas.iter().any(|s| s == internal), "{internal} not listed: {schemas:?}");
    }
    let shown: Vec<&String> = schemas.iter().filter(|s| !c.engine().is_system_schema(s)).collect();
    assert!(shown.iter().any(|s| *s == "public"), "{shown:?}");
    assert!(shown.iter().all(|s| !s.starts_with("_timescaledb") && !s.starts_with("timescaledb_")), "{shown:?}");
    // The chunks are where the hypertable's rows are: in a schema the tree does not show.
    let chunks = e.core.block_on(c.relations(&db, "_timescaledb_internal")).unwrap();
    assert!(chunks.iter().any(|r| r.name.ends_with("_chunk")), "{chunks:?}");
    let public = e.core.block_on(c.relations(&db, "public")).unwrap();
    assert!(public.iter().any(|r| r.name == "dataman_flavor_metrics"), "{public:?}");
    assert_eq!(first(&ok(&e, &c, "SELECT count(*) FROM public.dataman_flavor_metrics")).as_deref(), Some("2"));
    ok(&e, &c, "DROP TABLE public.dataman_flavor_metrics");
}

#[test]
fn yugabyte_is_told_by_its_version_and_listen_fails_clean() {
    let Some((e, c)) = server("DATAMAN_YUGABYTE", false) else { return };
    told(&c, Flavor::Yugabyte, "YugabyteDB 2", 11..=99);
    assert!(c.info.version.contains(" (PostgreSQL "), "{}", c.info.version);
    let err = failed(&e, &c, "LISTEN dataman_flavor");
    assert!(err.message.contains("LISTEN/NOTIFY is disabled"), "{err:?}");
    assert_eq!(first(&ok(&e, &c, "SELECT 1")).as_deref(), Some("1"));
    ok(&e, &c, "DROP TABLE IF EXISTS public.dataman_flavor_ro");
    ok(&e, &c, "CREATE TABLE public.dataman_flavor_ro (id int PRIMARY KEY)");
    let (e2, ro) = server("DATAMAN_YUGABYTE", true).expect("reached once");
    let err = failed(&e2, &ro, "INSERT INTO public.dataman_flavor_ro VALUES (1)");
    assert_eq!(err.code.as_deref(), Some("25006"), "{err:?}");
    ok(&e, &c, "DROP TABLE public.dataman_flavor_ro");
}

#[test]
fn percona_is_told_by_its_version_comment() {
    let Some((e, c)) = server("DATAMAN_PERCONA", false) else { return };
    told(&c, Flavor::Percona, "Percona Server 8.", 8..=8);
    assert!(!c.info.is_mariadb());
    ok(&e, &c, "CREATE DATABASE IF NOT EXISTS dataman_flavor");
    ok(&e, &c, "CREATE TABLE IF NOT EXISTS dataman_flavor.ro (id int PRIMARY KEY)");
    let (e2, ro) = server("DATAMAN_PERCONA", true).expect("reached once");
    failed(&e2, &ro, "INSERT INTO dataman_flavor.ro VALUES (1)");
    assert_eq!(first(&ok(&e, &c, "SELECT count(*) FROM dataman_flavor.ro")).as_deref(), Some("0"));
    ok(&e, &c, "DROP DATABASE dataman_flavor");
}

/// What a read-only connection to a MySQL-wire server must keep from writing, beyond what its
/// read-only session holds: the session itself (every one of these flavors writes once it is
/// turned off), files (`INTO OUTFILE` is no write to a read-only session), and a database marked
/// read-only on a writable connection (its session is the database the tab began on).
fn mysql_read_only_holds(var: &str, flavor: Flavor) {
    let Some((e, c)) = server(var, false) else { return };
    assert_eq!(c.info.flavor, flavor, "{}", c.info.version);
    let maria = flavor == Flavor::MariaDb;
    for sql in ["DROP DATABASE IF EXISTS dataman_flavor_ro", "DROP DATABASE IF EXISTS dataman_flavor_rw", "CREATE DATABASE dataman_flavor_ro", "CREATE DATABASE dataman_flavor_rw"] {
        ok(&e, &c, sql);
    }
    ok(&e, &c, "CREATE TABLE dataman_flavor_ro.t (id INT PRIMARY KEY, v INT)");
    ok(&e, &c, "INSERT INTO dataman_flavor_ro.t VALUES (1, 0)");
    ok(&e, &c, "CREATE FUNCTION dataman_flavor_ro.bump() RETURNS INT DETERMINISTIC MODIFIES SQL DATA BEGIN UPDATE dataman_flavor_ro.t SET v = v + 1 WHERE id = 1; RETURN 1; END");
    let v = || first(&ok(&e, &c, "SELECT v FROM dataman_flavor_ro.t WHERE id = 1"));

    let (e2, ro) = server_with(var, |p| p.read_only = true).expect("reached once");
    let db = "dataman_flavor_ro";
    let mut lifts = vec![
        "SET SESSION TRANSACTION READ WRITE",
        "SET TRANSACTION READ WRITE",
        "SET SESSION transaction_read_only = 0",
        "SET @@session.transaction_read_only = OFF; SELECT bump()",
        "START TRANSACTION READ WRITE",
        "(SELECT * FROM t) INTO OUTFILE '/tmp/dataman_flavor_out'",
        "DESC ANALYZE UPDATE t a JOIN t b ON a.id = b.id SET a.v = 9",
    ];
    if maria {
        lifts.extend(["SET STATEMENT transaction_read_only = 0 FOR UPDATE t SET v = 9 WHERE id = 1", "BEGIN NOT ATOMIC UPDATE t SET v = 9 WHERE id = 1; END", "ANALYZE UPDATE t SET v = 9 WHERE id = 1"]);
    } else {
        lifts.extend(["TABLE t INTO OUTFILE '/tmp/dataman_flavor_out'", "VALUES ROW(1) INTO DUMPFILE '/tmp/dataman_flavor_out'"]);
    }
    for sql in lifts {
        assert!(ro.read_only_refusal(db, sql).is_some(), "{sql}");
    }
    assert_eq!(ro.read_only_refusal(db, "SELECT @@transaction_read_only"), None);
    // What the check lets through, the session holds.
    let tab = ro.session(Some(db));
    let rs = tab.execute("SELECT bump()");
    rs.fetch_all();
    assert_eq!(e2.core.block_on(rs.wait_finished()), Phase::Failed);
    assert_eq!(v().as_deref(), Some("0"));

    // A database marked read-only on a writable connection: a tab on another one moves to it.
    let (e3, per_db) = server_with(var, |p| _ = p.read_only_databases.insert(db.into(), true)).expect("reached once");
    let tab = per_db.session(Some("dataman_flavor_rw"));
    let run = |sql: &str| {
        let rs = tab.execute(sql);
        rs.fetch_all();
        (e3.core.block_on(rs.wait_finished()), rs)
    };
    // `USE` ran, and the server stays there though the run failed after it.
    for sql in ["USE dataman_flavor_ro; SELECT bump()", "SELECT bump()"] {
        assert!(per_db.read_only_refusal(&tab.current_database(), sql).is_none(), "the session holds it: {sql}");
        assert_eq!(run(sql).0, Phase::Failed, "{sql}");
        assert_eq!(v().as_deref(), Some("0"), "{sql}");
    }
    assert_eq!(run("USE dataman_flavor_rw").0, Phase::Done);
    // A transaction begun while on a writable database stays writable: nothing more runs in it.
    assert_eq!(run("BEGIN").0, Phase::Done);
    let (ph, rs) = run("USE dataman_flavor_ro");
    assert_eq!(ph, Phase::Failed);
    assert!(rs.status().error.unwrap().message.contains("end it first"));
    assert_eq!(run("ROLLBACK").0, Phase::Done);
    ok(&e, &c, "DROP DATABASE dataman_flavor_ro");
    ok(&e, &c, "DROP DATABASE dataman_flavor_rw");
}

#[test]
fn percona_read_only_holds() {
    mysql_read_only_holds("DATAMAN_PERCONA", Flavor::Percona);
}

#[test]
fn mariadb_read_only_holds() {
    mysql_read_only_holds("DATAMAN_MARIADB", Flavor::MariaDb);
}

/// MariaDB keeps no VIEW_TABLE_USAGE: what a view reads comes from its definition, or agents
/// were refused every view on a connection with rules.
#[test]
fn mariadb_views_say_what_they_read() {
    let Some((e, c)) = server("DATAMAN_MARIADB", false) else { return };
    for sql in [
        "DROP DATABASE IF EXISTS dataman_flavor_v",
        "DROP DATABASE IF EXISTS dataman_flavor_o",
        "CREATE DATABASE dataman_flavor_v",
        "CREATE DATABASE dataman_flavor_o",
        "CREATE TABLE dataman_flavor_v.users (id INT, email TEXT)",
        "CREATE TABLE dataman_flavor_o.secrets (id INT, s TEXT)",
        "CREATE FUNCTION dataman_flavor_v.tag(x INT) RETURNS INT DETERMINISTIC RETURN x + 1",
        "CREATE VIEW dataman_flavor_v.v AS SELECT u.id, dataman_flavor_v.tag(u.id) AS t, s.s FROM dataman_flavor_v.users u JOIN dataman_flavor_o.secrets s ON s.id = u.id",
    ] {
        ok(&e, &c, sql);
    }
    let src = e.core.block_on(c.view_sources("dataman_flavor_v", "dataman_flavor_v", "v")).unwrap();
    let has = |schema: &str, name: &str, kind: ViewSourceKind| src.iter().any(|s| s.schema == schema && s.name.eq_ignore_ascii_case(name) && s.kind == kind);
    assert!(has("dataman_flavor_v", "users", ViewSourceKind::Relation) && has("dataman_flavor_o", "secrets", ViewSourceKind::Relation), "{src:?}");
    assert!(src.iter().any(|s| s.name.eq_ignore_ascii_case("tag") && s.kind == ViewSourceKind::Function), "{src:?}");
    assert!(e.core.block_on(c.view_sources("dataman_flavor_v", "dataman_flavor_v", "none")).is_err());
    ok(&e, &c, "DROP DATABASE dataman_flavor_v");
    ok(&e, &c, "DROP DATABASE dataman_flavor_o");
}

/// MariaDB's catalog writes a default as SQL: `'bronze'`, `curdate()`, and `NULL` for none.
#[test]
fn mariadb_defaults_read_as_written() {
    let Some((e, c)) = server("DATAMAN_MARIADB", false) else { return };
    told(&c, Flavor::MariaDb, "MariaDB ", 10..=99);
    ok(&e, &c, "CREATE DATABASE IF NOT EXISTS dataman_flavor");
    ok(&e, &c, "DROP TABLE IF EXISTS dataman_flavor.d");
    ok(&e, &c, "CREATE TABLE dataman_flavor.d (id INT AUTO_INCREMENT PRIMARY KEY, a VARCHAR(10) DEFAULT 'it''s', b INT DEFAULT 3, c VARCHAR(5), d DATE DEFAULT curdate(), e INT DEFAULT (b + 1), g VARCHAR(10) DEFAULT 'NULL', j INT AS (b * 2) VIRTUAL)");
    let cols = e.core.block_on(c.columns("dataman_flavor", "dataman_flavor", "d")).unwrap();
    let default = |n: &str| cols.iter().find(|c| c.name == n).unwrap().default.clone();
    assert_eq!(default("id").as_deref(), Some("AUTO_INCREMENT"));
    assert_eq!(default("a").as_deref(), Some("'it''s'"));
    assert_eq!(default("b").as_deref(), Some("3"));
    assert_eq!(default("c"), None, "no default");
    assert_eq!(default("d").as_deref(), Some("curdate()"));
    assert_eq!(default("e").as_deref(), Some("(`b` + 1)"));
    assert_eq!(default("g").as_deref(), Some("'NULL'"));
    assert!(default("j").is_some_and(|d| d.starts_with("GENERATED")), "{:?}", default("j"));
    ok(&e, &c, "DROP DATABASE dataman_flavor");
}

fn nosql(e: &Env, c: &Arc<Connection>, text: &str) -> Result<Output, String> {
    let db = c.database().to_string();
    e.core.block_on(c.run_nosql(&db, text, Page::default())).map(|r| r.output).map_err(|err| format!("{err:#}"))
}

/// Dataman offers no agent login where the server could not hold one, and says why before
/// anything runs: Dragonfly's ACL refuses a selector, Garnet's keeps no key pattern, FerretDB has
/// no roles (all measured).
fn no_agent_login(e: &Env, c: &Arc<Connection>) {
    let p = c.profile.clone();
    assert!(!e.core.block_on(live::has_logins(c, &p)).unwrap());
    let err = format!("{:#}", e.core.block_on(live::probe(c, &p)).err().expect("no probe"));
    assert!(err.starts_with(&format!("Dataman makes no agent login on {}", c.info.flavor.label(p.engine))), "{err}");
}

/// A Redis-wire server: told apart, reads and writes as Redis, read-only holds, and an agent
/// login is judged by the Redis release it answers as.
fn redis_like(var: &str, flavor: Flavor, label: &str) -> Option<(Env, Arc<Connection>)> {
    let (e, c) = server(var, false)?;
    told(&c, flavor, label, 6..=9);
    nosql(&e, &c, "SET dataman_flavor \"hi\"").unwrap();
    let Output::Docs(d) = nosql(&e, &c, "GET dataman_flavor").unwrap() else { panic!("GET") };
    assert_eq!(d.docs.iter().map(|d| &d.body).collect::<Vec<_>>(), [&Body::Json(serde_json::json!({ "value": "hi" }))]);
    let (e2, ro) = server(var, true).expect("reached once");
    assert!(nosql(&e2, &ro, "SET dataman_flavor x").unwrap_err().contains("read-only"));
    let p: ConnectionProfile = c.profile.clone();
    if flavor.spec().is_some_and(|f| f.no_agent_login.is_some()) {
        no_agent_login(&e, &c);
    } else {
        let (probe, _) = e.core.block_on(live::probe(&c, &p)).unwrap();
        assert_eq!(probe.flavor.major, c.info.major(), "{:?}", probe);
        assert_eq!(probe.lacks.iter().any(|l| l.contains("Redis 7 or later")), c.info.major() < 7, "{:?}", probe.lacks);
        assert!(e.core.block_on(live::has_logins(&c, &p)).unwrap());
    }
    nosql(&e, &c, "DEL dataman_flavor").unwrap();
    Some((e, c))
}

#[test]
fn dragonfly_is_told_by_its_info() {
    redis_like("DATAMAN_DRAGONFLY", Flavor::Dragonfly, "Dragonfly ");
}

#[test]
fn keydb_is_told_by_its_info_section() {
    let Some((_, c)) = redis_like("DATAMAN_KEYDB", Flavor::KeyDb, "KeyDB 6.") else { return };
    assert_eq!(c.engine(), Engine::Redis);
}

#[test]
fn garnet_is_told_by_its_info_and_streams_fail_clean() {
    let Some((e, c)) = redis_like("DATAMAN_GARNET", Flavor::Garnet, "Garnet ") else { return };
    let err = nosql(&e, &c, "XADD dataman_flavor_s * a 1").unwrap_err();
    assert!(err.to_lowercase().contains("unknown command"), "{err}");
    assert!(nosql(&e, &c, "XRANGE dataman_flavor_s - +").unwrap_err().to_lowercase().contains("unknown command"));
}

#[test]
fn ferretdb_is_told_by_its_build_info() {
    let Some((e, c)) = server("DATAMAN_FERRETDB", false) else { return };
    told(&c, Flavor::FerretDb, "FerretDB 2.", 5..=9);
    no_agent_login(&e, &c);
    assert!(c.info.version.contains(" (MongoDB "), "{}", c.info.version);
    nosql(&e, &c, "db.dataman_flavor.drop()").unwrap();
    nosql(&e, &c, "db.dataman_flavor.insertOne({ a: 1, s: 'x' })").unwrap();
    let Output::Docs(d) = nosql(&e, &c, "db.dataman_flavor.find({ a: 1 })").unwrap() else { panic!() };
    assert_eq!(d.docs.len(), 1);
    // It answers `ok: true` to dropIndexes, which the driver takes for an invalid reply.
    nosql(&e, &c, "db.dataman_flavor.createIndex({ a: 1 }, { name: 'dataman_a' })").unwrap();
    nosql(&e, &c, "db.dataman_flavor.dropIndex('dataman_a')").unwrap();
    let Output::Value(Body::Bson(ix)) = nosql(&e, &c, "db.dataman_flavor.getIndexes()").unwrap() else { panic!() };
    assert!(!format!("{ix:?}").contains("dataman_a"), "{ix:?}");
    assert!(nosql(&e, &c, "db.dataman_flavor.dropIndex('dataman_none')").unwrap_err().contains("IndexNotFound"));
    // What it takes and does not apply is refused, not answered without it.
    for text in [
        "db.dataman_flavor.find({ s: 'X' }).collation({ locale: 'en', strength: 2 })",
        "db.dataman_flavor.countDocuments({ s: 'X' }, { collation: { locale: 'en', strength: 2 } })",
        "db.dataman_flavor.aggregate([{ $match: { s: 'X' } }], { collation: { locale: 'en', strength: 2 } })",
        "db.runCommand({ find: 'dataman_flavor', filter: { s: 'X' }, collation: { locale: 'en', strength: 2 } })",
        "db.createView('dataman_flavor_v', 'dataman_flavor', [])",
        "db.createCollection('dataman_flavor_c', { capped: true, size: 4096, max: 1 })",
    ] {
        assert!(nosql(&e, &c, text).unwrap_err().contains("does not apply"), "{text}");
    }
    nosql(&e, &c, "db.dataman_flavor.drop()").unwrap();
}

/// What the shell passes beside a pipeline, a filter or a field reaches the server, which checks
/// it: a collation counts, an unknown option fails.
#[test]
fn mongo_options_reach_the_server() {
    let Some((e, c)) = server("DATAMAN_MONGODB", false) else { return };
    nosql(&e, &c, "db.dataman_opts.drop()").unwrap();
    nosql(&e, &c, "db.dataman_opts.insertMany([{ s: 'x' }, { s: 'y' }])").unwrap();
    const CI: &str = "{ collation: { locale: 'en', strength: 2 } }";
    let Output::Docs(d) = nosql(&e, &c, &format!("db.dataman_opts.aggregate([{{ $match: {{ s: 'X' }} }}], {CI})")).unwrap() else { panic!() };
    assert_eq!(d.docs.len(), 1);
    assert_eq!(nosql(&e, &c, &format!("db.dataman_opts.countDocuments({{ s: 'X' }}, {CI})")).unwrap(), Output::Value(Body::Bson(mongodb::bson::Bson::Int64(1))));
    assert_eq!(nosql(&e, &c, "db.dataman_opts.find({ s: 'X' }).collation({ locale: 'en', strength: 2 }).count()").unwrap(), Output::Value(Body::Bson(mongodb::bson::Bson::Int64(1))));
    let Output::Value(Body::Bson(v)) = nosql(&e, &c, &format!("db.dataman_opts.distinct('s', {{ s: 'X' }}, {CI})")).unwrap() else { panic!() };
    assert_eq!(v, mongodb::bson::Bson::Array(vec!["x".into()]));
    assert_eq!(nosql(&e, &c, "db.dataman_opts.countDocuments({}, { limit: 1 })").unwrap(), Output::Value(Body::Bson(mongodb::bson::Bson::Int64(1))));
    assert!(nosql(&e, &c, "db.dataman_opts.aggregate([], { bogus: 1 })").unwrap_err().contains("bogus"));
    nosql(&e, &c, "db.dataman_opts.drop()").unwrap();
}
