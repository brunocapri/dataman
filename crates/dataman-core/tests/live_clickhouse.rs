//! ClickHouse on the compose service (`docker compose -f e2e/compose.yaml up -d --wait clickhouse`):
//! what the server answers, read back through every layer. Skips without a server.

mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::edit::Editability;
use dataman_core::result::{Cell, ErrorClass, format_cell};
use dataman_core::schema::RelKind;
use dataman_core::value::Kind;
use dataman_core::{ResultSet, Session, guard};

const DB: &str = "dataman_ch";
const SEED: &str = include_str!("../../../seeds/clickhouse.sql");

fn profile(database: &str) -> ConnectionProfile {
    let addr = std::env::var("DATAMAN_CLICKHOUSE").unwrap_or_else(|_| "127.0.0.1:18123".into());
    let (host, port) = addr.rsplit_once(':').unwrap();
    ConnectionProfile {
        host: host.into(),
        port: port.parse().unwrap(),
        user: "default".into(),
        database: database.into(),
        sslmode: SslMode::Disable,
        engine: Engine::ClickHouse,
        ..ConnectionProfile::new("clickhouse")
    }
}

fn password() -> String {
    std::env::var("DATAMAN_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dataman".into())
}

fn open(e: &Env, p: ConnectionProfile, pw: &str) -> Option<Arc<Connection>> {
    match e.core.block_on(e.core.connect(p.clone(), Some(pw.to_string()))) {
        Ok(c) => Some(c),
        Err(err) => {
            eprintln!("skipping: cannot reach {} ({err:#})", p.summary());
            None
        }
    }
}

/// A database of the test's own: the tests run at once, and each drops what it made.
fn seeded(e: &Env, name: &str) -> Option<Arc<Connection>> {
    let c = open(e, profile(""), &password())?;
    let s = c.session(None);
    let d = dataman_core::sql::Dialect::ClickHouse;
    let seed = SEED.replace(DB, name);
    for st in dataman_core::sql::split_in(&seed, d) {
        ok(e, &s, &seed[st.range]);
    }
    open(e, profile(name), &password())
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

fn shown(rs: &ResultSet, r: usize, col: usize) -> Option<String> {
    let snap = rs.snapshot();
    let kind = rs.columns().unwrap()[col].kind;
    format_cell(kind, snap.source_cell(r, col)).map(|s| s.into_owned())
}

#[test]
fn types_come_back_as_their_kinds() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_types") else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT id, user_id, kind, amount, ratio, happened, happened_ms, day, tags, attrs, token, ok, note FROM events ORDER BY id");
    let cols = rs.columns().unwrap();
    let kinds: Vec<Kind> = cols.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [Kind::Numeric, Kind::Int, Kind::Text, Kind::Numeric, Kind::Float, Kind::Timestamp, Kind::Timestamp, Kind::Date, Kind::Other, Kind::Other, Kind::Text, Kind::Bool, Kind::Text]
    );
    assert_eq!(cols[1].type_name, "Nullable(Int32)");
    assert_eq!(rs.rows(), 3);
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some("1"));
    assert_eq!(shown(&rs, 2, 0).as_deref(), Some("18446744073709551615"), "past an i64, exact");
    assert_eq!(shown(&rs, 1, 1), None, "NULL");
    assert_eq!(shown(&rs, 0, 3).as_deref(), Some("12.5"), "as the server writes it");
    assert_eq!(shown(&rs, 2, 4).as_deref(), Some("NaN"));
    assert_eq!(shown(&rs, 0, 5).as_deref(), Some("2024-01-02 03:04:05"));
    assert_eq!(shown(&rs, 0, 6).as_deref(), Some("2024-01-02 03:04:05.123"));
    assert_eq!(shown(&rs, 0, 7).as_deref(), Some("2024-01-02"));
    assert_eq!(shown(&rs, 0, 8).as_deref(), Some("['a','b']"));
    assert_eq!(shown(&rs, 0, 10).as_deref(), Some("00000000-0000-0000-0000-000000000001"));
    assert_eq!(shown(&rs, 0, 11).as_deref(), Some("true"));
    assert_eq!(shown(&rs, 1, 12).as_deref(), Some("tab\there, line\nbreak, back\\slash"), "unescaped whole");
    ok(&e, &s, "DROP DATABASE dataman_ch_types");
}

