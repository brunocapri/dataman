//! Elasticsearch and OpenSearch over their REST APIs (reqwest, TLS on the process' rustls
//! provider). The two speak the same API for everything Dataman does: requests from the
//! console ([`crate::nosql::console`]), the indices for the explorer, a mapping's fields, and
//! documents saved and deleted by their `_index` and `_id`.
//!
//! Through an SSH tunnel the server's host name still names it (the TLS name, the `Host`
//! header): its address resolves to the tunnel's local end.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::{Value, json};

use crate::config::{ConnectionProfile, Engine, SslMode};
use crate::nosql::console::{self, Request};
use crate::nosql::{Body, Doc, Docs, Item, ItemKind, Output, Page, Source};
use crate::ssh::Tunnel;

pub(crate) struct Client {
    http: reqwest::Client,
    /// `https://host:9200` (no trailing `/`).
    base: String,
    auth: Option<HeaderValue>,
    pub tls_active: bool,
    /// `Elasticsearch 8.15.3`, `OpenSearch 2.19.1`.
    pub version: String,
}

/// The server's error: `type: reason (status)`.
#[derive(Debug)]
pub(crate) struct HttpError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl std::error::Error for HttpError {}

fn error_message(v: &Value, text: &str) -> String {
    let e = &v["error"];
    if let Some(s) = e.as_str() {
        return s.to_string();
    }
    let reason = e["root_cause"][0]["reason"].as_str().or(e["reason"].as_str());
    let kind = e["root_cause"][0]["type"].as_str().or(e["type"].as_str());
    match (kind, reason) {
        (Some(k), Some(r)) => format!("{k}: {r}"),
        (None, Some(r)) => r.to_string(),
        _ => text.chars().take(300).collect(),
    }
}

pub(crate) async fn connect(p: &ConnectionProfile, password: Option<&str>, tunnel: Option<&Tunnel>) -> Result<Client> {
    let tls = matches!(p.sslmode, SslMode::Require | SslMode::VerifyFull);
    let scheme = if tls { "https" } else { "http" };
    let host = if p.host.contains(':') && !p.host.starts_with('[') { format!("[{}]", p.host) } else { p.host.clone() };
    let mut b = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).user_agent("Dataman");
    if p.sslmode == SslMode::Require {
        b = b.danger_accept_invalid_certs(true);
    }
    let base = match tunnel {
        // A name: it resolves to the tunnel (the URL without a port takes the address' port).
        Some(t) if p.host.parse::<std::net::IpAddr>().is_err() => {
            b = b.resolve(&p.host, t.local_addr());
            format!("{scheme}://{host}")
        }
        Some(t) => format!("{scheme}://127.0.0.1:{}", t.local_addr().port()),
        None => format!("{scheme}://{host}:{}", p.port),
    };
    let auth = match (p.user.is_empty(), password.filter(|p| !p.is_empty())) {
        (false, pw) => {
            let raw = format!("{}:{}", p.user, pw.unwrap_or(""));
            Some(HeaderValue::from_str(&format!("Basic {}", data_encoding::BASE64.encode(raw.as_bytes())))?)
        }
        // No user, a secret: an API key.
        (true, Some(key)) => Some(HeaderValue::from_str(&format!("ApiKey {key}"))?),
        (true, None) => None,
    };
    let mut c = Client { http: b.build()?, base, auth, tls_active: tls, version: String::new() };
    let root = match c.call("GET", "/", None).await {
        Ok(v) => v,
        Err(e) => {
            if let Some(t) = tunnel
                && let Some(te) = t.take_error()
            {
                return Err(te.into());
            }
            return Err(e);
        }
    };
    let number = root["version"]["number"].as_str().unwrap_or("");
    let os = root["version"]["distribution"].as_str() == Some("opensearch");
    c.version = format!("{} {number}", if os { "OpenSearch" } else { "Elasticsearch" });
    if os != (p.engine == Engine::OpenSearch) {
        log::info!("{}: the server is {}", p.name, c.version);
    }
    Ok(c)
}

/// A request's body on the wire.
enum Wire {
    Json(Value),
    Lines(Vec<Value>),
}

