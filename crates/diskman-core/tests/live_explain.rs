//! Visual EXPLAIN against real servers: the plan of a join (JSON, then analyzed with a spill),
//! writes explained with ANALYZE inside the rolled-back wrapper (alone, inside an open
//! transaction, and failing), parameters bound to an EXPLAIN, the text format, and what LAR
//! answers. Skips without servers.

mod common;

use common::*;
use diskman_core::explain::{self, Format, Plan, Rule, Target};
use diskman_core::{ResultSet, Session, TxStatus};

fn finish(e: &Env, rs: &Arc<ResultSet>) -> Phase {
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") })
}

fn run(e: &Env, s: &Arc<Session>, sql: &str) -> Arc<ResultSet> {
    let rs = s.execute(sql);
    assert_eq!(finish(e, &rs), Phase::Done, "{sql}: {:?}", rs.status().error);
    rs
}

fn plan_of(rs: &ResultSet) -> Plan {
    let snap = rs.snapshot();
    let cols: Vec<String> = snap.columns.iter().map(|c| c.name.clone()).collect();
    let rows: Vec<Vec<Option<String>>> = (0..snap.rows()).map(|r| (0..cols.len()).map(|c| snap.text(r, c).map(|s| s.into_owned())).collect()).collect();
    Plan::parse(&Plan::source_of(&cols, &rows))
}

fn count(e: &Env, s: &Arc<Session>, table: &str) -> String {
    let rs = run(e, s, &format!("SELECT count(*) FROM {table}"));
    rs.snapshot().text(0, 0).unwrap().into_owned()
}

const JOIN: &str = "SELECT c.name, o.total FROM orders o JOIN customer c ON c.id = o.customer WHERE o.id < 20000 ORDER BY c.email;";

#[test]
fn explain_and_analyze_on_postgres() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let t = Target::of(&c.info);
    assert!(!t.lar && t.major >= 12, "{t:?}");
    let s = c.session(None);
    let rs = run(&e, &s, &explain::sql(JOIN, false, t));
    let p = plan_of(&rs);
    assert_eq!(p.format, Format::Json);
    assert!(!p.analyzed && p.nodes.len() >= 3, "{:?}", p.nodes.iter().map(|n| &n.title).collect::<Vec<_>>());
    assert!(p.totals.execution_ms.is_none());
    assert!((p.nodes.iter().map(|n| n.share).sum::<f64>() - 1.).abs() < 1e-6);

    // Analyzed, with a work_mem small enough to spill the sort.
    run(&e, &s, "SET work_mem = '64kB'");
    let t0 = Instant::now();
    let rs = run(&e, &s, &explain::sql(JOIN, true, t));
    assert!(rs.sql.starts_with("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, VERBOSE, SETTINGS, WAL)"));
    assert!(rs.rolled_back() == false, "a plain session run");
    let p = plan_of(&rs);
    eprintln!("explain analyze: {} nodes in {:?}, execution {:?} ms", p.nodes.len(), t0.elapsed(), p.totals.execution_ms);
    assert!(p.analyzed);
    assert!(p.totals.execution_ms.is_some() && p.totals.planning_ms.is_some());
    assert_eq!(p.totals.setting("work_mem"), Some("64kB"));
    assert!(p.hints.iter().any(|h| matches!(h.rule, Rule::Spill | Rule::HashBatches)), "{:?}", p.hints);
    let sum: f64 = p.nodes.iter().map(|n| n.excl_ms).sum();
    let root = p.nodes[0].incl_ms;
    assert!(sum <= root + 0.01 && sum > root * 0.5, "exclusive {sum} vs root {root}");

    // The text format of the same statement: the same tree.
    run(&e, &s, "RESET work_mem");
    let rs = run(&e, &s, &explain::text_sql(JOIN, t));
    let text = plan_of(&rs);
    let json = plan_of(&run(&e, &s, &explain::sql(JOIN, false, t)));
    assert_eq!(text.format, Format::Text);
    let a: Vec<&str> = json.nodes.iter().map(|n| n.op.as_str()).collect();
    let b: Vec<&str> = text.nodes.iter().map(|n| n.op.as_str()).collect();
    assert_eq!(a, b);
}

