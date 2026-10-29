//! The agent login against real servers: created with the admin's login, then every rule tried
//! as the agent itself, straight on the engine (Dataman's own checks not in the way), then drift
//! and its sync, a new password, and Remove (which leaves alone what Dataman didn't make).
//!
//! These create logins and roles, so they never run against the shared test servers: only with
//! DATAMAN_AGENT_LOGIN_LIVE=1, on servers of their own (DATAMAN_AU_PG, DATAMAN_AU_MYSQL,
//! DATAMAN_AU_MSSQL, DATAMAN_AU_MONGO, DATAMAN_AU_REDIS, DATAMAN_AU_ES, DATAMAN_AU_OS: host:port,
//! with the admin password in DATAMAN_AU_<ENGINE>_PASSWORD).

mod common;

use common::*;
use dataman_core::agent_login::{self, AgentLogin, Options, live};
use dataman_core::config::{ConnectionProfile, Engine, McpAccess, SslMode};
use dataman_core::nosql::{Output, Page};
use dataman_core::privacy::{AgentView, ItemKind, Mask, SensitiveCol, VisibleItem};
use dataman_core::Session;

fn on() -> bool {
    std::env::var("DATAMAN_AGENT_LOGIN_LIVE").is_ok_and(|v| v == "1")
}

/// One test at a time: MySQL's and MongoDB's inventory is the whole server's, so another
/// test's tables would show up in this one's drift.
fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

fn addr(var: &str, default: &str) -> (String, u16) {
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (h, p) = v.rsplit_once(':').unwrap();
    (h.into(), p.parse().unwrap())
}

fn pw(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.into())
}

/// An admin's profile on one of this test's own servers.
pub fn admin(engine: Engine, id: &str) -> (ConnectionProfile, String) {
    let (var, default, user, db, password, ssl) = match engine {
        Engine::Postgres => ("DATAMAN_AU_PG", "127.0.0.1:56432", "postgres", "au_shop", pw("DATAMAN_AU_PG_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::MySql => ("DATAMAN_AU_MYSQL", "127.0.0.1:34306", "root", "au_shop", pw("DATAMAN_AU_MYSQL_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::SqlServer => ("DATAMAN_AU_MSSQL", "127.0.0.1:15330", "sa", "au_shop", pw("DATAMAN_AU_MSSQL_PASSWORD", "Admin-pw-123"), SslMode::Require),
        Engine::MongoDb => ("DATAMAN_AU_MONGO", "127.0.0.1:28017", "root", "au_shop", pw("DATAMAN_AU_MONGO_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::Redis => ("DATAMAN_AU_REDIS", "127.0.0.1:64790", "", "0", pw("DATAMAN_AU_REDIS_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::Elasticsearch => ("DATAMAN_AU_ES", "127.0.0.1:9301", "elastic", "", pw("DATAMAN_AU_ES_PASSWORD", "admin-pw"), SslMode::Disable),
        Engine::OpenSearch => ("DATAMAN_AU_OS", "127.0.0.1:9302", "admin", "", pw("DATAMAN_AU_OS_PASSWORD", "Qx7#vT9!pLm2wZ"), SslMode::Require),
        _ => unreachable!(),
    };
    let (host, port) = addr(var, default);
    let mut p = ConnectionProfile { host, port, user: user.into(), database: db.into(), sslmode: ssl, engine, ..ConnectionProfile::new(id) };
    p.id = id.into();
    (p, password)
}

fn up(p: &ConnectionProfile) -> bool {
    std::net::TcpStream::connect_timeout(&format!("{}:{}", p.host, p.port).parse().unwrap(), Duration::from_secs(1)).is_ok()
}

fn connect(e: &Env, p: &ConnectionProfile, password: &str) -> Result<Arc<Connection>, String> {
    e.core.block_on(e.core.connect(p.clone(), Some(password.to_string()))).map_err(|x| format!("{x:#}"))
}

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    let rs = s.execute(sql);
    rs.fetch_all();
    let ph = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
    if ph != Phase::Done {
        return Err(rs.status().error.map(|x| x.to_string()).unwrap_or_else(|| format!("{ph:?}")));
    }
    let snap = rs.snapshot();
    Ok((0..snap.rows()).map(|r| (0..snap.columns.len()).map(|c| snap.text(r, c).map(|t| t.into_owned())).collect()).collect())
}

fn ok(e: &Env, s: &Arc<Session>, sql: &str) -> Vec<Vec<Option<String>>> {
    run(e, s, sql).unwrap_or_else(|x| panic!("{sql}: {x}"))
}

/// Refused by the server for want of a privilege (or, where the engine hides it, not found).
fn denied(e: &Env, s: &Arc<Session>, sql: &str) -> String {
    match run(e, s, sql) {
        Ok(r) => panic!("{sql} should be refused by the database, it returned {r:?}"),
        Err(x) => {
            let l = x.to_ascii_lowercase();
            let why = ["denied", "permission", "must be owner", "42501", "invalid object name", "invalid column name", "unknown column", "doesn't exist", "does not exist"];
            assert!(why.iter().any(|w| l.contains(w)), "{sql}: refused for another reason: {x}");
            x
        }
    }
}

fn first(r: &[Vec<Option<String>>]) -> String {
    r.first().and_then(|x| x.first().cloned().flatten()).unwrap_or_default()
}

fn opts() -> Options {
    Options { rotate_days: Some(90), conn_limit: Some(4), verify: true }
}

fn guard(p: &mut ConnectionProfile) {
    p.mcp = Some(McpAccess::Write);
    let r = &mut p.mcp_rules;
    r.view = AgentView::Masked;
    r.visible.items = vec![VisibleItem { name: "audit_log".into(), kind: ItemKind::Table }, VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column }];
    r.sensitive = vec![SensitiveCol { col: "users.email".into(), how: Mask::Partial, on: true }];
    r.structure = false;
    r.code = false;
    r.time_limit_s = Some(30);
}

/// Creates the login as the review would, keeps its password, and returns the record.
fn create(e: &Env, admin: &Arc<Connection>, p: &ConnectionProfile) -> (AgentLogin, String, live::Review) {
    let rv = e.core.block_on(live::review(admin, p, &opts())).unwrap();
    assert!(rv.plan.lacks.is_empty(), "{:?}", rv.plan.lacks);
    assert!(rv.probe.can_create, "{:?}", rv.probe);
    assert!(!rv.plan.shown().contains(&rv.password) && !rv.plan.copy().contains(&rv.password) || p.engine == Engine::MongoDb, "the password stays out of the script");
    e.core.block_on(live::apply(admin, &rv.plan, &p.database)).unwrap_or_else(|x| panic!("{x:#}\n{}", rv.plan.shown()));
    e.core.secrets.set(&agent_login::secret_account(&p.id), &rv.password).unwrap();
    (live::record(p, &rv.plan, &opts()), rv.password.clone(), rv)
}

fn as_agent(e: &Env, p: &ConnectionProfile, user: &str, password: &str) -> Result<Arc<Connection>, String> {
    connect(e, &live::agent_profile(p, user), password)
}

fn verify_all(e: &Env, agent: &Arc<Connection>, rv: &live::Review) {
    let checks = e.core.block_on(live::verify(agent, &rv.spec, &rv.inventory, &rv.probe)).unwrap();
    assert!(checks.len() >= 4, "{checks:?}");
    let bad: Vec<_> = checks.iter().filter(|c| !c.ok).collect();
    assert!(bad.is_empty(), "verify: {bad:?}");
}

fn drift_apply(e: &Env, admin: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin, password: &str) -> live::Drift {
    let d = e.core.block_on(live::drift(admin, p, login, password)).unwrap();
    assert!(!d.changes.is_empty(), "the rules changed: the login should drift");
    e.core.block_on(live::apply(admin, &d.plan, &login.database)).unwrap_or_else(|x| panic!("{x:#}\n{}", d.plan.shown()));
    let again = e.core.block_on(live::drift(admin, p, login, password)).unwrap();
    assert!(again.changes.is_empty(), "in sync after the sync: {:?}", again.changes);
    d
}

fn rotate(e: &Env, admin: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin, old: &str) -> String {
    let new = agent_login::hash::password();
    let plan = e.core.block_on(live::rotation(admin, p, login, old, &new)).unwrap();
    assert!(!plan.shown().contains(&new));
    e.core.block_on(live::apply(admin, &plan, &login.database)).unwrap();
    e.core.secrets.set(&agent_login::secret_account(&p.id), &new).unwrap();
    let r = as_agent(e, p, &login.user, &new);
    assert!(r.is_ok(), "the new password logs in: {:?}", r.err());
    drop(r);
    if p.engine != Engine::Redis {
        assert!(as_agent(e, p, &login.user, old).is_err(), "the old password no longer does");
    }
    new
}

fn remove(e: &Env, admin: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin, password: &str) {
    let plan = e.core.block_on(live::removal(admin, p, login)).unwrap();
    e.core.block_on(live::apply(admin, &plan, &login.database)).unwrap_or_else(|x| panic!("{x:#}\n{}", plan.shown()));
    e.core.secrets.delete(&agent_login::secret_account(&p.id)).unwrap();
    assert_eq!(e.core.block_on(live::exists(admin, p, &login.user)).unwrap(), None, "gone from the server");
    assert!(e.core.secrets.get(&agent_login::secret_account(&p.id)).unwrap().is_none());
    assert!(as_agent(e, p, &login.user, password).is_err(), "its password no longer logs in");
}

// ---- PostgreSQL ----

#[test]
fn postgres() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::Postgres, "aupgtest01");
    if !up(&p) {
        eprintln!("skipping: no PostgreSQL at {}:{}", p.host, p.port);
        return;
    }
    let user = agent_login::login_name(&p.id);
    let space = agent_login::space_of(&user);
    let root = connect(&e, &ConnectionProfile { database: "postgres".into(), ..p.clone() }, &apw).unwrap();
    let rs = root.session(None);
    for sql in [
        "DROP DATABASE IF EXISTS au_shop WITH (FORCE)".to_string(),
        format!("DROP ROLE IF EXISTS {user}"),
        "DROP ROLE IF EXISTS dataman_agent_decoy".into(),
        "DROP ROLE IF EXISTS dataman_agent_auclash1".into(),
        "CREATE DATABASE au_shop".into(),
        "CREATE ROLE dataman_agent_decoy LOGIN PASSWORD 'decoy'".into(),
        "CREATE ROLE dataman_agent_auclash1 LOGIN PASSWORD 'clash'".into(),
    ] {
        ok(&e, &rs, &sql);
    }
    let a = connect(&e, &p, &apw).unwrap();
    let s = a.session(None);
    for sql in [
        "CREATE TABLE orders (id serial PRIMARY KEY, total numeric, customer_id int)",
        "CREATE TABLE users (id serial PRIMARY KEY, name text, email text, password_hash text, created_at timestamptz DEFAULT now())",
        "CREATE TABLE audit_log (id serial PRIMARY KEY, what text)",
        "INSERT INTO orders (total, customer_id) VALUES (10, 1), (20, 2)",
        "INSERT INTO users (name, email, password_hash) VALUES ('Alice', 'alice@acme.com', 'h1'), ('Bob', 'bob@acme.com', 'h2')",
        "INSERT INTO audit_log (what) VALUES ('secret')",
        "CREATE SCHEMA other",
        "CREATE VIEW other.v AS SELECT 1 AS x",
    ] {
        ok(&e, &s, sql);
    }

    // A role named as Dataman's but not marked: refused, never touched.
    let mut clash = p.clone();
    clash.id = "auclash1xx".into();
    let rv = e.core.block_on(live::review(&a, &clash, &opts())).unwrap();
    assert_eq!(rv.probe.exists, Some(false));
    assert!(rv.plan.lacks.iter().any(|l| l.contains("isn't Dataman's")), "{:?}", rv.plan.lacks);

    guard(&mut p);
    let (login, password, rv) = create(&e, &a, &p);
    assert!(rv.plan.one_tx && rv.plan.views.len() == 1);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let g = agent.session(None);
    assert_eq!(first(&ok(&e, &g, "SELECT current_user")), user);
    ok(&e, &g, "SELECT id, name, created_at FROM public.users");
    denied(&e, &g, "SELECT password_hash FROM public.users");
    denied(&e, &g, "SELECT email FROM public.users");
    denied(&e, &g, "SELECT * FROM public.users");
    let masked = ok(&e, &g, &format!("SELECT email FROM {space}.users ORDER BY id"));
    assert_eq!(first(&masked), "a•••@acme.com");
    denied(&e, &g, &format!("SELECT * FROM {space}.salt"));
    denied(&e, &g, "SELECT * FROM public.audit_log");
    ok(&e, &g, "INSERT INTO public.orders (total, customer_id) VALUES (1, 1)");
    ok(&e, &g, "UPDATE public.users SET name = name WHERE id = 1");
    denied(&e, &g, "UPDATE public.users SET email = 'x' WHERE id = 1");
    denied(&e, &g, "TRUNCATE public.orders");
    denied(&e, &g, "DROP TABLE public.orders");
    denied(&e, &g, "CREATE TABLE public.mine (i int)");
    denied(&e, &g, "ALTER TABLE public.orders ADD COLUMN x int");
    denied(&e, &g, "GRANT SELECT ON public.audit_log TO PUBLIC");
    // Activity is off while data is kept: others' statements are hidden by the server.
    let hidden = ok(&e, &g, "SELECT count(*) FROM pg_stat_activity WHERE query = '<insufficient privilege>'");
    assert!(first(&hidden).parse::<i64>().unwrap() >= 1, "the admin's session is there, its statement hidden");
    assert_eq!(first(&ok(&e, &g, "SHOW statement_timeout")), "30s");
    assert_eq!(first(&ok(&e, &s, &format!("SELECT rolconnlimit FROM pg_roles WHERE rolname = '{user}'"))), "4");

    // The rules change: orders hidden, users.name masked whole, read only.
    drop(g);
    drop(agent);
    let mut q = p.clone();
    q.mcp = Some(McpAccess::Read);
    q.mcp_rules.visible.items.push(VisibleItem { name: "orders".into(), kind: ItemKind::Table });
    q.mcp_rules.sensitive.push(SensitiveCol { col: "users.name".into(), how: Mask::Hidden, on: true });
    assert_ne!(agent_login::rules_hash(&q, ""), login.rules, "drift is seen without the server");
    let d = drift_apply(&e, &a, &q, &login, &password);
    assert!(d.changes.iter().any(|c| !c.add && c.line.contains("orders")), "{:?}", d.changes);
    let agent = as_agent(&e, &q, &user, &password).unwrap();
    let g = agent.session(None);
    denied(&e, &g, "SELECT 1 FROM public.orders");
    assert_eq!(first(&ok(&e, &g, "SHOW default_transaction_read_only")), "on");
    // That default is the agent's to change; the grants are not.
    ok(&e, &g, "SET default_transaction_read_only = off");
    denied(&e, &g, "INSERT INTO public.users (name) VALUES ('x')");
    assert_eq!(first(&ok(&e, &g, &format!("SELECT name FROM {space}.users ORDER BY id"))), "‹hidden›");

    let password = rotate(&e, &a, &q, &login, &password);
    drop(agent);
    remove(&e, &a, &q, &login, &password);
    assert_eq!(first(&ok(&e, &s, "SELECT count(*) FROM pg_roles WHERE rolname IN ('dataman_agent_decoy', 'dataman_agent_auclash1')")), "2", "what Dataman didn't make stays");
    assert_eq!(first(&ok(&e, &s, &format!("SELECT count(*) FROM pg_namespace WHERE nspname = '{space}'"))), "0");
    assert_eq!(first(&ok(&e, &s, "SELECT count(*) FROM pg_views WHERE schemaname = 'other'")), "1");
}

