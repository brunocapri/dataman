mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::dataview::{Filter, Op, TableView};
use dataman_core::edit::{CellValue, ChangeSet, ConflictKind, Editability, KeySource, Outcome};
use dataman_core::export::{ExportFormat, ExportOptions};
use dataman_core::fknav::jump_in;
use dataman_core::schema::{ConstraintKind, FuncKind, RelKind};
use dataman_core::sql::Dialect;
use dataman_core::value::Kind;

fn run(e: &Env, c: &Arc<Connection>, sql: &str) -> Arc<dataman_core::ResultSet> {
    query(e, c, sql)
}

#[test]
fn explorer_levels_and_objects() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "dataman_t_cat") else { return };
    let db = "dataman_t_cat";
    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.iter().any(|d| d == db));
    assert!(dbs.iter().position(|d| d == "mysql") > dbs.iter().position(|d| d == db), "system databases last: {dbs:?}");
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
    // An integer's display width, as the server keeps it: gone from MySQL 8.0.19, never on MariaDB.
    let widths = c.info.is_mariadb() || c.info.release < (8, 0, 19);
    assert_eq!(cols.iter().find(|c| c.name == "code").unwrap().type_name, if widths { "bigint(20) unsigned" } else { "bigint unsigned" });
    let idx = e.core.block_on(c.indexes(db, db, "orders")).unwrap();
    assert!(idx[0].primary, "{idx:?}");
    let st = idx.iter().find(|i| i.name == "orders_status_idx").unwrap();
    assert_eq!(st.columns, vec!["status", "created_at"]);
    assert_eq!(st.definition.as_deref(), Some("CREATE INDEX orders_status_idx ON dataman_t_cat.orders (status, created_at)"));

    let counts = e.core.block_on(c.object_counts(db, db)).unwrap();
    assert_eq!((counts.functions, counts.triggers, counts.sequences), (Some(2), Some(1), None));
    let fs = e.core.block_on(c.functions(db, db)).unwrap();
    let p = fs.iter().find(|f| f.name == "customer_orders").unwrap();
    let int = if widths { "int(11)" } else { "int" };
    assert_eq!((p.kind, p.identity.clone(), p.arg_types.clone()), (FuncKind::Procedure, format!("IN c {int}"), vec![int.to_string()]));
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

    let ddl = e.core.block_on(c.table_ddl(db, db, "orders")).unwrap();
    assert!(ddl.starts_with("-- dataman_t_cat.orders: generated from the catalog of dataman_t_cat\nCREATE TABLE `orders`"), "{ddl}");
    assert!(ddl.contains("CONSTRAINT `orders_customer_fk` FOREIGN KEY"));
    let v = e.core.block_on(c.view_definition(db, db, "v_order_customer")).unwrap().unwrap();
    assert!(v.contains("VIEW `v_order_customer` AS select"), "{v}");

    e.core.block_on(c.preload(db)).unwrap();
    let snap = c.schema.snapshot(db);
    assert_eq!(snap.dialect, Dialect::MySql);
    let r = snap.resolve(None, "Orders").expect("case-insensitive");
    assert!(snap.columns_of(r).iter().any(|c| c.name == "total" && c.type_name == "decimal(12,2)"));
    let cols = e.core.block_on(c.columns(db, db, "orders")).unwrap();
    assert!(cols.iter().any(|c| c.name == "id" && c.primary_key), "{cols:?}");
    let cat = e.core.block_on(c.catalog(db)).unwrap();
    assert_eq!(cat.functions_named(None, "customer_total").count(), 1);
    exec_quiet(&e, &c, &["DROP DATABASE dataman_t_cat"]);
}