#[test]
fn cells_are_never_replaced_or_invented() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, r"SELECT unhex('ff00') AS b, 'a\rb' AS cr, '\\N' AS not_null, '' AS empty, toNullable('') AS empty2, CAST(NULL, 'Nullable(String)') AS n, toFixedString('ab', 3) AS f, toDateTime64('2024-01-02 03:04:05.123456789', 9, 'UTC') AS ns, toInt64(-9223372036854775808) AS low");
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some("\\xff00"), "bytes that are no text: hex, never a replacement character");
    assert_eq!(shown(&rs, 0, 1).as_deref(), Some("a\rb"), "a cell's \\r is no failure mark");
    assert_eq!(shown(&rs, 0, 2).as_deref(), Some("\\N"), "the text, not NULL");
    assert_eq!(rs.snapshot().source_cell(0, 3), Cell::Text(""), "empty, not NULL");
    assert_eq!(rs.snapshot().source_cell(0, 4), Cell::Text(""));
    assert_eq!(rs.snapshot().source_cell(0, 5), Cell::Null);
    assert_eq!(shown(&rs, 0, 6).as_deref(), Some("ab\0"));
    assert_eq!(shown(&rs, 0, 7).as_deref(), Some("2024-01-02 03:04:05.123456789"), "nanoseconds kept");
    assert_eq!(shown(&rs, 0, 8).as_deref(), Some("-9223372036854775808"));
}

#[test]
fn a_statement_with_its_own_format_is_shown_by_lines() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT number AS a FROM numbers(2) FORMAT JSONEachRow");
    assert_eq!(rs.columns().unwrap()[0].name, "result");
    assert_eq!((shown(&rs, 0, 0).as_deref(), shown(&rs, 1, 0).as_deref()), (Some("{\"a\":0}"), Some("{\"a\":1}")));
    let rs = ok(&e, &s, "SELECT 1 WHERE 0");
    assert_eq!((rs.columns().unwrap().len(), rs.rows()), (1, 0), "no rows still has its columns");
}

#[test]
fn errors_say_what_the_server_said() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let err = |sql: &str| {
        let rs = run(&e, &s, sql);
        assert_eq!(rs.phase(), Phase::Failed, "{sql}");
        (rs.status().error.unwrap(), rs)
    };
    let (i, _) = err("SELECT é FORM");
    assert_eq!(i.code.as_deref(), Some("62"));
    assert_eq!(i.class(Engine::ClickHouse), ErrorClass::Syntax);
    assert!(i.position.is_some(), "{i:?}");
    let (i, _) = err("SELECT * FROM nope");
    assert_eq!(i.class(Engine::ClickHouse), ErrorClass::Undefined);
    assert!(i.message.contains("nope") && !i.message.contains("(version"), "{}", i.message);
    // Far past the server's first flush (1 MiB): the status was 200 by then, and the rows stay.
    let (i, rs) = err("SELECT number, throwIf(number = 2000000, 'boom') FROM numbers(3000000) SETTINGS max_block_size = 1000, max_threads = 1");
    assert_eq!(i.code.as_deref(), Some("395"), "{i:?}");
    assert!(i.message.contains("boom"), "{}", i.message);
    assert!(rs.rows() > 0 && rs.rows() <= 2_000_000, "what came before the failure: {}", rs.rows());
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some("0"));
}

#[test]
fn a_wrong_password_asks_for_one() {
    let e = env();
    if open(&e, profile(""), &password()).is_none() {
        return;
    }
    let err = e.core.block_on(e.core.connect(profile(""), Some("wrong".into()))).err().expect("refused");
    assert_eq!(dataman_core::backend::server_error_class(&err), Some(ErrorClass::Auth), "{err:#}");
    assert!(dataman_core::backend::needs_password(&err));
}

