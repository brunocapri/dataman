//! The tools against real servers (skipped when one does not answer):
//!
//! - `DATAMAN_PG`  (default `127.0.0.1:55432`, user `postgres`, password `dataman`, db `dataman`)
//! - `DATAMAN_LAR` (default `127.0.0.1:5490`, user `lar`, db `lar`, no password)
//! - `DATAMAN_MYSQL` (default `127.0.0.1:33306`, root, `DATAMAN_MYSQL_PASSWORD`)
//! - `DATAMAN_MSSQL` (default `127.0.0.1:14330`, sa, `DATAMAN_MSSQL_PASSWORD`)
//! - `DATAMAN_MONGO` (default `127.0.0.1:27027`, root, `DATAMAN_MONGO_PASSWORD`)
//! - `DATAMAN_ES` (default `127.0.0.1:9201`, elastic, `DATAMAN_ES_PASSWORD`)
//! - `DATAMAN_REDIS` (default `127.0.0.1:63790`, `DATAMAN_REDIS_PASSWORD`)
//!
//! PostgreSQL work happens in a scratch schema, LAR's and SQL Server's in scratch tables (SQL
//! Server's in tempdb), MySQL's in a scratch database; all are dropped. The app is played by a
//! [`Front`] whose answers each test scripts (Allow, Deny, no answer): what a human would do.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dataman_core::{Dirs, MemorySecrets, SecretStore};
use dataman_mcp::{Approval, Front, Hub, Registry, Server};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn addr(var: &str, default: &str) -> (String, u16) {
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (h, p) = v.rsplit_once(':').unwrap();
    (h.into(), p.parse().unwrap())
}

fn up(host: &str, port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&format!("{host}:{port}").parse().unwrap(), Duration::from_secs(1)).is_ok()
}

/// What the human does with the next approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Allow,
    Deny,
    /// Never answers: the hub's timeout denies it.
    Never,
}

/// The app's side as a script: answers in order (Deny when none is left), every question kept.
#[derive(Default)]
struct Human {
    answers: Mutex<VecDeque<Answer>>,
    asked: Mutex<Vec<Approval>>,
}

impl Front for Human {
    fn approve(&self, a: Approval) -> BoxFuture<'static, bool> {
        self.asked.lock().push(a);
        let ans = self.answers.lock().pop_front().unwrap_or(Answer::Deny);
        Box::pin(async move {
            match ans {
                Answer::Allow => true,
                Answer::Deny => false,
                Answer::Never => std::future::pending().await,
            }
        })
    }
}

struct Env {
    server: Server,
    human: Arc<Human>,
    dir: tempfile::TempDir,
}

/// A config with PostgreSQL as `pg` (read), `pgw` (write), `pgf` (full), `pgprod` (production,
/// full), LAR as `lar` / `larw` / `larf`, MySQL as `myw` / `myf`, SQL Server as `msw` / `msf`.
fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("dataman");
    std::fs::create_dir_all(&cfg).unwrap();
    let (ph, pp) = addr("DATAMAN_PG", "127.0.0.1:55432");
    let (lh, lp) = addr("DATAMAN_LAR", "127.0.0.1:5490");
    let (mh, mp) = addr("DATAMAN_MYSQL", "127.0.0.1:33306");
    let (sh, sp) = addr("DATAMAN_MSSQL", "127.0.0.1:14330");
    let (gh, gp) = addr("DATAMAN_MONGO", "127.0.0.1:27027");
    let (eh, ep) = addr("DATAMAN_ES", "127.0.0.1:9201");
    let (rh, rp) = addr("DATAMAN_REDIS", "127.0.0.1:63790");
    let conn = |id: &str, host: &str, port: u16, user: &str, db: &str, extra: &str| {
        format!("[[connection]]\nid = \"{id}\"\nname = \"{id}\"\nhost = \"{host}\"\nport = {port}\nuser = \"{user}\"\ndatabase = \"{db}\"\nsslmode = \"disable\"\n{extra}\n")
    };
    let my = "engine = \"mysql\"\nsave_password = true\n";
    let ms = "engine = \"sql-server\"\nsave_password = true\n";
    let text = [
        conn("pg", &ph, pp, "postgres", "dataman", ""),
        conn("pgw", &ph, pp, "postgres", "dataman", "mcp = \"write\""),
        conn("pgf", &ph, pp, "postgres", "dataman", "mcp = \"full\""),
        conn("pgprod", &ph, pp, "postgres", "dataman", "env = \"production\"\nmcp = \"full\""),
        conn("lar", &lh, lp, "lar", "lar", ""),
        conn("larw", &lh, lp, "lar", "lar", "mcp = \"write\""),
        conn("larf", &lh, lp, "lar", "lar", "mcp = \"full\""),
        conn("myw", &mh, mp, "root", "mysql", &format!("{my}mcp = \"write\"")),
        conn("myf", &mh, mp, "root", "mysql", &format!("{my}mcp = \"full\"")),
        conn("msw", &sh, sp, "sa", "tempdb", &format!("{ms}mcp = \"write\"\nmcp_row_limit = 3")).replace("sslmode = \"disable\"", "sslmode = \"require\""),
        conn("msf", &sh, sp, "sa", "tempdb", &format!("{ms}mcp = \"full\"")).replace("sslmode = \"disable\"", "sslmode = \"require\""),
        conn("mg", &gh, gp, "root", "mcp_live", "engine = \"mongodb\"\nsave_password = true"),
        conn("mgw", &gh, gp, "root", "mcp_live", "engine = \"mongodb\"\nsave_password = true\nmcp = \"write\"\nmcp_row_limit = 3"),
        conn("mgf", &gh, gp, "root", "mcp_live", "engine = \"mongodb\"\nsave_password = true\nmcp = \"full\""),
        conn("mgprod", &gh, gp, "root", "mcp_live", "engine = \"mongodb\"\nsave_password = true\nenv = \"production\"\nmcp = \"full\""),
        conn("es", &eh, ep, "elastic", "", "engine = \"elasticsearch\"\nsave_password = true"),
        conn("esw", &eh, ep, "elastic", "", "engine = \"elasticsearch\"\nsave_password = true\nmcp = \"write\"\nmcp_row_limit = 3"),
        conn("esf", &eh, ep, "elastic", "", "engine = \"elasticsearch\"\nsave_password = true\nmcp = \"full\""),
        conn("rd", &rh, rp, "", "8", "engine = \"redis\"\nsave_password = true"),
        conn("rdw", &rh, rp, "", "8", "engine = \"redis\"\nsave_password = true\nmcp = \"write\"\nmcp_row_limit = 3"),
        conn("rdf", &rh, rp, "", "8", "engine = \"redis\"\nsave_password = true\nmcp = \"full\""),
    ]
    .concat();
    std::fs::write(cfg.join("connections.toml"), text).unwrap();
    let pgpass = dir.path().join("pgpass");
    std::fs::write(&pgpass, format!("{ph}:{pp}:*:postgres:dataman\n")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&pgpass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let my_pw = std::env::var("DATAMAN_MYSQL_PASSWORD").unwrap_or_else(|_| "dataman-root".into());
    let ms_pw = std::env::var("DATAMAN_MSSQL_PASSWORD").unwrap_or_else(|_| "Dataman-sa-1!".into());
    let mg_pw = std::env::var("DATAMAN_MONGO_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let es_pw = std::env::var("DATAMAN_ES_PASSWORD").unwrap_or_else(|_| "dataman".into());
    for id in ["es", "esw", "esf"] {
        secrets.set(id, &es_pw).unwrap();
    }
    let rd_pw = std::env::var("DATAMAN_REDIS_PASSWORD").unwrap_or_else(|_| "dataman".into());
    for id in ["rd", "rdw", "rdf"] {
        secrets.set(id, &rd_pw).unwrap();
    }
    for (id, pw) in [("myw", &my_pw), ("myf", &my_pw), ("msw", &ms_pw), ("msf", &ms_pw), ("mg", &mg_pw), ("mgw", &mg_pw), ("mgf", &mg_pw), ("mgprod", &mg_pw)] {
        secrets.set(id, pw).unwrap();
    }
    let hub = Hub::new(Dirs::at(&cfg), secrets).unwrap();
    // The hub's core reads pgpass at construction from PGPASSFILE; point it here instead.
    let hub = {
        let mut h = Arc::try_unwrap(hub).ok().unwrap();
        h.core.pgpass = Some(pgpass);
        Arc::new(h)
    };
    let human = Arc::new(Human::default());
    hub.set_front(Some(human.clone()));
    hub.set_approval_timeout(Duration::from_millis(600));
    let reg = Arc::new(Registry::standard(Some(cfg.join("mcp-tools.toml"))));
    Env { server: Server::new(hub, reg, "stdio"), human, dir }
}

impl Env {
    fn call_ct(&self, tool: &str, args: Value, ct: CancellationToken) -> Result<Value, String> {
        let t = self.server.registry.find(tool).unwrap_or_else(|| panic!("no tool {tool}"));
        let r = self.server.hub.core.block_on(self.server.run_tool(t, args.as_object().unwrap().clone(), "live-test".into(), ct));
        let text = match &r.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("{other:?}"),
        };
        if r.is_error == Some(true) { Err(text) } else { Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))) }
    }

    fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        self.call_ct(tool, args, CancellationToken::new())
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        self.call(tool, args.clone()).unwrap_or_else(|e| panic!("{tool} {args}: {e}"))
    }

    fn err(&self, tool: &str, args: Value) -> String {
        match self.call(tool, args.clone()) {
            Ok(v) => panic!("{tool} {args} should fail: {v}"),
            Err(e) => e,
        }
    }

    /// The next answers the human gives.
    fn answer(&self, a: &[Answer]) {
        self.human.answers.lock().extend(a.iter().copied());
    }

    /// The questions asked since the last call.
    fn asked(&self) -> Vec<Approval> {
        std::mem::take(&mut *self.human.asked.lock())
    }

    fn audit(&self) -> Vec<Value> {
        let t = std::fs::read_to_string(self.dir.path().join("dataman/mcp-audit.jsonl")).unwrap_or_default();
        t.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn count(&self, conn: &str, sql: &str) -> Value {
        self.ok("run_query", json!({"connection": conn, "sql": sql}))["rows"][0][0].clone()
    }
}

