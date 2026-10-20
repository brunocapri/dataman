mod common;
use common::*;

fn finish(e: &Env, rs: &Arc<dataman_core::ResultSet>) -> Phase {
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), rs.wait_finished()).await.expect("timeout") })
}

#[test]
fn agent_reads_postgres() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    let rs = s.execute_agent("SELECT 1 AS one", vec![], "claude-code");
    assert_eq!(finish(&e, &rs), Phase::Done, "{:?}", rs.status().error);
    assert_eq!((rs.agent(), rs.rows()), (Some("claude-code"), 1));
    let rs = s.execute_agent("CREATE TEMP TABLE agent_probe (a int)", vec![], "claude-code");
    assert_eq!(finish(&e, &rs), Phase::Failed);
    assert_eq!(rs.status().error.unwrap().code.as_deref(), Some("25006"));
    let b = s.execute("BEGIN");
    finish(&e, &b);
    let rs = s.execute_agent("SELECT 2", vec![], "claude-code");
    assert_eq!(finish(&e, &rs), Phase::Failed);
    assert!(rs.status().error.unwrap().message.contains("transaction is open"));
    let r = s.execute("ROLLBACK");
    finish(&e, &r);
    let rs = s.execute_agent("SELECT 3", vec![], "claude-code");
    assert_eq!(finish(&e, &rs), Phase::Done);
    assert!(!s.tx().status.is_open());
    let h = e.core.history.load(&c.profile.id, 10).unwrap();
    let mine: Vec<(&str, Option<&str>)> = h.iter().map(|x| (x.sql.as_str(), x.source.as_deref())).collect();
    assert!(mine.contains(&("SELECT 3", Some("agent"))) && mine.contains(&("BEGIN", None)), "{mine:?}");
}

#[test]
fn agent_reads_mysql() {
    let e = env();
    let Some(c) = mysql_on(&e, mysql_profile("mysql")) else { return };
    let s = c.session(None);
    let rs = s.execute_agent("SELECT 1 AS one", vec![], "claude-code");
    assert_eq!(finish(&e, &rs), Phase::Done, "{:?}", rs.status().error);
    assert_eq!(rs.agent(), Some("claude-code"));
    assert!(!s.tx().status.is_open());
}
