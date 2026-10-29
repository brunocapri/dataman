//! Red team of the agent login through the MCP: every tool, and what the server says about who
//! connected; twins of a connection with an agent login; the login changed while a call waits
//! for the user. The user's login here is a role of its own (`au_owner`), so a session as the
//! user can be told apart from the test's admin.
//!
//! Only with DATAMAN_AGENT_LOGIN_LIVE=1, on servers of their own (see dataman-core's
//! tests/live_agent_login.rs for the addresses). The twin and wait tests need no server.

use std::sync::Arc;
use std::time::Duration;

use dataman_core::agent_login::{self, Options, live};
use dataman_core::config::{ConnectionProfile, McpAccess, SslMode};
use dataman_core::{Dirs, MemorySecrets, SecretStore};
use dataman_mcp::{Approval, Front, Hub, Registry, Server};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn on() -> bool {
    std::env::var("DATAMAN_AGENT_LOGIN_LIVE").is_ok_and(|v| v == "1")
}

fn pg_addr() -> (String, u16) {
    let v = std::env::var("DATAMAN_AU_PG").unwrap_or_else(|_| "127.0.0.1:56432".into());
    let (h, p) = v.rsplit_once(':').unwrap();
    (h.into(), p.parse().unwrap())
}

fn admin_pw() -> String {
    std::env::var("DATAMAN_AU_PG_PASSWORD").unwrap_or_else(|_| "admin-pw".into())
}

fn up(host: &str, port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&format!("{host}:{port}").parse().unwrap(), Duration::from_secs(1)).is_ok()
}

/// Approves every ask; `during` runs first (the user doing something meanwhile).
#[derive(Default)]
struct Allows {
    during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Front for Allows {
    fn approve(&self, _: Approval) -> BoxFuture<'static, bool> {
        if let Some(f) = self.during.lock().take() {
            f();
        }
        Box::pin(async { true })
    }
}

struct Env {
    server: Server,
    dir: tempfile::TempDir,
    secrets: Arc<MemorySecrets>,
    front: Arc<Allows>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let hub = Hub::new(Dirs::at(dir.path().join("dataman")), secrets.clone()).unwrap();
    let front = Arc::new(Allows::default());
    hub.set_front(Some(front.clone()));
    let reg = Arc::new(Registry::standard(None));
    Env { server: Server::new(hub, reg, "stdio"), dir, secrets, front }
}

impl Env {
    fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        let t = self.server.registry.find(tool).unwrap_or_else(|| panic!("no tool {tool}"));
        let r = self.server.hub.core.block_on(self.server.run_tool(t, args.as_object().unwrap().clone(), "redteam".into(), CancellationToken::new()));
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
}

fn exec(core: &dataman_core::Core, c: &Arc<dataman_core::Connection>, sql: &str) -> Vec<Vec<Option<String>>> {
    let rs = c.session(None).execute(sql);
    rs.fetch_all();
    let ph = core.block_on(rs.wait_finished());
    assert_eq!(ph, dataman_core::result::Phase::Done, "{sql}: {:?}", rs.status().error);
    let snap = rs.snapshot();
    (0..snap.rows()).map(|r| (0..snap.columns.len()).map(|k| snap.text(r, k).map(|t| t.into_owned())).collect()).collect()
}

/// Backends of `user` on the server now.
fn pids_of(core: &dataman_core::Core, root: &Arc<dataman_core::Connection>, user: &str) -> Vec<String> {
    exec(core, root, &format!("SELECT pid::text FROM pg_stat_activity WHERE usename = '{user}' ORDER BY 1")).into_iter().filter_map(|r| r[0].clone()).collect()
}

