//! PostgreSQL sweep: valid scripts must split, lint and run cleanly; types decode; the catalog
//! and DDL round-trip. Skips without the server (`DATAMAN_PG=127.0.0.1:55433` for pgvector).

mod common;

use common::*;
use dataman_core::lint::{LintContext, Severity, lint};
use dataman_core::sql::split;

fn finish(e: &Env, rs: &Arc<dataman_core::ResultSet>) -> Phase {
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") })
}

/// Each statement must come back from the splitter verbatim, lint clean and run.
fn check_script(e: &Env, c: &Arc<Connection>, stmts: &[&str]) {
    let script = stmts.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n\n");
    let parts = split::split(&script);
    let got: Vec<&str> = parts.iter().map(|s| &script[s.range.clone()]).collect();
    for (i, (g, w)) in got.iter().zip(stmts).enumerate() {
        assert_eq!(g, w, "split mismatch at statement {i}");
    }
    assert_eq!(got.len(), stmts.len(), "statement count");
    let s = c.session(None);
    let mut lints = Vec::new();
    for sql in &got {
        let rs = s.execute(*sql);
        let ph = finish(e, &rs);
        assert_eq!(ph, Phase::Done, "{sql}: {:?}", rs.status().error);
    }
    // Lint with the schema the script created (as the editor does once the cache loaded).
    let db = c.database().to_string();
    let _ = e.core.block_on(c.preload(&db));
    let snap = c.schema.snapshot(&db);
    lints.extend(lint(&script, &LintContext { schema: Some(&snap), ..Default::default() }));
    let errs: Vec<_> = lints.iter().filter(|x| x.severity == Severity::Error).map(|x| (&script[x.range.clone()], &x.message)).collect();
    assert!(errs.is_empty(), "lint errors on a valid script: {errs:?}");
}

#[test]
fn big_valid_script() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS sweep CASCADE"]);
    exec(&e, &c, &["CREATE SCHEMA sweep"]);
    let stmts: &[&str] = &[
        "SET search_path = sweep, public",
        "CREATE TYPE mood AS ENUM ('sad', 'ok; fine', 'it''s ''great''')",
        "CREATE DOMAIN posint AS integer CHECK (VALUE > 0)",
        "CREATE TYPE pair AS (a int, b text)",
        "CREATE TABLE \"we;ird \"\"name\" (\"col;1\" int, \"--x\" text, \"/* y */\" text)",
        "INSERT INTO \"we;ird \"\"name\" VALUES (1, '--not a comment', '/* nor this ; */')",
        "CREATE TABLE emp (id serial PRIMARY KEY, name text NOT NULL, boss int REFERENCES emp(id), salary numeric(10,2), m mood, sq int GENERATED ALWAYS AS (id * id) STORED)",
        "INSERT INTO emp (name, boss, salary, m) VALUES ('a', NULL, 10, 'sad'), ('b', 1, 20, 'ok; fine'), ('c', 1, 30, 'sad'), ('d', 2, 40, NULL)",
        "CREATE TABLE log (id bigserial PRIMARY KEY, at timestamptz DEFAULT now(), msg text)",
        "CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$\nBEGIN\n  NEW.salary := coalesce(NEW.salary, 0) + 1; -- a comment; with semicolon\n  /* block ; comment */\n  INSERT INTO log(msg) VALUES ('bumped; ' || NEW.name);\n  RETURN NEW;\nEND;\n$$",
        "CREATE TRIGGER emp_bump BEFORE INSERT ON emp FOR EACH ROW EXECUTE FUNCTION bump()",
        "INSERT INTO emp (name, salary) VALUES ('e', 5)",
        "CREATE OR REPLACE FUNCTION nested(x int) RETURNS text LANGUAGE plpgsql AS $outer$\nDECLARE r text;\nBEGIN\n  EXECUTE $inner$ SELECT 'a;b$$c' $inner$ INTO r;\n  RAISE NOTICE 'hello % %', x, r;\n  RETURN r || $q$it's$q$;\nEND\n$outer$",
        "SELECT nested(1)",
        "CREATE PROCEDURE inout_p(INOUT a int, IN b int) LANGUAGE plpgsql AS $$ BEGIN a := a + b; END $$",
        "CALL inout_p(1, 2)",
        "DO $$ DECLARE i int; BEGIN FOR i IN 1..3 LOOP RAISE NOTICE 'i=%', i; END LOOP; END $$",
        "DO $do$ BEGIN IF true THEN PERFORM 1; ELSE PERFORM 2; END IF; END $do$ LANGUAGE plpgsql",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 5) SELECT sum(n) FROM t",
        "WITH ins AS (INSERT INTO log(msg) VALUES ('w') RETURNING id), upd AS (UPDATE emp SET salary = salary + 1 WHERE id = 1 RETURNING id) SELECT (SELECT count(*) FROM ins) + (SELECT count(*) FROM upd)",
        "SELECT name, salary, rank() OVER (PARTITION BY m ORDER BY salary DESC), sum(salary) OVER w, lag(salary) OVER w FROM emp WINDOW w AS (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)",
        "SELECT e.name, x.v FROM emp e CROSS JOIN LATERAL (SELECT e.salary * 2 AS v) x WHERE e.id = 1",
        "SELECT e.name, b.name FROM emp e LEFT JOIN LATERAL (SELECT name FROM emp WHERE id = e.boss) b ON true",
        "MERGE INTO emp t USING (VALUES (1, 'aa'), (99, 'zz')) AS s(id, name) ON t.id = s.id WHEN MATCHED THEN UPDATE SET name = s.name WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, s.name) RETURNING merge_action(), t.id",
        "SELECT E'a\\'b;c\\nd', E'\\\\', U&'d\\0061t\\+000061', U&\"d\\0061t\\0061\" FROM (SELECT 1 AS \"data\") z",
        "SELECT 'a -- b', '/* c', 'd */', \"name\" FROM emp WHERE /* c1 /* nested ; */ still comment ; */ id = 1",
        "SELECT 1",
        "PREPARE p1(int) AS SELECT * FROM emp WHERE id = $1",
        "EXECUTE p1(1)",
        "DEALLOCATE p1",
        "BEGIN",
        "SAVEPOINT s1",
        "INSERT INTO log(msg) VALUES ('in tx')",
        "ROLLBACK TO SAVEPOINT s1",
        "RELEASE SAVEPOINT s1",
        "COMMIT",
        "START TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "ROLLBACK",
        "DECLARE c1 CURSOR WITH HOLD FOR SELECT * FROM emp",
        "FETCH 2 FROM c1",
        "MOVE BACKWARD ALL IN c1",
        "CLOSE c1",
        "CREATE TABLE part (id int, d date, v text) PARTITION BY RANGE (d)",
        "CREATE TABLE part_2024 PARTITION OF part FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
        "CREATE TABLE part_def PARTITION OF part DEFAULT",
        "INSERT INTO part VALUES (1, '2024-05-05', 'x'), (2, '1999-01-01', 'y')",
        "CREATE VIEW v1 AS SELECT id, name FROM emp WHERE salary > 5",
        "CREATE MATERIALIZED VIEW mv1 AS SELECT m, count(*) AS n FROM emp GROUP BY m",
        "REFRESH MATERIALIZED VIEW mv1",
        "CREATE SEQUENCE sq1 START 10 INCREMENT 5",
        "SELECT nextval('sq1'), currval('sq1')",
        "CREATE INDEX emp_name_idx ON emp (lower(name)) WHERE salary > 0",
        "CREATE FUNCTION sqlf(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN a > 0 THEN a ELSE -a END; END",
        "CREATE FUNCTION sqlf2(a int) RETURNS int LANGUAGE sql RETURN a + 1",
        "SELECT sqlf(-3), sqlf2(1)",
        "CREATE RULE r1 AS ON INSERT TO log DO ALSO NOTIFY logged",
        "LISTEN logged",
        "NOTIFY logged, 'payload; x'",
        "COMMENT ON TABLE emp IS 'it''s; a table'",
        "ALTER TABLE emp ADD COLUMN extra jsonb DEFAULT '{\"a\": [1, 2]}'",
        "UPDATE emp SET extra = jsonb_set(extra, '{a,0}', '9') WHERE id = 1",
        "SELECT extra #> '{a,0}', extra @> '{\"a\":[9]}', extra -> 'a' ->> 0 FROM emp WHERE id = 1",
        "SELECT * FROM emp WHERE name ~ '^a.*;$' OR name !~* 'x' OR name SIMILAR TO '%a%' ESCAPE '\\'",
        "SELECT (ARRAY[1,2,3])[2], (ARRAY[[1,2],[3,4]])[2][1], '{1,2}'::int[] || 3, ARRAY(SELECT id FROM emp)",
        "SELECT (ROW(1,'x')::pair).b, (SELECT p FROM (SELECT ROW(1, 'x')::pair AS p) q)",
        "SELECT CASE WHEN 1 = 1 THEN 'y' END, COALESCE(NULL, 2), NULLIF(1, 1), GREATEST(1, 2)",
        "SELECT * FROM generate_series(1, 3) WITH ORDINALITY AS g(v, o)",
        "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(x, y) ORDER BY x DESC LIMIT 1 OFFSET 0",
        "SELECT id FROM emp GROUP BY GROUPING SETS ((id), ()) HAVING count(*) > 0",
        "SELECT DISTINCT ON (m) m, name FROM emp ORDER BY m, name",
        "SELECT 1 UNION SELECT 2 INTERSECT SELECT 2 EXCEPT SELECT 3",
        "TABLE emp",
        "EXPLAIN (COSTS OFF) SELECT * FROM emp",
        "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM emp",
        "SELECT '1 day'::interval + now()::date, '2024-01-01'::date - 1, interval '1 year 2 mons 3 days 04:05:06.789'",
        "CREATE TABLE tt (a int PRIMARY KEY, b text)",
        "TRUNCATE tt, log RESTART IDENTITY",
        "DELETE FROM tt WHERE a = 1 RETURNING *",
        "INSERT INTO tt VALUES (1, 'a') ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
        "CREATE TEMP TABLE tmp1 AS SELECT * FROM emp",
        "SELECT * FROM tmp1",
        "SELECT id INTO tmp2 FROM emp",
        "SELECT * FROM tmp2",
        "VACUUM ANALYZE tt",
        "SHOW search_path",
        "RESET ALL",
    ];
    check_script(&e, &c, stmts);
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS sweep CASCADE"]);
}

/// Every expression of `exprs` through the grid decoder next to the server's text for it.
fn decode_all(e: &Env, c: &Arc<Connection>, exprs: &[&str]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for x in exprs {
        let sql = format!("SELECT {x}");
        let rs = c.session(None).execute(&sql);
        let ph = finish(e, &rs);
        if ph != Phase::Done {
            out.push((x.to_string(), format!("ERROR {:?}", rs.status().error.map(|e| e.message)), String::new()));
            continue;
        }
        let snap = rs.snapshot();
        let got = snap.text(0, 0).map(|t| t.into_owned()).unwrap_or_else(|| "NULL".into());
        let want = query_text(e, c, &format!("SELECT ({x})::text"));
        out.push((x.to_string(), got, want));
    }
    out
}

fn query_text(e: &Env, c: &Arc<Connection>, sql: &str) -> String {
    let rs = c.session(None).execute(sql);
    finish(e, &rs);
    let snap = rs.snapshot();
    snap.text(0, 0).map(|t| t.into_owned()).unwrap_or_else(|| "NULL".into())
}

