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
use dataman_core::sql::{Dialect, functions};
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
            // CI brings the server up for these: a skip there would pass untested.
            assert!(std::env::var_os("DATAMAN_LIVE_REQUIRED").is_none(), "no server: {} ({err:#})", p.summary());
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
        [Kind::Numeric, Kind::Int, Kind::Text, Kind::Numeric, Kind::Float, Kind::Timestamp, Kind::Timestamp, Kind::Date, Kind::Other, Kind::Other, Kind::Other, Kind::Bool, Kind::Text]
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
fn nested_values_stay_the_literals_the_server_wrote() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT ['a'',''b'] AS one, ['a', 'b'] AS two, ('it''s', 1) AS t");
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some(r"['a\',\'b']"), "one element");
    assert_eq!(shown(&rs, 0, 1).as_deref(), Some("['a','b']"), "two elements");
    assert_eq!(shown(&rs, 0, 2).as_deref(), Some(r"('it\'s',1)"));
}

#[test]
fn a_row_longer_than_the_reader_s_room_arrives_whole() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT length(x) AS n, x FROM (SELECT arrayStringConcat(arrayWithConstant(70, repeat('x', 1000000))) AS x)");
    assert_eq!(rs.rows(), 1);
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some("70000000"));
    assert_eq!(shown(&rs, 0, 1).map(|t| t.len()), Some(70_000_000));
}

#[test]
fn empty_cells_are_rows_in_what_the_catalog_reads() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_empty") else { return };
    let s = c.session(None);
    ok(&e, &s, "CREATE TABLE t (v String) ENGINE = MergeTree ORDER BY v");
    ok(&e, &s, "INSERT INTO t VALUES ('a'), (''), ('b')");
    let got = e.core.block_on(c.sample_column_values("dataman_ch_empty", "dataman_ch_empty", "t", "v", 10)).unwrap();
    assert_eq!(got.len(), 3, "{got:?}");
    ok(&e, &s, "DROP DATABASE dataman_ch_empty");
}

#[test]
fn a_failure_is_a_failure_in_every_format() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    for format in ["JSONEachRow", "JSON", "CSV"] {
        let rs = run(&e, &s, &format!("SELECT number, throwIf(number = 2000000, 'boom') AS t FROM numbers(3000000) SETTINGS max_threads = 1, max_block_size = 1000 FORMAT {format}"));
        assert_eq!(rs.phase(), Phase::Failed, "{format}");
        assert!(rs.status().error.unwrap().message.contains("boom"), "{format}");
    }
}

#[test]
fn a_statement_s_own_format_is_not_cut_inside_a_character() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT arrayStringConcat(arrayWithConstant(3, repeat('é', 1000000))) FORMAT CSV");
    assert!(rs.rows() > 1, "past the cap");
    for r in 0..rs.rows() {
        assert!(!shown(&rs, r, 0).unwrap().starts_with("\\x"), "row {r} turned to hex");
    }
}

#[test]
fn line_ends_the_server_was_told_to_change_are_still_line_ends() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT 1 AS a, CAST(NULL, 'Nullable(String)') AS b SETTINGS output_format_tsv_crlf_end_of_line = 1");
    assert_eq!(rs.columns().unwrap()[1].name, "b");
    assert_eq!(rs.snapshot().source_cell(0, 1), Cell::Null);
    let rs = ok(&e, &s, "SELECT '__exception__' AS a SETTINGS output_format_tsv_crlf_end_of_line = 1");
    assert_eq!(shown(&rs, 0, 0).as_deref(), Some("__exception__"), "a value, not a failure");
}

#[test]
fn a_login_that_may_set_nothing_still_sees_a_failure_as_one() {
    let e = env();
    let Some(admin) = open(&e, profile(""), &password()) else { return };
    let a = admin.session(None);
    ok(&e, &a, "DROP USER IF EXISTS dataman_ro_json");
    ok(&e, &a, "CREATE USER dataman_ro_json IDENTIFIED WITH plaintext_password BY 'pw' SETTINGS readonly = 1");
    ok(&e, &a, "GRANT SELECT ON *.* TO dataman_ro_json");
    let c = open(&e, ConnectionProfile { user: "dataman_ro_json".into(), ..profile("") }, "pw").expect("connects");
    let s = c.session(None);
    for format in ["JSONEachRow", "JSON"] {
        let rs = run(&e, &s, &format!("SELECT number, throwIf(number = 2000000, 'boom') AS t FROM numbers(3000000) FORMAT {format}"));
        assert_eq!(rs.phase(), Phase::Failed, "{format}");
        assert!(rs.status().error.unwrap().message.contains("boom"), "{format}");
    }
    assert_eq!(ok(&e, &s, "SELECT 1 AS a FORMAT JSONEachRow").rows(), 1);
    ok(&e, &a, "DROP USER dataman_ro_json");
}

#[test]
fn a_redirect_is_not_followed() {
    use std::io::{Read, Write};
    let e = env();
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let there = target.local_addr().unwrap();
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let answering = std::thread::spawn(move || {
        let (mut c, _) = server.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = c.read(&mut buf);
        write!(c, "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{there}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let p = ConnectionProfile { host: "127.0.0.1".into(), port, ..profile("") };
    let err = e.core.block_on(e.core.connect(p, Some("secret".into()))).err().expect("no ClickHouse there");
    answering.join().unwrap();
    assert!(format!("{err:#}").contains("HTTP 307"), "{err:#}");
    assert!(matches!(target.accept(), Err(x) if x.kind() == std::io::ErrorKind::WouldBlock), "the statement and the password went on");
}

#[test]
fn bool_keeps_the_words_the_server_was_told_to_use() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let rs = ok(&e, &s, "SELECT (number % 2 = 0)::Bool AS b FROM numbers(2) ORDER BY number SETTINGS bool_true_representation = 'yes', bool_false_representation = 'no'");
    assert_eq!((shown(&rs, 0, 0).as_deref(), shown(&rs, 1, 0).as_deref()), (Some("yes"), Some("no")));
    let rs = ok(&e, &s, "SELECT (number % 2 = 0)::Bool AS b FROM numbers(2) ORDER BY number SETTINGS bool_true_representation = '1', bool_false_representation = 'no'");
    assert_eq!((shown(&rs, 0, 0).as_deref(), shown(&rs, 1, 0).as_deref()), (Some("1"), Some("no")), "not `true`");
}

#[test]
fn a_value_shown_as_hex_filters_by_its_bytes() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_hexf") else { return };
    let s = c.session(None);
    ok(&e, &s, "CREATE TABLE t (v String) ENGINE = MergeTree ORDER BY v");
    ok(&e, &s, r"INSERT INTO t SELECT unhex('ff00') UNION ALL SELECT '\\xff00' UNION ALL SELECT 'other'");
    let f = |op| dataman_core::dataview::Filter::new("v", Kind::Text, op, r"\xff00").sql_in(CH);
    let count = |w: String| shown(&ok(&e, &s, &format!("SELECT count() FROM t WHERE {w}")), 0, 0);
    assert_eq!(count(f(dataman_core::dataview::Op::Eq)).as_deref(), Some("2"), "the bytes, and the text that reads the same");
    assert_eq!(count(f(dataman_core::dataview::Op::Ne)).as_deref(), Some("1"));
    ok(&e, &s, "DROP DATABASE dataman_ch_hexf");
}

#[test]
fn final_and_sample_run_inside_a_narrowed_table() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_final") else { return };
    let s = c.session(None);
    ok(&e, &s, "CREATE TABLE users (id UInt64, name String, secret String) ENGINE = ReplacingMergeTree ORDER BY id SAMPLE BY id");
    ok(&e, &s, "INSERT INTO users VALUES (1, 'a', 's')");
    ok(&e, &s, "INSERT INTO users VALUES (1, 'b', 's')");
    // What `agent_sql::check` makes of `SELECT name FROM users FINAL SAMPLE 1`.
    let rs = ok(&e, &s, "SELECT name FROM (SELECT id, name FROM users FINAL SAMPLE 1) users");
    assert_eq!((rs.rows(), shown(&rs, 0, 0).as_deref()), (1, Some("b")));
    ok(&e, &s, "DROP DATABASE dataman_ch_final");
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

const CH: dataman_core::sql::Dialect = dataman_core::sql::Dialect::ClickHouse;

/// The first column of every row, as text.
fn column(e: &Env, s: &Arc<Session>, sql: &str) -> Vec<String> {
    let rs = ok(e, s, sql);
    (0..rs.rows()).map(|r| shown(&rs, r, 0).unwrap_or_default()).collect()
}

/// A database of the test's own: empty, and gone when the test ends if it got that far.
fn own(e: &Env, name: &str) -> Option<(Arc<Connection>, Arc<Session>)> {
    let admin = open(e, profile(""), &password())?;
    let a = admin.session(None);
    ok(e, &a, &format!("DROP DATABASE IF EXISTS {name}"));
    ok(e, &a, &format!("CREATE DATABASE {name}"));
    let c = open(e, profile(name), &password())?;
    let s = c.session(None);
    Some((c, s))
}


#[test]
fn every_table_function_the_server_lists_is_refused_or_known_to_read_nothing() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let listed = column(&e, &s, "SELECT name FROM system.table_functions ORDER BY name");
    assert!(listed.iter().any(|n| n == "file"), "{listed:?}");
    let unknown: Vec<&String> = listed.iter().filter(|n| !functions::CLICKHOUSE_PLAIN_TABLE_FUNCTIONS.contains(&n.as_str()) && guard::file_access(&format!("SELECT * FROM {n}('x')"), CH).is_none()).collect();
    assert!(unknown.is_empty(), "the server has table functions nothing refuses: {unknown:?}. Add them to CLICKHOUSE_FILE, or to the allowlist if they read only their arguments");
    for n in functions::CLICKHOUSE_PLAIN_TABLE_FUNCTIONS {
        assert!(listed.iter().any(|l| l == n), "{n} is gone from the server: drop it from the allowlist");
        assert_eq!(guard::file_access(&format!("SELECT * FROM {n}('x')"), CH), None, "{n}");
    }
}

#[test]
fn every_engine_the_server_lists_is_refused_or_keeps_to_its_own_disks() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let tables = column(&e, &s, "SELECT name FROM system.table_engines ORDER BY name");
    let unknown: Vec<&String> = tables
        .iter()
        .filter(|n| !functions::CLICKHOUSE_PLAIN_TABLE_ENGINES.contains(&n.as_str()) && !n.starts_with("Replicated") && guard::file_access(&format!("CREATE TABLE t (a Int8) ENGINE = {n}('x')"), CH).is_none())
        .collect();
    assert!(unknown.is_empty(), "table engines nothing refuses: {unknown:?}");
    let databases = column(&e, &s, "SELECT name FROM system.database_engines ORDER BY name");
    let unknown: Vec<&String> = databases.iter().filter(|n| !functions::CLICKHOUSE_PLAIN_DATABASE_ENGINES.contains(&n.as_str()) && guard::file_access(&format!("CREATE DATABASE d ENGINE = {n}('x')"), CH).is_none()).collect();
    assert!(unknown.is_empty(), "database engines nothing refuses: {unknown:?}");
    // What no list names yet is refused where it stands, and a word that is no table stays free.
    for sql in [
        "SELECT * FROM aNewFunction('x')",
        "SELECT 1 FROM t, aNewFunction('x')",
        "SELECT 1 FROM t JOIN aNewFunction('x') ON 1",
        "SELECT 1 FROM t JOIN u ON a = b, aNewFunction('x')",
        "SELECT * FROM (aNewFunction('x'))",
        "SELECT * FROM (SELECT 1 FROM aNewFunction('x'))",
        "SELECT * FROM (SELECT 1) AS a, aNewFunction('x')",
        // Named like the clause that would end the list of tables.
        "SELECT * FROM numbers(1) AS values, aNewFunction('x')",
        "SELECT * FROM numbers(1) values, aNewFunction('x')",
        "SELECT * FROM numbers(1) AS where, aNewFunction('x')",
        "SELECT * FROM numbers(1) AS limit, aNewFunction('x')",
        "SELECT * FROM numbers(1) AS settings, aNewFunction('x')",
        "SELECT * FROM numbers(1) AS format, aNewFunction('x')",
        "SELECT * FROM numbers(1) AS into, aNewFunction('x')",
        "SELECT * FROM numbers(1) AS select, aNewFunction('x')",
        "SELECT * FROM numbers(1) AS \"where\", aNewFunction('x')",
        "SELECT (SELECT 1 FROM aNewFunction('x'))",
        "FROM aNewFunction('x') SELECT 1",
        "SELECT * FROM (FROM aNewFunction('x') SELECT 1)",
        "SELECT 1 WHERE a IN (SELECT 1 FROM aNewFunction('x'))",
        "DESCRIBE TABLE aNewFunction('x')",
        "DESC aNewFunction('x')",
        "INSERT INTO FUNCTION aNewFunction('x') SELECT 1",
        "CREATE TABLE t AS aNewFunction('x')",
        "CREATE TABLE t (a Int8) ENGINE = ANewEngine('x')",
        "CREATE DATABASE d ENGINE = ANewEngine",
        "CREATE DICTIONARY d (a Int8) PRIMARY KEY a SOURCE(ANEWSOURCE(HOST 'h')) LAYOUT(FLAT()) LIFETIME(0)",
    ] {
        assert!(guard::file_access(sql, CH).is_some(), "{sql}");
    }
    for sql in [
        "SELECT extract(DAY FROM toDate(x)) FROM t",
        "SELECT trim(BOTH ' ' FROM upper(s)) FROM t",
        "SELECT 1 FROM t ARRAY JOIN arrayMap(x -> x, a) AS b",
        "SELECT 1 FROM t JOIN u ON lower(a) = lower(b)",
        "SELECT 1 FROM t JOIN u ON (lower(a), 1) = (lower(b), 1)",
        "SELECT f(a, g(b)) FROM t WHERE h(a) GROUP BY i(a) ORDER BY j(a)",
        "SELECT 1 FROM numbers(3), zeros(1)",
        "SELECT 1 FROM numbers(3) AS values, zeros(1)",
        "SELECT a, f(b) FROM t ORDER BY a, g(b)",
        "SELECT 1 FROM t LIMIT 1, 2",
        "SELECT name FROM system.tables WHERE engine = other",
        "INSERT INTO t (a) VALUES (1)",
        "CREATE TABLE t (a Int8) ENGINE = ReplicatedMergeTree('/x', 'r') ORDER BY a",
        "CREATE DICTIONARY d (a Int8) PRIMARY KEY a SOURCE(NULL()) LAYOUT(FLAT()) LIFETIME(0)",
    ] {
        assert_eq!(guard::file_access(sql, CH), None, "{sql}");
    }
    // `Filesystem` serves the server's own files, and takes no argument.
    assert!(guard::file_access("CREATE DATABASE d ENGINE = Filesystem", CH).is_some());
    assert!(guard::file_access("CREATE DATABASE d ENGINE=Filesystem", CH).is_some());
    assert!(guard::file_access("CREATE TABLE t (a Int8) ENGINE MergeTree ORDER BY a", CH).is_none());
    assert!(guard::file_access("CREATE TABLE t (a Int8) ENGINE = Memory", CH).is_none());
}

