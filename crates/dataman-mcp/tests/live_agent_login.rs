//! Through the real MCP tools, agents run as the connection's agent login: the server says so
//! (`current_user`, `CURRENT_USER()`, `SUSER_SNAME()`, connectionStatus, ACL WHOAMI,
//! `_authenticate`), on every tool and after a reconnect, a database switch, a new password and
//! a restart of the server process; without its password (or with a wrong one) agents are
//! refused, never served on the user's login; what the user allowed runs on the user's login.
//!
//! Only with DATAMAN_AGENT_LOGIN_LIVE=1, on servers of their own (see dataman-core's
//! tests/live_agent_login.rs for the addresses).

use std::sync::Arc;
use std::time::Duration;

use dataman_core::agent_login::{self, Options, live};
use dataman_core::config::{ConnectionProfile, Engine, McpAccess, SslMode};
use dataman_core::{Dirs, MemorySecrets, SecretStore};
use dataman_mcp::{Approval, Front, Hub, Registry, Server};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn on() -> bool {
    std::env::var("DATAMAN_AGENT_LOGIN_LIVE").is_ok_and(|v| v == "1")
}

fn addr(var: &str, default: &str) -> (String, u16) {
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (h, p) = v.rsplit_once(':').unwrap();
    (h.into(), p.parse().unwrap())
}

fn pw(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.into())
}