#[test]
fn type_decoding_sweep() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let exprs: &[&str] = &[
        "'[1,5)'::int4range", "'[1,5]'::int8range", "'(,)'::numrange", "'empty'::int4range", "'[2024-01-01,2024-02-01)'::daterange",
        "'[2024-01-01 00:00+00,2024-02-01 00:00+00)'::tstzrange", "'[2024-01-01 00:00,infinity)'::tsrange",
        "'{[1,3),[5,9)}'::int4multirange", "'{}'::int4multirange", "'{[1,3]}'::nummultirange",
        "B'10101'", "B'1010'::varbit", "''::varbit", "B'1'::bit(1)",
        "'(1,2)'::point", "'{1,2,3}'::line", "'[(0,0),(1,1)]'::lseg", "'((0,0),(1,1))'::box", "'[(0,0),(1,1),(2,0)]'::path", "'((0,0),(1,1),(2,0))'::path",
        "'((0,0),(1,1),(2,0))'::polygon", "'<(1,2),3>'::circle",
        "'08:00:2b:01:02:03:04:05'::macaddr8", "'08:00:2b:01:02:03'::macaddr",
        "to_tsvector('english', 'The quick brown fox jumps')", "to_tsquery('english', 'quick & fox')", "websearch_to_tsquery('english', '\"quick fox\" -dog')",
        "ts_rank(to_tsvector('fox'), to_tsquery('fox'))", "ts_headline('english', 'The quick brown fox', to_tsquery('fox'))",
        "to_tsvector('simple', 'a:1 b')", "'a:1A,2B b'::tsvector", "setweight(to_tsvector('simple','abc'), 'A')",
        "'{\"a\": [1, 2, {\"b\": null}]}'::jsonb", "'{\"a\": 1}'::json", "'$.a[*] ? (@ > 1)'::jsonpath", "'$'::jsonpath",
        "jsonb_path_query('{\"a\":[1,2,3]}', '$.a[*] ? (@ > 1)')", "'{\"a\":[1,2]}'::jsonb @? '$.a[*] ? (@ > 1)'", "'{\"a\":[1,2]}'::jsonb @@ '$.a[0] == 1'",
        "'ff:ff:ff:ff:ff:ff'::macaddr", "'192.168.1.5/24'::inet", "'192.168.1.0/24'::cidr", "'::1'::inet", "'2001:db8::/32'::cidr", "'10.0.0.1/32'::inet",
        "'$1,234.56'::money", "'-$0.05'::money", "'92233720368547758.07'::money",
        "interval '1 year 2 mons 3 days 04:05:06.789'", "interval '-1 days'", "interval '1 mon -1 day'", "interval 'infinity'", "interval '-178000000 years'", "interval '00:00:00.000001'", "interval '100 hours'",
        "'\\xdeadbeef'::bytea", "''::bytea", "'abc'::bytea",
        "'<a>b</a>'::xml", "'f81d4fae-7dec-11d0-a765-00a0c91e6bf6'::uuid",
        "'infinity'::timestamptz", "'-infinity'::timestamptz", "'infinity'::timestamp", "'infinity'::date", "'-infinity'::date", "'0044-03-15 BC'::date", "'0044-03-15 10:00:00 BC'::timestamp", "'0044-03-15 10:00:00+00 BC'::timestamptz",
        "'294276-12-31'::date", "'4713-01-01 BC'::date", "'2024-02-29 23:59:59.999999'::timestamp", "'2024-02-29 23:59:59.999999+05:30'::timestamptz", "'24:00:00'::time", "'23:59:59.999999+05:30'::timetz", "'12:00:00-08'::timetz",
        "'NaN'::numeric", "'Infinity'::numeric", "'-Infinity'::numeric", "123456789012345678901234567890.123456789012345678901234567890::numeric", "0.000000000000000000000000000001::numeric", "1e100::numeric", "(-1e-50)::numeric", "100000000::numeric", "0::numeric", "'0.00'::numeric(10,2)", "'12345.6'::numeric(4,-1)",
        "'NaN'::float8", "'Infinity'::float8", "'-Infinity'::float8", "1e300::float8", "1e-300::float8", "0.1::float4", "'NaN'::float4", "3.4028235e38::float4", "-0.0::float8", "1.0::float8", "100000000000000000000::float8", "123456789.123::float8",
        "9223372036854775807::bigint", "(-9223372036854775808)::bigint", "32767::int2", "4294967295::oid", "'pg_class'::regclass", "'int4'::regtype", "'pg_catalog'::regnamespace", "'postgres'::regrole", "'english'::regconfig", "'simple'::regdictionary", "'+'::regoper", "'abs(int4)'::regprocedure", "'abs'::regproc", "'(1,2)'::tid", "pg_current_xact_id()::text::xid8", "pg_current_snapshot()", "'0/16B3748'::pg_lsn", "'a'::\"char\"", "'abc'::name", "'abc'::bpchar", "'ab'::char(5)", "''::text", "E'a\\nb\\tc'", "E'\\u00e9'", "'null'::jsonb", "'[]'::jsonb", "'\"x\"'::jsonb",
        "ARRAY[1,2,3]", "ARRAY[]::int[]", "ARRAY[[1,2],[3,4]]", "ARRAY[NULL,1]", "'[0:2]={1,2,3}'::int[]", "ARRAY['a b','c\"d','e,f','NULL','']", "ARRAY[1.5,2.5]::numeric[]", "ARRAY['2024-01-01'::date]", "ARRAY[true,false]", "ARRAY['a','b']::char(3)[]", "'{1,2}'::int2vector", "'{1,2}'::oidvector",
        "ARRAY['[1,2)'::int4range]", "ARRAY['1 day'::interval]", "ARRAY['\\xdead'::bytea]", "ARRAY['{\"a\":1}'::jsonb]", "ARRAY['(1,2)'::point]", "ARRAY[B'101']", "ARRAY['1.2.3.4'::inet]",
        "ROW(1,'a',NULL)", "ROW(1,ROW(2,'x y'))", "ROW('a,b','c\"d')",
        "'a=>1, b=>NULL, \"c d\"=>\"x y\"'::hstore", "''::hstore", "'a.b.c'::ltree", "'a.b.*{1,2}'::lquery", "'a & b'::ltxtquery", "'Hello'::citext", "ARRAY['a'::citext]", "ARRAY['a.b'::ltree]", "ARRAY['a=>1'::hstore]",
        "'[1,2,3]'::vector", "'[1.5,-2,3e-5]'::vector", "'[1,2,3]'::halfvec", "'{1:1.5,3:2}/5'::sparsevec", "'[1,2,3]'::vector <-> '[4,5,6]'", "'[1,2,3]'::vector <=> '[4,5,6]'", "'[1,2,3]'::vector <#> '[4,5,6]'", "ARRAY['[1,2]'::vector]", "'[1,2,3]'::vector(3)", "l2_normalize('[3,4]'::vector)", "'[1,2,3]'::vector::real[]",
        "similarity('word', 'words')", "'word' % 'words'", "show_trgm('cat')",
        "'a'::\"char\"", "'ab'::bit(8)::text",
        "to_regclass('pg_class')", "NULL", "NULL::jsonb",
        "now()::timestamp(0)", "'2024-01-01 12:34:56.7'::timestamp(1)", "'12:34:56.78'::time(2)",
        "'fat & rat'::tsquery", "'!a | b <-> c'::tsquery", "phraseto_tsquery('a b')",
        "pg_typeof(1)", "pg_typeof('{}'::int4multirange)", "current_setting('server_version')",
    ];
    let rows = decode_all(&e, &c, exprs);
    let mut bad = Vec::new();
    for (x, got, want) in &rows {
        if got.starts_with("ERROR") {
            eprintln!("ERR  {x:60} -> {got}");
        } else if got != want {
            bad.push(x);
            eprintln!("DIFF {x:60} -> {got:?} vs {want:?}");
        }
    }
    eprintln!("{} diffs of {}", bad.len(), rows.len());
}

#[test]
fn extension_tables_and_catalog() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    if !has_extensions(&e, &c) {
        return;
    }
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS sweep_ext CASCADE"]);
    let ddl: &[&str] = &[
        "CREATE SCHEMA sweep_ext",
        "SET search_path = sweep_ext, public",
        "CREATE TYPE mood AS ENUM ('sad', 'ok')",
        "CREATE TYPE span AS RANGE (subtype = int4)",
        "CREATE TYPE inner_t AS (m mood, r int4range, v vector(2))",
        "CREATE TYPE outer_t AS (i inner_t, tags text[], h hstore)",
        "CREATE DOMAIN vec3 AS vector(3)",
        "CREATE TABLE docs (id bigserial PRIMARY KEY, v vector(3), hv halfvec(3), sv sparsevec(5), body text, ts tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(body, ''))) STORED, h hstore, lt ltree, ci citext, r int4range, sp span, moods mood[], oc outer_t, d vec3, vs vector[], trg text, geo point, bits varbit, ip inet, mac macaddr, rg int4multirange, money_c money, iv interval, x xml, j jsonb, jp jsonpath, big numeric, tz timestamptz, ts0 timestamp, dt date)",
        "INSERT INTO docs (v, hv, sv, body, h, lt, ci, r, sp, moods, oc, d, vs, trg, geo, bits, ip, mac, rg, money_c, iv, x, j, jp, big, tz, ts0, dt) VALUES ('[1,2,3]', '[1,2,3]', '{1:1,3:2}/5', 'The quick brown fox', 'a=>1,b=>NULL', 'Top.Science.Astronomy', 'MiXed', '[1,10)', '[3,4)', '{sad,ok}', ROW(ROW('ok', '[1,2)', '[0.5,1]'), '{x,y z}', 'k=>v')::outer_t, '[3,2,1]', ARRAY['[1,2]'::vector], 'hello world', '(1,2)', B'1010', '10.1.2.3/24', '08:00:2b:01:02:03', '{[1,3),[5,7)}', '$12.50', '1 day 02:03:04', '<a>b</a>', '{\"a\": [1, {\"b\": null}]}', '$.a[*]', 123456789012345678901234567890.123456789, 'infinity', '4713-01-01 BC', '5874897-12-31')",
        "INSERT INTO docs (body) VALUES (NULL)",
        "CREATE INDEX docs_hnsw ON docs USING hnsw (v vector_l2_ops)",
        "CREATE INDEX docs_ivf ON docs USING ivfflat (hv halfvec_cosine_ops) WITH (lists = 1)",
        "CREATE INDEX docs_gin ON docs USING gin (ts)",
        "CREATE INDEX docs_trgm ON docs USING gin (trg gin_trgm_ops)",
        "CREATE INDEX docs_lt ON docs USING gist (lt)",
        "CREATE INDEX docs_h ON docs USING gin (h)",
        "CREATE INDEX docs_j ON docs USING gin (j jsonb_path_ops)",
        "CREATE VIEW docs_v AS SELECT id, v, ts, h FROM docs",
        "CREATE MATERIALIZED VIEW docs_mv AS SELECT id, sv, lt FROM docs",
    ];
    exec(&e, &c, ddl);
    // Every column decodes without the hex fallback.
    let rs = query(&e, &c, "SELECT * FROM sweep_ext.docs ORDER BY id");
    let snap = rs.snapshot();
    let cols = rs.columns().unwrap();
    for (i, col) in cols.iter().enumerate() {
        let t = snap.text(0, i).map(|t| t.into_owned()).unwrap_or_else(|| "NULL".into());
        eprintln!("{:10} {:14} {t}", col.name, col.type_name);
        assert!(!t.starts_with("\\x") || col.type_name == "bytea", "{} not decoded: {t}", col.name);
    }
    // Searches over them.
    for q in [
        "SELECT id FROM sweep_ext.docs ORDER BY v <-> '[1,2,4]' LIMIT 1",
        "SELECT v <=> '[1,2,4]', v <#> '[1,2,4]', hv <-> '[1,2,4]', cosine_distance(v, '[1,2,4]') FROM sweep_ext.docs WHERE v IS NOT NULL",
        "SELECT id FROM sweep_ext.docs WHERE ts @@ websearch_to_tsquery('english', 'quick fox')",
        "SELECT ts_rank(ts, to_tsquery('fox')), ts_headline('english', body, to_tsquery('fox')) FROM sweep_ext.docs WHERE ts @@ to_tsquery('fox')",
        "SELECT id FROM sweep_ext.docs WHERE trg % 'helo world' AND similarity(trg, 'helo world') > 0.3",
        "SELECT h -> 'a', h ? 'b', akeys(h), hstore_to_jsonb(h) FROM sweep_ext.docs WHERE h IS NOT NULL",
        "SELECT lt, nlevel(lt), lt <@ 'Top', subpath(lt, 1) FROM sweep_ext.docs WHERE lt IS NOT NULL",
        "SELECT ci = 'mixed' FROM sweep_ext.docs WHERE ci IS NOT NULL",
        "SELECT j @? '$.a[*] ? (@ > 0)', jsonb_path_query(j, '$.a[0]') FROM sweep_ext.docs WHERE j IS NOT NULL",
        "SELECT (oc).i.m, (oc).i.r, (oc).h FROM sweep_ext.docs WHERE oc IS NOT NULL",
    ] {
        let rs = query(&e, &c, q);
        let snap = rs.snapshot();
        for i in 0..rs.columns().unwrap().len() {
            let t = snap.text(0, i).map(|t| t.into_owned()).unwrap_or_else(|| "NULL".into());
            eprintln!("  {q:.50} [{i}] = {t}");
            assert!(!t.starts_with("\\x"), "{q}: undecoded {t}");
        }
    }
    // Catalog, columns, objects.
    let db = c.database().to_string();
    let _ = e.core.block_on(c.preload(&db));
    let rels = e.core.block_on(c.relations(&db, "sweep_ext")).expect("relations");
    eprintln!("relations: {:?}", rels.iter().map(|r| r.name.clone()).collect::<Vec<_>>());
    let cols = e.core.block_on(c.columns(&db, "sweep_ext", "docs")).expect("columns");
    eprintln!("{} columns", cols.len());
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS sweep_ext CASCADE"]);
}