/// The common case on a managed server: an admin with CREATEROLE, not a superuser, who owns
/// some tables and not others (PostgreSQL 16+: it holds ADMIN on the roles it creates only).
#[test]
fn postgres_createrole_admin() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::Postgres, "aupgadmin1");
    if !up(&p) {
        return;
    }
    let user = agent_login::login_name(&p.id);
    let root = connect(&e, &ConnectionProfile { database: "postgres".into(), ..p.clone() }, &apw).unwrap();
    let rs = root.session(None);
    for sql in [
        "DROP DATABASE IF EXISTS au_shop2 WITH (FORCE)".to_string(),
        format!("DROP ROLE IF EXISTS {user}"),
        "DROP ROLE IF EXISTS au_admin".into(),
        "CREATE ROLE au_admin LOGIN CREATEROLE PASSWORD 'au-admin-pw'".into(),
        "CREATE DATABASE au_shop2 OWNER au_admin".into(),
    ] {
        ok(&e, &rs, &sql);
    }
    let su = connect(&e, &ConnectionProfile { database: "au_shop2".into(), ..p.clone() }, &apw).unwrap();
    ok(&e, &su.session(None), "CREATE TABLE public.theirs (id int, secret text)");
    p.user = "au_admin".into();
    p.database = "au_shop2".into();
    let a = connect(&e, &p, "au-admin-pw").unwrap();
    let s = a.session(None);
    ok(&e, &s, "CREATE TABLE public.mine (id int PRIMARY KEY, name text)");
    ok(&e, &s, "INSERT INTO public.mine VALUES (1, 'a')");
    p.mcp = Some(McpAccess::Write);
    let rv = e.core.block_on(live::review(&a, &p, &opts())).unwrap();
    assert!(rv.probe.can_create && !rv.probe.flavor.superuser, "{:?}", rv.probe);
    assert!(rv.plan.not_granted.iter().any(|n| n.object == "public.theirs" && n.reason.contains("postgres")), "{:?}", rv.plan.not_granted);
    assert!(!rv.plan.shown().contains("pg_read_all_stats\" TO"), "it can't grant the predefined role");
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let g = agent.session(None);
    ok(&e, &g, "SELECT name FROM public.mine");
    denied(&e, &g, "SELECT * FROM public.theirs");
    drop(g);
    drop(agent);
    // Remove without DROP OWNED (a CREATEROLE admin lacks the role's privileges): its grants
    // revoked one by one, then the role.
    let plan = e.core.block_on(live::removal(&a, &p, &login)).unwrap();
    assert!(!plan.shown().contains("DROP OWNED") && plan.shown().contains("REVOKE"), "{}", plan.shown());
    remove(&e, &a, &p, &login, &password);
    drop((s, a, su));
    ok(&e, &rs, "DROP DATABASE IF EXISTS au_shop2 WITH (FORCE)");
    ok(&e, &rs, "DROP ROLE IF EXISTS au_admin");
}

// ---- MySQL ----

#[test]
fn mysql() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::MySql, "aumytest01");
    if !up(&p) {
        eprintln!("skipping: no MySQL at {}:{}", p.host, p.port);
        return;
    }
    let user = agent_login::login_name(&p.id);
    let space = agent_login::space_of(&user);
    let root = connect(&e, &ConnectionProfile { database: String::new(), ..p.clone() }, &apw).unwrap();
    let rs = root.session(None);
    for sql in [
        "DROP DATABASE IF EXISTS au_shop".to_string(),
        "DROP DATABASE IF EXISTS au_other".into(),
        format!("DROP DATABASE IF EXISTS {space}"),
        format!("DROP USER IF EXISTS '{user}'@'%'"),
        "DROP USER IF EXISTS 'dataman_agent_decoy'@'%'".into(),
        "CREATE DATABASE au_shop".into(),
        "CREATE DATABASE au_other".into(),
        "CREATE USER 'dataman_agent_decoy'@'%' IDENTIFIED BY 'decoy-pw-1'".into(),
        "CREATE TABLE au_shop.orders (id INT AUTO_INCREMENT PRIMARY KEY, total DECIMAL(10,2), customer_id INT)".into(),
        "CREATE TABLE au_shop.users (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50), email VARCHAR(100), password_hash VARCHAR(100), created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)".into(),
        "CREATE TABLE au_shop.audit_log (id INT AUTO_INCREMENT PRIMARY KEY, what VARCHAR(50))".into(),
        "INSERT INTO au_shop.orders (total, customer_id) VALUES (10, 1), (20, 2)".into(),
        "INSERT INTO au_shop.users (name, email, password_hash) VALUES ('Alice', 'alice@acme.com', 'h1'), ('Bob', 'bob@acme.com', 'h2')".into(),
        "INSERT INTO au_shop.audit_log (what) VALUES ('secret')".into(),
        "CREATE VIEW au_other.v AS SELECT 1 AS x".into(),
        "CREATE PROCEDURE au_shop.p() SELECT 1".into(),
    ] {
        ok(&e, &rs, &sql);
    }
    let a = connect(&e, &p, &apw).unwrap();
    let s = a.session(None);
    guard(&mut p);
    // Only the shop's database is the agents': the other one is hidden.
    p.mcp_rules.visible.items.push(VisibleItem { name: "au_other.*".into(), kind: ItemKind::Schema });
    let (login, password, rv) = create(&e, &a, &p);
    assert!(rv.plan.shown().contains("ACCOUNT UNLOCK"));
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let g = agent.session(None);
    assert!(first(&ok(&e, &g, "SELECT CURRENT_USER()")).starts_with(&format!("{user}@")));
    ok(&e, &g, "SELECT id, name FROM au_shop.users");
    denied(&e, &g, "SELECT password_hash FROM au_shop.users");
    denied(&e, &g, "SELECT email FROM au_shop.users");
    assert_eq!(first(&ok(&e, &g, &format!("SELECT email FROM {space}.au_shop__users ORDER BY id"))), "a•••@acme.com");
    denied(&e, &g, &format!("SELECT * FROM {space}.salt"));
    denied(&e, &g, "SELECT * FROM au_shop.audit_log");
    denied(&e, &g, "SELECT * FROM au_other.v");
    // MySQL hides what it may not use.
    let names = ok(&e, &g, "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'au_shop' ORDER BY 1");
    let names: Vec<String> = names.iter().map(|r| r[0].clone().unwrap_or_default()).collect();
    assert!(!names.contains(&"audit_log".to_string()), "{names:?}");
    ok(&e, &g, "INSERT INTO au_shop.orders (total, customer_id) VALUES (1, 1)");
    denied(&e, &g, "UPDATE au_shop.users SET email = 'x' WHERE id = 1");
    denied(&e, &g, "TRUNCATE au_shop.orders");
    denied(&e, &g, "DROP TABLE au_shop.orders");
    denied(&e, &g, "CREATE TABLE au_shop.mine (i INT)");
    denied(&e, &g, "CALL au_shop.p()");
    denied(&e, &g, "CREATE PROCEDURE au_shop.q() SELECT 1");
    // No PROCESS: the server shows it its own threads only.
    assert_eq!(first(&ok(&e, &g, "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER <> SUBSTRING_INDEX(CURRENT_USER(), '@', 1)")), "0");

    drop(g);
    drop(agent);
    let mut q = p.clone();
    q.mcp = Some(McpAccess::Read);
    q.mcp_rules.visible.items.push(VisibleItem { name: "orders".into(), kind: ItemKind::Table });
    q.mcp_rules.sensitive.push(SensitiveCol { col: "users.name".into(), how: Mask::Hidden, on: true });
    drift_apply(&e, &a, &q, &login, &password);
    let agent = as_agent(&e, &q, &user, &password).unwrap();
    let g = agent.session(None);
    denied(&e, &g, "SELECT 1 FROM au_shop.orders");
    denied(&e, &g, "INSERT INTO au_shop.users (name) VALUES ('x')");
    assert_eq!(first(&ok(&e, &g, &format!("SELECT name FROM {space}.au_shop__users ORDER BY id"))), "‹hidden›");

    let password = rotate(&e, &a, &q, &login, &password);
    drop(agent);
    remove(&e, &a, &q, &login, &password);
    assert_eq!(first(&ok(&e, &s, "SELECT COUNT(*) FROM mysql.user WHERE User = 'dataman_agent_decoy'")), "1");
    assert_eq!(first(&ok(&e, &s, &format!("SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{space}'"))), "0");
    assert_eq!(first(&ok(&e, &s, "SELECT COUNT(*) FROM information_schema.VIEWS WHERE TABLE_SCHEMA = 'au_other'")), "1");
}

