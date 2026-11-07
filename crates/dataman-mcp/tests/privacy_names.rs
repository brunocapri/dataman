//! What a name in an agent's statement means is the server's to say: through the tools, on every
//! engine with a server (and the two file engines), what Dataman takes for a WITH query must be one
//! for the server too, or the rules are checked against the wrong thing. Each statement also runs
//! on a connection without rules, which says whether that server does reach the real table: the
//! table below is what the servers were seen to do. The live parts skip when a server is not up.

use std::sync::Arc;
use std::time::Duration;

use dataman_core::{Dirs, MemorySecrets, SecretStore};
use dataman_mcp::{Approval, Front, Hub, Registry, Server};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const CARD: &str = "card-4242-secret";
const HASH: &str = "hash-secret-ana";
const EMAIL: &str = "ana-at-acme";

fn addr(var: &str, default: &str) -> (String, u16) {
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (h, p) = v.rsplit_once(':').unwrap();
    (h.into(), p.parse().unwrap())
}

fn up(var: &str, default: &str) -> bool {
    let (h, p) = addr(var, default);
    let ok = std::net::TcpStream::connect_timeout(&format!("{h}:{p}").parse().unwrap(), Duration::from_secs(1)).is_ok_and(|s| s.local_addr().ok() != s.peer_addr().ok());
    if !ok {
        eprintln!("skipping: nothing at {h}:{p} ({var})");
    }
    ok
}

#[derive(Default)]
struct Human {
    allow: Mutex<Option<bool>>,
}

impl Front for Human {
    fn approve(&self, _: Approval) -> BoxFuture<'static, bool> {
        let yes = self.allow.lock().unwrap_or(false);
        Box::pin(async move { yes })
    }
}

struct Env {
    server: Server,
    human: Arc<Human>,
    _dir: tempfile::TempDir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum E {
    Pg,
    My,
    Ms,
    Ch,
    Lite,
    Duck,
}

impl E {
    fn id(self) -> &'static str {
        match self {
            E::Pg => "pg",
            E::My => "my",
            E::Ms => "ms",
            E::Ch => "ch",
            E::Lite => "lite",
            E::Duck => "duck",
        }
    }

    fn up(self) -> bool {
        match self {
            E::Pg => up("DATAMAN_PG", "127.0.0.1:55432"),
            E::My => up("DATAMAN_MYSQL", "127.0.0.1:33306"),
            E::Ms => up("DATAMAN_MSSQL", "127.0.0.1:14330"),
            E::Ch => up("DATAMAN_CLICKHOUSE", "127.0.0.1:18123"),
            E::Lite | E::Duck => true,
        }
    }
}