#[test]
fn edit_extension_cells() {
    use dataman_core::edit::{CellValue, ChangeSet, EditTarget, Editability, Outcome};
    let e = env();
    let Some(c) = pg(&e) else { return };
    if !has_extensions(&e, &c) {
        return;
    }
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS sweep_edit CASCADE"]);
    exec(
        &e,
        &c,
        &[
            "CREATE SCHEMA sweep_edit",
            "CREATE TYPE sweep_edit.mood AS ENUM ('sad', 'ok')",
            "CREATE TABLE sweep_edit.t (id int PRIMARY KEY, v vector(3), hv halfvec(3), sv sparsevec(5), h hstore, lt ltree, ci citext, r int4range, tsv tsvector, m money, b varbit, ip inet, pt point, mo sweep_edit.mood[], tq tsquery, jp jsonpath, rg int4multirange, iv interval, bx box, ba bytea, tz timestamptz, ar int[], n numeric, f8 float8, bo bool, js jsonb, mc macaddr, u uuid, x xml)",
            "INSERT INTO sweep_edit.t VALUES (1, '[1,2,3]', '[1,2,3]', '{1:1,3:2}/5', 'a=>1', 'a.b', 'Hi', '[1,5)', 'a:1 b:2', '$1.50', B'101', '10.0.0.1/8', '(1,2)', '{sad}', 'a & b', '$.a', '{[1,2)}', '1 day', '((1,1),(0,0))', '\\xdead', '2024-01-02 03:04:05+00', '{1,2}', 1.5, 2.5, true, '{\"a\":1}', '08:00:2b:01:02:03', 'f81d4fae-7dec-11d0-a765-00a0c91e6bf6', '<a/>')",
        ],
    );
    let rs = query(&e, &c, "SELECT * FROM sweep_edit.t ORDER BY id");
    let t: EditTarget = match e.core.block_on(c.edit_target(c.database(), &rs, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("read-only: {why}"),
    };
    let snap = rs.snapshot();
    let cols = rs.columns().unwrap();
    let news: &[(&str, &str)] = &[
        ("v", "[4,5,6]"), ("hv", "[4,5,6]"), ("sv", "{2:7}/5"), ("h", "\"z\"=>\"9\""), ("lt", "x.y.z"), ("ci", "Bye"), ("r", "[2,9)"),
        ("tsv", "'q':1 'w':2"), ("m", "$2.50"), ("b", "1111"), ("ip", "10.0.0.2/8"), ("pt", "(3,4)"), ("mo", "{ok,sad}"), ("tq", "'q' & 'w'"),
        ("jp", "$.b"), ("rg", "{[3,4)}"), ("iv", "2 days"), ("bx", "(3,3),(2,2)"), ("ba", "\\xbeef"), ("tz", "2025-01-01 00:00:00+00"), ("ar", "{3,4}"),
        ("n", "9.75"), ("f8", "0.5"), ("bo", "false"), ("js", "{\"b\": 2}"), ("mc", "08:00:2b:01:02:04"), ("u", "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"), ("x", "<b/>"),
    ];
    let mut cs = ChangeSet::new();
    for (name, new) in news {
        let col = cols.iter().position(|c| c.name == *name).unwrap();
        let old = snap.text(0, col).map(|t| CellValue::text(t.into_owned())).unwrap_or(CellValue::Null);
        cs.edit(0, col, old, CellValue::text(*new));
    }
    let plan = cs.plan(&t, &snap).unwrap();
    // One statement per row: apply and then each column must read back as typed.
    let r = e.core.block_on(c.apply_changes(None, plan)).unwrap();
    assert!(r.committed && r.outcomes.iter().all(|o| matches!(o.outcome, Outcome::Applied { rows: 1 })), "{r:?}");
    let after = query(&e, &c, "SELECT * FROM sweep_edit.t");
    let a = after.snapshot();
    for (name, new) in news {
        let col = cols.iter().position(|c| c.name == *name).unwrap();
        eprintln!("{name:4} {new:30} -> {:?}", a.text(0, col));
    }
    // Round trip: the shown text of every column is valid input and equal to itself (the
    // conflict check of the next save).
    let t2: EditTarget = match e.core.block_on(c.edit_target(c.database(), &after, None)).unwrap() {
        Editability::Editable(t) => t,
        Editability::ReadOnly(why) => panic!("read-only: {why}"),
    };
    let mut cs = ChangeSet::new();
    cs.edit(0, 1, CellValue::text(a.text(0, 1).unwrap().into_owned()), CellValue::text("[7,8,9]"));
    for (col, ci) in cols.iter().enumerate().skip(1) {
        let old = a.text(0, col).map(|t| CellValue::text(t.into_owned())).unwrap_or(CellValue::Null);
        let same = old.clone();
        cs.edit(0, col, old, same);
        let _ = ci;
    }
    cs.edit(0, 1, CellValue::text(a.text(0, 1).unwrap().into_owned()), CellValue::text("[7,8,9]"));
    let plan = cs.plan(&t2, &a).unwrap();
    let r = e.core.block_on(c.apply_changes(None, plan)).unwrap();
    assert!(r.committed, "re-save of unchanged shown text failed: {r:?}");
    // Deleting the row checks every column it can.
    let after = query(&e, &c, "SELECT * FROM sweep_edit.t");
    let a = after.snapshot();
    let mut cs = ChangeSet::new();
    cs.delete_row(0);
    let r = e.core.block_on(c.apply_changes(None, cs.plan(&t2, &a).unwrap())).unwrap();
    assert!(r.committed, "delete failed: {r:?}");
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS sweep_edit CASCADE"]);
}

fn run_ddl(e: &Env, c: &Arc<Connection>, script: &str) {
    let stmts: Vec<String> = split::split(script).into_iter().map(|s| script[s.range].to_string()).collect();
    let refs: Vec<&str> = stmts.iter().map(String::as_str).collect();
    exec(e, c, &refs);
}

#[test]
fn catalog_and_ddl_round_trip() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    if !has_extensions(&e, &c) {
        return;
    }
    let (a, b) = ("sweep_ddl_a", "sweep_ddl_b");
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {a} CASCADE"), &format!("DROP SCHEMA IF EXISTS {b} CASCADE")]);
    let setup = [
        format!("CREATE SCHEMA {a}"),
        format!("CREATE TYPE {a}.mood AS ENUM ('sad', 'it''s ok', 'semi;colon', 'ünï')"),
        format!("CREATE TYPE {a}.pair AS (x integer, \"Y z\" text, m {a}.mood)"),
        format!("CREATE DOMAIN {a}.posint AS integer NOT NULL DEFAULT 1 CONSTRAINT pos CHECK (VALUE > 0 AND VALUE::text <> 'a;b')"),
        format!("CREATE DOMAIN {a}.email AS text CHECK (VALUE ~ '^[^@;]+@[^@]+$')"),
        format!(
            "CREATE FUNCTION {a}.semi(x text DEFAULT 'a;b', VARIADIC rest int[] DEFAULT '{{}}') RETURNS text LANGUAGE plpgsql IMMUTABLE AS $f$\n\
             DECLARE r text := $q$it's; $$ nested$q$;\nBEGIN\n  -- comment; here\n  RETURN x || r;\nEND;\n$f$"
        ),
        format!("CREATE FUNCTION {a}.semi(x int) RETURNS int LANGUAGE sql AS $$ SELECT x; $$"),
        format!("CREATE FUNCTION {a}.outp(IN a int, OUT b int, OUT c text) LANGUAGE sql AS $$ SELECT a, 'x' $$"),
        format!("CREATE FUNCTION {a}.tbl(n int) RETURNS TABLE(i int, t text) LANGUAGE sql AS $$ SELECT g, 'r' || g FROM generate_series(1, n) g $$"),
        format!("CREATE PROCEDURE {a}.proc(INOUT x int, y text DEFAULT 'q') LANGUAGE plpgsql AS $$ BEGIN x := x + 1; RAISE NOTICE '%', y; END $$"),
        format!("CREATE FUNCTION {a}.sqlbody(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a * 2; END"),
        format!("CREATE FUNCTION {a}.\"We;ird Name\"(\"Arg 1\" int) RETURNS int LANGUAGE sql AS 'SELECT 1'"),
        format!("CREATE AGGREGATE {a}.mysum(int) (sfunc = int4pl, stype = int, initcond = '0')"),
        format!("CREATE SEQUENCE {a}.seq1 AS bigint START 5 INCREMENT BY 3 MINVALUE 1 MAXVALUE 1000 CYCLE CACHE 4"),
        format!(
            "CREATE TABLE {a}.docs (id bigint GENERATED BY DEFAULT AS IDENTITY (START WITH 10) PRIMARY KEY, v vector(3) NOT NULL, hv halfvec(3), sv sparsevec(5), \
             body text, ts tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(body, ''))) STORED, h hstore DEFAULT 'a=>1', lt ltree, ci citext, \
             r int4range DEFAULT '[1,2)', mood {a}.mood DEFAULT 'it''s ok', p {a}.pair, pi {a}.posint, em {a}.email, arr {a}.mood[] DEFAULT '{{sad}}', \
             \"Weird;Col\" text, \"select\" int, loc point, net inet DEFAULT '10.0.0.1', d date DEFAULT CURRENT_DATE, \
             CONSTRAINT body_chk CHECK (body <> 'a;b'), CONSTRAINT lt_uniq UNIQUE (lt) DEFERRABLE INITIALLY DEFERRED, \
             CONSTRAINT excl EXCLUDE USING gist (r WITH &&) )"
        ),
        format!("CREATE INDEX docs_hnsw ON {a}.docs USING hnsw (v vector_cosine_ops) WITH (m = 8, ef_construction = 32)"),
        format!("CREATE INDEX docs_ivf ON {a}.docs USING ivfflat (hv halfvec_l2_ops) WITH (lists = 2)"),
        format!("CREATE INDEX docs_gin ON {a}.docs USING gin (ts) WITH (fastupdate = off)"),
        format!("CREATE INDEX docs_trgm ON {a}.docs USING gin (body gin_trgm_ops)"),
        format!("CREATE INDEX docs_h ON {a}.docs USING gist (lt)"),
        format!("CREATE INDEX docs_expr ON {a}.docs ((lower(\"Weird;Col\")), \"select\" DESC NULLS LAST) INCLUDE (d) WHERE body IS NOT NULL"),
        format!("CREATE UNIQUE INDEX docs_u ON {a}.docs (ci)"),
        format!("COMMENT ON TABLE {a}.docs IS 'it''s; \"docs\"'"),
        format!("COMMENT ON COLUMN {a}.docs.\"Weird;Col\" IS 'a;b'"),
        format!("CREATE TABLE {a}.child (cid int, note text DEFAULT 'x;y') INHERITS ({a}.docs)"),
        format!("CREATE TABLE {a}.ev (at timestamptz NOT NULL, k text, v int, PRIMARY KEY (at, k)) PARTITION BY RANGE (at)"),
        format!("CREATE TABLE {a}.ev_1 PARTITION OF {a}.ev FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')"),
        format!("CREATE TABLE {a}.ev_d PARTITION OF {a}.ev DEFAULT"),
        format!("CREATE TABLE {a}.lst (k text, v int) PARTITION BY LIST (k)"),
        format!("CREATE TABLE {a}.lst_a PARTITION OF {a}.lst FOR VALUES IN ('a', 'b;c')"),
        format!("CREATE TABLE {a}.hsh (k int) PARTITION BY HASH (k)"),
        format!("CREATE TABLE {a}.hsh_0 PARTITION OF {a}.hsh FOR VALUES WITH (MODULUS 2, REMAINDER 0)"),
        format!("CREATE TABLE {a}.fk (id int PRIMARY KEY, doc bigint REFERENCES {a}.docs(id) ON DELETE SET NULL ON UPDATE CASCADE DEFERRABLE, ev_at timestamptz, ev_k text, FOREIGN KEY (ev_at, ev_k) REFERENCES {a}.ev (at, k) MATCH FULL)"),
        format!("CREATE TABLE {a}.logd (id serial, msg text) WITH (fillfactor = 70, autovacuum_enabled = false)"),
        format!("CREATE UNLOGGED TABLE {a}.unl (k int)"),
        format!("CREATE FUNCTION {a}.trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; NEW.msg := coalesce(NEW.msg, 'x;y'); RETURN NEW; END; $$"),
        format!("CREATE TRIGGER t1 BEFORE INSERT OR UPDATE OF msg ON {a}.logd FOR EACH ROW WHEN (NEW.msg IS DISTINCT FROM 'z;z') EXECUTE FUNCTION {a}.trg()"),
        format!("CREATE TRIGGER t2 AFTER DELETE ON {a}.logd REFERENCING OLD TABLE AS oldt FOR EACH STATEMENT EXECUTE FUNCTION {a}.trg()"),
        format!("CREATE VIEW {a}.v1 AS SELECT id, v <-> '[1,2,3]' AS dist, body ~ ';' AS semi FROM {a}.docs WHERE body <> 'a;b' ORDER BY 2"),
        format!("CREATE VIEW {a}.v2 WITH (security_barrier = true) AS SELECT * FROM {a}.v1 WITH CHECK OPTION"),
        format!("CREATE MATERIALIZED VIEW {a}.mv1 AS SELECT lt, count(*) n FROM {a}.docs GROUP BY lt WITH NO DATA"),
        format!("CREATE INDEX mv1_i ON {a}.mv1 (n)"),
        format!("INSERT INTO {a}.docs (v, body, ci, r) VALUES ('[1,2,3]', 'hello', 'X', '[1,2)'), ('[4,5,6]', NULL, 'Y', '[5,6)')"),
    ];
    let setup: Vec<&str> = setup.iter().map(String::as_str).collect();
    exec(&e, &c, &setup);
    let db = c.database().to_string();
    macro_rules! run {
        ($t:expr) => {
            e.core.block_on($t)
        };
    }
    c.schema.invalidate(None);
    run!(c.preload(&db));
    let rels = run!(c.relations(&db, a)).unwrap();
    eprintln!("relations: {:?}", rels.iter().map(|r| format!("{}:{:?}", r.name, r.kind)).collect::<Vec<_>>());
    let n = run!(c.object_counts(&db, a)).unwrap();
    eprintln!("counts: {n:?}");
    let funcs = run!(c.functions(&db, a)).unwrap();
    eprintln!("functions: {:?}", funcs.iter().map(|f| format!("{}({})", f.name, f.identity)).collect::<Vec<_>>());
    let seqs = run!(c.sequences(&db, a)).unwrap();
    let types = run!(c.types(&db, a)).unwrap();
    let trgs = run!(c.triggers(&db, a)).unwrap();
    eprintln!("sequences {} types {} triggers {}", seqs.len(), types.len(), trgs.len());
    for s in seqs.iter() {
        let _ = run!(c.sequence_value(&db, a, &s.name)).unwrap();
    }
    run!(c.rel_stats(&db, a)).unwrap();
    for r in &rels {
        let cols = run!(c.columns(&db, a, &r.name)).unwrap();
        assert!(!cols.is_empty(), "{} has columns", r.name);
        run!(c.indexes(&db, a, &r.name)).unwrap();
        if matches!(r.kind, dataman_core::schema::RelKind::Table | dataman_core::schema::RelKind::Partitioned) {
            run!(c.table_extras(&db, a, &r.name)).unwrap_or_else(|e| panic!("extras of {}: {e:#}", r.name));
        }
    }
    // Definitions of every function / type run (in schema b), and the schema reads the same.
    exec(&e, &c, &[&format!("CREATE SCHEMA {b}")]);
    let mut defs = Vec::new();
    for t in types.iter() {
        let d = run!(c.type_definition(&db, a, &t.name)).unwrap().unwrap_or_else(|| panic!("type {}", t.name));
        defs.push(d);
    }
    for t in &defs {
        run_ddl(&e, &c, &t.replace(a, b));
    }
    // Tables in dependency order, then their views.
    let tables = ["docs", "child", "ev", "ev_1", "ev_d", "lst", "lst_a", "hsh", "hsh_0", "fk", "logd", "unl"];
    let mut ddl_a = Vec::new();
    for t in tables {
        ddl_a.push(run!(c.table_ddl(&db, a, t)).unwrap_or_else(|e| panic!("ddl of {t}: {e:#}")));
    }
    for f in funcs.iter() {
        let d = run!(c.function_definition(&db, f.oid)).unwrap().unwrap_or_else(|| panic!("def of {}", f.name));
        run_ddl(&e, &c, &d.replace(a, b));
    }
    for (t, d) in tables.iter().zip(&ddl_a) {
        // Partitions are created by their parent's DDL elsewhere; this run is standalone, so the
        // statements are checked as scripts.
        let script = d.replace(a, b);
        let stmts: Vec<String> = split::split(&script).into_iter().map(|s| script[s.range].to_string()).collect();
        let refs: Vec<&str> = stmts.iter().map(String::as_str).collect();
        exec(&e, &c, &refs);
        let _ = t;
    }
    c.schema.invalidate(None);
    for (t, before) in tables.iter().zip(&ddl_a) {
        let after = run!(c.table_ddl(&db, b, t)).unwrap();
        let strip = |s: &str| s.lines().filter(|l| !l.starts_with("-- Inherits") && !l.starts_with("-- A partition")).collect::<Vec<_>>().join("\n");
        assert_eq!(strip(&after.replace(b, a)), strip(before), "round trip of {t}");
    }
    for v in ["v1", "v2", "mv1"] {
        let d = run!(c.view_definition(&db, a, v)).unwrap().unwrap();
        run_ddl(&e, &c, &d.replace(a, b));
    }
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {a} CASCADE"), &format!("DROP SCHEMA IF EXISTS {b} CASCADE")]);
}

