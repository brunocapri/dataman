//! PostgreSQL and the servers that speak its protocol. See live_versions.rs.

use super::*;
use dataman_core::bounded::{Limits, Mode};
use dataman_core::server::Flavor;
use serde_json::Value;

/// The rows of `sql` with each cell as the grid shows it, NULL as None.
fn cells(rows: &[Vec<Option<String>>]) -> Vec<Vec<&str>> {
    rows.iter().map(|r| r.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect()).collect()
}

impl Target {
    fn major(&self) -> u32 {
        self.c.info.major()
    }

    fn yugabyte(&self) -> bool {
        self.c.info.flavor == Flavor::Yugabyte
    }

    /// The statements, one run each, the first error failing the test.
    fn script(&self, stmts: &[&str]) {
        for s in stmts {
            self.ok(s);
        }
    }

    /// Each value as the grid shows it next to the server's own text for it (`::text`).
    fn shown_as_server_says(&self, exprs: &[&str]) {
        let mut bad = Vec::new();
        for x in exprs {
            let rows = self.rows(&format!("SELECT {x}, ({x})::text"));
            if rows[0][0] != rows[0][1] {
                bad.push(format!("{x}: shows {:?}, the server says {:?}", rows[0][0], rows[0][1]));
            }
        }
        assert!(bad.is_empty(), "{}\n{}", self.c.info.version, bad.join("\n"));
    }
}

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

#[test]
fn grouping_sets_cube_and_grouping() {
    let Some(t) = target(Engine::Postgres) else { return };
    let rows = t.rows(
        "SELECT a, b, grouping(a, b), count(*), sum(v) FROM (VALUES ('x', 1, 10), ('x', 2, 20), ('y', 1, 30)) s(a, b, v)
         GROUP BY CUBE (a, b) ORDER BY grouping(a, b), a NULLS LAST, b NULLS LAST",
    );
    assert_eq!(
        cells(&rows),
        [
            ["x", "1", "0", "1", "10"],
            ["x", "2", "0", "1", "20"],
            ["y", "1", "0", "1", "30"],
            ["x", "NULL", "1", "2", "30"],
            ["y", "NULL", "1", "1", "30"],
            ["NULL", "1", "2", "2", "40"],
            ["NULL", "2", "2", "1", "20"],
            ["NULL", "NULL", "3", "3", "60"],
        ]
    );
    let rows = t.rows("SELECT a, b, count(*) FROM (VALUES (1, 1), (1, 2)) s(a, b) GROUP BY GROUPING SETS ((a), (b), ()) ORDER BY a NULLS LAST, b NULLS LAST");
    assert_eq!(cells(&rows), [["1", "NULL", "2"], ["NULL", "1", "1"], ["NULL", "2", "1"], ["NULL", "NULL", "2"]]);
}

#[test]
fn window_frames() {
    let Some(t) = target(Engine::Postgres) else { return };
    let rows = t.rows(
        "SELECT x,
                sum(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING),
                sum(x) OVER (ORDER BY x RANGE BETWEEN 2 PRECEDING AND CURRENT ROW),
                sum(x) OVER (ORDER BY x GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING EXCLUDE CURRENT ROW),
                first_value(x) OVER w, last_value(x) OVER w, nth_value(x, 2) OVER w,
                lag(x, 2, -1) OVER (ORDER BY x), lead(x) OVER (ORDER BY x),
                ntile(2) OVER (ORDER BY x), percent_rank() OVER (ORDER BY x), cume_dist() OVER (ORDER BY x)
         FROM unnest(ARRAY[1, 2, 2, 5]) x
         WINDOW w AS (ORDER BY x ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) ORDER BY x",
    );
    assert_eq!(
        cells(&rows),
        [
            ["1", "3", "1", "4", "1", "5", "2", "-1", "2", "1", "0", "0.25"],
            ["2", "5", "5", "7", "1", "5", "2", "-1", "2", "1", "0.3333333333333333", "0.75"],
            ["2", "9", "5", "7", "1", "5", "2", "1", "5", "2", "0.3333333333333333", "0.75"],
            ["5", "7", "5", "NULL", "1", "5", "2", "2", "NULL", "2", "1", "1"],
        ]
    );
}