/// With server activity off an agent reads `system` tables known to hold no statement.
#[test]
fn every_system_table_the_server_lists_is_read_for_what_it_holds() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let listed = column(&e, &s, "SELECT name FROM system.tables WHERE database = 'system' ORDER BY name");
    assert!(listed.iter().any(|n| n == "query_cache"), "{listed:?}");
    let unknown: Vec<&String> = listed.iter().filter(|n| !functions::CLICKHOUSE_PLAIN_SYSTEM_TABLES.contains(&n.as_str()) && !functions::CLICKHOUSE_ACTIVITY_SYSTEM_TABLES.contains(&n.as_str())).collect();
    assert!(unknown.is_empty(), "the server has system tables no list reads: {unknown:?}. Add them to CLICKHOUSE_PLAIN_SYSTEM_TABLES if they hold no statement and no value, or to CLICKHOUSE_ACTIVITY_SYSTEM_TABLES");
    // A column that holds a statement or a failure's text says which list a table is not on.
    let telling = column(&e, &s, "SELECT DISTINCT table FROM system.columns WHERE database = 'system' AND name IN ('query', 'exception', 'last_exception', 'last_error_message', 'command', 'error', 'message')");
    let plain: Vec<&String> = telling.iter().filter(|t| functions::CLICKHOUSE_PLAIN_SYSTEM_TABLES.contains(&t.as_str())).collect();
    // Read one by one: definitions, built-in queries, the server's own notices, a load's failure.
    assert!(plain.iter().all(|t| matches!(t.as_str(), "dashboards" | "dictionaries" | "projection_parts" | "projections" | "warnings")), "listed as plain, with a column of statements or failures: {plain:?}");
}

/// Where a table's parts go is not the agent's: a `disk(…)` of its own reaches any endpoint.
#[test]
fn a_disk_of_its_own_is_refused_at_every_tier() {
    for sql in [
        "CREATE TABLE t (a Int8) ENGINE = MergeTree ORDER BY a SETTINGS disk = disk(type = s3, endpoint = 'http://x/', access_key_id = 'a', secret_access_key = 'b')",
        "CREATE TABLE t (a Int8) ENGINE = MergeTree ORDER BY a SETTINGS disk = 'default'",
        "CREATE TABLE t (a Int8) ENGINE = MergeTree ORDER BY a SETTINGS `d\\x69sk` = 'default'",
        "CREATE TABLE t (a Int8) ENGINE = MergeTree ORDER BY a SETTINGS storage_policy = 'default'",
        "CREATE TABLE t (a Int8) ENGINE = MergeTree ORDER BY a SETTINGS index_granularity = 8192, storage_policy = 'default'",
        "ALTER TABLE t MODIFY SETTING storage_policy = 'default'",
        "ATTACH TABLE t (a Int8) ENGINE = MergeTree ORDER BY a SETTINGS disk = 'default'",
        "SELECT disk(type = s3)",
    ] {
        assert!(guard::file_access(sql, CH).is_some(), "{sql}");
    }
    for sql in [
        "CREATE TABLE t (a Int8) ENGINE = MergeTree ORDER BY a SETTINGS index_granularity = 8192",
        "SELECT disk, storage_policy FROM t WHERE disk = 'x'",
        "SELECT disk_name FROM system.parts",
        "CREATE TABLE t ENGINE = Memory AS SELECT disk = 'x' AS d FROM u",
    ] {
        assert_eq!(guard::file_access(sql, CH), None, "{sql}");
    }
}

#[test]
fn every_function_that_reads_past_the_rules_is_refused() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let dict = column(&e, &s, "SELECT name FROM system.functions WHERE lower(name) LIKE 'dict%'");
    assert!(dict.len() > 30, "{dict:?}");
    // Read a table or a file by name, reach another server, or write past a read.
    let others = ["file", "joinGet", "joinGetOrNull", "hasColumnInTable", "catboostEvaluate", "generateSerialID", "logTrace"];
    for f in dict.iter().map(String::as_str).chain(others) {
        assert!(guard::agent_read_refusal_in(&format!("SELECT {f}('x')"), CH).is_some(), "{f}");
        assert!(guard::file_access(&format!("SELECT {f}('x')"), CH).is_some(), "{f}");
    }
    for f in others {
        assert_eq!(column(&e, &s, &format!("SELECT count() FROM system.functions WHERE name = '{f}'")), ["1"], "{f} is not a function of this server");
    }
    // What is named so without being called stays a column.
    assert_eq!(guard::file_access("SELECT file, url, merge, dictionary FROM t", CH), None);
}

#[test]
fn dictionary_sources_that_reach_out_are_refused() {
    let e = env();
    let Some((_c, s)) = own(&e, "dataman_ch_dict") else { return };
    let dictionary = |src: &str| format!("CREATE DICTIONARY dataman_ch_dict.d (a UInt64) PRIMARY KEY a SOURCE({src}(HOST 'h')) LAYOUT(HASHED()) LIFETIME(0)");
    for src in ["FILE", "EXECUTABLE", "EXECUTABLE_POOL", "HTTP", "CLICKHOUSE", "MYSQL", "POSTGRESQL", "MONGODB", "REDIS", "ODBC", "JDBC", "CASSANDRA", "YAMLRegExpTree", "YTSAURUS"] {
        let sql = dictionary(src);
        assert!(guard::file_access(&sql, CH).is_some(), "{src} is not refused");
        // The server does know it: the name in the list is the name it takes.
        let rs = run(&e, &s, &sql);
        let said = rs.status().error.map(|i| i.message).unwrap_or_default();
        assert!(!said.contains("unknown dictionary source"), "{src}: {said}");
        ok(&e, &s, "DROP DICTIONARY IF EXISTS dataman_ch_dict.d");
    }
    let rs = run(&e, &s, &dictionary("NOSUCH"));
    assert!(rs.status().error.unwrap().message.contains("unknown dictionary source"), "the check above can fail");
    ok(&e, &s, "DROP DATABASE dataman_ch_dict");
}

/// Whether the lexer sees a call of `lower`, under the name the server reads.
fn calls_lower(sql: &str) -> bool {
    let toks: Vec<_> = dataman_core::sql::tokenize_in(sql, CH).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    (0..toks.len().saturating_sub(1)).any(|i| {
        use dataman_core::sql::TokenKind::{Ident, Keyword, Punct, QuotedIdent};
        matches!(toks[i].kind, Ident | Keyword | QuotedIdent) && toks[i + 1].kind == Punct && toks[i + 1].text(sql) == "(" && dataman_core::sql::ident_value_in(sql, &toks[i], CH).eq_ignore_ascii_case("lower")
    })
}

/// The server's parse of `text`, or what it said instead.
fn parsed(e: &Env, s: &Arc<Session>, text: &str) -> Result<String, String> {
    let sql = format!("EXPLAIN AST {text}");
    let values = dataman_core::params::detect_in(&sql, CH).map_or(0, |p| p.params.len());
    // `}` fits the Enum the corpus types a parameter with, and any String.
    let rs = s.execute_with(sql.clone(), vec![Some("}".into()); values]);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") });
    if let Some(i) = rs.status().error {
        return Err(i.message);
    }
    Ok((0..rs.rows()).filter_map(|r| shown(&rs, r, 0)).collect::<Vec<_>>().join("\n"))
}

/// A call of `lower` in every place one may hide from a reader of the text. For each the server
/// says what it read, and the lexer must have read the same.
#[test]
fn the_lexer_sees_a_call_exactly_where_the_server_does() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let mut texts: Vec<String> = [
        "SELECT lower('x')",
        // Comments.
        "SELECT 1 -- lower('x')",
        "SELECT 1 --lower('x')",
        "SELECT 1 --\r, lower('x')",
        "SELECT 1 -- a\n, lower('x')",
        "SELECT 1 // lower('x')",
        "SELECT 1 // '\n, lower('x') --'",
        "SELECT 1 +// c\n lower('x')",
        "SELECT 1 # lower('x')",
        "SELECT 1 #! lower('x')",
        "SELECT 1 #!\n, lower('x')",
        "SELECT 1 /* lower('x') */",
        "SELECT 1 /* /* */ lower('x') */",
        "SELECT 1 /* a */, lower('x')",
        "SELECT 1 */*c*/ lower('x')",
        "SELECT lower /* c */ ('x')",
        "SELECT lower -- c\n('x')",
        "SELECT lower // c\n('x')",
        // Strings and quoted names, and their escapes.
        "SELECT 'lower(''x'')'",
        "SELECT 'a\\'', lower('x')",
        "SELECT 'a\\\\', lower('x')",
        "SELECT 'a''', lower('x')",
        "SELECT \"lower('x')\"",
        "SELECT \"a\\\"\", lower('x')",
        "SELECT \"a\"\"\", lower('x')",
        "SELECT `lower('x')`",
        "SELECT `a\\``, lower('x')",
        "SELECT `a```, lower('x')",
        "SELECT x'41', lower('x')",
        "SELECT b'01', lower('x')",
        // The function's own name, quoted and escaped.
        "SELECT `lower`('x')",
        "SELECT \"lower\"('x')",
        "SELECT “lower”('x')",
        "SELECT `lo\\x77er`('x')",
        "SELECT `lo\\Nwer`('x')",
        "SELECT \"lo\\x77er\"('x')",
        "SELECT `lo\\wer`('x')",
        // Heredocs.
        "SELECT $$lower('x')$$",
        "SELECT $t$lower('x')$t$",
        "SELECT $1$lower('x')$1$",
        "SELECT $_$lower('x')$_$",
        "SELECT $t$a$u$lower('x')$t$",
        "SELECT $$ ' $$, lower('x') -- '",
        "SELECT $t$ ' $t$, lower('x') -- '",
        "SELECT $1$ ' $1$, lower('x') -- '",
        "SELECT 1 AS $a$, lower('x')",
        "SELECT 1 AS a$b$, lower('x')",
        "SELECT $a$b, lower('x')",
        // Typographic quotes, closed by their own mark only.
        "SELECT ‘lower('x')’",
        "SELECT ‘ ' ’, lower('x') -- '",
        "SELECT‘'’, lower('x') -- '",
        "SELECT ‘a‘, lower('x')’",
        "SELECT “lower('x')”",
        "SELECT “a”, lower('x')",
        // Parameters.
        "SELECT {x:String}, lower('x')",
        "SELECT { x : String }, lower('x')",
        "SELECT {x:Enum('}' = 1)}, lower('x')",
        "SELECT {x:Enum('}' = 1)}, 'lower(''x'')'",
        "SELECT {x:Enum('}' = 1)}, lower('x'), ''",
        "SELECT {/*}*/x:/*}*/String}, lower('x')",
        "SELECT {x:/*'*/String}, lower('x'), '/*'",
        // Numbers and operators beside it.
        "SELECT 1, lower('x')",
        "SELECT 1e5, .5, 0x1F, lower('x')",
        "SELECT 1 == 2, 1 <=> 2, lower('x')",
        "SELECT a::String, lower('x')",
        "SELECT [1][1], (1, 2).1, lower('x')",
    ]
    .iter()
    .map(|t| t.to_string())
    .collect();
    for space in dataman_core::sql::Dialect::ClickHouse.lex().extra_spaces {
        texts.push(format!("SELECT{space}lower('x')"));
        texts.push(format!("SELECT 1,{space}lower{space}('x')"));
    }
    let (mut read, mut seen) = (0, 0);
    let mut wrong = Vec::new();
    for t in &texts {
        // What the server cannot parse runs nothing, whatever the lexer made of it.
        let Ok(ast) = parsed(&e, &s, t) else { continue };
        read += 1;
        let server = ast.contains("Function lower");
        seen += usize::from(server);
        if calls_lower(t) != server {
            wrong.push(format!("{t:?}: the server {} a call, the lexer {}", if server { "reads" } else { "reads no" }, if server { "no" } else { "one" }));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert!(read >= texts.len() * 9 / 10, "most of these must be statements the server takes: {read} of {}", texts.len());
    assert!(seen >= 20 && read - seen >= 10, "both ways: {seen} of {read}");
}

/// A `;` the server would stop at, the same way: it refuses two statements in a request.
#[test]
fn the_splitter_cuts_exactly_where_the_server_does() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let cases = [
        "SELECT 1; SELECT 2",
        "SELECT 1;",
        "SELECT 1 ; ; ",
        "SELECT ';'",
        "SELECT 'a\\'; SELECT 2'",
        "SELECT 'a\\\\'; SELECT 2",
        "SELECT \";\"",
        "SELECT `;`",
        "SELECT `a\\`;`",
        "SELECT 1 -- ;\n",
        "SELECT 1 // ;\n",
        "SELECT 1 # ;\n",
        "SELECT 1 #! ;\n",
        "SELECT 1 /* ; */",
        "SELECT 1 /* /* ; */ ; */",
        "SELECT 1 // x\n; SELECT 2",
        "SELECT $$;$$",
        "SELECT $t$;$t$",
        "SELECT $1$;$1$",
        "SELECT $1$;$1$; SELECT 2",
        "SELECT 1 AS $a$; SELECT 2",
        "SELECT ‘;’",
        "SELECT ‘;’; SELECT 2",
        "SELECT “;”",
        "SELECT 1;\u{a0}SELECT 2",
        "SELECT 1\u{2003};\u{feff}SELECT 2",
        "SELECT {x:Enum(';' = 1)}",
        "SELECT {x:Enum('}' = 1)}; SELECT 2",
    ];
    for t in cases {
        let two = matches!(parsed(&e, &s, t), Err(m) if m.contains("Multi-statements"));
        let n = dataman_core::sql::split_in(t, CH).into_iter().filter(|st| dataman_core::sql::tokenize_in(&t[st.range.clone()], CH).iter().any(|k| !k.kind.is_trivia() && k.text(&t[st.range.clone()]) != ";")).count();
        assert_eq!(n > 1, two, "{t:?}: {n} statements here");
    }
}

