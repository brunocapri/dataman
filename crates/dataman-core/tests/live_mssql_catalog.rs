mod common;

use common::*;
use dataman_core::bounded::{Limits, Mode, Stop};
use dataman_core::dataview::{Filter, Op, TableView};
use dataman_core::edit::{CellValue, ChangeSet, ConflictKind, Editability, KeySource, Outcome};
use dataman_core::export::{ExportFormat, ExportOptions};
use dataman_core::fknav::jump_in;
use dataman_core::schema::{ConstraintKind, FuncKind, RelKind, TypeKind};
use dataman_core::sql::Dialect;
use dataman_core::value::Kind;

fn on(e: &Env, c: &Arc<Connection>, sql: &str) -> Arc<dataman_core::ResultSet> {
    query(e, c, sql)
}

#[test]
fn explorer_levels_and_objects() {
    let e = env();
    let db = "dataman_t_cat";
    let Some(c) = mssql_seeded(&e, db) else { return };
    let dbs = e.core.block_on(c.databases()).unwrap();
    assert!(dbs.iter().any(|d| d == db));
    assert!(dbs.iter().position(|d| d == "master") > dbs.iter().position(|d| d == db), "system databases last: {dbs:?}");
    let schemas = e.core.block_on(c.schemas(db)).unwrap();
    assert_eq!(&schemas[..2], ["dbo", "sales"], "{schemas:?}");
    assert_eq!(schemas.last().map(String::as_str), Some("sys"));
    assert!(!schemas.iter().any(|s| s == "guest" || s.starts_with("db_")), "{schemas:?}");
    let rels = e.core.block_on(c.relations(db, "dbo")).unwrap();
    let names: Vec<(&str, RelKind)> = rels.iter().map(|r| (r.name.as_str(), r.kind)).collect();
    assert_eq!(names, vec![("customer", RelKind::Table), ("documents", RelKind::Table), ("item", RelKind::Table), ("orders", RelKind::Table), ("v_order_customer", RelKind::View)]);
    let cols = e.core.block_on(c.columns(db, "dbo", "customer")).unwrap();
    assert!(cols[0].primary_key);
    assert_eq!(cols[0].default.as_deref(), Some("IDENTITY(1,1)"));
    let get = |n: &str| cols.iter().find(|c| c.name == n).unwrap().clone();
    assert_eq!((get("name").type_name.as_str(), get("name").nullable), ("nvarchar(80)", Some(false)));
    assert_eq!((get("balance").type_name.as_str(), get("balance").default.as_deref()), ("decimal(12,2)", Some("0")));
    assert_eq!(get("created").type_name, "datetime2(3)");
    assert_eq!(get("seen").type_name, "datetimeoffset(0)");
    assert_eq!(get("photo").type_name, "varbinary(max)");
    assert_eq!(get("doc").type_name, "national_id", "an alias type by its name");
    assert_eq!(get("row_version").type_name, "rowversion");
    let item = e.core.block_on(c.columns(db, "dbo", "item")).unwrap();
    assert_eq!(item.iter().find(|c| c.name == "subtotal").unwrap().default.as_deref(), Some("AS ([qty]*[price]) PERSISTED"));
    let idx = e.core.block_on(c.indexes(db, "dbo", "orders")).unwrap();
    assert!(idx[0].primary, "{idx:?}");
    let st = idx.iter().find(|i| i.name == "IX_orders_status").unwrap();
    assert_eq!(st.columns, vec!["status", "created_at"]);
    assert_eq!(st.definition.as_deref(), Some("CREATE NONCLUSTERED INDEX IX_orders_status ON dbo.orders (status, created_at DESC) INCLUDE (total)"));

    let counts = e.core.block_on(c.object_counts(db, "dbo")).unwrap();
    assert_eq!((counts.functions, counts.triggers, counts.sequences, counts.types), (Some(2), Some(1), Some(1), Some(2)));
    let fs = e.core.block_on(c.functions(db, "dbo")).unwrap();
    let p = fs.iter().find(|f| f.name == "customer_orders").unwrap();
    assert_eq!((p.kind, p.identity.as_str(), p.arg_types.clone()), (FuncKind::Procedure, "@customer int", vec!["int".to_string()]));
    let f = fs.iter().find(|f| f.name == "customer_total").unwrap();
    assert_eq!((f.kind, f.result.as_deref()), (FuncKind::Function, Some("decimal(12,2)")));
    let def = e.core.block_on(c.routine_definition(db, "dbo", f)).unwrap().unwrap();
    assert!(def.starts_with("CREATE FUNCTION dbo.customer_total"), "{def}");
    let tvf = e.core.block_on(c.functions(db, "sales")).unwrap();
    assert_eq!((tvf[0].name.as_str(), tvf[0].returns_set), ("items_of", true));
    let tr = e.core.block_on(c.triggers(db, "dbo")).unwrap();
    assert_eq!((tr[0].name.as_str(), tr[0].table.as_str(), tr[0].events.as_str(), tr[0].enabled), ("orders_created", "orders", "after insert", true));
    assert!(tr[0].definition.as_deref().unwrap().contains("UPDATE o SET created_at"));
    let seqs = e.core.block_on(c.sequences(db, "dbo")).unwrap();
    assert_eq!((seqs[0].name.as_str(), seqs[0].type_name.as_deref()), ("seq_invoice", Some("bigint")));
    assert_eq!(e.core.block_on(c.sequence_value(db, "dbo", "seq_invoice")).unwrap(), Some(("1000".to_string(), false)));
    let types = e.core.block_on(c.types(db, "dbo")).unwrap();
    assert_eq!(types.iter().map(|t| (t.name.as_str(), t.kind)).collect::<Vec<_>>(), vec![("national_id", TypeKind::Domain), ("order_items", TypeKind::Composite)]);
    assert_eq!(types[0].base.as_deref(), Some("varchar(11)"));
    assert_eq!(e.core.block_on(c.type_definition(db, "dbo", "national_id")).unwrap().as_deref(), Some("CREATE TYPE dbo.national_id FROM varchar(11) NOT NULL;"));
    assert!(e.core.block_on(c.type_definition(db, "dbo", "order_items")).unwrap().unwrap().contains("product nvarchar(60) NOT NULL"));
    let x = e.core.block_on(c.table_extras(db, "dbo", "item")).unwrap();
    let cons = x.constraints.unwrap();
    let fk = cons.iter().find(|c| c.kind == ConstraintKind::Foreign).unwrap();
    assert_eq!(fk.definition.as_deref(), Some("FOREIGN KEY (order_id) REFERENCES orders(id) ON DELETE CASCADE"));
    assert!(cons.iter().any(|c| c.kind == ConstraintKind::Primary && c.definition.as_deref() == Some("PRIMARY KEY (order_id, line)")));
    assert!(cons.iter().any(|c| c.kind == ConstraintKind::Check && c.definition.as_deref() == Some("CHECK ([qty]>(0))")));
    assert_eq!(e.core.block_on(c.table_extras(db, "dbo", "orders")).unwrap().triggers.unwrap().len(), 1);
    let stats = e.core.block_on(c.rel_stats(db, "dbo")).unwrap();
    assert_eq!(stats.get("customer").and_then(|s| s.rows), Some(5.0));

    let ddl = e.core.block_on(c.table_ddl(db, "dbo", "item")).unwrap();
    assert!(ddl.starts_with("-- dbo.item: generated from the catalog of dataman_t_cat\nCREATE TABLE dbo.item (\n    order_id int NOT NULL,"), "{ddl}");
    assert!(ddl.contains("    subtotal AS ([qty]*[price]) PERSISTED,") && ddl.contains("CONSTRAINT FK_item_order FOREIGN KEY (order_id) REFERENCES orders(id) ON DELETE CASCADE"), "{ddl}");
    let ddl = e.core.block_on(c.table_ddl(db, "dbo", "orders")).unwrap();
    assert!(ddl.contains("CREATE NONCLUSTERED INDEX IX_orders_status ON dbo.orders (status, created_at DESC) INCLUDE (total);"), "{ddl}");
    let v = e.core.block_on(c.view_definition(db, "dbo", "v_order_customer")).unwrap().unwrap();
    assert!(v.starts_with("CREATE VIEW dbo.v_order_customer AS"), "{v}");

    e.core.block_on(c.preload(db)).unwrap();
    let snap = c.schema.snapshot(db);
    assert_eq!(snap.dialect, Dialect::TSql);
    let r = snap.resolve(None, "ORDERS").expect("case-insensitive");
    assert!(snap.columns_of(r).iter().any(|c| c.name == "total" && c.type_name == "decimal(12,2)"));
    let cols = e.core.block_on(c.columns(db, "dbo", "orders")).unwrap();
    assert!(cols.iter().any(|c| c.name == "id" && c.primary_key), "{cols:?}");
    let cat = e.core.block_on(c.catalog(db)).unwrap();
    assert_eq!(cat.functions_named(None, "customer_total").count(), 1);
    assert!(e.core.block_on(c.relations("master", "dbo")).is_ok());
    drop(c);
    mssql_drop(&e, db);
}