fn leak(e: Env) {
    // The hub's runtime may still run tasks that hold it: never drop it from one of them.
    e.server.hub.close_all();
    let Env { server, dir, .. } = e;
    std::mem::forget(server);
    drop(dir);
}

fn with(conn: &str, extra: Value) -> Value {
    let mut v = json!({"connection": conn});
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v
}

#[test]
fn postgres_tools() {
    let (h, p) = addr("DATAMAN_PG", "127.0.0.1:55432");
    if !up(&h, p) {
        eprintln!("skipping: no PostgreSQL at {h}:{p}");
        return;
    }
    use Answer::*;
    let e = env();
    let s = format!("mcp_live_{}", std::process::id());
    let c = |x: Value| with("pg", x);
    let w = |x: Value| with("pgw", x);
    let f = |x: Value| with("pgf", x);

    // DDL: refused in read and write, asked about first in full (no preview: it would hold its
    // locks), then run in one transaction.
    assert!(e.err("execute", c(json!({"sql": format!("CREATE SCHEMA {s}")}))).contains("reads only"));
    let err = e.err("execute", w(json!({"sql": format!("CREATE SCHEMA {s}")})));
    assert!(err.contains("destructive") && err.contains("full"), "{err}");
    assert!(e.asked().is_empty(), "write never asks about what it refuses");
    let ddl = format!(
        "CREATE SCHEMA {s}; \
         CREATE TABLE {s}.customer (id int PRIMARY KEY, name text NOT NULL, note text); \
         CREATE TABLE {s}.orders (id int PRIMARY KEY, customer_id int NOT NULL REFERENCES {s}.customer(id), total numeric(10,2) CHECK (total >= 0), data jsonb); \
         CREATE INDEX orders_customer ON {s}.orders (customer_id)"
    );
    e.answer(&[Deny]);
    assert!(e.err("execute", f(json!({"sql": ddl}))).contains("denied in Dataman"));
    let q = e.asked();
    assert_eq!(q.len(), 1);
    assert_eq!((q[0].reasons.clone(), q[0].affected, q[0].connection.as_str(), q[0].tool.as_str()), (vec!["CREATE SCHEMA".to_string(), "CREATE TABLE".into(), "CREATE INDEX".into()], None, "pgf", "execute"));
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM pg_namespace WHERE nspname = '{s}'")), json!(0), "denied before it ran");
    e.answer(&[Allow]);
    let r = e.ok("execute", f(json!({"sql": ddl})));
    assert_eq!((r["committed"].as_bool(), r["statements"].as_array().unwrap().len()), (Some(true), 4));
    assert_eq!(r["approved"]["reasons"][0], "CREATE SCHEMA");
    assert_eq!(e.asked().len(), 1);

    // Safe writes in the write tier: committed without a question, RETURNING read.
    let r = e.ok("execute", w(json!({"sql": format!("INSERT INTO {s}.customer VALUES (1, 'Alice', ''), (2, 'Bob', NULL) RETURNING id")})));
    assert_eq!(r["statements"][0], json!(["INSERT", 2]));
    assert_eq!(r["returning"]["rows"], json!([[1], [2]]));
    assert!(e.asked().is_empty());
    // Over the row limit (100): held, asked; Deny rolls back, Allow commits.
    let fill = format!("INSERT INTO {s}.orders SELECT g, 1 + g % 2, g * 1.5, jsonb_build_object('g', g) FROM generate_series(1, 500) g");
    e.answer(&[Deny]);
    let err = e.err("execute", w(json!({"sql": fill})));
    assert!(err.contains("denied") && err.contains("500 rows (limit 100)") && err.contains("rolled back"), "{err}");
    let q = e.asked();
    assert_eq!((q[0].affected, q[0].reasons.clone()), (Some(500), vec!["500 rows (limit 100)".to_string()]));
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM {s}.orders")), json!(0), "rolled back");
    e.answer(&[Allow]);
    e.ok("execute", w(json!({"sql": fill})));
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM {s}.orders")), json!(500), "committed after Allow");
    assert_eq!(e.asked().len(), 1);
    // A failing script leaves nothing behind.
    let err = e.err("execute", w(json!({"sql": format!("INSERT INTO {s}.customer VALUES (3, 'C', NULL); INSERT INTO {s}.customer VALUES (1, 'dup', NULL)")})));
    assert!(err.contains("23505"), "{err}");
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM {s}.customer")), json!(2), "rolled back as a whole");

    // Destructive DML: refused in write whatever the agent says; held and asked in full.
    for sql in [format!("UPDATE {s}.customer SET note = 'x'"), format!("UPDATE {s}.customer SET note = 'x' WHERE 1 = 1"), format!("UPDATE {s}.customer SET note = 'x' WHERE id = id")] {
        let err = e.err("execute", w(json!({"sql": sql, "confirm": true})));
        assert!(err.contains("destructive"), "{sql}: {err}");
    }
    e.answer(&[Deny]);
    assert!(e.err("execute", f(json!({"sql": format!("UPDATE {s}.customer SET note = 'x' RETURNING id, note")}))).contains("rolled back"));
    let q = e.asked();
    assert_eq!((q[0].reasons.clone(), q[0].affected), (vec!["UPDATE without WHERE".to_string()], Some(2)));
    assert_eq!(q[0].preview.as_ref().map(|(c, r)| (c.clone(), r.len())), Some((vec!["id".to_string(), "note".into()], 2)), "the RETURNING rows");
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM {s}.customer WHERE note = 'x'")), json!(0));
    // No answer in time: rolled back, a timeout for the agent.
    e.answer(&[Never]);
    let t = Instant::now();
    let err = e.err("execute", f(json!({"sql": format!("DELETE FROM {s}.orders")})));
    assert!(err.contains("no answer") && err.contains("rolled back"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM {s}.orders")), json!(500), "the timeout rolled back");
    assert_eq!(e.asked()[0].affected, Some(500));
    e.answer(&[Allow]);
    let r = e.ok("execute", f(json!({"sql": format!("UPDATE {s}.customer SET note = 'x'")})));
    assert_eq!(r["statements"][0], json!(["UPDATE", 2]));
    e.asked();
    // The app gone (stdio without it): anything needing a human is refused, never run.
    e.server.hub.set_front(None);
    let err = e.err("execute", f(json!({"sql": format!("DELETE FROM {s}.orders")})));
    assert!(err.contains("needs approval in Dataman; open the app"), "{err}");
    let err = e.err("execute", w(json!({"sql": format!("UPDATE {s}.orders SET total = total WHERE id > 0")})));
    assert!(err.contains("500 rows (limit 100)") && err.contains("open the app") && err.contains("rolled back"), "{err}");
    assert_eq!(e.ok("execute", w(json!({"sql": format!("UPDATE {s}.orders SET total = total WHERE id <= 3")})))["statements"][0], json!(["UPDATE", 3]), "within the limit it runs");
    e.server.hub.set_front(Some(e.human.clone()));
    // Production: every write asks, even a safe one in full.
    e.answer(&[Allow]);
    let prod = json!({"connection": "pgprod", "sql": format!("UPDATE {s}.customer SET note = 'y' WHERE id = 1")});
    assert_eq!(e.ok("execute", prod)["statements"][0], json!(["UPDATE", 1]));
    assert_eq!(e.asked()[0].reasons, vec!["production".to_string()]);

    // run_query: NULL is not "", types, params, limits.
    let r = e.ok("run_query", c(json!({"sql": format!("SELECT id, name, note, NULL::text AS n2 FROM {s}.customer WHERE id = $1"), "params": [2]})));
    assert_eq!(r["columns"], json!([["id", "int4"], ["name", "text"], ["note", "text"], ["n2", "text"]]));
    assert_eq!(r["rows"], json!([[2, "Bob", "x", null]]));
    e.ok("execute", w(json!({"sql": format!("UPDATE {s}.customer SET note = '' WHERE id = 1")})));
    let r = e.ok("run_query", c(json!({"sql": format!("SELECT note FROM {s}.customer ORDER BY id")})));
    assert_eq!(r["rows"], json!([[""], ["x"]]), "an empty string stays a string");
    let r = e.ok("run_query", c(json!({"sql": format!("SELECT id, total, data FROM {s}.orders ORDER BY id"), "max_rows": 3})));
    assert_eq!(r["row_count"], 3);
    assert_eq!(r["truncated"], "rows");
    assert_eq!(r["rows"][0], json!([1, "1.50", {"g": 1}]), "numeric exact as text, jsonb embedded");
    let r = e.ok("run_query", c(json!({"sql": "SELECT g FROM generate_series(1, 5000) g"})));
    assert_eq!((r["row_count"].as_u64(), r["truncated"].as_str()), (Some(200), Some("rows")), "200 rows by default");
    let r = e.ok("run_query", c(json!({"sql": "SELECT g FROM generate_series(1, 5000) g", "max_rows": 100000})));
    assert_eq!(r["row_count"], 1000, "the settings' cap (1000) wins");
    let r = e.ok("run_query", c(json!({"sql": "SELECT repeat('x', 3000) AS big, g FROM generate_series(1, 200) g"})));
    assert_eq!(r["truncated"], "bytes", "the byte cap");
    assert!(r["rows"][0][0].as_str().unwrap().ends_with('…') && r["cut_cells"].as_u64().unwrap() > 0);
    // open_query without tabs (no app tabs here): a read as run_query, a write as execute.
    let r = e.ok("open_query", c(json!({"sql": format!("SELECT count(*) FROM {s}.orders"), "title": "orders"})));
    assert_eq!((r["rows"].clone(), r["in_tab"].as_bool()), (json!([[500]]), Some(false)));
    assert!(e.err("open_query", c(json!({"sql": format!("DELETE FROM {s}.orders WHERE id = 1")}))).contains("reads only"));
    assert_eq!(e.ok("open_query", w(json!({"sql": format!("DELETE FROM {s}.orders WHERE id = 500")})))["statements"][0], json!(["DELETE", 1]));

    // Read-only, three ways: the client check, PostgreSQL's READ ONLY, and nothing left behind.
    assert!(e.err("run_query", c(json!({"sql": format!("DELETE FROM {s}.orders")}))).contains("not a read"));
    assert!(e.err("run_query", c(json!({"sql": "SELECT pg_terminate_backend(1)"}))).contains("effects"));
    e.answer(&[Allow]);
    e.ok("execute", f(json!({"sql": format!("CREATE FUNCTION {s}.sneaky() RETURNS int LANGUAGE sql AS $$ DELETE FROM {s}.orders RETURNING 1 $$")})));
    e.asked();
    let err = e.err("run_query", c(json!({"sql": format!("SELECT {s}.sneaky()")})));
    assert!(err.contains("25006") || err.contains("read-only"), "{err}");
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM {s}.orders")), json!(499));

    // Time limit and cancellation: the statement stops on the server.
    let t = Instant::now();
    let err = e.err("run_query", c(json!({"sql": "SELECT pg_sleep(10)", "timeout_ms": 300})));
    assert!(err.contains("57014") || err.contains("timed out"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    let ct = CancellationToken::new();
    let ct2 = ct.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        ct2.cancel();
    });
    let t = Instant::now();
    let err = e.call_ct("run_query", c(json!({"sql": "SELECT pg_sleep(10)"})), ct).unwrap_err();
    assert_eq!(err, "cancelled");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    std::thread::sleep(Duration::from_millis(300));
    let act = e.ok("run_query", c(json!({"sql": "SELECT count(*) FROM pg_stat_activity WHERE state = 'active' AND query = 'SELECT pg_sleep(10)'"})));
    assert_eq!(act["rows"], json!([[0]]), "the cancelled sleeps stopped on the server");
    let act = e.ok("activity_sessions", json!({"connection": "pg"}));
    assert_eq!(act["columns"][0], "pid");
    // A cancel while a held write waits for its answer rolls it back.
    e.answer(&[Never]);
    let ct = CancellationToken::new();
    let ct2 = ct.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        ct2.cancel();
    });
    assert_eq!(e.call_ct("execute", f(json!({"sql": format!("DELETE FROM {s}.orders")})), ct).unwrap_err(), "cancelled");
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM {s}.orders")), json!(499));
    e.asked();

    // Concurrency: two statements per connection at once.
    let t = Instant::now();
    std::thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| e.ok("run_query", c(json!({"sql": "SELECT pg_sleep(0.3)"}))));
        }
    });
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(590) && took < Duration::from_secs(3), "4 sleeps of 0.3 s through 2 slots: {took:?}");

    // The catalog.
    let r = e.ok("list_tables", c(json!({"schema": s})));
    assert_eq!(r["rows"].as_array().unwrap().iter().map(|r| r[1].as_str().unwrap()).collect::<Vec<_>>(), ["customer", "orders"]);
    let r = e.ok("list_schemas", c(json!({})));
    assert!(r["schemas"]["rows"].as_array().unwrap().iter().any(|x| x[0] == json!(s) && x[1] == json!(2)), "{r}");
    let d = e.ok("describe_table", c(json!({"table": format!("{s}.orders"), "include_ddl": true})));
    assert_eq!(d["primary_key"], json!(["id"]));
    assert_eq!(d["columns"][1], json!({"name": "customer_id", "type": "integer", "not_null": true}));
    assert!(d["foreign_keys"][0][1].as_str().unwrap().contains("REFERENCES"), "{d}");
    assert!(d["indexes"].as_array().unwrap().iter().any(|i| i[0] == "orders_customer"), "{d}");
    assert!(d["constraints"][0][1].as_str().unwrap().contains("CHECK"), "{d}");
    assert!(d["ddl"].as_str().unwrap().contains("CREATE TABLE"), "{d}");
    let d = e.ok("describe_table", c(json!({"table": format!("{s}.customer")})));
    assert!(d["referenced_by"][0][1].as_str().unwrap().ends_with(".orders"), "{d}");
    let r = e.ok("search_objects", c(json!({"pattern": "customer", "kinds": ["table", "column"], "limit": 500})));
    let found: Vec<(String, String)> = r["rows"].as_array().unwrap().iter().filter(|x| x[1] == json!(s)).map(|x| (x[0].as_str().unwrap().into(), x[2].as_str().unwrap().into())).collect();
    assert!(found.contains(&("table".into(), "customer".into())) && found.contains(&("column".into(), "orders.customer_id".into())), "{found:?}");
    let r = e.ok("search_objects", c(json!({"pattern": "sneak", "kinds": ["function"]})));
    assert_eq!(r["rows"][0][0], "function", "{r}");
    let r = e.ok("get_ddl", c(json!({"object": format!("{s}.sneaky")})));
    assert!(r["ddl"].as_str().unwrap().contains("CREATE OR REPLACE FUNCTION"), "{r}");
    let r = e.ok("sample_rows", c(json!({"table": format!("{s}.orders"), "n": 3})));
    assert_eq!(r["row_count"], 3);

    // Plans: explain never runs; explain_analyze of a write follows the tiers and is rolled back.
    let r = e.ok("explain", c(json!({"sql": format!("DELETE FROM {s}.orders WHERE id = 1")})));
    assert!(r["plan"].as_str().unwrap().contains("Delete on"), "{r}");
    let r = e.ok("explain_analyze", c(json!({"sql": format!("SELECT * FROM {s}.orders WHERE total > 100")})));
    assert!(r["plan"].as_str().unwrap().contains("actual time") && r["execution_ms"].is_number(), "{r}");
    assert!(e.err("explain_analyze", c(json!({"sql": format!("DELETE FROM {s}.orders WHERE id < 100")}))).contains("reads only"));
    let r = e.ok("explain_analyze", w(json!({"sql": format!("DELETE FROM {s}.orders WHERE id < 200"), "format": "json"})));
    assert_eq!(r["rolled_back"], true);
    assert!(r["plan"].is_array(), "{r}");
    assert!(e.asked().is_empty(), "rolled back: no row limit");
    assert!(e.err("explain_analyze", w(json!({"sql": format!("DELETE FROM {s}.orders")}))).contains("destructive"));
    assert_eq!(e.count("pg", &format!("SELECT count(*) FROM {s}.orders")), json!(499), "rolled back");

    // Resources: the snapshot knows the scratch tables.
    let call = dataman_mcp::Call::new(e.server.hub.clone(), "t".into(), "stdio", "resources/read", CancellationToken::new());
    let u = dataman_mcp::resources::parse("dataman://pg/dataman/schema?refresh=true").unwrap();
    let md = e.server.hub.core.block_on(dataman_mcp::resources::read(&call, &u)).unwrap();
    assert!(md.contains(&format!("## {s}")) && md.contains("- orders [table") && md.contains("customer_id integer NOT NULL"), "{md}");
    let u = dataman_mcp::resources::parse("dataman://pg/dataman/schema.json").unwrap();
    let js: Value = serde_json::from_str(&e.server.hub.core.block_on(dataman_mcp::resources::read(&call, &u)).unwrap()).unwrap();
    assert_eq!(js["schemas"][&s]["orders"]["columns"][0], json!(["id", "integer", "pk", "not_null"]));

    // User tools, bound parameters; a write template follows the tiers like execute.
    std::fs::write(
        e.dir.path().join("dataman/mcp-tools.toml"),
        format!(
            "[[tool]]\nname = \"orders_of\"\ndescription = \"Orders of a customer.\"\nconnection = \"pg\"\nsql = \"SELECT id FROM {s}.orders WHERE customer_id = $1 AND $2::text IS NOT NULL ORDER BY id LIMIT 3\"\n\
             [[tool.param]]\nname = \"customer\"\ntype = \"int\"\n[[tool.param]]\nname = \"x\"\ntype = \"string\"\n\
             [[tool]]\nname = \"wipe\"\ndescription = \"Deletes every order.\"\nconnection = \"pgw\"\nmode = \"write\"\nsql = \"DELETE FROM {s}.orders\"\n"
        ),
    )
    .unwrap();
    let r = e.ok("orders_of", json!({"customer": 2, "x": "'; DROP TABLE x; --"}));
    assert_eq!(r["rows"], json!([[1], [3], [5]]));
    assert!(e.err("wipe", json!({"confirm": true})).contains("destructive"));

    // Activity and signals.
    assert!(e.err("terminate_backend", json!({"connection": "pg", "pid": 1})).contains("reads only"));
    assert!(e.err("terminate_backend", json!({"connection": "pgw", "pid": 1})).contains("destructive"));
    e.answer(&[Deny]);
    assert!(e.err("terminate_backend", json!({"connection": "pgprod", "pid": 1, "confirm": true})).contains("denied"));
    assert_eq!(e.asked()[0].reasons, vec!["terminate a session".to_string(), "production".into()]);

    // Clean up, and the audit trail of it all.
    e.answer(&[Allow]);
    e.ok("execute", f(json!({"sql": format!("DROP SCHEMA {s} CASCADE")})));
    let log = e.audit();
    assert!(log.len() > 50, "{}", log.len());
    let drop = log.iter().rev().find(|l| l["tool"] == "execute").unwrap();
    assert_eq!((drop["outcome"].as_str(), drop["connection"].as_str()), (Some("ok"), Some("pgf")));
    assert_eq!(drop["approval"]["decision"], "approved");
    assert!(drop["approval"]["by"].as_str().is_some_and(|b| !b.is_empty()));
    for d in ["approved", "denied", "timeout", "unavailable"] {
        assert!(log.iter().any(|l| l["approval"]["decision"] == d), "no {d} in the audit");
    }
    let limit = log.iter().find(|l| l["approval"]["reasons"][0] == "500 rows (limit 100)").unwrap();
    assert_eq!((limit["approval"]["rows"].as_u64(), limit["outcome"].as_str()), (Some(500), Some("refused")));
    assert!(log.iter().any(|l| l["outcome"] == "timeout") && log.iter().any(|l| l["outcome"] == "cancelled"));
    assert!(log.iter().all(|l| !l["sql"].as_str().unwrap_or_default().contains("Alice")), "literals are redacted");
    leak(e);
}