/// Each of these hides a call of `file` from a plainer lexer, and each reaches the server's file
/// when it is run (the error names the file). Nothing here gets that far.
#[test]
fn what_hides_a_file_read_from_a_reader_is_refused_at_every_tier() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_hide") else { return };
    let s = c.session(None);
    for sql in [
        "SELECT 1 // '\n, file('nope') --'",
        "SELECT * FROM\u{a0}file('nope', 'CSV', 'a Int8')",
        "SELECT * FROM\u{feff}file('nope', 'CSV', 'a Int8')",
        "SELECT * FROM “file”('nope', 'CSV', 'a Int8')",
        "SELECT ‘ ' ’, file('nope') -- '",
        "SELECT‘'’, file('nope') -- '",
        "SELECT * FROM `fi\\x6Ce`('nope', 'CSV', 'a Int8')",
        "SELECT * FROM \"fi\\x6Ce\"('nope', 'CSV', 'a Int8')",
        "SELECT `fi\\Nle`('nope')",
        "SELECT 1 AS $a$, file('nope')",
        "SELECT $1$ ' $1$, file('nope') -- '",
    ] {
        let rs = run(&e, &s, sql);
        let said = rs.status().error.map(|i| i.message).unwrap_or_default();
        assert!(said.contains("nope") || said.to_lowercase().contains("file"), "the server does read it, or this proves nothing: {sql:?}: {said}");
        let read = e.core.block_on(c.bounded(None, sql, vec![], Mode::Read, Limits::default(), std::future::pending()));
        assert!(matches!(&read, Err(Stop::Refused(r)) if r.contains("file")), "{sql:?}: {read:?}");
        let allowed = e.core.block_on(c.bounded(None, sql, vec![], Mode::Autocommit, Limits::default(), std::future::pending()));
        assert!(matches!(allowed, Err(Stop::Refused(_))), "{sql:?} with Allow");
        assert!(guard::agent_write_class(sql, CH).is_destructive(), "{sql:?}");
    }
    // Hides the same in a parameter's type: the server takes it once the parameter is bound.
    let sql = "SELECT {x:Enum('}' = 1)}, file('nope'), ''";
    let rs = s.execute_with(sql, vec![Some("}".into())]);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") });
    assert!(rs.status().error.unwrap().message.contains("nope"));
    assert!(matches!(e.core.block_on(c.bounded(None, sql, vec![Some("}".into())], Mode::Read, Limits::default(), std::future::pending())), Err(Stop::Refused(_))));
    ok(&e, &s, "DROP DATABASE dataman_ch_hide");
}

#[test]
fn parallel_with_is_each_of_its_statements() {
    let e = env();
    let Some((c, s)) = own(&e, "dataman_ch_parallel") else { return };
    ok(&e, &s, "CREATE TABLE a (x Int8) ENGINE = Memory");
    ok(&e, &s, "CREATE TABLE b (x Int8) ENGINE = Memory");
    // What the classifier has to know: the server refuses the read in this form, and the two race,
    // so the drop may have run by then. Whether it did this time depends on the race.
    let rs = run(&e, &s, "SELECT 1 PARALLEL WITH DROP TABLE b");
    assert_eq!(rs.phase(), Phase::Failed);
    assert!(rs.status().error.unwrap().message.contains("PARALLEL WITH"));
    let sql = "SELECT 1 PARALLEL WITH DROP TABLE a";
    assert_eq!(guard::classify_in(sql, CH).effect, guard::Effect::Ddl);
    assert!(guard::agent_write_class(sql, CH).is_destructive());
    assert!(guard::read_only_refusal_in(sql, CH).is_some());
    assert!(matches!(e.core.block_on(c.bounded(None, sql, vec![], Mode::Read, Limits::default(), std::future::pending())), Err(Stop::Refused(_))));
    assert_eq!(column(&e, &s, "EXISTS TABLE a"), ["1"], "the read tier did not run it");
    // Past the first statement the refusals still hold.
    for sql in ["DROP TABLE IF EXISTS nope PARALLEL WITH DETACH TABLE a", "DROP TABLE IF EXISTS nope PARALLEL WITH ATTACH TABLE a", "DROP TABLE IF EXISTS nope PARALLEL WITH BACKUP TABLE a TO Disk('b', 'x')"] {
        assert!(guard::file_access(sql, CH).is_some(), "{sql}");
    }
    // Not the words of another clause.
    let plain = "SELECT parallel FROM (SELECT 1 AS parallel) GROUP BY parallel WITH TOTALS";
    assert_eq!(guard::agent_read_refusal_in(plain, CH), None);
    assert!(ok(&e, &s, plain).rows() >= 1);
    ok(&e, &s, "DROP DATABASE dataman_ch_parallel");
}

/// What the server holds a `readonly = 1` connection to, statement by statement, and where that
/// ends: there the guard is all there is.
#[test]
fn the_server_s_read_only_stops_the_writes_and_the_guard_the_rest() {
    let e = env();
    let Some(admin) = seeded(&e, "dataman_ch_held") else { return };
    let a = admin.session(None);
    ok(&e, &a, "CREATE TABLE gone (x Int8) ENGINE = MergeTree ORDER BY x");
    ok(&e, &a, "DROP TABLE gone");
    let Some(c) = open(&e, ConnectionProfile { read_only: true, ..profile("dataman_ch_held") }, &password()) else { return };
    let s = c.session(None);
    for sql in [
        "INSERT INTO events (id) VALUES (9)",
        "DELETE FROM events WHERE id = 1",
        "UPDATE events SET note = 'x' WHERE id = 1",
        "ALTER TABLE events DELETE WHERE 1",
        "ALTER TABLE events ADD COLUMN x Int8",
        "TRUNCATE TABLE events",
        "OPTIMIZE TABLE events FINAL",
        "DROP TABLE events",
        "RENAME TABLE events TO e2",
        "EXCHANGE TABLES events AND clicks",
        "DETACH TABLE events",
        "CREATE TABLE t (a Int8) ENGINE = Memory",
        "CREATE DATABASE dataman_ch_held2",
        "CREATE FUNCTION dataman_ch_held_f AS x -> x",
        "GRANT SELECT ON events TO default",
        "SYSTEM FLUSH LOGS",
        "SET readonly = 0",
        "SELECT 1 SETTINGS readonly = 0",
        // At `readonly = 2` the server ran these: the first one wrote the file.
        "INSERT INTO FUNCTION file('dataman_ch_held.csv', 'CSV', 'a Int8') SELECT 1",
        "INSERT INTO FUNCTION null('a Int8') SELECT 1",
        "SET max_threads = 1",
        "SELECT 1 SETTINGS max_threads = 1",
    ] {
        let rs = run(&e, &s, sql);
        assert_eq!(rs.phase(), Phase::Failed, "{sql}");
        assert_eq!(rs.status().error.unwrap().class(Engine::ClickHouse), ErrorClass::ReadOnly, "{sql}");
    }
    assert_eq!(column(&e, &a, "SELECT count() FROM events"), ["3"]);
    // These reach the server's own actions on a read-only connection: the app stops them first.
    for sql in ["UNDROP TABLE gone", "KILL QUERY WHERE query_id = 'nope' SYNC"] {
        assert!(guard::read_only_refusal_in(sql, CH).is_some(), "{sql} is not refused on a read-only tab");
        assert!(guard::agent_read_refusal_in(sql, CH).is_some(), "{sql}");
    }
    assert_eq!(ok(&e, &s, "UNDROP TABLE gone").phase(), Phase::Done, "the server takes it: if this ever fails, the guard's list above can shrink");
    assert_eq!(column(&e, &a, "EXISTS TABLE gone"), ["1"], "a read-only connection brought a table back");
    // What a read-only tab may still do: nothing that outlives the session.
    for sql in ["USE system", "USE dataman_ch_held", "SHOW TABLES", "EXISTS TABLE events", "DESCRIBE TABLE events", "EXPLAIN SELECT 1"] {
        assert_eq!(guard::read_only_refusal_in(sql, CH), None, "{sql}");
        ok(&e, &s, sql);
    }
    // An agent's read names the setting on every request.
    let o = e.core.block_on(c.bounded(None, "SELECT getSetting('readonly')", vec![], Mode::Read, Limits::default(), std::future::pending())).unwrap();
    assert_eq!(o.rows[0][0], serde_json::json!(1));
    ok(&e, &a, "DROP DATABASE dataman_ch_held");
}