// ---- SQL Server ----

#[test]
fn sql_server() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::SqlServer, "aumstest01");
    if !up(&p) {
        eprintln!("skipping: no SQL Server at {}:{}", p.host, p.port);
        return;
    }
    let user = agent_login::login_name(&p.id);
    let space = agent_login::space_of(&user);
    let root = connect(&e, &ConnectionProfile { database: "master".into(), ..p.clone() }, &apw).unwrap();
    let rs = root.session(None);
    for sql in [
        "IF DB_ID('au_shop') IS NOT NULL BEGIN ALTER DATABASE au_shop SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE au_shop; END".to_string(),
        format!("IF SUSER_ID('{user}') IS NOT NULL DROP LOGIN [{user}]"),
        "IF SUSER_ID('dataman_agent_decoy') IS NOT NULL DROP LOGIN [dataman_agent_decoy]".into(),
        "CREATE DATABASE au_shop".into(),
        "CREATE LOGIN [dataman_agent_decoy] WITH PASSWORD = 'Decoy-pw-123', CHECK_POLICY = OFF".into(),
    ] {
        ok(&e, &rs, &sql);
    }
    let a = connect(&e, &p, &apw).unwrap();
    let s = a.session(None);
    for sql in [
        "CREATE TABLE dbo.orders (id INT IDENTITY PRIMARY KEY, total DECIMAL(10,2), customer_id INT)",
        "CREATE TABLE dbo.users (id INT IDENTITY PRIMARY KEY, name NVARCHAR(50), email NVARCHAR(100), password_hash NVARCHAR(100), created_at DATETIME2 DEFAULT SYSDATETIME())",
        "CREATE TABLE dbo.audit_log (id INT IDENTITY PRIMARY KEY, what NVARCHAR(50))",
        "INSERT INTO dbo.orders (total, customer_id) VALUES (10, 1), (20, 2)",
        "INSERT INTO dbo.users (name, email, password_hash) VALUES (N'Alice', N'alice@acme.com', N'h1'), (N'Bob', N'bob@acme.com', N'h2')",
        "INSERT INTO dbo.audit_log (what) VALUES (N'secret')",
        "CREATE USER [dataman_agent_decoy] FOR LOGIN [dataman_agent_decoy]",
    ] {
        ok(&e, &s, sql);
    }
    ok(&e, &s, "CREATE PROCEDURE dbo.p AS SELECT 1");
    guard(&mut p);
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let g = agent.session(None);
    assert_eq!(first(&ok(&e, &g, "SELECT SUSER_SNAME()")), user);
    assert_eq!(first(&ok(&e, &g, "SELECT USER_NAME()")), user);
    ok(&e, &g, "SELECT id, total FROM dbo.orders");
    // Write tier: the visible columns of the table itself, for its writes' WHERE.
    ok(&e, &g, "SELECT id, name FROM dbo.users");
    denied(&e, &g, "SELECT email FROM dbo.users");
    denied(&e, &g, "SELECT password_hash FROM dbo.users");
    assert_eq!(first(&ok(&e, &g, &format!("SELECT email FROM [{space}].[users] ORDER BY id"))), "a•••@acme.com");
    denied(&e, &g, &format!("SELECT password_hash FROM [{space}].[users]"));
    denied(&e, &g, &format!("SELECT * FROM [{space}].[salt]"));
    denied(&e, &g, "SELECT * FROM dbo.audit_log");
    let names = ok(&e, &g, "SELECT name FROM sys.tables ORDER BY name");
    assert!(!names.iter().any(|r| r[0].as_deref() == Some("audit_log")), "SQL Server hides what it may not use: {names:?}");
    ok(&e, &g, "INSERT INTO dbo.orders (total, customer_id) VALUES (1, 1)");
    ok(&e, &g, "UPDATE dbo.users SET name = name WHERE id = 1");
    denied(&e, &g, "UPDATE dbo.users SET email = N'x' WHERE id = 1");
    denied(&e, &g, "TRUNCATE TABLE dbo.orders");
    denied(&e, &g, "DROP TABLE dbo.orders");
    denied(&e, &g, "CREATE TABLE dbo.mine (i INT)");
    denied(&e, &g, "EXEC dbo.p");
    // No VIEW SERVER STATE: the server shows it its own session only (the admin's is open).
    assert!(ok(&e, &g, "SELECT session_id FROM sys.dm_exec_sessions WHERE session_id <> @@SPID AND is_user_process = 1").is_empty());

    drop(g);
    drop(agent);
    let mut q = p.clone();
    q.mcp = Some(McpAccess::Read);
    q.mcp_rules.visible.items.push(VisibleItem { name: "orders".into(), kind: ItemKind::Table });
    q.mcp_rules.sensitive.push(SensitiveCol { col: "users.name".into(), how: Mask::Hidden, on: true });
    drift_apply(&e, &a, &q, &login, &password);
    let agent = as_agent(&e, &q, &user, &password).unwrap();
    let g = agent.session(None);
    denied(&e, &g, "SELECT 1 FROM dbo.orders");
    denied(&e, &g, "SELECT name FROM dbo.users");
    denied(&e, &g, "INSERT INTO dbo.users (name) VALUES (N'x')");
    assert_eq!(first(&ok(&e, &g, &format!("SELECT name FROM [{space}].[users] ORDER BY id"))), "‹hidden›");

    let password = rotate(&e, &a, &q, &login, &password);
    drop(agent);
    remove(&e, &a, &q, &login, &password);
    assert_eq!(first(&ok(&e, &s, "SELECT COUNT(*) FROM sys.server_principals WHERE name = 'dataman_agent_decoy'")), "1");
    assert_eq!(first(&ok(&e, &s, "SELECT COUNT(*) FROM sys.database_principals WHERE name = 'dataman_agent_decoy'")), "1");
    assert_eq!(first(&ok(&e, &s, &format!("SELECT COUNT(*) FROM sys.schemas WHERE name = '{space}'"))), "0");
}

// ---- MongoDB ----

fn nosql(e: &Env, c: &Arc<Connection>, db: &str, text: &str) -> Result<Output, String> {
    e.core.block_on(c.run_nosql(db, text, Page { skip: 0, limit: 100 })).map(|r| r.output).map_err(|x| format!("{x:#}"))
}

fn docs(o: &Output) -> Vec<serde_json::Value> {
    match o {
        Output::Docs(d) => d.docs.iter().map(|x| x.body.to_json()).collect(),
        Output::Value(v) => vec![v.to_json()],
        _ => Vec::new(),
    }
}

fn mongo_denied(e: &Env, c: &Arc<Connection>, db: &str, text: &str) {
    match nosql(e, c, db, text) {
        Ok(o) => panic!("{text} should be refused by MongoDB: {o:?}"),
        Err(x) => assert!(x.contains("not authorized") || x.contains("Unauthorized") || x.contains("requires authentication"), "{text}: {x}"),
    }
}

#[test]
fn mongodb() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::MongoDb, "aumgtest01");
    if !up(&p) {
        eprintln!("skipping: no MongoDB at {}:{}", p.host, p.port);
        return;
    }
    let user = agent_login::login_name(&p.id);
    let space = agent_login::space_of(&user);
    let a = connect(&e, &p, &apw).unwrap();
    for t in [
        "db.dropDatabase()".to_string(),
        format!("db.getSiblingDB('admin').runCommand({{dropUser: '{user}'}})"),
        format!("db.getSiblingDB('admin').runCommand({{dropRole: '{user}'}})"),
        "db.getSiblingDB('admin').runCommand({dropUser: 'dataman_agent_decoy'})".into(),
    ] {
        let _ = nosql(&e, &a, "au_shop", &t);
    }
    for t in [
        "db.orders.insertMany([{total: 10, customer_id: 1}, {total: 20, customer_id: 2}])",
        "db.users.insertMany([{name: 'Alice', email: 'alice@acme.com', password_hash: 'h1'}, {name: 'Bob', email: 'bob@acme.com', password_hash: 'h2'}])",
        "db.audit_log.insertOne({what: 'secret'})",
        "db.getSiblingDB('admin').runCommand({createUser: 'dataman_agent_decoy', pwd: 'decoy', roles: [{role: 'read', db: 'au_shop'}]})",
    ] {
        nosql(&e, &a, "au_shop", t).unwrap_or_else(|x| panic!("{t}: {x}"));
    }
    guard(&mut p);
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let who = docs(&nosql(&e, &agent, "admin", "db.runCommand({connectionStatus: 1})").unwrap());
    assert_eq!(who[0]["authInfo"]["authenticatedUsers"][0]["user"], user.as_str());
    assert_eq!(docs(&nosql(&e, &agent, "au_shop", "db.orders.find({})").unwrap()).len(), 2);
    let view = format!("db.getCollection('{space}.users').find({{}})");
    let u = docs(&nosql(&e, &agent, "au_shop", &view).unwrap());
    assert!(u[0].get("password_hash").is_none(), "hidden field left out by the view: {u:?}");
    assert_eq!(u[0]["email"], "a•••@acme.com");
    // The base collection: no find (the raw driver, Dataman's own view mapping not in the way).
    agent.set_agent_views(&[]);
    mongo_denied(&e, &agent, "au_shop", "db.users.find({})");
    mongo_denied(&e, &agent, "au_shop", "db.audit_log.find({})");
    mongo_denied(&e, &agent, "au_shop", "db.orders.drop()");
    mongo_denied(&e, &agent, "au_shop", "db.createCollection('mine')");
    mongo_denied(&e, &agent, "au_shop", "db.orders.createIndex({total: 1})");
    mongo_denied(&e, &agent, "admin", "db.runCommand({currentOp: 1, $all: true})");
    let names = docs(&nosql(&e, &agent, "au_shop", "db.runCommand({listCollections: 1, nameOnly: true, authorizedCollections: true})").unwrap());
    let text = names[0].to_string();
    assert!(!text.contains("audit_log"), "listCollections shows what it may use only: {text}");
    nosql(&e, &agent, "au_shop", "db.orders.insertOne({total: 1})").unwrap();
    // Dataman's own reads go to the view.
    agent.set_agent_views(&login.views);
    let u = docs(&nosql(&e, &agent, "au_shop", "db.users.find({})").unwrap());
    assert_eq!(u[0]["email"], "a•••@acme.com");

    drop(agent);
    let mut q = p.clone();
    q.mcp = Some(McpAccess::Read);
    q.mcp_rules.visible.items.push(VisibleItem { name: "orders".into(), kind: ItemKind::Table });
    drift_apply(&e, &a, &q, &login, &password);
    let agent = as_agent(&e, &q, &user, &password).unwrap();
    mongo_denied(&e, &agent, "au_shop", "db.orders.find({})");
    mongo_denied(&e, &agent, "au_shop", "db.users.insertOne({name: 'x'})");

    let password = rotate(&e, &a, &q, &login, &password);
    drop(agent);
    remove(&e, &a, &q, &login, &password);
    let decoy = docs(&nosql(&e, &a, "admin", "db.runCommand({usersInfo: 'dataman_agent_decoy'})").unwrap());
    assert_eq!(decoy[0]["users"].as_array().map(|x| x.len()), Some(1), "what Dataman didn't make stays");
    let views = docs(&nosql(&e, &a, "au_shop", "db.runCommand({listCollections: 1, nameOnly: true})").unwrap())[0].to_string();
    assert!(!views.contains(&format!("{space}.users")), "its view is gone: {views}");
}