#[test]
fn foreign_keys_and_the_data_view() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "dataman_t_fk") else { return };
    let db = "dataman_t_fk";
    let g = e.core.block_on(c.relationships(db, db)).unwrap();
    let l = g.links.iter().find(|l| l.from_table == "item").unwrap();
    assert_eq!((l.from_columns.clone(), l.to_table.as_str(), l.to_columns.clone()), (vec!["order_id".to_string()], "orders", vec!["id".to_string()]));

    let rs = run(&e, &c, "SELECT i.product, i.order_id, o.customer_id, c.name FROM item i JOIN orders o ON o.id = i.order_id JOIN customer c ON c.id = o.customer_id ORDER BY i.order_id, i.line");
    let fk = e.core.block_on(c.foreign_keys(db, &rs)).unwrap();
    assert_eq!(fk.flags(), vec![false, true, true, false]);
    let snap = rs.snapshot();
    let t = &fk.targets(2)[0];
    let j = jump_in(t, &[(Kind::Int, snap.cell(0, 2))], Dialect::MySql).unwrap();
    assert_eq!(j.sql, "SELECT * FROM dataman_t_fk.customer WHERE id = 1;");
    assert_eq!(run(&e, &c, &j.sql).rows(), 1);

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
    v.sort = Some(("balance".into(), dataman_core::result::SortDir::Desc));
    let f = run(&e, &c, &v.sql());
    assert_eq!((f.rows(), f.snapshot().text(0, 1).as_deref()), (1, Some("Diego O'Neil")));
    let n = e.core.block_on(c.count_rows(db, v.count_sql()).task).unwrap();
    assert_eq!(n, 1);
    let est = e.core.block_on(c.estimate_rows(db, db, "customer")).unwrap();
    assert!(est.is_some(), "TABLE_ROWS after ANALYZE");
    let j = Filter::new("profile", Kind::Json, Op::Eq, "{\"city\": \"Denver\"}").sql_in(Dialect::MySql);
    assert_eq!(run(&e, &c, &format!("SELECT id FROM customer WHERE {j}")).snapshot().text(0, 0).as_deref(), Some("2"));
    let rs = run(&e, &c, "SELECT id, tier, tags FROM customer");
    assert_eq!(*e.core.block_on(c.result_enum_labels(db, &rs, 1)).unwrap(), vec!["bronze", "silver", "gold"]);
    assert!(e.core.block_on(c.result_enum_labels(db, &rs, 2)).unwrap().is_empty(), "SET is not an enum");
    exec_quiet(&e, &c, &["DROP DATABASE dataman_t_fk"]);
}

#[test]
fn grid_edits_apply_in_a_transaction() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "dataman_t_edit") else { return };
    let db = "dataman_t_edit";
    let rs = run(&e, &c, "SELECT * FROM customer ORDER BY id");
    let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    assert_eq!((t.key_source, t.key[0].name.as_str(), t.dialect), (KeySource::PrimaryKey, "id", Dialect::MySql));
    let snap = rs.snapshot();
    let col = |n: &str| rs.columns().unwrap().iter().position(|c| c.name == n).unwrap();
    let mut cs = ChangeSet::new();
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

    let stale = rs.snapshot();
    let mut cs = ChangeSet::new();
    cs.edit(4, col("balance"), CellValue::text("12.00"), CellValue::text("13.00"));
    cs.edit(0, col("balance"), CellValue::text("1520.50"), CellValue::text("0"));
    query(&e, &c, "UPDATE customer SET balance = 1 WHERE id = 1");
    let r = e.core.block_on(c.apply_changes(Some(db), cs.plan(&t, &stale).unwrap())).unwrap();
    assert!(!r.committed);
    assert!(r.outcomes.iter().any(|o| matches!(&o.outcome, Outcome::Conflict { kind: ConflictKind::Changed, .. })), "{r:?}");
    assert_eq!(run(&e, &c, "SELECT balance FROM customer WHERE id = 5").snapshot().text(0, 0).as_deref(), Some("12.00"), "rolled back");

    let j = run(&e, &c, "SELECT o.id, c.name FROM orders o JOIN customer c ON c.id = o.customer_id");
    assert!(matches!(e.core.block_on(c.edit_target(db, &j, None)).unwrap(), Editability::ReadOnly(_)));
    let v = run(&e, &c, "SELECT * FROM v_order_customer");
    assert!(matches!(e.core.block_on(c.edit_target(db, &v, None)).unwrap(), Editability::ReadOnly(_)));
    let it = run(&e, &c, "SELECT * FROM `item`");
    match e.core.block_on(c.edit_target(db, &it, None)).unwrap() {
        Editability::Editable(t) => assert_eq!(t.key.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), vec!["order_id", "line"]),
        Editability::ReadOnly(why) => panic!("{why}"),
    }
    exec_quiet(&e, &c, &["DROP DATABASE dataman_t_edit"]);
}