#[test]
fn every_statement_is_classified_as_what_it_does() {
    let reads = [
        "SELECT 1",
        "(SELECT 1)",
        "(SELECT 1) UNION ALL (SELECT 2)",
        "SELECT 1 UNION ALL SELECT 2",
        "WITH 1 AS x SELECT x",
        "WITH (SELECT 1) AS x SELECT x",
        "SHOW TABLES",
        "SHOW DATABASES",
        "SHOW CREATE TABLE t",
        "SHOW PROCESSLIST",
        "SHOW GRANTS",
        "SHOW SETTINGS LIKE 'x'",
        "DESC t",
        "DESCRIBE TABLE t",
        "DESCRIBE (SELECT 1)",
        "EXISTS t",
        "EXISTS TABLE t",
        "EXISTS DATABASE d",
        "EXPLAIN SELECT 1",
        "EXPLAIN AST SELECT 1",
        "EXPLAIN AST DROP TABLE t",
        "EXPLAIN SYNTAX SELECT 1",
        "EXPLAIN PIPELINE SELECT 1",
        "EXPLAIN PLAN indexes = 1 SELECT 1",
        "EXPLAIN QUERY TREE SELECT 1",
        "EXPLAIN ESTIMATE SELECT 1",
        "SELECT * FROM numbers(3)",
        "SELECT * FROM null('a Int8')",
        "SELECT 1 SETTINGS max_threads = 1",
        "SELECT 1 FORMAT JSON",
    ];
    for sql in reads {
        assert_eq!(guard::agent_read_refusal_in(sql, CH), None, "{sql}");
        assert_eq!(guard::agent_write_class(sql, CH), guard::WriteClass::Read, "{sql}");
    }
    let writes = [
        "INSERT INTO t VALUES (1)",
        "INSERT INTO TABLE t VALUES (1)",
        "INSERT INTO t WITH 1 AS x SELECT x",
        "WITH 1 AS x INSERT INTO t SELECT x",
        "INSERT INTO FUNCTION null('a Int8') SELECT 1",
        "DELETE FROM t WHERE id = 1",
        "UPDATE t SET a = 1 WHERE id = 1",
        "TRUNCATE TABLE t",
        "TRUNCATE t",
        "TRUNCATE DATABASE d",
        "TRUNCATE ALL TABLES FROM d",
        "CREATE TABLE t (a Int8) ENGINE = Memory",
        "CREATE TEMPORARY TABLE t (a Int8)",
        "CREATE OR REPLACE TABLE t (a Int8) ENGINE = Memory",
        "REPLACE TABLE t (a Int8) ENGINE = Memory",
        "CREATE VIEW v AS SELECT 1",
        "CREATE MATERIALIZED VIEW v ENGINE = Memory AS SELECT 1",
        "CREATE LIVE VIEW v AS SELECT 1",
        "CREATE WINDOW VIEW v AS SELECT 1",
        "CREATE DATABASE d",
        "CREATE FUNCTION f AS x -> x",
        "CREATE USER u",
        "CREATE ROLE r",
        "CREATE ROW POLICY p ON t USING 1 TO ALL",
        "CREATE QUOTA q",
        "CREATE SETTINGS PROFILE p",
        "CREATE NAMED COLLECTION n AS a = 'b'",
        "CREATE DICTIONARY d (a Int8) PRIMARY KEY a SOURCE(NULL()) LAYOUT(FLAT()) LIFETIME(0)",
        "DROP TABLE t",
        "DROP DATABASE d",
        "DROP DICTIONARY d",
        "DROP VIEW v",
        "DROP FUNCTION f",
        "DROP USER u",
        "DROP ROLE r",
        "DROP QUOTA q",
        "DROP SETTINGS PROFILE p",
        "DROP ROW POLICY p ON t",
        "DROP NAMED COLLECTION n",
        "ALTER TABLE t ADD COLUMN a Int8",
        "ALTER TABLE t DELETE WHERE 1",
        "ALTER TABLE t UPDATE a = 1 WHERE 1",
        "ALTER TABLE t FREEZE",
        "ALTER TABLE t FETCH PARTITION 1 FROM '/z'",
        "ALTER TABLE t ATTACH PARTITION 1 FROM u",
        "ALTER TABLE t MOVE PARTITION 1 TO DISK 'd'",
        "ALTER USER u",
        "ALTER NAMED COLLECTION n SET a = 'c'",
        "ALTER DATABASE d MODIFY COMMENT 'x'",
        "OPTIMIZE TABLE t FINAL",
        "RENAME TABLE a TO b",
        "RENAME DATABASE a TO b",
        "EXCHANGE TABLES a AND b",
        "EXCHANGE DICTIONARIES a AND b",
        "MOVE USER u TO disk",
        "UNDROP TABLE t",
        "ATTACH TABLE t",
        "DETACH TABLE t",
        "BACKUP TABLE t TO Disk('b', 'x')",
        "RESTORE TABLE t FROM Disk('b', 'x')",
        "SYSTEM FLUSH LOGS",
        "SYSTEM RELOAD CONFIG",
        "SYSTEM SHUTDOWN",
        "KILL QUERY WHERE 1",
        "KILL MUTATION WHERE 1",
        "SET max_threads = 1",
        "SET ROLE r",
        "SET DEFAULT ROLE r TO u",
        "USE d",
        "GRANT SELECT ON t TO u",
        "REVOKE SELECT ON t FROM u",
        "BEGIN TRANSACTION",
        "COMMIT",
        "ROLLBACK",
        "CHECK TABLE t",
        "CHECK ALL TABLES",
        "WATCH v",
        "EXECUTE AS u SELECT 1",
        "DROP TABLE a PARALLEL WITH DROP TABLE b",
        "SELECT 1 PARALLEL WITH DROP TABLE t",
    ];
    for sql in writes {
        assert!(guard::agent_read_refusal_in(sql, CH).is_some(), "{sql} reads");
        assert!(guard::agent_write_class(sql, CH).is_destructive(), "{sql}: {:?}", guard::agent_write_class(sql, CH));
        // Runs alone: nothing holds it back, so no mode that promises to may run it.
        for mode in [Mode::Write, Mode::Rollback] {
            assert!(dataman_core::bounded::refusal_in(&[sql.to_string()], 0, mode, false, CH).is_some(), "{sql} in {mode:?}");
        }
    }
    assert!(dataman_core::bounded::refusal_in(&["INSERT INTO t VALUES (1)".into(), "SELECT 1".into()], 0, Mode::Autocommit, false, CH).is_some(), "a write is alone in its call");
    // Refused at every tier, even with Allow.
    for sql in ["ATTACH TABLE t", "DETACH TABLE t", "BACKUP TABLE t TO Disk('b', 'x')", "RESTORE TABLE t FROM Disk('b', 'x')", "SELECT 1 INTO OUTFILE 'x'", "(SELECT 1) INTO OUTFILE 'x'", "INSERT INTO t FROM INFILE 'x' FORMAT CSV", "INSERT INTO FUNCTION file('x') SELECT 1", "SELECT COLUMNS('a') FROM t", "SELECT t.COLUMNS('a') FROM t"] {
        assert!(guard::file_access(sql, CH).is_some(), "{sql}");
    }
}

