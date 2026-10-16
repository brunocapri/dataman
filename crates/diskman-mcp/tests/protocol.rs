//! The MCP protocol end to end: `diskman-mcp` spawned over stdio and driven with JSON-RPC
//! (initialize → tools/list → tools/call, resources), alone and handing its calls to an app
//! through `app.sock`. No database is needed: the calls here are answered or refused before any
//! connection opens.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

const CONNECTIONS: &str = r#"
[[connection]]
id = "pg1"
name = "local pg"
host = "127.0.0.1"
port = 55432
user = "postgres"
database = "diskman"
sslmode = "disable"

[[connection]]
id = "prod1"
name = "prod"
host = "10.255.255.1"
port = 5432
user = "app"
database = "app"
env = "production"

[[connection]]
id = "w1"
name = "writable"
host = "10.255.255.1"
port = 5432
user = "app"
database = "app"
mcp = "write"

[[connection]]
id = "f1"
name = "full"
host = "10.255.255.1"
port = 5432
user = "app"
database = "app"
mcp = "full"
"#;

fn config() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("diskman")).unwrap();
    std::fs::write(dir.path().join("diskman/connections.toml"), CONNECTIONS).unwrap();
    dir
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn spawn(cfg: &std::path::Path) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_diskman-mcp"))
            .env("XDG_CONFIG_HOME", cfg)
            .env("DISKMAN_MCP_SECRETS", "memory")
            .env("PGPASSFILE", cfg.join("none"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Mcp { child, stdin, stdout, next: 1 }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// A request and its response (notifications in between are skipped).
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "the server closed stdout");
            let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn tool(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.call("tools/call", json!({"name": name, "arguments": args}));
        let res = &r["result"];
        assert!(res.is_object(), "{r}");
        (res["isError"] == json!(true), res["content"][0]["text"].as_str().unwrap_or_default().to_string())
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_session() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    let init = m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "protocol-test", "version": "1"}}));
    let r = &init["result"];
    assert_eq!(r["serverInfo"]["name"], "diskman", "{init}");
    assert!(r["capabilities"]["tools"].is_object() && r["capabilities"]["resources"].is_object(), "{init}");
    assert!(r["instructions"].as_str().unwrap().contains("list_connections"));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    assert_eq!(m.call("ping", json!({}))["result"], json!({}));

    // tools/list: the built-ins, typed schemas and hints.
    let list = m.call("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.len() >= 15 && names.contains(&"run_query") && names.contains(&"execute"), "{names:?}");
    let rq = tools.iter().find(|t| t["name"] == "run_query").unwrap();
    assert_eq!(rq["inputSchema"]["required"], json!(["connection", "sql"]));
    assert_eq!(rq["annotations"]["readOnlyHint"], true);
    let ex = tools.iter().find(|t| t["name"] == "execute").unwrap();
    assert_eq!(ex["annotations"]["destructiveHint"], true);

    // list_connections: names, permissions, no secrets.
    let (err, text) = m.tool("list_connections", json!({}));
    assert!(!err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[1].as_array().unwrap()[0..3], [json!("prod"), json!("production"), json!("off")], "production defaults to off and shows nothing else");
    assert!(rows[1][4].is_null());
    assert_eq!((rows[2][2].as_str(), rows[2][11].as_u64()), (Some("write"), Some(100)), "the tier and its row limit");
    assert_eq!(rows[3][2], "full");
    assert!(!text.to_lowercase().contains("password"));

    // Argument validation, unknown tools, refusals before any connection.
    let (err, text) = m.tool("run_query", json!({"connection": "local pg"}));
    assert!(err && text.contains("\"sql\" is required"), "{text}");
    let (err, text) = m.tool("run_query", json!({"connection": "local pg", "sql": "select 1", "rows": 5}));
    assert!(err && text.contains("unknown argument"), "{text}");
    let (err, text) = m.tool("run_query", json!({"connection": "local pg", "sql": "DELETE FROM t"}));
    assert!(err && text.starts_with("refused") && text.contains("not a read"), "{text}");
    let (err, text) = m.tool("run_query", json!({"connection": "prod", "sql": "select 1"}));
    assert!(err && text.contains("off"), "{text}");
    let (err, text) = m.tool("execute", json!({"connection": "local pg", "sql": "INSERT INTO t VALUES (1)"}));
    assert!(err && text.contains("reads only"), "{text}");
    // The tiers, before any connection: write refuses what is destructive (the agent's
    // `confirm` changes nothing); full needs the app, which is not running here.
    for sql in ["DELETE FROM t", "DELETE FROM t WHERE 1=1", "DROP TABLE t", "TRUNCATE t", "UPDATE t SET a = 1 WHERE id = id"] {
        for confirm in [false, true] {
            let (err, text) = m.tool("execute", json!({"connection": "writable", "sql": sql, "confirm": confirm}));
            assert!(err && text.contains("destructive") && text.contains("full"), "{sql}: {text}");
        }
        let (err, text) = m.tool("execute", json!({"connection": "full", "sql": sql, "confirm": true}));
        assert!(err && text.contains("needs approval in Diskman; open the app"), "{sql}: {text}");
        let (err, text) = m.tool("open_query", json!({"connection": "full", "sql": sql}));
        assert!(err && text.contains("needs approval in Diskman"), "{sql}: {text}");
    }
    let (err, text) = m.tool("terminate_backend", json!({"connection": "full", "pid": 7}));
    assert!(err && text.contains("needs approval in Diskman"), "{text}");
    let (err, text) = m.tool("terminate_backend", json!({"connection": "writable", "pid": 7}));
    assert!(err && text.contains("destructive"), "{text}");
    let (err, text) = m.tool("execute", json!({"connection": "writable", "sql": "BEGIN; INSERT INTO t VALUES (1)"}));
    assert!(err && text.contains("transaction control"), "{text}");
    let (err, text) = m.tool("run_query", json!({"connection": "nope", "sql": "select 1"}));
    assert!(err && text.contains("no connection named"), "{text}");
    let unknown = m.call("tools/call", json!({"name": "drop_everything", "arguments": {}}));
    assert!(unknown["error"]["message"].as_str().unwrap().contains("unknown tool"), "{unknown}");
    // Drafts (open_query run: false) need the app: headless they are refused in every tier,
    // never run instead; off stays off.
    let oq = tools.iter().find(|t| t["name"] == "open_query").unwrap();
    assert_eq!(oq["inputSchema"]["properties"]["run"]["type"], "boolean");
    assert!(oq["description"].as_str().unwrap().contains("run: false") && r["instructions"].as_str().unwrap().contains("run: false"));
    for conn in ["local pg", "writable", "full"] {
        let (err, text) = m.tool("open_query", json!({"connection": conn, "sql": "DELETE FROM orders", "run": false}));
        assert!(err && text.contains("drafts need Diskman open"), "{conn}: {text}");
    }
    let (err, text) = m.tool("open_query", json!({"connection": "prod", "sql": "SELECT 1", "run": false}));
    assert!(err && text.contains("off") && !text.contains("drafts"), "{text}");

    // Resources: listed for usable connections only.
    let res = m.call("resources/list", json!({}));
    let uris: Vec<&str> = res["result"]["resources"].as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
    assert_eq!(
        uris,
        [
            "diskman://local%20pg/diskman/schema",
            "diskman://local%20pg/diskman/schema.json",
            "diskman://writable/app/schema",
            "diskman://writable/app/schema.json",
            "diskman://full/app/schema",
            "diskman://full/app/schema.json"
        ]
    );
    let tpl = m.call("resources/templates/list", json!({}));
    assert_eq!(tpl["result"]["resourceTemplates"].as_array().unwrap().len(), 2, "{tpl}");

    // Every call is in the audit log, with the client's name.
    let log = std::fs::read_to_string(cfg.path().join("diskman/mcp-audit.jsonl")).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.len() >= 9, "{log}");
    assert!(lines.iter().all(|l| l["client"] == "protocol-test" && l["transport"] == "stdio"));
    let refused = lines.iter().find(|l| l["sql"] == "DELETE FROM t" && l["tool"] == "run_query").unwrap();
    assert_eq!(refused["outcome"], "refused");
    assert!(refused.get("database").is_none(), "refused before connecting: {refused}");
    let unavailable = lines.iter().find(|l| l["connection"] == "full" && l["tool"] == "execute").unwrap();
    assert_eq!((unavailable["outcome"].as_str(), unavailable["approval"]["decision"].as_str()), (Some("refused"), Some("unavailable")), "{unavailable}");
    assert!(unavailable["approval"].get("by").is_none());
    let draft = lines.iter().find(|l| l["connection"] == "full" && l["tool"] == "open_query" && l["query"] == "DELETE FROM orders").unwrap();
    assert!(draft["outcome"] == "refused" && draft.get("draft").is_none() && draft.get("rows").is_none(), "{draft}");
}