/// Every engine four ways: `x-root` where a database is made, `x-open` without rules, `x` with a
/// hidden table and a hidden column, `x-masked` with a masked column, `x-structure` structure only.
fn env(p: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("dataman");
    std::fs::create_dir_all(&cfg).unwrap();
    let lite = dir.path().join("w.sqlite");
    let duck = dir.path().join("w.duckdb");
    dataman_core::import::create_database(dataman_core::Engine::Sqlite, &lite).unwrap();
    dataman_core::import::create_database(dataman_core::Engine::DuckDb, &duck).unwrap();
    let server = |id: &str, var: &str, default: &str, user: &str, db: &str, extra: &str| {
        let (h, port) = addr(var, default);
        format!("[[connection]]\nid = \"{id}\"\nname = \"{id}\"\nhost = \"{h}\"\nport = {port}\nuser = \"{user}\"\ndatabase = \"{db}\"\n{extra}\n")
    };
    let file = |id: &str, engine: &str, path: &std::path::Path, extra: &str| format!("[[connection]]\nid = \"{id}\"\nname = \"{id}\"\nengine = \"{engine}\"\npath = '{}'\nsslmode = \"disable\"\n{extra}\n", path.display());
    let hidden = format!("mcp = \"full\"\n[connection.mcp_rules.visible]\nitems = [{{ name = \"{p}_pay\", kind = \"table\" }}, {{ name = \"{p}_users.pw\", kind = \"column\" }}]\n");
    let masked = format!("mcp = \"full\"\n[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"{p}_cust.email\", how = \"partial\" }}]\n");
    let structure = "mcp = \"full\"\n[connection.mcp_rules]\nview = \"structure\"\n";
    let open = "mcp = \"full\"\n";
    let mut text = String::new();
    let (pg, my, ms, ch) = ("sslmode = \"disable\"\n", "engine = \"mysql\"\nsave_password = true\nsslmode = \"disable\"\n", "engine = \"sql-server\"\nsave_password = true\nsslmode = \"require\"\n", "engine = \"clickhouse\"\nsave_password = true\nsslmode = \"disable\"\n");
    for (suffix, rules) in [("", hidden.as_str()), ("-masked", masked.as_str()), ("-structure", structure), ("-open", open)] {
        text += &server(&format!("pg{suffix}"), "DATAMAN_PG", "127.0.0.1:55432", "postgres", "dataman", &format!("{pg}{rules}"));
        text += &server(&format!("my{suffix}"), "DATAMAN_MYSQL", "127.0.0.1:33306", "root", p, &format!("{my}{rules}"));
        text += &server(&format!("ms{suffix}"), "DATAMAN_MSSQL", "127.0.0.1:14330", "sa", "tempdb", &format!("{ms}{rules}"));
        text += &server(&format!("ch{suffix}"), "DATAMAN_CLICKHOUSE", "127.0.0.1:18123", "default", p, &format!("{ch}{rules}"));
        text += &file(&format!("lite{suffix}"), "sqlite", &lite, rules);
        text += &file(&format!("duck{suffix}"), "duckdb", &duck, rules);
    }
    text += &server("my-root", "DATAMAN_MYSQL", "127.0.0.1:33306", "root", "mysql", &format!("{my}{open}"));
    text += &server("ch-root", "DATAMAN_CLICKHOUSE", "127.0.0.1:18123", "default", "default", &format!("{ch}{open}"));
    std::fs::write(cfg.join("connections.toml"), text).unwrap();
    std::fs::write(cfg.join("settings.toml"), "").unwrap();
    let pgpass = dir.path().join("pgpass");
    let (ph, pp) = addr("DATAMAN_PG", "127.0.0.1:55432");
    std::fs::write(&pgpass, format!("{ph}:{pp}:*:postgres:dataman\n")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&pgpass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    for (engine, pw) in [("my", "dataman-root"), ("ms", "Dataman-sa-1!"), ("ch", "dataman")] {
        for suffix in ["", "-masked", "-structure", "-open", "-root"] {
            secrets.set(&format!("{engine}{suffix}"), pw).unwrap();
        }
    }
    let hub = Hub::new(Dirs::at(&cfg), secrets).unwrap();
    let hub = {
        let mut h = Arc::try_unwrap(hub).ok().unwrap();
        h.core.pgpass = Some(pgpass);
        Arc::new(h)
    };
    let human = Arc::new(Human::default());
    hub.set_front(Some(human.clone()));
    hub.set_approval_timeout(Duration::from_millis(600));
    hub.set_after_deny(Duration::ZERO);
    let reg = Arc::new(Registry::standard(None));
    Env { server: Server::new(hub, reg, "stdio"), human, _dir: dir }
}

/// `$pay`, `$PAY`, `$users`, `$open` and `$cust` are this run's tables.
fn named(sql: &str, p: &str) -> String {
    sql.replace("$pay", &format!("{p}_pay")).replace("$PAY", &format!("{}_PAY", p.to_uppercase())).replace("$users", &format!("{p}_users")).replace("$open", &format!("{p}_open")).replace("$cust", &format!("{p}_cust"))
}

impl Env {
    /// What an agent is told, reply or refusal: what it has seen either way.
    fn call(&self, tool: &str, conn: &str, sql: &str) -> Result<Value, String> {
        let t = self.server.registry.find(tool).unwrap();
        let args = json!({"connection": conn, "sql": sql});
        let r = self.server.hub.core.block_on(self.server.run_tool(t, args.as_object().unwrap().clone(), "names-test".into(), CancellationToken::new()));
        let text = match &r.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("{other:?}"),
        };
        if r.is_error == Some(true) { Err(text) } else { Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))) }
    }

    fn shown(&self, tool: &str, conn: &str, sql: &str) -> String {
        match self.call(tool, conn, sql) {
            Ok(v) => v.to_string(),
            Err(e) => e,
        }
    }

    fn admin(&self, conn: &str, sql: &str) {
        *self.human.allow.lock() = Some(true);
        let r = self.call("execute", conn, sql);
        *self.human.allow.lock() = None;
        if let Err(e) = r {
            panic!("{conn}: {sql}: {e}");
        }
    }

    fn seed(&self, e: E, p: &str) {
        let open = format!("{}-open", e.id());
        let (int, text, tail) = match e {
            E::Ch => ("UInt64", "String", " ENGINE = MergeTree ORDER BY id"),
            E::Ms | E::My => ("int", "varchar(100)", ""),
            _ => ("int", "text", ""),
        };
        match e {
            E::My => {
                self.admin("my-root", &format!("DROP DATABASE IF EXISTS {p}"));
                self.admin("my-root", &format!("CREATE DATABASE {p}"));
            }
            E::Ch => {
                self.admin("ch-root", &format!("DROP DATABASE IF EXISTS {p}"));
                self.admin("ch-root", &format!("CREATE DATABASE {p}"));
            }
            _ => {}
        }
        for (table, cols) in [("$pay", format!("id {int}, card {text}")), ("$users", format!("id {int}, name {text}, pw {text}")), ("$open", format!("id {int}, note {text}")), ("$cust", format!("id {int}, email {text}"))] {
            // A key, so that a row a filter names is one MySQL reads while it plans.
            let key = if tail.is_empty() { format!("{}, PRIMARY KEY (id)", cols.replacen(&format!("id {int}"), &format!("id {int} NOT NULL"), 1)) } else { cols };
            self.admin(&open, &named(&format!("CREATE TABLE {table} ({key}){tail}"), p));
        }
        for sql in [format!("INSERT INTO $pay VALUES (1, '{CARD}')"), format!("INSERT INTO $users VALUES (1, 'Ana', '{HASH}')"), "INSERT INTO $open VALUES (7, 'plain')".to_string(), format!("INSERT INTO $cust VALUES (1, '{EMAIL}')")] {
            self.admin(&open, &named(&sql, p));
        }
    }

    fn unseed(&self, e: E, p: &str) {
        let open = format!("{}-open", e.id());
        match e {
            E::My => self.admin("my-root", &format!("DROP DATABASE IF EXISTS {p}")),
            E::Ch => self.admin("ch-root", &format!("DROP DATABASE IF EXISTS {p}")),
            E::Lite | E::Duck => {}
            E::Pg | E::Ms => {
                for t in ["$pay", "$users", "$open", "$cust"] {
                    self.admin(&open, &named(&format!("DROP TABLE {t}"), p));
                }
            }
        }
    }
}

