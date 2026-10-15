//! MySQL's catalog and the features built on it, against a real server (`DISKMAN_MYSQL`, see
//! `common`): the explorer's levels and objects, DDL, foreign-key navigation, the table data
//! view, grid editing with conflict checks, exports and the agents' bounded statements. Skips
//! without a server.

mod common;

use common::*;
use diskman_core::bounded::{Limits, Mode, Stop};
use diskman_core::dataview::{Filter, Op, TableView};
use diskman_core::edit::{CellValue, ChangeSet, ConflictKind, Editability, KeySource, Outcome};
use diskman_core::export::{ExportFormat, ExportOptions};
use diskman_core::fknav::jump_in;
use diskman_core::schema::{ConstraintKind, FuncKind, RelKind};
use diskman_core::sql::Dialect;
use diskman_core::value::Kind;

fn run(e: &Env, c: &Arc<Connection>, sql: &str) -> Arc<diskman_core::ResultSet> {
    query(e, c, sql)
}

#[test]
fn explorer_levels_and_objects() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "diskman_t_cat") else { return };
    let db = "diskman_t_cat";
    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.iter().any(|d| d == db));
    assert!(dbs.iter().position(|d| d == "mysql") > dbs.iter().position(|d| d == db), "system databases last: {dbs:?}");
    // The database is its own schema.
    assert_eq!(e.core.block_on(c.schemas(db)).unwrap(), vec![db.to_string()]);
    let rels = e.core.block_on(c.relations(db, db)).unwrap();
    let names: Vec<(&str, RelKind)> = rels.iter().map(|r| (r.name.as_str(), r.kind)).collect();
    assert_eq!(
        names,
        vec![("customer", RelKind::Table), ("item", RelKind::Table), ("odd_dates", RelKind::Table), ("orders", RelKind::Table), ("v_order_customer", RelKind::View)]
    );
    let cols = e.core.block_on(c.columns(db, db, "customer")).unwrap();
    assert_eq!(cols[0].name, "id");
    assert!(cols[0].primary_key);
    assert_eq!(cols[0].default.as_deref(), Some("AUTO_INCREMENT"));
    let tier = cols.iter().find(|c| c.name == "tier").unwrap();
    assert_eq!(tier.type_name, "enum('bronze','silver','gold')");
    assert_eq!((tier.nullable, tier.default.as_deref()), (Some(false), Some("'bronze'")));
    assert_eq!(cols.iter().find(|c| c.name == "code").unwrap().type_name, "bigint unsigned");
    let idx = e.core.block_on(c.indexes(db, db, "orders")).unwrap();
    assert!(idx[0].primary, "{idx:?}");
    let st = idx.iter().find(|i| i.name == "orders_status_idx").unwrap();
    assert_eq!(st.columns, vec!["status", "created_at"]);
    assert_eq!(st.definition.as_deref(), Some("CREATE INDEX orders_status_idx ON diskman_t_cat.orders (status, created_at)"));

    // Routines, triggers, constraints.
    let counts = e.core.block_on(c.object_counts(db, db)).unwrap();
    assert_eq!((counts.functions, counts.triggers, counts.sequences), (Some(2), Some(1), None));
    let fs = e.core.block_on(c.functions(db, db)).unwrap();
    let p = fs.iter().find(|f| f.name == "customer_orders").unwrap();
    assert_eq!((p.kind, p.identity.as_str(), p.arg_types.clone()), (FuncKind::Procedure, "IN c int", vec!["int".to_string()]));
    let f = fs.iter().find(|f| f.name == "customer_total").unwrap();
    assert_eq!((f.kind, f.result.as_deref()), (FuncKind::Function, Some("decimal(12,2)")));
    let def = e.core.block_on(c.routine_definition(db, db, f)).unwrap().unwrap();
    assert!(def.contains("FUNCTION `customer_total`") && def.ends_with(';'), "{def}");
    let tr = e.core.block_on(c.triggers(db, db)).unwrap();
    assert_eq!((tr[0].name.as_str(), tr[0].table.as_str(), tr[0].events.as_str()), ("orders_created", "orders", "before insert"));
    assert!(tr[0].definition.as_deref().unwrap().contains("SET NEW.created_at = CURRENT_TIMESTAMP"));
    let x = e.core.block_on(c.table_extras(db, db, "item")).unwrap();
    let cons = x.constraints.unwrap();
    let fk = cons.iter().find(|c| c.kind == ConstraintKind::Foreign).unwrap();
    assert_eq!(fk.definition.as_deref(), Some("FOREIGN KEY (order_id) REFERENCES orders(id) ON DELETE CASCADE"));
    assert!(cons.iter().any(|c| c.kind == ConstraintKind::Primary && c.definition.as_deref() == Some("PRIMARY KEY (order_id, line)")));
    let stats = e.core.block_on(c.rel_stats(db, db)).unwrap();
    assert!(stats.get("customer").is_some_and(|s| s.bytes.is_some()));

    // DDL from the server itself.
    let ddl = e.core.block_on(c.table_ddl(db, db, "orders")).unwrap();
    assert!(ddl.starts_with("-- diskman_t_cat.orders: generated from the catalog of diskman_t_cat\nCREATE TABLE `orders`"), "{ddl}");
    assert!(ddl.contains("CONSTRAINT `orders_customer_fk` FOREIGN KEY"));
    let v = e.core.block_on(c.view_definition(db, db, "v_order_customer")).unwrap().unwrap();
    assert!(v.contains("VIEW `v_order_customer` AS select"), "{v}");

    // Autocomplete preload: the database's columns, with MySQL's type names.
    e.core.block_on(c.preload(db)).unwrap();
    let snap = c.schema.snapshot(db);
    assert_eq!(snap.dialect, Dialect::MySql);
    let r = snap.resolve(None, "Orders").expect("case-insensitive");
    assert!(snap.columns_of(r).iter().any(|c| c.name == "total" && c.type_name == "decimal(12,2)"));
    // The explorer's columns after the preload still carry the key.
    let cols = e.core.block_on(c.columns(db, db, "orders")).unwrap();
    assert!(cols.iter().any(|c| c.name == "id" && c.primary_key), "{cols:?}");
    let cat = e.core.block_on(c.catalog(db)).unwrap();
    assert_eq!(cat.functions_named(None, "customer_total").count(), 1);
    exec_quiet(&e, &c, &["DROP DATABASE diskman_t_cat"]);
}