/// Favorites (shared with the app's ⌘Y panel) and create_connection: no database needed.
#[test]
fn stdio_favorites_and_create_connection() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err, "{text}");
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["rows"], json!([]));

    let (err, text) = m.tool("add_favorite", json!({"name": "top", "sql": "select 1", "connection": "local pg"}));
    assert!(!err, "{text}");
    let id1 = serde_json::from_str::<Value>(&text).unwrap()["id"].as_str().unwrap().to_string();
    // Same statement (just padded): no duplicate, the same id comes back.
    let (err, text) = m.tool("add_favorite", json!({"name": "dup", "sql": "  select 1  "}));
    assert!(!err, "{text}");
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["id"].as_str().unwrap(), id1);

    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err, "{text}");
    let rows = serde_json::from_str::<Value>(&text).unwrap()["rows"].as_array().unwrap().clone();
    assert_eq!(rows, vec![json!([id1.clone(), "top", "local pg", Value::Null, "select 1"])]);

    let (err, text) = m.tool("add_favorite", json!({"name": "x", "sql": "   "}));
    assert!(err && text.contains("must not be empty"), "{text}");

    let (err, text) = m.tool("rename_favorite", json!({"id": id1, "name": "renamed"}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err, "{text}");
    let rows = serde_json::from_str::<Value>(&text).unwrap()["rows"].as_array().unwrap().clone();
    assert_eq!(rows, vec![json!([id1.clone(), "renamed", "local pg", Value::Null, "select 1"])], "the statement and connection stay");
    let (err, text) = m.tool("rename_favorite", json!({"id": "nope", "name": "x"}));
    assert!(err && text.contains("no favorite"), "{text}");
    let (err, text) = m.tool("rename_favorite", json!({"id": id1, "name": "  "}));
    assert!(err && text.contains("must not be empty"), "{text}");

    let (err, _) = m.tool("remove_favorite", json!({"id": id1}));
    assert!(!err);
    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["rows"], json!([]));
    // A missing id is not an error.
    let (err, _) = m.tool("remove_favorite", json!({"id": "nope"}));
    assert!(!err);

    // import_favorites: name from the file, deduped by content, non-.sql refused.
    let files = tempfile::tempdir().unwrap();
    let sql_path = files.path().join("top customers.sql");
    std::fs::write(&sql_path, "select * from customers").unwrap();
    let txt_path = files.path().join("notes.txt");
    std::fs::write(&txt_path, "select * from customers").unwrap();
    let (err, text) = m.tool("import_favorites", json!({"paths": [sql_path.display().to_string(), txt_path.display().to_string()]}));
    assert!(err && text.contains("not a .sql file"), "{text}");
    let (err, text) = m.tool("import_favorites", json!({"paths": [sql_path.display().to_string()]}));
    assert!(!err, "{text}");
    let (err, _) = m.tool("import_favorites", json!({"paths": [sql_path.display().to_string()]}));
    assert!(!err);
    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err);
    let rows = serde_json::from_str::<Value>(&text).unwrap()["rows"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "imported twice, same content: no duplicate");
    assert_eq!(rows[0][1], json!("top customers"));

    // create_connection: fields, then refused on the same name, then overwrite; a connection
    // string; an unknown engine; the password never shows up anywhere, including the file.
    let (err, text) = m.tool("create_connection", json!({"name": "new1", "host": "db.internal", "user": "bob", "password": "s3cret"}));
    assert!(!err, "{text}");
    assert!(!text.to_lowercase().contains("s3cret"), "{text}");
    let toml = std::fs::read_to_string(cfg.path().join("diskman/connections.toml")).unwrap();
    assert!(!toml.to_lowercase().contains("s3cret"), "{toml}");
    assert!(!toml.lines().any(|l| l.trim_start().starts_with("password")), "{toml}");

    let (err, text) = m.tool("create_connection", json!({"name": "new1", "host": "x", "user": "y"}));
    assert!(err && text.contains("already exists"), "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "new1", "host": "db2.internal", "user": "bob", "overwrite": true}));
    assert!(!err, "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "from url", "connection_string": "postgresql://alice@dbhost/app"}));
    assert!(!err, "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "bad", "host": "x", "user": "y", "engine": "oracle"}));
    assert!(err && text.contains("must be one of"), "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "no host"}));
    assert!(err && text.contains("\"host\" and \"user\" are required"), "{text}");

    let (err, text) = m.tool("list_connections", json!({}));
    assert!(!err, "{text}");
    let names: Vec<String> =
        serde_json::from_str::<Value>(&text).unwrap()["rows"].as_array().unwrap().iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    assert!(names.contains(&"new1".to_string()) && names.contains(&"from url".to_string()), "{names:?}");
}

/// The `[[connection]]` block naming `name`, for assertions on one field (`save_password`)
/// `list_connections` does not surface.
fn block_for<'a>(toml: &'a str, name: &str) -> &'a str {
    toml.split("[[connection]]").find(|b| b.contains(&format!("name = \"{name}\""))).unwrap_or_else(|| panic!("{name} not in {toml}"))
}

/// create_connection cannot be used to escalate: the most it grants is "read" (never "write"),
/// and overwriting an existing connection may only hold `mcp`, `env` and `read_only` steady or
/// tighten them, never loosen.
#[test]
fn stdio_create_connection_refuses_escalation() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    // A brand-new connection cannot be granted "write".
    let (err, text) = m.tool("create_connection", json!({"name": "x1", "host": "h", "user": "u", "mcp": "write"}));
    assert!(err && text.contains("cannot grant") && text.contains("write"), "{text}");

    // "prod" (env production, so mcp defaults to off): widening mcp to "read" is refused.
    let (err, text) = m.tool("create_connection", json!({"name": "prod", "host": "10.255.255.1", "user": "app", "overwrite": true, "mcp": "read"}));
    assert!(err && text.contains("widen"), "{text}");

    // Loosening env from production to dev is refused (it would also loosen the mcp default).
    let (err, text) = m.tool("create_connection", json!({"name": "prod", "host": "10.255.255.1", "user": "app", "overwrite": true, "env": "dev"}));
    assert!(err && text.contains("loosen"), "{text}");

    // Tightening (none → production) is fine; so is leaving env/mcp/read_only untouched while
    // changing something else.
    let (err, text) = m.tool("create_connection", json!({"name": "local pg", "host": "127.0.0.1", "user": "postgres", "overwrite": true, "env": "production"}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("create_connection", json!({"name": "local pg", "host": "127.0.0.1", "user": "postgres", "overwrite": true, "color": "violet"}));
    assert!(!err, "{text}");

    // read_only may turn on, but once on, an overwrite cannot turn it back off.
    let (err, text) = m.tool("create_connection", json!({"name": "local pg", "host": "127.0.0.1", "user": "postgres", "overwrite": true, "read_only": true}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("create_connection", json!({"name": "local pg", "host": "127.0.0.1", "user": "postgres", "overwrite": true, "read_only": false}));
    assert!(err && text.contains("turn off"), "{text}");

    // "writable" (mcp write, set by a human in the app) keeps it when the agent does not touch
    // mcp at all: inherited, not granted by this call.
    let (err, text) = m.tool("create_connection", json!({"name": "writable", "host": "10.255.255.1", "user": "app", "overwrite": true, "color": "amber"}));
    assert!(!err, "{text}");
    let toml = std::fs::read_to_string(cfg.path().join("diskman/connections.toml")).unwrap();
    assert!(block_for(&toml, "writable").contains("mcp = \"write\""), "{toml}");

    // Narrowing "writable" from write to read is a tightening: allowed.
    let (err, text) = m.tool("create_connection", json!({"name": "writable", "host": "10.255.255.1", "user": "app", "overwrite": true, "mcp": "read"}));
    assert!(!err, "{text}");
    let toml = std::fs::read_to_string(cfg.path().join("diskman/connections.toml")).unwrap();
    assert!(block_for(&toml, "writable").contains("mcp = \"read\""), "{toml}");

    // But it cannot be put back to write afterwards.
    let (err, text) = m.tool("create_connection", json!({"name": "writable", "host": "10.255.255.1", "user": "app", "overwrite": true, "mcp": "write"}));
    assert!(err && text.contains("cannot grant"), "{text}");
}

/// Overwriting a connection keeps its saved password only when the server it points at did not
/// change (engine, host, port, user); otherwise the Keychain secret is dropped, so a changed
/// host (or user) never receives a password it was not given.
#[test]
fn stdio_create_connection_drops_the_secret_when_the_target_changes() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let toml_of = |cfg: &std::path::Path| std::fs::read_to_string(cfg.join("diskman/connections.toml")).unwrap();

    let (err, _) = m.tool("create_connection", json!({"name": "secret1", "host": "h1", "port": 5432, "user": "u1", "password": "p1"}));
    assert!(!err);
    assert!(block_for(&toml_of(cfg.path()), "secret1").contains("save_password = true"));

    // Same target (engine, host, port, user unchanged), no new password: kept.
    let (err, _) = m.tool("create_connection", json!({"name": "secret1", "host": "h1", "port": 5432, "user": "u1", "database": "newdb", "overwrite": true}));
    assert!(!err);
    assert!(block_for(&toml_of(cfg.path()), "secret1").contains("save_password = true"));

    // The host changes, no new password: the secret is dropped, not carried to the new host.
    let (err, _) = m.tool("create_connection", json!({"name": "secret1", "host": "h2", "port": 5432, "user": "u1", "overwrite": true}));
    assert!(!err);
    assert!(block_for(&toml_of(cfg.path()), "secret1").contains("save_password = false"));

    // A second connection: only the user changes this time.
    let (err, _) = m.tool("create_connection", json!({"name": "secret2", "host": "h3", "port": 5432, "user": "u1", "password": "p2"}));
    assert!(!err);
    let (err, _) = m.tool("create_connection", json!({"name": "secret2", "host": "h3", "port": 5432, "user": "u2", "overwrite": true}));
    assert!(!err);
    assert!(block_for(&toml_of(cfg.path()), "secret2").contains("save_password = false"));

    // Giving a new password always sets it, target changed or not.
    let (err, _) = m.tool("create_connection", json!({"name": "secret2", "host": "h4", "port": 5432, "user": "u2", "password": "p3", "overwrite": true}));
    assert!(!err);
    assert!(block_for(&toml_of(cfg.path()), "secret2").contains("save_password = true"));
}

/// update_connection, headless (the app is not running): only the given fields change, the
/// password is write-only, nothing loosens, a move to another server drops the saved password,
/// an off connection is not the agent's to edit, names stay unique.
#[test]
fn stdio_update_connection() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let toml_of = |cfg: &std::path::Path| std::fs::read_to_string(cfg.join("diskman/connections.toml")).unwrap();
    let list = m.call("tools/list", json!({}));
    let uc = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "update_connection").cloned().unwrap();
    assert_eq!(uc["inputSchema"]["required"], json!(["connection"]));
    assert_eq!(uc["annotations"]["readOnlyHint"], false, "{uc}");

    // A password and a database: saved, the password never echoed nor in the file.
    let (err, text) = m.tool("update_connection", json!({"connection": "local pg", "password": "hunter2", "database": "other"}));
    assert!(!err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!((v["id"].as_str(), &v["changed"], v["password"].as_str()), (Some("pg1"), &json!(["database"]), Some("set")), "{text}");
    assert!(!text.contains("hunter2") && !toml_of(cfg.path()).contains("hunter2"));
    let block = block_for(&toml_of(cfg.path()), "local pg").to_string();
    assert!(block.contains("database = \"other\"") && block.contains("save_password = true") && block.contains("port = 55432"), "{block}");
    // By id, renamed and recolored; the password stays (same server).
    let (err, text) = m.tool("update_connection", json!({"connection": "pg1", "name": "local", "color": "violet"}));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(!err && v["changed"] == json!(["name", "color"]) && v["password"] == "kept", "{text}");
    // Nothing given: nothing changes.
    let (err, text) = m.tool("update_connection", json!({"connection": "local"}));
    assert!(!err && serde_json::from_str::<Value>(&text).unwrap()["changed"] == json!([]), "{text}");
    // Another host: the saved password is dropped, not carried there.
    let (err, text) = m.tool("update_connection", json!({"connection": "local", "host": "10.0.0.9"}));
    assert!(!err && serde_json::from_str::<Value>(&text).unwrap()["password"] == "dropped", "{text}");
    assert!(block_for(&toml_of(cfg.path()), "local").contains("save_password = false"));
    // Nothing loosens.
    let (err, text) = m.tool("update_connection", json!({"connection": "writable", "mcp": "full"}));
    assert!(err && text.contains("widen"), "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "writable", "env": "production"}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "writable", "env": "dev"}));
    assert!(err && text.contains("loosen"), "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "full", "read_only": true}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "full", "read_only": false}));
    assert!(err && text.contains("turn off"), "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "full", "mcp": "write"}));
    assert!(!err && text.contains("\"mcp\""), "narrowing is fine: {text}");
    // Off (prod) is edited in the app only; names stay unique; unknown names are invalid.
    let (err, text) = m.tool("update_connection", json!({"connection": "prod", "host": "x"}));
    assert!(err && text.contains("off"), "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "full", "name": "WRITABLE"}));
    assert!(err && text.contains("another connection"), "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "nope", "host": "x"}));
    assert!(err && text.contains("no connection named"), "{text}");
    // list_connections still never says anything about passwords.
    let (_, text) = m.tool("list_connections", json!({}));
    assert!(!text.to_lowercase().contains("password") && !text.contains("hunter2"), "{text}");
    let log = std::fs::read_to_string(cfg.path().join("diskman/mcp-audit.jsonl")).unwrap();
    assert!(!log.contains("hunter2") && log.contains("\"tool\":\"update_connection\""), "{log}");
}

/// The user tools file is read again when it changes, and the new tools are listed.
#[test]
fn stdio_user_tools_reload() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let names = |m: &mut Mcp| -> Vec<String> {
        m.call("tools/list", json!({}))["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    assert!(!names(&mut m).contains(&"active_users".to_string()));
    let file = cfg.path().join("diskman/mcp-tools.toml");
    std::fs::write(
        &file,
        "[[tool]]\nname = \"active_users\"\ndescription = \"Users seen in the last n days.\"\nconnection = \"local pg\"\nsql = \"SELECT $1::int AS days\"\n[[tool.param]]\nname = \"days\"\ntype = \"int\"\nmin = 1\nmax = 90\n",
    )
    .unwrap();
    assert!(names(&mut m).contains(&"active_users".to_string()));
    let (err, text) = m.tool("active_users", json!({"days": 500}));
    assert!(err && text.contains("at most 90"), "{text}");
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&file, "").unwrap();
    assert!(!names(&mut m).contains(&"active_users".to_string()));
}

/// An app that takes drafts (and would deny any question, which a draft must never ask).
#[derive(Default)]
struct Drafter {
    drafts: std::sync::Mutex<Vec<diskman_mcp::TabDraft>>,
    asked: std::sync::Mutex<usize>,
    reads: std::sync::Mutex<usize>,
}

impl diskman_mcp::Front for Drafter {
    fn approve(&self, _: diskman_mcp::Approval) -> futures_util::future::BoxFuture<'static, bool> {
        *self.asked.lock().unwrap() += 1;
        Box::pin(async { false })
    }
    fn tabs(&self) -> bool {
        true
    }
    fn run_read(&self, _: diskman_mcp::TabRead) -> futures_util::future::BoxFuture<'static, Result<diskman_core::bounded::Outcome, diskman_core::bounded::Stop>> {
        *self.reads.lock().unwrap() += 1;
        Box::pin(async { Err(diskman_core::bounded::Stop::Refused("no".into())) })
    }
    fn draft(&self, d: diskman_mcp::TabDraft) -> futures_util::future::BoxFuture<'static, Result<diskman_mcp::Drafted, String>> {
        let database = d.database.clone();
        self.drafts.lock().unwrap().push(d);
        Box::pin(async move { Ok(diskman_mcp::Drafted { tab: "orders".into(), database }) })
    }
}

/// Drafts through the app: any tier but off, destructive statements included, read-only and
/// production too; never classified, asked about, connected or run. Off and the switch off
/// refuse them; without the app (no front) they are refused, never run.
#[test]
fn drafts_go_to_the_app_and_never_run() {
    let cfg = tempfile::tempdir().unwrap();
    let dir = cfg.path().join("diskman");
    std::fs::create_dir_all(&dir).unwrap();
    // Unreachable hosts: anything that connected would fail.
    let conns = [("r", "read", "", false), ("w", "write", "", false), ("f", "full", "", false), ("ro", "full", "", true), ("p", "write", "production", false), ("o", "off", "", false)];
    let mut toml = String::new();
    for (name, tier, env, ro) in conns {
        toml.push_str(&format!("[[connection]]\nid = \"{name}\"\nname = \"{name}\"\nhost = \"10.255.255.1\"\nport = 5432\nuser = \"app\"\ndatabase = \"app\"\nmcp = \"{tier}\"\nread_only = {ro}\n"));
        if !env.is_empty() {
            toml.push_str(&format!("env = \"{env}\"\n"));
        }
    }
    std::fs::write(dir.join("connections.toml"), toml).unwrap();
    let hub = diskman_mcp::Hub::new(diskman_core::Dirs::at(&dir), Arc::new(diskman_core::MemorySecrets::default())).unwrap();
    let app = Arc::new(Drafter::default());
    hub.set_front(Some(app.clone()));
    let reg = diskman_mcp::registry(&hub.dirs);
    let server = diskman_mcp::Server::new(hub.clone(), reg.clone(), "stdio");
    let rt = hub.handle();
    let call = |args: Value| -> (bool, String) {
        let tool = reg.find("open_query").unwrap();
        let r = rt.block_on(server.run_tool(tool, args.as_object().unwrap().clone(), "draft-test".into(), Default::default()));
        let v = serde_json::to_value(r).unwrap();
        (v["isError"] == json!(true), v["content"][0]["text"].as_str().unwrap_or_default().to_string())
    };
    for conn in ["r", "w", "f", "ro", "p"] {
        for sql in ["DELETE FROM orders", "DROP TABLE orders", "TRUNCATE orders; UPDATE x SET a = 1", "SELECT 1"] {
            let (err, text) = call(json!({"connection": conn, "database": "shop", "sql": sql, "run": false, "title": "t"}));
            assert!(!err, "{conn} {sql}: {text}");
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), json!({"drafted": true, "tab": "orders", "connection": conn, "database": "shop"}));
        }
    }
    {
        let d = app.drafts.lock().unwrap();
        assert_eq!(d.len(), 20);
        assert_eq!((d[0].client.as_str(), d[0].connection_id.as_str(), d[0].database.as_str(), d[0].sql.as_str(), d[0].title.as_deref()), ("draft-test", "r", "shop", "DELETE FROM orders", Some("t")));
    }
    assert_eq!((*app.asked.lock().unwrap(), *app.reads.lock().unwrap()), (0, 0), "nothing asked, nothing run");
    assert!(hub.connected("f").is_none(), "nothing connected");
    // The database defaults to the connection's.
    let (_, text) = call(json!({"connection": "w", "sql": "DELETE FROM orders", "run": false}));
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["database"], "app");
    // Off, and params (the user runs the text as it is).
    let (err, text) = call(json!({"connection": "o", "sql": "SELECT 1", "run": false}));
    assert!(err && text.contains("off"), "{text}");
    let (err, text) = call(json!({"connection": "f", "sql": "SELECT $1", "params": [1], "run": false}));
    assert!(err && text.contains("params"), "{text}");
    // run: true (the default) on tier read still refuses the write.
    let (err, text) = call(json!({"connection": "r", "sql": "DELETE FROM orders", "run": true}));
    assert!(err && text.contains("reads only"), "{text}");
    // The switch off.
    std::fs::write(dir.join("settings.toml"), "mcp_enabled = false\n").unwrap();
    let (err, text) = call(json!({"connection": "f", "sql": "SELECT 1", "run": false}));
    assert!(err && text.contains("turned off"), "{text}");
    std::fs::remove_file(dir.join("settings.toml")).unwrap();
    // No app: refused, nothing run.
    hub.set_front(None);
    let (err, text) = call(json!({"connection": "f", "sql": "DELETE FROM orders", "run": false}));
    assert!(err && text.contains("drafts need Diskman open"), "{text}");
    assert_eq!(app.drafts.lock().unwrap().len(), 21);
    // The audit marks the drafts, with the statement redacted and as sent.
    let log = std::fs::read_to_string(dir.join("mcp-audit.jsonl")).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let d = lines.iter().find(|l| l["draft"] == true && l["connection"] == "r").unwrap();
    assert_eq!((d["outcome"].as_str(), d["sql"].as_str(), d["query"].as_str(), d["database"].as_str()), (Some("ok"), Some("DELETE FROM orders"), Some("DELETE FROM orders"), Some("shop")), "{d}");
    assert!(d.get("rows").is_none());
    assert_eq!(lines.iter().filter(|l| l["draft"] == true).count(), 21);
    assert!(lines.iter().filter(|l| l["outcome"] != "ok").all(|l| l.get("draft").is_none()));
    std::mem::forget(hub);
}

/// The app's side for the forwarding test: denies everything, counts the questions.
#[derive(Default)]
struct Denier(std::sync::Mutex<Vec<diskman_mcp::Approval>>);

impl diskman_mcp::Front for Denier {
    fn approve(&self, a: diskman_mcp::Approval) -> futures_util::future::BoxFuture<'static, bool> {
        self.0.lock().unwrap().push(a);
        Box::pin(async { false })
    }
}

