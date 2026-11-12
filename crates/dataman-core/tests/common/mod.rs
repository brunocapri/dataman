#![allow(dead_code, unused_imports)]

pub use std::sync::Arc;
pub use std::time::{Duration, Instant};

use dataman_core::config::{ConnectionProfile, Dirs, SslMode};
pub use dataman_core::result::Phase;
pub use dataman_core::{Connection, Core, MemorySecrets};

pub struct Env {
    pub core: Core,
    _dir: tempfile::TempDir,
}

pub fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(Dirs::at(dir.path().join("dataman")), Arc::new(MemorySecrets::default())).unwrap();
    Env { core, _dir: dir }
}

fn addr(var: &str, default: &str) -> (String, u16) {
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (h, p) = v.rsplit_once(':').unwrap();
    (h.into(), p.parse().unwrap())
}

pub fn pg_profile() -> ConnectionProfile {
    let (host, port) = addr("DATAMAN_PG", "127.0.0.1:55432");
    ConnectionProfile { host, port, user: "postgres".into(), database: "dataman".into(), sslmode: SslMode::Disable, ..ConnectionProfile::new("pg") }
}

pub fn pg_password() -> String {
    std::env::var("DATAMAN_PG_PASSWORD").unwrap_or_else(|_| "dataman".into())
}

fn connect(e: &Env, p: ConnectionProfile, pw: Option<&str>) -> Option<Arc<Connection>> {
    match e.core.block_on(e.core.connect(p.clone(), pw.map(str::to_string))) {
        Ok(c) => Some(c),
        Err(err) => {
            eprintln!("skipping: cannot reach {} ({err:#})", p.summary());
            None
        }
    }
}

pub fn pg(e: &Env) -> Option<Arc<Connection>> {
    connect(e, pg_profile(), Some(&pg_password()))
}

pub fn mysql_profile(database: &str) -> ConnectionProfile {
    let (host, port) = addr("DATAMAN_MYSQL", "127.0.0.1:33306");
    ConnectionProfile {
        host,
        port,
        user: "root".into(),
        database: database.into(),
        sslmode: SslMode::Disable,
        engine: dataman_core::config::Engine::MySql,
        ..ConnectionProfile::new("mysql")
    }
}

pub fn mysql_password() -> String {
    std::env::var("DATAMAN_MYSQL_PASSWORD").unwrap_or_else(|_| "dataman-root".into())
}

pub fn mysql_on(e: &Env, p: ConnectionProfile) -> Option<Arc<Connection>> {
    connect(e, p, Some(&mysql_password()))
}

pub fn mysql_seeded(e: &Env, database: &str) -> Option<Arc<Connection>> {
    let c = mysql_on(e, mysql_profile(""))?;
    let seed = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../seeds/mysql.sql")).unwrap().replace("dataman_test", database);
    let d = dataman_core::sql::Dialect::MySql;
    let stmts: Vec<&str> = dataman_core::sql::split_in(&seed, d).into_iter().map(|s| &seed[s.range]).collect();
    assert!(stmts.len() > 15, "{}", stmts.len());
    exec(e, &c, &stmts);
    mysql_on(e, mysql_profile(database))
}

/// A vtgate (vttestserver's MySQL port is its `--port` + 3) on the keyspace
/// `DATAMAN_VITESS_KEYSPACE` (`dataman`). vttestserver takes any password.
pub fn vitess_profile(var: &str, default: &str) -> ConnectionProfile {
    let (host, port) = addr(var, default);
    let keyspace = std::env::var("DATAMAN_VITESS_KEYSPACE").unwrap_or_else(|_| "dataman".into());
    ConnectionProfile {
        host,
        port,
        user: "root".into(),
        database: keyspace,
        sslmode: SslMode::Disable,
        engine: dataman_core::config::Engine::MySql,
        ..ConnectionProfile::new("vitess")
    }
}

pub fn connect_with(e: &Env, p: ConnectionProfile, password: Option<&str>) -> Option<Arc<Connection>> {
    connect(e, p, password)
}

pub fn mssql_profile(database: &str) -> ConnectionProfile {
    let (host, port) = addr("DATAMAN_MSSQL", "127.0.0.1:14330");
    ConnectionProfile {
        host,
        port,
        user: "sa".into(),
        database: database.into(),
        sslmode: SslMode::Require,
        engine: dataman_core::config::Engine::SqlServer,
        ..ConnectionProfile::new("mssql")
    }
}

pub fn mssql_password() -> String {
    std::env::var("DATAMAN_MSSQL_PASSWORD").unwrap_or_else(|_| "Dataman-sa-1!".into())
}

pub fn mssql_on(e: &Env, p: ConnectionProfile) -> Option<Arc<Connection>> {
    connect(e, p, Some(&mssql_password()))
}

pub fn mssql_seeded(e: &Env, database: &str) -> Option<Arc<Connection>> {
    let c = mssql_on(e, mssql_profile("master"))?;
    let seed = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../seeds/mssql.sql")).unwrap().replace("dataman_test", database);
    let d = dataman_core::sql::Dialect::TSql;
    let batches: Vec<&str> = dataman_core::sql::batches_in(&seed, d).into_iter().map(|s| &seed[s.range]).collect();
    assert!(batches.len() > 8, "{}", batches.len());
    exec(e, &c, &batches);
    mssql_on(e, mssql_profile(database))
}

pub fn mssql_drop(e: &Env, database: &str) {
    let Some(c) = mssql_on(e, mssql_profile("master")) else { return };
    let (lit, id) = (database.replace('\'', "''"), database.replace(']', "]]"));
    let sql = format!("IF DB_ID(N'{lit}') IS NOT NULL BEGIN ALTER DATABASE [{id}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{id}]; END");
    exec_quiet(e, &c, &[&sql]);
}

pub fn exec(e: &Env, c: &Arc<Connection>, sqls: &[&str]) {
    let s = c.session(None);
    for sql in sqls {
        let rs = s.execute(*sql);
        let ph = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
        assert_eq!(ph, Phase::Done, "{sql}: {:?}", rs.status().error);
    }
}

/// What DROP DATABASE … WITH (FORCE) does, before PostgreSQL 13 had it: the sessions on `db`
/// end (DROP DATABASE then waits a few seconds for them to go).
pub fn end_sessions_on(db: &str) -> String {
    format!("SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid()")
}

pub fn exec_quiet(e: &Env, c: &Arc<Connection>, sqls: &[&str]) {
    let s = c.session(None);
    for sql in sqls {
        let rs = s.execute(*sql);
        let _ = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await });
    }
}

pub fn query(e: &Env, c: &Arc<Connection>, sql: &str) -> Arc<dataman_core::ResultSet> {
    let s = c.session(None);
    let rs = s.execute(sql);
    rs.fetch_all();
    let ph = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
    assert_eq!(ph, Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

#[cfg(unix)]
pub fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// Windows has no `ps`: the working set, its resident memory.
#[cfg(windows)]
pub fn rss_kb() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    assert!(unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0, "{}", std::io::Error::last_os_error());
    c.WorkingSetSize as u64 / 1024
}
