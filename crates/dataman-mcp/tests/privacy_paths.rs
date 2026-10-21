//! Privacy and permissions for agents past the plain SELECT: values of hidden and masked columns
//! must not come back, under another name or in another form, on any engine (aggregates, JSON,
//! subqueries, set operations, catalogs, writes that copy, errors, scripts, joins, aliases and
//! views); the rules' precedence holds; rule changes apply at once; user tools follow the rules;
//! and the audit log records each call without the values kept from agents. The live parts skip
//! when their database is not up (the e2e Docker containers).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dataman_core::privacy::{self, CustomType, HashKey, Mask, TypeSettings, Types};
use dataman_core::{Dirs, Engine, Environment, McpAccess, MemorySecrets, SecretStore};
use dataman_mcp::policy::{self, Need, Target, Verdict};
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

fn up((var, default): (&str, &str)) -> bool {
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

const PG: (&str, &str) = ("DATAMAN_PG", "127.0.0.1:55432");
const MY: (&str, &str) = ("DATAMAN_MYSQL", "127.0.0.1:33306");
const MS: (&str, &str) = ("DATAMAN_MSSQL", "127.0.0.1:14330");
const MG: (&str, &str) = ("DATAMAN_MONGO", "127.0.0.1:27027");
const ES: (&str, &str) = ("DATAMAN_ES", "127.0.0.1:9201");
const RD: (&str, &str) = ("DATAMAN_REDIS", "127.0.0.1:63790");

/// Every value seeded below that agents must never get, nor the audit log keep.
const SECRETS: &[&str] = &["hash-secret-1", "dflt-secret", "taxid-secret-9", "card-secret", "ssn-secret-1", "kid-secret", "4242cardsecret", "zed-secret", "pw-secret", "sess-secret", "closed-secret"];

struct Conn<'a> {
    id: &'a str,
    server: (&'a str, &'a str),
    engine: &'a str,
    user: &'a str,
    db: &'a str,
    extra: String,
}

fn conn<'a>(id: &'a str, server: (&'a str, &'a str), engine: &'a str, user: &'a str, db: &'a str, extra: impl Into<String>) -> Conn<'a> {
    Conn { id, server, engine, user, db, extra: extra.into() }
}

fn profiles(conns: &[Conn]) -> String {
    let mut text = String::new();
    for c in conns {
        let (h, p) = addr(c.server.0, c.server.1);
        let ssl = if c.server.0 == MS.0 { "require" } else { "disable" };
        let engine = if c.engine.is_empty() { String::new() } else { format!("engine = \"{}\"\nsave_password = true\n", c.engine) };
        text.push_str(&format!("[[connection]]\nid = \"{}\"\nname = \"{}\"\nhost = \"{h}\"\nport = {p}\nuser = \"{}\"\ndatabase = \"{}\"\nsslmode = \"{ssl}\"\n{engine}{}\n", c.id, c.id, c.user, c.db, c.extra));
    }
    text
}

fn env(conns: &[Conn], settings: &str, tools: Option<&str>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("dataman");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("connections.toml"), profiles(conns)).unwrap();
    std::fs::write(cfg.join("settings.toml"), settings).unwrap();
    let tools_file = tools.map(|t| {
        let f = cfg.join("mcp-tools.toml");
        std::fs::write(&f, t).unwrap();
        f
    });
    let pgpass = dir.path().join("pgpass");
    let (ph, pp) = addr(PG.0, PG.1);
    std::fs::write(&pgpass, format!("{ph}:{pp}:*:postgres:dataman\n")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&pgpass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let pw = |var: &str, d: &str| std::env::var(var).unwrap_or_else(|_| d.into());
    for c in conns {
        let p = match c.server.0 {
            "DATAMAN_MYSQL" => pw("DATAMAN_MYSQL_PASSWORD", "dataman-root"),
            "DATAMAN_MSSQL" => pw("DATAMAN_MSSQL_PASSWORD", "Dataman-sa-1!"),
            "DATAMAN_MONGO" => pw("DATAMAN_MONGO_PASSWORD", "dataman"),
            "DATAMAN_ES" => pw("DATAMAN_ES_PASSWORD", "dataman"),
            "DATAMAN_REDIS" => pw("DATAMAN_REDIS_PASSWORD", "dataman"),
            _ => continue,
        };
        secrets.set(c.id, &p).unwrap();
    }
    let hub = Hub::new(Dirs::at(&cfg), secrets).unwrap();
    let hub = {
        let mut h = Arc::try_unwrap(hub).ok().unwrap();
        h.core.pgpass = Some(pgpass);
        Arc::new(h)
    };
    hub.set_mask_key(HashKey::fixed(b"privacy-test"));
    let human = Arc::new(Human::default());
    hub.set_front(Some(human.clone()));
    hub.set_approval_timeout(Duration::from_millis(600));
    hub.set_after_deny(Duration::ZERO);
    let reg = Arc::new(Registry::standard(tools_file));
    Env { server: Server::new(hub, reg, "stdio"), human, dir }
}

impl Env {
    fn cfg(&self) -> PathBuf {
        self.dir.path().join("dataman")
    }

