//! A connection whose sockets the server closed (`KILL`, `pg_terminate_backend`): what runs next
//! reconnects, never sends a statement twice, and says so when a session's state went with it.

mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode};
use dataman_core::config::Engine;
use dataman_core::link::CHECK_AFTER;
use dataman_core::{Link, ResultSet, Session, TxStatus};

struct Case {
    engine: Engine,
    /// Opened on its own database, which only this test's sockets use.
    conn: Arc<Connection>,
    admin: Arc<Connection>,
    db: String,
}

/// One database per test and engine: the tests run side by side and each kills every socket of
/// its own.
fn cases(e: &Env, test: &str) -> Vec<Case> {
    let mut out = Vec::new();
    let db = format!("dataman_t_relink_{test}");
    if let Some(admin) = pg(e) {
        exec_quiet(e, &admin, &[&format!("CREATE DATABASE {db}")]);
        let conn = e.core.block_on(e.core.connect(ConnectionProfile { database: db.clone(), ..pg_profile() }, Some(pg_password()))).unwrap();
        out.push(Case { engine: Engine::Postgres, conn, admin, db: db.clone() });
    }
    if let Some(admin) = mysql_on(e, mysql_profile("")) {
        exec(e, &admin, &[&format!("CREATE DATABASE IF NOT EXISTS {db}")]);
        out.push(Case { engine: Engine::MySql, conn: mysql_on(e, mysql_profile(&db)).unwrap(), admin, db: db.clone() });
    }
    if let Some(admin) = mssql_on(e, mssql_profile("master")) {
        exec(e, &admin, &[&format!("IF DB_ID(N'{db}') IS NULL CREATE DATABASE [{db}]")]);
        out.push(Case { engine: Engine::SqlServer, conn: mssql_on(e, mssql_profile(&db)).unwrap(), admin, db: db.clone() });
    }
    if out.is_empty() {
        eprintln!("skipping: no PostgreSQL, MySQL or SQL Server to reach");
    }
    out
}

use dataman_core::config::ConnectionProfile;

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = s.execute(sql);
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
    rs
}

fn ok(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

fn failed(e: &Env, s: &Arc<Session>, sql: &str) -> String {
    let rs = run(e, s, sql);
    assert_eq!(rs.phase(), Phase::Failed, "{sql} ran");
    rs.status().error.unwrap().message
}

impl Case {
    fn admin(&self, e: &Env, sql: &str) {
        exec(e, &self.admin, &[sql]);
    }

    fn kill(&self, e: &Env, pid: i32) {
        let sql = match self.engine {
            Engine::Postgres => format!("SELECT pg_terminate_backend({pid})"),
            _ => format!("KILL {pid}"),
        };
        self.admin(e, &sql);
    }

    /// Every socket on this case's database: the catalog's, the tabs', the agents'.
    fn kill_all(&self, e: &Env) {
        let pids = match self.engine {
            Engine::Postgres => format!("SELECT pid FROM pg_stat_activity WHERE datname = '{}' AND pid <> pg_backend_pid()", self.db),
            Engine::MySql => format!("SELECT id FROM information_schema.processlist WHERE db = '{}' AND id <> CONNECTION_ID()", self.db),
            _ => format!("SELECT session_id FROM sys.dm_exec_sessions WHERE database_id = DB_ID(N'{}') AND session_id <> @@SPID", self.db),
        };
        let rs = query(e, &self.admin, &pids);
        let snap = rs.snapshot();
        let n = (0..rs.rows()).map(|r| self.kill(e, snap.text(r, 0).unwrap().parse().unwrap())).count();
        assert!(n > 0, "{:?}: nothing to kill", self.engine);
    }

    fn busy(&self, e: &Env, pid: i32) -> bool {
        let sql = match self.engine {
            Engine::Postgres => format!("SELECT COUNT(*) FROM pg_stat_activity WHERE pid = {pid} AND state = 'active'"),
            Engine::MySql => format!("SELECT COUNT(*) FROM information_schema.processlist WHERE id = {pid} AND command IN ('Query', 'Execute')"),
            _ => format!("SELECT COUNT(*) FROM sys.dm_exec_requests WHERE session_id = {pid}"),
        };
        query(e, &self.admin, &sql).snapshot().text(0, 0).unwrap() == "1"
    }

    fn exists(&self, e: &Env, pid: i32) -> bool {
        let sql = match self.engine {
            Engine::Postgres => format!("SELECT COUNT(*) FROM pg_stat_activity WHERE pid = {pid}"),
            Engine::MySql => format!("SELECT COUNT(*) FROM information_schema.processlist WHERE id = {pid}"),
            _ => format!("SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE session_id = {pid}"),
        };
        query(e, &self.admin, &sql).snapshot().text(0, 0).unwrap() == "1"
    }

    fn table(&self, e: &Env, s: &Arc<Session>) {
        ok(e, s, "DROP TABLE IF EXISTS t");
        ok(e, s, "CREATE TABLE t (n int)");
    }

    fn count(&self, e: &Env) -> String {
        let rs = query(e, &self.conn, "SELECT COUNT(*) FROM t");
        rs.snapshot().text(0, 0).unwrap().into_owned()
    }

    fn begin(&self) -> &'static str {
        if self.engine == Engine::SqlServer { "BEGIN TRANSACTION" } else { "BEGIN" }
    }
}