#[test]
fn exports_and_bounded_statements() {
    let e = env();
    let Some(c) = mysql_seeded(&e, "dataman_t_mcp") else { return };
    let db = "dataman_t_mcp";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.csv");
    let x = c.export_query(Some(db), "SELECT id, name, balance, created, birth_date FROM customer ORDER BY id", &path, ExportOptions::new(ExportFormat::Csv), None);
    let s = e.core.block_on(x.task).unwrap();
    assert_eq!(s.rows, 5);
    let csv = std::fs::read_to_string(&path).unwrap();
    assert!(csv.lines().nth(1).unwrap().starts_with("1,Ana Smith,1520.50,2026-01-05 09:15:00.25,1990-04-12"), "{csv}");
    let rs = query(&e, &c, "SELECT id, name, photo FROM dataman_t_mcp.customer ORDER BY id");
    let mut o = ExportOptions::new(ExportFormat::SqlInsert);
    o.table = "dataman_t_mcp.copied".into();
    o.dialect = Dialect::MySql;
    let p = dir.path().join("c.sql");
    e.core.block_on(dataman_core::export::export_result(c.handle(), rs, &p, o).task).unwrap();
    let sql = std::fs::read_to_string(&p).unwrap();
    assert!(sql.contains("'Eva \\\\ Brown'") && sql.contains("X'89504e47'"), "{sql}");
    query(&e, &c, "CREATE TABLE dataman_t_mcp.copied (id INT, name VARCHAR(80), photo BLOB)");
    for st in dataman_core::sql::split_in(&sql, Dialect::MySql) {
        query(&e, &c, &sql[st.range]);
    }
    let back = query(&e, &c, "SELECT name FROM dataman_t_mcp.copied WHERE id = 5");
    assert_eq!(back.snapshot().text(0, 0).as_deref(), Some("Eva \\ Brown"), "round trip");

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
    exec_quiet(&e, &c, &["DROP DATABASE dataman_t_mcp"]);
}

/// A database's name is data: a backslash must not take the literal's closing quote away. Run on a
/// server whose own `sql_mode` has `NO_BACKSLASH_ESCAPES` as well (`DATAMAN_MYSQL` to one): the same
/// literal must read as the same name there.
#[test]
fn a_database_name_cannot_close_a_literal() {
    let e = env();
    let Some(admin) = mysql_on(&e, mysql_profile("")) else { return };
    let mode = run(&e, &admin, "SELECT @@GLOBAL.sql_mode").snapshot().text(0, 0).unwrap().into_owned();
    eprintln!("sql_mode: {mode}");
    let name = format!("d{}\\') UNION SELECT 0x70776e,1,2,3,4,5 #", std::process::id());
    assert!(name.len() <= 64);
    let q = Dialect::MySql.quote_ident(&name);
    exec_quiet(&e, &admin, &[&format!("DROP DATABASE IF EXISTS {q}")]);
    exec(&e, &admin, &[&format!("CREATE DATABASE {q}"), &format!("CREATE TABLE {q}.t (a INT PRIMARY KEY)")]);
    let c = mysql_on(&e, mysql_profile(&name)).unwrap();
    e.core.block_on(c.preload(&name)).unwrap();
    assert!(c.schema.cached_columns(&name, "pwn", "1").is_none(), "the statement ran");
    assert_eq!(c.schema.cached_columns(&name, &name, "t").expect("its own table's columns")[0].name, "a");
    let rels = e.core.block_on(c.relations(&name, &name)).unwrap();
    assert_eq!(rels.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["t"]);
    assert!(c.schema.notes().is_empty(), "{:?}", c.schema.notes());
    drop(c);
    exec_quiet(&e, &admin, &[&format!("DROP DATABASE {q}")]);
}