use E::{Ch, Duck, Lite, Ms, My, Pg};
const ALL: [E; 6] = [Pg, My, Ms, Ch, Lite, Duck];

/// Each statement, and the servers on which it reaches the real table.
const REACH_THE_TABLE: &[(&str, &[E])] = &[
    ("WITH $pay AS (SELECT * FROM $pay) SELECT * FROM $pay", &[Pg, My, Ch, Duck]),
    ("WITH a AS (SELECT * FROM $pay), $pay AS (SELECT 1 AS x, 'c' AS y) SELECT * FROM a", &[Pg, My, Ms, Duck]),
    ("WITH x AS (SELECT 1 AS y), $pay AS (SELECT * FROM (SELECT * FROM $pay) q) SELECT * FROM $pay", &[Pg, My, Ch, Duck]),
    ("WITH $pay AS (WITH x AS (SELECT * FROM $pay) SELECT * FROM x) SELECT * FROM $pay", &[Pg, My, Ch, Duck]),
    ("WITH $pay AS (SELECT 1 AS id, 'x' AS card UNION ALL SELECT * FROM $pay WHERE id = 1 AND card <> 'x') SELECT * FROM $pay", &[Pg, My, Ch, Duck]),
    ("WITH RECURSIVE $pay AS (SELECT * FROM $pay UNION ALL SELECT 1, 'x') SELECT * FROM $pay", &[Ch, Duck]),
    ("WITH RECURSIVE $pay AS (SELECT * FROM $pay) SELECT * FROM $pay", &[Ch, Duck]),
    ("WITH RECURSIVE a AS (SELECT * FROM $pay), $pay AS (SELECT 1 AS x, 'c' AS y) SELECT * FROM a", &[My, Duck]),
    ("WITH RECURSIVE $pay AS (SELECT 5 AS id, 'x' AS card UNION ALL SELECT * FROM $pay WHERE id = 1 UNION ALL SELECT 6, 'y') SELECT * FROM $pay", &[Duck]),
    ("WITH RECURSIVE $pay AS (SELECT 5 AS id, 'x' AS card UNION ALL SELECT * FROM $pay WHERE id = 1 EXCEPT SELECT 9, 'z') SELECT * FROM $pay", &[Ch, Duck]),
    ("WITH RECURSIVE $pay AS (WITH b AS (SELECT 5 AS id, 'x' AS card UNION ALL SELECT * FROM $pay) SELECT * FROM b) SELECT * FROM $pay", &[Ch, Duck]),
    // A cycle of names leads a query back to the table of its own name.
    ("WITH $pay AS (SELECT * FROM $open), $open AS (SELECT * FROM $pay) SELECT * FROM $pay", &[Ch]),
    ("SELECT 1 AS id, 'a' AS card UNION ALL WITH $pay AS (SELECT 2 AS id, 'c' AS card) SELECT * FROM $pay UNION ALL SELECT * FROM $pay", &[Ch]),
    ("SELECT * FROM (SELECT 1 AS id, 'a' AS card UNION ALL WITH $pay AS (SELECT 2 AS id, 'c' AS card) SELECT * FROM $pay UNION ALL SELECT * FROM $pay) q", &[Ch]),
    ("WITH $PAY AS (SELECT 2 AS id, 'c' AS card) SELECT * FROM $pay", &[My, Ch]),
    ("WITH 1 AS $pay SELECT * FROM $pay", &[Ch]),
    ("WITH (SELECT 1) AS $pay SELECT * FROM $pay", &[Ch]),
    ("(WITH $pay AS (SELECT 2 AS id, 'c' AS card) SELECT * FROM $pay) UNION ALL SELECT * FROM $pay", &[Pg, Ch, Duck]),
    ("SELECT 1 AS id, 'a' AS card UNION ALL (WITH $pay AS (SELECT 2 AS id, 'c' AS card) SELECT * FROM $pay) UNION ALL SELECT * FROM $pay", &[Pg, Ch, Duck]),
];

