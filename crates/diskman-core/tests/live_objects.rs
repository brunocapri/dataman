//! Live tests of the explorer's other objects (see `common` for the servers): a scratch schema
//! with a function, a procedure, a materialized view, sequences, an enum, a domain, a
//! composite, triggers, a check constraint, a partitioned table and grants to a scratch role;
//! every loader and definition read back from PostgreSQL. LAR reports none of it: its groups
//! stay empty. The schema and the role are dropped at the end.

mod common;
use common::*;

use diskman_core::schema::{ConstraintKind, FuncKind, RelKind, TypeKind, objects::Cell};

const S: &str = "diskman_objects_t";
const ROLE: &str = "diskman_objects_reader";

#[test]
fn pg_every_kind() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let cleanup = [format!("DROP SCHEMA IF EXISTS {S} CASCADE"), format!("DROP ROLE IF EXISTS {ROLE}")];
    let cleanup: Vec<&str> = cleanup.iter().map(String::as_str).collect();
    exec_quiet(&e, &c, &cleanup);
    let setup = [
        format!("CREATE SCHEMA {S}"),
        format!("CREATE ROLE {ROLE} NOLOGIN"),
        format!("CREATE TYPE {S}.mood AS ENUM ('sad', 'ok', 'happy')"),
        format!("CREATE TYPE {S}.point2 AS (x integer, y integer)"),
        format!("CREATE DOMAIN {S}.price AS numeric(12,2) NOT NULL DEFAULT 0 CONSTRAINT price_positive CHECK (VALUE >= 0)"),
        format!(
            "CREATE TABLE {S}.customers (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, email text NOT NULL UNIQUE, mood {S}.mood)"
        ),
        format!(
            "CREATE TABLE {S}.orders (id serial PRIMARY KEY, customer bigint REFERENCES {S}.customers(id) ON DELETE CASCADE, \
             total {S}.price, qty int CONSTRAINT qty_positive CHECK (qty > 0))"
        ),
        format!("CREATE SEQUENCE {S}.invoice_no START 1000"),
        format!("CREATE TABLE {S}.events (at date NOT NULL, v int) PARTITION BY RANGE (at)"),
        format!("CREATE TABLE {S}.events_2026_01 PARTITION OF {S}.events FOR VALUES FROM ('2026-01-01') TO ('2026-02-01')"),
        format!("CREATE TABLE {S}.events_2026_02 PARTITION OF {S}.events FOR VALUES FROM ('2026-02-01') TO ('2026-03-01')"),
        format!("CREATE MATERIALIZED VIEW {S}.order_totals AS SELECT customer, sum(total) AS total FROM {S}.orders GROUP BY customer"),
        format!("CREATE FUNCTION {S}.add(a integer, b integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT a + b'"),
        format!("CREATE FUNCTION {S}.recent(n integer) RETURNS SETOF {S}.orders LANGUAGE sql AS 'SELECT * FROM {S}.orders ORDER BY id DESC LIMIT n'"),
        format!("CREATE PROCEDURE {S}.archive(before date) LANGUAGE sql AS 'SELECT 1'"),
        format!("CREATE FUNCTION {S}.touch() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'"),
        format!("CREATE TRIGGER orders_touch BEFORE INSERT OR UPDATE ON {S}.orders FOR EACH ROW EXECUTE FUNCTION {S}.touch()"),
        format!("CREATE TRIGGER orders_audit AFTER DELETE ON {S}.orders FOR EACH ROW EXECUTE FUNCTION {S}.touch()"),
        format!("ALTER TABLE {S}.orders DISABLE TRIGGER orders_audit"),
        format!("INSERT INTO {S}.customers (email) SELECT 'c' || i || '@x' FROM generate_series(1, 500) i"),
        format!("INSERT INTO {S}.events SELECT date '2026-01-01' + i % 50, i FROM generate_series(1, 200) i"),
        format!("ANALYZE {S}.customers"),
        format!("GRANT USAGE ON SCHEMA {S} TO {ROLE}"),
        format!("GRANT SELECT ON {S}.orders TO {ROLE}"),
        format!("GRANT UPDATE ON {S}.orders TO {ROLE} WITH GRANT OPTION"),
    ];
    let setup: Vec<&str> = setup.iter().map(String::as_str).collect();
    exec(&e, &c, &setup);
    let db = c.database().to_string();
    macro_rules! run {
        ($t:expr) => {
            e.core.block_on($t)
        };
    }

    // Relations: the materialized view is its own kind; partitions are tables.
    let rels = run!(c.relations(&db, S)).unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.name == n).map(|r| r.kind);
    assert_eq!(kind("order_totals"), Some(RelKind::MaterializedView));
    assert_eq!(kind("events"), Some(RelKind::Partitioned));
    assert_eq!(kind("events_2026_01"), Some(RelKind::Table));

    // Counts, in one query.
    let n = run!(c.object_counts(&db, S)).unwrap();
    assert_eq!(n.functions, Some(4), "add, recent, archive, touch");
    assert_eq!(n.sequences, Some(3), "the identity's, the serial's and invoice_no");
    assert_eq!(n.types, Some(3), "mood, point2, price");
    assert_eq!(n.triggers, Some(2));

    let f = run!(c.functions(&db, S)).unwrap();
    let add = f.iter().find(|f| f.name == "add").unwrap();
    assert_eq!((add.kind, add.arg_types.clone(), add.result.as_deref()), (FuncKind::Function, vec!["integer".to_string(), "integer".into()], Some("integer")));
    assert_eq!(add.identity, "a integer, b integer");
    assert!(f.iter().find(|f| f.name == "recent").unwrap().returns_set);
    let archive = f.iter().find(|f| f.name == "archive").unwrap();
    assert_eq!(archive.kind, FuncKind::Procedure);
    let def = run!(c.function_definition(&db, add.oid)).unwrap().unwrap();
    assert!(def.starts_with(&format!("CREATE OR REPLACE FUNCTION {S}.add(a integer, b integer)")), "{def}");
    let call = diskman_core::sqlgen::function_call(S, archive);
    assert!(call.starts_with(&format!("CALL {S}.archive(")), "{call}");
    // The generated call runs (the placeholder is a valid date).
    exec(&e, &c, &[&call]);

    let seqs = run!(c.sequences(&db, S)).unwrap();
    let owned = seqs.iter().find(|s| s.name == "orders_id_seq").unwrap();
    assert_eq!(owned.owned_by, Some(("orders".into(), "id".into())));
    assert_eq!(owned.type_name.as_deref(), Some("integer"));
    assert!(seqs.iter().find(|s| s.name == "invoice_no").unwrap().owned_by.is_none());
    assert!(seqs.iter().any(|s| s.owned_by == Some(("customers".into(), "id".into()))), "identity columns own theirs");
    let (last, called) = run!(c.sequence_value(&db, S, "invoice_no")).unwrap().unwrap();
    assert_eq!((last.as_str(), called), ("1000", false));

    let types = run!(c.types(&db, S)).unwrap();
    let mood = types.iter().find(|t| t.name == "mood").unwrap();
    assert_eq!((mood.kind, mood.labels.clone()), (TypeKind::Enum, vec!["sad".to_string(), "ok".into(), "happy".into()]));
    assert_eq!(types.iter().find(|t| t.name == "price").unwrap().base.as_deref(), Some("numeric(12,2)"));
    assert_eq!(types.iter().find(|t| t.name == "point2").unwrap().kind, TypeKind::Composite);
    let enum_def = run!(c.type_definition(&db, S, "mood")).unwrap().unwrap();
    assert_eq!(enum_def, format!("CREATE TYPE {S}.mood AS ENUM (\n    'sad',\n    'ok',\n    'happy'\n);"));
    let comp = run!(c.type_definition(&db, S, "point2")).unwrap().unwrap();
    assert!(comp.contains("x integer,\n    y integer"), "{comp}");
    let dom = run!(c.type_definition(&db, S, "price")).unwrap().unwrap();
    assert!(dom.contains("AS numeric(12,2)") && dom.contains("DEFAULT 0") && dom.contains("NOT NULL") && dom.contains("CONSTRAINT price_positive CHECK"), "{dom}");

    let trg = run!(c.triggers(&db, S)).unwrap();
    assert_eq!(trg.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["orders_audit", "orders_touch"]);
    let touch = trg.iter().find(|t| t.name == "orders_touch").unwrap();
    assert_eq!((touch.table.as_str(), touch.enabled, touch.events.as_str()), ("orders", true, "before insert/update"));
    assert!(!trg.iter().find(|t| t.name == "orders_audit").unwrap().enabled);
    assert!(touch.definition.as_deref().unwrap().starts_with("CREATE TRIGGER orders_touch BEFORE INSERT OR UPDATE"));

    // A table's extras: constraints (PK, FK, check; NOT NULL left out), triggers.
    let x = run!(c.table_extras(&db, S, "orders")).unwrap();
    let cons = x.constraints.unwrap();
    let kinds: Vec<ConstraintKind> = cons.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, [ConstraintKind::Primary, ConstraintKind::Foreign, ConstraintKind::Check]);
    let qty = cons.iter().find(|c| c.name == "qty_positive").unwrap();
    assert_eq!(diskman_core::schema::objects::constraint_summary(qty.kind, qty.definition.as_deref()), "check (qty > 0)");
    assert_eq!(x.triggers.unwrap().len(), 2);
    assert_eq!(x.partitions, Some(vec![]));
    assert_eq!(x.parent, None);
    // Partitioned table and partition.
    let ev = run!(c.table_extras(&db, S, "events")).unwrap();
    let parts = ev.partitions.unwrap();
    assert_eq!(parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["events_2026_01", "events_2026_02"]);
    assert_eq!(parts[0].bound.as_deref(), Some("FOR VALUES FROM ('2026-01-01') TO ('2026-02-01')"));
    let p1 = run!(c.table_extras(&db, S, "events_2026_01")).unwrap();
    assert_eq!(p1.parent, Some((S.to_string(), "events".to_string())));

    // Sizes and estimates, one query for the schema.
    let st = run!(c.rel_stats(&db, S)).unwrap();
    assert_eq!(st["customers"].rows, Some(500.));
    assert!(st["customers"].bytes.unwrap() > 8192);
    assert!(st.contains_key("order_totals"));
    // A partitioned table adds up its partitions (the PostgreSQL 12+ query ran).
    assert!(st["events"].bytes.unwrap() > 0, "{:?}", st["events"]);

    // Extensions and roles.
    let exts = run!(c.extensions(&db)).unwrap();
    let plpgsql = exts.iter().find(|x| x.name == "plpgsql").expect("plpgsql is always installed");
    assert_eq!(plpgsql.schema.as_deref(), Some("pg_catalog"));
    assert!(plpgsql.default_version.is_some());
    let roles = run!(c.roles()).unwrap();
    assert!(roles.iter().all(|r| !r.name.starts_with("pg_")));
    let pg_user = roles.iter().find(|r| r.name == "postgres").unwrap();
    assert!(pg_user.superuser && pg_user.login);
    let reader = roles.iter().find(|r| r.name == ROLE).unwrap();
    assert!(!reader.login && !reader.superuser);
    let rdef = run!(c.role_definition(ROLE)).unwrap().unwrap();
    assert!(rdef.starts_with(&format!("CREATE ROLE {ROLE} WITH\n    NOLOGIN")), "{rdef}");

    // Privileges: the owner, the scratch role's grants, the schema's.
    let p = run!(c.privileges(&db, S, Some("orders"))).unwrap().unwrap();
    assert_eq!(p.owner.as_deref(), Some("postgres"));
    assert_eq!(p.roles()[0], "postgres");
    assert_eq!(p.cell(ROLE, "SELECT"), Cell::Yes);
    assert_eq!(p.cell(ROLE, "UPDATE"), Cell::Grantable);
    assert_eq!(p.cell(ROLE, "DELETE"), Cell::No);
    assert_eq!(p.cell("postgres", "TRUNCATE"), Cell::Yes);
    let sp = run!(c.privileges(&db, S, None)).unwrap().unwrap();
    assert_eq!(sp.cell(ROLE, "USAGE"), Cell::Yes);
    assert_eq!(sp.cell(ROLE, "CREATE"), Cell::No);
    // The generated templates run (in a transaction that is rolled back).
    let g = diskman_core::sqlgen::grant_template(S, Some("orders"), Some(ROLE));
    let r = diskman_core::sqlgen::revoke_template(S, Some("orders"), Some(ROLE));
    let reset = diskman_core::sqlgen::sequence_reset_template(S, "orders_id_seq", Some(("orders", "id")));
    let toggle = diskman_core::sqlgen::trigger_toggle_template(S, "orders", "orders_audit", true);
    exec(&e, &c, &["BEGIN", &g, &r, &reset, &toggle, "ROLLBACK"]);

    // Reloading a schema forgets its objects.
    c.schema.invalidate_schema(&db, S);
    assert!(c.schema.cached_functions(&db, S).is_none() && c.schema.cached_counts(&db, S).is_none());
    assert!(c.schema.cached_extras(&db, S, "orders").is_none() && c.schema.cached_stats(&db, S).is_none());

    exec(&e, &c, &cleanup);
}