/// The statement's target decides, not the database the connection is on, and a `USE` moves it.
#[test]
fn a_read_only_database_holds_against_a_write_from_another() {
    let e = env();
    let Some(admin) = mysql_on(&e, mysql_profile("")) else { return };
    let pid = std::process::id();
    let (w, ro) = (format!("dataman_t_w_{pid}"), format!("dataman_t_ro_{pid}"));
    for d in [&w, &ro] {
        exec_quiet(&e, &admin, &[&format!("DROP DATABASE IF EXISTS {d}")]);
        exec(&e, &admin, &[&format!("CREATE DATABASE {d}"), &format!("CREATE TABLE {d}.items (id INT)"), &format!("INSERT INTO {d}.items VALUES (1)")]);
    }
    let count = |d: &str| run(&e, &admin, &format!("SELECT count(*) FROM {d}.items")).snapshot().text(0, 0).map(|t| t.into_owned());
    let mut p = mysql_profile(&w);
    p.set_read_only_for(&ro, true);
    let c = mysql_on(&e, p).unwrap();
    let call = |sql: &str| e.core.block_on(c.bounded(None, sql, vec![], Mode::Write, Limits::default(), std::future::pending::<()>()));
    for sql in [
        format!("INSERT INTO {ro}.items VALUES (2)"),
        format!("UPDATE `{ro}`.items SET id = 9"),
        format!("DELETE FROM {ro}.items"),
        format!("REPLACE INTO {ro}.items VALUES (2)"),
        format!("UPDATE {w}.items w JOIN {ro}.items r ON w.id = r.id SET r.id = 9"),
        format!("/*! INSERT INTO {ro}.items VALUES (2) */"),
        format!("USE {ro}; INSERT INTO items VALUES (2)"),
        format!("INSERT INTO {w}.items VALUES (2); DELETE FROM {ro}.items"),
    ] {
        assert!(matches!(call(&sql), Err(Stop::Refused(r)) if r.contains("read-only")), "{sql}");
    }
    assert_eq!(count(&ro).as_deref(), Some("1"));
    assert_eq!(count(&w).as_deref(), Some("1"), "nothing of a refused call runs");

    call(&format!("INSERT INTO {w}.items SELECT id FROM {ro}.items")).unwrap();
    assert_eq!(count(&w).as_deref(), Some("2"));

    // A tab: the same check, with the database a `USE` left it on.
    let tab = c.session(None);
    let use_ro = tab.execute(format!("USE {ro}"));
    e.core.block_on(use_ro.wait_finished());
    assert_eq!(tab.current_database(), ro);
    assert!(c.read_only_refusal(&tab.current_database(), "INSERT INTO items VALUES (2)").is_some());

    // A `USE` must not stay on a connection the next call takes from the pool.
    let other = format!("dataman_t_other_{pid}");
    exec(&e, &admin, &[&format!("CREATE DATABASE {other}")]);
    call(&format!("USE {other}")).unwrap();
    let o = e.core.block_on(c.bounded(None, "SELECT DATABASE()", vec![], Mode::Read, Limits::default(), std::future::pending::<()>())).unwrap();
    assert_eq!(o.rows[0][0], serde_json::json!(w));
    exec_quiet(&e, &admin, &[&format!("DROP DATABASE {other}"), &format!("DROP DATABASE {w}"), &format!("DROP DATABASE {ro}")]);
}