#[test]
fn writes_explained_with_analyze_are_rolled_back() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let t = Target::of(&c.info);
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS diskman_explain CASCADE"]);
    exec(&e, &c, &["CREATE SCHEMA diskman_explain", "CREATE TABLE diskman_explain.t AS SELECT g AS id FROM generate_series(1, 100) g"]);
    let s = c.session(None);

    // Alone: a transaction of its own, rolled back.
    let rs = s.execute_opts(explain::sql("DELETE FROM diskman_explain.t WHERE id <= 60", true, t), Vec::new(), true);
    assert_eq!(finish(&e, &rs), Phase::Done, "{:?}", rs.status().error);
    assert!(rs.rolled_back());
    let p = plan_of(&rs);
    assert_eq!(p.nodes[0].op, "Delete");
    assert_eq!(p.nodes[1].act_rows(), Some(60.), "it really ran");
    assert_eq!(count(&e, &s, "diskman_explain.t"), "100", "and nothing stayed deleted");
    assert_eq!(s.tx().status, TxStatus::Idle);

    // Failing: still rolled back, no failed transaction left.
    let rs = s.execute_opts(explain::sql("UPDATE diskman_explain.t SET id = id / 0", true, t), Vec::new(), true);
    assert_eq!(finish(&e, &rs), Phase::Failed);
    assert_eq!(rs.status().error.and_then(|x| x.code).as_deref(), Some("22012"));
    assert_eq!(s.tx().status, TxStatus::Idle);
    assert_eq!(count(&e, &s, "diskman_explain.t"), "100");

    // Inside an open transaction: a savepoint; the transaction and its work survive, also
    // after a failure.
    run(&e, &s, "BEGIN");
    run(&e, &s, "INSERT INTO diskman_explain.t VALUES (1000)");
    let rs = s.execute_opts(explain::sql("DELETE FROM diskman_explain.t", true, t), Vec::new(), true);
    assert_eq!(finish(&e, &rs), Phase::Done, "{:?}", rs.status().error);
    assert_eq!(s.tx().status, TxStatus::Open);
    assert_eq!(count(&e, &s, "diskman_explain.t"), "101");
    let rs = s.execute_opts(explain::sql("UPDATE diskman_explain.t SET id = id / 0", true, t), Vec::new(), true);
    assert_eq!(finish(&e, &rs), Phase::Failed);
    assert_eq!(s.tx().status, TxStatus::Open, "the savepoint absorbed the error");
    assert_eq!(count(&e, &s, "diskman_explain.t"), "101");
    run(&e, &s, "ROLLBACK");
    assert_eq!(count(&e, &s, "diskman_explain.t"), "100");

    // A read-only connection refuses it before sending (the tab checks with this).
    assert!(diskman_core::guard::read_only_refusal(&explain::sql("DELETE FROM diskman_explain.t", true, t)).is_some());
    assert!(diskman_core::guard::read_only_refusal(&explain::sql("DELETE FROM diskman_explain.t", false, t)).is_none());
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS diskman_explain CASCADE"]);
}

#[test]
fn parameters_bind_to_the_explain() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let t = Target::of(&c.info);
    let s = c.session(None);
    let d = diskman_core::params::detect(&explain::sql("SELECT * FROM orders WHERE id = :id AND status <> $1", false, t)).expect("params");
    assert_eq!(d.params.len(), 2);
    let values = d.params.iter().map(|p| Some(if p.key == ":id" { "42" } else { "x" }.to_string())).collect();
    let rs = s.execute_opts(d.sql.clone(), values, false);
    assert_eq!(finish(&e, &rs), Phase::Done, "{:?}", rs.status().error);
    let p = plan_of(&rs);
    assert_eq!(p.nodes[0].title, "Index Scan using orders_pkey on orders");
    assert_eq!(p.nodes[0].str("Index Cond"), Some("(orders.id = '42'::bigint)"));
}

#[test]
fn lar_answers_explain() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    let t = Target::of(&c.info);
    assert!(t.lar);
    let sql = explain::sql("SELECT 1", false, t);
    assert_eq!(sql, "EXPLAIN SELECT 1", "plain EXPLAIN: LAR has no options");
    let s = c.session(None);
    for analyze in [false, true] {
        let rs = s.execute(explain::sql("SELECT 1", analyze, t));
        match finish(&e, &rs) {
            // Today: not supported, said plainly (the error view shows it).
            Phase::Failed => {
                let err = rs.status().error.unwrap();
                eprintln!("LAR EXPLAIN (analyze={analyze}): {} {}", err.code.as_deref().unwrap_or(""), err.message);
                assert_eq!(err.code.as_deref(), Some("0A000"));
            }
            // Should it answer: a text tree (or the raw text), never hints.
            Phase::Done => {
                let p = plan_of(&rs);
                assert!(matches!(p.format, Format::Text | Format::Raw));
                assert!(p.hints.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(s.tx().status, TxStatus::Idle);
}