#[test]
fn recursive_ctes_and_lateral() {
    let Some(t) = target(Engine::Postgres) else { return };
    let rows = t.rows(
        "WITH RECURSIVE fib(n, a, b) AS (SELECT 1, 0::numeric, 1::numeric UNION ALL SELECT n + 1, b, a + b FROM fib WHERE n < 120)
         SELECT n, a FROM fib WHERE n IN (1, 2, 120) ORDER BY n",
    );
    assert_eq!(cells(&rows), [["1", "0"], ["2", "1"], ["120", "3311648143516982017180081"]]);
    let rows = t.rows(
        "SELECT p.id, l.v FROM (VALUES (1), (2)) p(id)
         LEFT JOIN LATERAL (SELECT p.id * g AS v FROM generate_series(1, p.id) g ORDER BY g DESC LIMIT 1) l ON true ORDER BY p.id",
    );
    assert_eq!(cells(&rows), [["1", "1"], ["2", "4"]]);
    // SEARCH and CYCLE came with PostgreSQL 14.
    if t.major() >= 14 {
        let rows = t.rows(
            "WITH RECURSIVE g(id, next) AS (VALUES (1, 2), (2, 3), (3, 1)),
             walk(id) AS (SELECT 1 UNION ALL SELECT g.next FROM walk JOIN g ON g.id = walk.id)
             CYCLE id SET looped USING path
             SELECT id, looped FROM walk ORDER BY path",
        );
        assert_eq!(cells(&rows), [["1", "false"], ["2", "false"], ["3", "false"], ["1", "true"]]);
    }
}

#[test]
fn json_aggregates() {
    let Some(t) = target(Engine::Postgres) else { return };
    let rows = t.rows(
        "SELECT jsonb_agg(x ORDER BY x DESC), json_agg(x ORDER BY x), jsonb_object_agg('k' || x, x * 1.5), json_object_agg(x, NULL ORDER BY x),
                jsonb_build_object('a', array_agg(x ORDER BY x), 'n', NULL, 'q', 'it''s \"x\"'), jsonb_agg(DISTINCT x % 2)
         FROM generate_series(1, 3) x",
    );
    assert_eq!(
        cells(&rows),
        [[
            "[3, 2, 1]",
            "[1, 2, 3]",
            "{\"k1\": 1.5, \"k2\": 3.0, \"k3\": 4.5}",
            "{ \"1\" : null, \"2\" : null, \"3\" : null }",
            "{\"a\": [1, 2, 3], \"n\": null, \"q\": \"it's \\\"x\\\"\"}",
            "[0, 1]",
        ]]
    );
    let rows = t.rows("SELECT key, value, jsonb_typeof(value) FROM jsonb_each('{\"a\": [1, {\"b\": null}], \"c\": 1e400, \"d\": \"\\u00e9\"}') ORDER BY key");
    assert_eq!(cells(&rows)[0], ["a", "[1, {\"b\": null}]", "array"]);
    assert_eq!(rows[1][1].as_deref().map(str::len), Some(401), "1e400 kept whole: {:?}", rows[1][1]);
    assert_eq!(cells(&rows)[2], ["d", "\"é\"", "string"]);
    let rows = t.rows("SELECT jsonb_path_query_array('{\"a\": [1, 5, 9]}', '$.a[*] ? (@ > $min)', '{\"min\": 2}')");
    assert_eq!(cells(&rows), [["[5, 9]"]]);
}