impl Client {
    /// Sends a request; the answer as JSON (text that is not JSON as a string; nothing as
    /// null). A status of 400 or more is an error with the server's reason.
    async fn call(&self, method: &str, path: &str, body: Option<Wire>) -> Result<Value> {
        let m = reqwest::Method::from_bytes(method.as_bytes())?;
        let mut r = self.http.request(m, format!("{}{path}", self.base)).timeout(Duration::from_secs(120));
        if let Some(a) = &self.auth {
            r = r.header(AUTHORIZATION, a.clone());
        }
        r = match body {
            Some(Wire::Json(v)) => r.header(CONTENT_TYPE, "application/json").body(v.to_string()),
            Some(Wire::Lines(lines)) => {
                let mut s: String = lines.iter().map(|l| l.to_string() + "\n").collect();
                if s.is_empty() {
                    s.push('\n');
                }
                r.header(CONTENT_TYPE, "application/x-ndjson").body(s)
            }
            None => r,
        };
        let resp = r.send().await.with_context(|| format!("cannot reach {}", self.base))?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let v: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text.clone())) };
        if status >= 400 {
            if status == 401 {
                return Err(HttpError { status, message: "authentication failed: check the user and password (or API key)".into() }.into());
            }
            return Err(HttpError { status, message: error_message(&v, &text) }.into());
        }
        Ok(v)
    }
}

fn wire(b: &console::Body) -> Option<Wire> {
    match b {
        console::Body::None => None,
        console::Body::Json(v) => Some(Wire::Json(v.clone())),
        console::Body::Lines(l) => Some(Wire::Lines(l.clone())),
    }
}

// ---------------------------------------------------------------------------------------------
// The explorer

/// The indices (hidden ones, `.`-prefixed, last), with their counts and sizes.
pub(crate) async fn indices(c: &Client) -> Result<Vec<Item>> {
    let v = c.call("GET", "/_cat/indices?format=json&bytes=b&h=index,docs.count,store.size&s=index", None).await?;
    let num = |x: &Value| x.as_str().and_then(|s| s.parse::<u64>().ok()).or(x.as_u64());
    let mut items: Vec<Item> = v
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let name = r["index"].as_str()?.to_string();
                    Some(Item { name, kind: ItemKind::Collection, count: num(&r["docs.count"]), size: num(&r["store.size"]), note: None })
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| (a.name.starts_with('.'), &a.name).cmp(&(b.name.starts_with('.'), &b.name)));
    Ok(items)
}

/// An index's fields from its mapping (`a`, `a.b`), with their types.
pub(crate) async fn fields(c: &Client, index: &str) -> Result<Vec<(String, Vec<&'static str>)>> {
    let v = c.call("GET", &format!("/{}/_mapping", enc(index)), None).await?;
    let mut out: Vec<(String, Vec<&'static str>)> = Vec::new();
    fn walk(prefix: &str, props: &serde_json::Map<String, Value>, out: &mut Vec<(String, Vec<&'static str>)>) {
        for (k, p) in props {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            let t: &'static str = match p["type"].as_str() {
                Some(t) => known_type(t),
                None => "object",
            };
            if !out.iter().any(|(x, _)| *x == path) {
                out.push((path.clone(), vec![t]));
            }
            if let Some(sub) = p["properties"].as_object() {
                walk(&path, sub, out);
            }
        }
    }
    for (_, m) in v.as_object().into_iter().flatten() {
        if let Some(props) = m["mappings"]["properties"].as_object() {
            walk("", props, &mut out);
        }
    }
    out.insert(0, ("_id".into(), vec!["keyword"]));
    Ok(out)
}

/// The mapping's type names, as static strings (an unknown one is `other`).
fn known_type(t: &str) -> &'static str {
    const TYPES: &[&str] = &[
        "text", "keyword", "long", "integer", "short", "byte", "double", "float", "half_float", "scaled_float", "date", "date_nanos",
        "boolean", "binary", "object", "nested", "geo_point", "geo_shape", "ip", "completion", "dense_vector", "knn_vector",
        "flattened", "flat_object", "wildcard", "constant_keyword", "match_only_text", "search_as_you_type", "unsigned_long",
        "version", "alias",
    ];
    TYPES.iter().find(|x| **x == t).copied().unwrap_or("other")
}

/// A path segment percent-encoded (an index named with odd characters).
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' | b',' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Requests

