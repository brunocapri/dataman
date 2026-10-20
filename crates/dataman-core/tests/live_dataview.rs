mod common;
use common::*;
use dataman_core::dataview::{Filter, Op, RowCount, TableView};
use dataman_core::jsontree::{self, Doc};
use dataman_core::result::SortDir;
use dataman_core::value::Kind;

const S: &str = "dataman_dataview_scratch";

fn ids(e: &Env, c: &Arc<Connection>, sql: &str) -> Vec<i64> {
    let rs = query(e, c, sql.trim_end_matches(';'));
    let snap = rs.snapshot();
    (0..snap.rows()).map(|r| snap.text(r, 0).unwrap().parse().unwrap()).collect()
}

fn setup(e: &Env, c: &Arc<Connection>) {
    exec_quiet(e, c, &[&format!("DROP SCHEMA IF EXISTS {S} CASCADE")]);
    exec(
        e,
        c,
        &[
            &format!("CREATE SCHEMA {S}"),
            &format!("CREATE TYPE {S}.level AS ENUM ('low', 'mid', 'high')"),
            &format!(
                "CREATE TABLE {S}.\"Item\" (id int, part int, lvl {S}.level, doc jsonb, raw json, at timestamptz, ok boolean, \"Name\" text, \
                 PRIMARY KEY (part, id))"
            ),
            &format!(
                "INSERT INTO {S}.\"Item\" SELECT i, i % 2, (ARRAY['low','mid','high'])[1 + i % 3]::{S}.level, \
                 jsonb_build_object('n', i, 'tag', CASE WHEN i % 4 = 0 THEN 'x_y' ELSE 'xy' END), \
                 json_build_object('n', i), timestamptz '2026-01-01 00:00:00+00' + i * interval '1 day', \
                 CASE WHEN i % 5 = 0 THEN NULL ELSE i % 2 = 0 END, CASE WHEN i = 7 THEN NULL ELSE 'it''s ' || i END \
                 FROM generate_series(1, 100) i"
            ),
            &format!("ANALYZE {S}.\"Item\""),
        ],
    );
}

#[test]
fn filters_sort_and_pages_on_the_server() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    setup(&e, &c);
    let key = e.core.block_on(c.primary_key(c.database(), S, "Item")).unwrap();
    assert_eq!(key, vec!["part".to_string(), "id".to_string()], "the key, in key order");
    let mut v = TableView::new(S, "Item", vec!["id".into()], 10);
    assert_eq!(ids(&e, &c, &v.sql()), (1..=10).collect::<Vec<_>>());
    v.page = 2;
    assert_eq!(ids(&e, &c, &v.sql()), (21..=30).collect::<Vec<_>>(), "OFFSET pages");
    v.page = 0;
    v.page_size = 1000;
    let run = |v: &TableView| ids(&e, &c, &v.sql());
    let with = |f: Filter| {
        let mut w = v.clone();
        w.filters = vec![f];
        w
    };
    assert_eq!(run(&with(Filter::new("lvl", Kind::Text, Op::Eq, "high"))).len(), 33);
    assert_eq!(run(&with(Filter::new("lvl", Kind::Text, Op::In, "low, 'mid'"))).len(), 67);
    assert_eq!(run(&with(Filter::new("ok", Kind::Bool, Op::Eq, "true"))), (1..=100).filter(|i| i % 5 != 0 && i % 2 == 0).collect::<Vec<_>>());
    assert_eq!(run(&with(Filter::new("ok", Kind::Bool, Op::IsNull, ""))).len(), 20);
    assert_eq!(run(&with(Filter::new("ok", Kind::Bool, Op::In, "false, NULL"))).len(), 60);
    assert_eq!(run(&with(Filter::new("at", Kind::TimestampTz, Op::Ge, "2026-04-01 00:00:00+00"))), (90..=100).collect::<Vec<_>>());
    assert_eq!(run(&with(Filter::new("doc", Kind::Json, Op::Eq, "{\"tag\": \"xy\", \"n\": 3}"))), vec![3], "jsonb equality, any key order");
    assert_eq!(run(&with(Filter::new("raw", Kind::Json, Op::Eq, "{\"n\": 5}"))), vec![5], "json compared as jsonb");
    assert_eq!(run(&with(Filter::new("doc", Kind::Json, Op::Contains, "x_y"))).len(), 25, "_ is not a wildcard");
    assert_eq!(run(&with(Filter::new("Name", Kind::Text, Op::Eq, "it's 9"))), vec![9], "a quote in the value, a quoted name");
    assert_eq!(run(&with(Filter::new("Name", Kind::Text, Op::Eq, "NULL"))), vec![7]);
    assert_eq!(run(&with(Filter::new("Name", Kind::Text, Op::StartsWith, "IT'S 1"))).len(), 12, "case-insensitive prefix");
    assert_eq!(run(&with(Filter::new("id", Kind::Int, Op::Contains, "99"))), vec![99], "a number matched on its text");
    assert!(run(&with(Filter::new("id", Kind::Int, Op::In, ""))).is_empty());
    let mut s = v.clone();
    s.sort = Some(("at".into(), SortDir::Desc));
    s.page_size = 5;
    s.page = 1;
    assert_eq!(ids(&e, &c, &s.sql()), vec![95, 94, 93, 92, 91]);
    s.filters = vec![Filter::new("lvl", Kind::Text, Op::Eq, "low")];
    s.where_raw = "id < 10 OR id > 95".into();
    s.page = 0;
    s.page_size = 100;
    assert_eq!(ids(&e, &c, &s.sql()), vec![99, 96, 9, 6, 3]);
    let n = e.core.block_on(c.count_rows(c.database(), s.count_sql()).task).unwrap();
    assert_eq!(n, 5);
    let est = e.core.block_on(c.estimate_rows(c.database(), S, "Item")).unwrap();
    assert_eq!(est, Some(100), "reltuples after ANALYZE");
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {S} CASCADE")]);
}

