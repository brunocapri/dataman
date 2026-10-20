mod common;

use common::*;
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::nosql::{Body, Failed, ItemKind, Output, Page, Ran, Source};

fn profile(engine: Engine) -> (ConnectionProfile, Option<String>) {
    let (var, default, user, pw) = match engine {
        Engine::Elasticsearch => ("DATAMAN_ES", "127.0.0.1:9201", "elastic", Some(std::env::var("DATAMAN_ES_PASSWORD").unwrap_or_else(|_| "dataman".into()))),
        _ => ("DATAMAN_OPENSEARCH", "127.0.0.1:9200", "", None),
    };
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (host, port) = v.rsplit_once(':').unwrap();
    let p = ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: user.into(), database: String::new(), sslmode: SslMode::Disable, engine, ..ConnectionProfile::new("search") };
    (p, pw)
}

fn connect(e: &Env, p: ConnectionProfile, pw: Option<String>) -> Option<Arc<Connection>> {
    match e.core.block_on(e.core.connect(p.clone(), pw)) {
        Ok(c) => Some(c),
        Err(err) => {
            eprintln!("skipping: cannot reach {}:{} ({err:#})", p.host, p.port);
            None
        }
    }
}

fn run(e: &Env, c: &Arc<Connection>, text: &str, page: Page) -> Result<Ran, Failed> {
    let r = c.run_nosql("", text, page);
    match e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), r).await.expect("timeout") }) {
        Ok(ran) => Ok(ran),
        Err(err) => Err(err.downcast_ref::<Failed>().cloned().unwrap_or_else(|| Failed { message: format!("{err:#}"), at: None })),
    }
}

fn ok(e: &Env, c: &Arc<Connection>, text: &str) -> Output {
    run(e, c, text, Page::default()).unwrap_or_else(|f| panic!("{text}: {f:?}")).output
}

