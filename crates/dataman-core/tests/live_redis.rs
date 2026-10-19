//! Redis against a real server: connecting (AUTH), the databases and their keys with types,
//! commands and their replies, a key's value as entries (each type), paged, edited, added and
//! deleted; `SELECT`; read-only connections; refused commands. Skips without a server:
//!
//! - `DATAMAN_REDIS` (default `127.0.0.1:63790`, `DATAMAN_REDIS_PASSWORD` default `dataman`:
//!   `docker run -d --name dataman-redis -p 63790:6379 redis:7 --requirepass dataman`).
//!
//! Every test works in database 9 and empties it.

mod common;

use common::*;
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::nosql::{Body, Failed, ItemKind, Output, Page, Ran, Source};
use serde_json::json;

const DB: &str = "9";

fn profile() -> ConnectionProfile {
    let v = std::env::var("DATAMAN_REDIS").unwrap_or_else(|_| "127.0.0.1:63790".into());
    let (host, port) = v.rsplit_once(':').unwrap();
    ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: String::new(), database: DB.into(), sslmode: SslMode::Disable, engine: Engine::Redis, ..ConnectionProfile::new("redis") }
}

fn password() -> String {
    std::env::var("DATAMAN_REDIS_PASSWORD").unwrap_or_else(|_| "dataman".into())
}

fn connect(e: &Env, p: ConnectionProfile) -> Option<Arc<Connection>> {
    match e.core.block_on(e.core.connect(p.clone(), Some(password()))) {
        Ok(c) => Some(c),
        Err(err) => {
            eprintln!("skipping: cannot reach {}:{} ({err:#})", p.host, p.port);
            None
        }
    }
}

fn run(e: &Env, c: &Arc<Connection>, text: &str, page: Page) -> Result<Ran, Failed> {
    let r = c.run_nosql(DB, text, page);
    match e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), r).await.expect("timeout") }) {
        Ok(ran) => Ok(ran),
        Err(err) => Err(err.downcast_ref::<Failed>().cloned().unwrap_or_else(|| Failed { message: format!("{err:#}"), at: None })),
    }
}

fn ok(e: &Env, c: &Arc<Connection>, text: &str) -> Output {
    run(e, c, text, Page::default()).unwrap_or_else(|f| panic!("{text}: {f:?}")).output
}

fn docs(o: Output) -> dataman_core::nosql::Docs {
    match o {
        Output::Docs(d) => d,
        o => panic!("{o:?}"),
    }
}