#[test]
fn nothing_answers_on_a_closed_port() {
    let e = env();
    let p = ConnectionProfile { port: 1, ..profile("") };
    let err = e.core.block_on(e.core.connect(p, Some("x".into()))).err().expect("refused");
    assert!(format!("{err:#}").contains("cannot reach"), "{err:#}");
}

#[test]
fn prefer_falls_back_to_plain_on_a_plain_port() {
    let e = env();
    let Some(c) = open(&e, ConnectionProfile { sslmode: SslMode::Prefer, ..profile("") }, &password()) else { return };
    assert!(!c.tls_in_use());
    assert!(c.info.version.starts_with("ClickHouse "), "{}", c.info.version);
    assert!(c.info.major() >= 20, "{:?}", c.info.release);
    assert_eq!((c.info.database.as_str(), c.info.user.as_str()), ("default", "default"));
}

#[test]
fn tls_is_what_the_mode_asks_for() {
    let e = env();
    let addr = std::env::var("DATAMAN_CLICKHOUSE_TLS").unwrap_or_else(|_| "127.0.0.1:18443".into());
    let (host, port) = addr.rsplit_once(':').unwrap();
    let tls = |mode: SslMode| ConnectionProfile { host: host.into(), port: port.parse().unwrap(), sslmode: mode, ..profile("") };
    let Some(c) = open(&e, tls(SslMode::Require), &password()) else { return };
    assert!(c.tls_in_use(), "require: encrypted, whatever the certificate");
    let s = c.session(None);
    assert_eq!(shown(&ok(&e, &s, "SELECT 1"), 0, 0).as_deref(), Some("1"));
    let c = open(&e, tls(SslMode::Prefer), &password()).expect("prefer takes TLS where there is TLS");
    assert!(c.tls_in_use());
    let err = e.core.block_on(e.core.connect(tls(SslMode::VerifyFull), Some(password()))).err().expect("a certificate nobody vouches for");
    assert!(format!("{err:#}").to_lowercase().contains("certificate"), "{err:#}");
    assert!(e.core.block_on(e.core.connect(tls(SslMode::Disable), Some(password()))).is_err(), "plain HTTP to a TLS port");
}

#[test]
fn a_tab_keeps_its_session() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let (a, b) = (c.session(None), c.session(None));
    ok(&e, &a, "USE system");
    assert_eq!(shown(&ok(&e, &a, "SELECT currentDatabase()"), 0, 0).as_deref(), Some("system"));
    assert_eq!(shown(&ok(&e, &b, "SELECT currentDatabase()"), 0, 0).as_deref(), Some("default"), "another tab is another session");
    ok(&e, &a, "CREATE TEMPORARY TABLE t (a Int8)");
    ok(&e, &a, "INSERT INTO t VALUES (1)");
    assert_eq!(shown(&ok(&e, &a, "SELECT a FROM t"), 0, 0).as_deref(), Some("1"));
}

#[test]
fn an_insert_counts_what_it_wrote() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_insert") else { return };
    let s = c.session(None);
    ok(&e, &s, "CREATE TABLE t (a Int32) ENGINE = MergeTree ORDER BY a");
    let rs = ok(&e, &s, "INSERT INTO t SELECT number FROM numbers(7)");
    assert_eq!(rs.status().affected, Some(7));
    assert_eq!(rs.columns().map(|c| c.len()), Some(0));
    assert_eq!(ok(&e, &s, "CREATE TABLE u (a Int32) ENGINE = Memory").status().affected, None);
    ok(&e, &s, "DROP DATABASE dataman_ch_insert");
}

#[test]
fn a_stop_ends_the_statement_on_the_server_and_frees_the_session() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let probe = c.session(None);
    let tag = uuid_tag();
    // Paused after its first chunk: more rows than memory would hold.
    let rs = s.execute(format!("SELECT number, '{tag}' FROM numbers(100000000000)"));
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), async { while rs.rows() == 0 { tokio::time::sleep(Duration::from_millis(10)).await } }).await.expect("first rows") });
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("stopped") });
    assert!(rs.rows() > 0, "the rows loaded stay");
    assert_eq!(rs.phase(), Phase::Cancelled);
    let left = ok(&e, &probe, &format!("SELECT count() FROM system.processes WHERE query LIKE '%{tag}%' AND query NOT LIKE '%system.processes%'"));
    assert_eq!(shown(&left, 0, 0).as_deref(), Some("0"), "not running on the server");
    assert_eq!(shown(&ok(&e, &s, "SELECT 1"), 0, 0).as_deref(), Some("1"), "the session is free");
}