/// Runs a console request: a search's hits from `page`, another request's answer, or what a
/// write changed.
pub(crate) async fn run(c: &Client, req: &Request, page: Page) -> Result<Output> {
    if req.is_search() {
        return search(c, req, page).await;
    }
    let v = c.call(&req.method, &req.path, wire(&req.body)).await?;
    if req.class() == crate::guard::WriteClass::Read || req.method == "GET" {
        return Ok(Output::Value(Body::Json(v)));
    }
    Ok(written(&v))
}

/// A write's answer: how many documents it changed and what the server said.
fn written(v: &Value) -> Output {
    let n = |k: &str| v[k].as_u64();
    let (affected, summary) = if let Some(r) = v["result"].as_str() {
        (u64::from(matches!(r, "created" | "updated" | "deleted")), format!("document {r}"))
    } else if let Some(d) = n("deleted").filter(|_| v.get("updated").is_none()) {
        (d, format!("{d} documents deleted"))
    } else if let Some(u) = n("updated") {
        (u, format!("{u} documents updated"))
    } else if let Some(items) = v["items"].as_array() {
        let ok = items.iter().filter(|i| i.as_object().and_then(|o| o.values().next()).is_some_and(|x| x["error"].is_null())).count() as u64;
        let failed = items.len() as u64 - ok;
        (ok, if failed > 0 { format!("{ok} done, {failed} failed") } else { format!("{ok} bulk actions done") })
    } else if v["acknowledged"].as_bool() == Some(true) {
        (0, "acknowledged".into())
    } else {
        (0, "done".into())
    };
    Output::Written { affected, summary, detail: Some(Body::Json(v.clone())) }
}

async fn search(c: &Client, req: &Request, page: Page) -> Result<Output> {
    let mut body = match &req.body {
        console::Body::Json(Value::Object(m)) => m.clone(),
        console::Body::None => serde_json::Map::new(),
        _ => return Err(anyhow!("a search's body is one JSON object")),
    };
    // Aggregations: the whole answer, its buckets first (they are the point), the rest after.
    if body.contains_key("aggs") || body.contains_key("aggregations") {
        let v = c.call(&req.method, &req.path, Some(Wire::Json(Value::Object(body)))).await?;
        return Ok(Output::Value(Body::Json(aggregations_first(v))));
    }
    let from = body.get("from").and_then(Value::as_u64).unwrap_or(0);
    let size = body.get("size").and_then(Value::as_u64);
    let left = size.map(|s| s.saturating_sub(page.skip));
    let want = left.map_or(page.limit as u64, |l| l.min(page.limit as u64));
    // `search_after` and a point in time page by their own values, not by `from`.
    let after = body.contains_key("search_after") || body.contains_key("pit");
    if after && page.skip > 0 {
        return Err(anyhow!("a search_after search pages by its sort values: put the last hit's sort in search_after"));
    }
    if !after {
        body.insert("from".into(), json!(from + page.skip));
    }
    body.insert("size".into(), json!(want));
    // Partial documents (`_source` filtered, `fields`) and hits with more than their source
    // (highlights, script fields) are shown, not saved back.
    let partial = ["_source", "fields", "stored_fields", "docvalue_fields", "script_fields", "highlight"].iter().any(|k| body.contains_key(*k));
    let v = c.call(if req.method == "GET" { "POST" } else { &req.method }, &req.path, Some(Wire::Json(Value::Object(body)))).await?;
    let hits = v["hits"]["hits"].as_array().cloned().unwrap_or_default();
    let total = v["hits"]["total"]["value"].as_u64().or(v["hits"]["total"].as_u64());
    let exact = v["hits"]["total"]["relation"].as_str() != Some("gte");
    let n = hits.len() as u64;
    let window_end = from + size.unwrap_or(u64::MAX);
    let more = !after
        && match (total, exact) {
            (Some(t), true) => from + page.skip + n < t.min(window_end),
            _ => n == want && from + page.skip + n < window_end,
        };
    let docs = hits
        .into_iter()
        .map(|h| {
            // The source, and what the hit adds beside it: `_highlight`, `_fields`, `_sort`.
            let mut body = h.get("_source").cloned().unwrap_or(json!({}));
            if let Value::Object(m) = &mut body {
                for (k, as_) in [("highlight", "_highlight"), ("fields", "_fields"), ("inner_hits", "_inner_hits")] {
                    if let Some(v) = h.get(k) {
                        m.insert(as_.into(), v.clone());
                    }
                }
                if after && let Some(v) = h.get("sort") {
                    m.insert("_sort".into(), v.clone());
                }
            }
            Doc { id: h["_id"].as_str().map(|s| Body::Json(Value::String(s.to_string()))), index: h["_index"].as_str().map(str::to_string), body: Body::Json(body) }
        })
        .collect();
    let source = (!partial).then(|| Source::Search { index: req.index().filter(|i| !i.contains(['*', ','])).unwrap_or("").to_string() });
    Ok(Output::Docs(Docs { docs, source, total: total.map(|t| t.min(window_end.saturating_sub(from))), skip: page.skip, more }))
}