/// Several reads in one call (run_query, open_query headless): every result comes back, in
/// order, the row cap for each and the byte cap for all; a write anywhere refuses them all. One
/// statement keeps its own shape.
#[test]
fn several_reads() {
    let e = env();
    let (h, p) = addr("DATAMAN_PG", "127.0.0.1:55432");
    if up(&h, p) {
        let pg = |x: Value| with("pg", x);
        let r = e.ok("run_query", pg(json!({"sql": "SELECT 1 AS a;\nSELECT 'x' AS b, 2 AS c;\n-- five rows\nSELECT generate_series(1, 5) AS n;\nVALUES (true)", "max_rows": 3})));
        assert_eq!(r["statements"], json!(4), "{r}");
        let res = r["results"].as_array().unwrap();
        let shape: Vec<(Value, Value, Value)> = res.iter().map(|x| (x["statement"].clone(), x["sql"].clone(), x["rows"].clone())).collect();
        assert_eq!(
            shape,
            vec![
                (json!(1), json!("SELECT 1 AS a"), json!([[1]])),
                (json!(2), json!("SELECT 'x' AS b, 2 AS c"), json!([["x", 2]])),
                (json!(3), json!("SELECT generate_series(1, 5) AS n"), json!([[1], [2], [3]])),
                (json!(4), json!("VALUES (true)"), json!([[true]])),
            ]
        );
        assert_eq!((res[2]["truncated"].as_str(), res[1]["columns"].clone()), (Some("rows"), json!([["b", "text"], ["c", "int4"]])));
        assert!(r.get("truncated").is_none() && r["ms"].is_number());
        // One statement: the shape it always had.
        let r = e.ok("run_query", pg(json!({"sql": "SELECT 1 AS a;"})));
        assert_eq!((r["rows"].clone(), r.get("results")), (json!([[1]]), None));
        // open_query without the app: the same, headless.
        let r = e.ok("open_query", pg(json!({"sql": "SELECT 1 AS a; SELECT 2 AS b"})));
        assert_eq!((r["results"][1]["rows"].clone(), r["in_tab"].clone()), (json!([[2]]), json!(false)));
        // A write anywhere: refused before anything runs.
        let err = e.err("run_query", pg(json!({"sql": "SELECT 1; DELETE FROM pg_class WHERE false; SELECT 2"})));
        assert!(err.contains("statement 2") && err.contains("not a read"), "{err}");
        assert!(e.err("open_query", pg(json!({"sql": "SELECT 1; DELETE FROM pg_class WHERE false"}))).contains("reads only"));
        assert!(e.err("run_query", pg(json!({"sql": "SELECT $1::int; SELECT 2", "params": [1]}))).contains("single statement"));
        // ~256 KB in all: the second result is cut there, the third does not run.
        let big = "SELECT repeat('x', 1000) AS t FROM generate_series(1, 150)";
        let r = e.ok("run_query", pg(json!({"sql": format!("{big}; {big}; SELECT 3"), "max_rows": 1000})));
        let res = r["results"].as_array().unwrap();
        assert_eq!((res.len(), res[0]["row_count"].clone(), res[1]["truncated"].clone()), (2, json!(150), json!("bytes")), "{}", r["note"]);
        assert!(res[1]["row_count"].as_u64().unwrap() < 150);
        assert_eq!(r["truncated"], json!("bytes"));
        assert!(r["note"].as_str().unwrap().contains("statement 3 not returned"), "{}", r["note"]);
    } else {
        eprintln!("skipping PostgreSQL: none at {h}:{p}");
    }
    let (h, p) = addr("DATAMAN_MYSQL", "127.0.0.1:33306");
    if up(&h, p) {
        let r = e.ok("run_query", json!({"connection": "myw", "sql": "SELECT * FROM (SELECT 1 AS n UNION SELECT 2 UNION SELECT 3) x ORDER BY n; SELECT 'b' AS b; SHOW DATABASES LIKE 'mysql'", "max_rows": 2}));
        let res = r["results"].as_array().unwrap();
        assert_eq!(res.len(), 3, "{r}");
        assert_eq!((res[0]["rows"].clone(), res[0]["truncated"].clone()), (json!([[1], [2]]), json!("rows")));
        assert_eq!((res[1]["rows"].clone(), res[2]["rows"].clone()), (json!([["b"]]), json!([["mysql"]])));
        assert!(e.err("run_query", json!({"connection": "myw", "sql": "SELECT 1; UPDATE mysql.user SET x = 1 WHERE 0"})).contains("statement 2"));
    } else {
        eprintln!("skipping MySQL: none at {h}:{p}");
    }
    let (h, p) = addr("DATAMAN_MSSQL", "127.0.0.1:14330");
    if up(&h, p) {
        // Statements separated by ;, the row cap on the first.
        let r = e.ok("run_query", json!({"connection": "msw", "sql": "SELECT TOP 10 object_id FROM sys.objects ORDER BY object_id; SELECT 'b' AS b", "max_rows": 3}));
        let res = r["results"].as_array().unwrap();
        assert_eq!((res.len(), res[0]["row_count"].clone(), res[0]["truncated"].clone(), res[1]["rows"].clone()), (2, json!(3), json!("rows"), json!([["b"]])), "{r}");
        // One batch, three SELECTs without ;: three result sets of statement 1.
        let r = e.ok("run_query", json!({"connection": "msw", "sql": "SELECT 1 AS a\nSELECT 2 AS b\nSELECT 3 AS c"}));
        let res = r["results"].as_array().unwrap();
        let shape: Vec<(Value, Value, Value)> = res.iter().map(|x| (x["statement"].clone(), x["sql"].clone(), x["rows"].clone())).collect();
        assert_eq!(shape, vec![(json!(1), json!("SELECT 1 AS a"), json!([[1]])), (json!(1), json!("SELECT 2 AS b"), json!([[2]])), (json!(1), json!("SELECT 3 AS c"), json!([[3]]))], "{r}");
        // GO batches.
        let r = e.ok("run_query", json!({"connection": "msw", "sql": "SELECT 1 AS a\nGO\nSELECT 2 AS b\nGO"}));
        assert_eq!((r["statements"].clone(), r["results"][1]["rows"].clone()), (json!(2), json!([[2]])), "{r}");
        assert!(e.err("run_query", json!({"connection": "msw", "sql": "SELECT 1 AS a\nDELETE FROM sys.objects WHERE 1 = 0"})).contains("not a read"));
    } else {
        eprintln!("skipping SQL Server: none at {h}:{p}");
    }
    leak(e);
}

