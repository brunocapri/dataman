mod common;

use common::*;
use dataman_core::ResultSet;

fn estimate(e: &Env, c: &Arc<Connection>, db: &str, sql: &str) -> Option<u64> {
    let rs = ResultSet::new(sql);
    c.ask_row_estimate(db, &rs);
    let mut rx = rs.subscribe();
    e.core.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            while !rs.estimate_known() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        assert!(rs.estimate_known(), "no answer for {sql}");
    });
    rs.row_estimate()
}

fn near(est: Option<u64>, real: u64) -> bool {
    est.is_some_and(|x| x as f64 >= real as f64 / 3. && x as f64 <= real as f64 * 3. + 10.)
}

#[test]
fn postgres_estimates_a_full_table_select() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec(&e, &c, &["DROP TABLE IF EXISTS public.dataman_est", "CREATE TABLE public.dataman_est AS SELECT g AS id FROM generate_series(1, 30000) g", "ANALYZE public.dataman_est"]);
    let db = c.database().to_string();
    let est = estimate(&e, &c, &db, "SELECT * FROM public.dataman_est");
    assert!(near(est, 30_000), "{est:?}");
    assert_eq!(estimate(&e, &c, &db, "DELETE FROM public.dataman_est"), None);
    exec(&e, &c, &["DROP TABLE public.dataman_est"]);
}

#[test]
fn mysql_estimates_a_seed_table() {
    let e = env();
    let db = "dataman_t_est";
    let Some(c) = mysql_seeded(&e, db) else { return };
    exec(&e, &c, &["ANALYZE TABLE customer"]);
    let est = estimate(&e, &c, db, "SELECT * FROM customer");
    assert!(near(est, 5), "{est:?}");
    exec_quiet(&e, &c, &["DROP DATABASE dataman_t_est"]);
}

#[test]
fn sqlserver_estimates_with_showplan() {
    let e = env();
    let db = "dataman_t_est";
    let Some(c) = mssql_seeded(&e, db) else { return };
    let real: u64 = query(&e, &c, "SELECT COUNT(*) FROM dbo.orders").snapshot().text(0, 0).and_then(|s| s.parse().ok()).unwrap_or(0);
    let est = estimate(&e, &c, db, "SELECT * FROM dbo.orders");
    assert!(near(est, real), "{est:?} vs {real}");
    assert!(near(estimate(&e, &c, db, "SELECT id FROM dbo.orders"), real));
    assert_eq!(estimate(&e, &c, db, "EXEC dbo.customer_orders @customer = 1"), None);
    drop(c);
    mssql_drop(&e, db);
}