#[test]
fn arrays_ranges_and_records() {
    let Some(t) = target(Engine::Postgres) else { return };
    t.shown_as_server_says(&[
        "ARRAY[[1, 2], [3, 4]]",
        "ARRAY[[[1], [2]], [[3], [NULL::int]]]",
        "'[0:1][-1:0]={{1,2},{3,4}}'::int[]",
        "ARRAY[ARRAY['a b', 'c\"d'], ARRAY['e,f', NULL]]",
        "ARRAY[]::text[]",
        "'{}'::int[][]",
        "ARRAY['1 day'::interval, '-1 mon'::interval]",
        "ARRAY[int4range(1, 3), 'empty'::int4range]",
        "'[1,1]'::int4range",
        "numrange(NULL, 2.5, '(]')",
        "tsrange('2024-01-01 00:00', 'infinity')",
        "ARRAY[ROW(1, 'x'), ROW(2, NULL)]",
        "ROW(ROW(1, ARRAY[2, 3]), '')",
    ]);
    // Multiranges came with PostgreSQL 14.
    if t.major() >= 14 {
        t.shown_as_server_says(&["'{[1,3), [2,5), [7,8)}'::int4multirange", "(SELECT range_agg(r) FROM (VALUES (int4range(1, 2)), (int4range(5, 6))) v(r))"]);
    }
}

/// An anonymous record (`ROW(…)`) whose fields include a type with no binary output: the server
/// fails a binary read of it, so it is read as text.
#[test]
fn records_holding_sendless_types() {
    let Some(t) = target(Engine::Postgres) else { return };
    assert_eq!(t.one("SELECT ROW('=r/postgres'::aclitem, 1)").as_deref(), Some("(=r/postgres,1)"));
    assert_eq!(t.one("SELECT ARRAY[ROW('=r/postgres'::aclitem)]").as_deref(), Some("{(=r/postgres)}"));
    let rows = t.rows("SELECT relname, ROW(relacl, relname) FROM pg_class WHERE relname = 'pg_class'");
    assert!(rows[0][1].as_deref().is_some_and(|r| r.starts_with("(\"{") && r.ends_with(",pg_class)")), "{rows:?}");
    let out = t.e.core.block_on(t.c.bounded(None, "SELECT ROW('=r/postgres'::aclitem, 2) AS r", vec![], Mode::Read, Limits::default(), std::future::pending())).unwrap();
    assert_eq!(out.rows, [[Value::String("(=r/postgres,2)".into())]]);
}

#[test]
fn numeric_extremes() {
    let Some(t) = target(Engine::Postgres) else { return };
    let mut exprs = vec![
        "'NaN'::numeric",
        "power(10::numeric, 130) + 0.5",
        "1::numeric / 3",
        "round(-2.5::numeric)",
        "'-0'::float8",
        "'1e-320'::float8",
        "1.0000001::float4",
        "(-2147483648)::int4",
        "(SELECT sum(x) FROM (VALUES (9223372036854775807::int8), (1)) v(x))",
        "(SELECT avg(x) FROM (VALUES (1::int8), (2)) v(x))",
        "(SELECT stddev(x) FROM (VALUES (1.0::float8), (2.0), (4.0)) v(x))",
        "'12.340'::numeric(10, 3)",
    ];
    // numeric's infinities came with PostgreSQL 14.
    if t.major() >= 14 {
        exprs.extend(["'Infinity'::numeric", "'-Infinity'::numeric * 1"]);
    }
    t.shown_as_server_says(&exprs);
    assert_eq!(t.one("SELECT 'NaN'::float8 = 'NaN'::float8").as_deref(), Some("true"), "NaN equals itself in PostgreSQL");
}