/// Every tool that reaches the server, on a connection with an agent login: the server never
/// sees a new session as the user's login, and the audit names the agent's for each call; only
/// what the user allows in the app (cancel a backend) runs as the user, and the audit says so.
#[test]
fn postgres_every_tool_runs_as_the_agent() {
    if !on() {
        return;
    }
    let (host, port) = pg_addr();
    if !up(&host, port) {
        return;
    }
    let e = env();
    let hub = e.hub().clone();
    let core = &hub.core;
    let root = core.block_on(core.connect(ConnectionProfile { host: host.clone(), port, user: "postgres".into(), database: "postgres".into(), sslmode: SslMode::Disable, ..ConnectionProfile::new("root") }, Some(admin_pw()))).unwrap();
    let id = "aumcprt001";
    let user = agent_login::login_name(id);
    for sql in [
        "DROP DATABASE IF EXISTS au_rtm WITH (FORCE)".to_string(),
        format!("DROP ROLE IF EXISTS {user}"),
        "DROP ROLE IF EXISTS au_owner".into(),
        "CREATE ROLE au_owner LOGIN CREATEROLE PASSWORD 'Owner-pw-1'".into(),
        "CREATE DATABASE au_rtm OWNER au_owner".into(),
    ] {
        exec(core, &root, &sql);
    }
    let mut p = ConnectionProfile { host, port, user: "au_owner".into(), database: "au_rtm".into(), sslmode: SslMode::Disable, save_password: true, ..ConnectionProfile::new(id) };
    p.id = id.into();
    p.name = "rt".into();
    p.mcp = Some(McpAccess::Full);
    p.mcp_rules.view = dataman_core::privacy::AgentView::Masked;
    p.mcp_rules.sensitive.push(dataman_core::privacy::SensitiveCol { col: "users.email".into(), how: dataman_core::privacy::Mask::Partial, on: true });
    core.save_connection(p.clone(), Some("Owner-pw-1"), None).unwrap();
    {
        let admin = core.block_on(core.connect(p.clone(), None)).unwrap();
        for sql in ["CREATE TABLE t (id int PRIMARY KEY, name text)", "INSERT INTO t VALUES (1, 'a')", "CREATE TABLE users (id int PRIMARY KEY, email text)", "INSERT INTO users VALUES (1, 'ana@acme.co')"] {
            exec(core, &admin, sql);
        }
        let rv = core.block_on(live::review(&admin, &p, &Options::default())).unwrap();
        assert!(rv.plan.lacks.is_empty(), "{:?}", rv.plan.lacks);
        core.block_on(live::apply(&admin, &rv.plan, &p.database)).unwrap();
        e.secrets.set(&agent_login::secret_account(&p.id), &rv.password).unwrap();
        p.agent_login = Some(live::record(&p, &rv.plan, &Options::default()));
        core.config.upsert(p.clone()).unwrap();
        admin.close();
    }
    std::thread::sleep(Duration::from_millis(300));
    let before = pids_of(core, &root, "au_owner");
    let c = json!(id);
    let uri = format!("dataman://{id}/au_rtm/schema");
    e.ok("list_connections", json!({"probe": true}));
    e.ok("list_schemas", json!({"connection": c}));
    e.ok("list_tables", json!({"connection": c}));
    e.ok("search_objects", json!({"connection": c, "pattern": "t"}));
    e.ok("describe_table", json!({"connection": c, "table": "t", "include_ddl": true}));
    e.ok("get_ddl", json!({"connection": c, "object": "t"}));
    e.ok("sample_rows", json!({"connection": c, "table": "t"}));
    let who = e.ok("run_query", json!({"connection": c, "sql": "SELECT current_user, session_user"}));
    assert_eq!(who["rows"][0], json!([user, user]), "{who}");
    let masked = e.ok("run_query", json!({"connection": c, "sql": "SELECT email FROM users"}));
    assert_eq!(masked["rows"][0][0], "a•••@acme.co", "{masked}");
    e.ok("run_query", json!({"connection": c, "database": "postgres", "sql": "SELECT 1"}));
    e.ok("explain", json!({"connection": c, "sql": "SELECT * FROM t"}));
    e.ok("explain_analyze", json!({"connection": c, "sql": "SELECT * FROM t"}));
    e.ok("execute", json!({"connection": c, "sql": "INSERT INTO t VALUES (2, 'b')"}));
    e.ok("add_favorite", json!({"name": "rt who", "sql": "SELECT current_user", "connection": c}));
    let fav = e.ok("run_favorite", json!({"name": "rt who"}));
    assert!(fav.to_string().contains(&user), "{fav}");
    // Activity is off while data is kept: refused by Dataman first, nothing opened as the user.
    assert!(e.call("activity_sessions", json!({"connection": c, "include_idle": true})).unwrap_err().contains("activity is off"));
    let schema = core.block_on(e.server.read_text(&uri, "redteam".into(), CancellationToken::new())).unwrap();
    assert!(schema.contains("users"), "{schema}");
    assert!(!schema.contains(&agent_login::space_of(&user)), "the login's own views are not listed: {schema}");
    // A reconnect after every connection closed, then the same again.
    e.hub().close_all();
    let who = e.ok("run_query", json!({"connection": c, "sql": "SELECT current_user"}));
    assert_eq!(who["rows"][0][0], user.as_str());
    std::thread::sleep(Duration::from_millis(200));
    let after = pids_of(core, &root, "au_owner");
    let new: Vec<&String> = after.iter().filter(|x| !before.contains(x)).collect();
    assert!(new.is_empty(), "sessions as the user's login opened by agent calls: {new:?}");
    for x in e.audit().iter().filter(|x| x["database"].is_string()) {
        assert_eq!(x["login"], user.as_str(), "{x}");
    }
    // Nothing kept now: activity is the agents', as the agent login.
    p.mcp_rules = Default::default();
    core.config.upsert(p.clone()).unwrap();
    e.ok("activity_sessions", json!({"connection": c, "include_idle": true}));
    e.ok("activity_locks", json!({"connection": c}));
    std::thread::sleep(Duration::from_millis(200));
    let after = pids_of(core, &root, "au_owner");
    assert!(after.iter().all(|x| before.contains(x)), "activity tools opened a session as the user's login: {after:?} (before {before:?})");
    // What the user allows in the app runs on the user's login, and the audit says so.
    // (Dataman's own backends are refused first: a pid no session has.) A cancel needs no
    // Allow here: it runs as the agent's login; a terminate waits for the Allow, then runs on
    // the user's.
    e.ok("cancel_backend", json!({"connection": c, "pid": 2_000_000}));
    let last = e.audit().into_iter().rev().find(|x| x["tool"] == "cancel_backend").unwrap();
    assert_eq!(last["login"], user.as_str(), "{last}");
    let r = e.call("terminate_backend", json!({"connection": c, "pid": 2_000_000}));
    let last = e.audit().into_iter().rev().find(|x| x["tool"] == "terminate_backend").unwrap();
    assert_eq!(last["login"], "you", "{r:?} {last}");
    assert_eq!(last["approval"]["decision"], "approved", "{last}");
    // The agent login removed: refused, and nothing open as it stays in the hub.
    let mine = core.block_on(core.connect(ConnectionProfile { agent_login: None, ..p.clone() }, None)).unwrap();
    let login = p.agent_login.clone().unwrap();
    assert!(!pids_of(core, &root, &user).is_empty(), "the hub's agent sessions are open");
    // A CREATEROLE admin (not a superuser) ends them too.
    core.block_on(live::end_sessions(&mine, &p, &user)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(pids_of(core, &root, &user).is_empty(), "ended: {:?}", pids_of(core, &root, &user));
    assert_eq!(exec(core, &root, &format!("SELECT count(*) FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.roleid WHERE r.rolname = '{user}' AND (m.inherit_option OR m.set_option)"))[0][0].as_deref(), Some("0"), "the admin kept no use of it (only the ADMIN that PostgreSQL gives the role's creator)");
    let plan = core.block_on(live::removal(&mine, &p, &login)).unwrap();
    core.block_on(live::apply(&mine, &plan, &login.database)).unwrap();
    core.config.upsert(ConnectionProfile { agent_login: None, agents_refused: true, ..p.clone() }).unwrap();
    let err = e.call("run_query", json!({"connection": c, "sql": "SELECT 1"})).unwrap_err();
    assert!(err.contains("agents are refused"), "{err}");
    assert!(e.hub().connected(id).is_none(), "refused: what was open for it closed");
    mine.close();
    e.hub().close_all();
    exec(core, &root, "DROP DATABASE IF EXISTS au_rtm WITH (FORCE)");
    exec(core, &root, "DROP ROLE IF EXISTS au_owner");
    std::mem::forget(e);
}

fn with_login(id: &str) -> ConnectionProfile {
    let mut p = ConnectionProfile { host: "db.example.com".into(), port: 5432, user: "app".into(), database: "shop".into(), save_password: true, ..ConnectionProfile::new(id) };
    p.id = id.into();
    p.name = id.into();
    p.mcp = Some(McpAccess::Read);
    p.agent_login = Some(live::record(&p, &agent_login::Plan { user: agent_login::login_name(id), ..Default::default() }, &Options::default()));
    p
}

/// A new connection to what one with an agent login reaches, as the same user, would let
/// agents in as the user (a `~/.pgpass` line, trust or peer authentication): refused, by any
/// of the machine's names too. Agents refused there: the same.
#[test]
fn a_twin_of_an_agent_login_connection_is_refused() {
    let e = env();
    let core = &e.hub().core;
    let mut a = with_login("twinbase01");
    a.host = "localhost".into();
    core.config.upsert(a.clone()).unwrap();
    let refused = ConnectionProfile { id: "twinbase02".into(), name: "twinbase02".into(), host: "db2.example.com".into(), agent_login: None, agents_refused: true, ..a.clone() };
    core.config.upsert(refused.clone()).unwrap();
    for (host, port) in [("localhost", 5432), ("127.0.0.1", 5432), ("::1", 5432), ("LOCALHOST", 5432), ("db2.example.com", 5432), ("DB2.example.com.", 5432)] {
        let r = e.call("create_connection", json!({"name": format!("twin {host}"), "engine": "postgres", "host": host, "port": port, "user": "app", "database": "shop"}));
        let err = r.expect_err(&format!("{host}: a twin"));
        assert!(err.contains("already reaches") && err.contains("agent login"), "{host}: {err}");
    }
    // Another user is another login: not a twin (it needs that login's own password).
    e.ok("create_connection", json!({"name": "other user", "engine": "postgres", "host": "localhost", "port": 5432, "user": "someone_else", "database": "shop"}));
}

/// The user makes or removes the agent login while an agent's update_connection waits for the
/// user's Allow (a move): the save keeps what the user did, never the copy the call started
/// from.
#[test]
fn an_update_never_undoes_the_agent_login_meanwhile() {
    for removed in [true, false] {
        let e = env();
        let hub = e.hub().clone();
        let core = &hub.core;
        let p = with_login("waitconn01");
        let start = if removed { p.clone() } else { ConnectionProfile { agent_login: None, ..p.clone() } };
        core.config.upsert(start).unwrap();
        let (h2, p2) = (hub.clone(), p.clone());
        *e.front.during.lock() = Some(Box::new(move || {
            let now = if removed { ConnectionProfile { agent_login: None, agents_refused: true, ..p2.clone() } } else { p2.clone() };
            h2.core.config.upsert(now).unwrap();
        }));
        e.ok("update_connection", json!({"connection": "waitconn01", "port": 6543}));
        core.config.refresh();
        let got = core.config.get("waitconn01").unwrap();
        assert_eq!(got.port, 6543, "the move went through");
        if removed {
            assert!(got.agent_login.is_none() && got.agents_refused, "removed meanwhile stays removed: {:?}", got.agent_login);
        } else {
            assert!(got.agent_login.is_some(), "made meanwhile stays: agents would otherwise be back on the user's login");
        }
        // The connection moved: agents are refused (the login was made for the old server).
        if !removed {
            let err = e.call("run_query", json!({"connection": "waitconn01", "sql": "SELECT 1"})).unwrap_err();
            assert!(err.contains("another server"), "{err}");
        }
    }
}