/// Whether a WITH query is the same name's reach for the hidden column as for the hidden table.
const REACH_THE_COLUMN: &[(&str, &[E])] = &[
    ("WITH $users AS (SELECT * FROM $users) SELECT * FROM $users", &[Pg, My, Ch, Duck]),
    ("WITH a AS (SELECT * FROM $users), $users AS (SELECT 1 AS x) SELECT * FROM a", &[Pg, My, Ms, Duck]),
];

#[test]
fn a_with_query_named_like_a_table_does_not_hide_it() {
    let p = format!("w{}", std::process::id());
    let e = env(&p);
    let mut fails = Vec::new();
    for engine in ALL.into_iter().filter(|x| x.up()) {
        e.seed(engine, &p);
        let (guarded, open) = (engine.id().to_string(), format!("{}-open", engine.id()));
        assert!(e.call("run_query", &guarded, &named("SELECT * FROM $pay", &p)).is_err(), "{engine:?}: the rule hides the table");
        for (list, secret) in [(REACH_THE_TABLE, CARD), (REACH_THE_COLUMN, HASH)] {
            for (sql, reach) in list {
                let sql = named(sql, &p);
                let bare = e.shown("run_query", &open, &sql);
                if bare.contains(secret) != reach.contains(&engine) {
                    fails.push(format!("{engine:?} reads this differently than this test says: {sql}\n  {bare}"));
                }
                let told = e.shown("run_query", &guarded, &sql);
                if told.contains(secret) {
                    fails.push(format!("{engine:?} let it out: {sql}\n  {told}"));
                }
            }
        }
        e.unseed(engine, &p);
    }
    assert!(fails.is_empty(), "\n{}", fails.join("\n"));
}