/// The stdio binary hands its calls to a running app through `app.sock` (here a socket this
/// test serves with its own hub and a human that denies): a destructive write on a full
/// connection is asked about there and denied; with the app gone, the binary refuses it
/// itself, never runs it.
#[test]
fn stdio_hands_calls_to_the_app() {
    if std::net::TcpStream::connect_timeout(&"127.0.0.1:55432".parse().unwrap(), Duration::from_secs(1)).is_err() {
        eprintln!("skipping: no PostgreSQL at 127.0.0.1:55432");
        return;
    }
    let cfg = tempfile::tempdir().unwrap();
    let dir = cfg.path().join("diskman");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("connections.toml"),
        "[[connection]]\nid = \"pf\"\nname = \"pg full\"\nhost = \"127.0.0.1\"\nport = 55432\nuser = \"postgres\"\ndatabase = \"diskman\"\nsslmode = \"disable\"\nmcp = \"full\"\n",
    )
    .unwrap();
    let pgpass = cfg.path().join("pgpass");
    std::fs::write(&pgpass, "127.0.0.1:55432:*:postgres:diskman\n").unwrap();
    std::fs::set_permissions(&pgpass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
    let hub = diskman_mcp::Hub::new(diskman_core::Dirs::at(&dir), Arc::new(diskman_core::MemorySecrets::default())).unwrap();
    let hub = {
        let mut h = Arc::try_unwrap(hub).ok().unwrap();
        h.core.pgpass = Some(pgpass);
        Arc::new(h)
    };
    let human = Arc::new(Denier::default());
    hub.set_front(Some(human.clone()));
    let reg = diskman_mcp::registry(&hub.dirs);
    let (h, r) = (hub.clone(), reg.clone());
    let app = diskman_mcp::ipc::serve(&hub.dirs, Arc::new(move || Ok((h.clone(), r.clone())))).unwrap();

    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "forward-test", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let drop_sql = "DROP TABLE IF EXISTS mcp_ipc_never_there";
    let (err, text) = m.tool("execute", json!({"connection": "pg full", "sql": drop_sql, "confirm": true}));
    assert!(err && text.contains("denied in Diskman"), "{text}");
    {
        let asked = human.0.lock().unwrap();
        assert_eq!(asked.len(), 1, "asked in the app");
        assert_eq!((asked[0].client.as_str(), asked[0].connection.as_str(), asked[0].reasons.clone()), ("forward-test", "pg full", vec!["DROP TABLE".to_string()]));
    }
    let (err, text) = m.tool("run_query", json!({"connection": "pg full", "sql": "SELECT 1 AS one"}));
    assert!(!err && text.contains("\"one\""), "{text}");

    // The app quits: the binary refuses what needs a human, and runs the rest itself.
    app.stop();
    let (err, text) = m.tool("execute", json!({"connection": "pg full", "sql": drop_sql}));
    assert!(err && text.contains("needs approval in Diskman; open the app"), "{text}");
    assert_eq!(human.0.lock().unwrap().len(), 1, "nobody asked");
    let (err, text) = m.tool("run_query", json!({"connection": "pg full", "sql": "SELECT 2 AS two"}));
    assert!(err && text.contains("password") || !err && text.contains("\"two\""), "{text}");
    let log = std::fs::read_to_string(dir.join("mcp-audit.jsonl")).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let denied = lines.iter().find(|l| l["approval"]["decision"] == "denied").unwrap();
    assert_eq!((denied["client"].as_str(), denied["transport"].as_str()), (Some("forward-test"), Some("stdio")));
    assert!(lines.iter().any(|l| l["approval"]["decision"] == "unavailable"));
    std::mem::forget(hub);
}