fn admin(engine: Engine, id: &str) -> (ConnectionProfile, String) {
    let (var, default, user, db, password, ssl) = match engine {
        Engine::Postgres => ("DATAMAN_AU_PG", "127.0.0.1:56432", "postgres", "au_mcp", pw("DATAMAN_AU_PG_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::MySql => ("DATAMAN_AU_MYSQL", "127.0.0.1:34306", "root", "au_mcp", pw("DATAMAN_AU_MYSQL_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::SqlServer => ("DATAMAN_AU_MSSQL", "127.0.0.1:15330", "sa", "au_mcp", pw("DATAMAN_AU_MSSQL_PASSWORD", "Admin-pw-123"), SslMode::Require),
        Engine::MongoDb => ("DATAMAN_AU_MONGO", "127.0.0.1:28017", "root", "au_mcp", pw("DATAMAN_AU_MONGO_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::Redis => ("DATAMAN_AU_REDIS", "127.0.0.1:64790", "", "0", pw("DATAMAN_AU_REDIS_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::Elasticsearch => ("DATAMAN_AU_ES", "127.0.0.1:9301", "elastic", "", pw("DATAMAN_AU_ES_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::OpenSearch => ("DATAMAN_AU_OS", "127.0.0.1:9302", "admin", "", pw("DATAMAN_AU_OS_PASSWORD", "Qx7#vT9!pLm2wZ"), SslMode::Require),
        _ => unreachable!(),
    };
    let (host, port) = addr(var, default);
    let mut p = ConnectionProfile { host, port, user: user.into(), database: db.into(), sslmode: ssl, engine, save_password: true, ..ConnectionProfile::new(id) };
    p.id = id.into();
    p.name = id.into();
    (p, password)
}

fn up(p: &ConnectionProfile) -> bool {
    std::net::TcpStream::connect_timeout(&format!("{}:{}", p.host, p.port).parse().unwrap(), Duration::from_secs(1)).is_ok()
}

#[derive(Default)]
struct Allows;

impl Front for Allows {
    fn approve(&self, _: Approval) -> BoxFuture<'static, bool> {
        Box::pin(async { true })
    }
}

struct Env {
    server: Server,
    dir: tempfile::TempDir,
    secrets: Arc<MemorySecrets>,
}

fn env_in(dir: tempfile::TempDir, secrets: Arc<MemorySecrets>) -> Env {
    let cfg = dir.path().join("dataman");
    let hub = Hub::new(Dirs::at(&cfg), secrets.clone()).unwrap();
    hub.set_front(Some(Arc::new(Allows)));
    let reg = Arc::new(Registry::standard(None));
    Env { server: Server::new(hub, reg, "stdio"), dir, secrets }
}

fn env() -> Env {
    env_in(tempfile::tempdir().unwrap(), Arc::new(MemorySecrets::default()))
}

impl Env {
    fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        let t = self.server.registry.find(tool).unwrap_or_else(|| panic!("no tool {tool}"));
        let r = self.server.hub.core.block_on(self.server.run_tool(t, args.as_object().unwrap().clone(), "live-test".into(), CancellationToken::new()));
        let text = match &r.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("{other:?}"),
        };
        if r.is_error == Some(true) { Err(text) } else { Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))) }
    }

    fn ok(&self, tool: &str, args: Value) -> Value {
        self.call(tool, args.clone()).unwrap_or_else(|e| panic!("{tool} {args}: {e}"))
    }

    fn audit(&self) -> Vec<Value> {
        let t = std::fs::read_to_string(self.dir.path().join("dataman/mcp-audit.jsonl")).unwrap_or_default();
        t.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn hub(&self) -> &Arc<Hub> {
        &self.server.hub
    }

    /// The process ends and a new one starts on the same files and Keychain.
    fn restart(self) -> Env {
        self.server.hub.close_all();
        let Env { server, dir, secrets } = self;
        std::mem::forget(server);
        env_in(dir, secrets)
    }
}

/// The connection saved with the user's password, then its agent login made by the user's login.
fn setup(e: &Env, mut p: ConnectionProfile, password: &str, seed: &[&str]) -> (ConnectionProfile, String) {
    let core = &e.hub().core;
    core.save_connection(p.clone(), Some(password), None).unwrap();
    let admin = core.block_on(core.connect(p.clone(), None)).unwrap();
    for s in seed {
        if p.engine.is_sql() {
            let rs = admin.session(None).execute(*s);
            rs.fetch_all();
            core.block_on(rs.wait_finished());
        } else {
            let _ = core.block_on(admin.run_nosql(&p.database, s, dataman_core::nosql::Page::default()));
        }
    }
    let user = agent_login::login_name(&p.id);
    if let Ok(Some(true)) = core.block_on(live::exists(&admin, &p, &user)) {
        let old = live::record(&p, &dataman_core::agent_login::Plan { user: user.clone(), ..Default::default() }, &Options::default());
        let plan = core.block_on(live::removal(&admin, &p, &old)).unwrap();
        core.block_on(live::apply(&admin, &plan, &p.database)).unwrap();
    }
    let o = Options::default();
    let rv = core.block_on(live::review(&admin, &p, &o)).unwrap();
    assert!(rv.plan.lacks.is_empty(), "{:?}", rv.plan.lacks);
    core.block_on(live::apply(&admin, &rv.plan, &p.database)).unwrap_or_else(|x| panic!("{x:#}\n{}", rv.plan.shown()));
    e.secrets.set(&agent_login::secret_account(&p.id), &rv.password).unwrap();
    p.agent_login = Some(live::record(&p, &rv.plan, &o));
    core.config.upsert(p.clone()).unwrap();
    (p, rv.password)
}

fn teardown(e: &Env, p: &ConnectionProfile) {
    let core = &e.hub().core;
    let Some(login) = p.agent_login.clone() else { return };
    let mine = ConnectionProfile { agent_login: None, ..p.clone() };
    let admin = core.block_on(core.connect(mine.clone(), None)).unwrap();
    e.hub().close_all();
    let plan = core.block_on(live::removal(&admin, &mine, &login)).unwrap();
    core.block_on(live::apply(&admin, &plan, &login.database)).unwrap_or_else(|x| panic!("{x:#}\n{}", plan.shown()));
}

/// Who the server says the agent is, through `tool`.
fn whoami(e: &Env, id: &str, engine: Engine, database: Option<&str>) -> Result<String, String> {
    let mut args = json!({"connection": id});
    if let Some(d) = database {
        args["database"] = json!(d);
    }
    let (tool, key, q) = match engine {
        Engine::Postgres => ("run_query", "sql", "SELECT current_user, session_user"),
        Engine::MySql => ("run_query", "sql", "SELECT CURRENT_USER()"),
        Engine::SqlServer => ("run_query", "sql", "SELECT SUSER_SNAME(), USER_NAME()"),
        Engine::MongoDb => ("nosql_query", "query", "db.runCommand({connectionStatus: 1})"),
        Engine::Redis => ("nosql_query", "query", "ACL WHOAMI"),
        Engine::Elasticsearch => ("nosql_query", "query", "GET /_security/_authenticate"),
        Engine::OpenSearch => ("nosql_query", "query", "GET /_plugins/_security/authinfo"),
        _ => unreachable!(),
    };
    args[key] = json!(q);
    let v = e.call(tool, args)?;
    Ok(match engine {
        Engine::Postgres | Engine::SqlServer => {
            let row = &v["rows"][0];
            let me = row[0].as_str().unwrap_or("").to_string();
            // The login and its user (current_user and session_user): the same for the agent's.
            if me.starts_with(agent_login::PREFIX) {
                assert_eq!(row[0], row[1], "{v}");
            }
            me
        }
        Engine::MySql => v["rows"][0][0].as_str().unwrap_or("").split('@').next().unwrap_or("").to_string(),
        Engine::MongoDb => v["value"]["authInfo"]["authenticatedUsers"][0]["user"].as_str().or(v["documents"][0]["authInfo"]["authenticatedUsers"][0]["user"].as_str()).unwrap_or("").to_string(),
        Engine::Redis => v["value"].as_str().unwrap_or("").to_string(),
        Engine::Elasticsearch => v["value"]["username"].as_str().or(v["documents"][0]["username"].as_str()).unwrap_or("").to_string(),
        _ => v["value"]["user_name"].as_str().or(v["documents"][0]["user_name"].as_str()).unwrap_or("").to_string(),
    })
}

/// The whole round: identity on every path, a reconnect, a rotation, a restart, the password
/// gone or wrong, what the user allowed, and the connection after its agent login is removed.
fn round(engine: Engine, id: &str, seed: &[&str], extra: impl Fn(&Env, &ConnectionProfile, &str)) {
    if !on() {
        return;
    }
    let (mut p, password) = admin(engine, id);
    if !up(&p) {
        eprintln!("skipping: no {} at {}:{}", engine.label(), p.host, p.port);
        return;
    }
    p.mcp = Some(McpAccess::Full);
    let e = env();
    let (p, mut agent_pw) = setup(&e, p, &password, seed);
    let login = p.agent_login.clone().unwrap();
    let user = login.user.clone();
    assert_eq!(whoami(&e, id, engine, None).unwrap(), user);
    extra(&e, &p, &user);
    // The user's own connection (their tabs) is still the user's.
    let core = &e.hub().core;
    let mine = core.block_on(core.connect(ConnectionProfile { agent_login: None, ..p.clone() }, None)).unwrap();
    assert_ne!(mine.info.user, user);
    // A reconnect.
    e.hub().close_all();
    assert_eq!(whoami(&e, id, engine, None).unwrap(), user, "after a reconnect");
    // A new password: the server's, then the Keychain's; the next call connects with it.
    let new = agent_login::hash::password();
    let plan = core.block_on(live::rotation(&mine, &p, &login, &agent_pw, &new)).unwrap();
    core.block_on(live::apply(&mine, &plan, &login.database)).unwrap();
    e.secrets.set(&agent_login::secret_account(&p.id), &new).unwrap();
    let mut p2 = p.clone();
    p2.agent_login.as_mut().unwrap().rotated = Some(chrono::Utc::now());
    core.config.upsert(p2.clone()).unwrap();
    agent_pw = new;
    assert_eq!(whoami(&e, id, engine, None).unwrap(), user, "after a new password");
    // The audit says which login ran each call.
    assert!(e.audit().iter().rev().take(3).all(|x| x["login"] == user.as_str()), "{:?}", e.audit().last());
    // Dataman restarts: the new process reads the Keychain itself.
    let e = e.restart();
    assert_eq!(whoami(&e, id, engine, None).unwrap(), user, "after a restart");
    // Its password wrong, then gone: refused, never the user's login.
    e.secrets.set(&agent_login::secret_account(&p.id), "wrong-password-0000000000").unwrap();
    e.hub().close_all();
    let err = whoami(&e, id, engine, None).unwrap_err();
    assert!(err.contains("agent login") && err.contains("never uses your own login"), "{err}");
    e.secrets.delete(&agent_login::secret_account(&p.id)).unwrap();
    e.hub().close_all();
    let err = whoami(&e, id, engine, None).unwrap_err();
    assert!(err.contains("not in the") && err.contains("refused"), "{err}");
    e.secrets.set(&agent_login::secret_account(&p.id), &agent_pw).unwrap();
    assert_eq!(whoami(&e, id, engine, None).unwrap(), user);
    // Moved to another server since: refused.
    let moved = ConnectionProfile { port: p2.port + 1, ..p2.clone() };
    e.hub().core.config.upsert(moved).unwrap();
    assert!(whoami(&e, id, engine, None).unwrap_err().contains("another server"));
    e.hub().core.config.upsert(p2.clone()).unwrap();
    // Removed: refused until a new one, or the user says agents may use theirs.
    teardown(&e, &p2);
    e.hub().core.config.upsert(ConnectionProfile { agent_login: None, agents_refused: true, ..p2.clone() }).unwrap();
    let err = whoami(&e, id, engine, None).unwrap_err();
    assert!(err.contains("agents are refused"), "{err}");
    e.hub().core.config.upsert(ConnectionProfile { agent_login: None, agents_refused: false, ..p2.clone() }).unwrap();
    let me = if engine == Engine::Redis { "default".to_string() } else { mine.info.user.clone() };
    assert_eq!(whoami(&e, id, engine, None).unwrap(), me, "the user let agents use their login");
    e.hub().close_all();
    std::mem::forget(e);
}

#[test]
fn postgres() {
    let seed = [
        "DROP TABLE IF EXISTS public.au_t, public.au_ddl",
        "CREATE TABLE public.au_t (id int PRIMARY KEY, name text)",
        "INSERT INTO public.au_t VALUES (1, 'a')",
    ];
    // The database itself first (the admin's own connection is on it).
    if on() {
        let (p, password) = admin(Engine::Postgres, "aumcppg01");
        if up(&p) {
            let core = dataman_core::Core::new(Dirs::at(tempfile::tempdir().unwrap().path()), Arc::new(MemorySecrets::default())).unwrap();
            let c = core.block_on(core.connect(ConnectionProfile { database: "postgres".into(), ..p.clone() }, Some(password))).unwrap();
            for s in ["DROP DATABASE IF EXISTS au_mcp WITH (FORCE)", "CREATE DATABASE au_mcp"] {
                let rs = c.session(None).execute(s);
                core.block_on(rs.wait_finished());
            }
        }
    }
    round(Engine::Postgres, "aumcppg01", &seed, |e, p, user| {
        let id = p.id.as_str();
        // Every tool, and another database of the same server.
        let r = e.ok("list_tables", json!({"connection": id}));
        assert!(r.to_string().contains("au_t"), "{r}");
        e.ok("describe_table", json!({"connection": id, "table": "au_t"}));
        e.ok("sample_rows", json!({"connection": id, "table": "au_t"}));
        let r = e.ok("execute", json!({"connection": id, "sql": "INSERT INTO public.au_t VALUES (2, 'b')"}));
        assert_eq!(r["statements"][0], json!(["INSERT", 1]));
        assert_eq!(whoami(e, id, Engine::Postgres, Some("postgres")).unwrap(), user, "another database: still the agent");
        let r = e.ok("activity_sessions", json!({"connection": id, "include_idle": true}));
        assert!(r["rows"].as_array().is_some(), "{r}");
        // DDL the user allows runs on the user's login: the agent's can't.
        e.ok("execute", json!({"connection": id, "sql": "CREATE TABLE public.au_ddl (i int)"}));
        let last = e.audit().into_iter().rev().find(|x| x["tool"] == "execute").unwrap();
        assert_eq!(last["login"], "you", "{last}");
        let owner = e.ok("run_query", json!({"connection": id, "sql": "SELECT tableowner FROM pg_tables WHERE tablename = 'au_ddl'"}));
        assert_eq!(owner["rows"][0][0], "postgres");
        assert!(e.ok("run_query", json!({"connection": id, "sql": "SELECT has_table_privilege('public.au_ddl', 'SELECT')"}))["rows"][0][0] == json!(false), "the agent's login has no grant on what the user made since (fail closed)");
    });
}

#[test]
fn mysql() {
    let seed = ["CREATE DATABASE IF NOT EXISTS au_mcp", "DROP TABLE IF EXISTS au_mcp.au_t", "CREATE TABLE au_mcp.au_t (id INT PRIMARY KEY, name VARCHAR(10))", "INSERT INTO au_mcp.au_t VALUES (1, 'a')"];
    if on() {
        let (p, password) = admin(Engine::MySql, "aumcpmy01");
        if up(&p) {
            let core = dataman_core::Core::new(Dirs::at(tempfile::tempdir().unwrap().path()), Arc::new(MemorySecrets::default())).unwrap();
            let c = core.block_on(core.connect(ConnectionProfile { database: String::new(), ..p.clone() }, Some(password))).unwrap();
            let rs = c.session(None).execute("CREATE DATABASE IF NOT EXISTS au_mcp");
            core.block_on(rs.wait_finished());
        }
    }
    round(Engine::MySql, "aumcpmy01", &seed, |e, p, user| {
        e.ok("list_tables", json!({"connection": p.id}));
        e.ok("sample_rows", json!({"connection": p.id, "table": "au_t"}));
        let r = e.ok("execute", json!({"connection": p.id, "sql": "INSERT INTO au_mcp.au_t VALUES (2, 'b')"}));
        assert_eq!(r["statements"][0], json!(["INSERT", 1]));
        assert_eq!(whoami(e, &p.id, Engine::MySql, None).unwrap(), user);
    });
}

#[test]
fn sql_server() {
    if on() {
        let (p, password) = admin(Engine::SqlServer, "aumcpms01");
        if up(&p) {
            let core = dataman_core::Core::new(Dirs::at(tempfile::tempdir().unwrap().path()), Arc::new(MemorySecrets::default())).unwrap();
            let c = core.block_on(core.connect(ConnectionProfile { database: "master".into(), ..p.clone() }, Some(password))).unwrap();
            let rs = c.session(None).execute("IF DB_ID('au_mcp') IS NULL CREATE DATABASE au_mcp");
            core.block_on(rs.wait_finished());
        }
    }
    let seed = ["IF OBJECT_ID('dbo.au_t') IS NOT NULL DROP TABLE dbo.au_t", "CREATE TABLE dbo.au_t (id INT PRIMARY KEY, name NVARCHAR(10))", "INSERT INTO dbo.au_t VALUES (1, N'a')"];
    round(Engine::SqlServer, "aumcpms01", &seed, |e, p, user| {
        e.ok("list_tables", json!({"connection": p.id}));
        let r = e.ok("execute", json!({"connection": p.id, "sql": "INSERT INTO dbo.au_t VALUES (2, N'b')"}));
        assert!(r.to_string().contains("INSERT"), "{r}");
        assert_eq!(whoami(e, &p.id, Engine::SqlServer, None).unwrap(), user);
    });
}

#[test]
fn mongodb() {
    let seed = ["db.au_t.drop()", "db.au_t.insertOne({name: 'a'})"];
    round(Engine::MongoDb, "aumcpmg01", &seed, |e, p, user| {
        e.ok("list_tables", json!({"connection": p.id}));
        let r = e.ok("nosql_query", json!({"connection": p.id, "query": "db.au_t.find({})"}));
        assert_eq!(r["count"], 1);
        // The shell: a script runs as the agent too.
        let r = e.ok("nosql_query", json!({"connection": p.id, "query": "const s = db.runCommand({connectionStatus: 1}); s.authInfo.authenticatedUsers[0].user"}));
        assert!(r.to_string().contains(user), "{r}");
        e.ok("nosql_execute", json!({"connection": p.id, "query": "db.au_t.insertOne({name: 'b'})"}));
    });
}

#[test]
fn redis() {
    round(Engine::Redis, "aumcprd01", &["SET au:k v"], |e, p, _| {
        let r = e.ok("nosql_query", json!({"connection": p.id, "query": "GET au:k"}));
        assert!(r.to_string().contains('v'), "{r}");
        e.ok("nosql_execute", json!({"connection": p.id, "query": "SET au:k w"}));
    });
}

#[test]
fn elasticsearch() {
    round(Engine::Elasticsearch, "aumcpes01", &["PUT /au_mcp_t/_doc/1?refresh=true\n{\"name\": \"a\"}"], |e, p, _| {
        let r = e.ok("nosql_query", json!({"connection": p.id, "query": "GET /au_mcp_t/_search"}));
        assert!(r.to_string().contains("\"a\""), "{r}");
    });
}

#[test]
fn opensearch() {
    round(Engine::OpenSearch, "aumcpos01", &["PUT /au_mcp_t/_doc/1?refresh=true\n{\"name\": \"a\"}"], |e, p, _| {
        let r = e.ok("nosql_query", json!({"connection": p.id, "query": "GET /au_mcp_t/_search"}));
        assert!(r.to_string().contains("\"a\""), "{r}");
    });
}

/// Masked columns through the login's view: the agent's statement names the table, Dataman
/// sends it to the view, and the server masks; Dataman's own refusals still come first.
#[test]
fn postgres_masked_through_views() {
    if !on() {
        return;
    }
    let (mut p, password) = admin(Engine::Postgres, "aumcpq02");
    if !up(&p) {
        return;
    }
    p.database = "au_mcp2".into();
    {
        let core = dataman_core::Core::new(Dirs::at(tempfile::tempdir().unwrap().path()), Arc::new(MemorySecrets::default())).unwrap();
        let c = core.block_on(core.connect(ConnectionProfile { database: "postgres".into(), ..p.clone() }, Some(password.clone()))).unwrap();
        for s in ["DROP DATABASE IF EXISTS au_mcp2 WITH (FORCE)", "CREATE DATABASE au_mcp2", "DROP ROLE IF EXISTS dataman_agent_aumcpq02"] {
            let rs = c.session(None).execute(s);
            core.block_on(rs.wait_finished());
        }
    }
    p.mcp = Some(McpAccess::Read);
    p.mcp_rules.view = dataman_core::privacy::AgentView::Masked;
    p.mcp_rules.sensitive.push(dataman_core::privacy::SensitiveCol { col: "users.email".into(), how: dataman_core::privacy::Mask::Partial, on: true });
    p.mcp_rules.visible.items.push(dataman_core::privacy::VisibleItem { name: "users.password_hash".into(), kind: dataman_core::privacy::ItemKind::Column });
    let e = env();
    let seed = ["CREATE TABLE public.users (id int PRIMARY KEY, name text, email text, password_hash text)", "INSERT INTO public.users VALUES (1, 'Ana', 'ana@acme.co', 'h1')"];
    let (p, _) = setup(&e, p, &password, &seed);
    let user = p.agent_login.as_ref().unwrap().user.clone();
    assert_eq!(p.agent_login.as_ref().unwrap().views.len(), 1);
    let r = e.ok("run_query", json!({"connection": p.id, "sql": "SELECT id, name, email FROM users"}));
    assert_eq!(r["rows"][0], json!([1, "Ana", "a•••@acme.co"]), "{r}");
    let last = e.audit().into_iter().rev().find(|x| x["tool"] == "run_query").unwrap();
    assert!(last["rewritten"].as_str().unwrap_or("").contains(&user), "read through the login's view: {last}");
    assert_eq!(last["login"], user.as_str());
    let err = e.call("run_query", json!({"connection": p.id, "sql": "SELECT password_hash FROM users"})).unwrap_err();
    assert!(err.contains("hidden"), "Dataman refuses first: {err}");
    let r = e.ok("run_query", json!({"connection": p.id, "sql": "SELECT * FROM users"}));
    assert!(!r.to_string().contains("h1") && !r.to_string().contains("ana@"), "{r}");
    teardown(&e, &p);
    e.hub().close_all();
    std::mem::forget(e);
}