// ---- Redis ----

#[test]
fn redis() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::Redis, "aurdtest01");
    if !up(&p) {
        eprintln!("skipping: no Redis at {}:{}", p.host, p.port);
        return;
    }
    let user = agent_login::login_name(&p.id);
    let a = connect(&e, &p, &apw).unwrap();
    for t in [format!("ACL DELUSER {user}"), "ACL DELUSER dataman_agent_decoy".into(), "ACL SETUSER dataman_agent_decoy on >decoy ~* +@read".into(), "SET cache:1 one".into(), "SET secret:1 two".into(), "HSET user:1 name Alice".into()] {
        nosql(&e, &a, "0", &t).unwrap_or_else(|x| panic!("{t}: {x}"));
    }
    p.mcp = Some(McpAccess::Read);
    p.mcp_rules.activity = false;
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let who = docs(&nosql(&e, &agent, "0", "ACL WHOAMI").unwrap());
    assert_eq!(who[0], user.as_str());
    nosql(&e, &agent, "0", "GET cache:1").unwrap();
    nosql(&e, &agent, "0", "INFO server").unwrap();
    for t in ["SET cache:1 x", "DEL cache:1", "FLUSHALL", "FLUSHDB", "CONFIG GET requirepass", "KEYS *", "EVAL \"return 1\" 0", "LASTSAVE", "SWAPDB 0 1", "ACL LIST", "CLIENT LIST"] {
        let r = nosql(&e, &agent, "0", t);
        assert!(r.as_ref().is_err_and(|x| x.contains("NOPERM")), "{t} should be refused by Redis: {r:?}");
    }
    let left = docs(&nosql(&e, &a, "0", "GET cache:1").unwrap()).iter().map(|v| v.to_string()).collect::<String>();
    assert!(left.contains("\"one\""), "nothing was wiped: {left}");
    // What Dataman's client won't even send, asked of the server's ACL itself.
    for t in ["SHUTDOWN", "MONITOR", "REPLICAOF NO ONE", "MODULE LIST"] {
        let r = nosql(&e, &a, "0", &format!("ACL DRYRUN {user} {t}"));
        let text = format!("{r:?}");
        assert!(text.contains("no permissions"), "{t}: {text}");
    }

    // Only these keys: the ACL holds them.
    let mut q = p.clone();
    q.mcp_rules.visible = dataman_core::privacy::Visible { mode: dataman_core::privacy::VisibleMode::Only, items: vec![VisibleItem { name: "cache:*".into(), kind: ItemKind::Table }] };
    drift_apply(&e, &a, &q, &login, &password);
    let agent = as_agent(&e, &q, &user, &password).unwrap();
    nosql(&e, &agent, "0", "GET cache:1").unwrap();
    let r = nosql(&e, &agent, "0", "GET secret:1");
    assert!(r.as_ref().is_err_and(|x| x.contains("NOPERM")), "{r:?}");

    let password = rotate(&e, &a, &q, &login, &password);
    drop(agent);
    remove(&e, &a, &q, &login, &password);
    assert_eq!(e.core.block_on(live::exists(&a, &q, "dataman_agent_decoy")).unwrap(), Some(false), "what Dataman didn't make stays");
}

// ---- Elasticsearch, OpenSearch ----

fn search_denied(r: Result<Output, String>, what: &str) {
    match r {
        Ok(o) => panic!("{what} should be refused: {o:?}"),
        Err(x) => assert!(x.contains("403") || x.contains("security_exception") || x.contains("unauthorized") || x.contains("no permissions"), "{what}: {x}"),
    }
}

fn search_engine(engine: Engine, id: &str) {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(engine, id);
    if !up(&p) {
        eprintln!("skipping: no {} at {}:{}", engine.label(), p.host, p.port);
        return;
    }
    let os = engine == Engine::OpenSearch;
    let user = agent_login::login_name(&p.id);
    let a = connect(&e, &p, &apw).unwrap();
    let (ub, rb) = if os { ("/_plugins/_security/api/internalusers", "/_plugins/_security/api/roles") } else { ("/_security/user", "/_security/role") };
    for t in [format!("DELETE {ub}/{user}"), format!("DELETE {rb}/{user}"), format!("DELETE {ub}/dataman_agent_decoy"), "DELETE /au_orders".into(), "DELETE /au_users".into(), "DELETE /au_audit".into()] {
        let _ = nosql(&e, &a, "", &t);
    }
    let decoy = if os { format!("PUT {ub}/dataman_agent_decoy\n{{\"password\": \"Decoy-pw-123!x\"}}") } else { format!("PUT {ub}/dataman_agent_decoy\n{{\"password\": \"decoy-pw\", \"roles\": []}}") };
    for t in [
        "PUT /au_orders/_doc/1?refresh=true\n{\"total\": 10}".to_string(),
        "PUT /au_users/_doc/1?refresh=true\n{\"name\": \"Alice\", \"email\": \"alice@acme.com\", \"password_hash\": \"h1\"}".into(),
        "PUT /au_audit/_doc/1?refresh=true\n{\"what\": \"secret\"}".into(),
        decoy,
    ] {
        nosql(&e, &a, "", &t).unwrap_or_else(|x| panic!("{t}: {x}"));
    }
    guard(&mut p);
    // Only this test's indices (the server may hold others); au_audit is hidden.
    p.mcp_rules.visible = dataman_core::privacy::Visible {
        mode: dataman_core::privacy::VisibleMode::Only,
        items: vec![
            VisibleItem { name: "au_orders".into(), kind: ItemKind::Table },
            VisibleItem { name: "au_users".into(), kind: ItemKind::Table },
            VisibleItem { name: "au_users.password_hash".into(), kind: ItemKind::Column },
        ],
    };
    p.mcp_rules.sensitive = vec![SensitiveCol { col: "au_users.email".into(), how: Mask::Partial, on: true }];
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let who = docs(&nosql(&e, &agent, "", if os { "GET /_plugins/_security/authinfo" } else { "GET /_security/_authenticate" }).unwrap());
    assert_eq!(who[0][if os { "user_name" } else { "username" }], user.as_str());
    let found = docs(&nosql(&e, &agent, "", "GET /au_users/_search").unwrap());
    let doc = found[0].to_string();
    assert!(!doc.contains("password_hash") && !doc.contains("h1"), "hidden field left out by the server: {doc}");
    if os {
        assert!(!doc.contains("alice@acme.com"), "masked by OpenSearch: {doc}");
    } else {
        assert!(!doc.contains("alice@acme.com"), "Elasticsearch can't mask: the field is left out: {doc}");
    }
    search_denied(nosql(&e, &agent, "", "GET /au_audit/_search"), "the hidden index");
    search_denied(nosql(&e, &agent, "", "PUT /au_mine"), "creating an index");
    search_denied(nosql(&e, &agent, "", "DELETE /au_orders"), "deleting an index");
    search_denied(nosql(&e, &agent, "", "PUT /au_orders/_settings\n{\"index\": {\"number_of_replicas\": 0}}"), "settings");
    nosql(&e, &agent, "", "PUT /au_orders/_doc/2\n{\"total\": 1}").unwrap();

    drop(agent);
    let mut q = p.clone();
    q.mcp = Some(McpAccess::Read);
    drift_apply(&e, &a, &q, &login, &password);
    let agent = as_agent(&e, &q, &user, &password).unwrap();
    search_denied(nosql(&e, &agent, "", "PUT /au_orders/_doc/3\n{\"total\": 1}"), "a write on a read tier");

    let password = rotate(&e, &a, &q, &login, &password);
    drop(agent);
    remove(&e, &a, &q, &login, &password);
    assert!(nosql(&e, &a, "", &format!("GET {ub}/dataman_agent_decoy")).is_ok(), "what Dataman didn't make stays");
    let _ = nosql(&e, &a, "", &format!("DELETE {ub}/dataman_agent_decoy"));
}

#[test]
fn elasticsearch() {
    search_engine(Engine::Elasticsearch, "auestest01");
}

#[test]
fn opensearch() {
    search_engine(Engine::OpenSearch, "auostest01");
}

// ---- Red team: what an agent holding the login's password could try on the server itself ----

/// Ends the login's sessions as Rotate and Remove do.
fn end_sessions(e: &Env, admin: &Arc<Connection>, p: &ConnectionProfile, user: &str) -> usize {
    e.core.block_on(live::end_sessions(admin, p, user)).unwrap_or_else(|x| panic!("{x:#}"))
}