#[test]
fn keys_values_and_entries() {
    let e = env();
    let Some(c) = connect(&e, profile()) else { return };
    assert!(c.info.version.starts_with("Redis ") || c.info.version.starts_with("Valkey "), "{}", c.info.version);
    assert_eq!(c.database(), DB);
    ok(&e, &c, "FLUSHDB");

    // Commands and their replies.
    assert_eq!(ok(&e, &c, "SET greeting \"hello world\""), Output::Written { affected: 1, summary: "OK".into(), detail: None });
    assert_eq!(ok(&e, &c, "GET nothing_here"), Output::Value(Body::Json(serde_json::Value::Null)));
    let mut fields = String::from("HSET user:1");
    for i in 0..70 {
        fields.push_str(&format!(" f{i:02} v{i}"));
    }
    let Output::Written { affected, .. } = ok(&e, &c, &fields) else { panic!() };
    assert_eq!(affected, 70);
    ok(&e, &c, "RPUSH queue a b c d");
    ok(&e, &c, "SADD tags red green blue");
    ok(&e, &c, "ZADD board 10 ada 20 grace 5 alan");
    assert_eq!(ok(&e, &c, "LLEN queue"), Output::Value(Body::Json(json!(4))));
    assert_eq!(ok(&e, &c, "SELECT 3"), Output::Use("3".into()));

    // The explorer: database 9 with its key count, its keys with their types.
    let dbs = e.core.block_on(c.nosql_tree(None)).unwrap();
    assert!(dbs.iter().any(|d| d.name == DB && d.kind == ItemKind::Database && d.count == Some(5)), "{dbs:?}");
    let keys = e.core.block_on(c.nosql_tree(Some(DB))).unwrap();
    let names: Vec<(&str, Option<&str>)> = keys.iter().map(|k| (k.name.as_str(), k.note.as_deref())).collect();
    assert_eq!(names, [("board", Some("zset")), ("greeting", Some("string")), ("queue", Some("list")), ("tags", Some("set")), ("user:1", Some("hash"))]);
    assert_eq!(dataman_core::nosql::open_statement_for(&c, DB, "board"), "ZRANGE board 0 -1 WITHSCORES");

    // A hash as entries, paged; one edited (renamed), one added, one deleted.
    let d = docs(run(&e, &c, "HGETALL user:1", Page { skip: 0, limit: 50 }).unwrap().output);
    assert_eq!((d.docs.len(), d.total, d.more), (50, Some(70), true));
    let src = d.source.clone().unwrap();
    assert_eq!(src, Source::Redis { db: DB.into(), key: "user:1".into(), kind: "hash".into() });
    let d2 = docs(run(&e, &c, "HGETALL user:1", Page { skip: 50, limit: 50 }).unwrap().output);
    assert_eq!((d2.docs.len(), d2.more), (20, false));
    let first = d.docs[0].clone();
    let Body::Json(f) = first.body.clone() else { panic!() };
    let old = f["field"].as_str().unwrap().to_string();
    let id = e.core.block_on(c.save_document(&src, first.id.clone(), &format!("{{\"field\": \"renamed\", \"value\": \"{}\"}}", f["value"].as_str().unwrap()))).unwrap();
    assert_eq!(id, Body::Json(json!("renamed")));
    assert_eq!(ok(&e, &c, &format!("HEXISTS user:1 {old}")), Output::Value(Body::Json(json!(0))));
    e.core.block_on(c.save_document(&src, None, "{field: 'new', value: 42}")).unwrap();
    assert_eq!(ok(&e, &c, "HGET user:1 new"), Output::Value(Body::Json(json!("42"))));
    assert_eq!(e.core.block_on(c.delete_document(&src, Body::Json(json!("new")))).unwrap(), 1);
    assert_eq!(ok(&e, &c, "HLEN user:1"), Output::Value(Body::Json(json!(70))));

    // A list by index; a sorted set by member and score; a set; a string.
    let d = docs(ok(&e, &c, "LRANGE queue 0 -1"));
    assert_eq!(d.docs[1].body, Body::Json(json!({"index": 1, "value": "b"})));
    let lsrc = d.source.clone().unwrap();
    e.core.block_on(c.save_document(&lsrc, d.docs[1].id.clone(), "{index: 1, value: 'B'}")).unwrap();
    e.core.block_on(c.delete_document(&lsrc, Body::Json(json!(0)))).unwrap();
    assert_eq!(ok(&e, &c, "LRANGE queue 0 9"), Output::Value(Body::Json(json!(["B", "c", "d"]))));
    let d = docs(ok(&e, &c, "ZRANGE board 0 -1 WITHSCORES"));
    assert_eq!(d.docs[0].body, Body::Json(json!({"member": "alan", "score": 5.0})));
    e.core.block_on(c.save_document(d.source.as_ref().unwrap(), d.docs[0].id.clone(), "{member: 'alan', score: 30}")).unwrap();
    assert_eq!(ok(&e, &c, "ZSCORE board alan"), Output::Value(Body::Json(json!("30"))));
    let d = docs(ok(&e, &c, "SMEMBERS tags"));
    assert_eq!((d.docs.len(), d.total), (3, Some(3)));
    let d = docs(ok(&e, &c, "GET greeting"));
    assert_eq!(d.docs[0].body, Body::Json(json!({"value": "hello world"})));
    e.core.block_on(c.save_document(d.source.as_ref().unwrap(), d.docs[0].id.clone(), "{value: 'bye'}")).unwrap();
    assert_eq!(ok(&e, &c, "STRLEN greeting"), Output::Value(Body::Json(json!(3))));

    // Errors: the server's, a type that is not the key's, a refused command, a quote not closed.
    assert!(run(&e, &c, "LPUSH greeting x", Page::default()).unwrap_err().message.contains("WRONGTYPE"));
    assert!(run(&e, &c, "HGETALL queue", Page::default()).unwrap_err().message.contains("is a list"));
    assert!(run(&e, &c, "SUBSCRIBE news", Page::default()).unwrap_err().message.contains("not supported"));
    assert!(run(&e, &c, "GET \"open", Page::default()).unwrap_err().at.is_some());
    // MCP's count before a write.
    assert_eq!(e.core.block_on(c.nosql_affects(DB, "DEL a b c")).unwrap(), Some(3));

    // Read-only: writes refused, reads run.
    let mut p = profile();
    p.read_only = true;
    let ro = connect(&e, p).unwrap();
    assert!(run(&e, &ro, "SET x 1", Page::default()).unwrap_err().message.contains("read-only"));
    assert!(run(&e, &ro, "GET greeting", Page::default()).is_ok());

    // A wrong password says so.
    let err = e.core.block_on(e.core.connect(profile(), Some("wrong".into()))).err().expect("refused");
    assert!(format!("{err:#}").contains("authentication failed"), "{err:#}");
    ok(&e, &c, "FLUSHDB");
}