#[test]
fn a_stop_before_the_first_row_is_a_cancel() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let probe = c.session(None);
    let tag = uuid_tag();
    let rs = s.execute(format!("SELECT count(), '{tag}' FROM numbers_mt(1000000000000000)"));
    let running = format!("SELECT count() FROM system.processes WHERE query LIKE '%{tag}%' AND query NOT LIKE '%system.processes%'");
    let t0 = Instant::now();
    while shown(&ok(&e, &probe, &running), 0, 0).as_deref() != Some("1") {
        assert!(t0.elapsed() < Duration::from_secs(30), "it never started");
    }
    assert_eq!(rs.rows(), 0);
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("stopped") });
    assert_eq!(rs.phase(), Phase::Cancelled, "{:?}", rs.status().error);
    assert_eq!(shown(&ok(&e, &s, "SELECT 1"), 0, 0).as_deref(), Some("1"));
}

fn uuid_tag() -> String {
    format!("t{}", std::process::id()) + &format!("{:?}", std::thread::current().id()).replace(|c: char| !c.is_ascii_alphanumeric(), "")
}

#[test]
fn a_result_larger_than_memory_stays_bounded_while_paused() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let tag = uuid_tag();
    let rs = s.execute(format!("SELECT number, '{tag}' FROM numbers(100000000000)"));
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), async { while rs.phase() != Phase::Paused { tokio::time::sleep(Duration::from_millis(10)).await } }).await.expect("paused") });
    let held = rs.rows();
    assert!(held < 1_000_000, "it stops at what was asked for, not at the end: {held}");
    // The server stops sending once the reader's room is full: its count stands still, at about
    // that room's worth of rows of 10 or 11 bytes.
    let probe = c.session(None);
    let read = || shown(&ok(&e, &probe, &format!("SELECT read_rows FROM system.processes WHERE query LIKE '%{tag}%' AND query NOT LIKE '%system.processes%'")), 0, 0).and_then(|n| n.parse::<u64>().ok()).expect("still running");
    let mut last = read();
    let t0 = Instant::now();
    let stood = loop {
        assert!(t0.elapsed() < Duration::from_secs(20), "the server keeps sending: {last} rows read");
        std::thread::sleep(Duration::from_millis(500));
        let now = read();
        if now == last {
            break now;
        }
        last = now;
    };
    assert!(stood < 20_000_000, "{stood} rows read for a paused result");
    s.cancel();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("stopped") });
}

#[test]
fn parameters_bind_by_name_and_never_reach_the_text() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_params") else { return };
    let s = c.session(None);
    let exec = |sql: &str, params: Vec<Option<String>>| {
        let rs = s.execute_with(sql, params);
        rs.fetch_all();
        e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") });
        rs
    };
    let rs = exec("SELECT kind FROM events WHERE id = {id:UInt64} AND kind = {k:String} AND id = {id:UInt64}", vec![Some("1".into()), Some("click".into())]);
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some("click"));
    // A value that would be SQL, and the characters a parameter reads as escapes.
    let hostile = "x' OR 1=1 --\ttab\nline\\back";
    let rs = exec("SELECT {v:String}", vec![Some(hostile.into())]);
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some(hostile));
    let rs = exec("SELECT {v:Nullable(String)}", vec![None]);
    assert_eq!(shown(&rs, 0, 0), None);
    let rs = exec("SELECT {a:String}", vec![Some("1".into()), Some("2".into())]);
    assert!(rs.status().error.unwrap().message.contains("1 parameters and 2 values"));
    let p = dataman_core::params::detect_in("SELECT {a:String}, {b:Array(UInt8)}, {a:String}", dataman_core::sql::Dialect::ClickHouse).unwrap();
    assert_eq!(p.params.iter().map(|p| (p.key.as_str(), p.index)).collect::<Vec<_>>(), [("a", 1), ("b", 2)]);
    ok(&e, &s, "DROP DATABASE dataman_ch_params");
}

