//! The engines' older releases and the servers that speak their protocols, one at a time:
//! `scripts/versions.sh` starts each service of `e2e/versions/*.yaml` and runs this file against
//! it with `DATAMAN_VERSIONS_URL` (its connection string, as pasted in the connection sheet) and
//! `DATAMAN_VERSIONS_EXPECT` (what its version starts with). Without the URL every test returns.
//! With it, a server that does not answer, or is not the one expected, fails the run: a matrix
//! that reached nothing must never pass. Each module tests one engine and returns on the others.

mod common;
#[path = "versions/ch.rs"]
mod ch;
#[path = "versions/files.rs"]
mod files;
#[path = "versions/mongo.rs"]
mod mongo;
#[path = "versions/ms.rs"]
mod ms;
#[path = "versions/my.rs"]
mod my;
#[path = "versions/pg.rs"]
mod pg;
#[path = "versions/redis.rs"]
mod redis;
#[path = "versions/search.rs"]
mod search;

use common::*;
use dataman_core::nosql::{Output, Page, Ran};
use dataman_core::result::ErrorInfo;
use dataman_core::{Engine, ResultSet};

pub struct Target {
    pub e: Env,
    pub c: Arc<Connection>,
}

/// The server under test when it is of `engine`; None outside the matrix or on another engine.
pub fn target(engine: Engine) -> Option<Target> {
    let t = reach()?;
    (t.c.engine() == engine).then_some(t)
}

fn reach() -> Option<Target> {
    let Ok(url) = std::env::var("DATAMAN_VERSIONS_URL") else {
        eprintln!("skipping: DATAMAN_VERSIONS_URL is not set");
        return None;
    };
    let (p, pw) = dataman_core::connstr::parse_profile(&url).unwrap_or_else(|| panic!("{url} does not read as a connection string"));
    let e = env();
    let c = e.core.block_on(e.core.connect(p.clone(), pw)).unwrap_or_else(|err| panic!("cannot reach {}: {err:#}", p.summary()));
    if let Ok(want) = std::env::var("DATAMAN_VERSIONS_EXPECT") {
        assert!(c.info.version.starts_with(&want), "{url} is {:?}, not {want}…", c.info.version);
    }
    Some(Target { e, c })
}

impl Target {
    pub fn run(&self, sql: &str) -> Arc<ResultSet> {
        let rs = self.c.session(None).execute(sql);
        rs.fetch_all();
        self.e.core.block_on(async { tokio::time::timeout(Duration::from_secs(120), rs.wait_finished()).await.expect("timeout") });
        rs
    }

    /// Ran, and every value decoded: a value that did not shows as NULL, which no test would see.
    pub fn ok(&self, sql: &str) -> Arc<ResultSet> {
        let rs = self.run(sql);
        assert_eq!(rs.phase(), Phase::Done, "{sql}: {:?}", rs.status().error);
        let (messages, _) = rs.messages();
        assert!(!messages.iter().any(|m| m.contains("did not decode")), "{sql}: {messages:?}");
        rs
    }

    pub fn failed(&self, sql: &str) -> ErrorInfo {
        let rs = self.run(sql);
        assert_eq!(rs.phase(), Phase::Failed, "{sql} ran");
        rs.status().error.expect("an error")
    }

    /// Every cell as the grid shows it.
    pub fn rows(&self, sql: &str) -> Vec<Vec<Option<String>>> {
        let rs = self.ok(sql);
        let cols = rs.columns().map_or(0, |c| c.len());
        let s = rs.snapshot();
        (0..s.rows()).map(|r| (0..cols).map(|c| s.text(r, c).map(|t| t.into_owned())).collect()).collect()
    }

    pub fn one(&self, sql: &str) -> Option<String> {
        let rows = self.rows(sql);
        assert_eq!(rows.len(), 1, "{sql}: {rows:?}");
        rows[0][0].clone()
    }

    /// A MongoDB, search or Redis statement on `db` (a database, a Redis database's number, ""
    /// for search), one page of its result: what ran, or the message Dataman shows.
    pub fn nosql_page(&self, db: &str, text: &str, page: Page) -> Result<Ran, String> {
        let r = self.c.run_nosql(db, text, page);
        self.e.core.block_on(async { tokio::time::timeout(Duration::from_secs(120), r).await.expect("timeout") }).map_err(|e| format!("{e:#}"))
    }

    pub fn nosql(&self, db: &str, text: &str) -> Result<Output, String> {
        self.nosql_page(db, text, Page::default()).map(|r| r.output)
    }

    pub fn nosql_ok(&self, db: &str, text: &str) -> Output {
        self.nosql(db, text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }
}

#[test]
fn the_server_is_the_one_asked_for() {
    let Some(t) = reach() else { return };
    assert!(t.c.info.major() > 0, "{}: no release read", t.c.info.version);
}