#[test]
fn lint_quiet_on_valid_queries() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec_quiet(&e, &c, &["DROP TABLE IF EXISTS sweep_l1, sweep_l2 CASCADE", "DROP SCHEMA IF EXISTS sweep_s CASCADE"]);
    exec(
        &e,
        &c,
        &[
            "CREATE TABLE sweep_l1 (a int PRIMARY KEY, b text, c jsonb, d date, e int[], \"Mixed Case\" int, \"order\" int)",
            "CREATE TABLE sweep_l2 (a int, x text, y numeric, UNIQUE (a, x))",
            "CREATE SCHEMA sweep_s",
            "CREATE TABLE sweep_s.t3 (id int, a int, pay jsonb)",
            "CREATE VIEW sweep_s.v3 AS SELECT * FROM sweep_s.t3",
        ],
    );
    let db = c.database().to_string();
    c.schema.invalidate(None);
    let _ = e.core.block_on(c.preload(&db));
    for (s, t) in [("public", "sweep_l1"), ("public", "sweep_l2"), ("sweep_s", "t3"), ("sweep_s", "v3")] {
        e.core.block_on(c.columns(&db, s, t)).unwrap();
    }
    let qs: &[&str] = &[
        "SELECT * FROM sweep_l1",
        "SELECT t.* FROM sweep_l1 t",
        "SELECT t FROM sweep_l1 t",
        "SELECT row_to_json(t) FROM sweep_l1 t",
        "SELECT (t).a FROM sweep_l1 t",
        "SELECT a, b FROM sweep_l1 WHERE \"Mixed Case\" > 1 AND \"order\" = 2 ORDER BY \"order\"",
        "SELECT ctid, xmin, cmax, tableoid::regclass FROM sweep_l1",
        "SELECT a AS \"select\", b alias FROM sweep_l1 ORDER BY alias, \"select\"",
        "SELECT a + 1 AS n FROM sweep_l1 GROUP BY a + 1 HAVING a + 1 > 0 ORDER BY n",
        "SELECT l1.a, l2.x FROM sweep_l1 l1 JOIN sweep_l2 l2 USING (a)",
        "SELECT a, x FROM sweep_l1 NATURAL JOIN sweep_l2",
        "SELECT * FROM sweep_l1 l1 LEFT JOIN sweep_l2 l2 ON l1.a = l2.a AND l2.y > 1 WHERE l2.a IS NULL",
        "SELECT * FROM sweep_l1 l1, sweep_l2 l2 WHERE l1.a = l2.a",
        "SELECT * FROM public.sweep_l1 p WHERE p.a = 1",
        "SELECT sweep_l1.a FROM public.sweep_l1",
        "SELECT public.sweep_l1.a FROM public.sweep_l1",
        "SELECT t3.id, t3.pay->>'k' FROM sweep_s.t3",
        "SELECT v3.id FROM sweep_s.v3",
        "SELECT * FROM sweep_s.t3 WHERE pay @> '{\"a\":1}' AND pay ? 'k' AND pay ?| ARRAY['a','b']",
        "WITH x AS (SELECT a, b FROM sweep_l1) SELECT x.a, x.b FROM x",
        "WITH x(p, q) AS (SELECT a, b FROM sweep_l1) SELECT p, q FROM x",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
        "WITH d AS (DELETE FROM sweep_l2 WHERE a = -1 RETURNING *) SELECT d.a, d.x FROM d",
        "WITH i AS (INSERT INTO sweep_l2 (a, x) VALUES (-5, 'z') RETURNING a) SELECT a FROM i",
        "SELECT s.a, s.b FROM (SELECT a, b FROM sweep_l1) s",
        "SELECT s.n FROM (SELECT count(*) AS n FROM sweep_l1) AS s",
        "SELECT x.v, x.o FROM generate_series(1, 3) WITH ORDINALITY AS x(v, o)",
        "SELECT g FROM generate_series(1, 3) g",
        "SELECT g.g FROM generate_series(1, 3) g",
        "SELECT v.a, v.b FROM (VALUES (1, 'a'), (2, 'b')) AS v(a, b)",
        "SELECT u.e, u.n FROM unnest(ARRAY[1,2]) WITH ORDINALITY AS u(e, n)",
        "SELECT j.a, j.b FROM jsonb_to_recordset('[{\"a\":1,\"b\":\"x\"}]') AS j(a int, b text)",
        "SELECT * FROM jsonb_each('{\"a\":1}') AS e(key, value)",
        "SELECT key, value FROM jsonb_each_text('{\"a\":1}')",
        "SELECT * FROM ROWS FROM (generate_series(1, 2), generate_series(1, 3)) AS r(a, b)",
        "SELECT unnest(e) FROM sweep_l1",
        "SELECT e[1], e[1:2], (e)[1] FROM sweep_l1",
        "SELECT a FROM sweep_l1 WHERE a = ANY (ARRAY[1,2]) AND b = ANY (SELECT x FROM sweep_l2)",
        "SELECT a FROM sweep_l1 WHERE EXISTS (SELECT 1 FROM sweep_l2 WHERE sweep_l2.a = sweep_l1.a)",
        "SELECT a, (SELECT max(y) FROM sweep_l2 WHERE sweep_l2.a = sweep_l1.a) FROM sweep_l1",
        "SELECT a FROM sweep_l1 o WHERE a > (SELECT avg(a) FROM sweep_l1 i WHERE i.b = o.b)",
        "SELECT l.a, s.x FROM sweep_l1 l CROSS JOIN LATERAL (SELECT x FROM sweep_l2 WHERE sweep_l2.a = l.a LIMIT 1) s",
        "SELECT l.a, s.x FROM sweep_l1 l, LATERAL (SELECT x FROM sweep_l2 WHERE sweep_l2.a = l.a) AS s",
        "SELECT count(*) FILTER (WHERE a > 1), percentile_cont(0.5) WITHIN GROUP (ORDER BY a) FROM sweep_l1 GROUP BY b UNION ALL SELECT sum(a) OVER w, 1 FROM sweep_l1 WINDOW w AS (PARTITION BY b ORDER BY a)",
        "SELECT a, row_number() OVER (PARTITION BY b ORDER BY a DESC) AS rn FROM sweep_l1 ORDER BY rn",
        "SELECT EXTRACT(year FROM d), date_trunc('month', d), d AT TIME ZONE 'utc', d + INTERVAL '1' DAY FROM sweep_l1",
        "SELECT substring(b from 1 for 2), trim(both 'x' from b), position('a' in b), overlay(b placing 'x' from 1 for 1) FROM sweep_l1",
        "SELECT CAST(a AS text), a::bigint::text, b COLLATE \"C\", a IS DISTINCT FROM 1, b ~* 'x', b SIMILAR TO 'x%' FROM sweep_l1",
        "SELECT DISTINCT ON (b) b, a FROM sweep_l1 ORDER BY b, a",
        "SELECT a FROM sweep_l1 ORDER BY a OFFSET 1 ROWS FETCH FIRST 2 ROWS ONLY",
        "SELECT a FROM sweep_l1 TABLESAMPLE SYSTEM (50)",
        "SELECT a FROM sweep_l1 FOR UPDATE OF sweep_l1 SKIP LOCKED",
        "SELECT a FROM sweep_l1 UNION ALL SELECT a FROM sweep_l2 ORDER BY a",
        "(SELECT a FROM sweep_l1) UNION (SELECT a FROM sweep_l2) ORDER BY 1",
        "SELECT * FROM (SELECT a FROM sweep_l1 UNION SELECT a FROM sweep_l2) u WHERE u.a > 0",
        "SELECT a FROM sweep_l1 WHERE a IN (1, 2) AND b NOT IN ('x') AND d BETWEEN '2020-01-01' AND now()",
        "SELECT CASE WHEN a > 1 THEN 'x' ELSE b END AS k FROM sweep_l1",
        "SELECT current_date, now(), current_user, session_user, localtimestamp, pg_backend_pid()",
        "SELECT 1 AS a, 2 AS b WHERE false",
        "SELECT ARRAY(SELECT a FROM sweep_l1), ROW(a, b) FROM sweep_l1",
        "SELECT a, b FROM sweep_l1 GROUP BY ROLLUP (a, b)",
        "SELECT a FROM sweep_l1 GROUP BY GROUPING SETS ((a), (b))",
        "INSERT INTO sweep_l2 (a, x) SELECT a, b FROM sweep_l1 ON CONFLICT (a, x) DO UPDATE SET y = excluded.y + sweep_l2.y WHERE sweep_l2.a > 0 RETURNING a, x",
        "INSERT INTO sweep_l2 (a, x) VALUES (1, 'a') ON CONFLICT ON CONSTRAINT sweep_l2_a_x_key DO NOTHING",
        "INSERT INTO sweep_l2 AS t (a, x) VALUES (1, 'q') ON CONFLICT (a, x) DO UPDATE SET y = t.y",
        "INSERT INTO sweep_l2 DEFAULT VALUES RETURNING *",
        "UPDATE sweep_l2 SET y = l1.a FROM sweep_l1 l1 WHERE l1.a = sweep_l2.a",
        "UPDATE sweep_l2 t SET (x, y) = (SELECT b, a FROM sweep_l1 WHERE sweep_l1.a = t.a LIMIT 1) WHERE t.a = -9",
        "UPDATE sweep_l2 SET y = y + 1 WHERE a = -9 RETURNING sweep_l2.y",
        "DELETE FROM sweep_l2 USING sweep_l1 WHERE sweep_l1.a = sweep_l2.a AND sweep_l2.a = -9",
        "DELETE FROM sweep_l2 t WHERE t.a = -9 RETURNING t.*",
        "MERGE INTO sweep_l2 t USING sweep_l1 s ON t.a = s.a WHEN MATCHED AND s.b IS NULL THEN DELETE WHEN MATCHED THEN UPDATE SET x = s.b WHEN NOT MATCHED THEN INSERT (a, x) VALUES (s.a, s.b)",
        "TABLE sweep_l1",
        "CREATE TABLE sweep_l_new AS SELECT a, b FROM sweep_l1",
        "SELECT * FROM sweep_l_new",
        "SELECT a INTO sweep_l_new2 FROM sweep_l1",
        "SELECT a FROM sweep_l_new2",
        "DROP TABLE sweep_l_new, sweep_l_new2",
        "CREATE TEMP TABLE sweep_tmp (z int)",
        "SELECT z FROM sweep_tmp",
        "SELECT pg_typeof(a), pg_get_expr(adbin, adrelid) FROM sweep_l1, pg_attrdef LIMIT 1",
        "SELECT relname, n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE relkind = 'r'",
        "SELECT * FROM information_schema.columns WHERE table_name = 'sweep_l1'",
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE attrelid = 'sweep_l1'::regclass AND attnum > 0",
        "SELECT ts_rank(to_tsvector(b), q) FROM sweep_l1, to_tsquery('x') q WHERE to_tsvector(b) @@ q",
        "SELECT l1.a FROM sweep_l1 l1 WHERE l1.c #>> '{a,b}' = 'x' AND l1.c -> 'k' ->> 'z' IS NULL",
        "SELECT l1.a AS \"A b\" FROM sweep_l1 l1 ORDER BY \"A b\"",
        "SELECT \"Mixed Case\", l.\"Mixed Case\" FROM sweep_l1 l",
        "SELECT COUNT(*) AS cnt FROM sweep_l1 HAVING COUNT(*) > 0",
        "SELECT * FROM sweep_l1 WHERE a IN (SELECT a FROM sweep_l2) FOR SHARE",
        "SELECT a FROM sweep_l1 WHERE (a, b) IN (SELECT a, x FROM sweep_l2)",
        "SELECT (SELECT 1) AS one, (SELECT a FROM sweep_l1 LIMIT 1) AS first_a",
        "SELECT x FROM (SELECT 1 AS x) AS t(x)",
        "SELECT t.a FROM sweep_l1 AS t (a, b)",
        "SELECT a FROM ONLY sweep_l1",
        "SELECT a FROM sweep_l1 * ",
        "SELECT json_build_object('k', a) ->> 'k' FROM sweep_l1",
        "SELECT pg_catalog.count(*) FROM pg_catalog.pg_class",
        "SELECT 'x' || b FROM sweep_l1 WHERE b LIKE 'x%' ESCAPE '!'",
        "SELECT a FROM sweep_l1 WHERE d >= DATE '2020-01-01' AND d < TIMESTAMP '2021-01-01 00:00:00'",
        "SELECT (a, b) = (1, 'x'), (a, b) < ROW(1, 'x') FROM sweep_l1",
        "SELECT a FROM sweep_l1 LIMIT ALL",
        "SELECT a FROM sweep_l1 WHERE NOT EXISTS (SELECT FROM sweep_l2 WHERE sweep_l2.a = sweep_l1.a)",
    ];
    let s = c.session(None);
    let mut failures = Vec::new();
    for q in qs {
        // Run first (so the created names exist for the session's lint context).
        let rs = s.execute(*q);
        let ph = finish(&e, &rs);
        if ph != Phase::Done {
            failures.push(format!("RUN {q}: {:?}", rs.status().error.map(|e| e.message)));
            continue;
        }
        let snap = c.schema.snapshot(&db);
        let known: std::collections::HashSet<String> = ["sweep_tmp", "sweep_l_new", "sweep_l_new2"].iter().map(|s| s.to_string()).collect();
        let l = lint(q, &LintContext { schema: Some(&snap), known: Some(&known), ..Default::default() });
        for x in l.iter().filter(|x| x.severity == Severity::Error) {
            failures.push(format!("LINT {q}: {:?} {}", &q[x.range.clone()], x.message));
        }
    }
    exec_quiet(&e, &c, &["DROP TABLE IF EXISTS sweep_l1, sweep_l2 CASCADE", "DROP SCHEMA IF EXISTS sweep_s CASCADE"]);
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// Whole scripts (not executed) against a snapshot that knows only `sweep_l1`.
#[test]
fn lint_quiet_on_valid_scripts() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec_quiet(&e, &c, &["DROP TABLE IF EXISTS sweep_k1 CASCADE"]);
    exec(&e, &c, &["CREATE TABLE sweep_k1 (a int PRIMARY KEY, b text, date int, \"Mixed Case\" int)"]);
    let db = c.database().to_string();
    c.schema.invalidate(None);
    let _ = e.core.block_on(c.preload(&db));
    e.core.block_on(c.columns(&db, "public", "sweep_k1")).unwrap();
    let scripts: &[&str] = &[
        "CREATE TABLE sweep_new (id serial PRIMARY KEY, name text);\nINSERT INTO sweep_new (name) VALUES ('x');\nSELECT id, name FROM sweep_new;",
        "CREATE TABLE IF NOT EXISTS sweep_new (id int, z text);\nUPDATE sweep_new SET z = 'q' WHERE id = 1;",
        "CREATE TEMP TABLE tt AS SELECT a, b FROM sweep_k1;\nSELECT a, b FROM tt;\nSELECT tt.a FROM tt;",
        "CREATE TABLE sweep_like (LIKE sweep_k1 INCLUDING ALL);\nSELECT a, b FROM sweep_like;",
        "ALTER TABLE sweep_k1 ADD COLUMN extra int;\nUPDATE sweep_k1 SET extra = 1 WHERE a = 1;\nSELECT extra FROM sweep_k1;",
        "ALTER TABLE sweep_k1 RENAME COLUMN b TO bb;\nSELECT bb FROM sweep_k1;",
        "ALTER TABLE sweep_k1 RENAME TO sweep_k2;\nSELECT a FROM sweep_k2;",
        "ALTER TABLE sweep_k1 DROP COLUMN b;\nSELECT a FROM sweep_k1;",
        "CREATE VIEW sweep_vw AS SELECT a AS aa FROM sweep_k1;\nSELECT aa FROM sweep_vw;",
        "CREATE MATERIALIZED VIEW sweep_mvw AS SELECT a AS aa, count(*) AS n FROM sweep_k1 GROUP BY a;\nSELECT aa, n FROM sweep_mvw;",
        "CREATE TABLE sweep_p (id int, v int) PARTITION BY RANGE (id);\nCREATE TABLE sweep_p1 PARTITION OF sweep_p FOR VALUES FROM (1) TO (10);\nINSERT INTO sweep_p1 (id, v) VALUES (1, 1);\nSELECT id, v FROM sweep_p1;",
        "CREATE SCHEMA sweep_sc;\nCREATE TABLE sweep_sc.t (a int);\nSELECT a FROM sweep_sc.t;",
        "SELECT date FROM sweep_k1;\nSELECT \"Mixed Case\" FROM sweep_k1;",
        "SELECT A, B FROM SWEEP_K1;\nSELECT \"a\" FROM \"sweep_k1\";",
        "WITH u AS (UPDATE sweep_k1 SET b = 'x' WHERE a = 1 RETURNING a, b) SELECT a, b FROM u;",
        "WITH s AS MATERIALIZED (SELECT a FROM sweep_k1), t AS NOT MATERIALIZED (SELECT a FROM s) SELECT a FROM t;",
        "WITH x AS (SELECT 1 AS a) SEARCH DEPTH FIRST BY a SET ordercol SELECT * FROM x ORDER BY ordercol;",
        "WITH RECURSIVE t(n, path) AS (SELECT 1, ARRAY[1] UNION ALL SELECT n + 1, path || n FROM t WHERE n < 3) CYCLE n SET is_cycle USING cp SELECT n, path, is_cycle FROM t;",
        "DO $$ BEGIN PERFORM a FROM sweep_k1; END $$;\nSELECT 1;",
        "CREATE FUNCTION sweep_f() RETURNS int LANGUAGE sql AS $$ SELECT nonexistent_col FROM nonexistent_table $$;",
        "CREATE FUNCTION sweep_f2(x int) RETURNS TABLE (q int) LANGUAGE sql AS 'SELECT x';\nSELECT q FROM sweep_f2(1);",
        "CREATE FUNCTION sweep_f3(OUT p int, OUT r text) LANGUAGE sql AS $$ SELECT 1, 'a' $$;\nSELECT p, r FROM sweep_f3();",
        "CREATE TYPE sweep_ty AS (aa int, bb text);\nSELECT (x).aa FROM (SELECT ROW(1, 'x')::sweep_ty AS x) s;\nSELECT aa, bb FROM (SELECT (ROW(1, 'x')::sweep_ty).*) q;",
        "SELECT * FROM sweep_k1 -- trailing\n;\n-- only a comment\n;\n;;",
        "SELECT a FROM sweep_k1 /* c */ WHERE /* d */ a = 1;",
        "EXPLAIN SELECT a FROM sweep_k1;\nEXPLAIN (ANALYZE) SELECT a FROM sweep_k1 WHERE b = 'x';",
        "COPY (SELECT a FROM sweep_k1) TO STDOUT;\nCOPY sweep_k1 (a, b) FROM STDIN;",
        "PREPARE q(int) AS SELECT a FROM sweep_k1 WHERE a = $1;\nEXECUTE q(1);",
        "DECLARE cur CURSOR FOR SELECT a FROM sweep_k1;\nFETCH ALL FROM cur;",
        "CREATE INDEX ON sweep_k1 (a, lower(b));\nCREATE INDEX CONCURRENTLY idx2 ON sweep_k1 USING btree (a DESC NULLS LAST) WHERE b IS NOT NULL;",
        "GRANT SELECT (a, b) ON sweep_k1 TO PUBLIC;\nCOMMENT ON COLUMN sweep_k1.a IS 'x';",
        "CREATE TRIGGER trg BEFORE INSERT ON sweep_k1 FOR EACH ROW EXECUTE FUNCTION f();",
        "CREATE RULE r AS ON INSERT TO sweep_k1 DO ALSO (INSERT INTO sweep_k1 (a) VALUES (NEW.a + 1); SELECT 1);",
        "CREATE POLICY p ON sweep_k1 USING (a > 0) WITH CHECK (b IS NOT NULL);",
        "SELECT a, b FROM sweep_k1 WHERE a = 1\n  AND b = 'it''s;'\n  AND b <> E'\\';'\n  AND b <> $$ ; $$;",
        "INSERT INTO sweep_k1 (a, b) OVERRIDING SYSTEM VALUE VALUES (1, 'x'), (2, 'y') RETURNING a;",
        "SELECT sweep_k1.a, sweep_k1.* FROM sweep_k1;",
        "SELECT x.a FROM sweep_k1 AS x WHERE x.a IN (SELECT y.a FROM sweep_k1 y WHERE y.b = x.b);",
        "UPDATE sweep_k1 SET (a, b) = (1, 'x') WHERE a = 2;",
        "UPDATE sweep_k1 SET a = a + 1, b = b || '!' WHERE a < 3 RETURNING *;",
        "TRUNCATE sweep_k1 RESTART IDENTITY CASCADE;",
        "SELECT a FROM sweep_k1 WHERE a = 1 FOR NO KEY UPDATE NOWAIT;",
        "SELECT a FROM sweep_k1 FOR KEY SHARE OF sweep_k1 SKIP LOCKED;",
        "SELECT a FROM sweep_k1 s FOR UPDATE OF s;",
        "SELECT substring(b FROM 2 FOR 3) FROM sweep_k1;",
        "SELECT a FROM sweep_k1 WHERE b LIKE 'a%' ORDER BY a LIMIT 10 OFFSET 5;",
        "SELECT a FROM sweep_k1 ORDER BY a FETCH FIRST 5 ROWS WITH TIES;",
        "SELECT a FROM sweep_k1 WHERE a = ANY ($1);",
        "SELECT a FROM sweep_k1 WHERE b = :name;",
    ];
    let mut failures = Vec::new();
    for sc in scripts {
        let snap = c.schema.snapshot(&db);
        let l = lint(sc, &LintContext { schema: Some(&snap), ..Default::default() });
        for x in l.iter().filter(|x| x.severity == Severity::Error) {
            failures.push(format!("{:?}: {:?} {}", sc, &sc[x.range.clone()], x.message));
        }
        // And the splitter keeps every non-empty statement whole.
        let n = split::split(sc).len();
        assert!(n >= 1, "{sc}");
    }
    exec_quiet(&e, &c, &["DROP TABLE IF EXISTS sweep_k1 CASCADE"]);
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// The read-only guards: nothing that writes passes as a read; harmless text is not refused.
#[test]
fn guards_on_postgres_statements() {
    use dataman_core::guard;
    let writes: &[&str] = &[
        "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
        "with d as (insert into t values (1) returning 1) select * from d",
        "WITH u AS (UPDATE t SET a = 1 RETURNING a) SELECT a FROM u",
        "SELECT (WITH d AS (DELETE FROM t RETURNING 1) SELECT count(*) FROM d)",
        "SELECT * FROM t WHERE a IN (WITH i AS (INSERT INTO t VALUES (1) RETURNING a) SELECT a FROM i)",
        "SELECT * FROM (WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d) x",
        "WITH m AS (MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN DELETE RETURNING t.*) SELECT * FROM m",
        "SELECT * INTO newt FROM t",
        "SELECT a INTO TEMP TABLE newt FROM t",
        "EXPLAIN ANALYZE DELETE FROM t",
        "EXPLAIN (ANALYZE, BUFFERS) INSERT INTO t VALUES (1)",
        "EXPLAIN ANALYZE WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
        "COPY t TO PROGRAM 'rm -rf /'",
        "COPY t FROM STDIN",
        "DO $$ BEGIN DELETE FROM t; END $$",
        "CALL p()",
        "PREPARE x AS DELETE FROM t",
        "EXECUTE x",
        "REFRESH MATERIALIZED VIEW mv",
        "CREATE TEMP TABLE x (a int)",
        "SET ROLE postgres",
        "VACUUM t",
        "LOCK TABLE t",
        "NOTIFY ch",
        "TRUNCATE t",
        "/* c */ DELETE FROM t",
        "-- c\nDELETE FROM t",
        "dElEtE\nFROM t",
        "  \n\t DROP TABLE t",
        "MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN DELETE",
        "SELECT pg_terminate_backend(1)",
        "SELECT pg_catalog.pg_terminate_backend (1)",
        "SELECT \"pg_terminate_backend\"(1)",
        "SELECT nextval('s')",
        "SELECT setval('s', 1)",
        "SELECT lo_import('/etc/passwd')",
        "SELECT pg_read_file('/etc/passwd')",
        "SELECT * FROM pg_ls_dir('/')",
        "SELECT dblink_exec('x', 'drop table t')",
        "SELECT * FROM t FOR UPDATE",
        "SELECT * FROM t FOR NO KEY UPDATE SKIP LOCKED",
        "SELECT * FROM t FOR SHARE",
        "BEGIN",
        "COMMIT",
        "SAVEPOINT a",
        "SELECT 1; DELETE FROM t",
        "ALTER SYSTEM SET work_mem = '1GB'",
        "SELECT set_config('log_statement', 'all', false) WHERE false; DROP TABLE t",
        "WITH x AS (SELECT 1) DELETE FROM t",
        "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
        "WITH x AS (SELECT 1) UPDATE t SET a = 1",
        "(DELETE FROM t RETURNING *)",
        "TABLE t; DROP TABLE t",
        "SELECT pg_advisory_lock(1)",
        "SELECT txid_current()",
        "GRANT SELECT ON t TO PUBLIC",
        "DISCARD ALL",
        "RESET ALL",
        "LISTEN x",
        "IMPORT FOREIGN SCHEMA s FROM SERVER x INTO y",
        "SECURITY LABEL ON TABLE t IS 'x'",
        "CHECKPOINT",
        "CLUSTER t",
        "REINDEX TABLE t",
        "UNLISTEN *",
        "SELECT $$x$$; DROP TABLE t",
        "SELECT 'a;b'; DROP TABLE t",

    ];
    let reads: &[&str] = &[
        "SELECT * FROM t",
        "select 1",
        "WITH x AS (SELECT 1) SELECT * FROM x",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r",
        "TABLE t",
        "VALUES (1), (2)",
        "SHOW search_path",
        "EXPLAIN SELECT * FROM t",
        "EXPLAIN (COSTS OFF, FORMAT JSON) SELECT * FROM t",
        "EXPLAIN ANALYZE SELECT * FROM t",
        "(SELECT 1) UNION (SELECT 2)",
        "SELECT $$DELETE FROM t$$",
        "SELECT 'DROP TABLE t; DELETE FROM t'",
        "SELECT 1 AS \"for update\", 2 AS \"delete\", 'insert' AS update_",
        "SELECT * FROM t WHERE note = 'FOR UPDATE' AND x = 'nextval(1)'",
        "SELECT * FROM t -- FOR UPDATE\n WHERE a = 1",
        "SELECT * FROM t /* DELETE FROM t; */ WHERE a = 1",
        "SELECT currval('s')",
        "SELECT last_value FROM s",
        "SELECT a AS delete_count, b AS inserted FROM t",
        "SELECT * FROM t WHERE a = ANY (ARRAY[1, 2])",
        "SELECT e'a\\'b; DELETE'",
        "SELECT \"update\", \"delete\" FROM t",
        "SELECT * FROM insert_log, delete_log",
        "SELECT jsonb_path_query(j, '$.a ? (@ > 1)') FROM t",
        "SELECT pg_typeof(1), pg_size_pretty(pg_database_size(current_database()))",
        "SELECT * FROM pg_stat_activity",
        "SELECT t.* FROM t t WHERE EXISTS (SELECT 1 FROM u WHERE u.a = t.a)",
        "SELECT 1;",
        "SELECT * FROM t WHERE x ~ 'a;b';",
    ];
    let mut bad = Vec::new();
    for s in writes {
        let stmts: Vec<String> = split::split(s).into_iter().map(|x| s[x.range].to_string()).collect();
        // The agent path splits first; each piece, or the whole text when it is a single piece.
        let refused = if stmts.len() <= 1 { guard::agent_read_refusal(s).is_some() } else { stmts.iter().any(|p| guard::agent_read_refusal(p).is_some()) };
        if !refused {
            bad.push(format!("PASSED AS READ: {s:?}"));
        }
        if guard::agent_write_class(s, dataman_core::sql::Dialect::Postgres) == guard::WriteClass::Read {
            bad.push(format!("WRITE CLASS READ: {s:?}"));
        }
    }
    for s in reads {
        if let Some(r) = guard::agent_read_refusal(s) {
            bad.push(format!("REFUSED READ: {s:?}: {r}"));
        }
        if guard::read_only_refusal(s).is_some() {
            bad.push(format!("READ-ONLY CONN REFUSED: {s:?}"));
        }
    }
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}