#[test]
fn postgres_redteam() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::Postgres, "aupgrt0001");
    if !up(&p) {
        return;
    }
    p.database = "au_rt".into();
    let user = agent_login::login_name(&p.id);
    let space = agent_login::space_of(&user);
    let root = connect(&e, &ConnectionProfile { database: "postgres".into(), ..p.clone() }, &apw).unwrap();
    let rs = root.session(None);
    for sql in [
        "DROP DATABASE IF EXISTS au_rt WITH (FORCE)".to_string(),
        format!("DROP ROLE IF EXISTS {user}"),
        "DROP ROLE IF EXISTS dataman_agent_lookalike".into(),
        "DROP ROLE IF EXISTS dataman_agent_aupgcoll".into(),
        "CREATE DATABASE au_rt".into(),
        "CREATE ROLE dataman_agent_lookalike LOGIN PASSWORD 'look'".into(),
    ] {
        ok(&e, &rs, &sql);
    }
    let a = connect(&e, &p, &apw).unwrap();
    let s = a.session(None);
    for sql in [
        "CREATE EXTENSION IF NOT EXISTS dblink",
        "CREATE TABLE users (id serial PRIMARY KEY, name text, email text, password_hash text)",
        "INSERT INTO users (name, email, password_hash) VALUES ('Alice', 'alice@acme.com', 'h1'), ('Bob', 'bob@acme.com', 'h2')",
        "CREATE TABLE secrets (id int, s text)",
        "INSERT INTO secrets VALUES (1, 'top-secret')",
        // Hidden, yet PUBLIC's: every login reads it (the review must say so).
        "CREATE TABLE pub_leak (id int, s text)",
        "INSERT INTO pub_leak VALUES (1, 'public-secret')",
        "GRANT SELECT ON pub_leak TO PUBLIC",
        "CREATE TABLE cols_leak (id int, pw text)",
        "INSERT INTO cols_leak VALUES (1, 'col-secret')",
        "GRANT SELECT (pw) ON cols_leak TO PUBLIC",
        // Runs as its owner; PUBLIC may call it.
        "CREATE FUNCTION leak_secrets() RETURNS text SECURITY DEFINER LANGUAGE sql AS 'SELECT string_agg(s, '','') FROM secrets'",
        // Hostile names, and a table named as Dataman's salt.
        r#"CREATE TABLE "we""ird" (id int, "e""mail" text)"#,
        r#"INSERT INTO "we""ird" VALUES (1, 'carol@acme.com')"#,
        r#"CREATE TABLE "x'; DROP ROLE postgres; --" (id int, s text)"#,
        r#"INSERT INTO "x'; DROP ROLE postgres; --" VALUES (1, 'ok')"#,
        r#"CREATE TABLE "ünïcødé_表" (id int, "naïve" text)"#,
        "CREATE TABLE salt (id int, v text)",
        "INSERT INTO salt VALUES (1, 'not-the-salt')",
        "CREATE TABLE t_63_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (id int, email text)",
        "ANALYZE users",
    ] {
        ok(&e, &s, sql);
    }
    let lo = first(&ok(&e, &s, "SELECT lo_from_bytea(0, 'lo-secret')::text"));
    p.mcp = Some(McpAccess::Write);
    let r = &mut p.mcp_rules;
    r.view = AgentView::Masked;
    r.visible.items = ["secrets", "pub_leak"].iter().map(|n| VisibleItem { name: (*n).into(), kind: ItemKind::Table }).chain(["users.password_hash", "cols_leak.pw"].iter().map(|n| VisibleItem { name: (*n).into(), kind: ItemKind::Column })).collect();
    r.sensitive = vec![
        SensitiveCol { col: "users.email".into(), how: Mask::Partial, on: true },
        SensitiveCol { col: "we\"ird.e\"mail".into(), how: Mask::Partial, on: true },
        SensitiveCol { col: "salt.v".into(), how: Mask::Hash, on: true },
        SensitiveCol { col: "*.email".into(), how: Mask::Partial, on: true },
    ];
    r.code = false;

    let rv = e.core.block_on(live::review(&a, &p, &opts())).unwrap();
    let exposed: Vec<&str> = rv.plan.exposed.iter().map(|x| x.object.as_str()).collect();
    assert!(exposed.iter().any(|o| *o == "public.pub_leak") && exposed.iter().any(|o| o.contains("cols_leak.pw")), "what PUBLIC reads is said: {exposed:?}");
    assert!(rv.plan.copy().contains("REVOKE SELECT ON \"public\".\"pub_leak\" FROM PUBLIC;"), "{}", rv.plan.copy());
    assert!(rv.plan.notes.iter().any(|n| n.contains("public.leak_secrets") && n.contains("SECURITY DEFINER")), "{:?}", rv.plan.notes);
    assert!(rv.plan.rules.iter().any(|l| l.rule.contains("readable by every login") && l.who == agent_login::Who::Dataman), "{:?}", rv.plan.rules);
    let names: Vec<&str> = rv.plan.views.iter().map(|v| v.view.as_str()).collect();
    assert!(!names.iter().any(|v| v.ends_with(".\"salt\"")), "no view in the salt's place: {names:?}");
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let g = agent.session(None);
    // Becoming someone else.
    denied(&e, &g, "SET ROLE postgres");
    denied(&e, &g, "SET SESSION AUTHORIZATION postgres");
    denied(&e, &g, "SET ROLE pg_read_all_data");
    // The server's files, programs, large objects, other connections.
    denied(&e, &g, "SELECT pg_read_file('/etc/passwd')");
    denied(&e, &g, "SELECT pg_ls_dir('.')");
    denied(&e, &g, "COPY (SELECT 1) TO PROGRAM 'id'");
    denied(&e, &g, "COPY secrets TO STDOUT");
    denied(&e, &g, "SELECT lo_import('/etc/hosts')");
    denied(&e, &g, &format!("SELECT lo_get({lo})"));
    denied(&e, &g, &format!("SELECT lo_export({lo}, '/tmp/x')"));
    let x = run(&e, &g, "SELECT dblink_connect('dbname=au_rt user=postgres')").unwrap_err();
    assert!(x.contains("password"), "dblink needs a password the agent doesn't have: {x}");
    denied(&e, &g, "SELECT * FROM dblink_connect_u('dbname=au_rt')");
    // Hidden and masked, straight and sideways.
    denied(&e, &g, "SELECT * FROM secrets");
    denied(&e, &g, "SELECT email FROM users");
    denied(&e, &g, "SELECT id FROM users WHERE email LIKE 'a%'");
    denied(&e, &g, "UPDATE users SET name = name WHERE email LIKE 'a%'");
    denied(&e, &g, "DELETE FROM users WHERE password_hash = 'h1'");
    denied(&e, &g, "INSERT INTO users (name) VALUES ('x') RETURNING email");
    assert!(ok(&e, &g, "SELECT most_common_vals FROM pg_stats WHERE tablename = 'users' AND attname IN ('password_hash', 'email')").is_empty(), "statistics of columns it can't read");
    denied(&e, &g, &format!("SELECT * FROM {space}.salt"));
    // A user table named salt: read through its own view, Dataman's salt untouched.
    let v = ok(&e, &g, &format!("SELECT v FROM {} ORDER BY id", login.view_for("public", "salt").unwrap().view));
    assert!(first(&v).starts_with("‹h:"), "{v:?}");
    assert_eq!(first(&ok(&e, &s, "SELECT v FROM public.salt")), "not-the-salt");
    // Hostile names: read through their views, nothing else ran.
    let weird = login.view_for("public", "we\"ird").unwrap().view.clone();
    assert_eq!(first(&ok(&e, &g, &format!("SELECT \"e\"\"mail\" FROM {weird}"))), "c•••@acme.com");
    ok(&e, &g, r#"SELECT s FROM "x'; DROP ROLE postgres; --""#);
    ok(&e, &g, r#"SELECT "naïve" FROM "ünïcødé_表""#);
    assert_eq!(first(&ok(&e, &rs, "SELECT count(*) FROM pg_roles WHERE rolname = 'postgres'")), "1");
    // What PUBLIC holds is the database's gap, said in the review: the login reads it.
    assert_eq!(first(&ok(&e, &g, "SELECT s FROM pub_leak")), "public-secret");
    assert_eq!(first(&ok(&e, &g, "SELECT pw FROM cols_leak")), "col-secret");
    assert_eq!(first(&ok(&e, &g, "SELECT leak_secrets()")), "top-secret", "SECURITY DEFINER, said in the notes");
    // No structure, no grants of its own.
    denied(&e, &g, "CREATE TABLE public.mine (i int)");
    denied(&e, &g, "CREATE SCHEMA mine");
    // PostgreSQL takes a GRANT without the grant option with a warning, granting nothing.
    let _ = run(&e, &g, "GRANT SELECT ON users TO PUBLIC");
    assert_eq!(first(&ok(&e, &s, "SELECT has_table_privilege('public', 'users', 'SELECT')")), "false", "nothing was granted");
    denied(&e, &g, "ALTER TABLE users ADD COLUMN x int");
    denied(&e, &g, &format!("DROP VIEW {weird}"));
    denied(&e, &g, &format!("CREATE OR REPLACE VIEW {weird} AS SELECT 1"));
    denied(&e, &g, &format!("COMMENT ON ROLE {user} IS 'mine now'"));
    denied(&e, &g, &format!("ALTER ROLE {user} CREATEROLE"));
    denied(&e, &g, &format!("ALTER ROLE {user} CONNECTION LIMIT -1"));

    // Drift: a table made since is not granted; Apply grants it, revokes a grant added by
    // hand, and leaves alone what Dataman didn't make.
    for sql in [
        "CREATE TABLE later (id int)".to_string(),
        "INSERT INTO later VALUES (7)".into(),
        format!("GRANT SELECT ON secrets TO {user}"),
        format!("CREATE VIEW {space}.lookalike AS SELECT 1 AS x"),
    ] {
        ok(&e, &s, &sql);
    }
    denied(&e, &g, "SELECT * FROM later");
    assert_eq!(first(&ok(&e, &g, "SELECT s FROM secrets")), "top-secret", "the grant added by hand");
    let d = e.core.block_on(live::drift(&a, &p, &login, &password)).unwrap();
    assert!(d.changes.iter().any(|c| c.add && c.line.contains("later")), "{:?}", d.changes);
    assert!(d.changes.iter().any(|c| !c.add && c.line.contains("secrets")), "{:?}", d.changes);
    assert!(!d.plan.shown().contains("lookalike"), "{}", d.plan.shown());
    e.core.block_on(live::apply(&a, &d.plan, &login.database)).unwrap();
    assert_eq!(first(&ok(&e, &g, "SELECT id FROM later")), "7");
    denied(&e, &g, "SELECT * FROM secrets");
    assert_eq!(first(&ok(&e, &s, &format!("SELECT count(*) FROM pg_views WHERE schemaname = '{space}' AND viewname = 'lookalike'"))), "1", "unmarked: untouched");

    // Two connections whose ids start alike share the login's name: the other one's login is
    // not this one's (before: a prefix of the mark matched).
    let mut twin = p.clone();
    twin.id = "aupgrt0001x".into();
    let rv2 = e.core.block_on(live::review(&a, &twin, &opts())).unwrap();
    assert_eq!(rv2.probe.exists, Some(false), "marked for another connection");
    assert!(e.core.block_on(live::removal(&a, &twin, &login)).is_err(), "Remove from the other connection is refused");

    // Rotation: a session opened with the old password ends too.
    let old_session = as_agent(&e, &p, &user, &password).unwrap();
    let os = old_session.session(None);
    ok(&e, &os, "SELECT 1");
    let new = agent_login::hash::password();
    let plan = e.core.block_on(live::rotation(&a, &p, &login, &password, &new)).unwrap();
    e.core.block_on(live::apply(&a, &plan, &login.database)).unwrap();
    assert!(run(&e, &os, "SELECT id FROM later").is_ok(), "before: a session from the old password goes on working");
    assert!(end_sessions(&e, &a, &p, &user) >= 2, "the old sessions are ended");
    assert!(run(&e, &os, "SELECT id FROM later").is_err(), "ended");
    assert!(run(&e, &g, "SELECT id FROM later").is_err(), "ended");
    assert!(as_agent(&e, &p, &user, &password).is_err(), "the old password no longer logs in");
    let password = new;
    drop((g, agent, os, old_session));

    // Remove: everything Dataman made, nothing else.
    let left = as_agent(&e, &p, &user, &password).unwrap();
    let ls = left.session(None);
    ok(&e, &ls, "SELECT 1");
    end_sessions(&e, &a, &p, &user);
    let plan = e.core.block_on(live::removal(&a, &p, &login)).unwrap();
    e.core.block_on(live::apply(&a, &plan, &login.database)).unwrap_or_else(|x| panic!("{x:#}\n{}", plan.shown()));
    assert!(run(&e, &ls, "SELECT 1").is_err(), "its last session went with it");
    assert_eq!(first(&ok(&e, &rs, &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{user}'"))), "0");
    assert_eq!(first(&ok(&e, &rs, "SELECT count(*) FROM pg_roles WHERE rolname = 'dataman_agent_lookalike'")), "1");
    // Its schema stays: something not Dataman's is in it.
    assert_eq!(first(&ok(&e, &s, &format!("SELECT count(*) FROM pg_views WHERE schemaname = '{space}'"))), "1", "only the look-alike is left");
    assert_eq!(first(&ok(&e, &s, "SELECT count(*) FROM pg_class WHERE relname = 'salt' AND relnamespace = 'public'::regnamespace")), "1");
    drop((ls, left, s, a));
    ok(&e, &rs, "DROP DATABASE IF EXISTS au_rt WITH (FORCE)");
    ok(&e, &rs, "DROP ROLE IF EXISTS dataman_agent_lookalike");
}

#[test]
fn mysql_redteam() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::MySql, "aumyrt0001");
    if !up(&p) {
        return;
    }
    p.database = "au_rt".into();
    let user = agent_login::login_name(&p.id);
    let space = agent_login::space_of(&user);
    let root = connect(&e, &ConnectionProfile { database: String::new(), ..p.clone() }, &apw).unwrap();
    let rs = root.session(None);
    for sql in [
        "SET GLOBAL mandatory_roles = ''".to_string(),
        "DROP DATABASE IF EXISTS au_rt".into(),
        format!("DROP DATABASE IF EXISTS {space}"),
        format!("DROP USER IF EXISTS '{user}'@'%'"),
        "DROP ROLE IF EXISTS 'au_reader'".into(),
        "CREATE DATABASE au_rt".into(),
        "CREATE TABLE au_rt.users (id INT PRIMARY KEY, name VARCHAR(50), email VARCHAR(100), password_hash VARCHAR(100))".into(),
        "INSERT INTO au_rt.users VALUES (1, 'Alice', 'alice@acme.com', 'h1')".into(),
        "CREATE TABLE au_rt.secrets (id INT, s VARCHAR(50))".into(),
        "INSERT INTO au_rt.secrets VALUES (1, 'top-secret')".into(),
        "CREATE TABLE au_rt.`we``ird` (id INT, `e``mail` VARCHAR(100))".into(),
        "INSERT INTO au_rt.`we``ird` VALUES (1, 'carol@acme.com')".into(),
        "CREATE TABLE au_rt.`x'; DROP USER root; --` (id INT)".into(),
        "CREATE TABLE au_rt.`ünï` (id INT, `naïve` VARCHAR(10))".into(),
        "CREATE ROLE 'au_reader'".into(),
        "GRANT SELECT ON au_rt.secrets TO 'au_reader'".into(),
    ] {
        ok(&e, &rs, &sql);
    }
    let a = connect(&e, &p, &apw).unwrap();
    p.mcp = Some(McpAccess::Write);
    let r = &mut p.mcp_rules;
    r.view = AgentView::Masked;
    r.visible.items = vec![VisibleItem { name: "secrets".into(), kind: ItemKind::Table }, VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column }];
    r.sensitive = vec![SensitiveCol { col: "users.email".into(), how: Mask::Partial, on: true }, SensitiveCol { col: "we`ird.e`mail".into(), how: Mask::Partial, on: true }];
    // A role every account gets: the review says the database can't keep the agent from it.
    ok(&e, &rs, "SET GLOBAL mandatory_roles = 'au_reader'");
    let rv = e.core.block_on(live::review(&a, &p, &opts())).unwrap();
    assert!(rv.plan.exposed.iter().any(|x| x.object.contains("au_reader")), "{:?}", rv.plan.exposed);
    assert!(rv.plan.shown().contains("PASSWORD REQUIRE CURRENT"));
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let g = agent.session(None);
    ok(&e, &g, "SET ROLE 'au_reader'");
    assert_eq!(first(&ok(&e, &g, "SELECT s FROM au_rt.secrets")), "top-secret", "what mandatory_roles gives, said in the review");
    ok(&e, &g, "SET ROLE NONE");
    ok(&e, &rs, "SET GLOBAL mandatory_roles = ''");
    drop(g);
    drop(agent);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    let g = agent.session(None);
    let x = run(&e, &g, "SET ROLE 'au_reader'").unwrap_err();
    assert!(x.contains("not granted"), "{x}");
    denied(&e, &g, "SELECT * FROM au_rt.secrets");
    // Its own password: never by itself (it would then hold one Dataman doesn't).
    let x = run(&e, &g, "ALTER USER USER() IDENTIFIED BY 'mine-now-123'").unwrap_err();
    assert!(x.contains("current password") || x.contains("REPLACE"), "{x}");
    let x = run(&e, &g, "SET PASSWORD = 'mine-now-123'").unwrap_err();
    assert!(x.contains("current password") || x.contains("REPLACE"), "{x}");
    // Files, other accounts, the grant tables.
    assert_eq!(ok(&e, &g, "SELECT LOAD_FILE('/etc/passwd')")[0][0], None, "no FILE privilege");
    denied(&e, &g, "SELECT 1 INTO OUTFILE '/tmp/au_out.txt'");
    denied(&e, &g, "SELECT * FROM mysql.user");
    denied(&e, &g, "SHOW GRANTS FOR 'root'@'%'");
    denied(&e, &g, "CREATE USER 'x'@'%'");
    denied(&e, &g, &format!("GRANT SELECT ON au_rt.users TO '{user}'@'%'"));
    // Masked and hidden, sideways.
    denied(&e, &g, "SELECT id FROM au_rt.users WHERE email = 'alice@acme.com'");
    denied(&e, &g, "UPDATE au_rt.users SET name = name WHERE email LIKE 'a%'");
    denied(&e, &g, "DELETE FROM au_rt.users WHERE password_hash = 'h1'");
    let users = login.view_for("au_rt", "users").unwrap().view.clone();
    assert!(ok(&e, &g, &format!("SELECT id FROM {users} WHERE email = 'alice@acme.com'")).is_empty(), "a filter on the view compares the masked value");
    denied(&e, &g, &format!("CREATE OR REPLACE VIEW {users} AS SELECT 1"));
    denied(&e, &g, &format!("ALTER USER '{user}'@'%' ATTRIBUTE '{{\"dataman\": \"x\"}}'"));
    // Hostile names through their views.
    let weird = login.view_for("au_rt", "we`ird").unwrap().view.clone();
    assert_eq!(first(&ok(&e, &g, &format!("SELECT `e``mail` FROM {weird}"))), "c•••@acme.com");
    ok(&e, &g, "SELECT id FROM au_rt.`x'; DROP USER root; --`");
    ok(&e, &g, "SELECT `naïve` FROM au_rt.`ünï`");
    assert_eq!(first(&ok(&e, &rs, "SELECT COUNT(*) FROM mysql.user WHERE User = 'root'")), "2");

    // Drift: a table made since is not granted until Apply.
    ok(&e, &rs, "CREATE TABLE au_rt.later (id INT)");
    denied(&e, &g, "SELECT * FROM au_rt.later");
    let d = e.core.block_on(live::drift(&a, &p, &login, &password)).unwrap();
    assert!(d.changes.iter().any(|c| c.add && c.line.contains("later")), "{:?}", d.changes);
    e.core.block_on(live::apply(&a, &d.plan, &login.database)).unwrap();
    ok(&e, &g, "SELECT * FROM au_rt.later");

    // Rotation: a session opened with the old password goes on working until it is ended.
    let new = agent_login::hash::password();
    let plan = e.core.block_on(live::rotation(&a, &p, &login, &password, &new)).unwrap();
    e.core.block_on(live::apply(&a, &plan, &login.database)).unwrap();
    assert!(run(&e, &g, "SELECT id FROM au_rt.later").is_ok(), "before: the old session still works");
    assert!(end_sessions(&e, &a, &p, &user) >= 1);
    assert!(run(&e, &g, "SELECT id FROM au_rt.later").is_err(), "ended");
    assert!(as_agent(&e, &p, &user, &password).is_err());
    let password = new;
    drop((g, agent));

    // Remove: a session left open would outlive the user (MySQL keeps it).
    let left = as_agent(&e, &p, &user, &password).unwrap();
    let ls = left.session(None);
    ok(&e, &ls, "SELECT 1");
    end_sessions(&e, &a, &p, &user);
    let plan = e.core.block_on(live::removal(&a, &p, &login)).unwrap();
    e.core.block_on(live::apply(&a, &plan, &login.database)).unwrap();
    assert!(run(&e, &ls, "SELECT id FROM au_rt.later").is_err(), "ended with it");
    assert_eq!(first(&ok(&e, &rs, &format!("SELECT COUNT(*) FROM mysql.user WHERE User = '{user}'"))), "0");
    assert_eq!(first(&ok(&e, &rs, &format!("SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{space}'"))), "0");
    drop((ls, left, a));
    ok(&e, &rs, "DROP DATABASE IF EXISTS au_rt");
    ok(&e, &rs, "DROP ROLE IF EXISTS 'au_reader'");
}

/// A session left open after DROP USER: MySQL 8 refuses its next read (its privileges went
/// with the user); Remove ends the login's sessions anyway.
#[test]
fn mysql_session_outlives_drop_user() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (p, apw) = admin(Engine::MySql, "aumyrt0002");
    if !up(&p) {
        return;
    }
    let root = connect(&e, &ConnectionProfile { database: String::new(), ..p.clone() }, &apw).unwrap();
    let rs = root.session(None);
    for sql in ["DROP USER IF EXISTS 'au_gone'@'%'", "CREATE DATABASE IF NOT EXISTS au_rt2", "CREATE TABLE IF NOT EXISTS au_rt2.t (id INT)", "CREATE USER 'au_gone'@'%' IDENTIFIED BY 'Gone-pw-123'", "GRANT SELECT ON au_rt2.t TO 'au_gone'@'%'"] {
        ok(&e, &rs, sql);
    }
    let gone = connect(&e, &ConnectionProfile { user: "au_gone".into(), database: "au_rt2".into(), ..p.clone() }, "Gone-pw-123").unwrap();
    let gs = gone.session(None);
    ok(&e, &gs, "SELECT * FROM au_rt2.t");
    ok(&e, &rs, "DROP USER 'au_gone'@'%'");
    let after = run(&e, &gs, "SELECT * FROM au_rt2.t");
    assert!(after.is_err(), "MySQL 8 checks a session's table privileges at each statement: {after:?}");
    drop((gs, gone));
    ok(&e, &rs, "DROP DATABASE IF EXISTS au_rt2");
}