#[test]
fn the_catalog_reads_what_is_there() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_catalog") else { return };
    let (db, snapshot) = e.core.block_on(async {
        let dbs = c.databases().await.unwrap();
        let rels = c.relations("dataman_ch_catalog", "dataman_ch_catalog").await.unwrap();
        (dbs, rels)
    });
    assert!(db.iter().any(|d| d == "dataman_ch_catalog"));
    assert!(db.iter().position(|d| d == "system") > db.iter().position(|d| d == "dataman_ch_catalog"), "system ones last: {db:?}");
    let kind = |n: &str| snapshot.iter().find(|r| r.name == n).map(|r| r.kind);
    assert_eq!(kind("events"), Some(RelKind::Table));
    assert_eq!(kind("clicks"), Some(RelKind::View));
    assert_eq!(kind("rollup_mv"), Some(RelKind::MaterializedView));
    let (cols, idx) = e.core.block_on(async { (c.columns("dataman_ch_catalog", "dataman_ch_catalog", "events").await.unwrap(), c.indexes("dataman_ch_catalog", "dataman_ch_catalog", "events").await.unwrap()) });
    let col = |n: &str| cols.iter().find(|c| c.name == n).unwrap();
    assert!(col("id").primary_key && col("day").primary_key && !col("kind").primary_key);
    assert_eq!((col("user_id").nullable, col("id").nullable), (Some(true), Some(false)));
    assert_eq!(col("note").default.as_deref(), Some("'none'"));
    assert_eq!(col("upper_kind").default.as_deref(), Some("MATERIALIZED upper(kind)"));
    let pk = idx.iter().find(|i| i.primary).unwrap();
    assert_eq!((pk.columns.clone(), pk.unique), (vec!["id".to_string(), "day".to_string()], false), "a sort key, not a unique one");
    assert!(idx.iter().any(|i| i.name == "by_note" && i.definition.as_deref().is_some_and(|d| d.contains("bloom_filter"))), "{idx:?}");
    let (ddl, view, stats, key) = e.core.block_on(async {
        (
            c.table_ddl("dataman_ch_catalog", "dataman_ch_catalog", "events").await.unwrap(),
            c.view_definition("dataman_ch_catalog", "dataman_ch_catalog", "clicks").await.unwrap(),
            c.rel_stats("dataman_ch_catalog", "dataman_ch_catalog").await.unwrap(),
            c.primary_key("dataman_ch_catalog", "dataman_ch_catalog", "events").await.unwrap(),
        )
    });
    assert!(ddl.contains("ENGINE = MergeTree") && ddl.contains("ORDER BY (id, day)"), "{ddl}");
    assert!(view.unwrap().contains("CREATE VIEW"));
    assert_eq!(stats["events"].rows, Some(3.0));
    assert_eq!(key, ["id", "day"]);
    let s = c.session(None);
    ok(&e, &s, "DROP DATABASE dataman_ch_catalog");
}

#[test]
fn rows_are_not_edited_from_the_grid() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_edit") else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT * FROM events");
    match e.core.block_on(c.edit_target("dataman_ch_edit", &rs, None)).unwrap() {
        Editability::ReadOnly(why) => assert!(why.contains("ClickHouse"), "{why}"),
        Editability::Editable(_) => panic!("a sort key names no row"),
    }
    ok(&e, &s, "DROP DATABASE dataman_ch_edit");
}