#[test]
fn foreign_keys_and_the_data_view() {
    let e = env();
    let db = "dataman_t_fk";
    let Some(c) = mssql_seeded(&e, db) else { return };
    let g = e.core.block_on(c.relationships(db, "dbo")).unwrap();
    let l = g.links.iter().find(|l| l.from_table == "item").unwrap();
    assert_eq!((l.from_columns.clone(), l.to_table.as_str(), l.to_columns.clone()), (vec!["order_id".to_string()], "orders", vec!["id".to_string()]));

    let rs = on(&e, &c, "SELECT [line], order_id, product FROM [dbo].[item] ORDER BY order_id, line");
    let fk = e.core.block_on(c.foreign_keys(db, &rs)).unwrap();
    assert_eq!(fk.flags(), vec![false, true, false]);
    let snap = rs.snapshot();
    let t = &fk.targets(1)[0];
    let j = jump_in(t, &[(Kind::Int, snap.cell(0, 1))], Dialect::TSql).unwrap();
    assert_eq!(j.sql, "SELECT * FROM dbo.orders WHERE id = 1;");
    assert_eq!(on(&e, &c, &j.sql).rows(), 1);

    assert_eq!(e.core.block_on(c.primary_key(db, "dbo", "item")).unwrap(), vec!["order_id", "line"]);
    let mut v = TableView::new("dbo", "customer", vec!["id".into()], 2);
    v.dialect = Dialect::TSql;
    v.page = 1;
    let page = on(&e, &c, &v.sql());
    assert_eq!(page.snapshot().text(0, 0).as_deref(), Some("3"));
    v.page = 0;
    v.filters.push(Filter::new("tier", Kind::Int, Op::In, "2, 3"));
    v.filters.push(Filter::new("name", Kind::Text, Op::Contains, "o'n"));
    v.filters.push(Filter::new("active", Kind::Bool, Op::Eq, "true"));
    v.sort = Some(("balance".into(), dataman_core::result::SortDir::Desc));
    let f = on(&e, &c, &v.sql());
    assert_eq!((f.rows(), f.snapshot().text(0, 1).as_deref()), (1, Some("Diego O'Neil")));
    assert_eq!(e.core.block_on(c.count_rows(db, v.count_sql()).task).unwrap(), 1);
    let u = Filter::new("name", Kind::Text, Op::StartsWith, "Élodie Zürich").sql_in(Dialect::TSql);
    assert_eq!(on(&e, &c, &format!("SELECT id FROM dbo.customer WHERE {u}")).snapshot().text(0, 0).as_deref(), Some("5"), "Unicode text");
    let p = Filter::new("name", Kind::Text, Op::Contains, "%").sql_in(Dialect::TSql);
    assert_eq!(on(&e, &c, &format!("SELECT id FROM dbo.customer WHERE {p}")).rows(), 0, "a % is literal");
    assert_eq!(e.core.block_on(c.estimate_rows(db, "dbo", "customer")).unwrap(), Some(5));
    let mut h = TableView::new("sales", "event", vec![], 2);
    h.dialect = Dialect::TSql;
    assert_eq!(on(&e, &c, &h.sql()).rows(), 2);
    h.page = 1;
    assert_eq!(on(&e, &c, &h.sql()).rows(), 1);
    drop(c);
    mssql_drop(&e, db);
}

