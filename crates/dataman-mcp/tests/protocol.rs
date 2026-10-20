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
database = "dataman"
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
    std::fs::create_dir_all(dir.path().join("dataman")).unwrap();
    std::fs::write(dir.path().join("dataman/connections.toml"), CONNECTIONS).unwrap();
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
        Mcp::spawn_with(cfg, &cfg.join("none"))
    }

    fn spawn_with(cfg: &std::path::Path, pgpass: &std::path::Path) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dataman-mcp"))
            .env("XDG_CONFIG_HOME", cfg)
            .env("DATAMAN_MCP_SECRETS", "memory")
            .env("PGPASSFILE", pgpass)
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
    assert_eq!(r["serverInfo"]["name"], "dataman", "{init}");
    assert!(r["capabilities"]["tools"].is_object() && r["capabilities"]["resources"].is_object(), "{init}");
    assert!(r["instructions"].as_str().unwrap().contains("list_connections"));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    assert_eq!(m.call("ping", json!({}))["result"], json!({}));

    let list = m.call("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.len() >= 15 && names.contains(&"run_query") && names.contains(&"execute"), "{names:?}");
    let rq = tools.iter().find(|t| t["name"] == "run_query").unwrap();
    assert_eq!(rq["inputSchema"]["required"], json!(["connection", "sql"]));
    assert_eq!(rq["annotations"]["readOnlyHint"], true);
    let ex = tools.iter().find(|t| t["name"] == "execute").unwrap();
    assert_eq!(ex["annotations"]["destructiveHint"], true);

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
    for sql in ["DELETE FROM t", "DELETE FROM t WHERE 1=1", "DROP TABLE t", "TRUNCATE t", "UPDATE t SET a = 1 WHERE id = id"] {
        for confirm in [false, true] {
            let (err, text) = m.tool("execute", json!({"connection": "writable", "sql": sql, "confirm": confirm}));
            assert!(err && text.contains("destructive") && text.contains("full"), "{sql}: {text}");
        }
        let (err, text) = m.tool("execute", json!({"connection": "full", "sql": sql, "confirm": true}));
        assert!(err && text.contains("needs approval in Dataman; open the app"), "{sql}: {text}");
        let (err, text) = m.tool("open_query", json!({"connection": "full", "sql": sql}));
        assert!(err && text.contains("needs approval in Dataman"), "{sql}: {text}");
    }
    let (err, text) = m.tool("terminate_backend", json!({"connection": "full", "pid": 7}));
    assert!(err && text.contains("needs approval in Dataman"), "{text}");
    let (err, text) = m.tool("terminate_backend", json!({"connection": "writable", "pid": 7}));
    assert!(err && text.contains("destructive"), "{text}");
    let (err, text) = m.tool("execute", json!({"connection": "writable", "sql": "BEGIN; INSERT INTO t VALUES (1)"}));
    assert!(err && text.contains("transaction control"), "{text}");
    let (err, text) = m.tool("run_query", json!({"connection": "nope", "sql": "select 1"}));
    assert!(err && text.contains("no connection named"), "{text}");
    let unknown = m.call("tools/call", json!({"name": "drop_everything", "arguments": {}}));
    assert!(unknown["error"]["message"].as_str().unwrap().contains("unknown tool"), "{unknown}");
    let oq = tools.iter().find(|t| t["name"] == "open_query").unwrap();
    assert_eq!(oq["inputSchema"]["properties"]["run"]["type"], "boolean");
    assert!(oq["description"].as_str().unwrap().contains("run: false") && r["instructions"].as_str().unwrap().contains("run: false"));
    for conn in ["local pg", "writable", "full"] {
        let (err, text) = m.tool("open_query", json!({"connection": conn, "sql": "DELETE FROM orders", "run": false}));
        assert!(err && text.contains("drafts need Dataman open"), "{conn}: {text}");
    }
    let (err, text) = m.tool("open_query", json!({"connection": "prod", "sql": "SELECT 1", "run": false}));
    assert!(err && text.contains("off") && !text.contains("drafts"), "{text}");

    let res = m.call("resources/list", json!({}));
    let uris: Vec<&str> = res["result"]["resources"].as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
    assert_eq!(
        uris,
        [
            "dataman://local%20pg/dataman/schema",
            "dataman://local%20pg/dataman/schema.json",
            "dataman://writable/app/schema",
            "dataman://writable/app/schema.json",
            "dataman://full/app/schema",
            "dataman://full/app/schema.json"
        ]
    );
    let tpl = m.call("resources/templates/list", json!({}));
    assert_eq!(tpl["result"]["resourceTemplates"].as_array().unwrap().len(), 2, "{tpl}");

    let log = std::fs::read_to_string(cfg.path().join("dataman/mcp-audit.jsonl")).unwrap();
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

/// A favorite saved on a connection agents cannot reach (production: off by default) is not
/// theirs to read, run, rename or remove.
#[test]
fn stdio_favorites_of_closed_connections() {
    let cfg = config();
    std::fs::write(
        cfg.path().join("dataman/favorites.toml"),
        "[[favorite]]\nid = \"open\"\nname = \"open one\"\nsql = \"select 1\"\nconnection_id = \"pg1\"\n\n[[favorite]]\nid = \"shut\"\nname = \"payroll\"\nsql = \"select salary from payroll where ssn = '078-05-1120'\"\nconnection_id = \"prod1\"\n",
    )
    .unwrap();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err && text.contains("open one") && !text.contains("payroll") && !text.contains("078-05"), "{text}");
    let (_, text) = m.tool("run_favorite", json!({"name": "payroll"}));
    assert!(!text.contains("078-05") && !text.contains("salary"), "{text}");
    let (err, text) = m.tool("rename_favorite", json!({"id": "shut", "name": "harmless"}));
    assert!(err && text.contains("no favorite"), "{text}");
    let (err, _) = m.tool("remove_favorite", json!({"id": "shut"}));
    assert!(!err);
    let (err, text) = m.tool("add_favorite", json!({"name": "x", "sql": "select 2", "connection": "prod"}));
    assert!(err && text.contains("off"), "{text}");
    let saved = std::fs::read_to_string(cfg.path().join("dataman/favorites.toml")).unwrap();
    assert!(saved.contains("name = \"payroll\"") && !saved.contains("select 2"), "{saved}");
}