#[test]
fn foreign_keys_and_the_data_view() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "diskman_t_fk") else { return };
    let db = "diskman_t_fk";
    let g = e.core.block_on(c.relationships(db, db)).unwrap();
    let l = g.links.iter().find(|l| l.from_table == "item").unwrap();
    assert_eq!((l.from_columns.clone(), l.to_table.as_str(), l.to_columns.clone()), (vec!["order_id".to_string()], "orders", vec!["id".to_string()]));

    // References named in the result itself: joins work.
    let rs = run(&e, &c, "SELECT i.product, i.order_id, o.customer_id, c.name FROM item i JOIN orders o ON o.id = i.order_id JOIN customer c ON c.id = o.customer_id ORDER BY i.order_id, i.line");
    let fk = e.core.block_on(c.foreign_keys(db, &rs)).unwrap();
    assert_eq!(fk.flags(), vec![false, true, true, false]);
    let snap = rs.snapshot();
    let t = &fk.targets(2)[0];
    let j = jump_in(t, &[(Kind::Int, snap.cell(0, 2))], Dialect::MySql).unwrap();
    assert_eq!(j.sql, "SELECT * FROM diskman_t_fk.customer WHERE id = 1;");
    assert_eq!(run(&e, &c, &j.sql).rows(), 1);

    // The data view: paging, filters, sort, counts.
    let key = e.core.block_on(c.primary_key(db, db, "item")).unwrap();
    assert_eq!(key, vec!["order_id", "line"]);
    let mut v = TableView::new(db, "customer", vec!["id".into()], 2);
    v.dialect = Dialect::MySql;
    v.page = 1;
    let page = run(&e, &c, &v.sql());
    assert_eq!(page.snapshot().text(0, 0).as_deref(), Some("3"));
    v.page = 0;
    v.filters.push(Filter::new("tier", Kind::Text, Op::In, "silver, gold"));
    v.filters.push(Filter::new("profile", Kind::Json, Op::NotNull, ""));
    v.filters.push(Filter::new("name", Kind::Text, Op::Contains, "o'n"));
    v.sort = Some(("balance".into(), diskman_core::result::SortDir::Desc));
    let f = run(&e, &c, &v.sql());
    assert_eq!((f.rows(), f.snapshot().text(0, 1).as_deref()), (1, Some("Diego O'Neil")));
    let n = e.core.block_on(c.count_rows(db, v.count_sql()).task).unwrap();
    assert_eq!(n, 1);
    let est = e.core.block_on(c.estimate_rows(db, db, "customer")).unwrap();
    assert!(est.is_some(), "TABLE_ROWS after ANALYZE");
    let j = Filter::new("profile", Kind::Json, Op::Eq, "{\"city\": \"Denver\"}").sql_in(Dialect::MySql);
    assert_eq!(run(&e, &c, &format!("SELECT id FROM customer WHERE {j}")).snapshot().text(0, 0).as_deref(), Some("2"));
    // Enum labels for the editor, from the column's type.
    let rs = run(&e, &c, "SELECT id, tier, tags FROM customer");
    assert_eq!(*e.core.block_on(c.result_enum_labels(db, &rs, 1)).unwrap(), vec!["bronze", "silver", "gold"]);
    assert!(e.core.block_on(c.result_enum_labels(db, &rs, 2)).unwrap().is_empty(), "SET is not an enum");
    exec_quiet(&e, &c, &["DROP DATABASE diskman_t_fk"]);
}