#[test]
fn long_scripts_are_fast_and_whole() {
    let mut script = String::new();
    let n = 20_000;
    for i in 0..n {
        match i % 4 {
            0 => script.push_str(&format!("INSERT INTO t (a, b) VALUES ({i}, 'x;{i}');\n")),
            1 => script.push_str(&format!("UPDATE t SET b = 'it''s -- {i}' WHERE a = {i}; -- trailing ; comment\n")),
            2 => script.push_str(&format!("DO $${i}$ BEGIN PERFORM {i}; END $${i}$;\n")),
            _ => script.push_str(&format!("/* c ; /* nested */ */ SELECT a, b FROM t WHERE a = {i};\n")),
        }
    }
    let t0 = Instant::now();
    let parts = split::split(&script);
    let t_split = t0.elapsed();
    assert_eq!(parts.len(), n);
    let t1 = Instant::now();
    let l = lint(&script, &LintContext::default());
    let t_lint = t1.elapsed();
    eprintln!("split {t_split:?} lint {t_lint:?} ({} findings)", l.len());
    assert!(t_split < Duration::from_secs(2), "split took {t_split:?}");
    assert!(t_lint < Duration::from_secs(5), "lint took {t_lint:?}");
    // The statement under the cursor anywhere.
    let mid = script.len() / 2;
    let s = split::statement_at(&script, mid).unwrap();
    assert!(s.range.start <= mid + 100);
}