#[test]
fn sql_server_redteam() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::SqlServer, "aumsrt0001");
    if !up(&p) {
        return;
    }
    p.database = "au_rt".into();
    let user = agent_login::login_name(&p.id);
    let root = connect(&e, &ConnectionProfile { database: "master".into(), ..p.clone() }, &apw).unwrap();
    let rs = root.session(None);
    for sql in [
        "IF DB_ID('au_rt') IS NOT NULL BEGIN ALTER DATABASE au_rt SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE au_rt; END".to_string(),
        format!("IF SUSER_ID('{user}') IS NOT NULL DROP LOGIN [{user}]"),
        "CREATE DATABASE au_rt".into(),
    ] {
        ok(&e, &rs, &sql);
    }
    let a = connect(&e, &p, &apw).unwrap();
    let s = a.session(None);
    for sql in [
        "CREATE TABLE dbo.users (id INT PRIMARY KEY, name NVARCHAR(50), email NVARCHAR(100), password_hash NVARCHAR(100))",
        "INSERT INTO dbo.users VALUES (1, N'Alice', N'alice@acme.com', N'h1')",
        "CREATE TABLE dbo.secrets (id INT, s NVARCHAR(50))",
        "INSERT INTO dbo.secrets VALUES (1, N'top-secret')",
        "CREATE TABLE dbo.pub_leak (id INT, s NVARCHAR(50))",
        "INSERT INTO dbo.pub_leak VALUES (1, N'public-secret')",
        "GRANT SELECT ON dbo.pub_leak TO public",
        "CREATE TABLE dbo.[we]]ird] (id INT, [e]]mail] NVARCHAR(100))",
        "INSERT INTO dbo.[we]]ird] VALUES (1, N'carol@acme.com')",
        "CREATE TABLE dbo.[x'; DROP LOGIN sa; --] (id INT)",
        "CREATE TABLE dbo.salt (id INT, v NVARCHAR(50))",
    ] {
        ok(&e, &s, sql);
    }
    ok(&e, &s, "CREATE PROCEDURE dbo.leak WITH EXECUTE AS OWNER AS SELECT s FROM dbo.secrets");
    p.mcp = Some(McpAccess::Read);
    let r = &mut p.mcp_rules;
    r.view = AgentView::Masked;
    r.visible.items = vec![VisibleItem { name: "secrets".into(), kind: ItemKind::Table }, VisibleItem { name: "pub_leak".into(), kind: ItemKind::Table }, VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column }];
    r.sensitive = vec![SensitiveCol { col: "users.email".into(), how: Mask::Partial, on: true }, SensitiveCol { col: "we]ird.e]mail".into(), how: Mask::Partial, on: true }, SensitiveCol { col: "salt.v".into(), how: Mask::Hash, on: true }];
    let rv = e.core.block_on(live::review(&a, &p, &opts())).unwrap();
    assert!(rv.plan.exposed.iter().any(|x| x.object == "dbo.pub_leak"), "{:?}", rv.plan.exposed);
    assert!(rv.plan.copy().contains("REVOKE SELECT ON OBJECT::[dbo].[pub_leak] FROM [public];"));
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let g = agent.session(None);
    // Becoming someone else, the server's shell and files.
    denied(&e, &g, "EXECUTE AS LOGIN = 'sa'");
    denied(&e, &g, "EXECUTE AS USER = 'dbo'");
    denied(&e, &g, "EXEC master.dbo.xp_cmdshell 'whoami'");
    denied(&e, &g, "SELECT * FROM OPENROWSET(BULK '/etc/passwd', SINGLE_CLOB) AS x");
    denied(&e, &g, "EXEC dbo.leak");
    denied(&e, &g, "ALTER LOGIN [sa] WITH PASSWORD = 'Mine-now-123'");
    let x = run(&e, &g, &format!("ALTER LOGIN [{user}] WITH PASSWORD = 'Mine-now-123'")).unwrap_err();
    assert!(x.contains("OLD_PASSWORD") || x.to_ascii_lowercase().contains("permission") || x.contains("15151"), "its own password needs the old one: {x}");
    denied(&e, &g, "CREATE TABLE dbo.mine (i INT)");
    denied(&e, &g, "GRANT SELECT ON dbo.users TO public");
    denied(&e, &g, "ALTER ROLE db_owner ADD MEMBER [public]");
    // Hidden and masked, sideways.
    denied(&e, &g, "SELECT * FROM dbo.secrets");
    denied(&e, &g, "SELECT email FROM dbo.users");
    denied(&e, &g, "SELECT id FROM dbo.users WHERE email = N'alice@acme.com'");
    assert!(ok(&e, &g, "SELECT name FROM sys.tables WHERE name = 'secrets'").is_empty(), "SQL Server hides it");
    assert_eq!(ok(&e, &g, "SELECT OBJECT_ID('dbo.secrets')")[0][0], None);
    let weird = login.view_for("dbo", "we]ird").unwrap().view.clone();
    assert_eq!(first(&ok(&e, &g, &format!("SELECT [e]]mail] FROM {weird}"))), "c•••@acme.com");
    let salt_view = login.view_for("dbo", "salt").unwrap().view.clone();
    assert!(!salt_view.ends_with("[salt]"), "{salt_view}");
    assert_eq!(first(&ok(&e, &rs, "SELECT COUNT(*) FROM sys.server_principals WHERE name = 'sa' AND is_disabled = 0")), "1");
    // What public holds, said in the review.
    assert_eq!(first(&ok(&e, &g, "SELECT s FROM dbo.pub_leak")), "public-secret");

    // Rotation: the old password's sessions end.
    let new = agent_login::hash::password();
    let plan = e.core.block_on(live::rotation(&a, &p, &login, &password, &new)).unwrap();
    e.core.block_on(live::apply(&a, &plan, &login.database)).unwrap();
    let q = "SELECT id FROM dbo.[x'; DROP LOGIN sa; --]";
    assert!(run(&e, &g, q).is_ok(), "before: the old session still works");
    assert!(end_sessions(&e, &a, &p, &user) >= 1);
    assert!(run(&e, &g, q).is_err(), "ended");
    assert!(as_agent(&e, &p, &user, &password).is_err());
    drop((g, agent));

    // The connection's database changed since: Remove still finds the user where it was made.
    let moved = ConnectionProfile { database: "master".into(), ..p.clone() };
    let plan = e.core.block_on(live::removal(&a, &moved, &login)).unwrap();
    assert!(plan.shown().contains(&format!("DROP USER IF EXISTS [{user}]")) && plan.shown().contains("DROP VIEW"), "{}", plan.shown());
    e.core.block_on(live::apply(&a, &plan, &login.database)).unwrap();
    assert_eq!(first(&ok(&e, &s, &format!("SELECT COUNT(*) FROM sys.database_principals WHERE name = '{user}'"))), "0", "its user went from au_rt");
    assert_eq!(first(&ok(&e, &rs, &format!("SELECT COUNT(*) FROM sys.server_principals WHERE name = '{user}'"))), "0");
    drop((s, a));
    ok(&e, &rs, "ALTER DATABASE au_rt SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE au_rt;");
}

