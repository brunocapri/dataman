//! See live_versions.rs. Redis and the servers that speak its protocol: keys whatever their bytes
//! open from the tree as they are, every type reads as redis-cli shows it, a script keeps to its
//! database, and the agent login is offered from Redis 7 only, saying why before it.

use super::*;
use dataman_core::agent_login::live;
use dataman_core::nosql::Body;
use dataman_core::server::Flavor;
use serde_json::{Value, json};

/// The server, its database (Garnet has only 0, so every test has keys of its own instead), with
/// none of the test's keys left.
fn redis(prefix: &str) -> Option<(Target, String)> {
    let t = target(Engine::Redis)?;
    let db = t.c.database().to_string();
    if let Output::Value(Body::Json(Value::Array(keys))) = t.nosql_ok(&db, &format!("KEYS *{prefix}*")) {
        for k in keys {
            let k = k.as_str().expect("a key").to_string();
            t.nosql_ok(&db, &format!("DEL {}", dataman_core::nosql::redis::quote(&k)));
        }
    }
    Some((t, db))
}

fn value(o: Output) -> Value {
    match o {
        Output::Value(Body::Json(v)) => v,
        Output::Docs(d) => Value::Array(d.docs.into_iter().map(|d| d.body.to_json()).collect()),
        o => panic!("{o:?}"),
    }
}

