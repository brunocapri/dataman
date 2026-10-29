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