#[test]
fn intervals_and_odd_zones() {
    let Some(t) = target(Engine::Postgres) else { return };
    let mut exprs = vec![
        "interval '1 year -2 days 03:04:05.678'",
        "interval '-178000000 years'",
        "interval '1 microsecond'",
        "justify_interval(interval '35 days 25 hours')",
        "age(timestamp '2024-03-01', timestamp '2023-02-28 12:00')",
        "timestamp '2024-02-29 23:59:59.999999' + interval '1 microsecond'",
        "date '0001-01-01' - 1",
        "make_timestamp(294276, 12, 31, 23, 59, 59.999999)",
        "time with time zone '04:05:06.789-03:30'",
    ];
    // infinite intervals came with PostgreSQL 17.
    if t.major() >= 17 {
        exprs.push("interval 'infinity' - interval '-infinity'");
    }
    t.shown_as_server_says(&exprs);
    // A timestamptz shows in this machine's zone, whatever the session's: what it shows, read back
    // by the server, is the same instant (zones of :45 and :30, and local mean times with seconds
    // before zones were standard).
    let s = t.c.session(None);
    for zone in ["Asia/Kathmandu", "America/St_Johns", "Pacific/Chatham", "UTC"] {
        let set = s.execute(format!("SET TIME ZONE '{zone}'"));
        assert_eq!(t.e.core.block_on(set.wait_finished()), Phase::Done);
        for at in ["2024-06-30 23:30:00.000001+00", "1900-01-01 00:00+00", "1883-11-18 12:00+00", "0044-03-15 10:00+00 BC"] {
            let rs = s.execute(format!("SELECT timestamptz '{at}', ARRAY[timestamptz '{at}']"));
            rs.fetch_all();
            assert_eq!(t.e.core.block_on(rs.wait_finished()), Phase::Done, "{zone}: {:?}", rs.status().error);
            let snap = rs.snapshot();
            let shown = snap.text(0, 0).unwrap().into_owned();
            let same = t.one(&format!("SELECT timestamptz '{shown}' = timestamptz '{at}' AND '{}'::timestamptz[] = ARRAY[timestamptz '{at}']", snap.text(0, 1).unwrap()));
            assert_eq!(same.as_deref(), Some("true"), "{zone}: {at} shows as {shown}");
        }
    }
}

#[test]
fn odd_names_and_shapes() {
    let Some(t) = target(Engine::Postgres) else { return };
    let long = "a".repeat(63);
    let rs = t.ok(&format!("SELECT 1 AS a, 2 AS a, 4 AS \"{long}x\", 5 AS \"é ;\"\"q\", 6"));
    let names: Vec<String> = rs.columns().unwrap().iter().map(|c| c.name.clone()).collect();
    assert_eq!(names, ["a", "a", long.as_str(), "é ;\"q", "?column?"], "the server cuts a name at 63 bytes");
    assert_eq!(cells(&t.rows("SELECT 1 AS a, 2 AS a")), [["1", "2"]]);
    // No columns at all, rows still counted.
    let rs = t.ok("SELECT FROM generate_series(1, 3)");
    assert_eq!(rs.columns().map(|c| c.len()), Some(0));
    assert_eq!(rs.snapshot().rows(), 3);
    let rs = t.ok("SELECT 1 WHERE false");
    assert_eq!(rs.snapshot().rows(), 0);
}

/// Several statements in one run (the simple protocol): the first's rows are shown, every one runs,
/// and the notices of all of them reach the result.
#[test]
fn several_statements_notices_and_do_blocks() {
    let Some(t) = target(Engine::Postgres) else { return };
    t.ok("DROP TABLE IF EXISTS public.dv_pg_several");
    let rs = t.ok("SELECT 1 AS one, 'x' AS two; CREATE TABLE public.dv_pg_several AS SELECT 2 AS x; DO $$ BEGIN RAISE NOTICE 'hi %', (SELECT x FROM public.dv_pg_several); END $$; INSERT INTO public.dv_pg_several VALUES (3)");
    assert_eq!(rs.columns().map(|c| c.iter().map(|c| c.name.clone()).collect::<Vec<_>>()), Some(vec!["one".to_string(), "two".to_string()]));
    let s = rs.snapshot();
    assert_eq!((s.text(0, 0).as_deref(), s.text(0, 1).as_deref()), (Some("1"), Some("x")));
    assert_eq!(rs.messages().0.as_slice(), ["NOTICE: hi 2"]);
    assert_eq!(t.one("SELECT string_agg(x::text, ',' ORDER BY x) FROM public.dv_pg_several").as_deref(), Some("2,3"));
    // One that fails stops the run there, and says which error.
    let e = t.failed("INSERT INTO public.dv_pg_several VALUES (4); SELECT 1/0; INSERT INTO public.dv_pg_several VALUES (5)");
    assert_eq!(e.code.as_deref(), Some("22012"));
    assert_eq!(t.one("SELECT string_agg(x::text, ',' ORDER BY x) FROM public.dv_pg_several").as_deref(), Some("2,3"), "one implicit transaction, rolled back whole");
    t.ok("DROP TABLE public.dv_pg_several");
}