#[test]
fn types_show_their_values() {
    let e = env();
    let Some((_c, s)) = own(&e, "dataman_ch_kinds") else { return };
    ok(
        &e,
        &s,
        "CREATE TABLE t (
            id UInt8, v Variant(String, UInt64, Array(String)), d Dynamic, j JSON, n Nested(x Int8, y String), ip6 IPv6, ip4 IPv4, en Enum8('a b' = 1, 'c''d' = 2),
            d32 Date32, dt64 DateTime64(3, 'UTC'), dt DateTime('UTC'), m Map(String, String), tu Tuple(a Int8, b String), f FixedString(4), lc LowCardinality(Nullable(String)),
            dec Decimal(38, 10), i128 Int128, u256 UInt256, sa SimpleAggregateFunction(sum, UInt64), arr Array(Nullable(String)), p Point
        ) ENGINE = MergeTree ORDER BY id",
    );
    ok(
        &e,
        &s,
        r#"INSERT INTO t (id, v, d, j, `n.x`, `n.y`, ip6, ip4, en, d32, dt64, dt, m, tu, f, lc, dec, i128, u256, sa, arr, p) VALUES
            (1, 'tab\there, it''s', 'tab\there, it''s', '{"a":"q","b":{"c":[1,2]}}', [1, 2], ['p', 'q'], '::1', '1.2.3.4', 'a b', '1900-01-01', '1900-01-01 00:00:00.000', '1970-01-01 00:00:00', map('k', 'v'), (1, 's'), 'ab', NULL, 1.5, -170141183460469231731687303715884105728, 115792089237316195423570985008687907853269984665640564039457584007913129639935, 3, ['a', NULL], (1, 2)),
            (2, ['a''b', 'c'], 42, '{}', [], [], '2001:db8::1', '255.255.255.255', 'c''d', '2299-12-31', '2299-12-31 23:59:59.999', '2106-02-07 06:28:15', map(), (0, ''), 'abcd', 'z', 0, 0, 0, 0, [], (0, 0)),
            (3, NULL, NULL, '{}', [], [], '::', '0.0.0.0', 'a b', '1970-01-01', '1969-12-31 23:59:59.999', '2024-01-01 00:00:00', map(), (0, '\\N'), '', '', 0, 0, 0, 0, [NULL], (0, 0))"#,
    );
    let rs = ok(&e, &s, "SELECT * FROM t ORDER BY id");
    let cols = rs.columns().unwrap();
    let at = |name: &str| cols.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("{name}"));
    let cell = |r: usize, name: &str| shown(&rs, r, at(name));
    let some = |s: &str| Some(s.to_string());
    // A scalar in a Variant or a Dynamic is the value, not its escapes.
    assert_eq!(cell(0, "v"), some("tab\there, it's"));
    assert_eq!(cell(0, "d"), some("tab\there, it's"));
    assert_eq!(cell(1, "d"), some("42"));
    assert_eq!((cell(2, "v"), cell(2, "d")), (None, None), "NULL");
    // Nested in one stays the literal the server wrote.
    assert_eq!(cell(1, "v"), some(r"['a\'b','c']"));
    assert_eq!(cell(0, "j"), some(r#"{"a":"q","b":{"c":[1,2]}}"#));
    assert!(serde_json::from_str::<serde_json::Value>(&cell(0, "j").unwrap()).is_ok());
    assert_eq!(cols[at("j")].kind, Kind::Json);
    // Nested comes back as parallel arrays.
    assert_eq!((cell(0, "n.x"), cell(0, "n.y")), (some("[1,2]"), some("['p','q']")));
    assert_eq!((cell(0, "ip6"), cell(0, "ip4"), cell(1, "ip6")), (some("::1"), some("1.2.3.4"), some("2001:db8::1")));
    assert_eq!((cell(0, "en"), cell(1, "en")), (some("a b"), some("c'd")));
    // The limits of the wider dates.
    assert_eq!((cell(0, "d32"), cell(1, "d32")), (some("1900-01-01"), some("2299-12-31")));
    assert_eq!((cell(0, "dt64"), cell(1, "dt64"), cell(2, "dt64")), (some("1900-01-01 00:00:00"), some("2299-12-31 23:59:59.999"), some("1969-12-31 23:59:59.999")));
    assert_eq!((cell(0, "dt"), cell(1, "dt")), (some("1970-01-01 00:00:00"), some("2106-02-07 06:28:15")));
    assert_eq!((cell(0, "m"), cell(0, "tu"), cell(0, "arr"), cell(0, "p")), (some("{'k':'v'}"), some("(1,'s')"), some("['a',NULL]"), some("(1,2)")));
    assert_eq!(cell(2, "tu"), some(r"(0,'\\N')"), "the text, not NULL");
    assert_eq!((cell(0, "lc"), cell(2, "lc")), (None, some("")));
    assert_eq!(rs.snapshot().source_cell(0, at("lc")), Cell::Null);
    assert_eq!((cell(0, "i128"), cell(0, "u256")), (some("-170141183460469231731687303715884105728"), some("115792089237316195423570985008687907853269984665640564039457584007913129639935")), "exact");
    assert_eq!((cell(0, "dec"), cell(0, "sa")), (some("1.5"), some("3")));
    assert_eq!(cell(0, "f"), some("ab\0\0"));
    assert_eq!(cols[at("v")].type_name, "Variant(Array(String), String, UInt64)");
    // Odd column names, from the header line.
    let rs = ok(&e, &s, r#"SELECT 1 AS `a b`, 2 AS `c.d`, 3 AS `e\tf`, 4 AS `g'h`, 5 AS `select`, 6 AS `é`, 7 AS `\\N`, 8 AS ````"#);
    let names: Vec<String> = rs.columns().unwrap().iter().map(|c| c.name.clone()).collect();
    assert_eq!(names, ["a b", "c.d", "e\tf", "g'h", "select", "é", "\\N", "`"]);
    assert_eq!(shown(&rs, 0, 6).as_deref(), Some("7"));
    ok(&e, &s, "DROP DATABASE dataman_ch_kinds");
}

#[test]
fn the_grid_s_filters_find_what_they_say() {
    let e = env();
    let Some((_c, s)) = own(&e, "dataman_ch_filters") else { return };
    ok(
        &e,
        &s,
        "CREATE TABLE t (
            id UInt8, name String, nul Nullable(String), lc LowCardinality(String), f FixedString(3), u UUID, ip4 IPv4, ip6 IPv6, en Enum8('red' = 1, 'blue' = 2),
            d Date, d32 Date32, dt DateTime('UTC'), dt64 DateTime64(3, 'UTC'), big Int128, huge UInt256, u64 UInt64, dec Decimal(38, 10), fl Float64, ok Bool,
            arr Array(String), m Map(String, Int8), tu Tuple(Int8, String), v Variant(String, UInt8), j JSON
        ) ENGINE = MergeTree ORDER BY id",
    );
    ok(
        &e,
        &s,
        r#"INSERT INTO t VALUES
            (1, '100%', 'x', 'red', 'abc', '00000000-0000-0000-0000-000000000001', '10.0.0.1', '::1', 'red', '2024-01-02', '1900-01-01', '2024-01-02 03:04:05', '1900-01-01 00:00:00.500', 170141183460469231731687303715884105727, 115792089237316195423570985008687907853269984665640564039457584007913129639935, 18446744073709551615, 1234567890123456789012345678.1234567891, 1.5, true, ['a', 'b'], map('k', 1), (1, 's'), 'hello', '{"a":"needle"}'),
            (2, '100x', NULL, 'blue', 'xyz', '00000000-0000-0000-0000-000000000002', '10.0.0.2', '2001:db8::1', 'blue', '2024-01-03', '2299-12-31', '2024-01-03 00:00:00', '2299-12-31 23:59:59.999', 170141183460469231731687303715884105726, 1, 18446744073709551614, 1234567890123456789012345678.1234567892, 2.5, false, [], map(), (2, 't'), 7, '{"a":"hay"}'),
            (3, 'a_b', 'y', 'red', 'abc', '00000000-0000-0000-0000-000000000003', '10.0.0.3', '::2', 'red', '2024-01-04', '1970-01-01', '2024-01-04 00:00:00', '2024-01-04 00:00:00.000', 0, 0, 0, 0, 0, true, [], map(), (3, 'u'), NULL, '{}'),
            (4, 'axb', 'z', 'red', 'abc', '00000000-0000-0000-0000-000000000004', '10.0.0.4', '::3', 'red', '2024-01-05', '1970-01-02', '2024-01-05 00:00:00', '2024-01-05 00:00:00.000', 0, 0, 0, 0, 0, true, [], map(), (4, 'v'), NULL, '{}'),
            (5, 'c\\d', 'é', 'red', 'abc', '00000000-0000-0000-0000-000000000005', '10.0.0.5', '::4', 'red', '2024-01-06', '1970-01-03', '2024-01-06 00:00:00', '2024-01-06 00:00:00.000', 0, 0, 0, 0, 0, true, [], map(), (5, 'w'), NULL, '{}'),
            (6, 'it''s', '', 'red', 'abc', '00000000-0000-0000-0000-000000000006', '10.0.0.6', '::5', 'red', '2024-01-07', '1970-01-04', '2024-01-07 00:00:00', '2024-01-07 00:00:00.000', 0, 0, 0, 0, 0, true, [], map(), (6, 'x'), NULL, '{}'),
            (7, 'tab	here', 'CaSe', 'red', 'abc', '00000000-0000-0000-0000-000000000007', '10.0.0.7', '::6', 'red', '2024-01-08', '1970-01-05', '2024-01-08 00:00:00', '2024-01-08 00:00:00.000', 0, 0, 0, 0, 0, true, [], map(), (7, 'y'), NULL, '{}')"#,
    );
    use dataman_core::dataview::{Filter, Op, TableView};
    // As the grid builds it: the kind is what the server's column gave.
    let filter = |column: &str, op: Op, value: &str| -> Vec<String> {
        let kinds = ok(&e, &s, &format!("SELECT `{column}` FROM t LIMIT 0")).columns().unwrap();
        let mut view = TableView { dialect: CH, ..TableView::new("dataman_ch_filters", "t", vec!["id".into()], 100) };
        view.filters.push(Filter::new(column, kinds[0].kind, op, value));
        let sql = view.sql();
        let rs = run(&e, &s, &sql);
        assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
        let ids: Vec<String> = (0..rs.rows()).map(|r| shown(&rs, r, 0).unwrap()).collect();
        // The count the grid shows agrees.
        assert_eq!(column_count(&e, &s, &view.count_sql()), ids.len(), "{}", view.count_sql());
        ids
    };
    fn column_count(e: &Env, s: &Arc<Session>, sql: &str) -> usize {
        shown(&ok(e, s, sql), 0, 0).unwrap().parse().unwrap()
    }
    let ids = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // Text, and the characters `LIKE` and a literal read specially.
    assert_eq!(filter("name", Op::Contains, "%"), ids(&["1"]), "a percent sign, not any text");
    assert_eq!(filter("name", Op::Contains, "_"), ids(&["3"]), "an underscore, not any character");
    assert_eq!(filter("name", Op::Contains, "\\"), ids(&["5"]));
    assert_eq!(filter("name", Op::Eq, "c\\d"), ids(&["5"]));
    assert_eq!(filter("name", Op::Contains, "'"), ids(&["6"]));
    assert_eq!(filter("name", Op::Eq, "it's"), ids(&["6"]));
    assert_eq!(filter("name", Op::Contains, "\t"), ids(&["7"]));
    assert_eq!(filter("name", Op::StartsWith, "100"), ids(&["1", "2"]));
    assert_eq!(filter("nul", Op::Contains, "case"), ids(&["7"]), "ignoring case");
    assert_eq!(filter("nul", Op::Contains, "É"), ids(&["5"]));
    assert_eq!(filter("nul", Op::IsNull, ""), ids(&["2"]));
    assert_eq!(filter("nul", Op::Eq, "null"), ids(&["2"]));
    assert_eq!(filter("nul", Op::Eq, ""), ids(&["6"]), "empty, not NULL");
    assert_eq!(filter("nul", Op::NotNull, "").len(), 6);
    assert_eq!(filter("nul", Op::In, "x, y"), ids(&["1", "3"]));
    assert_eq!(filter("nul", Op::In, "x, null"), ids(&["1", "2"]));
    assert_eq!(filter("nul", Op::In, "null"), ids(&["2"]));
    assert_eq!(filter("name", Op::In, ""), ids(&[]));
    assert_eq!(filter("lc", Op::Eq, "blue"), ids(&["2"]));
    assert_eq!(filter("lc", Op::Contains, "LU"), ids(&["2"]));
    assert_eq!(filter("f", Op::Eq, "xyz"), ids(&["2"]));
    assert_eq!(filter("f", Op::Contains, "y"), ids(&["2"]));
    // Types that read as text but are none to the server.
    assert_eq!(filter("u", Op::Contains, "0002"), ids(&["2"]));
    assert_eq!(filter("u", Op::Eq, "00000000-0000-0000-0000-000000000003"), ids(&["3"]));
    assert_eq!(filter("ip4", Op::Contains, "0.0.2"), ids(&["2"]));
    assert_eq!(filter("ip4", Op::Eq, "10.0.0.3"), ids(&["3"]));
    assert_eq!(filter("ip6", Op::Contains, "db8"), ids(&["2"]));
    assert_eq!(filter("ip6", Op::Eq, "::1"), ids(&["1"]));
    assert_eq!(filter("en", Op::Eq, "blue"), ids(&["2"]));
    assert_eq!(filter("en", Op::In, "blue"), ids(&["2"]));
    assert_eq!(filter("en", Op::Contains, "LU"), ids(&["2"]));
    // Dates, at the limits of the wider ones.
    assert_eq!(filter("d", Op::Eq, "2024-01-03"), ids(&["2"]));
    assert_eq!(filter("d", Op::Lt, "2024-01-03"), ids(&["1"]));
    assert_eq!(filter("d", Op::Contains, "01-03"), ids(&["2"]));
    assert_eq!(filter("d32", Op::Eq, "1900-01-01"), ids(&["1"]));
    assert_eq!(filter("d32", Op::Ge, "2299-12-31"), ids(&["2"]));
    assert_eq!(filter("dt", Op::Eq, "2024-01-02 03:04:05"), ids(&["1"]));
    assert_eq!(filter("dt", Op::Gt, "2024-01-07 00:00:00"), ids(&["7"]));
    assert_eq!(filter("dt64", Op::Eq, "1900-01-01 00:00:00.500"), ids(&["1"]));
    assert_eq!(filter("dt64", Op::Eq, "2299-12-31 23:59:59.999"), ids(&["2"]));
    // Numbers past what a float tells apart: the row itself, not its neighbours.
    assert_eq!(filter("big", Op::Eq, "170141183460469231731687303715884105727"), ids(&["1"]));
    assert_eq!(filter("big", Op::Eq, "170141183460469231731687303715884105726"), ids(&["2"]));
    assert_eq!(filter("big", Op::In, "170141183460469231731687303715884105726"), ids(&["2"]));
    assert_eq!(filter("big", Op::Gt, "170141183460469231731687303715884105726"), ids(&["1"]));
    assert_eq!(filter("huge", Op::Eq, "115792089237316195423570985008687907853269984665640564039457584007913129639935"), ids(&["1"]));
    assert_eq!(filter("u64", Op::Eq, "18446744073709551615"), ids(&["1"]));
    assert_eq!(filter("u64", Op::Lt, "18446744073709551615").len(), 6);
    assert_eq!(filter("dec", Op::Eq, "1234567890123456789012345678.1234567891"), ids(&["1"]));
    assert_eq!(filter("dec", Op::Eq, "1234567890123456789012345678.1234567892"), ids(&["2"]));
    assert_eq!(filter("big", Op::Contains, "5727"), ids(&["1"]));
    assert_eq!(filter("id", Op::Eq, "3"), ids(&["3"]));
    assert_eq!(filter("id", Op::Contains, "3"), ids(&["3"]));
    assert_eq!(filter("fl", Op::Eq, "2.5"), ids(&["2"]));
    assert_eq!(filter("ok", Op::Eq, "false"), ids(&["2"]));
    // Composite ones, compared with the literal the grid shows, or searched as text.
    assert_eq!(filter("arr", Op::Contains, "b"), ids(&["1"]));
    assert_eq!(filter("arr", Op::Eq, "['a','b']"), ids(&["1"]));
    assert_eq!(filter("m", Op::Contains, "k"), ids(&["1"]));
    assert_eq!(filter("tu", Op::Contains, "t"), ids(&["2"]));
    assert_eq!(filter("v", Op::Contains, "ell"), ids(&["1"]));
    assert_eq!(filter("v", Op::IsNull, "").len(), 5);
    assert_eq!(filter("j", Op::Contains, "needle"), ids(&["1"]));
    ok(&e, &s, "DROP DATABASE dataman_ch_filters");
}

/// What `sqlgen` writes, read back by the server as the same value.
#[test]
fn literals_are_the_values_they_were_made_from() {
    use dataman_core::sqlgen::{CellValue, literal_in, value_literal_in};
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    for text in ["plain", "it's", "back\\slash", "\\", "\\'", "''", "tab\there", "line\nbreak", "cr\rhere", "é€😀", "%_", "$$", "$t$", "{x:String}", "-- c", "/* c */", "// c", "# c", "‘q’", "“q”", "\\x41", "\\N", "\u{a0}", ""] {
        let lit = value_literal_in(CH, Kind::Text, &CellValue::text(text));
        assert_eq!(shown(&ok(&e, &s, &format!("SELECT {lit}")), 0, 0).as_deref(), Some(text), "{lit}");
        assert_eq!(shown(&ok(&e, &s, &format!("SELECT {}", literal_in(CH, Kind::Text, Cell::Text(text)))), 0, 0).as_deref(), Some(text));
        // The guard reads the literal whole: nothing in a value becomes a statement or a call.
        assert_eq!(guard::agent_read_refusal_in(&format!("SELECT {lit}"), CH), None, "{lit}");
        assert_eq!(dataman_core::sql::split_in(&format!("SELECT {lit}"), CH).len(), 1, "{lit}");
    }
    assert_eq!(shown(&ok(&e, &s, &format!("SELECT {}", CH.quote_literal("a\\b'c"))), 0, 0).as_deref(), Some("a\\b'c"));
    // Bytes, by the literal the dialect has for them.
    let lit = value_literal_in(CH, Kind::Bytes, &CellValue::text("\\xff00"));
    assert_eq!(lit, "unhex('ff00')");
    assert_eq!(shown(&ok(&e, &s, &format!("SELECT {lit}")), 0, 0).as_deref(), Some("\\xff00"));
    // Dates and numbers, compared by the server with the type they are for.
    for (kind, value, ty) in [
        (Kind::Date, "2299-12-31", "Date32"),
        (Kind::Timestamp, "1900-01-01 00:00:00.5", "DateTime64(3, 'UTC')"),
        (Kind::Numeric, "170141183460469231731687303715884105727", "Int128"),
        (Kind::Numeric, "-1.5", "Decimal(10, 2)"),
        (Kind::Numeric, "18446744073709551615", "UInt64"),
        (Kind::Float, "1.5", "Float64"),
        (Kind::Int, "-3", "Int8"),
        (Kind::Bool, "true", "Bool"),
    ] {
        let lit = value_literal_in(CH, kind, &CellValue::text(value));
        let sql = format!("SELECT CAST({}, '{}') = {lit}", CH.quote_literal(value), ty.replace('\'', "\\'"));
        assert_eq!(shown(&ok(&e, &s, &sql), 0, 0).as_deref(), Some("1"), "{sql}");
    }
}

#[test]
fn the_catalog_holds_on_empty_databases_and_names_that_need_quotes() {
    let e = env();
    let Some(admin) = open(&e, profile(""), &password()) else { return };
    let a = admin.session(None);
    // Nothing in it: empty lists, not errors.
    ok(&e, &a, "DROP DATABASE IF EXISTS dataman_ch_vacant");
    ok(&e, &a, "CREATE DATABASE dataman_ch_vacant");
    let (rels, stats) = e.core.block_on(async { (admin.relations("dataman_ch_vacant", "dataman_ch_vacant").await.unwrap(), admin.rel_stats("dataman_ch_vacant", "dataman_ch_vacant").await.unwrap()) });
    assert!(rels.is_empty() && stats.is_empty());
    assert!(e.core.block_on(admin.databases()).unwrap().iter().any(|d| d == "dataman_ch_vacant"));
    assert!(e.core.block_on(admin.columns("dataman_ch_vacant", "dataman_ch_vacant", "nothing")).unwrap().is_empty());
    assert!(e.core.block_on(admin.indexes("dataman_ch_vacant", "dataman_ch_vacant", "nothing")).unwrap().is_empty());
    ok(&e, &a, "DROP DATABASE dataman_ch_vacant");
    // Every name quoted by the dialect, in every place the catalog names it.
    for db in ["dataman ch odd", "dataman`ch", "dataman\\ch", "dataman'ch", "dataman.ch", "select", "DataMan_Ch", "dataman-é"] {
        let qdb = CH.quote_ident(db);
        ok(&e, &a, &format!("DROP DATABASE IF EXISTS {qdb}"));
        ok(&e, &a, &format!("CREATE DATABASE {qdb}"));
        for table in ["a b", "a`b", "a\\b", "a'b", "a.b", "order", "CaSe", "é"] {
            let t = CH.qualified(Some(db), table);
            let col = format!("c {table}");
            let qc = CH.quote_ident(&col);
            ok(&e, &a, &format!("CREATE TABLE {t} ({qc} Int8, `from` String, k Int8, INDEX `i {}` `from` TYPE bloom_filter GRANULARITY 1) ENGINE = MergeTree ORDER BY ({qc}, k)", table.replace(['`', '\\'], "_")));
            ok(&e, &a, &format!("INSERT INTO {t} VALUES (1, 'x', 2)"));
            // The explorer reads each once: a connection of its own sees this table.
            let c = open(&e, profile(""), &password()).expect("connects");
            let (rels, cols, idx, ddl, key, stats) = e.core.block_on(async {
                (
                    c.relations(db, db).await.unwrap(),
                    c.columns(db, db, table).await.unwrap(),
                    c.indexes(db, db, table).await.unwrap(),
                    c.table_ddl(db, db, table).await.unwrap(),
                    c.primary_key(db, db, table).await.unwrap(),
                    c.rel_stats(db, db).await.unwrap(),
                )
            });
            assert!(rels.iter().any(|r| r.name == table), "{db}.{table}: {rels:?}");
            assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), [col.as_str(), "from", "k"], "{db}.{table}");
            assert!(cols[0].primary_key && cols[2].primary_key && !cols[1].primary_key);
            assert_eq!(key, [col.as_str(), "k"], "{db}.{table}");
            let pk = idx.iter().find(|i| i.primary).unwrap_or_else(|| panic!("{idx:?}"));
            assert_eq!(pk.columns, [col.as_str(), "k"], "{db}.{table}");
            assert!(idx.iter().any(|i| !i.primary && i.definition.as_deref().is_some_and(|d| d.contains("bloom_filter"))), "{idx:?}");
            assert!(ddl.contains("ENGINE = MergeTree"), "{ddl}");
            // A statement the server takes back.
            let again = ddl.replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1);
            ok(&e, &a, &again);
            // Rows, by what the grid and the explorer write.
            let sample = e.core.block_on(c.sample_column_values(db, db, table, "from", 5)).unwrap();
            assert_eq!(sample, ["x"], "{db}.{table}");
            assert_eq!(stats[table].rows, Some(1.0), "{db}.{table}");
            let view = dataman_core::dataview::TableView { dialect: CH, ..dataman_core::dataview::TableView::new(db, table, key.clone(), 10) };
            assert_eq!(ok(&e, &a, &view.sql()).rows(), 1, "{}", view.sql());
            assert_eq!(shown(&ok(&e, &a, &view.count_sql()), 0, 0).as_deref(), Some("1"));
            assert_eq!(ok(&e, &a, &dataman_core::sqlgen::select_star_in(CH, db, table, Some(1))).rows(), 1);
        }
        ok(&e, &a, &format!("DROP DATABASE {qdb}"));
    }
}