fn exercise(engine: Engine) {
    let e = env();
    let (p, pw) = profile(engine);
    let Some(c) = connect(&e, p.clone(), pw.clone()) else { return };
    let idx = format!("dataman_t_{}", engine.slug());
    let _ = run(&e, &c, &format!("DELETE /{idx}"), Page::default());
    let name = if engine == Engine::OpenSearch { "OpenSearch " } else { "Elasticsearch " };
    assert!(c.info.version.starts_with(name), "{}", c.info.version);

    ok(&e, &c, &format!("PUT /{idx}\n{{mappings: {{properties: {{n: {{type: 'integer'}}, name: {{type: 'keyword'}}, at: {{type: 'date'}}, addr: {{properties: {{city: {{type: 'keyword'}}}}}}}}}}}}"));
    let mut bulk = format!("POST /{idx}/_bulk?refresh=true\n");
    for i in 0..120 {
        bulk.push_str(&format!("{{\"index\": {{\"_id\": \"{i}\"}}}}\n{{\"n\": {i}, \"name\": \"user {i}\", \"even\": {}}}\n", i % 2 == 0));
    }
    let Output::Written { affected, .. } = ok(&e, &c, &bulk) else { panic!() };
    assert_eq!(affected, 120);

    let items = e.core.block_on(c.nosql_tree(None)).unwrap();
    let it = items.iter().find(|i| i.name == idx).unwrap_or_else(|| panic!("{items:?}"));
    assert_eq!((it.kind, it.count), (ItemKind::Collection, Some(120)));
    let fields = e.core.block_on(c.sample_fields("", &idx)).unwrap();
    assert_eq!(fields[0].0, "_id");
    assert!(fields.iter().any(|(f, t)| f == "addr.city" && t == &vec!["keyword"]), "{fields:?}");

    let search = format!("GET /{idx}/_search\n{{query: {{match_all: {{}}}}, sort: [{{n: 'asc'}}]}}");
    let page = |skip| match run(&e, &c, &search, Page { skip, limit: 50 }).unwrap().output {
        Output::Docs(d) => d,
        o => panic!("{o:?}"),
    };
    let p1 = page(0);
    assert_eq!((p1.docs.len(), p1.total, p1.more), (50, Some(120), true));
    assert_eq!(p1.source, Some(Source::Search { index: idx.clone() }));
    assert_eq!(p1.docs[0].index.as_deref(), Some(idx.as_str()));
    assert_eq!(p1.docs[0].id, Some(Body::Json(serde_json::json!("0"))));
    let p3 = page(100);
    assert_eq!((p3.docs.len(), p3.more), (20, false));
    assert!(p3.docs[0].body.text().contains("\"n\": 100"), "{}", p3.docs[0].body.text());
    let Output::Docs(d) = run(&e, &c, &format!("GET /{idx}/_search\n{{size: 60, query: {{term: {{even: true}}}}}}"), Page { skip: 50, limit: 50 }).unwrap().output else { panic!() };
    assert_eq!((d.docs.len(), d.more), (10, false));
    let Output::Docs(d) = ok(&e, &c, &format!("GET /{idx}/_search\n{{_source: ['n']}}")) else { panic!() };
    assert_eq!(d.source, None);
    let Output::Value(Body::Json(v)) = ok(&e, &c, &format!("GET /{idx}/_search\n{{size: 0, aggs: {{evens: {{terms: {{field: 'even'}}}}}}}}")) else { panic!() };
    assert_eq!(v["aggregations"]["evens"]["buckets"].as_array().map(Vec::len), Some(2), "{v}");
    let Output::Value(Body::Json(v)) = ok(&e, &c, &format!("GET /{idx}/_count")) else { panic!() };
    assert_eq!(v["count"], 120);

    let src = Source::Search { index: idx.clone() };
    let id = e.core.block_on(c.save_document(&src, Some(Body::Json(serde_json::json!("0"))), "{n: 0, name: 'Ada', even: true}")).unwrap();
    assert_eq!(id, Body::Json(serde_json::json!("0")));
    let Output::Docs(d) = ok(&e, &c, &format!("GET /{idx}/_search\n{{query: {{term: {{name: 'Ada'}}}}}}")) else { panic!() };
    assert_eq!(d.docs.len(), 1);
    let Output::Docs(d) = ok(&e, &c, &format!("GET /{idx}/_search\n{{query: {{match_all: {{}}}}}}")) else { panic!() };
    assert_eq!(d.docs.len(), 50);
    assert_ne!(d.docs[0].id, Some(Body::Json(serde_json::json!("0"))), "the edited document stayed first");
    assert!(d.docs.iter().all(|x| x.id != Some(Body::Json(serde_json::json!("0")))), "the edited document is still on the first page");
    let count = |q: &str| match ok(&e, &c, &format!("GET /{idx}/_count\n{{query: {{term: {{name: '{q}'}}}}}}")) {
        Output::Value(Body::Json(v)) => v["count"].as_u64(),
        o => panic!("{o:?}"),
    };
    let new = e.core.block_on(c.save_document(&src, None, "{n: 500, name: 'new'}")).unwrap();
    assert_eq!(count("new"), Some(1));
    assert_eq!(e.core.block_on(c.delete_document(&src, new)).unwrap(), 1);
    assert_eq!(count("new"), Some(0));
    let err = e.core.block_on(c.save_document(&src, None, "[1, 2]")).unwrap_err();
    assert!(format!("{err:#}").contains("object"), "{err:#}");

    let del = format!("POST /{idx}/_delete_by_query?refresh=true\n{{query: {{range: {{n: {{gte: 100}}}}}}}}");
    assert_eq!(e.core.block_on(c.nosql_affects("", &del)).unwrap(), Some(20));
    let Output::Written { affected, .. } = ok(&e, &c, &del) else { panic!() };
    assert_eq!(affected, 20);

    let f = run(&e, &c, "GET /dataman_no_such_index/_search", Page::default()).unwrap_err();
    assert!(f.message.contains("index_not_found_exception") && f.message.contains("404"), "{f:?}");
    let f = run(&e, &c, &format!("GET /{idx}/_search\n{{query: }}"), Page::default()).unwrap_err();
    assert!(f.at.is_some(), "{f:?}");

    let mut ro = p.clone();
    ro.read_only = true;
    let roc = connect(&e, ro, pw.clone()).unwrap();
    let f = run(&e, &roc, &format!("DELETE /{idx}/_doc/1"), Page::default()).unwrap_err();
    assert!(f.message.contains("read-only"), "{f:?}");
    assert!(e.core.block_on(roc.save_document(&src, None, "{a: 1}")).is_err());
    assert!(run(&e, &roc, &search, Page::default()).is_ok());

    ok(&e, &c, &format!("DELETE /{idx}"));
    if engine == Engine::Elasticsearch {
        let err = e.core.block_on(e.core.connect(p, Some("wrong".into()))).err().expect("refused");
        assert!(format!("{err:#}").contains("authentication failed"), "{err:#}");
    }
}

#[test]
fn elasticsearch() {
    exercise(Engine::Elasticsearch);
}

#[test]
fn opensearch() {
    exercise(Engine::OpenSearch);
}