/// A `*` line of ~/.pgpass is not sent to a server an agent named (it could ask for the
/// password in clear); the user's own connections keep libpq's behavior.
#[test]
fn stdio_pgpass_wildcard_stays_home() {
    use std::os::unix::fs::PermissionsExt;
    let cfg = config();
    let pgpass = cfg.path().join("pgpass");
    let pw = std::env::var("DATAMAN_PG_PASSWORD").unwrap_or_else(|_| "dataman".into());
    std::fs::write(&pgpass, format!("*:*:*:postgres:{pw}\n")).unwrap();
    std::fs::set_permissions(&pgpass, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut m = Mcp::spawn_with(cfg.path(), &pgpass);
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let (err, text) = m.tool("run_query", json!({"connection": "local pg", "sql": "SELECT 1"}));
    if err && text.contains("cannot connect") && !text.contains("password") {
        eprintln!("skipping: {text}");
        return;
    }
    assert!(!err, "the user's connection uses the * line: {text}");
    let (err, text) = m.tool("create_connection", json!({"name": "agent pg", "host": "127.0.0.1", "port": 55432, "user": "postgres", "database": "dataman", "sslmode": "disable"}));
    assert!(!err, "{text}");
    let toml = std::fs::read_to_string(cfg.path().join("dataman/connections.toml")).unwrap();
    assert!(block_for(&toml, "agent pg").contains("agent_made = true"), "{toml}");
    let (err, text) = m.tool("run_query", json!({"connection": "agent pg", "sql": "SELECT 1"}));
    assert!(err && text.contains("password"), "no password for the agent's own server: {text}");
}

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
    let (err, text) = m.tool("add_favorite", json!({"name": "dup", "sql": "  select 1  "}));
    assert!(!err, "{text}");
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["id"].as_str().unwrap(), id1);

    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err, "{text}");
    let rows = serde_json::from_str::<Value>(&text).unwrap()["rows"].as_array().unwrap().clone();
    assert_eq!(rows, vec![json!([id1.clone(), "top", "local pg", Value::Null, "select 1", Value::Null])]);

    let (err, text) = m.tool("add_favorite", json!({"name": "x", "sql": "   "}));
    assert!(err && text.contains("must not be empty"), "{text}");

    let (err, text) = m.tool("rename_favorite", json!({"id": id1, "name": "renamed"}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err, "{text}");
    let rows = serde_json::from_str::<Value>(&text).unwrap()["rows"].as_array().unwrap().clone();
    assert_eq!(rows, vec![json!([id1.clone(), "renamed", "local pg", Value::Null, "select 1", Value::Null])], "the statement and connection stay");
    let (err, text) = m.tool("rename_favorite", json!({"id": "nope", "name": "x"}));
    assert!(err && text.contains("no favorite"), "{text}");
    let (err, text) = m.tool("rename_favorite", json!({"id": id1, "name": "  "}));
    assert!(err && text.contains("must not be empty"), "{text}");

    let (err, _) = m.tool("remove_favorite", json!({"id": id1}));
    assert!(!err);
    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["rows"], json!([]));
    let (err, _) = m.tool("remove_favorite", json!({"id": "nope"}));
    assert!(!err);

    let files = tempfile::tempdir().unwrap();
    let sql_path = files.path().join("top customers.sql");
    std::fs::write(&sql_path, "select * from customers").unwrap();
    let txt_path = files.path().join("notes.txt");
    std::fs::write(&txt_path, "select * from customers").unwrap();
    let (err, text) = m.tool("import_favorites", json!({"paths": [sql_path.display().to_string(), txt_path.display().to_string()]}));
    assert!(err && text.contains("not a .sql file"), "{text}");
    // Reading files of this Mac waits for the user's Allow: without the app, nothing is read.
    let (err, text) = m.tool("import_favorites", json!({"paths": [sql_path.display().to_string()]}));
    assert!(err && text.contains("reads a file on this Mac") && text.contains("open the app"), "{text}");
    let (err, text) = m.tool("list_favorites", json!({}));
    assert!(!err && !text.contains("customers"), "{text}");
    let (err, text) = m.tool("import_favorites", json!({"paths": [files.path().join("folder.sql").display().to_string()]}));
    assert!(err && text.contains("No such file"), "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "new1", "host": "db.internal", "user": "bob", "password": "s3cret"}));
    assert!(!err, "{text}");
    assert!(!text.to_lowercase().contains("s3cret"), "{text}");
    let toml = std::fs::read_to_string(cfg.path().join("dataman/connections.toml")).unwrap();
    assert!(!toml.to_lowercase().contains("s3cret"), "{toml}");
    assert!(!toml.lines().any(|l| l.trim_start().starts_with("password")), "{toml}");

    let (err, text) = m.tool("create_connection", json!({"name": "new1", "host": "x", "user": "y"}));
    assert!(err && text.contains("already exists"), "{text}");

    // TLS not asked for: required for a remote host, opportunistic only on this machine.
    let (err, text) = m.tool("create_connection", json!({"name": "here", "host": "localhost", "user": "bob"}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("create_connection", json!({"name": "clear", "host": "db.internal", "user": "bob", "sslmode": "disable"}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("create_connection", json!({"name": "by string", "connection_string": "postgresql://alice@dbhost/app?sslmode=prefer"}));
    assert!(!err, "{text}");
    let toml = std::fs::read_to_string(cfg.path().join("dataman/connections.toml")).unwrap();
    assert!(block_for(&toml, "new1").contains("sslmode = \"require\""), "{toml}");
    assert!(block_for(&toml, "here").contains("sslmode = \"prefer\""), "{toml}");
    assert!(block_for(&toml, "clear").contains("sslmode = \"disable\""), "asked for: {toml}");
    assert!(block_for(&toml, "by string").contains("sslmode = \"prefer\""), "the string asked for it: {toml}");

    let (err, text) = m.tool("create_connection", json!({"name": "new1", "host": "db2.internal", "user": "bob", "overwrite": true}));
    assert!(err && text.contains("moves the connection to another server") && text.contains("open the app"), "headless, a move is refused: {text}");
    let (err, text) = m.tool("create_connection", json!({"name": "new1", "host": "db.internal", "user": "bob", "database": "other", "overwrite": true}));
    assert!(!err, "same server: {text}");

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

fn block_for<'a>(toml: &'a str, name: &str) -> &'a str {
    toml.split("[[connection]]").find(|b| b.contains(&format!("name = \"{name}\""))).unwrap_or_else(|| panic!("{name} not in {toml}"))
}

#[test]
fn stdio_create_connection_refuses_escalation() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let (err, text) = m.tool("create_connection", json!({"name": "x1", "host": "h", "user": "u", "mcp": "write"}));
    assert!(err && text.contains("cannot grant") && text.contains("write"), "{text}");

    // An MCP-off connection (prod) is edited in the app only, whatever the change.
    let (err, text) = m.tool("create_connection", json!({"name": "prod", "host": "10.255.255.1", "user": "app", "overwrite": true, "mcp": "read"}));
    assert!(err && text.contains("is off"), "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "prod", "host": "10.255.255.1", "user": "app", "overwrite": true, "env": "dev"}));
    assert!(err && text.contains("is off"), "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "full", "host": "10.255.255.1", "user": "app", "overwrite": true, "env": "none"}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("create_connection", json!({"name": "full", "host": "10.255.255.1", "user": "app", "overwrite": true, "mcp": "read", "env": "dev"}));
    assert!(!err, "narrowing, tightening: {text}");
    let (err, text) = m.tool("create_connection", json!({"name": "full", "host": "10.255.255.1", "user": "app", "overwrite": true, "env": "none"}));
    assert!(err && text.contains("loosen"), "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "local pg", "host": "127.0.0.1", "user": "postgres", "overwrite": true, "env": "staging"}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("create_connection", json!({"name": "local pg", "host": "127.0.0.1", "user": "postgres", "overwrite": true, "color": "violet"}));
    assert!(!err, "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "local pg", "host": "127.0.0.1", "user": "postgres", "overwrite": true, "read_only": true}));
    assert!(!err, "{text}");
    let (err, text) = m.tool("create_connection", json!({"name": "local pg", "host": "127.0.0.1", "user": "postgres", "overwrite": true, "read_only": false}));
    assert!(err && text.contains("turn off"), "{text}");

    let (err, text) = m.tool("create_connection", json!({"name": "writable", "host": "10.255.255.1", "user": "app", "overwrite": true, "color": "amber"}));
    assert!(!err, "{text}");
    let toml = std::fs::read_to_string(cfg.path().join("dataman/connections.toml")).unwrap();
    assert!(block_for(&toml, "writable").contains("mcp = \"write\""), "{toml}");

    let (err, text) = m.tool("create_connection", json!({"name": "writable", "host": "10.255.255.1", "user": "app", "overwrite": true, "mcp": "read"}));
    assert!(!err, "{text}");
    let toml = std::fs::read_to_string(cfg.path().join("dataman/connections.toml")).unwrap();
    assert!(block_for(&toml, "writable").contains("mcp = \"read\""), "{toml}");

    let (err, text) = m.tool("create_connection", json!({"name": "writable", "host": "10.255.255.1", "user": "app", "overwrite": true, "mcp": "write"}));
    assert!(err && text.contains("cannot grant"), "{text}");
}

#[test]
fn stdio_create_connection_drops_the_secret_when_the_target_changes() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let toml_of = |cfg: &std::path::Path| std::fs::read_to_string(cfg.join("dataman/connections.toml")).unwrap();

    let (err, _) = m.tool("create_connection", json!({"name": "secret1", "host": "h1", "port": 5432, "user": "u1", "password": "p1"}));
    assert!(!err);
    assert!(block_for(&toml_of(cfg.path()), "secret1").contains("save_password = true"));

    let (err, _) = m.tool("create_connection", json!({"name": "secret1", "host": "h1", "port": 5432, "user": "u1", "database": "newdb", "overwrite": true}));
    assert!(!err);
    assert!(block_for(&toml_of(cfg.path()), "secret1").contains("save_password = true"));

    // Moving it to another server waits for the user's Allow: without the app, nothing changes.
    for (host, user) in [("h2", "u1"), ("h1", "u2")] {
        let (err, text) = m.tool("create_connection", json!({"name": "secret1", "host": host, "port": 5432, "user": user, "overwrite": true}));
        assert!(err && text.contains("another server"), "{text}");
        let block = block_for(&toml_of(cfg.path()), "secret1").to_string();
        assert!(block.contains("host = \"h1\"") && block.contains("user = \"u1\"") && block.contains("save_password = true"), "{block}");
    }
    let (err, text) = m.tool("create_connection", json!({"name": "secret1", "host": "h1", "port": 5432, "user": "u1", "sslmode": "disable", "overwrite": true}));
    assert!(err && text.contains("sslmode would loosen"), "{text}");
    let (err, text) = m.tool("create_connection", json!({"name": "prod", "host": "10.255.255.1", "user": "app", "color": "violet", "overwrite": true}));
    assert!(err && text.contains("off"), "an MCP-off connection is not replaced: {text}");
}

#[test]
fn stdio_update_connection() {
    let cfg = config();
    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let toml_of = |cfg: &std::path::Path| std::fs::read_to_string(cfg.join("dataman/connections.toml")).unwrap();
    let list = m.call("tools/list", json!({}));
    let uc = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "update_connection").cloned().unwrap();
    assert_eq!(uc["inputSchema"]["required"], json!(["connection"]));
    assert_eq!(uc["annotations"]["readOnlyHint"], false, "{uc}");

    let (err, text) = m.tool("update_connection", json!({"connection": "local pg", "password": "hunter2", "database": "other"}));
    assert!(!err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!((v["id"].as_str(), &v["changed"], v["password"].as_str()), (Some("pg1"), &json!(["database"]), Some("set")), "{text}");
    assert!(!text.contains("hunter2") && !toml_of(cfg.path()).contains("hunter2"));
    let block = block_for(&toml_of(cfg.path()), "local pg").to_string();
    assert!(block.contains("database = \"other\"") && block.contains("save_password = true") && block.contains("port = 55432"), "{block}");
    let (err, text) = m.tool("update_connection", json!({"connection": "pg1", "name": "local", "color": "violet"}));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(!err && v["changed"] == json!(["name", "color"]) && v["password"] == "kept", "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "local"}));
    assert!(!err && serde_json::from_str::<Value>(&text).unwrap()["changed"] == json!([]), "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "local", "host": "10.0.0.9"}));
    assert!(err && text.contains("another server"), "{text}");
    let block = block_for(&toml_of(cfg.path()), "local").to_string();
    assert!(block.contains("host = \"127.0.0.1\"") && block.contains("save_password = true"), "{block}");
    let (err, text) = m.tool("update_connection", json!({"connection": "local", "sslmode": "require"}));
    assert!(!err, "tightening is fine: {text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "local", "sslmode": "disable"}));
    assert!(err && text.contains("sslmode would loosen"), "{text}");
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
    let (err, text) = m.tool("update_connection", json!({"connection": "prod", "host": "x"}));
    assert!(err && text.contains("off"), "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "full", "name": "WRITABLE"}));
    assert!(err && text.contains("another connection"), "{text}");
    let (err, text) = m.tool("update_connection", json!({"connection": "nope", "host": "x"}));
    assert!(err && text.contains("no connection named"), "{text}");
    let (_, text) = m.tool("list_connections", json!({}));
    assert!(!text.to_lowercase().contains("password") && !text.contains("hunter2"), "{text}");
    let log = std::fs::read_to_string(cfg.path().join("dataman/mcp-audit.jsonl")).unwrap();
    assert!(!log.contains("hunter2") && log.contains("\"tool\":\"update_connection\""), "{log}");
    // A password written into a statement stays out of the log too (refused or not).
    let (err, _) = m.tool("execute", json!({"connection": "local", "sql": "CREATE USER eve PASSWORD 'hunter3'"}));
    assert!(err);
    let log = std::fs::read_to_string(cfg.path().join("dataman/mcp-audit.jsonl")).unwrap();
    assert!(!log.contains("hunter3") && log.contains("CREATE USER eve PASSWORD '••••••'"), "{log}");
}

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
    let file = cfg.path().join("dataman/mcp-tools.toml");
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

#[derive(Default)]
struct Drafter {
    drafts: std::sync::Mutex<Vec<dataman_mcp::TabDraft>>,
    asked: std::sync::Mutex<usize>,
    reads: std::sync::Mutex<usize>,
}

impl dataman_mcp::Front for Drafter {
    fn approve(&self, _: dataman_mcp::Approval) -> futures_util::future::BoxFuture<'static, bool> {
        *self.asked.lock().unwrap() += 1;
        Box::pin(async { false })
    }
    fn tabs(&self) -> bool {
        true
    }
    fn run_read(&self, _: dataman_mcp::TabRead) -> futures_util::future::BoxFuture<'static, Result<dataman_core::bounded::Outcome, dataman_core::bounded::Stop>> {
        *self.reads.lock().unwrap() += 1;
        Box::pin(async { Err(dataman_core::bounded::Stop::Refused("no".into())) })
    }
    fn draft(&self, d: dataman_mcp::TabDraft) -> futures_util::future::BoxFuture<'static, Result<dataman_mcp::Drafted, String>> {
        let database = d.database.clone();
        self.drafts.lock().unwrap().push(d);
        Box::pin(async move { Ok(dataman_mcp::Drafted { tab: "orders".into(), database }) })
    }
}

#[test]
fn drafts_go_to_the_app_and_never_run() {
    let cfg = tempfile::tempdir().unwrap();
    let dir = cfg.path().join("dataman");
    std::fs::create_dir_all(&dir).unwrap();
    let conns = [("r", "read", "", false), ("w", "write", "", false), ("f", "full", "", false), ("ro", "full", "", true), ("p", "write", "production", false), ("o", "off", "", false)];
    let mut toml = String::new();
    for (name, tier, env, ro) in conns {
        toml.push_str(&format!("[[connection]]\nid = \"{name}\"\nname = \"{name}\"\nhost = \"10.255.255.1\"\nport = 5432\nuser = \"app\"\ndatabase = \"app\"\nmcp = \"{tier}\"\nread_only = {ro}\n"));
        if !env.is_empty() {
            toml.push_str(&format!("env = \"{env}\"\n"));
        }
    }
    std::fs::write(dir.join("connections.toml"), toml).unwrap();
    let hub = dataman_mcp::Hub::new(dataman_core::Dirs::at(&dir), Arc::new(dataman_core::MemorySecrets::default())).unwrap();
    let app = Arc::new(Drafter::default());
    hub.set_front(Some(app.clone()));
    let reg = dataman_mcp::registry(&hub.dirs);
    let server = dataman_mcp::Server::new(hub.clone(), reg.clone(), "stdio");
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
    let (_, text) = call(json!({"connection": "w", "sql": "DELETE FROM orders", "run": false}));
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["database"], "app");
    let (err, text) = call(json!({"connection": "o", "sql": "SELECT 1", "run": false}));
    assert!(err && text.contains("off"), "{text}");
    let (err, text) = call(json!({"connection": "f", "sql": "SELECT $1", "params": [1], "run": false}));
    assert!(err && text.contains("params"), "{text}");
    let (err, text) = call(json!({"connection": "r", "sql": "DELETE FROM orders", "run": true}));
    assert!(err && text.contains("reads only"), "{text}");
    std::fs::write(dir.join("settings.toml"), "mcp_enabled = false\n").unwrap();
    let (err, text) = call(json!({"connection": "f", "sql": "SELECT 1", "run": false}));
    assert!(err && text.contains("turned off"), "{text}");
    std::fs::remove_file(dir.join("settings.toml")).unwrap();
    hub.set_front(None);
    let (err, text) = call(json!({"connection": "f", "sql": "DELETE FROM orders", "run": false}));
    assert!(err && text.contains("drafts need Dataman open"), "{text}");
    assert_eq!(app.drafts.lock().unwrap().len(), 21);
    let log = std::fs::read_to_string(dir.join("mcp-audit.jsonl")).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let d = lines.iter().find(|l| l["draft"] == true && l["connection"] == "r").unwrap();
    assert_eq!((d["outcome"].as_str(), d["sql"].as_str(), d["query"].as_str(), d["database"].as_str()), (Some("ok"), Some("DELETE FROM orders"), Some("DELETE FROM orders"), Some("shop")), "{d}");
    assert!(d.get("rows").is_none());
    assert_eq!(lines.iter().filter(|l| l["draft"] == true).count(), 21);
    assert!(lines.iter().filter(|l| l["outcome"] != "ok").all(|l| l.get("draft").is_none()));
    std::mem::forget(hub);
}

#[derive(Default)]
struct Denier(std::sync::Mutex<Vec<dataman_mcp::Approval>>);

impl dataman_mcp::Front for Denier {
    fn approve(&self, a: dataman_mcp::Approval) -> futures_util::future::BoxFuture<'static, bool> {
        self.0.lock().unwrap().push(a);
        Box::pin(async { false })
    }
}

#[test]
fn stdio_hands_calls_to_the_app() {
    if std::net::TcpStream::connect_timeout(&"127.0.0.1:55432".parse().unwrap(), Duration::from_secs(1)).is_err() {
        eprintln!("skipping: no PostgreSQL at 127.0.0.1:55432");
        return;
    }
    let cfg = tempfile::tempdir().unwrap();
    let dir = cfg.path().join("dataman");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("connections.toml"),
        "[[connection]]\nid = \"pf\"\nname = \"pg full\"\nhost = \"127.0.0.1\"\nport = 55432\nuser = \"postgres\"\ndatabase = \"dataman\"\nsslmode = \"disable\"\nmcp = \"full\"\n",
    )
    .unwrap();
    let pgpass = cfg.path().join("pgpass");
    std::fs::write(&pgpass, "127.0.0.1:55432:*:postgres:dataman\n").unwrap();
    std::fs::set_permissions(&pgpass, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600)).unwrap();
    let hub = dataman_mcp::Hub::new(dataman_core::Dirs::at(&dir), Arc::new(dataman_core::MemorySecrets::default())).unwrap();
    let hub = {
        let mut h = Arc::try_unwrap(hub).ok().unwrap();
        h.core.pgpass = Some(pgpass);
        Arc::new(h)
    };
    let human = Arc::new(Denier::default());
    hub.set_front(Some(human.clone()));
    let reg = dataman_mcp::registry(&hub.dirs);
    let (h, r) = (hub.clone(), reg.clone());
    let app = dataman_mcp::ipc::serve(&hub.dirs, Arc::new(move || Ok((h.clone(), r.clone())))).unwrap();

    let mut m = Mcp::spawn(cfg.path());
    m.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "forward-test", "version": "1"}}));
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let drop_sql = "DROP TABLE IF EXISTS mcp_ipc_never_there";
    let (err, text) = m.tool("execute", json!({"connection": "pg full", "sql": drop_sql, "confirm": true}));
    assert!(err && text.contains("denied in Dataman"), "{text}");
    {
        let asked = human.0.lock().unwrap();
        assert_eq!(asked.len(), 1, "asked in the app");
        assert_eq!((asked[0].client.as_str(), asked[0].connection.as_str(), asked[0].reasons.clone()), ("forward-test", "pg full", vec!["DROP TABLE".to_string()]));
    }
    let (err, text) = m.tool("run_query", json!({"connection": "pg full", "sql": "SELECT 1 AS one"}));
    assert!(!err && text.contains("\"one\""), "{text}");

    app.stop();
    let (err, text) = m.tool("execute", json!({"connection": "pg full", "sql": drop_sql}));
    assert!(err && text.contains("needs approval in Dataman; open the app"), "{text}");
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