#[test]
fn a_key_of_expressions_is_cut_at_its_own_commas() {
    let e = env();
    let Some((c, s)) = own(&e, "dataman_ch_keys") else { return };
    ok(&e, &s, "CREATE TABLE t (a Int8, b String, c Date) ENGINE = MergeTree ORDER BY (toYYYYMM(c), intHash32(a), concat(b, 'x, y'))");
    ok(&e, &s, "CREATE TABLE u (`p, q` Int8, r Int8) ENGINE = MergeTree ORDER BY (`p, q`, r)");
    ok(&e, &s, "CREATE TABLE n (a Int8) ENGINE = MergeTree ORDER BY tuple()");
    let index = |t: &str| e.core.block_on(c.indexes("dataman_ch_keys", "dataman_ch_keys", t)).unwrap();
    assert_eq!(index("t")[0].columns, ["toYYYYMM(c)", "intHash32(a)", "concat(b, 'x, y')"]);
    assert_eq!(index("u")[0].columns, ["p, q", "r"]);
    assert!(index("n").is_empty(), "no key");
    ok(&e, &s, "DROP DATABASE dataman_ch_keys");
}

#[test]
fn the_tree_s_stats_say_nothing_of_what_stores_nothing() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_stats") else { return };
    let s = c.session(None);
    ok(&e, &s, "CREATE TABLE mem (a Int8) ENGINE = Memory");
    ok(&e, &s, "INSERT INTO mem VALUES (1), (2)");
    ok(&e, &s, "CREATE MATERIALIZED VIEW own_mv ENGINE = Memory AS SELECT id FROM events");
    let fresh = open(&e, profile("dataman_ch_stats"), &password()).expect("connects");
    let stats = e.core.block_on(fresh.rel_stats("dataman_ch_stats", "dataman_ch_stats")).unwrap();
    assert_eq!(stats["events"].rows, Some(3.0));
    assert!(stats["events"].bytes.is_some_and(|b| b > 0), "{:?}", stats["events"]);
    assert_eq!(stats["mem"].rows, Some(2.0));
    // A view holds no rows: no number, not zero.
    assert_eq!((stats["clicks"].rows, stats["clicks"].bytes), (None, None));
    let rels = e.core.block_on(fresh.relations("dataman_ch_stats", "dataman_ch_stats")).unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.name == n).map(|r| r.kind);
    assert_eq!((kind("clicks"), kind("own_mv")), (Some(RelKind::View), Some(RelKind::MaterializedView)));
    ok(&e, &s, "DROP DATABASE dataman_ch_stats");
}

#[test]
fn use_follows_names_of_any_shape() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let here = |s: &Arc<Session>| shown(&ok(&e, s, "SELECT currentDatabase()"), 0, 0).unwrap();
    for db in ["database", "dataman ch use", "dataman`use", "dataman\\use", "dataman'use", "dataman.use", "use", "é"] {
        let q = CH.quote_ident(db);
        ok(&e, &s, &format!("DROP DATABASE IF EXISTS {q}"));
        ok(&e, &s, &format!("CREATE DATABASE {q}"));
        ok(&e, &s, &format!("USE {q}"));
        assert_eq!(here(&s), db, "USE {q}");
        // Followed for the next statement in that session, a table there included.
        ok(&e, &s, "CREATE TABLE t (a Int8) ENGINE = Memory");
        ok(&e, &s, "INSERT INTO t VALUES (1)");
        assert_eq!(shown(&ok(&e, &s, "SELECT a FROM t"), 0, 0).as_deref(), Some("1"));
        assert_eq!(here(&c.session(None)), "default", "another tab stays");
        ok(&e, &s, "USE default");
        assert_eq!(here(&s), "default");
        ok(&e, &s, &format!("DROP DATABASE {q}"));
    }
    // The words the server takes: bare, `DATABASE`, a comment, a closing `;`.
    ok(&e, &s, "DROP DATABASE IF EXISTS `database`");
    ok(&e, &s, "CREATE DATABASE `database`");
    for sql in ["USE database", "USE DATABASE database", "use `database`", "/* c */ USE database ;", "USE\u{a0}database", "USE “database”", "USE `databas\\x65`"] {
        ok(&e, &s, "USE default");
        ok(&e, &s, sql);
        assert_eq!(here(&s), "database", "{sql:?}");
    }
    ok(&e, &s, "USE default");
    ok(&e, &s, "DROP DATABASE `database`");
    // A USE that fails moves nothing.
    assert_eq!(run(&e, &s, "USE nope_not_here").phase(), Phase::Failed);
    assert_eq!(here(&s), "default");
    // An agent's USE, once allowed, ends with its call: what the agent reads next is where it asked.
    ok(&e, &s, "DROP DATABASE IF EXISTS dataman_ch_use2");
    ok(&e, &s, "CREATE DATABASE dataman_ch_use2");
    e.core.block_on(c.bounded(Some("dataman_ch_use2"), "USE system", vec![], Mode::Autocommit, Limits::default(), std::future::pending())).unwrap();
    for mode in [Mode::Read, Mode::Autocommit] {
        let o = e.core.block_on(c.bounded(Some("dataman_ch_use2"), "SELECT currentDatabase()", vec![], mode, Limits::default(), std::future::pending())).unwrap();
        assert_eq!(o.rows[0][0], serde_json::json!("dataman_ch_use2"), "{mode:?}");
    }
    ok(&e, &s, "DROP DATABASE dataman_ch_use2");
}

#[test]
fn read_only_follows_a_change_of_mind_on_an_open_tab() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_toggle") else { return };
    let s = c.session(None);
    let probe = |s: &Arc<Session>, n: u32| run(&e, s, &format!("CREATE TABLE made{n} (a Int8) ENGINE = Memory"));
    assert_eq!(probe(&s, 1).phase(), Phase::Done);
    c.set_read_only_for("dataman_ch_toggle", true);
    assert!(c.read_only_for("dataman_ch_toggle"));
    let rs = probe(&s, 2);
    assert_eq!(rs.phase(), Phase::Failed, "the open tab is held at once");
    assert_eq!(rs.status().error.unwrap().class(Engine::ClickHouse), ErrorClass::ReadOnly);
    assert_eq!(shown(&ok(&e, &s, "SELECT count() FROM events"), 0, 0).as_deref(), Some("3"), "reads go on");
    // A tab opened before and one opened after agree, and other databases are not touched.
    assert_eq!(probe(&c.session(None), 3).phase(), Phase::Failed);
    let other = c.session(Some("default"));
    assert_eq!(shown(&ok(&e, &other, "SELECT getSetting('readonly')"), 0, 0).as_deref(), Some("0"));
    // An allowed write of an agent's is held too, by the same switch, and let go with it.
    let write = |n: u32| e.core.block_on(c.bounded(None, &format!("CREATE TABLE agent{n} (a Int8) ENGINE = Memory"), vec![], Mode::Autocommit, Limits::default(), std::future::pending()));
    assert!(matches!(write(1), Err(Stop::Refused(_))));
    c.set_read_only_for("dataman_ch_toggle", false);
    assert!(write(2).is_ok());
    assert_eq!(probe(&s, 4).phase(), Phase::Done, "and let go");
    // Its read pool stays read-only whatever the switch says.
    let o = e.core.block_on(c.bounded(None, "SELECT getSetting('readonly')", vec![], Mode::Read, Limits::default(), std::future::pending())).unwrap();
    assert_eq!(o.rows[0][0], serde_json::json!(1));
    assert_eq!(shown(&ok(&e, &s, "SELECT getSetting('readonly')"), 0, 0).as_deref(), Some("0"));
    let made = column(&e, &s, "SELECT name FROM system.tables WHERE database = currentDatabase() AND name LIKE 'made%' OR name LIKE 'agent%' ORDER BY name");
    assert_eq!(made, ["agent2", "made1", "made4"]);
    ok(&e, &s, "DROP DATABASE dataman_ch_toggle");
}