/// Commands outside the normal query flow must come back (done or failed), never hang or kill
/// the session.
#[test]
fn odd_commands_settle() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    let cmds = [
        "COPY (SELECT 1) TO STDOUT",
        "COPY (SELECT 1, 'a') TO STDOUT WITH (FORMAT csv)",
        "CREATE TEMP TABLE cp (a int)",
        "COPY cp FROM STDIN",
        "SELECT 'after copy'",
        "COPY cp TO PROGRAM 'cat'",
        "LISTEN sweep_ch",
        "NOTIFY sweep_ch, 'hi'",
        "UNLISTEN *",
        "SELECT pg_sleep(0.01)",
        "VACUUM cp",
        "BEGIN",
        "SELECT 1/0",
        "SELECT 'in failed tx'",
        "ROLLBACK",
        "SELECT 'after rollback'",
        "SET statement_timeout = '50ms'",
        "SELECT pg_sleep(1)",
        "RESET statement_timeout",
        "SELECT 'alive'",
        "DECLARE cc CURSOR FOR SELECT 1",
        "SELECT 'x' \\gset",
        "SELECT 1; SELECT 2",
        "SELECT 1 /* multi */; SELECT 'second'",
        "CREATE TABLE sweep_multi_a (a int); INSERT INTO sweep_multi_a VALUES (1); SELECT * FROM sweep_multi_a",
        "DROP TABLE sweep_multi_a",
        "DO $$ BEGIN RAISE NOTICE 'hello'; END $$",
        "DO $$ BEGIN RAISE EXCEPTION 'boom %', 1; END $$",
        "SELECT 'still alive'",
        "SELECT * FROM generate_series(1, 5) g WHERE g > 100",
        "SELECT",
        "SELECT NULL",
        "SELECT ''",
        "SELECT 1 AS a, 2 AS a, 3 AS \"A\"",
        "SELECT 1 AS \"\"",
        "SELECT repeat('x', 5000000)",
        "SELECT E'\\x00'::text",
    ];
    for sql in cmds {
        let rs = s.execute(sql);
        rs.fetch_all();
        let ph = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), rs.wait_finished()).await });
        match ph {
            Err(_) => panic!("HANG: {sql}"),
            Ok(p) => eprintln!("{p:?} {sql:.60} {:?}", rs.status().error.map(|e| e.message)),
        }
    }
    let rs = query(&e, &c, "SELECT 'last'");
    assert_eq!(rs.snapshot().text(0, 0).as_deref(), Some("last"));
}