#[test]
fn parameters_bind() {
    let Some(t) = target(Engine::Postgres) else { return };
    let rs = t.c.session(None).execute_with("SELECT $1::int + 1, $2::text || '!', $3::jsonb -> 'a', $4::date + 1, $5::text IS NULL", vec![Some("41".into()), Some("é".into()), Some("{\"a\": [1]}".into()), Some("2024-02-28".into()), None]);
    rs.fetch_all();
    assert_eq!(t.e.core.block_on(rs.wait_finished()), Phase::Done, "{:?}", rs.status().error);
    let s = rs.snapshot();
    let got: Vec<String> = (0..5).map(|c| s.text(0, c).unwrap().into_owned()).collect();
    assert_eq!(got, ["42", "é!", "[1]", "2024-02-29", "true"]);
}

/// The catalog the explorer and the agents read, compared with what the server says.
#[test]
fn catalog_reads_what_the_server_has() {
    let Some(t) = target(Engine::Postgres) else { return };
    let sc = "dv_pg_cat";
    t.ok(&format!("DROP SCHEMA IF EXISTS {sc} CASCADE"));
    let mut ddl = vec![
        format!("CREATE SCHEMA {sc}"),
        format!("CREATE TYPE {sc}.mood AS ENUM ('a', 'b c')"),
        format!("CREATE DOMAIN {sc}.pos AS int CHECK (VALUE > 0)"),
        format!(
            "CREATE TABLE {sc}.t (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, n {sc}.pos NOT NULL DEFAULT 1, m {sc}.mood, \
             price numeric(10, 2), twice numeric GENERATED ALWAYS AS (price * 2) STORED, \"Odd \"\"name\" text UNIQUE, CHECK (price >= 0))"
        ),
        format!("CREATE INDEX t_lower ON {sc}.t (lower(\"Odd \"\"name\")) WHERE price > 0"),
        format!("CREATE TABLE {sc}.child (id int PRIMARY KEY, t_id bigint REFERENCES {sc}.t (id) ON DELETE CASCADE)"),
        format!("CREATE VIEW {sc}.v AS SELECT id, twice FROM {sc}.t WHERE m = 'a'"),
        format!("CREATE MATERIALIZED VIEW {sc}.mv AS SELECT count(*) AS n FROM {sc}.t"),
        format!("CREATE FUNCTION {sc}.f(a int, VARIADIC b int[] DEFAULT '{{}}') RETURNS TABLE (x int) LANGUAGE sql AS $$ SELECT a $$"),
        format!("CREATE PROCEDURE {sc}.p() LANGUAGE plpgsql AS $$ BEGIN END $$"),
        format!("CREATE FUNCTION {sc}.trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$"),
        format!("CREATE TRIGGER t_trg BEFORE INSERT OR UPDATE OF price ON {sc}.t FOR EACH ROW EXECUTE FUNCTION {sc}.trg()"),
        format!("CREATE SEQUENCE {sc}.s START 7 INCREMENT 3"),
        format!("CREATE TABLE {sc}.part (id int, d date) PARTITION BY RANGE (d)"),
        format!("CREATE TABLE {sc}.part_2024 PARTITION OF {sc}.part FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')"),
        format!("INSERT INTO {sc}.t (n, m, price, \"Odd \"\"name\") VALUES (1, 'a', 1.5, 'x'), (2, 'b c', 2, 'y')"),
    ];
    if t.yugabyte() {
        // YugabyteDB keeps no materialized view's data apart, and has no partition DEFAULT: what
        // it lacks is left out, the rest is the same.
        ddl.retain(|s| !s.contains("MATERIALIZED"));
    }
    t.script(&ddl.iter().map(String::as_str).collect::<Vec<_>>());
    let (e, c, db) = (&t.e, &t.c, t.c.database().to_string());

    let rels = e.core.block_on(c.relations(&db, sc)).unwrap();
    let mut names: Vec<&str> = rels.iter().map(|r| r.name.as_str()).collect();
    names.sort();
    let mut want = vec!["child", "mv", "part", "part_2024", "t", "v"];
    if t.yugabyte() {
        want.retain(|n| *n != "mv");
    }
    assert_eq!(names, want);

    let cols = e.core.block_on(c.columns(&db, sc, "t")).unwrap();
    let shown: Vec<(&str, &str, Option<bool>, bool)> = cols.iter().map(|c| (c.name.as_str(), c.type_name.as_str(), c.nullable, c.primary_key)).collect();
    assert_eq!(
        shown,
        [
            ("id", "bigint", Some(false), true),
            ("n", &format!("{sc}.pos") as &str, Some(false), false),
            ("m", &format!("{sc}.mood"), Some(true), false),
            ("price", "numeric(10,2)", Some(true), false),
            ("twice", "numeric", Some(true), false),
            ("Odd \"name", "text", Some(true), false),
        ]
    );
    let idx = e.core.block_on(c.indexes(&db, sc, "t")).unwrap();
    assert!(idx.iter().any(|i| i.primary && i.columns == ["id"]), "{idx:?}");
    assert!(idx.iter().any(|i| i.name == "t_lower" && i.definition.as_deref().is_some_and(|d| d.contains("WHERE (price > (0)::numeric)"))), "{idx:?}");

    let extras = e.core.block_on(c.table_extras(&db, sc, "t")).unwrap();
    let cons = extras.constraints.expect("constraints read");
    assert_eq!(cons.len(), 3, "primary key, unique, check: {cons:?}");
    let trg = extras.triggers.expect("triggers read");
    assert_eq!(trg.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["t_trg"]);
    let parts = e.core.block_on(c.table_extras(&db, sc, "part")).unwrap().partitions.expect("partitions read");
    assert_eq!(parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["part_2024"]);

    let fns = e.core.block_on(c.functions(&db, sc)).unwrap();
    let mut fnames: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
    fnames.sort();
    assert_eq!(fnames, ["f", "p", "trg"]);
    let seqs = e.core.block_on(c.sequences(&db, sc)).unwrap();
    assert!(seqs.iter().any(|s| s.name == "s"), "{seqs:?}");
    let types = e.core.block_on(c.types(&db, sc)).unwrap();
    let mut tnames: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
    tnames.sort();
    assert_eq!(tnames, ["mood", "pos"]);
    let counts = e.core.block_on(c.object_counts(&db, sc)).unwrap();
    // The identity column's own sequence counts too, as in the list.
    assert_eq!(seqs.len(), 2, "{seqs:?}");
    assert_eq!((counts.functions, counts.sequences, counts.types, counts.triggers), (Some(3), Some(2), Some(2), Some(1)));
    let exts = e.core.block_on(c.extensions(&db)).unwrap();
    assert!(exts.iter().any(|x| x.name == "plpgsql"), "{exts:?}");
    let roles = e.core.block_on(c.roles()).unwrap();
    assert!(roles.iter().any(|r| r.name == t.c.info.user), "{roles:?}");

    // The DDL Dataman writes makes the same table again, in another schema.
    let ddl = e.core.block_on(c.table_ddl(&db, sc, "t")).unwrap();
    let again = ddl.replace(&format!("CREATE TABLE {sc}.t ("), "CREATE TABLE dv_pg_cat2.t (").replace(&format!(" ON {sc}.t "), " ON dv_pg_cat2.t ");
    assert!(again.contains("dv_pg_cat2.t (") && again.contains("ON dv_pg_cat2.t "), "{ddl}");
    t.ok("DROP SCHEMA IF EXISTS dv_pg_cat2 CASCADE");
    t.ok("CREATE SCHEMA dv_pg_cat2");
    t.ok(&again);
    let cols2 = e.core.block_on(c.columns(&db, "dv_pg_cat2", "t")).unwrap();
    assert_eq!(cols2, cols, "{again}");
    let idx2 = e.core.block_on(c.indexes(&db, "dv_pg_cat2", "t")).unwrap();
    assert_eq!(idx2.iter().map(|i| i.definition.clone().map(|d| d.replace("dv_pg_cat2.", ""))).collect::<Vec<_>>(), idx.iter().map(|i| i.definition.clone().map(|d| d.replace(&format!("{sc}."), ""))).collect::<Vec<_>>());
    t.ok("DROP SCHEMA dv_pg_cat2 CASCADE");
    let view = e.core.block_on(c.view_definition(&db, sc, "v")).unwrap().expect("a view's definition");
    assert!(view.contains("WHERE") && view.contains("twice"), "{view}");

    // The agents' path: the generated column and what it is computed from.
    let generated = e.core.block_on(c.generated_columns(&db, std::future::pending())).unwrap();
    let g = generated.iter().find(|g| g.schema == sc && g.table == "t").expect("the generated column read");
    assert_eq!((g.column.as_str(), g.from.as_slice()), ("twice", &["price".to_string()][..]));

    // The MCP read path, bounded, on the same table.
    let out = e
        .core
        .block_on(c.bounded(Some(&db), &format!("SELECT id, m, twice, \"Odd \"\"name\" FROM {sc}.t ORDER BY id"), vec![], Mode::Read, Limits::default(), std::future::pending()))
        .unwrap();
    assert_eq!(out.rows.len(), 2);
    assert_eq!(out.rows[1][1], Value::String("b c".into()));
    // As the server keeps it (YugabyteDB drops the scale of a stored numeric: 3, not 3.00).
    let twice = t.one(&format!("SELECT twice::text FROM {sc}.t WHERE id = 1")).unwrap();
    assert_eq!(out.rows[0][2].to_string().trim_matches('"'), twice);
    t.ok(&format!("DROP SCHEMA {sc} CASCADE"));
}