#[test]
fn a_write_s_own_table_is_the_table_whatever_the_with_says() {
    let p = format!("wt{}", std::process::id());
    let e = env(&p);
    for engine in [Pg, Lite, Duck].into_iter().filter(|x| x.up()) {
        e.seed(engine, &p);
        let (guarded, open) = (engine.id().to_string(), format!("{}-open", engine.id()));
        *e.human.allow.lock() = Some(true);
        for sql in ["WITH $pay AS (SELECT 1 AS x) DELETE FROM $pay", "WITH $pay AS (SELECT 1 AS x) UPDATE $pay SET card = 'z' WHERE id = 1", "WITH $pay AS (SELECT 1 AS x) INSERT INTO $pay VALUES (2, 'new')", "WITH $users AS (SELECT 1 AS x) UPDATE $users SET pw = 'z' WHERE id = 1"] {
            let sql = named(sql, &p);
            let err = e.call("execute", &guarded, &sql).expect_err(&sql);
            assert!(err.contains("not visible") || err.contains("hidden"), "{engine:?}: {sql}: {err}");
        }
        *e.human.allow.lock() = None;
        assert!(e.shown("run_query", &open, &named("SELECT card FROM $pay", &p)).contains(CARD), "{engine:?}");
        assert!(e.shown("run_query", &open, &named("SELECT pw FROM $users", &p)).contains(HASH), "{engine:?}");
        e.unseed(engine, &p);
    }
}

#[test]
fn ordinary_with_queries_keep_working() {
    let p = format!("wo{}", std::process::id());
    let e = env(&p);
    for engine in ALL.into_iter().filter(|x| x.up()) {
        e.seed(engine, &p);
        let conn = engine.id();
        let recursive = if engine == Ms { "" } else { "RECURSIVE " };
        for (sql, want) in [
            ("SELECT * FROM $open".to_string(), "plain"),
            ("SELECT count(*) AS k FROM $open".to_string(), "1"),
            ("SELECT * FROM $users".to_string(), "Ana"),
            ("WITH a AS (SELECT * FROM $open) SELECT * FROM a".to_string(), "plain"),
            ("WITH a AS (SELECT note FROM $open), b AS (SELECT * FROM a) SELECT * FROM b".to_string(), "plain"),
            ("WITH a AS (SELECT note FROM $open) SELECT * FROM (SELECT * FROM a) q".to_string(), "plain"),
            ("WITH a AS (SELECT note FROM $open) SELECT note FROM a UNION ALL SELECT note FROM a".to_string(), "plain"),
            // Its own name is the table where the server reads the table, and the table is open.
            ("WITH $users AS (SELECT * FROM $users) SELECT name FROM $users".to_string(), "Ana"),
            (format!("WITH {recursive}c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM c WHERE n < 3) SELECT max(n) AS most FROM c"), "3"),
        ] {
            let sql = named(&sql, &p);
            let v = e.call("run_query", conn, &sql).unwrap_or_else(|err| panic!("{engine:?}: {sql}: {err}"));
            assert!(v["rows"].to_string().contains(want), "{engine:?}: {sql}: {v}");
        }
        // Inside a subquery of the recursive term, where the server allows it.
        if matches!(engine, Pg | Ch | Duck) {
            let sql = "WITH RECURSIVE c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM (SELECT n FROM c) q WHERE n < 3) SELECT max(n) AS most FROM c";
            assert!(e.call("run_query", conn, sql).unwrap_or_else(|err| panic!("{engine:?}: {err}"))["rows"].to_string().contains('3'), "{engine:?}");
        }
        // A later query's name, where the server reaches it.
        if matches!(engine, Pg | Lite) {
            let sql = "WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT 5 AS x) SELECT x FROM a";
            assert!(e.call("run_query", conn, sql).unwrap_or_else(|err| panic!("{engine:?}: {err}"))["rows"].to_string().contains('5'), "{engine:?}");
        }
        if engine == Lite {
            let v = e.call("run_query", conn, "WITH c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM c WHERE n < 3) SELECT max(n) AS most FROM c").unwrap();
            assert!(v["rows"].to_string().contains('3'), "{v}");
        }
        e.unseed(engine, &p);
    }
}