    fn call_as(&self, client: &str, tool: &str, args: Value) -> Result<Value, String> {
        let t = self.server.registry.find(tool).unwrap_or_else(|| panic!("no tool {tool}"));
        let r = self.server.hub.core.block_on(self.server.run_tool(t, args.as_object().unwrap().clone(), client.into(), CancellationToken::new()));
        let text = match &r.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("{other:?}"),
        };
        if r.is_error == Some(true) { Err(text) } else { Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))) }
    }

    fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        self.call_as("privacy-test", tool, args)
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        self.call(tool, args.clone()).unwrap_or_else(|e| panic!("{tool} {args}: {e}"))
    }

    fn admin(&self, tool: &str, args: Value) -> Value {
        self.human.allow.lock().extend([true; 4]);
        let v = self.ok(tool, args);
        self.human.allow.lock().clear();
        v
    }

    fn audit_text(&self) -> String {
        std::fs::read_to_string(self.cfg().join("mcp-audit.jsonl")).unwrap_or_default()
    }

    fn audit(&self) -> Vec<Value> {
        self.audit_text().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// The log of the agents' connections holds no value kept from agents (the admin
    /// connections' own statements seed them).
    fn nothing_kept_logged(&self) {
        let agents: Vec<String> = self.audit_text().lines().filter(|l| !l.contains("admin\"")).map(str::to_string).collect();
        assert!(!agents.is_empty());
        for l in &agents {
            for s in SECRETS {
                assert!(!l.contains(s), "the audit log holds {s}: {l}");
            }
        }
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

/// The reply has no value kept from agents.
fn clean(label: &str, v: &Value) {
    let text = v.to_string();
    for s in SECRETS {
        assert!(!text.contains(s), "{label}: {s} came back: {text}");
    }
}

fn refused(label: &str, r: Result<Value, String>, want: &str) {
    match r {
        Ok(v) => panic!("{label} should be refused ({want}): {v}"),
        Err(e) => {
            assert!(e.contains(want), "{label}: {e}");
            for s in SECRETS {
                assert!(!e.contains(s), "{label}: {s} in the error: {e}");
            }
        }
    }
}

const MASKED: &str = "is masked for agents";

#[test]
fn postgres_values_stay_kept() {
    if !up(PG) {
        return;
    }
    let s = format!("mcp_paths_{}", std::process::id());
    let rules = format!(
        "mcp = \"write\"\n[connection.mcp_rules]\nview = \"masked\"\n\
         sensitive = [{{ col = \"customers.email\", how = \"partial\" }}, {{ col = \"*tax_id*\", how = \"hash\" }}, {{ col = \"users.password_hash\", how = \"partial\" }}]\n\
         [connection.mcp_rules.visible]\nitems = [{{ name = \"{s}.payments\", kind = \"table\" }}, {{ name = \"users.password_hash\", kind = \"column\" }}]\n"
    );
    let e = env(&[conn("pgadmin", PG, "", "postgres", "dataman", "mcp = \"full\""), conn("pp", PG, "", "postgres", "dataman", rules)], "", None);
    e.admin(
        "execute",
        with("pgadmin", json!({"sql": format!(
            "CREATE SCHEMA {s}; \
             CREATE TABLE {s}.users (id int PRIMARY KEY, name text, email text, password_hash text DEFAULT 'dflt-secret' CHECK (password_hash <> 'chk')); \
             CREATE TABLE {s}.customers (id int PRIMARY KEY, name text, email text, tax_id text, city text, n text); \
             CREATE TABLE {s}.payments (id int PRIMARY KEY, card text); \
             CREATE TABLE {s}.notes (id int PRIMARY KEY, body text); \
             INSERT INTO {s}.users VALUES (1, 'Ana', 'ana@acme.com', 'hash-secret-1'); \
             INSERT INTO {s}.customers VALUES (1, 'Ana', 'ana@acme.com', 'taxid-secret-9', 'Recife', '12345678901234'); \
             INSERT INTO {s}.payments VALUES (1, 'card-secret'); \
             CREATE VIEW {s}.v_kept AS SELECT id, password_hash AS h FROM {s}.users; \
             CREATE VIEW {s}.v_open AS SELECT id, body FROM {s}.notes; \
             CREATE TABLE {s}.nums (id int, card bigint, doc numeric, meta jsonb); \
             INSERT INTO {s}.nums VALUES (1, 4242424242424242, 4242424242424242, '{{\"card\": 4242424242424242}}')"
        )})),
    );
    let q = |sql: String| e.call("run_query", with("pp", json!({"sql": sql})));
    let x = |sql: String| e.call("execute", with("pp", json!({"sql": sql})));

    // Sensitive types inside numbers too: a card kept in a bigint, a numeric or a JSON number.
    let r = q(format!("SELECT card, doc, meta FROM {s}.nums")).unwrap();
    assert!(!r.to_string().contains("4242424242424242") && r["masked"].is_object(), "{r}");

    // Hidden columns and tables, through forms that read a table whole.
    refused("TABLE", q(format!("TABLE {s}.users")), "TABLE");
    refused("(TABLE)", q(format!("SELECT * FROM (TABLE {s}.users) t")), "TABLE");
    for f in [
        format!("SELECT table_to_xml('{s}.users', true, false, '')"),
        format!("SELECT schema_to_xml('{s}', true, false, '')"),
        format!("SELECT query_to_xml_and_xmlschema('select * from {s}.users', true, false, '')"),
        format!("SELECT database_to_xml(true, false, '')"),
        format!("SELECT * FROM ts_stat('SELECT to_tsvector(password_hash) FROM {s}.users')"),
    ] {
        refused(&f, q(f.clone()), "is not available to agents on pp");
    }
    refused("pg_ function", q("SELECT * FROM pg_options_to_table(ARRAY['a=1'])".into()), "system catalog");

    // Masked values may not reach the reply under another name.
    for sql in [
        format!("SELECT (SELECT tax_id FROM {s}.customers LIMIT 1) AS x"),
        format!("SELECT name FROM {s}.customers UNION ALL SELECT tax_id FROM {s}.customers"),
        format!("WITH x(c) AS (SELECT tax_id FROM {s}.customers) SELECT c FROM x"),
        format!("SELECT c FROM (SELECT tax_id FROM {s}.customers) t(c)"),
        format!("SELECT ARRAY(SELECT tax_id FROM {s}.customers)"),
        format!("SELECT t.x FROM {s}.customers c CROSS JOIN LATERAL (SELECT c.tax_id) t(x)"),
        format!("SELECT * FROM (VALUES ((SELECT tax_id FROM {s}.customers LIMIT 1))) v(x)"),
        format!("SELECT v FROM {s}.customers c, unnest(array[c.tax_id]) u(v)"),
        format!("SELECT substring(tax_id FROM 1 FOR 3) FROM {s}.customers"),
        format!("SELECT string_agg(tax_id, ',') FROM {s}.customers"),
        format!("SELECT count(DISTINCT tax_id) FROM {s}.customers"),
        format!("SELECT tax_id, lag(tax_id) OVER () FROM {s}.customers"),
        format!("SELECT tax_id FROM {s}.customers GROUP BY tax_id"),
    ] {
        refused(&sql, q(sql.clone()), MASKED);
    }
    refused("column list", q(format!("SELECT d FROM {s}.customers c(a, b, e, d)")), "renamed by a column list");
    refused("whole row of a subquery", q(format!("SELECT row_to_json(x) FROM (SELECT tax_id FROM {s}.customers) x")), "whole rows");
    refused("json_agg", q(format!("SELECT json_agg(c) FROM {s}.customers c")), "whole rows");
    refused("ORDER BY 1", q(format!("SELECT tax_id FROM {s}.customers ORDER BY 1")), "BY a position");
    // Filters compare a masked column as it is: computed, it would also be quoted by errors.
    for sql in [
        format!("SELECT id FROM {s}.customers WHERE tax_id::regclass IS NULL"),
        format!("SELECT id FROM {s}.customers WHERE CAST(tax_id AS int) = 1"),
        format!("SELECT id FROM {s}.customers WHERE to_date(tax_id, 'YYYY') IS NULL"),
        format!("SELECT id FROM {s}.customers WHERE tax_id::text[] IS NULL"),
        format!("SELECT id FROM {s}.customers WHERE tax_id || '' = 'x'"),
    ] {
        refused(&sql, q(sql.clone()), MASKED);
    }
    let err = q(format!("SELECT id FROM {s}.customers WHERE n::int > 0")).unwrap_err();
    assert!(err.contains("‹value›") && !err.contains("12345678901234"), "errors lose their values: {err}");

    // What stays usable.
    let r = q(format!("SELECT c.id, c.tax_id, c.email FROM {s}.customers c WHERE c.tax_id LIKE 't%' AND c.city IN ('Recife') ORDER BY c.id")).unwrap();
    clean("plain select", &r);
    assert!(r["rows"][0][1].as_str().unwrap().starts_with("‹h:") && r["rows"][0][2] == "a•••@acme.com", "{r}");
    for sql in [
        format!("WITH x AS (SELECT id, tax_id FROM {s}.customers) SELECT tax_id FROM x"),
        format!("SELECT tax_id FROM {s}.customers UNION SELECT tax_id FROM {s}.customers"),
        format!("SELECT * FROM (SELECT tax_id FROM {s}.customers) t"),
        format!("SELECT id FROM {s}.customers WHERE id IN (SELECT id FROM {s}.customers WHERE tax_id = 'x')"),
        format!("SELECT id FROM {s}.customers WHERE tax_id IN (SELECT tax_id FROM {s}.customers)"),
    ] {
        let r = q(sql.clone()).unwrap_or_else(|err| panic!("{sql}: {err}"));
        clean(&sql, &r);
    }

    // Writes read the table itself: whole rows and copies of masked values are refused.
    refused("whole row", x(format!("UPDATE {s}.users SET name = users::text WHERE id = 1")), "whole rows");
    refused("RETURNING row", x(format!("UPDATE {s}.users SET name = 'Ana' WHERE id = 1 RETURNING users")), "whole rows");
    refused("row oracle", x(format!("DELETE FROM {s}.users WHERE users::text LIKE '%h%' AND id = 99")), "whole rows");
    refused("unfollowed", x(format!("UPDATE {s}.users SET name = substring(password_hash FROM 1 FOR 30) WHERE id = 1")), "hidden");
    refused("INSERT … TABLE", x(format!("INSERT INTO {s}.notes TABLE {s}.notes")), "TABLE");
    refused("INSERT … SELECT masked", x(format!("INSERT INTO {s}.notes (id, body) SELECT 10, tax_id FROM {s}.customers")), MASKED);
    refused("INSERT … VALUES masked", x(format!("INSERT INTO {s}.notes (id, body) VALUES (11, (SELECT tax_id FROM {s}.customers LIMIT 1))")), MASKED);
    let r = e.admin("run_query", with("pgadmin", json!({"sql": format!("SELECT (SELECT count(*) FROM {s}.notes), (SELECT name FROM {s}.users)")})));
    assert_eq!(r["rows"][0], json!([0, "Ana"]), "nothing was copied");

    // A column hidden and masked is hidden.
    refused("hidden and masked", q(format!("SELECT password_hash FROM {s}.users")), "is hidden from agents");
    let r = q(format!("SELECT * FROM {s}.users")).unwrap();
    assert!(!r.to_string().contains("password_hash"), "{r}");

    // The catalog tools, the schema resource and favorites.
    for (tool, args) in [
        ("describe_table", json!({"table": format!("{s}.users"), "include_ddl": true})),
        ("get_ddl", json!({"object": format!("{s}.users")})),
        ("search_objects", json!({"pattern": "password"})),
        ("sample_rows", json!({"table": format!("{s}.users")})),
        ("sample_rows", json!({"table": format!("{s}.customers")})),
        ("explain", json!({"sql": format!("SELECT * FROM {s}.users")})),
    ] {
        let r = e.ok(tool, with("pp", args));
        clean(tool, &r);
        assert!(!r.to_string().contains("password_hash"), "{tool}: {r}");
    }
    let listed = e.ok("list_tables", with("pp", json!({"schema": s}))).to_string();
    assert!(listed.contains("v_open") && !listed.contains("v_kept"), "{listed}");
    let call = dataman_mcp::Call::new(e.server.hub.clone(), "privacy-test".into(), "stdio", "resources/read", CancellationToken::new());
    for json in [false, true] {
        let u = dataman_mcp::resources::parse(&dataman_mcp::resources::uri("pp", "dataman", json)).unwrap();
        let text = e.server.hub.core.block_on(dataman_mcp::resources::read(&call, &u)).unwrap();
        assert!(!text.contains("password_hash"), "resource: {text}");
        if json {
            let v: Value = serde_json::from_str(&text).unwrap();
            let mine = &v["schemas"][&s];
            assert!(mine["customers"].is_object() && mine["payments"].is_null(), "{mine}");
            assert!(mine["v_open"].is_object() && mine["v_kept"].is_null(), "views follow list_tables: {mine}");
        } else {
            let mine = text.split(&format!("## {s}\n")).nth(1).unwrap().split("\n## ").next().unwrap();
            assert!(mine.contains("- customers [") && !mine.contains("- payments ["), "{mine}");
            assert!(mine.contains("- v_open [") && !mine.contains("v_kept"), "views follow list_tables: {mine}");
        }
    }
    e.ok("add_favorite", with("pp", json!({"name": "pw fav", "sql": format!("SELECT password_hash FROM {s}.users")})));
    refused("run_favorite", e.call("run_favorite", json!({"name": "pw fav"})), "is hidden from agents");

    e.admin("execute", with("pgadmin", json!({"sql": format!("DROP SCHEMA {s} CASCADE")})));
    e.nothing_kept_logged();
    e.done();
}

#[test]
fn mysql_and_sql_server_values_stay_kept() {
    let s = format!("mcp_paths_{}", std::process::id());
    if up(MY) {
        let rules = "[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{ col = \"customers.email\", how = \"hash\" }]\n[connection.mcp_rules.visible]\nitems = [{ name = \"users.password_hash\", kind = \"column\" }, { name = \"payments\", kind = \"table\" }]\n";
        let e = env(&[conn("myadmin", MY, "mysql", "root", "mysql", "mcp = \"full\""), conn("mm", MY, "mysql", "root", &s, rules)], "", None);
        let a = |sql: String| e.admin("execute", with("myadmin", json!({"sql": sql})));
        a(format!("CREATE DATABASE {s}"));
        a(format!("CREATE TABLE {s}.users (id int PRIMARY KEY, name text, password_hash varchar(50) DEFAULT 'dflt-secret')"));
        a(format!("CREATE TABLE {s}.customers (id int PRIMARY KEY, name text, email text)"));
        a(format!("CREATE TABLE {s}.payments (id int PRIMARY KEY, card text)"));
        a(format!("INSERT INTO {s}.users VALUES (1, 'Ana', 'hash-secret-1'); INSERT INTO {s}.customers VALUES (1, 'Ana', 'zed-secret@acme.com'); INSERT INTO {s}.payments VALUES (1, 'card-secret')"));
        let q = |sql: &str| e.call("run_query", with("mm", json!({"sql": sql})));
        for sql in ["TABLE users", "SHOW CREATE TABLE users", "SHOW CREATE TABLE payments", "SHOW COLUMNS FROM users", "DESC users", "DESCRIBE users", "EXPLAIN users", "SHOW TABLES", "SHOW INDEX FROM payments"] {
            refused(sql, q(sql), "is not available to agents on mm");
        }
        for sql in [
            "SELECT (SELECT email FROM customers LIMIT 1) AS x",
            "SELECT name FROM customers UNION SELECT email FROM customers",
            "SELECT JSON_OBJECT('e', email) FROM customers",
            "SELECT GROUP_CONCAT(email) FROM customers",
            "SELECT id FROM customers WHERE EXTRACTVALUE(1, CONCAT(0x7e, email))",
            "SELECT id FROM customers WHERE email + 0 = 0",
            "VALUES ROW((SELECT email FROM customers LIMIT 1))",
        ] {
            refused(sql, q(sql), MASKED);
        }
        let r = q("SELECT * FROM users").unwrap();
        assert_eq!(r["columns"].as_array().unwrap().len(), 2, "{r}");
        let r = q("SELECT id, email FROM customers WHERE email LIKE 'z%'").unwrap();
        clean("mysql select", &r);
        assert!(r["rows"][0][1].as_str().unwrap().starts_with("‹h:"), "{r}");
        assert!(q("SHOW VARIABLES LIKE 'version'").is_ok(), "SHOW of settings stays");
        a(format!("DROP DATABASE {s}"));
        e.nothing_kept_logged();
        e.done();
    }
    if up(MS) {
        let rules = format!(
            "[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"{s}_customers.email\", how = \"hash\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"{s}_users.password_hash\", kind = \"column\" }}]\n"
        );
        let e = env(&[conn("msadmin", MS, "sql-server", "sa", "tempdb", "mcp = \"full\""), conn("ss", MS, "sql-server", "sa", "tempdb", rules)], "", None);
        e.admin("execute", with("msadmin", json!({"sql": format!("CREATE TABLE dbo.{s}_users (id int, name varchar(50), password_hash varchar(50)); CREATE TABLE dbo.{s}_customers (id int, name varchar(50), email varchar(50))")})));
        e.admin("execute", with("msadmin", json!({"sql": format!("INSERT INTO dbo.{s}_users VALUES (1, 'Ana', 'hash-secret-1'); INSERT INTO dbo.{s}_customers VALUES (1, 'Ana', 'zed-secret@acme.com')")})));
        let q = |sql: String| e.call("run_query", with("ss", json!({"sql": sql})));
        refused("FOR JSON", q(format!("SELECT email FROM dbo.{s}_customers FOR JSON PATH")), "FOR JSON");
        refused("FOR XML", q(format!("SELECT email FROM dbo.{s}_customers FOR XML PATH")), "FOR JSON");
        refused("scalar", q(format!("SELECT (SELECT TOP 1 email FROM dbo.{s}_customers) AS x")), MASKED);
        refused("union", q(format!("SELECT name FROM dbo.{s}_customers UNION SELECT email FROM dbo.{s}_customers")), MASKED);
        refused("alias =", q(format!("SELECT x = email FROM dbo.{s}_customers")), MASKED);
        refused("histogram", q(format!("SELECT * FROM sys.dm_db_stats_histogram(OBJECT_ID('dbo.{s}_users'), 1)")), "system catalog");
        let r = q(format!("SELECT * FROM dbo.{s}_users")).unwrap();
        assert_eq!(r["columns"].as_array().unwrap().len(), 2, "{r}");
        clean("mssql select", &q(format!("SELECT id, email FROM dbo.{s}_customers WHERE email = 'x' OR id = 1")).unwrap());
        e.admin("execute", with("msadmin", json!({"sql": format!("DROP TABLE dbo.{s}_users; DROP TABLE dbo.{s}_customers")})));
        e.nothing_kept_logged();
        e.done();
    }
}

#[test]
fn mongodb_values_stay_kept() {
    if !up(MG) {
        return;
    }
    let s = format!("mcp_paths_{}", std::process::id());
    let rules = format!(
        "mcp = \"write\"\n[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"customers.email\", how = \"partial\" }}, {{ col = \"customers.tax_id\", how = \"hash\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"{s}.payments\", kind = \"table\" }}, {{ name = \"users.ssn\", kind = \"column\" }}]\n"
    );
    let e = env(&[conn("mgadmin", MG, "mongodb", "root", &s, "mcp = \"full\""), conn("mg", MG, "mongodb", "root", &s, rules)], "", None);
    let a = |q: &str| e.admin("nosql_execute", with("mgadmin", json!({"query": q})));
    a("db.users.insertOne({_id: 1, name: \"Ana\", ssn: \"ssn-secret-1\", kids: [{ssn: \"kid-secret\"}]})");
    a("db.customers.insertOne({_id: 1, email: \"zed-secret@acme.com\", tax_id: \"taxid-secret-9\", city: \"Recife\"})");
    a("db.payments.insertOne({_id: 1, card: \"card-secret\"})");
    a("db.orders.insertOne({_id: 1, uid: 1, cid: 1, pid: 1})");
    a("db.createView(\"uview\", \"users\", [{$project: {s: \"$ssn\"}}])");
    a("db.createView(\"vv\", \"uview\", [])");
    let q = |t: &str| e.call("nosql_query", with("mg", json!({"query": t})));

    // Other collections a pipeline reads: hidden ones refused however they are written.
    for t in [
        "db.orders.aggregate([{\"$lookup\": {\"from\": \"payments\", \"localField\": \"pid\", \"foreignField\": \"_id\", \"as\": \"p\"}}])",
        "db.orders.aggregate([{$lookup: {from: \"pay\\u006dents\", localField: \"pid\", foreignField: \"_id\", as: \"p\"}}])",
        "db.orders.aggregate([{$unionWith: {\"coll\": \"payments\"}}])",
        "db.orders.aggregate([{$facet: {a: [{$unionWith: \"payments\"}]}}])",
    ] {
        refused(t, q(t), "payments is not visible");
    }
    // …and what they keep of the others comes back stripped or masked.
    for t in [
        "db.orders.aggregate([{$lookup: {from: \"users\", localField: \"uid\", foreignField: \"_id\", as: \"u\"}}])",
        "db.orders.aggregate([{$unionWith: \"users\"}])",
        "db.orders.aggregate([{$lookup: {from: \"customers\", localField: \"cid\", foreignField: \"_id\", as: \"c\"}}])",
        "db.orders.aggregate([{$unionWith: {coll: \"customers\"}}])",
    ] {
        let r = q(t).unwrap_or_else(|err| panic!("{t}: {err}"));
        clean(t, &r);
        assert!(r.to_string().contains("Ana") || r.to_string().contains("Recife"), "{t}: {r}");
    }
    // Escapes are read as the server reads them; field names computed at run time are refused.
    for (t, want) in [
        ("db.users.find({\"\\u0073sn\": \"x\"})", "hidden"),
        ("db.users.aggregate([{$project: {x: \"\\u0024ssn\"}}])", "hidden"),
        ("db.users.find({}, {x: \"\\u0024ssn\"})", "hidden"),
        ("db.users.aggregate([{$project: {x: {\"\\u0024objectToArray\": \"\\u0024\\u0024ROOT\"}}}])", "whole documents"),
        ("db.users.aggregate([{$project: {x: {$getField: {$concat: [\"s\", \"sn\"]}}}}])", "$getField"),
        ("db.customers.aggregate([{$project: {e: {$getField: {$concat: [\"c\", \"pf\"]}}}}])", "$getField"),
        ("db.users.find({$where: \"this['s'+'sn'] == 'x'\"})", "$where"),
        ("db.customers.find({}, {e: \"$tax_id\"})", MASKED),
        ("db.customers.find({$expr: {$eq: [{$toInt: \"$tax_id\"}, 1]}})", MASKED),
        ("db.customers.distinct(\"tax_id\")", MASKED),
        ("db.uview.find({})", "view that reads users"),
        ("db.vv.find({})", "view that reads users"),
        ("db.orders.aggregate([{$lookup: {from: \"uview\", localField: \"uid\", foreignField: \"_id\", as: \"u\"}}])", "view that reads users"),
    ] {
        refused(t, q(t), want);
    }
    let r = q("db.customers.find({tax_id: \"x\"})").unwrap();
    clean("find", &r);
    let r = q("db.customers.find({city: \"Recife\"}, {email: 1, tax_id: 1})").unwrap();
    clean("find", &r);
    assert_eq!(r["documents"][0]["email"], "z•••@acme.com", "{r}");
    clean("users", &q("db.users.find({})").unwrap());
    clean("sample_rows", &e.ok("sample_rows", with("mg", json!({"table": "users"}))));
    refused("sample of a view", e.call("sample_rows", with("mg", json!({"table": "uview"}))), "view that reads users");
    a("db.dropDatabase()");
    e.nothing_kept_logged();
    e.done();
}

#[test]
fn search_values_stay_kept() {
    if !up(ES) {
        return;
    }
    let s = format!("mcp_paths_{}", std::process::id());
    let rules = format!(
        "[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"{s}-cust.email\", how = \"hash\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"{s}-pay*\", kind = \"table\" }}, {{ name = \"{s}-cust.card\", kind = \"column\" }}]\n"
    );
    let cols = format!("[connection.mcp_rules.visible]\nitems = [{{ name = \"{s}-cust.card\", kind = \"column\" }}]\n");
    let e = env(&[conn("esadmin", ES, "elasticsearch", "elastic", "", "mcp = \"full\""), conn("es", ES, "elasticsearch", "elastic", "", rules), conn("esc", ES, "elasticsearch", "elastic", "", cols)], "", None);
    let a = |t: String| e.admin("nosql_execute", with("esadmin", json!({"query": t})));
    a(format!("PUT {s}-cust/_doc/1?refresh=true\n{{\"name\": \"Ana\", \"card\": \"4242cardsecret\", \"email\": \"zed-secret@acme.com\"}}"));
    a(format!("PUT {s}-pay1/_doc/1?refresh=true\n{{\"amount\": 1, \"note\": \"card-secret\"}}"));
    a(format!("POST _aliases\n{{\"actions\": [{{\"add\": {{\"index\": \"{s}-pay1\", \"alias\": \"{s}-al\"}}}}, {{\"add\": {{\"index\": \"{s}-cust\", \"alias\": \"{s}-alc\"}}}}]}}"));
    let q = |c: &str, t: String| e.call("nosql_query", with(c, json!({"query": t})));
    for (t, want) in [
        (format!("GET {s}-cust/_search?q=card:4242*"), "hidden"),
        (format!("GET {s}-cust/_search?q=4242*"), "searches over every field"),
        (format!("GET {s}-cust/_search\n{{\"script_fields\": {{\"x\": {{\"script\": \"doc['card.keyword'].value\"}}}}}}"), "scripts"),
        (format!("GET {s}-cust/_search\n{{\"runtime_mappings\": {{\"x\": {{\"type\": \"keyword\", \"script\": \"emit(doc['card.keyword'].value)\"}}}}, \"fields\": [\"x\"]}}"), "scripts"),
        (format!("GET {s}-cust/_search\n{{\"size\": 0, \"aggs\": {{\"x\": {{\"terms\": {{\"script\": \"doc['email.keyword'].value\"}}}}}}}}"), "scripts"),
        (format!("GET {s}-cust/_search\n{{\"query\": {{\"query_string\": {{\"query\": \"card:4242*\"}}}}}}"), "hidden"),
        (format!("GET {s}-cust/_search\n{{\"query\": {{\"multi_match\": {{\"query\": \"4242\"}}}}}}"), "searches over every field"),
        (format!("GET {s}-cust/_search\n{{\"query\": {{\"exists\": {{\"field\": \"car*\"}}}}}}"), "searches over every field"),
        (format!("GET {s}-cust/_mget\n{{\"docs\": [{{\"_id\": \"1\"}}, {{\"_index\": \"{s}-pay1\", \"_id\": \"1\"}}]}}"), "not visible"),
        (format!("GET {s}-cust/_msearch\n{{\"index\": [\"{s}-pay1\"]}}\n{{\"query\": {{\"match_all\": {{}}}}}}"), "not visible"),
        (format!("GET {s}-cust/_termvectors/1?fields=email"), "term vectors"),
        (format!("GET {s}-cust/_search/template\n{{\"source\": \"{{}}\"}}"), "templates"),
        (format!("GET {s}-cust/_search?sort=email.keyword:asc"), MASKED),
        (format!("GET {s}-al/_search"), "alias of"),
    ] {
        refused(&t, q("es", t.clone()), want);
    }
    for t in [format!("GET {s}-cust/_search\n{{\"fields\": [\"*\"]}}"), format!("GET {s}-cust/_search"), format!("GET {s}-cust/_search\n{{\"query\": {{\"match\": {{\"name\": \"card holder\"}}}}}}"), format!("GET {s}-alc/_search\n{{\"fields\": [\"*\"]}}"), format!("GET {s}-cust/_field_caps?fields=*")] {
        let r = q("es", t.clone()).unwrap_or_else(|err| panic!("{t}: {err}"));
        clean(&t, &r);
        assert!(!r.to_string().contains("card"), "{t}: {r}");
    }
    // Rules on columns only: a wildcard may stand for the index, so its rules apply. The agent
    // sees Data there, so the e-mail comes as stored: types mask in Masked data only.
    let r = q("esc", format!("GET {s}-c*/_search\n{{\"fields\": [\"*\"]}}")).unwrap();
    let text = r.to_string();
    assert!(text.contains("Ana") && !text.contains("card") && text.contains("zed-secret@acme.com"), "{r}");
    let _ = e.call("nosql_execute", with("esadmin", json!({"query": format!("DELETE {s}-cust")})));
    e.human.allow.lock().extend([true; 2]);
    let _ = e.call("nosql_execute", with("esadmin", json!({"query": format!("DELETE {s}-pay1")})));
    e.nothing_kept_logged();
    e.done();
}

#[test]
fn redis_values_stay_kept() {
    if !up(RD) {
        return;
    }
    let s = format!("mcp_paths_{}", std::process::id());
    let rules = format!(
        "mcp = \"write\"\n[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"{s}:user:*.email\", how = \"hash\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"{s}:session:*\", kind = \"table\" }}, {{ name = \"{s}:user:*.password\", kind = \"column\" }}]\n"
    );
    let only = format!("mcp = \"write\"\n[connection.mcp_rules.visible]\nmode = \"only\"\nitems = [{{ name = \"{s}:open:*\", kind = \"table\" }}]\n");
    let e = env(&[conn("rdadmin", RD, "redis", "", "9", "mcp = \"full\""), conn("rd", RD, "redis", "", "9", rules), conn("ro", RD, "redis", "", "9", only)], "", None);
    let a = |t: String| e.admin("nosql_execute", with("rdadmin", json!({"query": t})));
    a(format!("HSET {s}:user:1 name Ana password pw-secret email zed-secret@acme.com"));
    a(format!("SET {s}:session:1 sess-secret"));
    a(format!("SADD {s}:closed:set closed-secret"));
    let q = |c: &str, t: String| e.call("nosql_query", with(c, json!({"query": t})));
    // Masked fields come back masked even without their names.
    for t in [format!("HGET {s}:user:1 email"), format!("HMGET {s}:user:1 email name"), format!("HGETALL {s}:user:1"), format!("HSCAN {s}:user:1 0")] {
        let r = q("rd", t.clone()).unwrap_or_else(|err| panic!("{t}: {err}"));
        clean(&t, &r);
        assert!(r.to_string().contains("‹h:"), "{t}: {r}");
    }
    refused("HVALS", q("rd", format!("HVALS {s}:user:1")), "HMGET");
    refused("OBJECT", q("rd", format!("OBJECT ENCODING {s}:session:1")), "not visible");
    // Only mode: every key a command reads or writes is checked, not its first.
    for (tool, t) in [
        ("nosql_query", format!("SINTER {s}:open:x {s}:closed:set")),
        ("nosql_execute", format!("SUNIONSTORE {s}:open:dst {s}:closed:set")),
        ("nosql_execute", format!("ZUNIONSTORE {s}:open:z 1 {s}:closed:set")),
        ("nosql_execute", format!("COPY {s}:closed:set {s}:open:cp")),
        ("nosql_execute", format!("SMOVE {s}:closed:set {s}:open:mv closed-secret")),
    ] {
        refused(&t, e.call(tool, with("ro", json!({"query": t.clone()}))), "not visible");
    }
    assert_eq!(e.admin("nosql_query", with("rdadmin", json!({"query": format!("EXISTS {s}:open:dst {s}:open:z {s}:open:cp {s}:open:mv")})))["value"], 0, "nothing was copied");
    a(format!("DEL {s}:user:1 {s}:session:1 {s}:closed:set"));
    e.done();
}

/// User tools (mcp-tools.toml) run for agents: the connection's rules and dialect apply to them.
#[test]
fn user_tools_follow_the_rules() {
    let tools = "[[tool]]\nname = \"all_users\"\ndescription = \"d\"\nconnection = \"pp\"\nsql = \"SELECT * FROM {s}.users\"\n\n\
                 [[tool]]\nname = \"hashes\"\ndescription = \"d\"\nconnection = \"pp\"\nsql = \"SELECT password_hash FROM {s}.users\"\n\n\
                 [[tool]]\nname = \"tax_ids\"\ndescription = \"d\"\nconnection = \"pp\"\nsql = \"SELECT id, tax_id FROM {s}.customers\"\n\n\
                 [[tool]]\nname = \"renamed\"\ndescription = \"d\"\nconnection = \"pp\"\nsql = \"SELECT upper(tax_id) AS c FROM {s}.customers\"\n";
    // Statements are read in their connection's dialect when the tools load.
    let d = |c: &str| match c {
        "my" => Some(dataman_core::Dialect::MySql),
        "pg" => Some(dataman_core::Dialect::Postgres),
        _ => None,
    };
    let two = "[[tool]]\nname = \"t\"\ndescription = \"d\"\nconnection = \"{c}\"\nsql = \"SELECT 1 /*!; DELETE FROM t */\"\n";
    let (ok, errs) = dataman_mcp::user::load(&two.replace("{c}", "pg"), &[], &d);
    assert_eq!((ok.len(), errs.len()), (1, 0), "PostgreSQL reads one statement: {errs:?}");
    let (ok, errs) = dataman_mcp::user::load(&two.replace("{c}", "my"), &[], &d);
    assert!(ok.is_empty() && errs[0].contains("exactly one statement"), "MySQL runs two: {errs:?}");
    let any = "[[tool]]\nname = \"t\"\ndescription = \"d\"\nconnection = \"any\"\nsql = \"SELECT `a` FROM t\"\n";
    assert_eq!(dataman_mcp::user::load(any, &[], &d).0.len(), 1, "connection any: valid in one dialect is enough at load");

    if !up(PG) {
        return;
    }
    let s = format!("mcp_tools_{}", std::process::id());
    let rules = format!(
        "[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"*tax_id*\", how = \"hash\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"users.password_hash\", kind = \"column\" }}]\n"
    );
    let e = env(&[conn("pgadmin", PG, "", "postgres", "dataman", "mcp = \"full\""), conn("pp", PG, "", "postgres", "dataman", rules)], "", Some(&tools.replace("{s}", &s)));
    e.admin(
        "execute",
        with("pgadmin", json!({"sql": format!(
            "CREATE SCHEMA {s}; CREATE TABLE {s}.users (id int, name text, password_hash text); CREATE TABLE {s}.customers (id int, tax_id text); \
             INSERT INTO {s}.users VALUES (1, 'Ana', 'hash-secret-1'); INSERT INTO {s}.customers VALUES (1, 'taxid-secret-9')"
        )})),
    );
    let r = e.ok("all_users", json!({}));
    clean("all_users", &r);
    assert!(!r.to_string().contains("password_hash") && r["narrowed"].is_string(), "{r}");
    refused("hashes", e.call("hashes", json!({})), "is hidden from agents");
    let r = e.ok("tax_ids", json!({}));
    clean("tax_ids", &r);
    assert!(r["rows"][0][1].as_str().unwrap().starts_with("‹h:"), "{r}");
    refused("renamed", e.call("renamed", json!({})), MASKED);
    e.admin("execute", with("pgadmin", json!({"sql": format!("DROP SCHEMA {s} CASCADE")})));
    e.nothing_kept_logged();
    e.done();
}

/// The SQL inside an Elasticsearch or OpenSearch request is read as that server reads it:
/// Elasticsearch nests block comments, OpenSearch does not.
#[test]
fn search_sql_in_the_servers_dialect() {
    let req = |path: &str| format!("POST {path}\n{{\"query\": \"/* /* */ DELETE FROM t WHERE a = 1 -- */\"}}");
    let os = dataman_core::nosql::classify(Engine::OpenSearch, &req("_plugins/_sql")).unwrap();
    assert!(os.is_write(), "OpenSearch runs the DELETE: {os:?}");
    let es = dataman_core::nosql::classify(Engine::Elasticsearch, &req("_sql")).unwrap();
    assert!(!es.is_write(), "Elasticsearch reads one comment: {es:?}");
    let backquoted = "POST _plugins/_sql\n{\"query\": \"SELECT `a` FROM x WHERE b = 'c;d'\"}";
    assert!(!dataman_core::nosql::classify(Engine::OpenSearch, backquoted).unwrap().is_write(), "a read stays a read");
}

fn target(access: McpAccess, env: Environment, read_only: bool, yolo: bool) -> Target {
    Target { access, env, read_only, dialect: dataman_core::Dialect::Postgres, yolo }
}

/// One order, whatever the combination: (1) MCP switched off, (2) the connection's tier off,
/// (3) a read-only database refuses every write, (4) the connection's agent rules
/// (Privacy, Permissions) refuse or narrow before any tier decision, and nothing loosens them,
/// (5) allowed queries and YOLO run without asking, on production and on a read tier too,
/// (6) production asks for every write, (7) the tier: read refuses writes, write runs
/// non-destructive ones, full asks for destructive ones.
#[test]
fn rules_have_one_precedence() {
    use McpAccess::*;
    let del = "DELETE FROM t";
    let upd = "UPDATE t SET a = 1 WHERE id = 2";
    let allowed = vec![upd.to_string(), del.to_string()];
    for env in Environment::ALL {
        for access in [Off, Read, Write, Full] {
            for ro in [false, true] {
                for yolo in [false, true] {
                    for list in [&[][..], &allowed[..]] {
                        let t = target(access, env, ro, yolo && access != Off);
                        for sql in [upd, del] {
                            let j = policy::judge(true, t, sql, list).verdict;
                            let why = format!("{access:?} {env:?} ro={ro} yolo={yolo} allowlist={} {sql}", !list.is_empty());
                            assert!(matches!(policy::judge(false, t, sql, list).verdict, Verdict::Refuse(_)), "(1) {why}");
                            if access == Off {
                                assert!(matches!(j, Verdict::Refuse(_)), "(2) {why}");
                            } else if ro {
                                // A read tier says so first, unless an allowlist or YOLO would let it write.
                                let read_tier = access == Read && !yolo && list.is_empty();
                                assert!(matches!(&j, Verdict::Refuse(e) if read_tier || e.contains("read-only")), "(3) {why}: {j:?}");
                            } else if yolo || !list.is_empty() {
                                assert_eq!(j, Verdict::Run, "(5) {why}");
                            } else if env.is_production() && access != Read {
                                assert!(matches!(&j, Verdict::Ask(r) if r.iter().any(|x| x == "production")) || (access == Write && sql == del), "(6) {why}: {j:?}");
                            } else {
                                match (access, sql == del) {
                                    (Read, _) | (Write, true) => assert!(matches!(j, Verdict::Refuse(_)), "(7) {why}: {j:?}"),
                                    (_, false) => assert_eq!(j, Verdict::Run, "(7) {why}"),
                                    _ => assert!(matches!(j, Verdict::Ask(_)), "(7) {why}: {j:?}"),
                                }
                            }
                        }
                        assert!(policy::allow(true, t, Need::Read).is_ok() == (access != Off), "reads follow the tier only");
                    }
                }
            }
        }
    }

    // (4) The agent rules run before the tier on every write path: a write naming a hidden
    // column is refused on a full, production connection without asking anyone, and so is DDL
    // with structure changes off.
    if up(PG) {
        let s = format!("mcp_prec_{}", std::process::id());
        let rules = "mcp = \"full\"\nenv = \"production\"\n[connection.mcp_rules]\nview = \"masked\"\nstructure = false\n\
                     sensitive = [{ col = \"users.secret\", how = \"partial\" }]\n[connection.mcp_rules.visible]\nitems = [{ name = \"users.secret\", kind = \"column\" }]\n";
        let e = env(&[conn("pgadmin", PG, "", "postgres", "dataman", "mcp = \"full\""), conn("prod", PG, "", "postgres", "dataman", rules)], "", None);
        e.admin("execute", with("pgadmin", json!({"sql": format!("CREATE SCHEMA {s}; CREATE TABLE {s}.users (id int, secret text)")})));
        e.human.asked.lock().clear();
        refused("hidden on production", e.call("execute", with("prod", json!({"sql": format!("UPDATE {s}.users SET secret = 'x' WHERE id = 1")}))), "is hidden from agents");
        refused("structure off", e.call("execute", with("prod", json!({"sql": format!("CREATE TABLE {s}.t (a int)")}))), "changing structure");
        assert!(e.human.asked.lock().is_empty(), "nobody is asked for what the rules refuse");
        e.human.allow.lock().push_back(false);
        refused("production asks", e.call("execute", with("prod", json!({"sql": format!("UPDATE {s}.users SET id = 2 WHERE id = 1")}))), "denied");
        assert_eq!(e.human.asked.lock().len(), 1, "a write the rules let through asks on production");
        e.admin("execute", with("pgadmin", json!({"sql": format!("DROP SCHEMA {s} CASCADE")})));
        e.done();
    }
    let r = dataman_core::privacy::AgentRules {
        view: dataman_core::privacy::AgentView::Masked,
        visible: dataman_core::privacy::Visible { mode: dataman_core::privacy::VisibleMode::Except, items: vec![dataman_core::privacy::VisibleItem { name: "users.secret".into(), kind: dataman_core::privacy::ItemKind::Column }] },
        sensitive: vec![dataman_core::privacy::SensitiveCol { col: "users.secret".into(), how: Mask::Partial, on: true }],
        ..Default::default()
    };
    let tables = |_: Option<&str>, n: &str| (n == "users").then(|| dataman_core::agent_sql::Table { schema: "public".into(), name: "users".into(), columns: vec!["id".into(), "secret".into()] });
    let routines = Default::default();
    let cx = dataman_core::agent_sql::Context { rules: &r, connection: "c", dialect: dataman_core::Dialect::Postgres, tables: &tables, routines: &routines };
    assert!(dataman_core::agent_sql::check("SELECT secret FROM users", false, &cx).unwrap_err().contains("hidden"), "hidden wins over masked");
    let c = dataman_core::agent_sql::check("SELECT * FROM users", false, &cx).unwrap();
    assert!(!c.sql.contains("secret,") && c.sql.contains("(SELECT id FROM public.users)"), "{}", c.sql);

    // Sensitive types: one list for every connection; a value matched by a built-in and a
    // custom type gets the earliest, then longest, match, and on a tie the built-in's mask.
    let key = HashKey::fixed(b"k");
    let mut global = TypeSettings::default();
    global.custom.push(CustomType::new("Acme mail", r"[a-z]+@acme\.com", Mask::Hidden));
    let scrub = |t: &Types, s: &str| t.scrub(s, &key, &mut Default::default()).into_owned();
    let all = Types::new(&global);
    assert_eq!(scrub(&all, "mail ana@acme.com"), "mail a•••@acme.com", "the built-in e-mail wins the tie");
    assert_eq!(scrub(&all, "mail x.ana@acme.com"), "mail x•••@acme.com", "the longer match wins");
    let mut off = global.clone();
    off.set_builtin("email", Some(false), None);
    assert_eq!(scrub(&Types::new(&off), "mail ana@acme.com"), "mail ‹acme mail›", "e-mail switched off, the custom type stays");
    off.custom[0].on = false;
    assert_eq!(scrub(&Types::new(&off), "mail ana@acme.com"), "mail ana@acme.com");
    let mut quiet = global.clone();
    quiet.custom[0].on = false;
    assert_eq!(scrub(&Types::new(&quiet), "mail ana@acme.com"), "mail a•••@acme.com");
    // A connection saved with its own switches (an older version's) loads, and they do nothing.
    let old: dataman_core::privacy::AgentRules = toml::from_str("[types]\nemail = false\n").unwrap();
    assert!(old.is_default(), "{old:?}");
}

/// Custom patterns: what cannot be used is refused when saved, and refuses reads while it is on;
/// a pattern cannot hang a reply.
#[test]
fn custom_patterns_at_their_edges() {
    for p in ["", "   ", ".*", "a*", "(?:)", "x?"] {
        assert!(privacy::check_pattern(p).unwrap_err().contains("empty text"), "{p:?}");
    }
    assert!(privacy::check_pattern("(").is_err());
    assert!(privacy::check_pattern("(?=a)b").unwrap_err().contains("Look-around"));
    assert!(privacy::check_pattern("(a)\\1").unwrap_err().contains("Back-references"));
    assert!(privacy::check_pattern(&"a".repeat(501)).unwrap_err().contains("too large"));
    assert!(privacy::check_pattern("(?:a{1,1000}){1,1000}").unwrap_err().contains("too large"));
    // Matches everything: allowed, flagged as broad by the editor's test, masks every value.
    privacy::check_pattern(".+").unwrap();
    let all = CustomType::new("All", ".+", Mask::Hidden);
    assert!(privacy::test_pattern(&all, &["a b c", "d e f"], &HashKey::fixed(b"k")).unwrap().broad);
    // Backtracking patterns run in linear time: a long value is checked well within budget.
    let mut slow = TypeSettings::default();
    slow.custom.push(CustomType::new("Slow", "(a+)+$x", Mask::Hidden));
    slow.custom.push(CustomType::new("Slow2", "(a|aa)+b", Mask::Hidden));
    let t = Types::new(&slow);
    assert!(t.broken.is_empty());
    let mut v = json!({"cell": "a".repeat(200_000)});
    let started = std::time::Instant::now();
    let r = t.scrub_json(&mut v, &HashKey::fixed(b"k"), &mut Default::default(), std::time::Instant::now() + privacy::BUDGET);
    assert!(r.is_ok() && started.elapsed() < privacy::BUDGET * 2, "{:?}", started.elapsed());
    // A broken pattern switched on refuses reads; one switched off does not.
    let mut broken = TypeSettings::default();
    broken.custom.push(CustomType::new("Broken", "(", Mask::Hidden));
    assert_eq!(Types::new(&broken).broken, ["Broken"]);
    broken.custom[0].on = false;
    assert!(Types::new(&broken).broken.is_empty());
}

/// Rules and types are read again when the user changes them, while the server runs.
#[test]
fn rule_changes_apply_at_once() {
    let nowhere = ("DATAMAN_NOWHERE", "127.0.0.1:9");
    let data = "mcp = \"read\"";
    let structure = "mcp = \"read\"\n[connection.mcp_rules]\nview = \"structure\"\n";
    let e = env(&[conn("conn-alpha", nowhere, "", "postgres", "x", data)], "", None);
    let err = e.call("run_query", with("conn-alpha", json!({"sql": "SELECT 1"}))).unwrap_err();
    assert!(!err.contains("structure only"), "{err}");
    let write = |text: String| {
        // A rewrite the way the app saves: a new file in its place.
        let tmp = e.cfg().join("tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, e.cfg().join("connections.toml")).unwrap();
    };
    write(profiles(&[conn("conn-alpha", nowhere, "", "postgres", "x", structure)]));
    assert!(e.call("run_query", with("conn-alpha", json!({"sql": "SELECT 1"}))).unwrap_err().contains("structure only"));
    write(profiles(&[conn("conn-alpha", nowhere, "", "postgres", "x", data)]));
    assert!(!e.call("run_query", with("conn-alpha", json!({"sql": "SELECT 1"}))).unwrap_err().contains("structure only"));

    let settings = |p: &str| format!("[agent_types]\n[[agent_types.custom]]\nid = \"t1\"\nname = \"Conn\"\npattern = \"{p}\"\nmask = \"hidden\"\n");
    let names = || e.ok("update_connection", with("conn-alpha", json!({}))).to_string();
    std::fs::write(e.cfg().join("settings.toml"), settings("conn-alpha")).unwrap();
    assert!(!names().contains("‹conn›"), "the agent sees Data: nothing is masked");
    write(profiles(&[conn("conn-alpha", nowhere, "", "postgres", "x", "mcp = \"read\"\n[connection.mcp_rules]\nview = \"masked\"\n")]));
    assert!(names().contains("‹conn›"), "Masked data, read again: the type masks");
    std::fs::write(e.cfg().join("settings.toml"), settings("conn-zzz")).unwrap();
    assert!(!names().contains("‹conn›"), "the compiled types follow the new pattern");
    std::fs::write(e.cfg().join("settings.toml"), settings("(")).unwrap();
    assert!(e.call("update_connection", with("conn-alpha", json!({}))).unwrap_err().contains("invalid pattern"));
    e.done();
}

/// Each call is logged with what happened to it; agent-chosen text is shown neutralized.
#[test]
fn audit_records_each_call() {
    assert!(dataman_mcp::shown_client("Dataman\u{202E}nimda").contains("⟨U+202E⟩"));
    assert!(dataman_mcp::shown_client(&"x".repeat(500)).chars().count() <= 49);
    assert!(dataman_mcp::shown_text("SELECT 1\u{200B};\n\n\n\n\n\n\n\n\n\n\n\nDROP TABLE t").contains("blank lines"));
    if !up(PG) {
        return;
    }
    let s = format!("mcp_audit_{}", std::process::id());
    let rules = "mcp = \"full\"\n[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{ col = \"*tax_id*\", how = \"hash\" }]\n";
    let e = env(&[conn("pgadmin", PG, "", "postgres", "dataman", "mcp = \"full\""), conn("pg", PG, "", "postgres", "dataman", rules)], "", None);
    e.admin("execute", with("pgadmin", json!({"sql": format!("CREATE SCHEMA {s}; CREATE TABLE {s}.c (id int PRIMARY KEY, tax_id text UNIQUE); INSERT INTO {s}.c VALUES (1, 'taxid-secret-9')")})));
    let client = "Agent\u{202E}x";
    let _ = e.call_as(client, "run_query", with("pg", json!({"sql": format!("SELECT id, tax_id FROM {s}.c")})));
    let _ = e.call_as(client, "run_query", with("pg", json!({"sql": format!("SELECT upper(tax_id) FROM {s}.c")})));
    let _ = e.call_as(client, "run_query", with("pg", json!({"sql": "SELECT 1/0"})));
    let _ = e.call_as(client, "run_query", json!({"sql": "SELECT 1"}));
    // A duplicate key: the error quotes the value; the log keeps it cleaned.
    let _ = e.call_as(client, "execute", with("pg", json!({"sql": format!("INSERT INTO {s}.c (id) VALUES (1)")})));
    e.human.allow.lock().push_back(false);
    let _ = e.call_as(client, "execute", with("pg", json!({"sql": format!("DELETE FROM {s}.c")})));
    let mine: Vec<Value> = e.audit().into_iter().filter(|x| x["client"] == client).collect();
    let got: Vec<(String, String)> = mine.iter().map(|x| (x["tool"].as_str().unwrap().to_string(), x["outcome"].as_str().unwrap().to_string())).collect();
    assert_eq!(
        got,
        [("run_query", "ok"), ("run_query", "refused"), ("run_query", "error"), ("run_query", "invalid"), ("execute", "error"), ("execute", "refused")].map(|(a, b)| (a.to_string(), b.to_string())),
        "{mine:?}"
    );
    assert_eq!(mine[0]["seen"]["masked"][0][0], "tax_id", "{}", mine[0]);
    assert_eq!(mine[5]["approval"]["decision"], "denied", "{}", mine[5]);
    assert!(mine[1]["error"].as_str().unwrap().contains(MASKED));
    assert!(mine[4]["error"].as_str().unwrap().contains("‹value›"), "{}", mine[4]);
    e.admin("execute", with("pgadmin", json!({"sql": format!("DROP SCHEMA {s} CASCADE")})));
    e.nothing_kept_logged();
    e.done();
}

/// A generated (computed) column is protected as the strongest of the columns it is computed
/// from, on every engine and through every tool: hidden from a hidden column, masked from a
/// masked one.
#[test]
fn generated_columns_follow_their_sources() {
    let s = format!("mcp_gen_{}", std::process::id());
    let check = |e: &Env, c: &str, table: &str, star: &str| {
        let q = |sql: String| e.call("run_query", with(c, json!({"sql": sql})));
        let r = q(star.to_string()).unwrap_or_else(|err| panic!("{c}: {err}"));
        let cols = r["columns"].to_string();
        assert!(!cols.contains("pw") && cols.contains("email_up") && cols.contains("name_up"), "{c}: {r}");
        let text = r.to_string();
        assert!(!text.contains("ZED-SECRET") && !text.contains("zed-secret") && text.contains("ANA"), "{c}: {r}");
        for col in ["pw_len", "pw_v"] {
            if !(c == "mm" || c == "ss") || col == "pw_len" {
                refused(col, q(format!("SELECT {col} FROM {table}")), "is hidden from agents");
            }
        }
        let r = q(format!("SELECT email_up FROM {table}")).unwrap();
        assert!(r["rows"][0][0].as_str().unwrap().starts_with("‹h:"), "{c}: masked as the strongest of its sources: {r}");
        refused("computed", q(format!("SELECT lower(email_up) FROM {table}")), MASKED);
        let sample = e.ok("sample_rows", with(c, json!({"table": table})));
        assert!(!sample.to_string().contains("pw") && !sample.to_string().contains("ZED-SECRET"), "{c}: {sample}");
        for (tool, args) in [("describe_table", json!({"table": table, "include_ddl": true})), ("get_ddl", json!({"object": table}))] {
            let d = e.ok(tool, with(c, args)).to_string();
            assert!(!d.contains("pw_len") && !d.contains("pw_v") && !d.contains("(pw") && !d.contains("`pw`") && !d.contains("[pw]"), "{c} {tool}: {d}");
            assert!(d.contains("email_up"), "{c} {tool}: {d}");
        }
    };
    if up(PG) {
        let rules = "[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{ col = \"gen.email\", how = \"hash\" }]\n[connection.mcp_rules.visible]\nitems = [{ name = \"gen.pw\", kind = \"column\" }]\n";
        let e = env(&[conn("pgadmin", PG, "", "postgres", "dataman", "mcp = \"full\""), conn("pp", PG, "", "postgres", "dataman", rules)], "", None);
        e.admin(
            "execute",
            with("pgadmin", json!({"sql": format!(
                "CREATE SCHEMA {s}; CREATE TABLE {s}.gen (id int PRIMARY KEY, name text, email text, pw text, \
                 pw_len int GENERATED ALWAYS AS (length(pw)) STORED, pw_v text GENERATED ALWAYS AS (pw || '!') VIRTUAL, \
                 email_up text GENERATED ALWAYS AS (upper(email)) STORED, name_up text GENERATED ALWAYS AS (upper(name)) STORED); \
                 INSERT INTO {s}.gen (id, name, email, pw) VALUES (1, 'Ana', 'zed-secret@acme.com', 'hash-secret-1')"
            )})),
        );
        let table = format!("{s}.gen");
        check(&e, "pp", &table, &format!("SELECT * FROM {table}"));
        let call = dataman_mcp::Call::new(e.server.hub.clone(), "privacy-test".into(), "stdio", "resources/read", CancellationToken::new());
        let u = dataman_mcp::resources::parse(&dataman_mcp::resources::uri("pp", "dataman", false)).unwrap();
        let text = e.server.hub.core.block_on(dataman_mcp::resources::read(&call, &u)).unwrap();
        assert!(text.contains("email_up") && !text.contains("pw_len") && !text.contains("pw_v"), "{text}");
        e.admin("execute", with("pgadmin", json!({"sql": format!("DROP SCHEMA {s} CASCADE")})));
        e.nothing_kept_logged();
        e.done();
    }
    if up(MY) {
        let rules = "[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{ col = \"gen.email\", how = \"hash\" }]\n[connection.mcp_rules.visible]\nitems = [{ name = \"gen.pw\", kind = \"column\" }]\n";
        let e = env(&[conn("myadmin", MY, "mysql", "root", "mysql", "mcp = \"full\""), conn("mm", MY, "mysql", "root", &s, rules)], "", None);
        let a = |sql: String| e.admin("execute", with("myadmin", json!({"sql": sql})));
        a(format!("CREATE DATABASE {s}"));
        a(format!(
            "CREATE TABLE {s}.gen (id int PRIMARY KEY, name varchar(50), email varchar(50), pw varchar(50), \
             pw_len int AS (length(`pw`)) VIRTUAL, email_up varchar(50) AS (upper(email)) STORED, name_up varchar(50) AS (upper(name)) VIRTUAL)"
        ));
        a(format!("INSERT INTO {s}.gen (id, name, email, pw) VALUES (1, 'Ana', 'zed-secret@acme.com', 'hash-secret-1')"));
        check(&e, "mm", "gen", "SELECT * FROM gen");
        a(format!("DROP DATABASE {s}"));
        e.nothing_kept_logged();
        e.done();
    }
    if up(MS) {
        let t = format!("{s}_gen");
        let rules = format!("[connection.mcp_rules]\nview = \"masked\"\nsensitive = [{{ col = \"{t}.email\", how = \"hash\" }}]\n[connection.mcp_rules.visible]\nitems = [{{ name = \"{t}.pw\", kind = \"column\" }}]\n");
        let e = env(&[conn("msadmin", MS, "sql-server", "sa", "tempdb", "mcp = \"full\""), conn("ss", MS, "sql-server", "sa", "tempdb", rules)], "", None);
        e.admin("execute", with("msadmin", json!({"sql": format!(
            "CREATE TABLE dbo.{t} (id int, name varchar(50), email varchar(50), pw varchar(50), pw_len AS (len(pw)), email_up AS (upper(email)) PERSISTED, name_up AS (upper(name))); \
             INSERT INTO dbo.{t} (id, name, email, pw) VALUES (1, 'Ana', 'zed-secret@acme.com', 'hash-secret-1')"
        )})));
        let table = format!("dbo.{t}");
        check(&e, "ss", &table, &format!("SELECT * FROM {table}"));
        e.admin("execute", with("msadmin", json!({"sql": format!("DROP TABLE dbo.{t}")})));
        e.nothing_kept_logged();
        e.done();
    }
}