#[test]
fn an_agent_s_connection_is_as_good_after_a_failure() {
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_reuse") else { return };
    let s = c.session(None);
    let read = |sql: &str, limits: Limits| e.core.block_on(c.bounded(None, sql, vec![], Mode::Read, limits, std::future::pending()));
    let healthy = || {
        let o = read("SELECT currentDatabase(), getSetting('readonly'), count() FROM events", Limits::default()).unwrap();
        assert_eq!(o.rows[0], vec![serde_json::json!("dataman_ch_reuse"), serde_json::json!(1), serde_json::json!("3")]);
        assert_eq!(o.statements.len(), 1);
    };
    healthy();
    let failures: [(&str, fn(&Stop) -> bool); 5] = [
        ("SELECT 1 FROM", |x| matches!(x, Stop::Server(i) if i.code.as_deref() == Some("62"))),
        ("SELECT * FROM nope", |x| matches!(x, Stop::Server(i) if i.message.contains("nope"))),
        ("SELECT throwIf(1, 'boom')", |x| matches!(x, Stop::Server(i) if i.message.contains("boom"))),
        // After the first flush, status 200 already.
        ("SELECT number, throwIf(number = 2000000, 'boom') FROM numbers(3000000)", |x| matches!(x, Stop::Server(i) if i.message.contains("boom"))),
        ("SELECT {nope:String}", |x| matches!(x, Stop::Server(i) if i.message.contains("nope"))),
    ];
    for (sql, expected) in failures {
        for _ in 0..3 {
            let err = read(sql, Limits { max_rows: usize::MAX, max_bytes: usize::MAX, ..Limits::default() }).unwrap_err();
            assert!(expected(&err), "{sql}: {err}");
            healthy();
        }
    }
    // A statement of several: the first's rows do not lead the next call astray.
    let err = read("SELECT 1; SELECT * FROM nope", Limits::default()).unwrap_err();
    assert!(matches!(err, Stop::Server(_)), "{err}");
    healthy();
    // Cut short by its limits: what is left stops on the server.
    let probe = c.session(None);
    let tag = uuid_tag() + "cut";
    let running = format!("SELECT count() FROM system.processes WHERE query LIKE '%{tag}%' AND query NOT LIKE '%system.processes%'");
    let o = read(&format!("SELECT number, '{tag}' FROM numbers(100000000000)"), Limits::default()).unwrap();
    assert_eq!(o.truncated, Some(dataman_core::bounded::Truncation::Rows));
    let gone = |what: &str| {
        let t0 = Instant::now();
        while shown(&ok(&e, &probe, &running), 0, 0).as_deref() != Some("0") {
            assert!(t0.elapsed() < Duration::from_secs(30), "{what}: still running");
        }
    };
    gone("past its row limit");
    healthy();
    // So does one that runs out of time, and the next call does not wait for it.
    let tag = uuid_tag() + "time";
    let running = format!("SELECT count() FROM system.processes WHERE query LIKE '%{tag}%' AND query NOT LIKE '%system.processes%'");
    let slow = format!("SELECT count(), '{tag}' FROM numbers_mt(1000000000000000)");
    assert!(matches!(read(&slow, Limits { timeout: Duration::from_millis(300), ..Limits::default() }), Err(Stop::Timeout(_))));
    let t0 = Instant::now();
    while shown(&ok(&e, &probe, &running), 0, 0).as_deref() != Some("0") {
        assert!(t0.elapsed() < Duration::from_secs(30), "out of time: still running");
    }
    healthy();
    // An allowed write that fails leaves the next one able to run, on its own connection.
    let write = |sql: &str| e.core.block_on(c.bounded(None, sql, vec![], Mode::Autocommit, Limits::default(), std::future::pending()));
    assert!(matches!(write("INSERT INTO nope VALUES (1)"), Err(Stop::Server(_))));
    assert_eq!(write("INSERT INTO rollup (kind, n) VALUES ('k', 1)").unwrap().affected(), 1);
    healthy();
    ok(&e, &s, "DROP DATABASE dataman_ch_reuse");
}

fn exported(e: &Env, c: &Arc<Connection>, db: Option<&str>, sql: &str, opts: dataman_core::export::ExportOptions) -> anyhow::Result<(String, dataman_core::export::ExportSummary)> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out");
    let x = c.export_query(db, sql.to_string(), &path, opts, None);
    let done = e.core.block_on(x.task)?;
    Ok((std::fs::read_to_string(&path).unwrap_or_default(), done))
}

#[test]
fn exports_write_what_the_server_sent() {
    use dataman_core::export::{ExportFormat, ExportOptions};
    let e = env();
    let Some(c) = seeded(&e, "dataman_ch_export") else { return };
    let s = c.session(None);
    let db = Some("dataman_ch_export");
    let (csv, done) = exported(&e, &c, db, "SELECT id, user_id, kind, amount, ratio, happened_ms, day, tags, token, ok, note FROM events ORDER BY id", ExportOptions::new(ExportFormat::Csv)).unwrap();
    assert_eq!(done.rows, 3);
    assert!(done.complete);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], "id,user_id,kind,amount,ratio,happened_ms,day,tags,token,ok,note");
    assert!(lines[1].starts_with("1,10,click,12.5,0.5,2024-01-02 03:04:05.123,2024-01-02,"), "{}", lines[1]);
    assert!(lines[1].ends_with(",00000000-0000-0000-0000-000000000001,true,first"), "{}", lines[1]);
    assert!(lines[2].starts_with("2,,view,"), "NULL as nothing: {}", lines[2]);
    assert!(csv.contains("\"tab\there, line\nbreak, back\\slash\""), "unescaped, then quoted: {csv}");
    assert!(csv.contains("18446744073709551615,-1,click"), "exact: {csv}");
    // In JSON too, what is exact stays exact.
    let (json, _) = exported(&e, &c, db, "SELECT id, user_id, note FROM events ORDER BY id", ExportOptions::new(ExportFormat::Json)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[2]["id"].to_string().trim_matches('"'), "18446744073709551615");
    assert_eq!(v[1]["user_id"], serde_json::Value::Null);
    assert_eq!(v[1]["note"], "tab\there, line\nbreak, back\\slash");
    // More rows than a chunk, in order: nothing dropped between them.
    let (ndjson, done) = exported(&e, &c, db, "SELECT number AS n FROM numbers(250000)", ExportOptions::new(ExportFormat::Ndjson)).unwrap();
    assert_eq!(done.rows, 250000);
    assert_eq!(ndjson.lines().count(), 250000);
    assert_eq!(ndjson.lines().last(), Some("{\"n\":249999}"));
    // A statement with a format of its own is its lines.
    let (raw, done) = exported(&e, &c, db, "SELECT number AS a FROM numbers(2) FORMAT JSONEachRow", ExportOptions::new(ExportFormat::Tsv)).unwrap();
    assert_eq!(done.rows, 2);
    assert!(raw.contains("result") && raw.contains("a"), "{raw}");
    // No rows still has its header.
    let (csv, done) = exported(&e, &c, db, "SELECT id, kind FROM events WHERE 0", ExportOptions::new(ExportFormat::Csv)).unwrap();
    assert_eq!((done.rows, csv.trim()), (0, "id,kind"));
    // Back through INSERT: every value the server wrote, the server reads the same.
    ok(&e, &s, "CREATE TABLE copy (id UInt64, user_id Nullable(Int32), kind LowCardinality(String), amount Decimal(18, 2), happened DateTime('UTC'), day Date, token UUID, ok Bool, note String) ENGINE = MergeTree ORDER BY id");
    let sql = "SELECT id, user_id, kind, amount, happened, day, token, ok, note FROM events ORDER BY id";
    let opts = ExportOptions { table: "copy".into(), dialect: CH, ..ExportOptions::new(ExportFormat::SqlInsert) };
    let (inserts, _) = exported(&e, &c, db, sql, opts).unwrap();
    for st in dataman_core::sql::split_in(&inserts, CH) {
        if !inserts[st.range.clone()].trim().is_empty() {
            ok(&e, &s, &inserts[st.range]);
        }
    }
    let both = |t: &str| column(&e, &s, &format!("SELECT toString(tuple(id, user_id, kind, amount, happened, day, token, ok, note)) FROM {t} ORDER BY id"));
    assert_eq!(both("copy"), both("events"), "{inserts}");
    ok(&e, &s, "DROP DATABASE dataman_ch_export");
}

#[test]
fn an_export_that_fails_says_so_and_one_that_writes_does_not_start() {
    use dataman_core::export::{ExportFormat, ExportOptions};
    let e = env();
    let Some((c, s)) = own(&e, "dataman_ch_export2") else { return };
    let opts = || ExportOptions::new(ExportFormat::Csv);
    let err = exported(&e, &c, None, "SELECT * FROM nope", opts()).unwrap_err();
    assert!(format!("{err:#}").contains("nope"), "{err:#}");
    // After the first rows the status was 200 already.
    let err = exported(&e, &c, None, "SELECT number, throwIf(number = 2000000, 'boom') FROM numbers(3000000) SETTINGS max_block_size = 1000, max_threads = 1", opts()).unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "not a clean end: {err:#}");
    // It runs the statement again: what would write there is not exported, whatever it starts with.
    ok(&e, &s, "CREATE TABLE t (a Int8) ENGINE = Memory");
    for sql in ["SELECT 1 PARALLEL WITH DROP TABLE t", "DROP TABLE t", "INSERT INTO t SELECT 1", "SELECT 1 INTO OUTFILE 'x'"] {
        assert!(exported(&e, &c, None, sql, opts()).is_err(), "{sql}");
    }
    assert_eq!(column(&e, &s, "SELECT count() FROM t"), ["0"]);
    assert_eq!(column(&e, &s, "EXISTS TABLE t"), ["1"]);
    // A cancel stops it on the server.
    let dir = tempfile::tempdir().unwrap();
    let tag = uuid_tag() + "export";
    let x = c.export_query(None, format!("SELECT number, '{tag}' FROM numbers(100000000000)"), dir.path().join("big"), opts(), None);
    let t0 = Instant::now();
    while x.progress.rows() == 0 {
        assert!(t0.elapsed() < Duration::from_secs(30), "no rows");
        std::thread::yield_now();
    }
    x.progress.cancel();
    let done = e.core.block_on(x.task).unwrap();
    assert!(done.cancelled && !done.complete);
    let probe = c.session(None);
    let running = format!("SELECT count() FROM system.processes WHERE query LIKE '%{tag}%' AND query NOT LIKE '%system.processes%'");
    let t0 = Instant::now();
    while shown(&ok(&e, &probe, &running), 0, 0).as_deref() != Some("0") {
        assert!(t0.elapsed() < Duration::from_secs(30), "still running on the server");
    }
    ok(&e, &s, "DROP DATABASE dataman_ch_export2");
}