#[test]
fn a_read_only_database_is_refused_by_the_server_itself() {
    let e = env();
    let Some(admin) = seeded(&e, "dataman_ch_ro") else { return };
    let a = admin.session(None);
    let Some(c) = open(&e, ConnectionProfile { read_only: true, ..profile("dataman_ch_ro") }, &password()) else { return };
    let s = c.session(None);
    assert_eq!(shown(&ok(&e, &s, "SELECT count() FROM events"), 0, 0).as_deref(), Some("3"));
    for sql in ["INSERT INTO events (id) VALUES (9)", "ALTER TABLE events DELETE WHERE 1", "DROP TABLE events", "INSERT INTO events (id) SETTINGS readonly = 0 VALUES (9)"] {
        let rs = run(&e, &s, sql);
        assert_eq!(rs.phase(), Phase::Failed, "{sql}");
        assert_eq!(rs.status().error.unwrap().class(Engine::ClickHouse), ErrorClass::ReadOnly, "{sql}");
    }
    assert_eq!(shown(&ok(&e, &a, "SELECT count() FROM events"), 0, 0).as_deref(), Some("3"), "nothing written");
    // A change of mind in the explorer reaches the tab that is open.
    c.set_read_only_for("dataman_ch_ro", false);
    ok(&e, &s, "CREATE TABLE made (a Int8) ENGINE = Memory");
    ok(&e, &a, "DROP DATABASE dataman_ch_ro");
}

#[test]
fn a_login_that_is_read_only_already_still_connects() {
    let e = env();
    let Some(admin) = open(&e, profile(""), &password()) else { return };
    let a = admin.session(None);
    ok(&e, &a, "DROP USER IF EXISTS dataman_ro");
    ok(&e, &a, "CREATE USER dataman_ro IDENTIFIED WITH plaintext_password BY 'pw' SETTINGS readonly = 1");
    ok(&e, &a, "GRANT SELECT ON *.* TO dataman_ro");
    for read_only in [false, true] {
        let c = open(&e, ConnectionProfile { user: "dataman_ro".into(), read_only, ..profile("") }, "pw").expect("connects");
        let s = c.session(None);
        assert_eq!(shown(&ok(&e, &s, "SELECT 1"), 0, 0).as_deref(), Some("1"));
        let rs = run(&e, &s, "CREATE TABLE default.nope (a Int8) ENGINE = Memory");
        assert_eq!(rs.phase(), Phase::Failed);
        let o = e.core.block_on(c.bounded(None, "SELECT 2 AS two", vec![], Mode::Read, Limits::default(), std::future::pending())).unwrap();
        assert_eq!(o.rows[0][0], serde_json::json!(2));
    }
    ok(&e, &a, "DROP USER dataman_ro");
}

#[test]
fn nothing_pretends_to_roll_back() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_rollback") else { return };
    let s = c.session(None);
    let rs = s.execute_opts("ALTER TABLE events DELETE WHERE 1 SETTINGS mutations_sync = 1", vec![], true);
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") });
    assert_eq!(rs.phase(), Phase::Failed);
    assert!(rs.status().error.unwrap().message.contains("cannot be rolled back"));
    assert_eq!(shown(&ok(&e, &s, "SELECT count() FROM events"), 0, 0).as_deref(), Some("3"), "it did not run");
    let rs = s.execute_opts("SELECT count() FROM events", vec![], true);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") });
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some("3"), "a read has nothing to undo");
    ok(&e, &s, "DROP DATABASE dataman_ch_rollback");
}