fn idle() {
    std::thread::sleep(CHECK_AFTER + Duration::from_millis(200));
}

/// A wait for what the server does on its own (closing a socket), bounded.
fn until(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_dead_idle_socket_reconnects_and_the_statement_runs_once() {
    let e = env();
    for c in cases(&e, "idle") {
        let s = c.conn.session(None);
        c.table(&e, &s);
        let pid = s.backend_pid().unwrap();
        c.kill(&e, pid);
        if c.engine == Engine::Postgres {
            // Its driver reads the socket while it is idle: the end is known at once.
            until("PostgreSQL's link lost", || c.conn.link() == Link::Lost);
        }
        idle();
        ok(&e, &s, "INSERT INTO t VALUES (1)");
        assert_eq!(c.count(&e), "1", "{:?}", c.engine);
        assert_eq!(c.conn.link(), Link::Up);
    }
}

#[test]
fn a_statement_cut_off_is_reported_and_not_sent_again() {
    let e = env();
    for c in cases(&e, "cut") {
        let s = c.conn.session(None);
        c.table(&e, &s);
        let slow = match c.engine {
            Engine::Postgres => "INSERT INTO t SELECT 1 FROM pg_sleep(20)",
            Engine::MySql => "INSERT INTO t SELECT SLEEP(20)",
            _ => "WAITFOR DELAY '00:00:20'; INSERT INTO t VALUES (1)",
        };
        let pid = s.backend_pid().unwrap();
        let rs = s.execute(slow);
        until(&format!("{:?} runs it", c.engine), || c.busy(&e, pid));
        c.kill(&e, pid);
        e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") });
        let err = rs.status().error.unwrap();
        assert_eq!(rs.phase(), Phase::Failed);
        assert!(err.message.starts_with("Connection lost during this statement: it may have run."), "{:?}: {err:?}", c.engine);
        assert!(err.detail.is_some(), "the driver's own words stay");
        assert_eq!(c.conn.link(), Link::Lost, "{:?}", c.engine);
        // MySQL's KILL ends the SLEEP early and the INSERT may still finish: once, by the
        // server, which is what the message says. Never a second time by Dataman.
        until(&format!("{:?}: the killed session gone", c.engine), || !c.exists(&e, pid));
        let once: u32 = c.count(&e).parse().unwrap();
        assert!(once <= 1, "{:?}: sent again", c.engine);
        ok(&e, &s, "INSERT INTO t VALUES (2)");
        assert_eq!(c.count(&e), (once + 1).to_string());
        assert_eq!(c.conn.link(), Link::Up);
    }
}

#[test]
fn a_transaction_lost_with_its_socket_is_reported_not_carried_on() {
    let e = env();
    for c in cases(&e, "tx") {
        let s = c.conn.session(None);
        c.table(&e, &s);
        ok(&e, &s, c.begin());
        ok(&e, &s, "INSERT INTO t VALUES (1)");
        assert_eq!(s.tx().status, TxStatus::Open);
        c.kill(&e, s.backend_pid().unwrap());
        idle();
        let msg = failed(&e, &s, "INSERT INTO t VALUES (2)");
        // PostgreSQL knew before the statement went out; the others learn by sending it.
        let want = if c.engine == Engine::Postgres { "Connection lost with a transaction open: the server rolled it back. This statement did not run." } else { "Connection lost during this statement, with a transaction open" };
        assert!(msg.starts_with(want), "{:?}: {msg}", c.engine);
        assert_eq!(s.tx().status, TxStatus::Idle);
        assert_eq!(c.count(&e), "0", "{:?}: rolled back, and nothing ran outside it", c.engine);
        ok(&e, &s, "INSERT INTO t VALUES (3)");
        assert_eq!(c.count(&e), "1");
    }
}

#[test]
fn a_commit_on_a_lost_connection_says_so() {
    let e = env();
    for c in cases(&e, "commit") {
        let s = c.conn.session(None);
        c.table(&e, &s);
        ok(&e, &s, c.begin());
        ok(&e, &s, "INSERT INTO t VALUES (1)");
        c.kill(&e, s.backend_pid().unwrap());
        if c.engine == Engine::Postgres {
            until("PostgreSQL's session gone", || s.tx().status == TxStatus::Idle);
        }
        let err = e.core.block_on(s.end_transaction(true)).unwrap_err().to_string();
        assert!(err.starts_with("Connection lost"), "{:?}: {err}", c.engine);
        assert_eq!(c.count(&e), "0");
        ok(&e, &s, "INSERT INTO t VALUES (2)");
        assert_eq!(c.count(&e), "1");
    }
}

#[test]
fn session_state_lost_with_its_socket_is_reported() {
    let e = env();
    for c in cases(&e, "state") {
        let s = c.conn.session(None);
        let set = match c.engine {
            Engine::Postgres => "SET search_path = pg_catalog",
            Engine::MySql => "SET @dataman_relink = 1",
            _ => "SET LANGUAGE Deutsch",
        };
        ok(&e, &s, set);
        c.kill(&e, s.backend_pid().unwrap());
        idle();
        let msg = failed(&e, &s, "SELECT 1");
        assert!(msg.starts_with("Connection lost, and this session's settings"), "{:?}: {msg}", c.engine);
        ok(&e, &s, "SELECT 1");
    }
}

#[test]
fn the_catalog_and_agents_reconnect_on_their_own() {
    let e = env();
    for c in cases(&e, "catalog") {
        let s = c.conn.session(None);
        c.table(&e, &s);
        let read = || e.core.block_on(c.conn.bounded(Some(&c.db), "SELECT COUNT(*) FROM t", vec![], Mode::Read, Limits::default(), std::future::pending::<()>()));
        read().unwrap();
        c.kill_all(&e);
        idle();
        c.conn.schema.invalidate(None);
        let dbs = e.core.block_on(c.conn.databases()).unwrap();
        assert!(dbs.contains(&c.db), "{:?}: {dbs:?}", c.engine);
        let o = read().unwrap_or_else(|s| panic!("{:?}: {s:?}", c.engine));
        assert_eq!(o.rows.len(), 1);
        ok(&e, &s, "SELECT 1");
        assert_eq!(c.conn.link(), Link::Up);
    }
}

#[test]
fn sql_server_parse_checks_stay_quiet_while_the_connection_is_lost() {
    let e = env();
    for c in cases(&e, "parse").into_iter().filter(|c| c.engine == Engine::SqlServer) {
        let check = |sql: &str| e.core.block_on(c.conn.parse_check(&c.db, sql.into())).unwrap();
        assert_eq!(check("SELECT FROM").len(), 1);
        c.kill_all(&e);
        assert!(check("SELECT FROM").is_empty(), "the dead socket gives no bogus error");
        assert_eq!(c.conn.link(), Link::Lost);
        assert!(check("SELECT FROM").is_empty(), "and no login while lost");
        let s = c.conn.session(None);
        ok(&e, &s, "SELECT 1");
        assert_eq!(check("SELECT FROM").len(), 1, "back once a statement reconnected");
    }
}