/// On a connection that keeps something, settings that change what names mean are not the agent's.
#[test]
fn clickhouse_settings_do_not_change_what_a_name_means() {
    if !Ch.up() {
        return;
    }
    let p = format!("ws{}", std::process::id());
    let e = env(&p);
    e.seed(Ch, &p);
    *e.human.allow.lock() = Some(true);
    for sql in [
        "CREATE TABLE $open.c ENGINE = Memory AS WITH $pay AS (SELECT 2 AS x) SELECT * FROM (SELECT * FROM $pay) SETTINGS enable_global_with_statement = 0",
        "SET enable_global_with_statement = 0",
        "SET enable_analyzer = 0",
        "INSERT INTO $open SELECT 1, 'x' SETTINGS allow_experimental_analyzer = 0",
    ] {
        let sql = named(sql, &p);
        assert!(e.call("execute", "ch", &sql).unwrap_err().contains("setting"), "{sql}");
    }
    *e.human.allow.lock() = None;
    e.unseed(Ch, &p);
}

#[test]
fn structure_only_returns_no_rows_through_a_plan() {
    let p = format!("wp{}", std::process::id());
    let e = env(&p);
    if Ch.up() {
        e.seed(Ch, &p);
        // Each prints the row on a connection without rules.
        for sql in [
            "EXPLAIN SYNTAX run_query_tree_passes=1 SELECT (SELECT card FROM $pay LIMIT 1)",
            "EXPLAIN QUERY TREE SELECT (SELECT card FROM $pay LIMIT 1)",
            "EXPLAIN QUERY TREE WITH (SELECT card FROM $pay LIMIT 1) AS c SELECT c",
        ] {
            let sql = named(sql, &p);
            assert!(e.shown("run_query", "ch-open", &sql).contains(CARD), "{sql} prints no row: the case proves nothing");
            for conn in ["ch-structure", "ch-masked"] {
                let told = e.call("run_query", conn, &sql).expect_err(&sql);
                assert!(told.contains("EXPLAIN") && !told.contains(CARD), "{conn}: {sql}: {told}");
            }
        }
        let plain = named("EXPLAIN SELECT id FROM $pay WHERE id = 1", &p);
        assert!(e.call("run_query", "ch-structure", &plain).is_ok(), "a plan is structure");
        e.unseed(Ch, &p);
    }
    if My.up() {
        e.seed(My, &p);
        let wrote_it = "EXPLAIN FORMAT=TREE SELECT o.id FROM $open o JOIN $pay k ON k.id = 1 WHERE o.note = k.card";
        for sql in [wrote_it, "EXPLAIN FORMAT=JSON SELECT o.id FROM $open o WHERE o.note = (SELECT card FROM $pay WHERE id = 1)", "EXPLAIN FORMAT=TREE SELECT o.id FROM $open o WHERE o.note IN (SELECT card FROM $pay WHERE id = 1)"] {
            let sql = named(sql, &p);
            assert!(e.shown("run_query", "my-open", &sql).contains(CARD), "{sql} prints no row: the case proves nothing");
            let told = e.call("run_query", "my-structure", &sql).expect_err(&sql);
            assert!(!told.contains(CARD), "{sql}: {told}");
        }
        assert!(e.call("run_query", "my-structure", &named("EXPLAIN FOR CONNECTION 1", &p)).is_err());
        let plain = named("EXPLAIN SELECT id FROM $pay WHERE id = 1", &p);
        assert!(e.call("run_query", "my-structure", &plain).is_ok(), "a plan is structure");
        // The server's warnings quote the values a cast choked on and the plan with the row it read.
        for sql in ["EXPLAIN SELECT * FROM $cust WHERE id = 1; SHOW WARNINGS", "SELECT id FROM $cust WHERE email = 0; SHOW WARNINGS", "SHOW ERRORS"] {
            let sql = named(sql, &p);
            assert!(e.call("run_query", "my-masked", &sql).is_err(), "{sql}");
        }
        e.unseed(My, &p);
    }
    if Pg.up() {
        e.seed(Pg, &p);
        let plain = named("EXPLAIN SELECT (SELECT card FROM $pay LIMIT 1)", &p);
        assert!(!e.call("run_query", "pg-structure", &plain).unwrap().to_string().contains(CARD), "PostgreSQL plans without reading a row");
        // A write tool returns what a prepared read answers.
        let prepared = named("PREPARE leak AS SELECT * FROM $pay; EXECUTE leak", &p);
        *e.human.allow.lock() = Some(true);
        assert!(e.shown("execute", "pg-open", &prepared).contains(CARD), "the case proves nothing");
        assert!(e.call("execute", "pg-structure", &prepared).unwrap_err().contains("structure only"));
        *e.human.allow.lock() = None;
        // Quoted, the same option: it runs the statement.
        for sql in ["EXPLAIN (\"analyze\") SELECT * FROM $pay", "EXPLAIN (\"analyze\" \"true\") SELECT * FROM $pay", "EXPLAIN (U&\"analyze\") SELECT * FROM $pay", "EXPLAIN (ANALYZE) SELECT * FROM $pay"] {
            let sql = named(sql, &p);
            assert!(e.shown("run_query", "pg-open", &sql).contains("actual"), "{sql} does not analyze: the case proves nothing");
            assert!(e.call("run_query", "pg-structure", &sql).is_err(), "{sql}");
        }
        e.unseed(Pg, &p);
    }
    for engine in [Lite, Duck] {
        e.seed(engine, &p);
        let conn = format!("{}-structure", engine.id());
        let told = e.call("run_query", &conn, &named("EXPLAIN SELECT (SELECT card FROM $pay LIMIT 1)", &p)).unwrap().to_string();
        assert!(!told.contains(CARD), "{engine:?}: {told}");
        assert!(e.call("run_query", &conn, &named("EXPLAIN ANALYZE SELECT * FROM $pay", &p)).is_err(), "{engine:?}");
        assert!(e.call("run_query", &conn, &named("SELECT * FROM $pay", &p)).is_err(), "{engine:?}");
        e.unseed(engine, &p);
    }
}