#[test]
fn grid_edits_apply_in_a_transaction() {
    let e = env();
    let db = "dataman_t_edit";
    let Some(c) = mssql_seeded(&e, db) else { return };
    let rs = on(&e, &c, "SELECT * FROM dbo.customer ORDER BY id");
    let t = match e.core.block_on(c.edit_target(db, &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    assert_eq!((t.key_source, t.key[0].name.as_str(), t.dialect), (KeySource::PrimaryKey, "id", Dialect::TSql));
    let col = |n: &str| rs.columns().unwrap().iter().position(|c| c.name == n).unwrap();
    assert!(!t.is_editable(col("id")) && !t.is_editable(col("row_version")) && t.is_editable(col("name")));
    let snap = rs.snapshot();
    let mut cs = ChangeSet::new();
    cs.edit(0, col("name"), CellValue::text("Ana Smith"), CellValue::text("Zoë d'Arcy 東京"));
    cs.edit(1, col("active"), CellValue::text("true"), CellValue::text("false"));
    cs.edit(1, col("photo"), CellValue::Null, CellValue::text("\\xcafe"));
    cs.edit(2, col("created"), CellValue::text("2026-03-20 18:45:10"), CellValue::text("2026-03-21 08:00:00.125"));
    cs.edit(2, col("credit_limit"), CellValue::text("0.0001"), CellValue::text("12.3456"));
    cs.edit(3, col("points"), CellValue::text("-9223372036854775808"), CellValue::Null);
    let i = cs.insert_row(snap.columns.len());
    cs.set_inserted(i, col("name"), Some(CellValue::text("New")));
    cs.set_inserted(i, col("email"), Some(CellValue::text("new@example.com")));
    cs.set_inserted(i, col("id"), Some(CellValue::text("99")));
    cs.delete_row(4);
    let plan = cs.plan(&t, &snap).unwrap();
    assert!(plan[1].sql.contains("photo = 0xcafe") && plan[1].sql.contains("active = 0"), "{}", plan[1].sql);
    let r = e.core.block_on(c.apply_changes(Some(db), plan)).unwrap();
    assert!(r.committed, "{r:?}");
    let after = on(&e, &c, "SELECT name, active, photo, created, credit_limit, points FROM dbo.customer ORDER BY id");
    let a = after.snapshot();
    assert_eq!(a.text(0, 0).as_deref(), Some("Zoë d'Arcy 東京"));
    assert_eq!((a.text(1, 1).as_deref(), a.text(1, 2).as_deref()), (Some("false"), Some("\\xcafe")));
    assert_eq!((a.text(2, 3).as_deref(), a.text(2, 4).as_deref()), (Some("2026-03-21 08:00:00.125"), Some("12.3456")));
    assert_eq!(a.text(3, 5), None);
    assert_eq!(a.rows(), 5, "one inserted, one deleted");
    let ord = on(&e, &c, "SELECT * FROM dbo.orders ORDER BY id");
    let ot = match e.core.block_on(c.edit_target(db, &ord, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    let oc = |n: &str| ord.columns().unwrap().iter().position(|c| c.name == n).unwrap();
    let mut cs = ChangeSet::new();
    cs.edit(0, oc("notes"), CellValue::text("first order"), CellValue::text("revised"));
    let i = cs.insert_row(ord.columns().unwrap().len());
    cs.set_inserted(i, oc("customer_id"), Some(CellValue::text("2")));
    cs.set_inserted(i, oc("total"), Some(CellValue::text("7.5")));
    let r = e.core.block_on(c.apply_changes(Some(db), cs.plan(&ot, &ord.snapshot()).unwrap())).unwrap();
    assert!(r.committed, "{r:?}");
    let n = on(&e, &c, "SELECT COUNT(*) FROM dbo.orders WHERE created_at IS NOT NULL AND total = 7.5");
    assert_eq!(n.snapshot().text(0, 0).as_deref(), Some("1"), "the trigger ran");

    let stale = after.clone();
    let st = on(&e, &c, "SELECT id, balance FROM dbo.customer ORDER BY id");
    let tt = match e.core.block_on(c.edit_target(db, &st, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("{why}"),
    };
    let _ = stale;
    let mut cs = ChangeSet::new();
    cs.edit(1, 1, CellValue::text("310.00"), CellValue::text("1"));
    cs.edit(0, 1, CellValue::text("1520.50"), CellValue::text("0"));
    on(&e, &c, "UPDATE dbo.customer SET balance = 2 WHERE id = 1");
    let r = e.core.block_on(c.apply_changes(Some(db), cs.plan(&tt, &st.snapshot()).unwrap())).unwrap();
    assert!(!r.committed);
    assert!(r.outcomes.iter().any(|o| matches!(&o.outcome, Outcome::Conflict { kind: ConflictKind::Changed { .. }, .. })), "{r:?}");
    assert_eq!(on(&e, &c, "SELECT balance FROM dbo.customer WHERE id = 2").snapshot().text(0, 0).as_deref(), Some("310.00"), "rolled back");

    let j = on(&e, &c, "SELECT o.id, c.name FROM dbo.orders o JOIN dbo.customer c ON c.id = o.customer_id");
    assert!(matches!(e.core.block_on(c.edit_target(db, &j, None)).unwrap(), Editability::ReadOnly(_)));
    let v = on(&e, &c, "SELECT * FROM dbo.v_order_customer");
    assert!(matches!(e.core.block_on(c.edit_target(db, &v, None)).unwrap(), Editability::ReadOnly(_)));
    let top = on(&e, &c, "SELECT TOP (2) * FROM [dbo].[item]");
    match e.core.block_on(c.edit_target(db, &top, None)).unwrap() {
        Editability::Editable(t) => {
            assert_eq!(t.key.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), vec!["order_id", "line"]);
            assert!(!t.is_editable(top.columns().unwrap().iter().position(|c| c.name == "subtotal").unwrap()), "computed");
        }
        Editability::ReadOnly(why) => panic!("{why}"),
    }
    drop(c);
    mssql_drop(&e, db);
}

#[test]
fn exports_and_bounded_statements() {
    let e = env();
    let db = "dataman_t_mcp";
    let Some(c) = mssql_seeded(&e, db) else { return };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.csv");
    let x = c.export_query(Some(db), "SELECT id, name, balance, created, birth_date FROM dbo.customer ORDER BY id", &path, ExportOptions::new(ExportFormat::Csv), None);
    let s = e.core.block_on(x.task).unwrap();
    assert_eq!(s.rows, 5);
    let csv = std::fs::read_to_string(&path).unwrap();
    assert!(csv.lines().nth(1).unwrap().starts_with("1,Ana Smith,1520.50,2026-01-05 09:15:00.25,1990-04-12"), "{csv}");
    let rs = query(&e, &c, "SELECT id, name, photo, active, created, seen FROM dbo.customer ORDER BY id");
    let mut o = ExportOptions::new(ExportFormat::SqlInsert);
    o.table = "dbo.copied".into();
    o.dialect = Dialect::TSql;
    let p = dir.path().join("c.sql");
    e.core.block_on(dataman_core::export::export_result(c.handle(), rs, &p, o).task).unwrap();
    let sql = std::fs::read_to_string(&p).unwrap();
    assert!(sql.contains("N'Élodie Zürich 東京'") && sql.contains("0x89504e47") && sql.contains("N'2026-01-05T09:15:00.25'"), "{sql}");
    query(&e, &c, "CREATE TABLE dbo.copied (id int, name nvarchar(80), photo varbinary(max), active bit, created datetime2(3), seen datetimeoffset(0))");
    for st in dataman_core::sql::split_in(&sql, Dialect::TSql) {
        query(&e, &c, &sql[st.range]);
    }
    let back = query(&e, &c, "SELECT a.name FROM dbo.copied a JOIN dbo.customer b ON b.id = a.id AND b.name = a.name AND b.active = a.active AND b.created = a.created AND (b.seen = a.seen OR b.seen IS NULL) AND (b.photo = a.photo OR b.photo IS NULL)");
    assert_eq!(back.rows(), 5, "round trip");

    let lim = Limits { max_rows: 2, ..Default::default() };
    let o = e.core.block_on(c.bounded(Some(db), "SELECT id, name, active FROM dbo.customer ORDER BY id", vec![], Mode::Read, lim, std::future::pending::<()>())).unwrap();
    assert_eq!(o.rows.len(), 2);
    assert_eq!(o.rows[0][2], serde_json::json!(true));
    assert!(o.truncated.is_some());
    for bad in ["DELETE FROM dbo.customer", "EXEC dbo.customer_orders 1", "SELECT 1 UPDATE dbo.orders SET notes = 'x' WHERE id = 1"] {
        let r = e.core.block_on(c.bounded(Some(db), bad, vec![], Mode::Read, Limits::default(), std::future::pending::<()>()));
        assert!(matches!(r, Err(Stop::Refused(_))), "{bad}");
    }
    let o = e.core
        .block_on(c.bounded(Some(db), "UPDATE dbo.orders SET notes = @P1 WHERE id = @P2", vec![Some("x".into()), Some("2".into())], Mode::Write, Limits::default(), std::future::pending::<()>()))
        .unwrap();
    assert_eq!(o.statements[0].affected, Some(1));
    let o = e.core.block_on(c.bounded(Some(db), "SELECT notes FROM dbo.orders WHERE id = @P1", vec![Some("2".into())], Mode::Read, Limits::default(), std::future::pending::<()>())).unwrap();
    assert_eq!(o.rows[0][0], serde_json::json!("x"));
    let o = e.core.block_on(c.bounded(Some(db), "DELETE FROM dbo.item WHERE order_id = 1", vec![], Mode::Rollback, Limits::default(), std::future::pending::<()>())).unwrap();
    assert_eq!(o.statements[0].affected, Some(2));
    assert_eq!(query(&e, &c, "SELECT COUNT(*) FROM dbo.item WHERE order_id = 1").snapshot().text(0, 0).as_deref(), Some("2"), "rolled back");
    drop(c);
    mssql_drop(&e, db);
}