#[test]
fn lar_reports_none_of_it() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    let db = c.database().to_string();
    macro_rules! run {
        ($t:expr) => {
            e.core.block_on($t)
        };
    }
    let rels = run!(c.relations(&db, "public")).unwrap();
    let n = run!(c.object_counts(&db, "public")).unwrap();
    // Zero or not reported: the explorer hides both.
    for v in [n.functions, n.sequences, n.types, n.triggers] {
        assert!(v.unwrap_or(0) == 0, "{n:?}");
    }
    assert!(run!(c.functions(&db, "public")).unwrap().is_empty());
    assert!(run!(c.triggers(&db, "public")).unwrap().is_empty());
    assert!(run!(c.roles()).unwrap().is_empty(), "LAR's pg_roles is empty");
    // Every table answers zero rows and zero bytes: not reported.
    assert!(run!(c.rel_stats(&db, "public")).unwrap().is_empty());
    if let Some(r) = rels.iter().find(|r| r.kind == RelKind::Table) {
        let x = run!(c.table_extras(&db, "public", &r.name)).unwrap();
        for v in [x.constraints.map(|v| v.len()), x.triggers.map(|v| v.len()), x.partitions.map(|v| v.len())] {
            assert!(v.unwrap_or(0) == 0, "{r:?}");
        }
        assert!(run!(c.privileges(&db, "public", Some(&r.name))).unwrap().is_none());
    }
}
