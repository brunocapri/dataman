//! Servers that speak an engine's protocol and are products of their own: each one is told by
//! what it reports, behaves as its engine, and fails clean where it lacks something. Each test
//! reads its server's connection string (as pasted in the connection sheet) from an environment
//! variable and skips itself without one, or when the server does not answer:
//! `DATAMAN_TIMESCALE=postgres://postgres:pw@127.0.0.1:5432/postgres`, `DATAMAN_YUGABYTE`,
//! `DATAMAN_PERCONA`, `DATAMAN_DRAGONFLY`, `DATAMAN_KEYDB`, `DATAMAN_GARNET`, `DATAMAN_FERRETDB`.

mod common;

use common::*;
use dataman_core::agent_login::live;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::nosql::{Body, Output, Page};
use dataman_core::server::Flavor;
use dataman_core::{ConnectionProfile, Engine, ResultSet};

fn server(var: &str, read_only: bool) -> Option<(Env, Arc<Connection>)> {
    let Ok(url) = std::env::var(var) else {
        eprintln!("skipping: {var} is not set");
        return None;
    };
    let (mut p, pw) = dataman_core::connstr::parse_profile(&url).unwrap_or_else(|| panic!("{var} does not read as a connection string"));
    p.read_only = read_only;
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
    // It stops every job of the server's from a read-only transaction: no agent's read runs it.
    let read = |sql: &str| e.core.block_on(c.bounded(None, sql, vec![], Mode::Read, Limits::default(), std::future::pending()));
    for sql in ["SELECT _timescaledb_functions.stop_background_workers()", "SELECT public.add_job('pg_sleep'::regproc, '1h')"] {
        assert!(read(sql).is_err(), "{sql}");
    }
    assert!(matches!(read("SELECT _timescaledb_functions.stop_background_workers()"), Err(Stop::Refused(_))));
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
    let read = |sql: &str| e.core.block_on(c.bounded(None, sql, vec![], Mode::Read, Limits::default(), std::future::pending()));
    assert!(matches!(read("SELECT yb_cancel_transaction('00000000-0000-0000-0000-000000000000')"), Err(Stop::Refused(_))), "it cancels another session's transaction");
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

fn nosql(e: &Env, c: &Arc<Connection>, text: &str) -> Result<Output, String> {
    let db = c.database().to_string();
    e.core.block_on(c.run_nosql(&db, text, Page::default())).map(|r| r.output).map_err(|err| format!("{err:#}"))
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
    let (probe, _) = e.core.block_on(live::probe(&c, &p)).unwrap();
    assert_eq!(probe.flavor.major, c.info.major(), "{:?}", probe);
    assert_eq!(probe.lacks.iter().any(|l| l.contains("Redis 7 or later")), c.info.major() < 7, "{:?}", probe.lacks);
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
    assert!(c.info.version.contains(" (MongoDB "), "{}", c.info.version);
    nosql(&e, &c, "db.dataman_flavor.insertOne({ a: 1 })").unwrap();
    let Output::Docs(d) = nosql(&e, &c, "db.dataman_flavor.find({ a: 1 })").unwrap() else { panic!() };
    assert_eq!(d.docs.len(), 1);
    nosql(&e, &c, "db.dataman_flavor.drop()").unwrap();
}