#[test]
fn mongodb_redteam() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::MongoDb, "aumgrt0001");
    if !up(&p) {
        return;
    }
    p.database = "au_rt".into();
    let user = agent_login::login_name(&p.id);
    let space = agent_login::space_of(&user);
    let a = connect(&e, &p, &apw).unwrap();
    for t in ["db.dropDatabase()".to_string(), format!("db.getSiblingDB('admin').runCommand({{dropUser: '{user}'}})"), format!("db.getSiblingDB('admin').runCommand({{dropRole: '{user}'}})")] {
        let _ = nosql(&e, &a, "au_rt", &t);
    }
    for t in [
        "db.orders.insertMany([{_id: 1, total: 10, customer_id: 1}])",
        "db.users.insertMany([{_id: 1, name: 'Alice', email: 'alice@acme.com', password_hash: 'h1'}])",
        "db.audit_log.insertOne({what: 'top-secret'})",
    ] {
        nosql(&e, &a, "au_rt", t).unwrap_or_else(|x| panic!("{t}: {x}"));
    }
    p.mcp = Some(McpAccess::Write);
    let r = &mut p.mcp_rules;
    r.view = AgentView::Masked;
    r.visible.items = vec![VisibleItem { name: "audit_log".into(), kind: ItemKind::Table }, VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column }];
    r.sensitive = vec![SensitiveCol { col: "users.email".into(), how: Mask::Partial, on: true }];
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    // The raw driver: Dataman's view mapping out of the way.
    agent.set_agent_views(&[]);
    let view = format!("{space}.users");
    for (what, t) in [
        ("$lookup into a hidden collection", "db.orders.aggregate([{$lookup: {from: 'audit_log', pipeline: [], as: 'x'}}])".to_string()),
        ("$lookup into the masked collection itself", "db.orders.aggregate([{$lookup: {from: 'users', localField: 'customer_id', foreignField: '_id', as: 'u'}}])".into()),
        ("$unionWith a hidden collection", "db.orders.aggregate([{$unionWith: 'audit_log'}])".into()),
        ("$unionWith from its view", format!("db.getCollection('{view}').aggregate([{{$unionWith: 'users'}}])")),
        ("$out into a new collection", "db.orders.aggregate([{$out: 'au_copy'}])".into()),
        ("$merge into a hidden collection", "db.orders.aggregate([{$merge: {into: 'audit_log'}}])".into()),
        ("a view of its own over users", "db.createView('mine', 'users', [])".into()),
        ("system.users", "db.getSiblingDB('admin').getCollection('system.users').find({})".into()),
        ("another user's info", "db.getSiblingDB('admin').runCommand({usersInfo: 'root', showCredentials: true})".into()),
        ("its own password", format!("db.getSiblingDB('admin').runCommand({{updateUser: '{user}', pwd: 'mine-now'}})")),
        ("its own roles", format!("db.getSiblingDB('admin').runCommand({{grantRolesToUser: '{user}', roles: ['root']}})")),
        ("its role's privileges", format!("db.getSiblingDB('admin').runCommand({{updateRole: '{user}', privileges: []}})")),
        ("collMod of its view", format!("db.runCommand({{collMod: '{view}', viewOn: 'users', pipeline: []}})")),
        ("dropping its view", format!("db.getCollection('{view}').drop()")),
        ("count of a hidden collection", "db.audit_log.countDocuments({})".into()),
        ("distinct on the masked base", "db.users.distinct('email')".into()),
        ("the base collection's stats", "db.runCommand({collStats: 'users'})".into()),
        ("serverStatus", "db.getSiblingDB('admin').runCommand({serverStatus: 1})".into()),
        ("getLog", "db.getSiblingDB('admin').runCommand({getLog: 'global'})".into()),
    ] {
        let r = nosql(&e, &agent, "au_rt", &t);
        assert!(r.as_ref().is_err_and(|x| x.contains("not authorized") || x.contains("Unauthorized")), "{what}: {t} → {r:?}");
    }
    // Its view: the hidden field gone, the e-mail masked; filtering on it compares the mask.
    let u = docs(&nosql(&e, &agent, "au_rt", &format!("db.getCollection('{view}').find({{email: 'alice@acme.com'}})")).unwrap());
    assert!(u.is_empty(), "{u:?}");
    let u = docs(&nosql(&e, &agent, "au_rt", &format!("db.getCollection('{view}').find({{password_hash: 'h1'}})")).unwrap());
    assert!(u.is_empty(), "the hidden field is not there to filter on: {u:?}");
    // Server-side JavaScript runs within its privileges (MongoDB can't turn it off per user).
    let j = nosql(&e, &agent, "au_rt", "db.orders.find({$where: 'function() { return true }'})");
    eprintln!("MongoDB $where as the agent: {:?}", j.as_ref().map(|_| "ran"));
    // Rotation: MongoDB keeps a connection authenticated with the old password (it has no
    // command to end another user's connections): a limitation, said in the report.
    agent.set_agent_views(&login.views);
    let new = agent_login::hash::password();
    let plan = e.core.block_on(live::rotation(&a, &p, &login, &password, &new)).unwrap();
    e.core.block_on(live::apply(&a, &plan, &login.database)).unwrap();
    assert!(as_agent(&e, &p, &user, &password).is_err(), "the old password no longer logs in");
    let still = nosql(&e, &agent, "au_rt", "db.orders.find({})");
    eprintln!("MongoDB, a connection from before the rotation: {:?}", still.as_ref().map(|_| "still reads"));
    drop(agent);
    remove(&e, &a, &p, &login, &new);
    let _ = nosql(&e, &a, "au_rt", "db.dropDatabase()");
}