#[test]
fn agents_read_on_a_connection_that_cannot_write() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_agent") else { return };
    let read = |sql: &str, params: Vec<Option<String>>| e.core.block_on(c.bounded(None, sql, params, Mode::Read, Limits::default(), std::future::pending()));
    let o = read("SELECT id, kind FROM events ORDER BY id LIMIT 1; SELECT count() AS n FROM events", vec![]).unwrap();
    assert_eq!(o.parts.len(), 2);
    assert_eq!(o.parts[0].rows[0], vec![serde_json::json!("1"), serde_json::json!("click")]);
    assert_eq!(o.parts[1].columns[0].name, "n");
    let o = read("SELECT kind FROM events WHERE id = {id:UInt64}", vec![Some("2".into())]).unwrap();
    assert_eq!(o.rows[0][0], serde_json::json!("view"));
    let o = read("SELECT number FROM numbers(1000000)", vec![]).unwrap();
    assert_eq!((o.rows.len(), o.truncated), (Limits::default().max_rows, Some(dataman_core::bounded::Truncation::Rows)));
    for sql in ["INSERT INTO events (id) VALUES (9)", "SYSTEM SHUTDOWN", "KILL QUERY WHERE 1", "SET max_threads = 1", "ALTER TABLE events DELETE WHERE 1", "SELECT * FROM file('/etc/passwd', 'LineAsString')", "SELECT * FROM url('http://127.0.0.1:1/', CSV, 'a Int8')", "SELECT * FROM remote('127.0.0.1', system.one)", "SELECT 1 INTO OUTFILE 'x'", "SELECT * FROM merge('system', 'one')", "SELECT dictGet('d', 'a', 1)"] {
        assert!(matches!(read(sql, vec![]), Err(Stop::Refused(_))), "{sql}");
    }
    assert!(matches!(e.core.block_on(c.bounded(None, "SELECT sleep(3), sleep(3), sleep(3)", vec![], Mode::Read, Limits { timeout: Duration::from_millis(300), ..Limits::default() }, std::future::pending())), Err(Stop::Timeout(_))));
    // A write that was allowed runs alone, and at once.
    let o = e.core.block_on(c.bounded(None, "INSERT INTO events (id) VALUES (9)", vec![], Mode::Autocommit, Limits::default(), std::future::pending())).unwrap();
    assert!(o.committed);
    assert_eq!(o.affected(), 2, "its row and the one the materialized view wrote");
    // What cannot be held open or rolled back does not run in a mode that promises it.
    for mode in [Mode::Write, Mode::Rollback] {
        let err = e.core.block_on(c.bounded(None, "INSERT INTO events (id) VALUES (10)", vec![], mode, Limits::default(), std::future::pending())).unwrap_err();
        assert!(matches!(err, Stop::Refused(_)), "{err}");
    }
    let s = c.session(None);
    assert_eq!(shown(&ok(&e, &s, "SELECT count() FROM events"), 0, 0).as_deref(), Some("4"));
    ok(&e, &s, "DROP DATABASE dataman_ch_agent");
}

#[test]
fn every_write_waits_for_a_human() {
    let d = dataman_core::sql::Dialect::ClickHouse;
    // Nothing counts the rows of a write before it is done: no write is a safe write here.
    for sql in ["INSERT INTO t VALUES (1)", "DELETE FROM t WHERE id = 1", "UPDATE t SET a = 1 WHERE id = 1", "ALTER TABLE t DELETE WHERE id = 1", "OPTIMIZE TABLE t FINAL", "TRUNCATE TABLE t", "DROP TABLE t", "RENAME TABLE a TO b", "SYSTEM SHUTDOWN", "KILL QUERY WHERE 1", "SET max_threads = 1", "CREATE TABLE t (a Int8) ENGINE = Memory"] {
        assert!(guard::agent_write_class(sql, d).is_destructive(), "{sql}");
        assert_eq!(guard::agent_preview(sql, d), guard::Preview::Outside, "{sql}");
    }
    for sql in ["SELECT 1", "WITH 1 AS x SELECT x", "SHOW TABLES", "DESC t", "EXISTS TABLE t", "EXPLAIN SELECT 1"] {
        assert_eq!(guard::agent_write_class(sql, d), guard::WriteClass::Read, "{sql}");
        assert_eq!(guard::agent_read_refusal_in(sql, d), None, "{sql}");
    }
    // Hidden from a reader of the text, as the server's lexer reads it.
    for sql in [r"SELECT 'a\'' ; DROP TABLE t", "SELECT 1 /* a /* b */ c */ ; DROP TABLE t", "SELECT 1 --x\n; DROP TABLE t"] {
        assert!(guard::agent_read_refusal_in(sql, d).is_some(), "{sql}");
    }
    assert_eq!(guard::agent_read_refusal_in(r"SELECT 'a\'; DROP TABLE t'", d), None, "all of it is a string");
    assert!(guard::file_access("SELECT * FROM S3('http://x/', CSV)", d).is_some());
    assert!(guard::file_access("CREATE TABLE t (a Int8) ENGINE = File(CSV)", d).is_some());
    assert!(guard::file_access("SELECT url FROM t", d).is_none(), "a column named so");
    assert!(guard::file_access("SELECT file('x')", dataman_core::sql::Dialect::Postgres).is_none(), "ClickHouse's names only");
}