/// run_favorite: a favorite found by a name typed from memory runs as open_query would; an
/// unclear name lists the candidates and runs nothing.
#[test]
fn run_favorite_by_name() {
    let (h, p) = addr("DATAMAN_PG", "127.0.0.1:55432");
    if !up(&h, p) {
        eprintln!("skipping: no PostgreSQL at {h}:{p}");
        return;
    }
    let e = env();
    for (name, sql, conn) in [("Orders by day", "SELECT 1 AS day", Some("pg")), ("Orders by month", "SELECT 2 AS month", Some("pg")), ("Top customers", "SELECT 3 AS top;\nSELECT 4 AS more", Some("pg")), ("Loose one", "SELECT 5", None)] {
        let mut a = json!({"name": name, "sql": sql});
        if let Some(c) = conn {
            a["connection"] = json!(c);
        }
        e.ok("add_favorite", a);
    }
    assert!(e.ok("list_favorites", json!({}))["rows"].as_array().unwrap().len() == 4);
    // A typo, on the favorite's own connection: it runs, and says which favorite.
    let r = e.ok("run_favorite", json!({"name": "ordrs by dya"}));
    assert_eq!((r["favorite"]["name"].clone(), r["rows"].clone()), (json!("Orders by day"), json!([[1]])), "{r}");
    // Word order and case; several statements come back as results.
    let r = e.ok("run_favorite", json!({"name": "CUSTOMERS TOP"}));
    assert_eq!((r["favorite"]["name"].clone(), r["results"][1]["rows"].clone()), (json!("Top customers"), json!([[4]])), "{r}");
    // Ambiguous: the candidates, nothing run.
    let before = e.audit().len();
    let r = e.ok("run_favorite", json!({"name": "orders by"}));
    let names: Vec<&str> = r["candidates"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!((r["matched"].clone(), names), (json!(false), vec!["Orders by day", "Orders by month"]), "{r}");
    assert_eq!((r["candidates"][0]["connection"].clone(), r["candidates"][0]["sql"].clone()), (json!("pg"), json!("SELECT 1 AS day")));
    let r = e.ok("run_favorite", json!({"name": "inventory forecast"}));
    assert_eq!(r["matched"], json!(false));
    let log = e.audit();
    assert!(log[before..].iter().all(|l| l["sql"].is_null() && l["favorite"].is_null()), "nothing ran: {:?}", &log[before..]);
    // Another connection given; a favorite saved without one needs it.
    let r = e.ok("run_favorite", json!({"name": "Orders by month", "connection": "pgw"}));
    assert_eq!(r["rows"], json!([[2]]));
    assert!(e.err("run_favorite", json!({"name": "loose one"})).contains("pass connection"));
    assert_eq!(e.ok("run_favorite", json!({"name": "loose one", "connection": "pg"}))["rows"], json!([[5]]));
    // The audit names the tool and the favorite.
    let log = e.audit();
    let line = log.iter().find(|l| l["tool"] == "run_favorite" && l["favorite"] == "Orders by day").unwrap();
    assert_eq!((line["connection"].as_str(), line["sql"].as_str(), line["outcome"].as_str()), (Some("pg"), Some("SELECT ? AS day"), Some("ok")));
    leak(e);
}

#[test]
fn lar_tools() {
    let (h, p) = addr("DATAMAN_LAR", "127.0.0.1:5490");
    if !up(&h, p) {
        eprintln!("skipping: no LAR at {h}:{p}");
        return;
    }
    use Answer::*;
    let e = env();
    let t = format!("mcp_live_{}", std::process::id());
    let list = e.ok("list_connections", json!({"probe": true}));
    let lar = list["rows"].as_array().unwrap().iter().find(|r| r[0] == "lar").unwrap().clone();
    assert_eq!(lar[9], true, "is_lar: {lar}");

    // LAR accepts READ ONLY without enforcing it: Dataman refuses before sending, in every
    // shape a write can take.
    for sql in [
        format!("CREATE TABLE {t} (id bigint)"),
        format!("INSERT INTO {t} VALUES (1)"),
        format!("WITH x AS (DELETE FROM {t} RETURNING *) SELECT * FROM x"),
        format!("SELECT 1; DELETE FROM {t}"),
        format!("/* SELECT */ DELETE FROM {t}"),
        "SELECT lar_backup('/tmp/x')".to_string(),
    ] {
        let err = e.err("run_query", json!({"connection": "lar", "sql": sql}));
        assert!(err.contains("not a read") || err.contains("administrative") || err.contains("one statement"), "{sql}: {err}");
        let err = e.err("execute", json!({"connection": "lar", "sql": sql}));
        assert!(err.contains("reads only"), "{sql}: {err}");
    }
    let err = e.err("execute", json!({"connection": "larw", "sql": format!("CREATE TABLE {t} (id bigint)")}));
    assert!(err.contains("destructive"), "{err}");
    e.answer(&[Allow]);
    e.ok("execute", json!({"connection": "larf", "sql": format!("CREATE TABLE {t} (id bigint, name text)")}));
    assert_eq!(e.asked()[0].affected, None, "DDL is asked about before it runs");
    let r = e.ok("execute", json!({"connection": "larw", "sql": format!("INSERT INTO {t} VALUES (1, 'a'), (2, NULL), (3, '')")}));
    assert_eq!(r["statements"][0], json!(["INSERT", 3]));
    let r = e.ok("run_query", json!({"connection": "lar", "sql": format!("SELECT id, name FROM {t} ORDER BY id")}));
    assert_eq!(r["rows"], json!([[1, "a"], [2, null], [3, ""]]));
    // A held DELETE on LAR: counted, denied, rolled back (LAR rolls back DML).
    e.answer(&[Deny]);
    assert!(e.err("execute", json!({"connection": "larf", "sql": format!("DELETE FROM {t}")})).contains("rolled back"));
    assert_eq!(e.asked()[0].affected, Some(3));
    assert_eq!(e.count("lar", &format!("SELECT count(*) FROM {t}")), json!(3), "LAR rolled the DELETE back");
    // TRUNCATE is never previewed (LAR does not roll it back): asked first, denied, not run.
    e.answer(&[Deny]);
    assert!(e.err("execute", json!({"connection": "larf", "sql": format!("TRUNCATE {t}")})).contains("denied"));
    assert_eq!(e.asked()[0].affected, None);
    assert_eq!(e.count("lar", &format!("SELECT count(*) FROM {t}")), json!(3));
    let r = e.ok("run_query", json!({"connection": "lar", "sql": format!("SELECT id FROM {t} ORDER BY id"), "max_rows": 2}));
    assert_eq!((r["row_count"].as_u64(), r["truncated"].as_str()), (Some(2), Some("rows")));
    let r = e.ok("list_tables", json!({"connection": "lar"}));
    assert!(r["rows"].as_array().unwrap().iter().any(|x| x[1] == json!(t)), "{r}");
    let d = e.ok("describe_table", json!({"connection": "lar", "table": t}));
    assert_eq!(d["columns"][0]["name"], "id");
    let r = e.ok("search_objects", json!({"connection": "lar", "pattern": &t[..12]}));
    assert!(r["rows"].as_array().unwrap().iter().any(|x| x[0] == "table" && x[2] == json!(t)), "{r}");
    let r = e.ok("sample_rows", json!({"connection": "lar", "table": t, "n": 2}));
    assert_eq!(r["row_count"], 2);
    let call = dataman_mcp::Call::new(e.server.hub.clone(), "t".into(), "stdio", "resources/read", CancellationToken::new());
    let md = e.server.hub.core.block_on(dataman_mcp::resources::read(&call, &dataman_mcp::resources::parse("dataman://lar/lar/schema").unwrap())).unwrap();
    assert!(md.contains(&format!("- {t} [table")), "{md}");
    assert!(e.err("execute", json!({"connection": "larw", "sql": format!("DROP TABLE {t}"), "confirm": true})).contains("destructive"));
    e.answer(&[Allow]);
    e.ok("execute", json!({"connection": "larf", "sql": format!("DROP TABLE {t}")}));
    leak(e);
}

#[test]
fn mysql_tiers() {
    let (h, p) = addr("DATAMAN_MYSQL", "127.0.0.1:33306");
    if !up(&h, p) {
        eprintln!("skipping: no MySQL at {h}:{p}");
        return;
    }
    use Answer::*;
    let e = env();
    let db = format!("mcp_live_{}", std::process::id());
    let on = |conn: &str, sql: String| json!({"connection": conn, "database": db, "sql": sql});
    // CREATE DATABASE runs outside a transaction: asked first, then alone.
    e.answer(&[Allow]);
    e.ok("execute", json!({"connection": "myf", "sql": format!("CREATE DATABASE {db}")}));
    assert_eq!(e.asked()[0].reasons, vec!["CREATE DATABASE".to_string()]);
    // MySQL commits DDL on its own: asked before it runs, never previewed.
    e.answer(&[Allow]);
    e.ok("execute", on("myf", "CREATE TABLE t (id int PRIMARY KEY, a int)".into()));
    assert_eq!(e.asked()[0].affected, None);
    assert!(e.err("execute", on("myw", "CREATE TABLE u (id int)".into())).contains("destructive"));
    let r = e.ok("execute", on("myw", "INSERT INTO t VALUES (1, 0), (2, 0), (3, 0)".into()));
    assert_eq!(r["statements"][0], json!(["INSERT", 3]));
    assert_eq!(e.ok("execute", on("myw", "UPDATE t SET a = 1 WHERE id = 2".into()))["statements"][0], json!(["UPDATE", 1]));
    for sql in ["DELETE FROM t", "DELETE FROM t WHERE 1", "UPDATE t SET a = 2 WHERE id = id", "DELETE FROM t ORDER BY id LIMIT 1"] {
        assert!(e.err("execute", on("myw", sql.into())).contains("destructive"), "{sql}");
    }
    // Held and denied: InnoDB rolls the DELETE back.
    e.answer(&[Deny]);
    assert!(e.err("execute", on("myf", "DELETE FROM t".into())).contains("rolled back"));
    assert_eq!(e.asked()[0].affected, Some(3));
    let n = e.ok("run_query", json!({"connection": "myw", "database": db, "sql": "SELECT count(*) FROM t"}));
    assert_eq!(n["rows"], json!([[3]]));
    e.answer(&[Allow]);
    e.ok("execute", json!({"connection": "myf", "sql": format!("DROP DATABASE {db}")}));
    leak(e);
}

#[test]
fn sql_server_tiers() {
    let (h, p) = addr("DATAMAN_MSSQL", "127.0.0.1:14330");
    if !up(&h, p) {
        eprintln!("skipping: no SQL Server at {h}:{p}");
        return;
    }
    use Answer::*;
    let e = env();
    let t = format!("mcp_live_{}", std::process::id());
    // msw's own row limit is 3.
    e.answer(&[Allow]);
    e.ok("execute", json!({"connection": "msf", "sql": format!("CREATE TABLE dbo.{t} (id int PRIMARY KEY, a int)")}));
    assert_eq!(e.asked()[0].reasons, vec!["CREATE TABLE".to_string()]);
    let r = e.ok("execute", json!({"connection": "msw", "sql": format!("SET NOCOUNT OFF; INSERT INTO dbo.{t} VALUES (1, 0), (2, 0), (3, 0)")}));
    assert!(r["statements"].as_array().unwrap().iter().any(|s| s == &json!(["INSERT", 3])), "{r}");
    for sql in [format!("DELETE TOP (1) FROM dbo.{t}"), format!("DELETE FROM dbo.{t} WHERE 1=1"), format!("EXEC('DROP TABLE dbo.{t}')"), format!("TRUNCATE TABLE dbo.{t}")] {
        assert!(e.err("execute", json!({"connection": "msw", "sql": sql})).contains("destructive"), "{sql}");
    }
    // Over msw's limit of 3: held with its OUTPUT rows, denied, rolled back.
    e.answer(&[Deny]);
    let err = e.err("execute", json!({"connection": "msw", "sql": format!("INSERT INTO dbo.{t} OUTPUT inserted.id VALUES (4, 0), (5, 0), (6, 0), (7, 0)")}));
    assert!(err.contains("4 rows (limit 3)") && err.contains("rolled back"), "{err}");
    let q = e.asked();
    assert_eq!((q[0].affected, q[0].preview.as_ref().map(|p| p.1.len())), (Some(4), Some(4)));
    // A held DELETE on SQL Server, then allowed.
    e.answer(&[Deny]);
    assert!(e.err("execute", json!({"connection": "msf", "sql": format!("DELETE FROM dbo.{t}")})).contains("rolled back"));
    let n = e.ok("run_query", json!({"connection": "msw", "sql": format!("SELECT count(*) FROM dbo.{t}")}));
    assert_eq!(n["rows"], json!([[3]]), "rolled back");
    e.asked();
    e.answer(&[Allow]);
    e.ok("execute", json!({"connection": "msf", "sql": format!("DROP TABLE dbo.{t}")}));
    leak(e);
}

#[test]
fn mongodb_tools() {
    let (h, p) = addr("DATAMAN_MONGO", "127.0.0.1:27027");
    if !up(&h, p) {
        eprintln!("skipping: no MongoDB at {h}:{p}");
        return;
    }
    use Answer::*;
    let e = env();
    let r = |x: Value| with("mg", x);
    let w = |x: Value| with("mgw", x);
    let f = |x: Value| with("mgf", x);
    let _ = e.call("nosql_execute", f(json!({"query": "db.dropDatabase()"})));
    e.answer(&[Allow]);
    let _ = e.call("nosql_execute", f(json!({"query": "db.dropDatabase()"})));
    let _ = e.asked();

    // list_connections says what each connection speaks.
    let conns = e.ok("list_connections", json!({}));
    let cols: Vec<&str> = conns["columns"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    let (ei, li) = (cols.iter().position(|c| *c == "engine").unwrap(), cols.iter().position(|c| *c == "language").unwrap());
    let row = conns["rows"].as_array().unwrap().iter().find(|r| r[0] == "mg").unwrap();
    assert_eq!(row[ei], "mongodb");
    assert!(row[li].as_str().unwrap().contains("nosql_query"), "{row:?}");

    // Reads only in the read tier; writes refused before connecting.
    assert!(e.err("nosql_execute", r(json!({"query": "db.items.insertOne({a: 1})"}))).contains("reads only"));
    assert!(e.err("nosql_query", r(json!({"query": "db.items.insertOne({a: 1})"}))).contains("nosql_execute"));
    assert!(e.err("nosql_query", r(json!({"query": "db.items.find({a: })"}))).contains("invalid"));

    // Write tier: inserts and filtered updates within the row limit (3) run.
    let v = e.ok("nosql_execute", w(json!({"query": "db.items.insertMany([{n: 1, k: 'a'}, {n: 2, k: 'a'}, {n: 3, k: 'b'}])"})));
    assert_eq!(v["affected"], 3, "{v}");
    let v = e.ok("nosql_execute", w(json!({"query": "db.items.updateMany({k: 'a'}, {$set: {seen: true}})"})));
    assert_eq!(v["affected"], 2, "{v}");
    // Past the limit it waits for the user: denied, nothing changed; allowed, it runs.
    e.ok("nosql_execute", w(json!({"query": "db.items.insertOne({n: 4, k: 'b'})"})));
    let err = e.err("nosql_execute", w(json!({"query": "db.items.updateMany({n: {$gt: 0}}, {$set: {big: true}})"})));
    assert!(err.contains("denied") && err.contains("4 rows (limit 3)"), "{err}");
    assert_eq!(e.asked().len(), 1);
    assert_eq!(e.ok("nosql_query", r(json!({"query": "db.items.countDocuments({big: true})"})))["value"], 0);
    e.answer(&[Allow]);
    let v = e.ok("nosql_execute", w(json!({"query": "db.items.updateMany({n: {$gt: 0}}, {$set: {big: true}})"})));
    assert_eq!((v["affected"].clone(), v["approved"]["reasons"][0].clone()), (json!(4), json!("4 rows (limit 3)")), "{v}");
    let _ = e.asked();
    // Destructive: refused in write, asked in full.
    assert!(e.err("nosql_execute", w(json!({"query": "db.items.deleteMany({})"}))).contains("destructive"));
    e.answer(&[Deny]);
    assert!(e.err("nosql_execute", f(json!({"query": "db.items.drop()"}))).contains("denied"));
    let asked = e.asked();
    assert!(asked[0].destructive && asked[0].reasons[0].contains("drops a collection"), "{:?}", asked[0].reasons);
    // Production asks for every write.
    e.answer(&[Deny]);
    assert!(e.err("nosql_execute", with("mgprod", json!({"query": "db.items.insertOne({n: 5})"}))).contains("production"));
    let _ = e.asked();

    // Reads: documents as extended JSON, bounded; run_query takes the shell's language too.
    let v = e.ok("nosql_query", r(json!({"query": "db.items.find({}, {_id: 0}).sort({n: 1})", "max_rows": 2})));
    assert_eq!((v["count"].clone(), v["more"].clone(), v["documents"][0]["n"].clone()), (json!(2), json!(true), json!(1)), "{v}");
    let v = e.ok("run_query", r(json!({"sql": "db.items.find({n: 1})"})));
    assert!(v["documents"][0]["_id"]["$oid"].is_string(), "{v}");
    let v = e.ok("execute", w(json!({"sql": "db.items.deleteOne({n: 4})"})));
    assert_eq!(v["affected"], 1, "{v}");

    // Scripts (the shell's JavaScript): a read runs as a read, with what it printed; one that
    // writes is refused by nosql_query and, in tier write, waits for the user (its writes cannot
    // be counted first); a call the reading did not see is refused when it comes.
    let v = e.ok("nosql_query", r(json!({"query": "let n = 0\ndb.items.find().forEach(d => { n += d.n })\nprint('sum', n)\ndb.items.find({}, {_id: 0}).sort({n: 1})"})));
    assert_eq!((v["printed"][0].clone(), v["documents"][0]["n"].clone(), v["count"].clone()), (json!("sum 6"), json!(1), json!(3)), "{v}");
    let script = "for (const n of [7, 8]) db.items.insertOne({n})\ndb.items.countDocuments({n: {$gte: 7}})";
    assert!(e.err("nosql_query", r(json!({"query": script}))).contains("nosql_execute"));
    e.answer(&[Deny]);
    let err = e.err("nosql_execute", w(json!({"query": script})));
    assert!(err.contains("denied") && err.contains("a script"), "{err}");
    assert_eq!(e.asked().len(), 1);
    e.answer(&[Allow]);
    let v = e.ok("nosql_execute", w(json!({"query": script})));
    assert_eq!((v["value"].clone(), v["writes"].as_array().map(Vec::len)), (json!(2), Some(2)), "{v}");
    let _ = e.asked();
    let err = e.err("nosql_query", r(json!({"query": "const m = 'delete' + 'Many'\ndb.items[m]({n: 7})"})));
    assert!(err.contains("refused: this call writes"), "{err}");
    assert_eq!(e.ok("nosql_query", r(json!({"query": "db.items.countDocuments({n: 7})"})))["value"], 1);
    assert!(e.err("nosql_execute", w(json!({"query": "db.items.find().forEach(d => db.items.deleteMany({}))"}))).contains("destructive"));
    e.answer(&[Allow]);
    let v = e.ok("nosql_execute", w(json!({"query": "db.items.deleteMany({n: {$gte: 7}}); 'done'"})));
    assert_eq!(v["value"], "done", "{v}");
    let _ = e.asked();

    // The catalog: databases, collections, fields sampled, documents.
    let v = e.ok("list_schemas", r(json!({})));
    assert!(v["databases"]["rows"].as_array().unwrap().iter().any(|r| r[0] == "mcp_live"), "{v}");
    let v = e.ok("list_tables", r(json!({})));
    assert_eq!(v["rows"][0][1], "items", "{v}");
    let v = e.ok("describe_table", r(json!({"table": "items"})));
    assert_eq!(v["fields"][0][0], "_id", "{v}");
    assert!(v["fields"].as_array().unwrap().iter().any(|f| f[0] == "seen" && f[1] == "bool"), "{v}");
    assert!(v["indexes"].as_array().is_some_and(|i| !i.is_empty()), "{v}");
    let v = e.ok("sample_rows", r(json!({"table": "items", "n": 2})));
    assert_eq!(v["count"], 2, "{v}");
    let v = e.ok("search_objects", r(json!({"pattern": "ITE"})));
    assert!(v["rows"].as_array().unwrap().iter().any(|r| r[2] == "items"), "{v}");
    assert!(e.err("get_ddl", r(json!({"object": "items"}))).contains("describe_table"));

    // The audit line has the statement and the decision.
    let audit = e.audit();
    assert!(audit.iter().any(|l| l["tool"] == "nosql_execute" && l["approval"]["decision"] == "approved"), "{audit:?}");
    e.answer(&[Allow]);
    let _ = e.call("nosql_execute", f(json!({"query": "db.dropDatabase()"})));
    leak(e);
}

#[test]
fn search_tools() {
    let (h, p) = addr("DATAMAN_ES", "127.0.0.1:9201");
    if !up(&h, p) {
        eprintln!("skipping: no Elasticsearch at {h}:{p}");
        return;
    }
    use Answer::*;
    let e = env();
    let r = |x: Value| with("es", x);
    let w = |x: Value| with("esw", x);
    let f = |x: Value| with("esf", x);
    let idx = format!("mcp_live_{}", std::process::id());
    // Creating an index is destructive: full, after the user's Allow.
    assert!(e.err("nosql_execute", w(json!({"query": format!("PUT /{idx}")}))).contains("destructive"));
    e.answer(&[Allow]);
    e.ok("nosql_execute", f(json!({"query": format!("PUT /{idx}")})));
    let _ = e.asked();

    // Write tier: documents within the row limit; a query over it waits for the user.
    for i in 0..4 {
        e.ok("nosql_execute", w(json!({"query": format!("PUT /{idx}/_doc/{i}?refresh=true\n{{\"n\": {i}, \"k\": \"a\"}}")})));
    }
    let v = e.ok("nosql_query", r(json!({"query": format!("GET /{idx}/_search\n{{\"sort\": [{{\"n\": \"asc\"}}]}}"), "max_rows": 3})));
    assert_eq!((v["count"].clone(), v["total"].clone(), v["more"].clone(), v["documents"][0]["n"].clone()), (json!(3), json!(4), json!(true), json!(0)), "{v}");
    let err = e.err("nosql_execute", w(json!({"query": format!("POST /{idx}/_delete_by_query?refresh=true\n{{\"query\": {{\"term\": {{\"k\": \"a\"}}}}}}")})));
    assert!(err.contains("denied") && err.contains("4 rows (limit 3)"), "{err}");
    let _ = e.asked();
    assert!(e.err("nosql_execute", w(json!({"query": format!("POST /{idx}/_delete_by_query\n{{\"query\": {{\"match_all\": {{}}}}}}")}))).contains("destructive"));
    assert!(e.err("nosql_query", r(json!({"query": format!("DELETE /{idx}/_doc/1")}))).contains("nosql_execute"));
    // run_query hands the console's requests on too.
    let v = e.ok("run_query", r(json!({"sql": format!("GET /{idx}/_count")})));
    assert_eq!(v["value"]["count"], 4, "{v}");

    // The catalog: the indices, the mapping's fields, documents.
    let v = e.ok("list_tables", r(json!({})));
    assert!(v["rows"].as_array().unwrap().iter().any(|x| x[1] == json!(idx)), "{v}");
    let v = e.ok("describe_table", r(json!({"table": idx})));
    assert!(v["fields"].as_array().unwrap().iter().any(|x| x[0] == "n" && x[1] == "long"), "{v}");
    let v = e.ok("sample_rows", r(json!({"table": idx, "n": 2})));
    assert_eq!(v["count"], 2, "{v}");

    e.answer(&[Allow]);
    e.ok("nosql_execute", f(json!({"query": format!("DELETE /{idx}")})));
    leak(e);
}

#[test]
fn redis_tools() {
    let (h, p) = addr("DATAMAN_REDIS", "127.0.0.1:63790");
    if !up(&h, p) {
        eprintln!("skipping: no Redis at {h}:{p}");
        return;
    }
    use Answer::*;
    let e = env();
    let r = |x: Value| with("rd", x);
    let w = |x: Value| with("rdw", x);
    let f = |x: Value| with("rdf", x);
    // FLUSHDB is destructive: full, after the user's Allow.
    assert!(e.err("nosql_execute", w(json!({"query": "FLUSHDB"}))).contains("destructive"));
    e.answer(&[Allow]);
    e.ok("nosql_execute", f(json!({"query": "FLUSHDB"})));
    let _ = e.asked();
    // Writes within the row limit; a DEL of more keys waits for the user.
    assert!(e.err("nosql_execute", r(json!({"query": "SET a 1"}))).contains("reads only"));
    e.ok("nosql_execute", w(json!({"query": "HSET user:1 name Ada age 36"})));
    e.ok("nosql_execute", w(json!({"query": "MSET a 1 b 2 c 3"})));
    let err = e.err("nosql_execute", w(json!({"query": "DEL a b c user:1"})));
    assert!(err.contains("denied") && err.contains("4 rows (limit 3)"), "{err}");
    let _ = e.asked();
    assert!(e.err("nosql_query", r(json!({"query": "DEL a"}))).contains("nosql_execute"));
    assert!(e.err("nosql_query", r(json!({"query": "SUBSCRIBE x"}))).contains("not supported"));
    // Reads: a value, a hash's entries.
    // A string's value is one entry; another read's reply is its value.
    assert_eq!(e.ok("nosql_query", r(json!({"query": "GET b"})))["documents"][0]["value"], "2");
    assert_eq!(e.ok("nosql_query", r(json!({"query": "STRLEN b"})))["value"], 1);
    let v = e.ok("run_query", r(json!({"sql": "HGETALL user:1"})));
    assert_eq!((v["count"].clone(), v["documents"][0]["field"].clone()), (json!(2), json!("name")), "{v}");
    // The catalog: databases, keys with their types.
    let v = e.ok("list_schemas", r(json!({})));
    assert!(v["databases"]["rows"].as_array().unwrap().iter().any(|x| x[0] == "8"), "{v}");
    let v = e.ok("list_tables", r(json!({})));
    assert!(v["rows"].as_array().unwrap().iter().any(|x| x[1] == "user:1" && x[2] == "hash"), "{v}");
    let v = e.ok("describe_table", r(json!({"table": "user:1"})));
    assert_eq!((v["type"].clone(), v["view"].clone()), (json!("hash"), json!("HGETALL user:1")), "{v}");
    let v = e.ok("sample_rows", r(json!({"table": "user:1", "n": 1})));
    assert_eq!(v["count"], 1, "{v}");
    e.answer(&[Allow]);
    e.ok("nosql_execute", f(json!({"query": "FLUSHDB"})));
    leak(e);
}
