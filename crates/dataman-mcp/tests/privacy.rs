//! Privacy and Permissions for agents, through the MCP server: what is hidden is refused or
//! narrowed, what is sensitive is masked, errors lose their values, and agents cannot loosen any
//! of it. The live parts skip when their database is not up (the e2e Docker containers).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

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

fn up(var: &str, default: &str) -> bool {
    let (h, p) = addr(var, default);
    let ok = std::net::TcpStream::connect_timeout(&format!("{h}:{p}").parse().unwrap(), Duration::from_secs(1)).is_ok();
    if !ok {
        eprintln!("skipping: nothing at {h}:{p} ({var})");
    }
    ok
}

#[derive(Default)]
struct Human {
    allow: Mutex<VecDeque<bool>>,
    asked: Mutex<Vec<Approval>>,
}

impl Front for Human {
    fn approve(&self, a: Approval) -> BoxFuture<'static, bool> {
        self.asked.lock().push(a);
        let yes = self.allow.lock().pop_front().unwrap_or(false);
        Box::pin(async move { yes })
    }
}

struct Env {
    server: Server,
    human: Arc<Human>,
    dir: tempfile::TempDir,
}

fn conn(id: &str, var: &str, default: &str, user: &str, db: &str, extra: &str) -> String {
    let (h, p) = addr(var, default);
    format!("[[connection]]\nid = \"{id}\"\nname = \"{id}\"\nhost = \"{h}\"\nport = {p}\nuser = \"{user}\"\ndatabase = \"{db}\"\nsslmode = \"disable\"\n{extra}\n")
}

