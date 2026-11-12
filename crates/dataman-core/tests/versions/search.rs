//! See live_versions.rs. Elasticsearch and OpenSearch: aggregations come back with their values,
//! paging stops where the server's window does and never claims a total it was not told, a
//! mapping's every field (multi-fields, runtime ones) is listed, and SQL answers on each release.

use super::*;
use dataman_core::nosql::Body;
use serde_json::{Value, json};

/// The server on an index of the test's own (the tests run at once), deleted first. An http URL
/// reads as Elasticsearch: an OpenSearch server is connected to again as OpenSearch, as its sheet
/// would.
fn search(index: &str) -> Option<(Target, bool)> {
    let t = target(Engine::Elasticsearch)?;
    let os = t.c.info.version.starts_with("OpenSearch");
    let t = if os {
        let p = dataman_core::ConnectionProfile { engine: Engine::OpenSearch, ..t.c.profile.clone() };
        let c = t.e.core.block_on(t.e.core.connect(p, None)).expect("OpenSearch, as OpenSearch");
        Target { e: t.e, c }
    } else {
        t
    };
    if let Err(e) = t.nosql("", &format!("DELETE /{index}")) {
        assert!(e.contains("index_not_found_exception"), "{e}");
    }
    Some((t, os))
}

fn value(o: Output) -> Value {
    match o {
        Output::Value(Body::Json(v)) => v,
        o => panic!("{o:?}"),
    }
}

fn bulk(t: &Target, index: &str, docs: impl Iterator<Item = Value>) {
    let mut text = format!("POST /{index}/_bulk?refresh=true\n");
    for d in docs {
        text.push_str(&format!("{{\"index\": {{}}}}\n{d}\n"));
    }
    let Output::Written { summary, .. } = t.nosql_ok("", &text) else { panic!() };
    assert!(summary.ends_with("bulk actions done"), "{summary}");
}

#[test]
fn aggregations_come_back_with_their_values() {
    const IX: &str = "dataman_v_aggs";
    let Some((t, _)) = search(IX) else { return };
    t.nosql_ok("", &format!("PUT /{IX}\n{}", json!({"mappings": {"properties": {"g": {"type": "keyword"}, "v": {"type": "integer"}, "at": {"type": "date"}, "items": {"type": "nested", "properties": {"n": {"type": "integer"}}}}}})));
    let rows = [("a", 1, "2024-01-01", [1, 2]), ("a", 5, "2024-01-03", [3, 4]), ("b", 3, "2024-01-03", [5, 6]), ("b", 10, "2024-01-07", [7, 8])];
    bulk(&t, IX, rows.iter().map(|(g, v, at, n)| json!({"g": g, "v": v, "at": at, "items": [{"n": n[0]}, {"n": n[1]}]})));
    let items = t.e.core.block_on(t.c.nosql_tree(None)).unwrap();
    assert_eq!(items.iter().find(|i| i.name == IX).map(|i| i.count), Some(Some(4)), "4 documents, whatever their nested objects");
    let aggs = |a: Value| value(t.nosql_ok("", &format!("GET /{IX}/_search\n{}", json!({"size": 0, "aggs": a}))))["aggregations"].clone();
    let a = aggs(json!({
        "g": {"terms": {"field": "g"}, "aggs": {"top": {"top_hits": {"size": 1, "sort": [{"v": "desc"}], "_source": ["v"]}}}},
        "days": {"date_histogram": {"field": "at", "calendar_interval": "day"}, "aggs": {"s": {"sum": {"field": "v"}}, "run": {"cumulative_sum": {"buckets_path": "s"}}, "twice": {"bucket_script": {"buckets_path": {"x": "s"}, "script": "params.x * 2"}}}},
        "items": {"nested": {"path": "items"}, "aggs": {"n": {"sum": {"field": "items.n"}}}},
        "p": {"percentiles": {"field": "v", "percents": [0, 100]}},
        "c": {"composite": {"size": 1, "sources": [{"g": {"terms": {"field": "g"}}}]}},
        "m": {"scripted_metric": {"init_script": "state.n = 0", "map_script": "state.n += doc['v'].value", "combine_script": "return state.n", "reduce_script": "double t = 0; for (s in states) { t += s } return t"}},
    }));
    let terms: Vec<(Value, Value, Value)> = a["g"]["buckets"].as_array().unwrap().iter().map(|b| (b["key"].clone(), b["doc_count"].clone(), b["top"]["hits"]["hits"][0]["_source"]["v"].clone())).collect();
    assert_eq!(terms, [(json!("a"), json!(2), json!(5)), (json!("b"), json!(2), json!(10))]);
    let days: Vec<(Value, Value, Value, Value)> = a["days"]["buckets"].as_array().unwrap().iter().filter(|b| b["doc_count"] != json!(0)).map(|b| (b["key_as_string"].clone(), b["s"]["value"].clone(), b["run"]["value"].clone(), b["twice"]["value"].clone())).collect();
    assert_eq!(days, [
        (json!("2024-01-01T00:00:00.000Z"), json!(1.0), json!(1.0), json!(2.0)),
        (json!("2024-01-03T00:00:00.000Z"), json!(8.0), json!(9.0), json!(16.0)),
        (json!("2024-01-07T00:00:00.000Z"), json!(10.0), json!(19.0), json!(20.0)),
    ]);
    assert_eq!(a["items"]["n"]["value"], json!(36.0));
    assert_eq!(a["p"]["values"], json!({"0.0": 1.0, "100.0": 10.0}));
    assert_eq!(a["c"]["after_key"], json!({"g": "a"}));
    assert_eq!(a["m"]["value"], json!(19.0));
    // `interval` on a date_histogram: deprecated in 7.2, gone in 8. Gone, the server says which
    // field it does not take.
    let old = t.nosql("", &format!("GET /{IX}/_search\n{}", json!({"size": 0, "aggs": {"d": {"date_histogram": {"field": "at", "interval": "1d"}}}})));
    match old {
        Ok(o) => assert_eq!(value(o)["aggregations"]["d"]["buckets"].as_array().map(Vec::len), Some(7)),
        Err(e) => assert!(e.contains("interval"), "{e}"),
    }
}

