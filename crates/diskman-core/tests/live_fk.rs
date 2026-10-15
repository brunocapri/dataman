//! Live tests of foreign-key navigation (see `common` for the servers): which result columns are
//! references, on PostgreSQL (origins + `pg_constraint`, joins included, a composite key in a
//! scratch schema) and on LAR (inferred links, single-table SELECTs only).

mod common;
use common::*;
use diskman_core::fknav::{ForeignKeys, jump};

fn fks(e: &Env, c: &Arc<Connection>, sql: &str) -> (Arc<diskman_core::ResultSet>, Arc<ForeignKeys>) {
    let s = c.session(None);
    let rs = s.execute(sql);
    let fk = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), c.foreign_keys(c.database(), &rs)).await.expect("timeout") }).unwrap();
    let ph = e.core.block_on(rs.wait_finished());
    assert_eq!(ph, Phase::Done, "{sql}: {:?}", rs.status().error);
    (rs, fk)
}

fn jump_at(rs: &diskman_core::ResultSet, fk: &ForeignKeys, row: usize, col: usize) -> Option<diskman_core::fknav::FkJump> {
    let snap = rs.snapshot();
    let t = fk.targets(col).first()?;
    let values: Vec<_> = t.from_cols.iter().map(|&c| (snap.columns[c].kind, snap.cell(row, c))).collect();
    jump(t, &values)
}

#[test]
fn pg_references_from_origins() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    // A join: each column has its own table, so `o.customer` is still a reference.
    let (rs, fk) = fks(&e, &c, "SELECT o.id, o.customer, c.name, o.total * 2 AS doubled FROM orders o JOIN customer c ON c.id = o.customer ORDER BY o.id LIMIT 5");
    assert_eq!(fk.flags(), vec![false, true, false, false]);
    let j = jump_at(&rs, &fk, 0, 1).unwrap();
    let id = rs.snapshot().text(0, 1).unwrap().into_owned();
    assert_eq!(j.sql, format!("SELECT * FROM public.customer WHERE id = {id};"));
    assert_eq!(j.title, format!("customer · id = {id}"));
    // The statement it builds runs and finds the row.
    let s = c.session(None);
    let r2 = s.execute(j.sql.trim_end_matches(';'));
    assert_eq!(e.core.block_on(r2.wait_finished()), Phase::Done);
    assert_eq!(r2.rows(), 1);
    // No table columns: nothing.
    let (_, fk) = fks(&e, &c, "SELECT 1 AS customer, now()");
    assert!(fk.is_empty());
    // A statement without columns.
    let (_, fk) = fks(&e, &c, "SET search_path TO public");
    assert!(fk.is_empty());
}

#[test]
fn pg_composite_key_in_a_scratch_schema() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = "diskman_fk_scratch";
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {s} CASCADE")]);
    exec(
        &e,
        &c,
        &[
            &format!("CREATE SCHEMA {s}"),
            &format!("CREATE TABLE {s}.\"Line\" (order_id int, n int, note text, PRIMARY KEY (order_id, n))"),
            &format!("CREATE TABLE {s}.adjustment (id int PRIMARY KEY, n int, order_id int, FOREIGN KEY (order_id, n) REFERENCES {s}.\"Line\" (order_id, n))"),
            &format!("INSERT INTO {s}.\"Line\" VALUES (7, 1, 'a'), (7, 2, 'b')"),
            &format!("INSERT INTO {s}.adjustment VALUES (1, 2, 7), (2, NULL, 7)"),
        ],
    );
    let (rs, fk) = fks(&e, &c, &format!("SELECT * FROM {s}.adjustment ORDER BY id"));
    assert_eq!(fk.flags(), vec![false, true, true]);
    let j = jump_at(&rs, &fk, 0, 1).unwrap();
    assert_eq!(j.sql, format!("SELECT * FROM {s}.\"Line\" WHERE order_id = 7 AND n = 2;"));
    assert_eq!(j.label, "Go to \"Line\"");
    // A NULL part: nothing to go to.
    assert!(jump_at(&rs, &fk, 1, 2).is_none());
    // Half of the key in the result: not a reference.
    let (_, fk) = fks(&e, &c, &format!("SELECT id, n FROM {s}.adjustment"));
    assert!(fk.is_empty());
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {s} CASCADE")]);
}

#[test]
fn lar_inferred_links() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    // Needs `demo_orders.demo_customer → demo_customers` (the scratch LAR's demo tables).
    let (rs, fk) = fks(&e, &c, "SELECT * FROM demo_orders LIMIT 3");
    let cols = rs.columns().unwrap();
    let Some(ci) = cols.iter().position(|c| c.name == "demo_customer") else {
        eprintln!("skipping: no demo_orders.demo_customer on this LAR");
        return;
    };
    assert!(fk.is_fk(ci), "{:?}", fk.flags());
    let t = &fk.targets(ci)[0];
    assert!(t.inferred);
    let j = jump_at(&rs, &fk, 0, ci).unwrap();
    assert!(j.sql.starts_with("SELECT * FROM public.demo_customers WHERE id = "), "{}", j.sql);
    assert!(j.detail.ends_with("· inferred"));
    let s = c.session(None);
    let r2 = s.execute(j.sql.trim_end_matches(';'));
    assert_eq!(e.core.block_on(r2.wait_finished()), Phase::Done);
    assert_eq!(r2.rows(), 1);
    // A join has no resolvable columns on LAR.
    let (_, fk) = fks(&e, &c, "SELECT o.demo_customer, c.id FROM demo_orders o JOIN demo_customers c ON c.id = o.demo_customer LIMIT 1");
    assert!(fk.is_empty());
}