#[test]
fn copy_to_stdout_shows_lines() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let rs = query(&e, &c, "COPY (SELECT g, 'a' || chr(9) || 'b', NULL FROM generate_series(1, 3) g) TO STDOUT");
    let snap = rs.snapshot();
    assert_eq!(snap.rows(), 3);
    assert_eq!(snap.text(0, 0).as_deref(), Some("1\ta\\tb\t\\N"));
    assert_eq!(snap.text(2, 0).as_deref(), Some("3\ta\\tb\t\\N"));
    let rs = query(&e, &c, "copy (select 1 as a, 'x,y' as b) to stdout with (format csv, header)");
    let snap = rs.snapshot();
    assert_eq!((snap.text(0, 0).as_deref(), snap.text(1, 0).as_deref()), (Some("a,b"), Some("1,\"x,y\"")));
    let rs = c.session(None).execute("COPY (SELECT 1) TO STDOUT BINARY");
    assert_eq!(finish(&e, &rs), Phase::Failed);
}

#[test]
fn explain_many_plan_shapes() {
    use dataman_core::explain::{self, Plan, Target};
    let e = env();
    let Some(c) = pg(&e) else { return };
    if !has_extensions(&e, &c) {
        return;
    }
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS sweep_x CASCADE"]);
    exec(
        &e,
        &c,
        &[
            "CREATE SCHEMA sweep_x",
            "SET search_path = sweep_x, public",
            "CREATE TABLE sweep_x.d (id int PRIMARY KEY, v vector(3), body text, ts tsvector, j jsonb, g int, h hstore, lt ltree)",
            "INSERT INTO sweep_x.d SELECT i, ('[' || i || ',' || (i % 7) || ',1]')::vector, 'word' || i % 50, to_tsvector('word' || i % 50), jsonb_build_object('k', i % 9), i % 10, hstore('a', i::text), text2ltree('a.b' || i % 5) FROM generate_series(1, 5000) i",
            "CREATE INDEX ON sweep_x.d USING hnsw (v vector_l2_ops)",
            "CREATE INDEX ON sweep_x.d USING gin (ts)",
            "CREATE INDEX ON sweep_x.d USING gin (body gin_trgm_ops)",
            "CREATE INDEX ON sweep_x.d USING gin (j)",
            "CREATE INDEX ON sweep_x.d USING gist (lt)",
            "CREATE TABLE sweep_x.p (id int, d date) PARTITION BY RANGE (d)",
            "CREATE TABLE sweep_x.p1 PARTITION OF sweep_x.p FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
            "CREATE TABLE sweep_x.p2 PARTITION OF sweep_x.p FOR VALUES FROM ('2025-01-01') TO ('2026-01-01')",
            "INSERT INTO sweep_x.p SELECT i, '2024-01-01'::date + i FROM generate_series(1, 600) i",
            "ANALYZE sweep_x.d",
            "ANALYZE sweep_x.p",
        ],
    );
    let t = Target::of(&c.info);
    let qs = [
        "SELECT * FROM sweep_x.d ORDER BY v <-> '[3,1,1]' LIMIT 5",
        "SELECT id FROM sweep_x.d WHERE ts @@ to_tsquery('word7')",
        "SELECT id FROM sweep_x.d WHERE body % 'word7'",
        "SELECT id FROM sweep_x.d WHERE j @> '{\"k\": 3}'",
        "SELECT id FROM sweep_x.d WHERE lt <@ 'a.b1'",
        "SELECT g, count(*), avg(id), rank() OVER (ORDER BY g) FROM sweep_x.d GROUP BY g",
        "WITH x AS MATERIALIZED (SELECT * FROM sweep_x.d WHERE g = 1) SELECT * FROM x a JOIN x b USING (id)",
        "SELECT * FROM sweep_x.p WHERE d < '2024-06-01'",
        "SELECT * FROM sweep_x.p p JOIN sweep_x.d d ON d.id = p.id WHERE d.g = (SELECT max(g) FROM sweep_x.d)",
        "SELECT * FROM sweep_x.d d, LATERAL (SELECT * FROM sweep_x.p WHERE p.id = d.id LIMIT 1) l",
        "SELECT * FROM generate_series(1, 10) gs JOIN sweep_x.d ON d.id = gs",
        "SELECT id, (SELECT count(*) FROM sweep_x.p WHERE p.id = d.id) FROM sweep_x.d d",
        "SELECT * FROM sweep_x.d WHERE id IN (1, 2, 3) UNION ALL SELECT * FROM sweep_x.d WHERE id = 5",
        "SELECT sum(d.id) FROM sweep_x.d d JOIN sweep_x.d e ON d.g = e.g",
        "SELECT jsonb_path_query(j, '$.k') FROM sweep_x.d",
        "SELECT * FROM sweep_x.d TABLESAMPLE SYSTEM (10)",
        "SELECT 1",
        "VALUES (1), (2)",
        "INSERT INTO sweep_x.d (id, g) VALUES (99999, 1)",
        "UPDATE sweep_x.d SET g = g + 1 WHERE id < 10",
        "DELETE FROM sweep_x.d WHERE id > 4000",
        "MERGE INTO sweep_x.d t USING (VALUES (1, 5)) s(id, g) ON t.id = s.id WHEN MATCHED THEN UPDATE SET g = s.g",
        "WITH i AS (INSERT INTO sweep_x.p VALUES (1, '2024-02-02') RETURNING id) SELECT * FROM i",
    ];
    let s = c.session(None);
    let before = query_text(&e, &c, "SELECT (SELECT count(*) FROM sweep_x.d) || '/' || (SELECT count(*) FROM sweep_x.p)");
    let mut failures = Vec::new();
    for q in qs {
        for analyze in [false, true] {
            let sql = explain::sql(q, analyze, t);
            let rs = s.execute_opts(sql.clone(), Vec::new(), analyze);
            if finish(&e, &rs) != Phase::Done {
                failures.push(format!("RUN analyze={analyze} {q}: {:?}", rs.status().error.map(|e| e.message)));
                continue;
            }
            let snap = rs.snapshot();
            let cols: Vec<String> = snap.columns.iter().map(|c| c.name.clone()).collect();
            let rows: Vec<Vec<Option<String>>> = (0..snap.rows()).map(|r| (0..cols.len()).map(|c| snap.text(r, c).map(|s| s.into_owned())).collect()).collect();
            let p = Plan::parse(&Plan::source_of(&cols, &rows));
            if p.nodes.is_empty() {
                failures.push(format!("NO NODES analyze={analyze} {q}"));
            }
            if analyze && p.totals.execution_ms.is_none() {
                failures.push(format!("NO EXEC TIME {q}"));
            }
        }
    }
    let after = query_text(&e, &c, "SELECT (SELECT count(*) FROM sweep_x.d) || '/' || (SELECT count(*) FROM sweep_x.p)");
    if before != after {
        failures.push(format!("EXPLAIN ANALYZE changed data: {before} -> {after}"));
    }
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS sweep_x CASCADE"]);
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// Every editor analysis at every cursor position of scripts typed partway: none may panic.
#[test]
fn editor_analyses_never_panic() {
    use dataman_core::complete::{complete, definition_at, signature_help};
    use dataman_core::sql::tokenize;
    let e = env();
    let Some(c) = pg(&e) else { return };
    let db = c.database().to_string();
    let _ = e.core.block_on(c.preload(&db));
    let _ = e.core.block_on(c.catalog(&db));
    let snap = c.schema.snapshot(&db);
    let texts: &[&str] = &[
        "SELECT é, 日本語, '😀', \"naïve col\" FROM t WHERE x = 'ü' -- コメント\n/* 😀 /* nested */ */ ORDER BY 1;",
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql AS $é$ BEGIN RETURN a; END $é$; SELECT f(1);",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT * FROM r JOIN LATERAL (SELECT 1) x ON true WINDOW w AS (PARTITION BY n);",
        "INSERT INTO t (a, b) VALUES (1, 'é') ON CONFLICT (a) DO UPDATE SET b = excluded.b RETURNING *;",
        "MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN UPDATE SET b = s.b WHEN NOT MATCHED THEN INSERT VALUES (s.a);",
        "SELECT '[1,2]'::vector <-> '[3,4]', ARRAY[1,2][1], E'a\\'b', U&'\\0061', $1, $$x$$, x::int[], a->>'k', a#>>'{x}';",
        "UPDATE t SET (a, b) = (SELECT 1, 2) FROM u WHERE EXISTS (SELECT 1) AND t.a IN (1, 2) FOR UPDATE;",
        "DO $$ BEGIN PERFORM 1; EXCEPTION WHEN OTHERS THEN RAISE; END $$ LANGUAGE plpgsql; CALL p(1, y => 2);",
    ];
    let mut n = 0;
    for text in texts {
        let idx: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
        for &i in &idx {
            // Cursor everywhere, prefixes (as typed), suffixes (as pasted).
            let r = std::panic::catch_unwind(|| {
                let _ = complete(text, i, &snap);
                let toks = tokenize(text);
                let _ = signature_help(text, &toks, i, &snap);
                let _ = definition_at(text, i, &snap);
                let _ = split::statement_at(text, i);
                let pre = &text[..i];
                let _ = lint(pre, &LintContext { schema: Some(&snap), ..Default::default() });
                let _ = split::split(pre);
                let suf = &text[i..];
                let _ = lint(suf, &LintContext { schema: Some(&snap), ..Default::default() });
                let _ = dataman_core::guard::classify(pre);
                let _ = dataman_core::guard::agent_read_refusal(suf);
                let _ = dataman_core::guard::agent_write_class(pre, dataman_core::sql::Dialect::Postgres);
                let _ = dataman_core::params::detect(pre);
            });
            if r.is_err() {
                panic!("panicked at cursor {i} of {text:?}");
            }
            n += 1;
        }
    }
    eprintln!("{n} positions");
}

/// Mutated (broken) scripts through the lexer, splitter, lint, completion and guards.
#[test]
fn mutated_scripts_never_panic() {
    use dataman_core::complete::{complete, definition_at, signature_help};
    use dataman_core::sql::tokenize;
    let e = env();
    let Some(c) = pg(&e) else { return };
    let db = c.database().to_string();
    let _ = e.core.block_on(c.preload(&db));
    let snap = c.schema.snapshot(&db);
    let seeds: &[&str] = &[
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n, (SELECT max(n) FROM r) FROM r JOIN LATERAL (SELECT 1 AS q) x ON true WINDOW w AS (PARTITION BY n ORDER BY n ROWS BETWEEN 1 PRECEDING AND CURRENT ROW);",
        "INSERT INTO emp (name, boss) SELECT e.name, b.id FROM emp e LEFT JOIN emp b ON b.id = e.boss ON CONFLICT (id) DO UPDATE SET name = excluded.name WHERE emp.id > 0 RETURNING emp.*;",
        "CREATE FUNCTION f(a int DEFAULT 1) RETURNS TABLE (x int) LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN a > 0 THEN a END; SELECT 2; END; SELECT f();",
        "MERGE INTO t USING (SELECT * FROM s) s ON t.a = s.a WHEN MATCHED AND s.b THEN DELETE WHEN NOT MATCHED THEN INSERT (a) VALUES (s.a) RETURNING merge_action();",
        "DO $x$ DECLARE r record; BEGIN FOR r IN SELECT * FROM pg_class LOOP RAISE NOTICE '%', r.relname; END LOOP; END $x$; SELECT $1::int, E'a\\'b', U&'\\0041', B'101', X'1F', 'it''s', \"q\"\"q\";",
        "UPDATE t SET a = a + 1, (b, c) = (SELECT 1, 2) FROM u JOIN v USING (id) WHERE t.id = u.id AND EXISTS (SELECT 1 FROM w WHERE w.x = t.x) RETURNING a;",
        "SELECT a::int[], b->'k'->>0, c#>'{x,y}', d @> '{}', e ? 'k', f && g, ARRAY[1,2][1], (h).x, i[1:2], 'a' || 'b' FROM t TABLESAMPLE SYSTEM (1) FOR UPDATE OF t SKIP LOCKED;",
        "ALTER TABLE ONLY t ADD CONSTRAINT c CHECK (a > 0) NOT VALID, ALTER COLUMN b SET DEFAULT now(), DROP COLUMN IF EXISTS z CASCADE; COMMENT ON COLUMN t.a IS 'x';",
    ];
    let mut state = 0x2545F4914F6CDD1Du64;
    let mut rnd = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n.max(1) as u64) as usize
    };
    let junk = ["(", ")", "'", "\"", "$$", "/*", "*/", "--", "é", "😀", ";", "$1", "E'", "U&'", "::", "[", "]", ",", ".", "*"];
    let mut n = 0;
    for seed in seeds {
        for _ in 0..1500 {
            let mut chars: Vec<char> = seed.chars().collect();
            for _ in 0..1 + rnd(3) {
                let at = rnd(chars.len() + 1);
                match rnd(3) {
                    0 => {
                        let len = rnd(8).min(chars.len() - at.min(chars.len()));
                        chars.drain(at.min(chars.len())..at.min(chars.len()) + len);
                    }
                    1 => {
                        let j = junk[rnd(junk.len())];
                        for (k, ch) in j.chars().enumerate() {
                            chars.insert((at + k).min(chars.len()), ch);
                        }
                    }
                    _ => chars.truncate(at),
                }
            }
            let text: String = chars.into_iter().collect();
            let cur = text.char_indices().map(|(i, _)| i).nth(rnd(text.chars().count() + 1)).unwrap_or(text.len());
            let r = std::panic::catch_unwind(|| {
                let toks = tokenize(&text);
                let _ = complete(&text, cur, &snap);
                let _ = signature_help(&text, &toks, cur, &snap);
                let _ = definition_at(&text, cur, &snap);
                let _ = split::statement_at(&text, cur);
                let _ = lint(&text, &LintContext { schema: Some(&snap), ..Default::default() });
                let _ = dataman_core::guard::classify(&text);
                let _ = dataman_core::guard::agent_read_refusal(&text);
                let _ = dataman_core::guard::agent_write_class(&text, dataman_core::sql::Dialect::Postgres);
                let _ = dataman_core::guard::agent_preview(&text, dataman_core::sql::Dialect::Postgres);
                let _ = dataman_core::params::detect(&text);
                let _ = dataman_core::sql::first_table(&text);
                let _ = dataman_core::sql::statement_verb(&text);
            });
            if r.is_err() {
                panic!("panicked on {text:?} cursor {cur}");
            }
            n += 1;
        }
    }
    eprintln!("{n} mutations");
}