#[test]
fn a_star_in_a_call_carries_nothing_kept() {
    if !Ch.up() {
        return;
    }
    let p = format!("wc{}", std::process::id());
    let e = env(&p);
    e.seed(Ch, &p);
    for sql in ["tuple(*)", "concat(*)", "formatRow('CSV', *)", "tuple($cust.*)", "tuple(COLUMNS('.*'))", "COLUMNS('.*') APPLY(toString)", "(*, 1)", "* APPLY(toString)", "* REPLACE(upper(email) AS email)", "$cust.* APPLY(toString)", "uniq(*)"] {
        let sql = named(&format!("SELECT {sql} FROM $cust"), &p);
        let bare = e.shown("run_query", "ch-open", &sql);
        let told = e.call("run_query", "ch-masked", &sql).expect_err(&sql);
        assert!(!told.contains(EMAIL), "{sql}: {told}\n(without rules: {bare})");
    }
    // Copied into a table: refused before anything is asked, and nothing is made.
    *e.human.allow.lock() = Some(true);
    for (conn, select) in [("ch", "tuple(*) AS t FROM $users"), ("ch", "COLUMNS('.*') FROM $users"), ("ch", "* REPLACE(toString(id) AS id) FROM $users"), ("ch", "* EXCEPT(id) FROM $users"), ("ch-masked", "tuple(*) AS t FROM $cust")] {
        let sql = named(&format!("CREATE TABLE {p}.copy ENGINE = Memory AS SELECT {select}"), &p);
        assert!(e.call("execute", conn, &sql).is_err(), "{sql}");
        assert!(!e.shown("run_query", "ch-open", &format!("EXISTS TABLE {p}.copy")).contains("[[1"), "{sql}");
    }
    *e.human.allow.lock() = None;
    let read = |conn: &str, sql: &str| e.call("run_query", conn, &named(sql, &p)).unwrap_or_else(|err| panic!("{sql}: {err}"));
    assert!(read("ch-masked", "SELECT count(*) AS k FROM $cust")["rows"].to_string().contains('1'));
    assert!(!read("ch-masked", "SELECT * FROM $cust")["rows"].to_string().contains(EMAIL), "masked by its name");
    assert!(read("ch", "SELECT * EXCEPT(id) FROM $open")["rows"].to_string().contains("plain"));
    assert!(read("ch", "SELECT 2 * id AS twice FROM $open")["rows"].to_string().contains("14"));
    assert!(read("ch", "SELECT tuple(*) AS t FROM $open")["rows"].to_string().contains("plain"), "nothing of that table is kept");
    e.unseed(Ch, &p);
}

