//! What the NoSQL classifiers call a read must be what the server runs: a statement the agents'
//! read tools accept may not change anything.

mod common;

use common::*;
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::guard::WriteClass;
use dataman_core::nosql::{self, Failed, Output, Page, Ran};

fn search_profile(engine: Engine) -> (ConnectionProfile, Option<String>) {
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

fn run(e: &Env, c: &Arc<Connection>, db: &str, text: &str) -> Result<Ran, Failed> {
    let r = c.run_nosql(db, text, Page::default());
    match e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), r).await.expect("timeout") }) {
        Ok(ran) => Ok(ran),
        Err(err) => Err(err.downcast_ref::<Failed>().cloned().unwrap_or_else(|| Failed { message: format!("{err:#}"), at: None })),
    }
}

/// Runs `text` the way nosql_query does: only when it is classified as a read.
fn read_tool(e: &Env, c: &Arc<Connection>, db: &str, text: &str) -> String {
    match nosql::classify(c.engine(), text) {
        Ok(WriteClass::Read) => match run(e, c, db, text) {
            Ok(_) => "ran".into(),
            Err(f) => f.message,
        },
        Ok(other) => format!("not a read: {other:?}"),
        Err(f) => format!("refused: {}", f.message),
    }
}

fn search_paths(engine: Engine) {
    let e = env();
    let (p, pw) = search_profile(engine);
    let Some(c) = connect(&e, p, pw) else { return };
    let idx = format!("dataman_guard_{}", engine.slug());
    let _ = run(&e, &c, "", &format!("DELETE /{idx}"));
    run(&e, &c, "", &format!("POST /{idx}/_bulk?refresh=true\n{{\"index\": {{\"_id\": \"1\"}}}}\n{{\"n\": 1}}\n{{\"index\": {{\"_id\": \"2\"}}}}\n{{\"n\": 2}}\n")).unwrap();
    let count = |e: &Env| -> u64 {
        let Ok(Ran { output: Output::Value(v), .. }) = run(e, &c, "", &format!("GET /{idx}/_count")) else { panic!("count") };
        v.to_json()["count"].as_u64().unwrap()
    };
    assert_eq!(count(&e), 2);
    let body = "{\"query\": {\"match_all\": {}}}";
    for text in [
        format!("POST /{idx}/_search/../_delete_by_query?refresh=true\n{body}"),
        format!("POST /{idx}/_search/%2e%2e/_delete_by_query?refresh=true\n{body}"),
        format!("POST /_sql/../{idx}/_delete_by_query?refresh=true\n{body}"),
        format!("POST /{idx}/_pit/../_delete_by_query?refresh=true\n{body}"),
        format!("POST /{idx}/%5Fdelete_by_query?refresh=true\n{body}"),
        format!("GET /{idx}/_search/../../{idx}"),
    ] {
        let ran = read_tool(&e, &c, "", &text);
        assert_eq!(count(&e), 2, "{text} ({ran}) changed the index");
    }
    let _ = run(&e, &c, "", &format!("DELETE /{idx}"));
}

#[test]
fn elasticsearch_paths() {
    search_paths(Engine::Elasticsearch);
}

#[test]
fn opensearch_paths() {
    search_paths(Engine::OpenSearch);
}
