//! Live tests of the generated `CREATE TABLE` / view statements (see `common` for the servers):
//! the DDL of a table in one scratch schema, run in a second one, gives back the same DDL.

mod common;
use common::*;

/// Runs every statement of a script (the generated DDL) in one session.
fn run_script(e: &Env, c: &Arc<Connection>, script: &str) {
    let stmts: Vec<String> = dataman_core::sql::split(script).into_iter().map(|s| script[s.range].to_string()).collect();
    let refs: Vec<&str> = stmts.iter().map(String::as_str).collect();
    exec(e, c, &refs);
}

#[test]
fn pg_create_table_round_trip() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let (a, b) = ("dataman_ddl_a", "dataman_ddl_b");
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {a} CASCADE"), &format!("DROP SCHEMA IF EXISTS {b} CASCADE")]);
    exec(
        &e,
        &c,
        &[
            &format!("CREATE SCHEMA {a}"),
            &format!(
                "CREATE TABLE {a}.customers (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, email text NOT NULL UNIQUE, \
                 \"Name\" varchar(80) COLLATE \"C\", created timestamptz NOT NULL DEFAULT now())"
            ),
            &format!(
                "CREATE TABLE {a}.orders (id serial PRIMARY KEY, customer bigint NOT NULL REFERENCES {a}.customers(id) ON DELETE CASCADE, \
                 total numeric(12,2) NOT NULL DEFAULT 0 CHECK (total >= 0), qty int, twice numeric GENERATED ALWAYS AS (total * 2) STORED, \
                 tags text[] DEFAULT '{{}}', status text DEFAULT 'new', UNIQUE (customer, status))"
            ),
            &format!("CREATE INDEX orders_lower_status ON {a}.orders (lower(status)) WHERE qty > 0"),
            &format!("COMMENT ON TABLE {a}.orders IS 'orders, it''s'"),
            &format!("COMMENT ON COLUMN {a}.orders.total IS 'gross'"),
            &format!("CREATE UNLOGGED TABLE {a}.scratch (k int)"),
            &format!("CREATE TABLE {a}.events (at timestamptz NOT NULL, v int) PARTITION BY RANGE (at)"),
            &format!("CREATE VIEW {a}.big_orders AS SELECT id, total FROM {a}.orders WHERE total > 100"),
        ],
    );
    let db = c.database().to_string();
    let tables = ["customers", "orders", "scratch", "events"];
    let ddl_a: Vec<String> = tables.iter().map(|t| e.core.block_on(c.table_ddl(&db, a, t)).unwrap()).collect();
    let orders = &ddl_a[1];
    eprintln!("{}\n{orders}", ddl_a[0]);
    for want in [
        "id serial,",
        &format!("customer bigint NOT NULL,"),
        "total numeric(12,2) NOT NULL DEFAULT 0,",
        "GENERATED ALWAYS AS",
        "STORED",
        "tags text[] DEFAULT '{}'::text[]",
        &format!("PRIMARY KEY (id)"),
        &format!("FOREIGN KEY (customer) REFERENCES {a}.customers(id) ON DELETE CASCADE"),
        "CHECK (total >= 0::numeric)",
        "UNIQUE (customer, status)",
        &format!("CREATE INDEX orders_lower_status ON {a}.orders USING btree (lower(status)) WHERE (qty > 0);"),
        &format!("COMMENT ON TABLE {a}.orders IS 'orders, it''s';"),
        &format!("COMMENT ON COLUMN {a}.orders.total IS 'gross';"),
    ] {
        assert!(orders.contains(want), "missing {want:?} in:\n{orders}");
    }
    let customers = &ddl_a[0];
    assert!(customers.contains("id bigint GENERATED ALWAYS AS IDENTITY,"), "{customers}");
    assert!(customers.contains("\"Name\" character varying(80) COLLATE \"C\""), "{customers}");
    assert!(customers.contains("created timestamp with time zone NOT NULL DEFAULT now()"), "{customers}");
    assert!(ddl_a[2].contains(&format!("CREATE UNLOGGED TABLE {a}.scratch")), "{}", ddl_a[2]);
    assert!(ddl_a[3].contains("PARTITION BY RANGE (at)"), "{}", ddl_a[3]);
    // The constraint indexes are not repeated as CREATE INDEX.
    assert_eq!(orders.matches("CREATE INDEX").count(), 1, "{orders}");

    // Run it in the second schema: the DDL read back from there is the same.
    exec(&e, &c, &[&format!("CREATE SCHEMA {b}")]);
    for d in &ddl_a {
        run_script(&e, &c, &d.replace(a, b));
    }
    c.schema.invalidate(None);
    for (t, before) in tables.iter().zip(&ddl_a) {
        let after = e.core.block_on(c.table_ddl(&db, b, t)).unwrap();
        assert_eq!(after.replace(b, a), *before, "round trip of {t}");
    }

    // The view's definition runs too.
    let view = e.core.block_on(c.view_definition(&db, a, "big_orders")).unwrap().expect("a view definition");
    assert!(view.starts_with(&format!("CREATE OR REPLACE VIEW {a}.big_orders AS\n")), "{view}");
    run_script(&e, &c, &view.replace(a, b));
    let again = e.core.block_on(c.view_definition(&db, b, "big_orders")).unwrap().unwrap();
    assert_eq!(again.replace(b, a), view);

    exec(&e, &c, &[&format!("DROP SCHEMA {a} CASCADE"), &format!("DROP SCHEMA {b} CASCADE")]);
}

#[test]
fn lar_create_table_lists_columns_only() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    let t = "dataman_ddl_probe";
    exec_quiet(&e, &c, &[&format!("DROP TABLE {t}")]);
    exec(&e, &c, &[&format!("CREATE TABLE {t} (id bigint PRIMARY KEY, name text NOT NULL, at timestamp)")]);
    let db = c.database().to_string();
    let ddl = e.core.block_on(c.table_ddl(&db, "public", t));
    exec_quiet(&e, &c, &[&format!("DROP TABLE {t}")]);
    let ddl = ddl.unwrap();
    eprintln!("{ddl}");
    assert!(ddl.contains("does not report keys, NOT NULL or defaults"), "{ddl}");
    assert!(ddl.contains(&format!("CREATE TABLE public.{t} (\n    id bigint,\n    name text,\n")), "{ddl}");
    assert!(!ddl.contains("NOT NULL,") && !ddl.contains("PRIMARY KEY"), "{ddl}");
}