/// Past 10,000 hits the server counts no further (`relation: gte`): the total is not shown as if
/// it were exact, and the page past the window fails in the server's words.
#[test]
fn paging_past_the_window() {
    const IX: &str = "dataman_v_deep";
    let Some((t, _)) = search(IX) else { return };
    t.nosql_ok("", &format!("PUT /{IX}\n{}", json!({"settings": {"number_of_shards": 1}, "mappings": {"properties": {"n": {"type": "integer"}}}})));
    for chunk in 0..3 {
        bulk(&t, IX, (0..3500).map(|i| json!({"n": chunk * 3500 + i})));
    }
    let q = format!("GET /{IX}/_search\n{}", json!({"query": {"match_all": {}}, "sort": [{"n": "asc"}]}));
    let page = |skip| t.nosql_page("", &q, Page { skip, limit: 50 });
    let Output::Docs(first) = page(0).unwrap().output else { panic!() };
    assert_eq!((first.docs.len(), first.total, first.more), (50, None, true), "10,500 hits counted to 10,000 are not 10,000");
    let Output::Docs(last) = page(9950).unwrap().output else { panic!() };
    assert_eq!((last.docs.len(), last.more), (50, true));
    assert!(last.docs[49].body.text().contains("\"n\": 9999"), "{}", last.docs[49].body.text());
    let e = page(10_000).unwrap_err();
    assert!(e.contains("Result window is too large"), "{e}");
    let Output::Docs(exact) = t.nosql_ok("", &format!("GET /{IX}/_search\n{}", json!({"track_total_hits": true}))) else { panic!() };
    assert_eq!(exact.total, Some(10_500));
}

/// The fields a filter or completion offers: a multi-field (`name.raw`), a nested one, a runtime
/// one (7.11), as the mapping has them.
#[test]
fn every_field_of_a_mapping() {
    const IX: &str = "dataman_v_fields";
    let Some((t, os)) = search(IX) else { return };
    let mut props = json!({
        "name": {"type": "text", "fields": {"raw": {"type": "keyword"}}},
        "items": {"type": "nested", "properties": {"sku": {"type": "keyword"}}},
        "geo": {"type": "geo_point"},
        "nick": {"type": "alias", "path": "name"},
    });
    let mut want = vec![("_id", "keyword"), ("name", "text"), ("name.raw", "keyword"), ("items", "nested"), ("items.sku", "keyword"), ("geo", "geo_point"), ("nick", "alias")];
    let mut mapping = json!({"properties": {}});
    if !os {
        props["tags"] = json!({"type": "flattened"});
        props["vec"] = json!({"type": "dense_vector", "dims": 3});
        want.extend([("tags", "flattened"), ("vec", "dense_vector")]);
        if t.c.info.release >= (7, 11, 0) {
            mapping["runtime"] = json!({"day": {"type": "keyword", "script": {"source": "emit('x')"}}});
            want.push(("day", "keyword"));
        }
    }
    mapping["properties"] = props;
    t.nosql_ok("", &format!("PUT /{IX}\n{}", json!({"mappings": mapping})));
    let fields = t.e.core.block_on(t.c.sample_fields("", IX)).unwrap();
    let got: Vec<(&str, &str)> = fields.iter().map(|(f, ts)| (f.as_str(), ts[0])).collect();
    for w in &want {
        assert!(got.contains(w), "{w:?} not in {got:?}");
    }
    assert_eq!(got.len(), want.len(), "{got:?}");
}

#[test]
fn sql_answers() {
    const IX: &str = "dataman_v_sql";
    let Some((t, os)) = search(IX) else { return };
    bulk(&t, IX, [("a", 1), ("a", 5), ("b", 3)].iter().map(|(g, v)| json!({"g": g, "v": v})));
    let q = format!("SELECT g, SUM(v) AS s FROM {IX} GROUP BY g ORDER BY g");
    let text = if os { format!("POST _plugins/_sql\n{}", json!({"query": q})) } else { format!("POST _sql?format=json\n{}", json!({"query": q})) };
    assert_eq!(dataman_core::nosql::classify(t.c.engine(), &text), Ok(dataman_core::guard::WriteClass::Read));
    let v = value(t.nosql_ok("", &text));
    let rows = if os { &v["datarows"] } else { &v["rows"] };
    let rows: Vec<(Value, f64)> = rows.as_array().unwrap_or_else(|| panic!("{v}")).iter().map(|r| (r[0].clone(), r[1].as_f64().unwrap())).collect();
    assert_eq!(rows, [(json!("a"), 6.0), (json!("b"), 3.0)]);
}