/// `s` is this run's PostgreSQL schema (and the MySQL database, the Mongo database, the Redis key
/// prefix and the index prefix).
fn env(s: &str, settings: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("dataman");
    std::fs::create_dir_all(&cfg).unwrap();
    let my = "engine = \"mysql\"\nsave_password = true\n";
    let ms = "engine = \"sql-server\"\nsave_password = true\n";
    let mg = "engine = \"mongodb\"\nsave_password = true\n";
    let es = "engine = \"elasticsearch\"\nsave_password = true\n";
    let rd = "engine = \"redis\"\nsave_password = true\n";
    let pg_rules = format!(
        "mcp = \"write\"\n[connection.mcp_rules]\nview = \"masked\"\nstructure = false\n\
         sensitive = [{{ col = \"customers.email\", how = \"partial\" }}, {{ col = \"*cpf*\", how = \"hash\" }}, {{ col = \"customers.city\", how = \"hidden\", on = false }}]\n\
         types = {{ phone = false }}\n\
         [connection.mcp_rules.visible]\nitems = [{{ name = \"{s}.payments\", kind = \"table\" }}, {{ name = \"users.password_hash\", kind = \"column\" }}]\n"
    );
    let text = [
        conn("pgadmin", "DATAMAN_PG", "127.0.0.1:55432", "postgres", "dataman", "mcp = \"full\""),
        conn("prod-pg", "DATAMAN_PG", "127.0.0.1:55432", "postgres", "dataman", &pg_rules),
        conn("structure-pg", "DATAMAN_PG", "127.0.0.1:55432", "postgres", "dataman", "mcp = \"write\"\n[connection.mcp_rules]\nview = \"structure\"\n"),
        conn("myadmin", "DATAMAN_MYSQL", "127.0.0.1:33306", "root", "mysql", &format!("{my}mcp = \"full\"")),
        conn(
            "my",
            "DATAMAN_MYSQL",
            "127.0.0.1:33306",
            "root",
            s,
            &format!("{my}[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"customers.email\", how = \"hidden\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"users.password_hash\", kind = \"column\" }}]\n"),
        ),
        conn("ms", "DATAMAN_MSSQL", "127.0.0.1:14330", "sa", "tempdb", &format!("{ms}[connection.mcp_rules.visible]\nmode = \"only\"\nitems = [{{ name = \"dbo.{s}_open\", kind = \"table\" }}]\n"))
            .replace("sslmode = \"disable\"", "sslmode = \"require\""),
        conn("msadmin", "DATAMAN_MSSQL", "127.0.0.1:14330", "sa", "tempdb", &format!("{ms}mcp = \"full\"")).replace("sslmode = \"disable\"", "sslmode = \"require\""),
        conn("mgadmin", "DATAMAN_MONGO", "127.0.0.1:27027", "root", s, &format!("{mg}mcp = \"full\"")),
        conn(
            "mg",
            "DATAMAN_MONGO",
            "127.0.0.1:27027",
            "root",
            s,
            &format!("{mg}mcp = \"write\"\n[connection.mcp_rules]\nview = \"masked\"\ncode = false\nsensitive = [{{ col = \"customers.email\", how = \"partial\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"{s}.payments\", kind = \"table\" }}, {{ name = \"users.ssn\", kind = \"column\" }}]\n"),
        ),
        conn("esadmin", "DATAMAN_ES", "127.0.0.1:9201", "elastic", "", &format!("{es}mcp = \"full\"")),
        conn("es", "DATAMAN_ES", "127.0.0.1:9201", "elastic", "", &format!("{es}[connection.mcp_rules.visible]\nitems = [{{ name = \"{s}-pay*\", kind = \"table\" }}, {{ name = \"{s}-cust.card\", kind = \"column\" }}]\n")),
        conn("rdadmin", "DATAMAN_REDIS", "127.0.0.1:63790", "", "9", &format!("{rd}mcp = \"full\"")),
        conn(
            "rd",
            "DATAMAN_REDIS",
            "127.0.0.1:63790",
            "",
            "9",
            &format!("{rd}mcp = \"write\"\n[connection.mcp_rules]\n[connection.mcp_rules.redis]\nscripting = false\n[connection.mcp_rules.visible]\nitems = [{{ name = \"{s}:session:*\", kind = \"table\" }}, {{ name = \"{s}:user:*.password\", kind = \"column\" }}]\n"),
        ),
    ]
    .concat();
    std::fs::write(cfg.join("connections.toml"), text).unwrap();
    std::fs::write(cfg.join("settings.toml"), settings).unwrap();
    let pgpass = dir.path().join("pgpass");
    let (ph, pp) = addr("DATAMAN_PG", "127.0.0.1:55432");
    std::fs::write(&pgpass, format!("{ph}:{pp}:*:postgres:dataman\n")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&pgpass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let pw = |var: &str, d: &str| std::env::var(var).unwrap_or_else(|_| d.into());
    for (ids, p) in [
        (&["myadmin", "my"][..], pw("DATAMAN_MYSQL_PASSWORD", "dataman-root")),
        (&["ms", "msadmin"][..], pw("DATAMAN_MSSQL_PASSWORD", "Dataman-sa-1!")),
        (&["mg", "mgadmin"][..], pw("DATAMAN_MONGO_PASSWORD", "dataman")),
        (&["es", "esadmin"][..], pw("DATAMAN_ES_PASSWORD", "dataman")),
        (&["rd", "rdadmin"][..], pw("DATAMAN_REDIS_PASSWORD", "dataman")),
    ] {
        for id in ids {
            secrets.set(id, &p).unwrap();
        }
    }
    let hub = Hub::new(Dirs::at(&cfg), secrets).unwrap();
    let hub = {
        let mut h = Arc::try_unwrap(hub).ok().unwrap();
        h.core.pgpass = Some(pgpass);
        Arc::new(h)
    };
    hub.set_mask_key(dataman_core::privacy::HashKey::fixed(b"privacy-test"));
    let human = Arc::new(Human::default());
    hub.set_front(Some(human.clone()));
    hub.set_approval_timeout(Duration::from_millis(600));
    hub.set_after_deny(Duration::ZERO);
    let reg = Arc::new(Registry::standard(None));
    Env { server: Server::new(hub, reg, "stdio"), human, dir }
}

impl Env {
    fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        let t = self.server.registry.find(tool).unwrap_or_else(|| panic!("no tool {tool}"));
        let r = self.server.hub.core.block_on(self.server.run_tool(t, args.as_object().unwrap().clone(), "privacy-test".into(), CancellationToken::new()));
        let text = match &r.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("{other:?}"),
        };
        if r.is_error == Some(true) { Err(text) } else { Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))) }
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

    /// Runs as the user would (a full-tier connection, Allowed).
    fn admin(&self, tool: &str, args: Value) -> Value {
        self.human.allow.lock().extend([true; 4]);
        let v = self.ok(tool, args);
        self.human.allow.lock().clear();
        v
    }

    fn audit(&self) -> Vec<Value> {
        let t = std::fs::read_to_string(self.dir.path().join("dataman/mcp-audit.jsonl")).unwrap_or_default();
        t.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn done(self) {
        self.server.hub.close_all();
        let Env { server, dir, .. } = self;
        std::mem::forget(server);
        drop(dir);
    }
}

