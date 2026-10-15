//! Editor intelligence against real servers: the function and type catalog (signature help,
//! enum labels), go to definition (and the definitions it opens), parameters bound through
//! the extended protocol, and lint staying quiet on LAR. Skips without servers.

mod common;

use common::*;
use diskman_core::complete::{Definition, complete, definition_at, signature_help};
use diskman_core::lint::{LintContext, lint};
use diskman_core::params;
use diskman_core::sql::tokenize;

fn wait<T>(e: &Env, t: diskman_core::Task<T>) -> T {
    e.core.block_on(t).expect("task")
}

fn finish(e: &Env, rs: &Arc<diskman_core::ResultSet>) {
    rs.fetch_all();
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), rs.wait_finished()).await.expect("timeout") });
}

#[test]
fn catalog_signatures_and_definitions_on_postgres() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec_quiet(&e, &c, &["DROP SCHEMA IF EXISTS diskman_intel CASCADE"]);
    exec(
        &e,
        &c,
        &[
            "CREATE SCHEMA diskman_intel",
            "CREATE TYPE diskman_intel.mood AS ENUM ('happy', 'tense', 'content')",
            "CREATE TABLE diskman_intel.person (id bigint PRIMARY KEY, name text, mood diskman_intel.mood)",
            "CREATE VIEW diskman_intel.happy_people AS SELECT id, name FROM diskman_intel.person WHERE mood = 'happy'",
            "CREATE FUNCTION diskman_intel.rate(amount numeric, pct numeric DEFAULT 0.1) RETURNS numeric LANGUAGE sql AS 'SELECT amount * pct'",
        ],
    );
    let db = c.database().to_string();
    let t0 = Instant::now();
    let cat = wait(&e, c.catalog(&db));
    eprintln!("catalog: {} functions, {} types in {:?}", cat.functions.len(), cat.types.len(), t0.elapsed());
    assert!(cat.functions_named(None, "date_trunc").count() >= 2, "built-ins are in");
    assert!(cat.functions_named(None, "int4in").next().is_none(), "type I/O is not");
    let rate = cat.functions_named(Some("diskman_intel"), "rate").next().expect("rate");
    assert_eq!(rate.args, "amount numeric, pct numeric DEFAULT 0.1");
    assert_eq!(rate.result, "numeric");
    assert_eq!(cat.enum_labels("diskman_intel.mood", &[]).unwrap(), &["happy".to_string(), "tense".into(), "content".into()]);
    // Cached: the second call returns the same catalog.
    assert!(Arc::ptr_eq(&cat, &wait(&e, c.catalog(&db))));

    wait(&e, c.preload(&db));
    wait(&e, c.relations(&db, "diskman_intel"));
    wait(&e, c.columns(&db, "diskman_intel", "person"));
    let snap = c.schema.snapshot(&db);
    assert!(snap.catalog.is_some() && snap.complete);

    // Signature help on the live catalog.
    let src = "select diskman_intel.rate(10, ";
    let h = signature_help(src, &tokenize(src), src.len(), &snap).expect("signature help");
    assert_eq!((h.function.name.as_str(), h.active), ("rate", Some(1)));
    assert!(h.params[1].starts_with("pct numeric"));
    let src = "select date_trunc('day', ";
    let h = signature_help(src, &tokenize(src), src.len(), &snap).expect("date_trunc");
    assert_eq!(h.active, Some(1));
    assert!(h.overloads >= 2);

    // Enum labels inside a string compared with an enum column.
    let src = "select * from diskman_intel.person p where p.mood = 'te";
    let comp = complete(src, src.len(), &snap);
    assert_eq!(comp.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), vec!["tense", "content"], "prefix first, then substring");

    // Go to definition: a view, a column, a function, then their definitions.
    let src = "select a.name from diskman_intel.happy_people a where diskman_intel.rate(a.id) > 1";
    let at = |needle: &str| definition_at(src, src.find(needle).unwrap() + 1, &snap);
    assert_eq!(at("happy_people"), Some(Definition::Relation { schema: "diskman_intel".into(), name: "happy_people".into(), column: None }));
    assert_eq!(at("name"), Some(Definition::Relation { schema: "diskman_intel".into(), name: "happy_people".into(), column: Some("name".into()) }));
    let Some(Definition::Function { oid, name, .. }) = at("rate") else { panic!("function") };
    assert_eq!(name, "rate");
    let def = wait(&e, c.function_definition(&db, oid)).expect("function definition");
    assert!(def.contains("CREATE OR REPLACE FUNCTION diskman_intel.rate"), "{def}");
    let view = wait(&e, c.view_definition(&db, "diskman_intel", "happy_people")).expect("view definition");
    assert!(view.to_uppercase().contains("SELECT"), "{view}");

    // Lint against the live schema: a typo is caught, the real names are not.
    let l = lint("select p.nam from diskman_intel.person p", &LintContext { schema: Some(&snap), ..Default::default() });
    assert_eq!(l.iter().map(|x| x.message.as_str()).collect::<Vec<_>>(), vec!["column p.nam does not exist"]);
    assert!(lint("select p.name, mood from diskman_intel.person p join orders x on x.id = p.id", &LintContext { schema: Some(&snap), ..Default::default() }).is_empty());

    exec(&e, &c, &["DROP SCHEMA diskman_intel CASCADE"]);
}