#[test]
fn activity_reads_this_session() {
    let Some(t) = target(Engine::Postgres) else { return };
    let db = t.c.database().to_string();
    let sessions = t.e.core.block_on(t.c.activity_sessions(&db)).unwrap();
    assert!(sessions.iter().any(|s| s.is_self), "{sessions:?}");
    t.e.core.block_on(t.c.activity_locks(&db)).unwrap();
    t.e.core.block_on(t.c.activity_tables(&db, true)).unwrap();
    t.e.core.block_on(t.c.activity_statements(&db)).unwrap();
}

/// A catalog the login may not read is an error, never an empty list kept as the truth.
#[test]
fn a_refused_catalog_is_an_error() {
    let Some(t) = target(Engine::Postgres) else { return };
    if t.yugabyte() {
        // Its catalog's grants are its own; the rest of the battery covers it.
        return;
    }
    t.ok("DROP ROLE IF EXISTS dv_pg_lowly");
    t.ok("CREATE ROLE dv_pg_lowly LOGIN PASSWORD 'lowly'");
    t.ok("REVOKE SELECT ON pg_catalog.pg_roles, pg_catalog.pg_extension FROM PUBLIC");
    let p = dataman_core::ConnectionProfile { user: "dv_pg_lowly".into(), ..t.c.profile.clone() };
    let low = t.e.core.block_on(t.e.core.connect(p, Some("lowly".into())));
    let refused = low.as_ref().map(|low| (t.e.core.block_on(low.roles()).map_err(|e| format!("{e:#}")), t.e.core.block_on(low.extensions(low.database())).map_err(|e| format!("{e:#}")))).map_err(|e| format!("{e:#}"));
    t.ok("GRANT SELECT ON pg_catalog.pg_roles, pg_catalog.pg_extension TO PUBLIC");
    let low = low.unwrap();
    let (roles, exts) = refused.unwrap();
    assert!(roles.as_ref().is_err_and(|e| e.contains("permission denied")), "{roles:?}");
    assert!(exts.as_ref().is_err_and(|e| e.contains("permission denied")), "{exts:?}");
    // The refusal was not kept: readable again, they list.
    assert!(t.e.core.block_on(low.roles()).is_ok_and(|r| r.iter().any(|r| r.name == "dv_pg_lowly")));
    assert!(t.e.core.block_on(low.extensions(low.database())).is_ok_and(|x| x.iter().any(|x| x.name == "plpgsql")));
    drop(low);
    t.ok("DROP ROLE dv_pg_lowly");
}