/// What a relation reads, from the catalog: every kind of relation, and what cannot be followed.
#[test]
fn view_sources_read_what_a_relation_reads() {
    let e = env();
    let name = format!("dataman_follow_{}", std::process::id());
    let Some(c) = seeded(&e, &name) else { return };
    let s = c.session(None);
    let pw = password();
    for sql in [
        format!("CREATE TABLE {name}.base (id UInt64, v String) ENGINE = MergeTree ORDER BY id"),
        format!("CREATE TABLE {name}.other (id UInt64, v String) ENGINE = MergeTree ORDER BY id"),
        format!("CREATE VIEW {name}.a_view AS SELECT b.id, v FROM {name}.base b JOIN (SELECT id FROM {name}.other) o USING id"),
        format!("CREATE VIEW {name}.in_view AS SELECT id FROM {name}.base WHERE id IN {name}.other"),
        format!("CREATE VIEW {name}.of_nothing AS SELECT number FROM numbers(1)"),
        format!("CREATE VIEW {name}.sys_view AS SELECT dummy FROM system.one"),
        format!("CREATE VIEW {name}.fn_view AS SELECT * FROM merge('{name}', 'base')"),
        format!("CREATE MATERIALIZED VIEW {name}.stored ENGINE = MergeTree ORDER BY id AS SELECT id FROM {name}.base"),
        format!("CREATE MATERIALIZED VIEW {name}.into_other TO {name}.other AS SELECT id, v FROM {name}.base"),
        format!("CREATE TABLE {name}.merged (id UInt64) ENGINE = Merge('{name}', '^(base|other)$')"),
        format!("CREATE TABLE {name}.buffered (id UInt64) ENGINE = Buffer('{name}', 'base', 1, 10, 100, 10, 100, 1000, 10000)"),
        format!("CREATE TABLE {name}.nowhere (id UInt64) ENGINE = Buffer('', '', 1, 10, 100, 10, 100, 1000, 10000)"),
        format!("CREATE TABLE {name}.shards (id UInt64) ENGINE = Distributed(default, '{name}', 'base')"),
        format!("CREATE DICTIONARY {name}.dict (id UInt64, v String) PRIMARY KEY id SOURCE(CLICKHOUSE(DB '{name}' TABLE 'base' USER 'default' PASSWORD '{pw}')) LAYOUT(FLAT()) LIFETIME(0)"),
        format!("CREATE DICTIONARY {name}.dict_q (id UInt64, v String) PRIMARY KEY id SOURCE(CLICKHOUSE(QUERY 'SELECT id, v FROM {name}.other' USER 'default' PASSWORD '{pw}')) LAYOUT(FLAT()) LIFETIME(0)"),
        format!("CREATE DICTIONARY {name}.dict_far (id UInt64, v String) PRIMARY KEY id SOURCE(CLICKHOUSE(HOST 'elsewhere' DB 'x' TABLE 'y')) LAYOUT(FLAT()) LIFETIME(0)"),
        format!("CREATE DICTIONARY {name}.dict_null (id UInt64, v String) PRIMARY KEY id SOURCE(NULL()) LAYOUT(FLAT()) LIFETIME(0)"),
        format!("CREATE TABLE {name}.dict_t (id UInt64, v String) ENGINE = Dictionary('{name}.dict')"),
        format!("CREATE TABLE {name}.outside (id UInt64) ENGINE = S3('http://127.0.0.1:1/x.csv', 'CSV')"),
    ] {
        ok(&e, &s, &sql);
    }
    let read = |n: &str| e.core.block_on(c.view_sources(&name, &name, n));
    let relations = |n: &str| {
        let mut v: Vec<String> = read(n).unwrap().iter().map(|s| {
            assert_eq!(s.kind, dataman_core::schema::ViewSourceKind::Relation, "{n}: {s:?}");
            format!("{}.{}", s.schema, s.name)
        }).collect();
        v.sort();
        v
    };
    let base = format!("{name}.base");
    let other = format!("{name}.other");
    assert_eq!(relations("a_view"), [base.as_str(), other.as_str()]);
    assert_eq!(relations("in_view"), [base.as_str(), other.as_str()], "IN names a table with no FROM");
    assert!(relations("of_nothing").is_empty());
    assert!(relations("base").is_empty(), "a table with data of its own and nothing feeding it");
    assert_eq!(relations("stored"), [base.as_str()]);
    assert_eq!(relations("into_other"), [base.as_str(), other.as_str()]);
    assert_eq!(relations("other"), [format!("{name}.into_other")], "a table a materialized view fills");
    assert_eq!(relations("merged"), [base.as_str(), other.as_str()]);
    assert_eq!(relations("buffered"), [base.as_str()]);
    assert!(relations("nowhere").is_empty());
    assert_eq!(relations("shards"), [base.as_str()]);
    assert_eq!(relations("dict"), [base.as_str()]);
    assert_eq!(relations("dict_q"), [other.as_str()]);
    assert!(relations("dict_null").is_empty());
    assert_eq!(relations("dict_t"), [format!("{name}.dict")]);
    let stored = ok(&e, &s, &format!("SELECT name FROM system.tables WHERE database = '{name}' AND name LIKE '.inner%'"));
    let stored = shown(&stored, 0, 0).unwrap();
    assert_eq!(relations(&stored), [format!("{name}.stored")], "where a materialized view stores is that view");
    for n in ["sys_view", "fn_view", "dict_far", "outside"] {
        let got = read(n).unwrap();
        assert!(got.iter().any(|s| s.kind == dataman_core::schema::ViewSourceKind::Unknown), "{n}: {got:?}");
    }
    assert!(read("no_such_relation").is_err(), "what the catalog does not show is not a relation with no sources");

    // A failed read says so: past one row every catalog read of this login is refused.
    let user = format!("{name}_small");
    ok(&e, &s, &format!("CREATE USER {user} IDENTIFIED WITH plaintext_password BY 'x' SETTINGS max_result_rows = 1, result_overflow_mode = 'throw'"));
    ok(&e, &s, &format!("GRANT SELECT ON {name}.* TO {user}"));
    let p = ConnectionProfile { user: user.clone(), ..profile(&name) };
    let small = open(&e, p, "x").expect("the login connects");
    let err = e.core.block_on(small.view_sources(&name, &name, "a_view")).expect_err("no answer is not an empty answer");
    assert!(format!("{err:#}").contains("max_result_rows") || format!("{err:#}").contains("Limit for result"), "{err:#}");
    ok(&e, &s, &format!("DROP USER {user}"));
    ok(&e, &s, &format!("DROP DATABASE {name}"));
}

/// A value reaches the server as it was given, through the form: every byte a cell may hold, next to
/// other values, on a read-only connection and a session's, with a statement past one field's limit.
#[test]
fn bound_values_reach_the_server_whole() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let value = "tab\there\nnewline\r\\back'quote\"é☃--boundary\r\n";
    let run = |s: &Arc<Session>, sql: &str, values: Vec<Option<String>>| {
        let rs = s.execute_with(sql.to_string(), values);
        rs.fetch_all();
        e.core.block_on(rs.wait_finished());
        assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
        rs
    };
    let rs = run(&s, "SELECT {a:String}, {b:UInt8}, {c:Nullable(String)} IS NULL", vec![Some(value.into()), Some("7".into()), None]);
    let snap = rs.snapshot();
    assert_eq!(snap.text(0, 0).as_deref(), Some(value));
    assert_eq!(snap.text(0, 1).as_deref(), Some("7"));
    assert_eq!(snap.text(0, 2).as_deref(), Some("1"));

    // 100 KB of statement, past what a form field takes in one, with a character across the cut.
    let long = format!("SELECT '{}', {{a:String}}", "é".repeat(50_000));
    let rs = run(&s, &long, vec![Some("v".into())]);
    assert_eq!(rs.snapshot().text(0, 1).as_deref(), Some("v"));

    // A read-only connection, the one agents read on.
    let o = e.core.block_on(c.bounded(None, "SELECT {a:String}", vec![Some(value.into())], Mode::Read, Limits::default(), std::future::pending::<()>())).unwrap();
    assert_eq!(o.rows[0][0], serde_json::json!(value));
}

fn drop_db(e: &Env, s: &Arc<Session>, name: &str) {
    ok(e, s, &format!("DROP DATABASE IF EXISTS {}", Dialect::ClickHouse.quote_ident(name)));
}

/// A database's name is data: whoever may create one must not run a statement as whoever opens it.
#[test]
fn a_database_name_cannot_close_a_literal() {
    let e = env();
    let Some(admin) = open(&e, profile(""), &password()) else { return };
    let s = admin.session(None);
    let pid = std::process::id();
    let secrets = format!("dataman_ch_secret_{pid}");
    drop_db(&e, &s, &secrets);
    ok(&e, &s, &format!("CREATE DATABASE {secrets}"));
    ok(&e, &s, &format!("CREATE TABLE {secrets}.payments (card String) ENGINE = Memory"));
    ok(&e, &s, &format!("INSERT INTO {secrets}.payments VALUES ('4111')"));
    let name = format!("aud{pid}\\') UNION ALL SELECT card, hex(1), hex(2), hex(3), hex(4), toUInt64(5) FROM {secrets}.payments -- ");
    drop_db(&e, &s, &name);
    let q = Dialect::ClickHouse.quote_ident(&name);
    ok(&e, &s, &format!("CREATE DATABASE {q}"));
    ok(&e, &s, &format!("CREATE TABLE {q}.t (a Int32) ENGINE = Memory"));

    let c = open(&e, profile(&name), &password()).unwrap();
    assert_eq!(c.database(), name);
    e.core.block_on(c.preload(&name)).unwrap();
    assert!(c.schema.cached_columns(&name, "4111", "01").is_none(), "the statement ran");
    let cols = c.schema.cached_columns(&name, &name, "t").expect("its own table's columns");
    assert_eq!(cols[0].name, "a");
    assert!(c.schema.notes().is_empty(), "{:?}", c.schema.notes());
    drop_db(&e, &s, &name);
    drop_db(&e, &s, &secrets);
}

/// The statement's target decides, not the database the connection is on.
#[test]
fn a_read_only_database_holds_against_a_write_from_another() {
    let e = env();
    let Some(admin) = open(&e, profile(""), &password()) else { return };
    let s = admin.session(None);
    let pid = std::process::id();
    let (w, ro) = (format!("dataman_ch_w_{pid}"), format!("dataman_ch_ro_{pid}"));
    for d in [&w, &ro] {
        drop_db(&e, &s, d);
        ok(&e, &s, &format!("CREATE DATABASE {d}"));
        ok(&e, &s, &format!("CREATE TABLE {d}.items (id UInt64) ENGINE = MergeTree ORDER BY id"));
    }
    ok(&e, &s, &format!("INSERT INTO {ro}.items VALUES (1)"));
    let count = |d: &str| shown(&ok(&e, &s, &format!("SELECT count() FROM {d}.items")), 0, 0);

    let mut p = profile(&w);
    p.set_read_only_for(&ro, true);
    let c = open(&e, p, &password()).unwrap();
    let call = |sql: &str, mode: Mode| e.core.block_on(c.bounded(None, sql, vec![], mode, Limits::default(), std::future::pending::<()>()));
    for sql in [
        format!("INSERT INTO {ro}.items VALUES (2)"),
        format!("INSERT INTO `{ro}`.`items` VALUES (2)"),
        format!("ALTER TABLE {ro}.items DELETE WHERE 1"),
        format!("ALTER TABLE {ro}.items UPDATE id = 9 WHERE 1"),
        format!("DROP TABLE {ro}.items"),
        format!("TRUNCATE TABLE {ro}.items"),
        format!("DROP DATABASE {ro}"),
        format!("RENAME TABLE {ro}.items TO {w}.taken"),
        format!("/* a comment */ INSERT /* here too */ INTO {ro} . items VALUES (2)"),
    ] {
        assert!(matches!(call(&sql, Mode::Autocommit), Err(Stop::Refused(r)) if r.contains(&format!("read-only database {ro}"))), "{sql}");
    }
    assert_eq!(count(&ro).as_deref(), Some("1"));

    // The database itself stays writable, and what is only read of the other one stays allowed.
    call(&format!("INSERT INTO {w}.items VALUES (5)"), Mode::Autocommit).unwrap();
    call("INSERT INTO items VALUES (6)", Mode::Autocommit).unwrap();
    call(&format!("INSERT INTO {w}.items SELECT id FROM {ro}.items"), Mode::Autocommit).unwrap();
    assert_eq!(count(&w).as_deref(), Some("3"));
    assert_eq!(call(&format!("SELECT count() FROM {ro}.items"), Mode::Read).unwrap().rows.len(), 1);

    // A tab: the same check, with the database a `USE` left it on.
    let tab = c.session(None);
    assert_eq!(c.read_only_refusal(&tab.current_database(), "INSERT INTO items VALUES (7)"), None);
    assert!(c.read_only_refusal(&tab.current_database(), &format!("ALTER TABLE {ro}.items DELETE WHERE 1")).is_some());
    ok(&e, &tab, &format!("USE {ro}"));
    assert_eq!(tab.current_database(), ro);
    assert!(c.read_only_refusal(&tab.current_database(), "INSERT INTO items VALUES (7)").is_some());
    assert_eq!(c.read_only_refusal(&tab.current_database(), "SELECT * FROM items"), None);
    ok(&e, &tab, &format!("USE {w}"));
    assert_eq!(c.read_only_refusal(&tab.current_database(), "INSERT INTO items VALUES (7)"), None);

    // The toggle counts at once.
    c.set_read_only_for(&ro, false);
    call(&format!("INSERT INTO {ro}.items VALUES (2)"), Mode::Autocommit).unwrap();

    // Read-only by default with one database let out: every other one holds, and a table's name in
    // front of its column is no database.
    let mut p = profile(&w);
    p.read_only = true;
    p.set_read_only_for(&w, false);
    let c = open(&e, p, &password()).unwrap();
    let call = |sql: &str, mode: Mode| e.core.block_on(c.bounded(None, sql, vec![], mode, Limits::default(), std::future::pending::<()>()));
    assert!(matches!(call(&format!("INSERT INTO {ro}.items VALUES (3)"), Mode::Autocommit), Err(Stop::Refused(r)) if r.contains("read-only")));
    assert_eq!(c.read_only_refusal(&w, "ALTER TABLE items DELETE WHERE items.id = 0"), None);
    drop_db(&e, &s, &w);
    drop_db(&e, &s, &ro);
}

/// What the row cap does on the server's real stream: the rows before it stay, the statement stops.
#[test]
fn a_long_cell_stops_the_statement_on_the_server() {
    let e = env();
    let Some(c) = open(&e, profile(""), &password()) else { return };
    let s = c.session(None);
    let tag = uuid_tag();
    let sql = format!("SELECT number, if(number < 2, '{tag}', arrayStringConcat(arrayMap(x -> repeat('x', 1000000), range(140)))) FROM numbers(100000000) SETTINGS max_block_size = 1");
    let rs = run(&e, &s, &sql);
    assert_eq!(rs.phase(), Phase::Failed);
    assert!(rs.status().error.unwrap().message.contains("larger than 128 MiB"));
    assert_eq!(rs.rows(), 2, "the rows before it stay");
    let probe = c.session(None);
    let running = format!("SELECT count() FROM system.processes WHERE query LIKE '%{tag}%' AND query NOT LIKE '%system.processes%'");
    let t0 = Instant::now();
    while shown(&ok(&e, &probe, &running), 0, 0).as_deref() != Some("0") {
        assert!(t0.elapsed() < Duration::from_secs(10), "still running on the server");
    }
    // Just under the cap is a row like any other.
    let rs = ok(&e, &s, "SELECT length(a), a FROM (SELECT arrayStringConcat(arrayMap(x -> repeat('x', 1000000), range(120))) AS a)");
    assert_eq!(rs.snapshot().text(0, 0).as_deref(), Some("120000000"));
}