#[test]
fn a_function_named_with_quotes_is_the_function() {
    if !Ch.up() {
        return;
    }
    let p = format!("wf{}", std::process::id());
    let e = env(&p);
    e.seed(Ch, &p);
    let right = format!("(toUInt64(1), '{CARD}')");
    for f in ["\"in\"", "`in`", "`i\\x6E`", "“in”", "`globalI\\x6E`", "\"notIn\""] {
        let sql = named(&format!("SELECT {f}({right}, $pay)"), &p);
        assert!(e.call("run_query", "ch", &sql).unwrap_err().contains("IN with a table"), "{sql}");
    }
    e.unseed(Ch, &p);
}

#[test]
fn postgres_names_written_in_escapes_are_read() {
    if !Pg.up() {
        return;
    }
    let p = format!("wu{}", std::process::id());
    let e = env(&p);
    for sql in ["SELECT U&\"pg_terminate_backend\"(0)", "SELECT U&\"pg\\005Fterminate_backend\"(0)", "SELECT U&\"pg_read_file\"('x')", "SELECT U&\"query_to_xml\"('select 1', true, true, '')"] {
        assert!(e.call("run_query", "pg-open", sql).is_err_and(|err| err.contains("effects") || err.contains("runs SQL") || err.contains("files")), "{sql}");
    }
}

/// A statement that never closes is a refusal, with or without a server to refuse it.
#[test]
fn what_never_closes_is_refused() {
    let p = format!("wn{}", std::process::id());
    let e = env(&p);
    for engine in ALL.into_iter().filter(|x| x.up()) {
        e.seed(engine, &p);
        for sql in ["SELECT * FROM numbers(", "SELECT * FROM generate_series(1, (", "SELECT * FROM (SELECT 1", "WITH a AS (SELECT 1"] {
            assert!(e.call("run_query", engine.id(), sql).is_err(), "{engine:?}: {sql}");
        }
        e.unseed(engine, &p);
    }
}