#[test]
fn parameters_bind_through_the_extended_protocol() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = c.session(None);
    // $n with a NULL and a value the server parses as the parameter's type.
    let rs = s.execute_with("SELECT $1::int + 1 AS n, $2::text IS NULL AS null, $3::date AS d", vec![Some("41".into()), None, Some("2026-10-13".into())]);
    finish(&e, &rs);
    assert_eq!(rs.phase(), Phase::Done, "{:?}", rs.status().error);
    let snap = rs.snapshot();
    assert_eq!(snap.text(0, 0).unwrap(), "42");
    assert_eq!(snap.text(0, 1).unwrap(), "true");
    assert_eq!(snap.text(0, 2).unwrap(), "2026-10-13");
    // psql variables are rewritten to $n: a value with quotes stays a value.
    let p = params::detect("SELECT :n::int * 2 AS doubled, :'name' AS name, :n::int AS again").unwrap();
    assert_eq!(p.sql, "SELECT $1::int * 2 AS doubled, $2 AS name, $1::int AS again");
    let rs = s.execute_with(p.sql.clone(), vec![Some("21".into()), Some("o'brien'); drop table x; --".into())]);
    finish(&e, &rs);
    assert_eq!(rs.phase(), Phase::Done, "{:?}", rs.status().error);
    let snap = rs.snapshot();
    assert_eq!(snap.text(0, 0).unwrap(), "42");
    assert_eq!(snap.text(0, 1).unwrap(), "o'brien'); drop table x; --");
    // A wrong value is the server's error, never a client-side splice.
    let rs = s.execute_with("SELECT $1::int", vec![Some("abc".into())]);
    finish(&e, &rs);
    assert_eq!(rs.phase(), Phase::Failed);
    assert_eq!(rs.status().error.unwrap().code.as_deref(), Some("22P02"));
}

#[test]
fn lar_catalog_degrades_quietly() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    let db = c.database().to_string();
    // Whatever LAR's catalog has, loading it never fails.
    let cat = wait(&e, c.catalog(&db));
    eprintln!("LAR catalog: {} functions, {} types", cat.functions.len(), cat.types.len());
    wait(&e, c.preload(&db));
    let snap = c.schema.snapshot(&db);
    let l = lint("select x from lar_not_in_catalog", &LintContext { schema: Some(&snap), lar: true, ..Default::default() });
    assert!(l.is_empty(), "LAR gaps are not reported: {l:?}");
    let rs = c.session(None).execute_with("SELECT $1::bigint + 1", vec![Some("1".into())]);
    finish(&e, &rs);
    eprintln!("LAR $1 binding: {:?} {:?}", rs.phase(), rs.status().error);
}