/// What the explorer lists for these keys, and what opening each one shows.
#[test]
fn keys_of_any_bytes_open_from_the_tree() {
    let Some((t, db)) = redis("dvbin") else { return };
    for cmd in [r#"SET "dvbin\xffkey" one"#, r#"SET "dvbin\xfekey" two"#, r#"SET "\"dvbin\" quoted" three"#, "SET dvbin:plain four", r#"HSET "dvbin\th" f "\xc0v""#] {
        t.nosql_ok(&db, cmd);
    }
    let items = t.e.core.block_on(t.c.nosql_tree(Some(&db))).unwrap();
    let mut mine: Vec<(String, Option<String>)> = items.iter().filter(|i| i.name.contains("dvbin")).map(|i| (i.name.clone(), i.note.clone())).collect();
    mine.sort();
    assert_eq!(mine.len(), 5, "two keys whose bytes differ are two keys: {mine:?}");
    let mut seen = Vec::new();
    for (name, kind) in &mine {
        assert!(matches!(kind.as_deref(), Some("string" | "hash")), "{name}: {kind:?}");
        let open = dataman_core::nosql::open_statement_for(&t.c, &db, name);
        seen.push((name.clone(), value(t.nosql(&db, &open).unwrap_or_else(|e| panic!("{open}: {e}")))));
    }
    assert_eq!(
        seen,
        [
            (r#""\"dvbin\" quoted""#.to_string(), json!([{"value": "three"}])),
            (r#""dvbin\xfekey""#.into(), json!([{"value": "two"}])),
            (r#""dvbin\xffkey""#.into(), json!([{"value": "one"}])),
            ("dvbin\th".into(), json!([{"field": "f", "value": r"\xc0v"}])),
            ("dvbin:plain".into(), json!([{"value": "four"}])),
        ]
    );
    // Typed as redis-cli takes it, the key is the same one.
    assert_eq!(value(t.nosql_ok(&db, r#"GET "dvbin\xffkey""#)), json!([{"value": "one"}]));
    let Output::Docs(d) = t.nosql_ok(&db, r#"GET "dvbin\xffkey""#) else { panic!() };
    assert!(d.source.is_none(), "a key that is not text is shown, not edited by name");
}

#[test]
fn values_read_as_redis_cli_shows_them() {
    let Some((t, db)) = redis("dvval") else { return };
    let v = |cmd: &str| value(t.nosql(&db, cmd).unwrap_or_else(|e| panic!("{cmd}: {e}")));
    t.nosql_ok(&db, "SET dvval:n 9223372036854775806");
    assert_eq!(t.nosql_ok(&db, "INCR dvval:n"), Output::Written { affected: 9223372036854775807, summary: "(integer) 9223372036854775807".into(), detail: None });
    assert_eq!(v("GET dvval:n"), json!([{"value": "9223372036854775807"}]));
    let e = t.nosql(&db, "INCR dvval:n").unwrap_err();
    assert!(e.contains("overflow") || e.contains("out of range"), "{e}");
    t.nosql_ok(&db, "SETBIT dvval:bits 7 1");
    // A control character is still text (JSON escapes it); only bytes that are not UTF-8 show as \x…
    assert_eq!(v("GET dvval:bits"), json!([{"value": "\u{1}"}]));
    t.nosql_ok(&db, "SETBIT dvval:bits 0 1");
    assert_eq!(v("GET dvval:bits"), json!([{"value": r"\x81"}]));
    t.nosql_ok(&db, "PFADD dvval:hll a b c");
    assert_eq!(v("PFCOUNT dvval:hll"), json!(3));
    if t.c.info.flavor != Flavor::Garnet {
        t.nosql_ok(&db, "XADD dvval:s 1-1 a 1 b \"x y\"");
        assert_eq!(v("XRANGE dvval:s - +"), json!([["1-1", ["a", "1", "b", "x y"]]]));
    }
    let big = format!("dvval:{}", "k".repeat(100_000));
    t.nosql_ok(&db, &format!("SET {big} v"));
    assert_eq!(v(&format!("GET {big}")), json!([{"value": "v"}]));
    let items = t.e.core.block_on(t.c.nosql_tree(Some(&db))).unwrap();
    assert!(items.iter().any(|i| i.name == big), "a 100 kB key name is listed whole");
}

/// A script's own replies, nested and of each RESP type, as Redis sends them.
#[test]
fn scripts_answer_in_every_reply_type() {
    let Some((t, db)) = redis("dvlua") else { return };
    let v = |cmd: &str| t.nosql(&db, cmd);
    if t.c.info.flavor == Flavor::Garnet {
        return;
    }
    // EVAL is no read, and what it answers is shown in full, not as "OK".
    let all = Output::Written { affected: 1, summary: "4 replies".into(), detail: Some(Body::Json(json!([1, "two", [3, null], "OK"]))) };
    assert_eq!(v(r#"EVAL "return {1, 'two', {3, false}, redis.status_reply('OK')}" 0"#).unwrap(), all);
    t.nosql_ok(&db, "RPUSH dvlua:l a b c");
    // LPOP's count is 6.2's.
    if t.c.info.release >= (6, 2, 0) {
        assert_eq!(v("LPOP dvlua:l 2").unwrap(), Output::Written { affected: 1, summary: "2 replies".into(), detail: Some(Body::Json(json!(["a", "b"]))) });
    } else {
        t.nosql_ok(&db, "LTRIM dvlua:l 2 -1");
    }
    assert_eq!(v("LPOP dvlua:l").unwrap(), Output::Written { affected: 1, summary: "c".into(), detail: None });
    assert!(v(r#"EVAL "return redis.error_reply('my bad')" 0"#).unwrap_err().contains("my bad"));
}

/// A script that SELECTs another database: before Redis 7 the connection stays there, and what
/// runs next must still go to the database it is run on.
#[test]
fn a_script_keeps_to_its_database() {
    let Some((t, db)) = redis("dvsel") else { return };
    if matches!(t.c.info.flavor, Flavor::Garnet | Flavor::Dragonfly) || db != "9" {
        return;
    }
    t.nosql_ok("8", "DEL dvsel:x");
    t.nosql_ok(&db, r#"EVAL "redis.call('SELECT', 8); return 1" 0"#);
    t.nosql_ok(&db, "SET dvsel:x here");
    assert_eq!(value(t.nosql_ok(&db, "EXISTS dvsel:x")), json!(1), "written on {db}");
    assert_eq!(value(t.nosql_ok("8", "EXISTS dvsel:x")), json!(0), "not on 8");
    t.nosql_ok(&db, "DEL dvsel:x");
}

/// Redis 7's selectors mark Dataman's users: before it, the agent login says so before anything
/// runs; a flavor without the ACL it needs says so too.
#[test]
fn the_agent_login_needs_redis_7() {
    let Some((t, _)) = redis("dvacl") else { return };
    let p = t.c.profile.clone();
    let probe = t.e.core.block_on(live::probe(&t.c, &p));
    if let Some(why) = t.c.info.flavor.spec().and_then(|f| f.no_agent_login) {
        let e = format!("{:#}", probe.expect_err("no probe"));
        assert!(e.contains(why), "{e}");
        return;
    }
    let (probe, _) = probe.unwrap();
    let lacks = probe.lacks.iter().any(|l| l.contains("Redis 7 or later"));
    assert_eq!((lacks, probe.can_create), (t.c.info.major() < 7, t.c.info.major() >= 7), "{}: {probe:?}", t.c.info.version);
}

/// Redis 8's modules: each type opens from the tree with a read of its own, classed as a read.
#[test]
fn module_types_open_from_the_tree() {
    let Some((t, db)) = redis("dvmod") else { return };
    if !(t.c.info.flavor.is_standard() && t.c.info.major() >= 8) {
        return;
    }
    t.nosql_ok(&db, r#"JSON.SET dvmod:j $ "{\"a\": [1, 2, {\"b\": null}]}""#);
    t.nosql_ok(&db, "TS.ADD dvmod:ts 1000 1.5");
    t.nosql_ok(&db, "BF.ADD dvmod:bf x");
    let items = t.e.core.block_on(t.c.nosql_tree(Some(&db))).unwrap();
    for (key, kind, want) in [("dvmod:j", "ReJSON-RL", json!(r#"{"a":[1,2,{"b":null}]}"#)), ("dvmod:ts", "TSDB-TYPE", json!([[1000, "1.5"]]))] {
        let i = items.iter().find(|i| i.name == key).unwrap_or_else(|| panic!("{key} not listed"));
        assert_eq!(i.note.as_deref(), Some(kind));
        let open = dataman_core::nosql::open_statement_for(&t.c, &db, key);
        assert_eq!(dataman_core::nosql::classify(Engine::Redis, &open), Ok(dataman_core::guard::WriteClass::Read), "{open}");
        assert_eq!(value(t.nosql(&db, &open).unwrap_or_else(|e| panic!("{open}: {e}"))), want, "{open}");
    }
    let bf = dataman_core::nosql::open_statement_for(&t.c, &db, "dvmod:bf");
    assert_eq!(dataman_core::nosql::classify(Engine::Redis, &bf), Ok(dataman_core::guard::WriteClass::Read), "{bf}");
    t.nosql(&db, &bf).unwrap_or_else(|e| panic!("{bf}: {e}"));
}