#[test]
fn redis_redteam() {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(Engine::Redis, "aurdrt0001");
    if !up(&p) {
        return;
    }
    let user = agent_login::login_name(&p.id);
    let a = connect(&e, &p, &apw).unwrap();
    for t in [format!("ACL DELUSER {user}"), "SET cache:1 one".into(), "SET secret:1 topsecret".into(), "RPUSH cache:list a".into(), "CONFIG SET slowlog-log-slower-than 0".into(), "SLOWLOG RESET".into()] {
        nosql(&e, &a, "0", &t).unwrap_or_else(|x| panic!("{t}: {x}"));
    }
    p.mcp = Some(McpAccess::Read);
    p.mcp_rules.activity = false;
    p.mcp_rules.visible = dataman_core::privacy::Visible { mode: dataman_core::privacy::VisibleMode::Only, items: vec![VisibleItem { name: "cache:*".into(), kind: ItemKind::Table }] };
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    // The admin writes a secret while the slow log records everything.
    nosql(&e, &a, "0", "SET secret:2 slowlog-secret").unwrap();
    nosql(&e, &agent, "0", "GET cache:1").unwrap();
    for t in [
        "SLOWLOG GET 128",
        "GET secret:1",
        "TYPE secret:1",
        "EXISTS secret:1",
        "OBJECT ENCODING secret:1",
        "MEMORY USAGE secret:1",
        "DUMP secret:1",
        "SORT cache:list BY nosort GET secret:*",
        "SORT_RO cache:list BY nosort GET secret:*",
        "SUBSCRIBE __keyspace@0__:secret:1",
        "PSUBSCRIBE *",
        "CLIENT TRACKING ON BCAST PREFIX secret:",
        "CLIENT LIST",
        "CLIENT KILL USER default",
        "MONITOR",
        "CONFIG GET *",
        "ACL SETUSER default nopass",
        format!("ACL SETUSER {user} ~*").as_str(),
        "ACL GETUSER default",
        "EVAL_RO \"return redis.call('GET', 'secret:1')\" 0",
        "FCALL_RO f 0",
        "FUNCTION LIST",
        "MIGRATE 127.0.0.1 6380 secret:1 0 1000",
        "DEBUG OBJECT cache:1",
        "MODULE LIST",
        "SCRIPT LOAD \"return 1\"",
        "BGSAVE",
        "COPY cache:1 secret:copy",
        "LATENCY HISTOGRAM",
    ] {
        let r = nosql(&e, &agent, "0", t);
        let text = format!("{r:?}");
        assert!(!text.contains("topsecret") && !text.contains("slowlog-secret"), "{t}: {text}");
        assert!(r.is_err(), "{t} should be refused by Redis: {text}");
    }
    // Key names outside its patterns: SCAN and RANDOMKEY show them (an ACL pattern only
    // allows; said in the review as Dataman's).
    let names = format!("{:?}", nosql(&e, &agent, "0", "SCAN 0 COUNT 1000"));
    eprintln!("Redis SCAN as the agent: {}", if names.contains("secret:") { "lists names outside its patterns (known)" } else { "only its own" });
    // Another database: the same patterns (Redis has no ACL per database).
    nosql(&e, &a, "1", "SET secret:1 db1-secret").unwrap();
    let r = nosql(&e, &agent, "1", "GET secret:1");
    assert!(r.as_ref().is_err_and(|x| x.contains("NOPERM")), "{r:?}");
    // Rotation: a connection from the old password is ended.
    let new = agent_login::hash::password();
    let plan = e.core.block_on(live::rotation(&a, &p, &login, &password, &new)).unwrap();
    e.core.block_on(live::apply(&a, &plan, &login.database)).unwrap();
    assert!(nosql(&e, &agent, "0", "GET cache:1").is_ok(), "before: the old connection still works");
    assert!(end_sessions(&e, &a, &p, &user) >= 1);
    let after = nosql(&e, &agent, "0", "GET cache:1");
    assert!(after.is_err(), "ended: {after:?}");
    assert!(as_agent(&e, &p, &user, &password).is_err(), "the old password no longer logs in");
    drop(agent);
    remove(&e, &a, &p, &login, &new);
    for t in ["CONFIG SET slowlog-log-slower-than 10000", "DEL secret:1 secret:2 cache:1 cache:list", "SELECT 1"] {
        let _ = nosql(&e, &a, "0", t);
    }
    let _ = nosql(&e, &a, "1", "DEL secret:1");
}

fn search_redteam(engine: Engine, id: &str) {
    if !on() {
        return;
    }
    let _one = one_at_a_time();
    let e = env();
    let (mut p, apw) = admin(engine, id);
    if !up(&p) {
        return;
    }
    let os = engine == Engine::OpenSearch;
    let user = agent_login::login_name(&p.id);
    let a = connect(&e, &p, &apw).unwrap();
    let (ub, rb) = if os { ("/_plugins/_security/api/internalusers", "/_plugins/_security/api/roles") } else { ("/_security/user", "/_security/role") };
    for t in [format!("DELETE {ub}/{user}"), format!("DELETE {rb}/{user}"), "DELETE /aurt_users".into(), "DELETE /aurt_audit".into(), "DELETE /aurt_copy".into()] {
        let _ = nosql(&e, &a, "", &t);
    }
    for t in [
        "PUT /aurt_users/_doc/1?refresh=true\n{\"name\": \"Alice\", \"email\": \"alice@acme.com\", \"password_hash\": \"hash-secret\"}",
        "PUT /aurt_audit/_doc/1?refresh=true\n{\"what\": \"audit-secret\"}",
        "POST /_aliases\n{\"actions\": [{\"add\": {\"index\": \"aurt_audit\", \"alias\": \"aurt_all\"}}, {\"add\": {\"index\": \"aurt_users\", \"alias\": \"aurt_all\"}}]}",
    ] {
        nosql(&e, &a, "", t).unwrap_or_else(|x| panic!("{t}: {x}"));
    }
    p.mcp = Some(McpAccess::Read);
    p.mcp_rules.view = AgentView::Masked;
    p.mcp_rules.visible = dataman_core::privacy::Visible {
        mode: dataman_core::privacy::VisibleMode::Only,
        items: vec![VisibleItem { name: "aurt_users".into(), kind: ItemKind::Table }, VisibleItem { name: "aurt_users.password_hash".into(), kind: ItemKind::Column }],
    };
    p.mcp_rules.sensitive = vec![SensitiveCol { col: "aurt_users.email".into(), how: Mask::Partial, on: true }];
    let (login, password, rv) = create(&e, &a, &p);
    let agent = as_agent(&e, &p, &user, &password).unwrap();
    verify_all(&e, &agent, &rv);
    let secret = |text: &str| text.contains("hash-secret") || text.contains("audit-secret") || text.contains("alice@acme.com");
    for (what, t) in [
        ("an aggregation on the hidden field", "POST /aurt_users/_search\n{\"size\": 0, \"aggs\": {\"x\": {\"terms\": {\"field\": \"password_hash.keyword\"}}}}"),
        ("an aggregation on the masked field", "POST /aurt_users/_search\n{\"size\": 0, \"aggs\": {\"x\": {\"terms\": {\"field\": \"email.keyword\"}}}}"),
        ("docvalue_fields", "POST /aurt_users/_search\n{\"docvalue_fields\": [\"password_hash.keyword\", \"email.keyword\"]}"),
        ("stored_fields", "POST /aurt_users/_search\n{\"stored_fields\": [\"*\"], \"_source\": true}"),
        ("highlight", "POST /aurt_users/_search\n{\"query\": {\"match\": {\"password_hash\": \"hash-secret\"}}, \"highlight\": {\"fields\": {\"*\": {}}}}"),
        ("a term query on the hidden field", "POST /aurt_users/_search\n{\"query\": {\"term\": {\"password_hash.keyword\": \"hash-secret\"}}}"),
        ("a wildcard over every index", "POST /aurt_*/_search\n{\"query\": {\"match_all\": {}}}"),
        ("_all", "POST /_all/_search\n{\"query\": {\"match_all\": {}}}"),
        ("the field's capabilities", "GET /aurt_users/_field_caps?fields=*"),
        ("the mapping", "GET /aurt_users/_mapping"),
        ("a document by id", "GET /aurt_users/_doc/1"),
        ("_mget", "POST /_mget\n{\"docs\": [{\"_index\": \"aurt_users\", \"_id\": \"1\"}]}"),
        ("term vectors", "GET /aurt_users/_termvectors/1?fields=password_hash,email"),
    ] {
        let r = nosql(&e, &agent, "", t);
        let text = format!("{r:?}");
        assert!(!secret(&text), "{what}: {t} → {text}");
    }
    for (what, t) in [
        ("the hidden index", "POST /aurt_audit/_search"),
        ("an alias over the hidden index", "POST /aurt_all/_search"),
        ("_msearch into the hidden index", "POST /_msearch\n{\"index\": \"aurt_audit\"}\n{\"query\": {\"match_all\": {}}}"),
        ("a document of the hidden index", "GET /aurt_audit/_doc/1"),
        ("_reindex out of it", "POST /_reindex\n{\"source\": {\"index\": \"aurt_users\"}, \"dest\": {\"index\": \"aurt_copy\"}}"),
        ("other users", if os { "GET /_plugins/_security/api/internalusers" } else { "GET /_security/user" }),
        ("its role", if os { "PUT /_plugins/_security/api/roles/x\n{}" } else { "PUT /_security/role/x\n{}" }),
        ("cluster settings", "PUT /_cluster/settings\n{\"persistent\": {\"cluster.routing.allocation.enable\": \"all\"}}"),
        ("nodes", "GET /_nodes/stats"),
        ("a script stored", "PUT /_scripts/x\n{\"script\": {\"lang\": \"painless\", \"source\": \"1\"}}"),
    ] {
        let r = nosql(&e, &agent, "", t);
        let text = format!("{r:?}");
        assert!(!secret(&text), "{what}: {t} → {text}");
        let refused = r.as_ref().err().is_some_and(|x| x.contains("403") || x.contains("security_exception") || x.contains("unauthorized") || x.contains("no permissions") || x.contains("404") || x.contains("index_not_found"));
        let empty = r.as_ref().is_ok_and(|o| !text.contains("\"_source\"") && !matches!(o, Output::Docs(d) if !d.docs.is_empty()));
        assert!(refused || empty, "{what}: {t} → {text}");
    }
    if !os {
        let r = nosql(&e, &agent, "", "POST /_sql?format=json\n{\"query\": \"SELECT * FROM aurt_audit\"}");
        assert!(!secret(&format!("{r:?}")), "{r:?}");
        let r = nosql(&e, &agent, "", "POST /_sql?format=json\n{\"query\": \"SELECT password_hash, email FROM aurt_users\"}");
        assert!(!secret(&format!("{r:?}")), "{r:?}");
    }
    drop(agent);
    remove(&e, &a, &p, &login, &password);
    for t in ["DELETE /aurt_users", "DELETE /aurt_audit", "DELETE /aurt_copy"] {
        let _ = nosql(&e, &a, "", t);
    }
}

#[test]
fn elasticsearch_redteam() {
    search_redteam(Engine::Elasticsearch, "auesrt0001");
}

#[test]
fn opensearch_redteam() {
    search_redteam(Engine::OpenSearch, "auosrt0001");
}