#[test]
fn grid_edits_apply_in_a_transaction() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "diskman_t_edit") else { return };
    let db = "diskman_t_edit";
    let rs = run(&e, &c, "SELECT * FROM customer ORDER BY id");
    let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    assert_eq!((t.key_source, t.key[0].name.as_str(), t.dialect), (KeySource::PrimaryKey, "id", Dialect::MySql));
    let snap = rs.snapshot();
    let col = |n: &str| rs.columns().unwrap().iter().position(|c| c.name == n).unwrap();
    let mut cs = ChangeSet::new();
    // Text with a quote and a backslash, an enum, bytes, a datetime, NULL.
    cs.edit(0, col("name"), CellValue::text("Ana Smith"), CellValue::text("Zoë d'Arcy \\ S."));
    cs.edit(1, col("tier"), CellValue::text("silver"), CellValue::text("gold"));
    cs.edit(1, col("photo"), CellValue::Null, CellValue::text("\\xcafe"));
    cs.edit(2, col("created"), CellValue::text("2026-03-20 18:45:10"), CellValue::text("2026-03-21 08:00:00.125"));
    cs.edit(3, col("code"), CellValue::text("9223372036854775808"), CellValue::Null);
    let i = cs.insert_row(snap.columns.len());
    cs.set_inserted(i, col("name"), Some(CellValue::text("New")));
    let plan = cs.plan(&t, &snap).unwrap();
    assert!(plan[1].sql.contains("photo = X'cafe'") && plan[1].sql.contains("tier <=> 'silver'"), "{}", plan[1].sql);
    let r = e.core.block_on(c.apply_changes(Some(db), plan)).unwrap();
    assert!(r.committed, "{r:?}");
    let after = run(&e, &c, "SELECT name, tier, HEX(photo), created, code FROM customer ORDER BY id");
    let a = after.snapshot();
    assert_eq!(a.text(0, 0).as_deref(), Some("Zoë d'Arcy \\ S."));
    assert_eq!((a.text(1, 1).as_deref(), a.text(1, 2).as_deref()), (Some("gold"), Some("CAFE")));
    assert_eq!(a.text(2, 3).as_deref(), Some("2026-03-21 08:00:00.125"));
    assert_eq!(a.text(3, 4), None);
    assert_eq!(a.rows(), 6);

    // A conflict (the row changed since it was read) rolls everything back.
    let stale = rs.snapshot();
    let mut cs = ChangeSet::new();
    cs.edit(4, col("balance"), CellValue::text("12.00"), CellValue::text("13.00"));
    cs.edit(0, col("balance"), CellValue::text("1520.50"), CellValue::text("0"));
    // Row 0's name changed above: the update guarded on it… only balance is checked here, so
    // change balance behind the grid's back.
    query(&e, &c, "UPDATE customer SET balance = 1 WHERE id = 1");
    let r = e.core.block_on(c.apply_changes(Some(db), cs.plan(&t, &stale).unwrap())).unwrap();
    assert!(!r.committed);
    assert!(r.outcomes.iter().any(|o| matches!(&o.outcome, Outcome::Conflict { kind: ConflictKind::Changed { .. }, .. })), "{r:?}");
    assert_eq!(run(&e, &c, "SELECT balance FROM customer WHERE id = 5").snapshot().text(0, 0).as_deref(), Some("12.00"), "rolled back");

    // A join is read-only; a view too.
    let j = run(&e, &c, "SELECT o.id, c.name FROM orders o JOIN customer c ON c.id = o.customer_id");
    assert!(matches!(e.core.block_on(c.edit_target(db, &j, None)).unwrap(), Editability::ReadOnly(_)));
    let v = run(&e, &c, "SELECT * FROM v_order_customer");
    assert!(matches!(e.core.block_on(c.edit_target(db, &v, None)).unwrap(), Editability::ReadOnly(_)));
    // A composite key.
    let it = run(&e, &c, "SELECT * FROM `item`");
    match e.core.block_on(c.edit_target(db, &it, None)).unwrap() {
        Editability::Editable(t) => assert_eq!(t.key.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), vec!["order_id", "line"]),
        Editability::ReadOnly(why) => panic!("{why}"),
    }
    exec_quiet(&e, &c, &["DROP DATABASE diskman_t_edit"]);
}