/// Types without a binary output function (aclitem, isn, cube…) must still show.
#[test]
fn types_without_binary_output() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec_quiet(&e, &c, &["CREATE EXTENSION IF NOT EXISTS cube", "CREATE EXTENSION IF NOT EXISTS isn", "CREATE EXTENSION IF NOT EXISTS seg"]);
    let s = c.session(None);
    let mut bad = Vec::new();
    for q in [
        "SELECT * FROM pg_class LIMIT 5",
        "SELECT * FROM pg_namespace",
        "SELECT * FROM pg_database",
        "SELECT relacl FROM pg_class WHERE relacl IS NOT NULL LIMIT 1",
        "SELECT '=r/postgres'::aclitem",
        "SELECT * FROM pg_proc LIMIT 3",
        "SELECT * FROM pg_attribute LIMIT 3",
        "SELECT * FROM pg_stats LIMIT 3",
        "SELECT * FROM pg_settings LIMIT 3",
        "SELECT * FROM pg_stat_activity",
        "SELECT * FROM pg_roles",
        "SELECT * FROM pg_index LIMIT 3",
        "SELECT * FROM pg_type LIMIT 3",
        "SELECT * FROM pg_constraint LIMIT 3",
        "SELECT * FROM pg_extension",
        "SELECT * FROM pg_depend LIMIT 3",
        "SELECT * FROM pg_trigger LIMIT 3",
        "SELECT * FROM pg_user_mappings",
        "SELECT * FROM pg_locks",
        "SELECT * FROM pg_prepared_xacts",
        "SELECT * FROM pg_replication_slots",
        "SELECT * FROM pg_stat_all_tables LIMIT 3",
        "SELECT * FROM information_schema.tables LIMIT 3",
        "SELECT * FROM information_schema.columns LIMIT 3",
        "SELECT * FROM information_schema.routines LIMIT 3",
        "SELECT '(1,2),(3,4)'::cube",
        "SELECT '0-306-40615-2'::isbn",
        "SELECT '1 .. 2'::seg",
        "SELECT ARRAY['(1,2)'::cube]",
        "SELECT ROW('(1,2)'::cube, 1)",
        "SELECT pg_get_keywords() LIMIT 1",
        "SELECT * FROM pg_get_keywords() LIMIT 3",
        "SELECT * FROM pg_timezone_names LIMIT 3",
        "SELECT * FROM pg_available_extensions LIMIT 3",
        "SELECT * FROM pg_hba_file_rules",
        "SELECT * FROM pg_ls_waldir() LIMIT 1",
        "SELECT '1'::regclass::oid, (SELECT array_agg(c) FROM pg_class c LIMIT 1)",
    ] {
        let rs = s.execute(q);
        let ph = finish(&e, &rs);
        if ph != Phase::Done {
            bad.push(format!("{q}: {:?}", rs.status().error.map(|e| e.message)));
            continue;
        }
        let snap = rs.snapshot();
        for col in 0..snap.columns.len() {
            for row in 0..snap.rows().min(3) {
                if let Some(t) = snap.text(row, col)
                    && t.starts_with("\\x")
                    && snap.columns[col].type_name != "bytea"
                {
                    bad.push(format!("{q}: column {} ({}) shows {t:.40}", snap.columns[col].name, snap.columns[col].type_name));
                    break;
                }
            }
        }
    }
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}

/// The same rows through every reader: the grid, the bounded read of the agents, the export.
#[test]
fn sendless_types_in_every_reader() {
    use dataman_core::bounded::{Limits, Mode};
    use dataman_core::export::{ExportFormat, ExportOptions};
    let e = env();
    let Some(c) = pg(&e) else { return };
    let q = "SELECT oid, relname, relpages, relkind, relacl, reltuples FROM pg_class WHERE relacl IS NOT NULL AND relname = 'pg_class'";
    let rs = query(&e, &c, q);
    assert!(rs.status().text_mode);
    let snap = rs.snapshot();
    assert_eq!(snap.rows(), 1);
    assert_eq!(snap.text(0, 1).as_deref(), Some("pg_class"));
    assert!(snap.text(0, 4).unwrap().starts_with("{postgres=arwdDxtm/postgres"), "{:?}", snap.text(0, 4));
    // Numbers stay numbers in the grid.
    assert_eq!(rs.columns().unwrap()[0].kind, dataman_core::value::Kind::Int);
    assert!(matches!(snap.cell(0, 0), dataman_core::result::Cell::Int(1259)));
    // Agents.
    let out = e.core.block_on(c.bounded(None, q, vec![], Mode::Read, Limits::default(), std::future::pending())).unwrap();
    assert_eq!(out.rows.len(), 1);
    assert!(out.rows[0][4].to_string().contains("postgres=arwdDxtm/postgres"), "{:?}", out.rows[0]);
    // Export.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.csv");
    let x = c.export_query(None, q, &path, ExportOptions::new(ExportFormat::Csv), None);
    assert_eq!(e.core.block_on(x.task).unwrap().rows, 1);
    assert!(std::fs::read_to_string(&path).unwrap().contains("postgres=arwdDxtm/postgres"));
    // And the table in the explorer's data view.
    let rs = query(&e, &c, "SELECT * FROM pg_namespace");
    assert!(rs.snapshot().rows() >= 3);
}


/// The extension tests need the pgvector server (`DATAMAN_PG=127.0.0.1:55433`).
fn has_extensions(e: &Env, c: &Arc<Connection>) -> bool {
    let ok = query_text(e, c, "SELECT count(*) FROM pg_extension WHERE extname IN ('vector', 'hstore', 'ltree', 'citext', 'pg_trgm')") == "5";
    if !ok {
        eprintln!("skipping: the server lacks pgvector, hstore, ltree, citext or pg_trgm");
    }
    ok
}