#[test]
fn counts_stop_and_enum_labels() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let RowCount { task, stop } = c.count_rows(c.database(), "SELECT count(*) FROM pg_sleep(30)".into());
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(300));
    drop(stop);
    let r = e.core.block_on(async { tokio::time::timeout(Duration::from_secs(10), task).await.expect("the count did not stop") });
    assert!(r.is_err());
    assert!(t0.elapsed() < Duration::from_secs(10));
    let s = "dataman_dataview_enum";
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {s} CASCADE")]);
    exec(&e, &c, &[&format!("CREATE SCHEMA {s}"), &format!("CREATE TYPE {s}.mood AS ENUM ('sad', 'ok', 'happy')"), &format!("CREATE TABLE {s}.t (m {s}.mood)")]);
    let rs = query(&e, &c, &format!("SELECT m, 1 AS n FROM {s}.t"));
    let cols = rs.columns().unwrap();
    let labels = e.core.block_on(c.enum_labels(c.database(), cols[0].type_oid)).unwrap();
    assert_eq!(*labels, vec!["sad".to_string(), "ok".into(), "happy".into()]);
    assert!(e.core.block_on(c.enum_labels(c.database(), cols[1].type_oid)).unwrap().is_empty());
    assert!(e.core.block_on(c.primary_key(c.database(), s, "t")).unwrap().is_empty(), "no key");
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {s} CASCADE")]);
}

#[test]
fn json_tree_edits_round_trip_through_the_server() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let s = "dataman_dataview_json";
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {s} CASCADE")]);
    exec(
        &e,
        &c,
        &[
            &format!("CREATE SCHEMA {s}"),
            &format!("CREATE TABLE {s}.t (id int PRIMARY KEY, b jsonb, j json)"),
            &format!("INSERT INTO {s}.t VALUES (1, '{{\"name\": \"ana\", \"tags\": [\"a\"], \"n\": 1.50}}', '{{\"z\": 1, \"a\": [true]}}')"),
        ],
    );
    let rs = query(&e, &c, &format!("SELECT b, j FROM {s}.t"));
    let (b, j) = (rs.snapshot().text(0, 0).unwrap().into_owned(), rs.snapshot().text(0, 1).unwrap().into_owned());
    let mut d = Doc::parse(&b).unwrap();
    let name = d.rows().iter().find(|r| matches!(&r.key, Some(jsontree::RowKey::Key(k)) if k == "name")).unwrap().path.clone();
    d.set_text(&name, "Ana \"Lee\"");
    let tags = d.rows().iter().find(|r| matches!(&r.key, Some(jsontree::RowKey::Key(k)) if k == "tags")).unwrap().path.clone();
    let p = d.add(&tags).unwrap();
    d.set_text(&p, "{\"deep\": null}");
    let mut dj = Doc::parse(&j).unwrap();
    dj.rename(&[0], "zz").unwrap();
    let (nb, nj) = (d.root.compact(), dj.root.compact());
    let lit = dataman_core::sqlgen::quote_literal;
    exec(&e, &c, &[&format!("UPDATE {s}.t SET b = {}, j = {} WHERE id = 1", lit(&nb), lit(&nj))]);
    let back = query(&e, &c, &format!("SELECT b, j, b = {}::jsonb FROM {s}.t", lit(&nb)));
    let snap = back.snapshot();
    assert_eq!(snap.text(0, 2).as_deref(), Some("true"), "the server reads the edited tree as written");
    assert_eq!(jsontree::parse(&snap.text(0, 1).unwrap()).unwrap(), dj.root);
    assert!(snap.text(0, 0).unwrap().contains("\"Ana \\\"Lee\\\"\""));
    exec_quiet(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {s} CASCADE")]);
}