fn with(conn: &str, extra: Value) -> Value {
    let mut v = json!({"connection": conn});
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v
}

fn names(v: &Value, col: usize) -> Vec<String> {
    v["rows"].as_array().unwrap().iter().map(|r| r[col].as_str().unwrap_or_default().to_string()).collect()
}

#[test]
fn rules_hold_without_a_database() {
    let e = env("nodb", "[agent_types]\n[[agent_types.custom]]\nid = \"t_conn\"\nname = \"Conn\"\npattern = \"\\\\bprod-pg\\\\b\"\nmask = \"hidden\"\n");
    // The final pass covers every tool: here a custom type masks a connection's name.
    let conns = e.ok("list_connections", json!({}));
    let text = conns.to_string();
    assert!(!text.contains("\"prod-pg\"") && text.contains("‹conn›"), "{text}");
    assert_eq!(conns["masked"]["Conn"].as_u64().map(|n| n > 0), Some(true), "the reply counts what was masked");
    let cols: Vec<&str> = conns["columns"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    let at = |c: &str| cols.iter().position(|x| *x == c).unwrap();
    let rows = conns["rows"].as_array().unwrap();
    let structure = rows.iter().find(|r| r[0] == "structure-pg").unwrap();
    assert!(structure[at("host")].is_null() && structure[at("user")].is_null() && structure[at("port")].is_null(), "no address for agents that see no data");
    assert_eq!(structure[at("agents")]["sees"], "structure");
    let admin = rows.iter().find(|r| r[0] == "pgadmin").unwrap();
    assert!(admin[at("host")].is_string());

    let err = e.err("run_query", json!({"connection": "structure-pg", "sql": "SELECT 1"}));
    assert!(err.contains("structure only"), "{err}");
    assert!(e.err("sample_rows", json!({"connection": "structure-pg", "table": "x"})).contains("structure only") || !up("DATAMAN_PG", "127.0.0.1:55432"));

    // A broken custom type that is on refuses reads.
    let broken = env("nodb", "[agent_types]\n[[agent_types.custom]]\nid = \"t_b\"\nname = \"Broken\"\npattern = \"(\"\nmask = \"hidden\"\n");
    assert!(broken.err("list_connections", json!({})).contains("Broken has an invalid pattern"));
    broken.done();

    // Agents cannot loosen the rules: update_connection keeps them as they are.
    let r = e.call("update_connection", json!({"connection": "prod-pg", "env": "staging"}));
    let saved = dataman_core::ConfigStore::open(&Dirs::at(&e.dir.path().join("dataman"))).unwrap().connections();
    let p = saved.iter().find(|p| p.id == "prod-pg").unwrap();
    assert_eq!(p.mcp_rules.view, dataman_core::privacy::AgentView::Masked, "{r:?}");
    assert!(!p.mcp_rules.visible.is_open() && !p.mcp_rules.structure);
    for tool in ["create_connection", "update_connection"] {
        let t = e.server.registry.find(tool).unwrap();
        let props: Vec<&String> = t.schema().properties().keys().collect();
        assert!(!props.iter().any(|k| k.contains("rules") || k.contains("privacy") || k.contains("visible") || k.contains("sensitive")), "{tool}: {props:?}");
    }
    e.done();
}

#[test]
fn postgres_privacy() {
    if !up("DATAMAN_PG", "127.0.0.1:55432") {
        return;
    }
    let s = format!("mcp_priv_{}", std::process::id());
    let e = env(&s, "");
    let a = |x: Value| with("pgadmin", x);
    let p = |x: Value| with("prod-pg", x);
    e.admin(
        "execute",
        a(json!({"sql": format!(
            "CREATE SCHEMA {s}; \
             CREATE TABLE {s}.users (id int PRIMARY KEY, name text, email text UNIQUE, password_hash text); \
             CREATE TABLE {s}.customers (id int PRIMARY KEY, name text, email text, cpf text, city text, phone text); \
             CREATE TABLE {s}.payments (id int PRIMARY KEY, card text); \
             CREATE VIEW {s}.everything AS SELECT * FROM {s}.customers; \
             INSERT INTO {s}.users VALUES (1, 'Ana', 'ana@acme.com', '$2b$12$abcdefghijklmnopqrstuv'); \
             INSERT INTO {s}.customers VALUES (1, 'Ana', 'ana@acme.com', '123.456.789-09', 'Recife', '(81) 98765-4321'); \
             INSERT INTO {s}.payments VALUES (1, '4242 4242 4242 4242')"
        )})),
    );
    let q = |sql: String| e.call("run_query", p(json!({"sql": sql})));

    // Hidden tables and columns.
    assert!(q(format!("SELECT * FROM {s}.payments")).unwrap_err().contains("payments is not visible to agents on prod-pg"));
    let err = q(format!("SELECT upper(password_hash) FROM {s}.users")).unwrap_err();
    assert!(err.contains("users.password_hash is hidden from agents"), "{err}");
    let r = q(format!("SELECT * FROM {s}.users u")).unwrap();
    let cols: Vec<&str> = r["columns"].as_array().unwrap().iter().map(|c| c[0].as_str().unwrap()).collect();
    assert_eq!(cols, ["id", "name", "email"], "the hidden column does not exist for agents");
    assert!(r["narrowed"].is_string());
    let row = q(format!("SELECT row_to_json(u) FROM {s}.users u")).unwrap()["rows"][0][0].to_string();
    assert!(!row.contains("password_hash") && !row.contains("$2b$"), "{row}");
    assert!(q("SELECT * FROM pg_catalog.pg_class".into()).unwrap_err().contains("system catalog"));
    assert!(q("SELECT * FROM pg_stat_activity".into()).unwrap_err().contains("server activity"));

    // Masked data: sensitive columns, and types inside any value.
    let r = q(format!("SELECT id, name, email, cpf, city, phone FROM {s}.customers WHERE email LIKE 'ana%'")).unwrap();
    let row = &r["rows"][0];
    assert_eq!(row[2], "a•••@acme.com");
    assert!(row[3].as_str().unwrap().starts_with("‹h:"), "{row}");
    assert_eq!(row[4], "Recife", "unchecked columns are not masked");
    assert_eq!(row[5], "(81) 98765-4321", "phone is switched off on this connection");
    assert!(r["masked_columns"].to_string().contains("email (partial)"));
    let r = q(format!("SELECT name || ' <' || 'ana@acme.com' || '>' AS x FROM {s}.customers")).unwrap();
    assert_eq!(r["rows"][0][0], "Ana <a•••@acme.com>", "e-mails are masked inside any value");
    assert!(q(format!("SELECT upper(email) FROM {s}.customers")).unwrap_err().contains("masked for agents"));
    assert!(q(format!("SELECT email AS e FROM {s}.customers")).unwrap_err().contains("masked for agents"));
    assert!(q(format!("SELECT cpf, count(*) FROM {s}.customers GROUP BY cpf")).unwrap_err().contains("masked for agents"));
    let r = e.ok("sample_rows", p(json!({"table": format!("{s}.customers")})));
    assert_eq!(r["rows"][0][2], "a•••@acme.com");

    // Introspection.
    let t = e.ok("list_tables", p(json!({"schema": s})));
    let listed = names(&t, 1);
    assert!(listed.contains(&"users".to_string()) && !listed.contains(&"payments".to_string()), "{listed:?}");
    let d = e.ok("describe_table", p(json!({"table": format!("{s}.users"), "include_ddl": true})));
    assert!(!d.to_string().contains("password_hash"), "{d}");
    assert!(d["ddl"].as_str().unwrap().starts_with("-- 1 column is not visible"));
    assert!(e.err("describe_table", p(json!({"table": format!("{s}.payments")}))).contains("not visible"));
    assert!(e.err("get_ddl", p(json!({"object": format!("{s}.payments")}))).contains("not visible"));
    let found = e.ok("search_objects", p(json!({"pattern": "password"})));
    assert!(!found.to_string().contains("password_hash"), "{found}");

    // Writes: hidden columns refused, structure off, masked values never written back.
    assert!(e.err("execute", p(json!({"sql": format!("UPDATE {s}.users SET password_hash = 'x' WHERE id = 1")}))).contains("hidden from agents"));
    assert!(e.err("execute", p(json!({"sql": format!("CREATE TABLE {s}.t (a int)")}))).contains("changing structure"));
    assert!(e.err("execute", p(json!({"sql": format!("UPDATE {s}.customers SET email = 'a•••@acme.com' WHERE id = 1")}))).contains("masked values"));

    // Errors lose their values, in the reply and in the log.
    let err = e.err("execute", p(json!({"sql": format!("INSERT INTO {s}.users (id, name, email) VALUES (2, 'Bia', 'ana@acme.com')")})));
    assert!(err.contains("‹value›") && !err.contains("ana@acme.com") && !err.contains("a•••"), "{err}");
    let last = e.audit().into_iter().rev().find(|x| x["tool"] == "execute" && x["outcome"] == "error").unwrap();
    let logged = last["error"].as_str().unwrap();
    assert!(logged.contains("‹value›") && !logged.contains("ana@acme.com"), "the log keeps the cleaned error: {last}");
    let seen = e.audit().into_iter().find(|x| x["seen"]["masked"].is_array()).unwrap();
    assert!(seen["seen"]["masked"].to_string().contains("email"), "{seen}");
    let rewritten = e.audit().into_iter().find(|x| x["rewritten"].is_string()).unwrap();
    assert!(rewritten["rewritten"].as_str().unwrap().contains("(SELECT"), "{rewritten}");

    // Structure only.
    let st = |x: Value| with("structure-pg", x);
    assert!(e.err("run_query", st(json!({"sql": format!("SELECT * FROM {s}.users")}))).contains("structure only"));
    assert!(e.err("sample_rows", st(json!({"table": format!("{s}.users")}))).contains("structure only"));
    assert!(e.ok("describe_table", st(json!({"table": format!("{s}.users")})))["columns"].is_array());
    assert!(e.ok("explain", st(json!({"sql": format!("SELECT * FROM {s}.users")})))["plan"].is_string());

    e.admin("execute", a(json!({"sql": format!("DROP SCHEMA {s} CASCADE")})));
    e.done();
}

#[test]
fn mysql_and_sql_server_privacy() {
    let s = format!("mcp_priv_{}", std::process::id());
    if up("DATAMAN_MYSQL", "127.0.0.1:33306") {
        let e = env(&s, "");
        e.admin("execute", with("myadmin", json!({"sql": format!("CREATE DATABASE {s}")})));
        e.admin("execute", with("myadmin", json!({"sql": format!("CREATE TABLE {s}.users (id int PRIMARY KEY, name text, password_hash text)")})));
        e.admin("execute", with("myadmin", json!({"sql": format!("CREATE TABLE {s}.customers (id int PRIMARY KEY, email text)")})));
        e.admin("execute", with("myadmin", json!({"sql": format!("INSERT INTO {s}.users VALUES (1, 'Ana', 'x'); INSERT INTO {s}.customers VALUES (1, 'ana@acme.com')")})));
        let m = |x: Value| with("my", x);
        let r = e.ok("run_query", m(json!({"sql": "SELECT * FROM users"})));
        assert_eq!(r["columns"].as_array().unwrap().len(), 2, "{r}");
        assert!(e.err("run_query", m(json!({"sql": "SELECT password_hash FROM users"}))).contains("hidden"));
        assert_eq!(e.ok("run_query", m(json!({"sql": "SELECT email FROM customers"})))["rows"][0][0], "‹hidden›");
        e.admin("execute", with("myadmin", json!({"sql": format!("DROP DATABASE {s}")})));
        e.done();
    }
    if up("DATAMAN_MSSQL", "127.0.0.1:14330") {
        let e = env(&s, "");
        e.admin("execute", with("msadmin", json!({"sql": format!("CREATE TABLE dbo.{s}_open (id int); CREATE TABLE dbo.{s}_closed (id int)")})));
        let m = |x: Value| with("ms", x);
        assert!(e.ok("run_query", m(json!({"sql": format!("SELECT * FROM dbo.{s}_open")})))["rows"].is_array());
        assert!(e.err("run_query", m(json!({"sql": format!("SELECT * FROM dbo.{s}_closed")}))).contains("not visible"));
        let listed = names(&e.ok("list_tables", m(json!({"schema": "dbo"}))), 1);
        assert!(listed.contains(&format!("{s}_open")) && !listed.contains(&format!("{s}_closed")), "{listed:?}");
        e.admin("execute", with("msadmin", json!({"sql": format!("DROP TABLE dbo.{s}_open; DROP TABLE dbo.{s}_closed")})));
        e.done();
    }
}

#[test]
fn mongodb_privacy() {
    if !up("DATAMAN_MONGO", "127.0.0.1:27027") {
        return;
    }
    let s = format!("mcp_priv_{}", std::process::id());
    let e = env(&s, "");
    let a = |x: Value| with("mgadmin", x);
    let m = |x: Value| with("mg", x);
    e.admin("nosql_execute", a(json!({"query": "db.users.insertOne({_id: 1, name: \"Ana\", ssn: \"123-45-6789\", nested: {ssn: \"x\"}})"})));
    e.admin("nosql_execute", a(json!({"query": "db.customers.insertOne({_id: 1, email: \"ana@acme.com\", city: \"Recife\"})"})));
    e.admin("nosql_execute", a(json!({"query": "db.payments.insertOne({_id: 1, card: \"x\"})"})));
    let r = e.ok("nosql_query", m(json!({"query": "db.users.find({})"})));
    assert!(!r.to_string().contains("ssn") && r.to_string().contains("Ana"), "{r}");
    assert!(e.err("nosql_query", m(json!({"query": "db.users.find({ssn: \"123-45-6789\"})"}))).contains("hidden"));
    assert!(e.err("nosql_query", m(json!({"query": "db.payments.find({})"}))).contains("not visible"));
    assert!(e.err("nosql_query", m(json!({"query": "db.users.aggregate([{$lookup: {from: \"payments\", localField: \"_id\", foreignField: \"_id\", as: \"p\"}}])"}))).contains("not visible"));
    assert!(e.err("nosql_query", m(json!({"query": "db.users.find({$where: \"true\"})"}))).contains("running code"));
    let r = e.ok("nosql_query", m(json!({"query": "db.customers.find({email: /acme/})"})));
    assert_eq!(r["documents"][0]["email"], "a•••@acme.com", "{r}");
    let listed = e.ok("list_tables", m(json!({})));
    assert!(!listed.to_string().contains("payments"), "{listed}");
    let d = e.ok("describe_table", m(json!({"table": "users"})));
    assert!(!d.to_string().contains("ssn"), "{d}");
    e.admin("nosql_execute", a(json!({"query": "db.dropDatabase()"})));
    e.done();
}

#[test]
fn search_and_redis_privacy() {
    let s = format!("mcp_priv_{}", std::process::id());
    if up("DATAMAN_ES", "127.0.0.1:9201") {
        let e = env(&s, "");
        e.admin("nosql_execute", with("esadmin", json!({"query": format!("PUT {s}-cust/_doc/1?refresh=true\n{{\"name\": \"Ana\", \"card\": \"x\"}}")})));
        e.admin("nosql_execute", with("esadmin", json!({"query": format!("PUT {s}-pay1/_doc/1?refresh=true\n{{\"card\": \"x\"}}")})));
        let q = |t: String| e.call("nosql_query", with("es", json!({"query": t})));
        let r = q(format!("GET {s}-cust/_search")).unwrap();
        assert!(r.to_string().contains("Ana") && !r.to_string().contains("card"), "{r}");
        assert!(q(format!("GET {s}-pay1/_search")).unwrap_err().contains("not visible"));
        assert!(q("GET _search".into()).unwrap_err().contains("name the index"));
        assert!(q(format!("GET {s}-cust/_search\n{{\"query\": {{\"term\": {{\"card\": \"x\"}}}}}}")).unwrap_err().contains("hidden"));
        let _ = e.call("nosql_execute", with("esadmin", json!({"query": format!("DELETE {s}-cust")})));
        e.human.allow.lock().extend([true; 2]);
        let _ = e.call("nosql_execute", with("esadmin", json!({"query": format!("DELETE {s}-pay1")})));
        e.done();
    }
    if up("DATAMAN_REDIS", "127.0.0.1:63790") {
        let e = env(&s, "");
        e.admin("nosql_execute", with("rdadmin", json!({"query": format!("HSET {s}:user:1 name Ana password secret")})));
        e.admin("nosql_execute", with("rdadmin", json!({"query": format!("SET {s}:session:1 token")})));
        let q = |t: String| e.call("nosql_query", with("rd", json!({"query": t})));
        let r = q(format!("HGETALL {s}:user:1")).unwrap();
        assert!(r.to_string().contains("Ana") && !r.to_string().contains("secret"), "{r}");
        assert!(q(format!("HGET {s}:user:1 password")).unwrap_err().contains("hidden"));
        assert!(q(format!("GET {s}:session:1")).unwrap_err().contains("not visible"));
        assert_eq!(q("KEYS *".into()).unwrap_err(), "refused: KEYS never runs for agents");
        assert!(e.err("nosql_execute", with("rd", json!({"query": "EVAL \"return 1\" 0"}))).contains("@scripting"));
        let r = q(format!("SCAN 0 MATCH {s}:* COUNT 1000")).unwrap();
        assert!(!r.to_string().contains("session"), "{r}");
        e.admin("nosql_execute", with("rdadmin", json!({"query": format!("DEL {s}:user:1 {s}:session:1")})));
        e.done();
    }
}
