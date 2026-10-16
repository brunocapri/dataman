//! A script run the way a query tab runs it: one session, one statement after the other,
//! stopping at the first error; a statement that pauses before the end of the script keeps the
//! rows it loaded (`stop_fetching`) and the next one runs. Skips without the PostgreSQL server.

mod common;

use common::*;
use dataman_core::ResultSet;
use dataman_core::batch::{self, Key, Kind, Outcome, State};
use dataman_core::result::PREFETCH;

/// Runs `stmts` like the tab does; `None` for the statements after an error.
fn run_script(e: &Env, s: &Arc<dataman_core::Session>, stmts: &[&str]) -> Vec<Option<Arc<ResultSet>>> {
    let mut out = vec![None; stmts.len()];
    for (i, sql) in stmts.iter().enumerate() {
        let rs = s.execute(*sql);
        let last = i + 1 == stmts.len();
        let phase = e.core.block_on(async {
            tokio::time::timeout(Duration::from_secs(60), async {
                let mut rx = rs.subscribe();
                loop {
                    let p = rs.phase();
                    if p.is_finished() || (p == Phase::Paused && last) {
                        return p;
                    }
                    if p == Phase::Paused {
                        rs.stop_fetching();
                    }
                    if rx.changed().await.is_err() {
                        return rs.phase();
                    }
                }
            })
            .await
            .expect("timeout")
        });
        out[i] = Some(rs);
        if phase == Phase::Failed {
            break;
        }
    }
    out
}

fn tabs(stmts: &[&str], results: &[Option<Arc<ResultSet>>]) -> Vec<batch::Tab> {
    let v: Vec<(&str, Option<Outcome>)> = stmts.iter().zip(results).map(|(s, r)| (*s, r.as_deref().map(Outcome::of))).collect();
    batch::tabs(&v)
}

#[test]
fn four_statement_script() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    let script = [
        "CREATE TEMP TABLE batch_t AS SELECT g AS id, md5(g::text) AS v FROM generate_series(1, 60000) g",
        "SELECT * FROM batch_t ORDER BY id",
        "UPDATE batch_t SET v = 'x' WHERE id <= 3",
        "SELECT * FROM batch_nope",
        "SELECT 1",
    ];
    let results = run_script(&e, &s, &script);
    let t = tabs(&script, &results);
    let names: Vec<String> = t.iter().map(|t| format!("{}{}", t.number.map(|n| format!("{n} · ")).unwrap_or_default(), t.label)).collect();
    assert_eq!(names, ["2 · batch_t", "3 · UPDATE 3", "4 · batch_nope", "messages"]);
    assert_eq!(t.iter().map(|t| t.kind).collect::<Vec<_>>(), [Kind::Rows, Kind::Affected, Kind::Error, Kind::Command]);
    assert_eq!(t[3].folded, 1, "CREATE TABLE AS folds");
    assert_eq!(batch::default_active(&t), Some(Key::Stmt(3)), "the error is shown");
    assert!(results[4].is_none(), "nothing runs after the error");

    // Statement 2 paused before the end: it kept one prefetch window, the rest was dropped.
    let big = results[1].as_ref().unwrap();
    assert_eq!(t[0].state, State::Partial);
    assert_eq!(big.phase(), Phase::Done);
    assert!(big.rows() >= PREFETCH && big.rows() < 60_000, "{}", big.rows());
    assert_eq!(big.snapshot().text(0, 0).as_deref(), Some("1"));
    let err = results[3].as_ref().unwrap().status().error.unwrap();
    assert_eq!(err.code.as_deref(), Some("42P01"));

    // The history has every statement that ran, once.
    let h = e.core.history.load(&c.profile.id, 20).unwrap();
    assert_eq!(h.len(), 4, "{h:?}");
}

#[test]
fn stopping_a_paused_result_keeps_the_transaction() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    let script = [
        "BEGIN",
        "CREATE TEMP TABLE batch_tx (id int)",
        "SELECT g FROM generate_series(1, 100000) g",
        "INSERT INTO batch_tx VALUES (1), (2)",
        "SELECT count(*) FROM batch_tx",
        "ROLLBACK",
        "SELECT g FROM generate_series(1, 100000) g",
    ];
    let results = run_script(&e, &s, &script);
    for (sql, r) in script.iter().zip(&results) {
        let r = r.as_ref().unwrap();
        assert_ne!(r.phase(), Phase::Failed, "{sql}: {:?}", r.status().error);
    }
    // No CancelRequest: the INSERT and the count ran inside the same transaction.
    let count = results[4].as_ref().unwrap();
    assert_eq!(count.snapshot().text(0, 0).as_deref(), Some("2"));
    let t = tabs(&script, &results);
    let names: Vec<&str> = t.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(names, ["SELECT", "INSERT 2", "batch_tx", "SELECT", "messages"]);
    assert_eq!(t[0].state, State::Partial);
    // The last statement stays paused with its stream open (Fetch next / Fetch all).
    assert_eq!(t[3].state, State::Paused);
    assert_eq!(t[4].folded, 3);
    results[6].as_ref().unwrap().fetch_all();
    let last = results[6].as_ref().unwrap();
    let ph = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), last.wait_finished()).await.expect("timeout") });
    assert_eq!(ph, Phase::Done);
    assert_eq!(last.rows(), 100_000);
}
