//! PostgreSQL and the servers that speak its protocol. See live_versions.rs.

use super::*;

#[test]
fn aggregations_and_odd_queries_come_back_whole() {
    let Some(t) = target(Engine::Postgres) else { return };
    let rows = t.rows(
        "SELECT g, sum(x), count(*) FILTER (WHERE x % 2 = 0), string_agg(x::text, ',' ORDER BY x DESC)
         FROM (SELECT x, x % 3 AS g FROM generate_series(1, 10) x) s GROUP BY ROLLUP (g) ORDER BY g NULLS LAST",
    );
    let want = [["0", "18", "1", "9,6,3"], ["1", "22", "2", "10,7,4,1"], ["2", "15", "2", "8,5,2"]];
    for (got, want) in rows.iter().zip(want) {
        assert_eq!(got.iter().map(|c| c.as_deref().unwrap()).collect::<Vec<_>>(), want);
    }
    assert_eq!(rows[3], [None, Some("55".into()), Some("5".into()), Some("10,9,8,7,6,5,4,3,2,1".into())]);
}