fn aggregations_first(v: Value) -> Value {
    let Value::Object(mut m) = v else { return v };
    let mut out = serde_json::Map::new();
    if let Some(a) = m.remove("aggregations") {
        out.insert("aggregations".into(), a);
    }
    out.extend(m);
    Value::Object(out)
}

/// How many documents a write would change (counted before it runs).
pub(crate) async fn affects(c: &Client, req: &Request) -> Result<Option<u64>> {
    Ok(match (req.method.as_str(), req.endpoint()) {
        ("POST", Some("_delete_by_query" | "_update_by_query")) => {
            let q = match &req.body {
                console::Body::Json(b) => b.get("query").cloned().unwrap_or(json!({"match_all": {}})),
                _ => json!({"match_all": {}}),
            };
            let index = req.index().unwrap_or("_all");
            let v = c.call("POST", &format!("/{index}/_count"), Some(Wire::Json(json!({"query": q})))).await?;
            v["count"].as_u64()
        }
        (_, Some("_doc" | "_create" | "_update")) => Some(1),
        (_, Some("_bulk")) => match &req.body {
            console::Body::Lines(l) => Some(l.iter().filter(|x| ["index", "create", "update", "delete"].iter().any(|k| x.get(k).is_some())).count() as u64),
            _ => None,
        },
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------------
// Documents

/// Writes `doc` as the document `id` of `index` (`None`: a new one, its id the server's), and
/// waits until searches see it. The saved document's id.
pub(crate) async fn save(c: &Client, index: &str, id: Option<&str>, doc: Value) -> Result<String> {
    if !doc.is_object() {
        return Err(anyhow!("a document is a JSON object {{…}}"));
    }
    let v = match id {
        Some(id) => c.call("PUT", &format!("/{}/_doc/{}?refresh=wait_for", enc(index), enc(id)), Some(Wire::Json(doc))).await?,
        None => c.call("POST", &format!("/{}/_doc?refresh=wait_for", enc(index)), Some(Wire::Json(doc))).await?,
    };
    v["_id"].as_str().map(str::to_string).ok_or_else(|| anyhow!("the server did not say the document's id"))
}

pub(crate) async fn delete(c: &Client, index: &str, id: &str) -> Result<u64> {
    let v = c.call("DELETE", &format!("/{}/_doc/{}?refresh=wait_for", enc(index), enc(id)), None).await;
    match v {
        Ok(v) => Ok(u64::from(v["result"].as_str() == Some("deleted"))),
        Err(e) if e.downcast_ref::<HttpError>().is_some_and(|h| h.status == 404) => Ok(0),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_say_what_changed() {
        let s = |v: Value| match written(&v) {
            Output::Written { affected, summary, .. } => (affected, summary),
            _ => panic!(),
        };
        assert_eq!(s(json!({"result": "created", "_id": "1"})), (1, "document created".into()));
        assert_eq!(s(json!({"result": "noop"})), (0, "document noop".into()));
        assert_eq!(s(json!({"deleted": 7, "total": 7})), (7, "7 documents deleted".into()));
        assert_eq!(s(json!({"updated": 3, "deleted": 0})), (3, "3 documents updated".into()));
        assert_eq!(s(json!({"items": [{"index": {"status": 201}}, {"index": {"error": {"type": "x"}}}]})), (1, "1 done, 1 failed".into()));
        assert_eq!(enc("logs-2024.01"), "logs-2024.01");
        assert_eq!(enc("a b/c"), "a%20b%2Fc");
        assert_eq!(error_message(&json!({"error": {"root_cause": [{"type": "index_not_found_exception", "reason": "no such index [x]"}]}}), ""), "index_not_found_exception: no such index [x]");
    }
}