#[test]
fn exports_and_bounded_statements() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "diskman_t_mcp") else { return };
    let db = "diskman_t_mcp";
    let dir = tempfile::tempdir().unwrap();
    // A query export through the binary protocol, then SQL INSERTs in MySQL's quoting.
    let path = dir.path().join("c.csv");
    let x = c.export_query(Some(db), "SELECT id, name, balance, created, birth_date FROM customer ORDER BY id", &path, ExportOptions::new(ExportFormat::Csv), None);
    let s = e.core.block_on(x.task).unwrap();
    assert_eq!(s.rows, 5);
    let csv = std::fs::read_to_string(&path).unwrap();
    assert!(csv.lines().nth(1).unwrap().starts_with("1,Ana Smith,1520.50,2026-01-05 09:15:00.25,1990-04-12"), "{csv}");
    let rs = query(&e, &c, "SELECT id, name, photo FROM diskman_t_mcp.customer ORDER BY id");
    let mut o = ExportOptions::new(ExportFormat::SqlInsert);
    o.table = "diskman_t_mcp.copied".into();
    o.dialect = Dialect::MySql;
    let p = dir.path().join("c.sql");
    e.core.block_on(diskman_core::export::export_result(c.handle(), rs, &p, o).task).unwrap();
    let sql = std::fs::read_to_string(&p).unwrap();
    assert!(sql.contains("'Eva \\\\ Brown'") && sql.contains("X'89504e47'"), "{sql}");
    query(&e, &c, "CREATE TABLE diskman_t_mcp.copied (id INT, name VARCHAR(80), photo BLOB)");
    for st in diskman_core::sql::split_in(&sql, Dialect::MySql) {
        query(&e, &c, &sql[st.range]);
    }
    let back = query(&e, &c, "SELECT name FROM diskman_t_mcp.copied WHERE id = 5");
    assert_eq!(back.snapshot().text(0, 0).as_deref(), Some("Eva \\ Brown"), "round trip");

    // Agents: a bounded read (rows capped), a refused write, a write in a transaction.
    let lim = Limits { max_rows: 2, ..Default::default() };
    let never = std::future::pending::<()>();
    let o = e.core.block_on(c.bounded(Some(db), "SELECT id, name, profile FROM customer ORDER BY id", vec![], Mode::Read, lim, never)).unwrap();
    assert_eq!(o.rows.len(), 2);
    assert_eq!(o.rows[0][2], serde_json::json!({"city": "Boston", "points": 120}));
    assert!(o.truncated.is_some());
    let r = e.core.block_on(c.bounded(Some(db), "DELETE FROM customer", vec![], Mode::Read, Limits::default(), std::future::pending::<()>()));
    assert!(matches!(r, Err(Stop::Refused(_))));
    let o = e.core.block_on(c.bounded(Some(db), "UPDATE orders SET notes = ? WHERE id = ?", vec![Some("x".into()), Some("2".into())], Mode::Write, Limits::default(), std::future::pending::<()>())).unwrap();
    assert_eq!(o.statements[0].affected, Some(1));
    let o = e.core.block_on(c.bounded(Some(db), "SELECT notes FROM orders WHERE id = ?", vec![Some("2".into())], Mode::Read, Limits::default(), std::future::pending::<()>())).unwrap();
    assert_eq!(o.rows[0][0], serde_json::json!("x"));
    exec_quiet(&e, &c, &["DROP DATABASE diskman_t_mcp"]);
}
