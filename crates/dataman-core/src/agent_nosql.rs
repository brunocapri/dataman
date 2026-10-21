//! MongoDB, Elasticsearch/OpenSearch and Redis statements as agents may run them: hidden
//! collections, indices and keys are refused, hidden fields are refused when named and removed
//! from what comes back, sensitive fields are masked in Masked data, and what Agents may do is
//! held. Fail closed: what cannot be checked (scripts, cross-index searches) is refused while a
//! connection keeps something from agents.

use regex::Regex;
use mongodb::bson::Bson;
use serde_json::Value;
use std::sync::OnceLock;

use crate::Engine;
use crate::nosql::{console, mql, redis};
use crate::privacy::{AgentRules, AgentView, HashKey, Mask, Names, RedisCat, glob, mask_value, sensitive_match};

/// What to do to what comes back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shaped {
    /// Field names (globs) removed at any depth.
    pub strip: Vec<String>,
    /// Field names (globs) masked at any depth, with their mask.
    pub mask: Vec<(String, Mask)>,
    /// The statement lists collections, indices or keys: hidden names are dropped.
    pub listing: bool,
    /// Every value of the reply is masked so: values that come back without their field's name
    /// (Redis `HGET`, `HMGET` of a masked field).
    pub whole: Option<Mask>,
}

pub struct Context<'a> {
    pub rules: &'a AgentRules,
    pub connection: &'a str,
    pub engine: Engine,
    pub database: &'a str,
    /// Elasticsearch/OpenSearch: some names in the request are aliases or data streams, so a
    /// name may stand for other indices: every field rule applies.
    pub wide: bool,
}

fn restricted(r: &AgentRules) -> bool {
    !r.visible.is_open() || (r.view == AgentView::Masked && r.sensitive.iter().any(|s| s.on))
}

/// Names in a statement's text: identifiers, dotted paths and quoted strings.
fn words(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#""((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)'|([A-Za-z_$][A-Za-z0-9_.$]*)"#).expect("words"));
    re.captures_iter(text).filter_map(|c| c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str().trim_start_matches('$').to_string())).collect()
}

fn names_field(words: &[String], pattern: &str) -> bool {
    words.iter().any(|w| glob(pattern, w) || w.split('.').any(|seg| glob(pattern, seg)) || w.starts_with(&format!("{pattern}.")))
}

/// A reply key that is the field or one of its parts (`card.keyword`, `customer.card`).
fn key_is(pattern: &str, key: &str) -> bool {
    glob(pattern, key) || (key.contains('.') && key.split('.').any(|seg| glob(pattern, seg)))
}

/// The names inside code or a query string: `doc['card'].value` and `card:42*` name `card`
/// (plain words searched for are not names).
fn split_names(s: &str, out: &mut Vec<String>) {
    if !s.contains(['[', ']', '\'', '"', '(', ')', ':', '=']) {
        return;
    }
    out.extend(s.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '*' | '?' | '-'))).filter(|x| !x.is_empty()).map(str::to_string));
}

pub fn check(text: &str, write: bool, cx: &Context) -> Result<Shaped, String> {
    match cx.engine {
        Engine::MongoDb => mongo(text, write, cx),
        Engine::Elasticsearch | Engine::OpenSearch => search(text, write, cx),
        Engine::Redis => redis_check(text, write, cx),
        _ => Ok(Shaped::default()),
    }
}

fn mongo(text: &str, write: bool, cx: &Context) -> Result<Shaped, String> {
    let (r, conn) = (cx.rules, cx.connection);
    let lower = text.to_ascii_lowercase();
    if crate::nosql::is_script(Engine::MongoDb, text) {
        if !r.code {
            return Err(format!("running code (scripts) is off for agents on {conn} (Dataman › Agents › Permissions)"));
        }
        if restricted(r) || r.view == AgentView::Structure {
            return Err(format!("scripts are not available to agents on {conn} while some data is kept from them; send one statement"));
        }
        if !r.structure && ["createindex", "dropindex", "drop(", "dropdatabase", "renamecollection", "createcollection", "$out", "$merge"].iter().any(|w| lower.contains(w)) {
            return Err(format!("changing structure is off for agents on {conn} (Dataman › Agents › Permissions)"));
        }
        return Ok(Shaped::default());
    }
    let parsed = mql::parse(text).map_err(|e| e.message)?;
    let db = parsed.db.as_deref().unwrap_or(cx.database);
    let op = &parsed.op;
    // The statement as the server reads it: `"\u0024ssn"` is `$ssn`, `"pay\u006dents"` is
    // `payments`. Every check reads the written text and the parsed one.
    let decoded = format!("{text}\n{op:?}");
    let lower = decoded.to_ascii_lowercase();
    let structural = matches!(
        op,
        mql::Op::CreateIndex { .. } | mql::Op::DropIndex { .. } | mql::Op::DropCollection { .. } | mql::Op::CreateCollection { .. } | mql::Op::RenameCollection { .. } | mql::Op::DropDatabase
    ) || lower.contains("$out")
        || lower.contains("$merge")
        || matches!(op, mql::Op::Command { doc, .. } if doc.keys().next().is_some_and(|k| ["createindexes", "dropindexes", "drop", "create", "renamecollection", "collmod", "dropdatabase"].contains(&k.to_ascii_lowercase().as_str())));
    if !r.structure && structural {
        return Err(format!("changing structure is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    if !r.code && ["$where", "$function", "$accumulator", "mapreduce", "$eval"].iter().any(|w| lower.contains(w)) {
        return Err(format!("running code ($where, $function, $accumulator, mapReduce) is off for agents on {conn}"));
    }
    let activity = lower.contains("currentop")
        || lower.contains("serverstatus")
        || lower.contains("system.profile")
        || lower.contains("\"top\"")
        || lower.contains("{top:")
        || matches!(op, mql::Op::Command { doc, .. } if doc.keys().next().is_some_and(|k| ["currentop", "serverstatus", "top", "profile", "getlog", "connpoolstats", "lockinfo"].contains(&k.to_ascii_lowercase().as_str())));
    if !r.activity_on() && activity {
        return Err(format!("server activity is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    if r.view == AgentView::Structure {
        let ok = if write { structural } else { matches!(op, mql::Op::ShowDbs | mql::Op::ShowCollections | mql::Op::Use(_) | mql::Op::Indexes { .. } | mql::Op::Stats { .. }) };
        if !ok {
            return Err(format!("{conn} shows agents its structure only: documents are neither read nor written"));
        }
    }
    let listing = matches!(op, mql::Op::ShowDbs | mql::Op::ShowCollections);
    if !restricted(r) {
        return Ok(Shaped { listing, ..Default::default() });
    }
    let names = Names::Qualified;
    if !r.visible.schema_visible(db, names) {
        return Err(format!("{db} is not visible to agents on {conn}"));
    }
    if let mql::Op::Command { .. } = op {
        return Err(format!("database commands are not available to agents on {conn} while some data is kept from them"));
    }
    let coll = op.collection();
    if let Some(c) = coll
        && !r.visible.table_visible(Some(db), c, names)
    {
        return Err(format!("{c} is not visible to agents on {conn}"));
    }
    // Other collections a pipeline reads or writes ($lookup, $graphLookup, $unionWith, $out,
    // $merge), as written and as parsed.
    let mut read: Vec<(String, String)> = Vec::new();
    static TARGETS: OnceLock<Regex> = OnceLock::new();
    let targets = TARGETS.get_or_init(|| Regex::new(r#"(?:\bfrom|\bcoll|\$unionWith|\$out|\binto|getCollection\(|getSiblingDB\()["']?\s*:?\s*["']([^"']+)["']"#).expect("targets"));
    for t in targets.captures_iter(text).filter_map(|c| c.get(1)) {
        read.push((db.to_string(), t.as_str().to_string()));
    }
    let mut docs = Vec::new();
    op_values(op, &mut docs);
    for d in &docs {
        pipeline_targets(d, db, &mut read);
    }
    for (tdb, t) in &read {
        if !r.visible.schema_visible(tdb, names) {
            return Err(format!("{tdb} is not visible to agents on {conn}"));
        }
        if !r.visible.table_visible(Some(tdb), t, names) {
            return Err(format!("{t} is not visible to agents on {conn}"));
        }
    }
    let w = words(&decoded);
    let Some(coll) = coll else {
        return Ok(Shaped { listing, ..Default::default() });
    };
    // What the rules keep of every collection read: fields hidden at any depth, and masked.
    let mut all: Vec<(String, String)> = vec![(db.to_string(), coll.to_string())];
    all.extend(read.iter().cloned());
    let mut hidden: Vec<String> = Vec::new();
    for (tdb, t) in &all {
        for h in r.visible.hidden_column_patterns(Some(tdb), t, names) {
            if !hidden.contains(&h) {
                hidden.push(h);
            }
        }
    }
    let mut sensitive: Vec<(String, Mask)> = Vec::new();
    if r.view == AgentView::Masked {
        for s in r.sensitive.iter().filter(|s| s.on) {
            for (tdb, t) in &all {
                let field = s.col.rsplit_once('.').map_or(s.col.as_str(), |(tp, f)| if sensitive_match(&s.col, Some(tdb), t, f, names) || glob(tp, t) { f } else { "" });
                if !field.is_empty() && !sensitive.iter().any(|(f, _)| f == field) {
                    sensitive.push((field.to_string(), s.how));
                }
            }
        }
    }
    let kept = !hidden.is_empty() || !sensitive.is_empty();
    if kept {
        // Field names computed at run time, and code that sees whole documents.
        for x in ["$getfield", "$setfield", "$unsetfield", "$where", "$function", "$accumulator"] {
            if lower.contains(x) {
                return Err(format!("{} is not available to agents on {conn} while fields of {coll} are kept from them", x.replace("field", "Field")));
            }
        }
    }
    if !hidden.is_empty() {
        if let Some(f) = hidden.iter().find(|f| names_field(&w, f)) {
            return Err(format!("{coll}.{f} is hidden from agents on {conn}"));
        }
        if ["$$root", "$$current", "$objecttoarray", "$replaceroot", "$replacewith"].iter().any(|x| lower.contains(x)) {
            return Err(format!("whole documents of {coll} are not available to agents on {conn}: some fields are hidden"));
        }
    }
    let mut mask = Vec::new();
    if !sensitive.is_empty() {
        // A find computes when its projection holds expressions (`{e: "$ssn"}`) or its filter
        // does ($expr): their values, and their errors, would carry the field.
        let projects = matches!(op, mql::Op::Find { projection: Some(p), .. } if p.values().any(|v| !matches!(v, Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Boolean(_))));
        let plain = matches!(op, mql::Op::Find { .. } | mql::Op::Count { .. }) && !projects && !lower.contains("$expr");
        for (field, how) in &sensitive {
            let computes = !plain || matches!(op, mql::Op::Find { sort: Some(d), .. } if d.keys().any(|k| glob(field, k)));
            if computes && names_field(&w, field) {
                return Err(format!("{coll}.{field} is masked for agents on {conn}: find and filter on it; it cannot be grouped, sorted or computed"));
            }
            mask.push((field.clone(), *how));
        }
        if ["$$root", "$$current", "$objecttoarray"].iter().any(|x| lower.contains(x)) {
            return Err(format!("whole documents of {coll} are not available to agents in Masked data on {conn}"));
        }
    }
    Ok(Shaped { strip: hidden, mask, listing, ..Default::default() })
}

/// The collections an aggregation stage names, at any depth.
fn pipeline_targets(v: &Bson, db: &str, out: &mut Vec<(String, String)>) {
    let named = |x: &Bson, key: &str, out: &mut Vec<(String, String)>| match x {
        Bson::String(c) => out.push((db.to_string(), c.clone())),
        Bson::Document(d) => {
            let tdb = d.get_str("db").unwrap_or(db).to_string();
            if let Ok(c) = d.get_str(key) {
                out.push((tdb, c.to_string()));
            } else if let Some(Bson::Document(inner)) = d.get(key)
                && let Ok(c) = inner.get_str("coll")
            {
                out.push((inner.get_str("db").unwrap_or(&tdb).to_string(), c.to_string()));
            }
        }
        _ => {}
    };
    match v {
        Bson::Document(d) => {
            for (k, x) in d {
                match k.as_str() {
                    "$lookup" | "$graphLookup" => named(x, "from", out),
                    "$unionWith" | "$out" => named(x, "coll", out),
                    "$merge" => named(x, "into", out),
                    _ => {}
                }
                pipeline_targets(x, db, out);
            }
        }
        Bson::Array(a) => a.iter().for_each(|x| pipeline_targets(x, db, out)),
        _ => {}
    }
}

fn op_values(op: &mql::Op, out: &mut Vec<Bson>) {
    use mql::Op;
    let d = |x: &mongodb::bson::Document| Bson::Document(x.clone());
    match op {
        Op::Find { filter, projection, sort, options, .. } => {
            out.extend([d(filter), d(options)]);
            out.extend(projection.iter().chain(sort.iter()).map(d));
        }
        Op::Aggregate { pipeline, .. } => out.extend(pipeline.iter().map(d)),
        Op::Count { filter, .. } => out.extend(filter.iter().map(d)),
        Op::Distinct { field, filter, .. } => out.extend([Bson::String(field.clone()), d(filter)]),
        Op::Insert { docs, .. } => out.extend(docs.iter().map(d)),
        Op::Update { filter, update, options, .. } => out.extend([d(filter), update.clone(), d(options)]),
        Op::Replace { filter, doc, options, .. } => out.extend([d(filter), d(doc), d(options)]),
        Op::Delete { filter, options, .. } => out.extend([d(filter), d(options)]),
        Op::FindAndModify { filter, update, options, .. } => {
            out.extend([d(filter), d(options)]);
            out.extend(update.iter().cloned());
        }
        Op::BulkWrite { ops, .. } => out.extend(ops.iter().map(d)),
        Op::CreateIndex { keys, options, .. } => out.extend([d(keys), d(options)]),
        Op::CreateCollection { options, .. } => out.push(d(options)),
        Op::Command { doc, .. } => out.push(d(doc)),
        _ => {}
    }
}

/// The collections a statement reads: its own and those its pipeline names.
pub fn mongo_reads(text: &str, database: &str) -> Vec<(String, String)> {
    let Ok(parsed) = mql::parse(text) else { return Vec::new() };
    let db = parsed.db.as_deref().unwrap_or(database);
    let mut out: Vec<(String, String)> = parsed.op.collection().map(|c| (db.to_string(), c.to_string())).into_iter().collect();
    let mut docs = Vec::new();
    op_values(&parsed.op, &mut docs);
    for d in &docs {
        pipeline_targets(d, db, &mut out);
    }
    out.dedup();
    out
}

/// Whether the rules keep anything of a collection (it, a field, or a masked field).
pub fn mongo_keeps(r: &AgentRules, db: &str, coll: &str) -> bool {
    let q = Names::Qualified;
    !r.visible.schema_visible(db, q)
        || !r.visible.table_visible(Some(db), coll, q)
        || !r.visible.hidden_column_patterns(Some(db), coll, q).is_empty()
        || (r.view == AgentView::Masked && r.sensitive.iter().any(|s| s.on && s.col.rsplit_once('.').is_none_or(|(t, f)| sensitive_match(&s.col, Some(db), coll, f, q) || glob(t, coll))))
}

fn path_of(req: &console::Request) -> (String, Vec<String>) {
    let p = req.path.split('?').next().unwrap_or("").trim_start_matches('/');
    let p = percent_decode(p);
    let segs: Vec<String> = p.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    (p, segs)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_names(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| json_names(x, out)),
        Value::Object(m) => {
            for (k, x) in m {
                out.push(k.clone());
                json_names(x, out);
            }
        }
        _ => {}
    }
}

fn has_key(v: &Value, keys: &[&str]) -> bool {
    match v {
        Value::Array(a) => a.iter().any(|x| has_key(x, keys)),
        Value::Object(m) => m.iter().any(|(k, x)| keys.contains(&k.as_str()) || has_key(x, keys)),
        _ => false,
    }
}

fn search(text: &str, write: bool, cx: &Context) -> Result<Shaped, String> {
    let (r, conn) = (cx.rules, cx.connection);
    let req = console::parse(text).map_err(|e| e.message)?;
    let (path, segs) = path_of(&req);
    let method = req.method.to_ascii_uppercase();
    let api = |s: &str| segs.iter().any(|x| x == s);
    let first = segs.first().map(String::as_str).unwrap_or("");
    let body: Vec<Value> = match &req.body {
        console::Body::None => Vec::new(),
        console::Body::Json(v) => vec![v.clone()],
        console::Body::Lines(l) => l.clone(),
    };
    let structural = (matches!(method.as_str(), "PUT" | "DELETE") && segs.len() == 1 && !first.starts_with('_'))
        || ["_mapping", "_mappings", "_settings", "_alias", "_aliases", "_template", "_index_template", "_component_template", "_rollover", "_shrink", "_split", "_clone", "_open", "_close"]
            .iter()
            .any(|x| api(x))
            && method != "GET";
    if !r.structure && structural {
        return Err(format!("changing structure (indices, mappings, settings) is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    if !r.code && (api("_scripts") || body.iter().any(|b| has_key(b, &["script", "scripted_metric", "runtime_mappings"]))) {
        return Err(format!("running code (scripts) is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    let activity = matches!(first, "_tasks" | "_nodes")
        || path.starts_with("_cluster/stats")
        || path.starts_with("_cluster/state")
        || path.starts_with("_cluster/pending_tasks")
        || ["_cat/tasks", "_cat/nodes", "_cat/thread_pool", "_cat/pending_tasks", "_cat/recovery"].iter().any(|p| path.starts_with(p));
    if !r.activity_on() && activity {
        return Err(format!("server activity is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    let listing = path.starts_with("_cat/indices") || path.starts_with("_cat/aliases") || first == "_aliases" || path.starts_with("_resolve/index");
    if r.view == AgentView::Structure {
        let ok = if write { structural } else { listing || api("_mapping") || api("_mappings") || api("_settings") || (method == "GET" && segs.len() == 1 && !first.starts_with('_')) };
        if !ok {
            return Err(format!("{conn} shows agents its structure only: documents are neither read nor written"));
        }
    }
    if !restricted(r) {
        return Ok(Shaped { listing, ..Default::default() });
    }
    let names = Names::Flat;
    if ["_sql", "_query", "_eql", "_esql"].iter().any(|x| api(x)) || path.starts_with("_plugins/_sql") || path.starts_with("_plugins/_ppl") || path.starts_with("_opendistro") {
        return Err(format!("SQL, ES|QL, PPL and EQL are not available to agents on {conn} while some data is kept from them"));
    }
    let mut indices: Vec<String> = if first.starts_with('_') { Vec::new() } else { first.split(',').map(str::to_string).collect() };
    let params: Vec<(String, String)> = req.path.split_once('?').map(|(_, q)| q.split('&').filter_map(|kv| kv.split_once('=').or(Some((kv, "")))).map(|(k, v)| (percent_decode(k), percent_decode(&v.replace('+', " ")))).collect()).unwrap_or_default();
    if indices.is_empty() && !listing && !matches!(first, "_cluster" | "_cat" | "_nodes" | "_tasks") {
        return Err(format!("name the index: some indices are kept from agents on {conn}"));
    }
    if first == "_cat" && !listing {
        return Err(format!("{path} is not available to agents on {conn} while some data is kept from them; list_tables lists the indices"));
    }
    for i in &indices {
        let wild = i.contains('*') || i == "_all" || i.starts_with('-');
        let ok = if wild {
            r.visible.mode == crate::privacy::VisibleMode::Only && r.visible.items.iter().any(|x| x.kind == crate::privacy::ItemKind::Table && x.name == *i)
                || r.visible.mode == crate::privacy::VisibleMode::Except && !r.visible.items.iter().any(|x| x.kind != crate::privacy::ItemKind::Column)
        } else {
            r.visible.table_visible(None, i, names)
        };
        if !ok {
            return Err(format!("{i} is not visible to agents on {conn}"));
        }
    }
    // _mget, _msearch and the like name more indices inside the body: every one counts.
    for b in &body {
        let mut named: Vec<&Value> = Vec::new();
        for k in ["_index", "index"] {
            named.extend(b.get(k));
            if let Some(Value::Array(docs)) = b.get("docs") {
                named.extend(docs.iter().filter_map(|d| d.get(k)));
            }
        }
        for v in named {
            let list: Vec<&str> = match v {
                Value::String(x) => x.split(',').collect(),
                Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                _ => return Err(format!("name the index plainly: some indices are kept from agents on {conn}")),
            };
            for i in list {
                if i.contains('*') || i == "_all" || !r.visible.table_visible(None, i, names) {
                    return Err(format!("{i} is not visible to agents on {conn}"));
                }
                if !indices.iter().any(|x| x == i) {
                    indices.push(i.to_string());
                }
            }
        }
    }
    let mut seen: Vec<String> = Vec::new();
    for b in &body {
        json_names(b, &mut seen);
    }
    for x in seen.clone() {
        split_names(&x, &mut seen);
    }
    for (k, v) in &params {
        seen.push(k.clone());
        split_names(v, &mut seen);
    }
    // A wildcard, an alias or a data stream may stand for any index: every field rule applies.
    let wide = cx.wide || indices.iter().any(|i| i.contains('*') || i == "_all" || i.starts_with('-'));
    let covers = |t: &str| wide || t == "*" || indices.iter().any(|i| glob(t, i));
    let mut strip = Vec::new();
    for i in &indices {
        strip.extend(r.visible.hidden_column_patterns(None, i, names));
    }
    if wide {
        for it in r.visible.items.iter().filter(|x| x.kind == crate::privacy::ItemKind::Column && r.visible.mode == crate::privacy::VisibleMode::Except) {
            let f = it.name.rsplit_once('.').map_or(it.name.as_str(), |(_, f)| f).to_string();
            if !strip.contains(&f) {
                strip.push(f);
            }
        }
    }
    let mut mask = Vec::new();
    if r.view == AgentView::Masked {
        for s in r.sensitive.iter().filter(|s| s.on) {
            let (t, field) = s.col.rsplit_once('.').unwrap_or(("*", s.col.as_str()));
            if covers(t) {
                mask.push((field.to_string(), s.how));
            }
        }
    }
    if !strip.is_empty() || !mask.is_empty() {
        // Code and templates read fields by names Dataman cannot follow; term vectors carry a
        // field's words as keys.
        let code = api("_scripts") || api("template") || api("_render") || api("_termvectors") || api("_mtermvectors") || body.iter().any(|b| has_key(b, &["script", "scripted_metric", "runtime_mappings", "script_fields", "script_score"]));
        if code {
            return Err(format!("scripts, templates and term vectors are not available to agents on {conn} while fields of {} are kept from them", indices.join(",")));
        }
    }
    if !strip.is_empty() {
        if let Some(f) = strip.iter().find(|f| names_field(&seen, f)) {
            return Err(format!("{f} is hidden from agents on {conn}"));
        }
        // Searches that reach fields by pattern or by default (`q=`, query_string, exists on
        // `car*`, multi_match over every field) would filter on hidden ones.
        let by_pattern = params.iter().any(|(k, _)| matches!(k.as_str(), "q" | "df"))
            || body.iter().any(|b| has_key(b, &["query_string", "simple_query_string"]))
            || body.iter().any(|b| wide_fields(b, false));
        if by_pattern {
            return Err(format!("searches over every field or fields by pattern are not available to agents on {conn} while some fields are hidden: name the fields"));
        }
    }
    if !mask.is_empty() {
        let mut computed = Vec::new();
        for b in &body {
            for k in ["aggs", "aggregations", "sort", "collapse", "script_fields", "runtime_mappings"] {
                if let Some(x) = b.get(k) {
                    json_names(x, &mut computed);
                }
            }
        }
        for (k, v) in &params {
            if k == "sort" {
                split_names(v, &mut computed);
            }
        }
        if let Some((field, _)) = mask.iter().find(|(f, _)| names_field(&computed, f)) {
            return Err(format!("{field} is masked for agents on {conn}: search and filter on it; it cannot be aggregated or sorted"));
        }
    }
    Ok(Shaped { strip, mask, listing, ..Default::default() })
}

/// A query part that names fields by pattern (`exists: {field: "car*"}`), or a multi-field query
/// with no fields (every field). The reply's own field lists (`_source`, `fields`, `highlight`…)
/// are not queries: what they bring back is stripped.
fn wide_fields(v: &Value, in_query: bool) -> bool {
    match v {
        Value::Object(m) => m.iter().any(|(k, x)| {
            let q = in_query || !matches!(k.as_str(), "_source" | "fields" | "docvalue_fields" | "stored_fields" | "highlight");
            let wild = |x: &Value| x.as_str().is_some_and(|f| f.contains('*') || f.contains('?'));
            match k.as_str() {
                "field" | "default_field" if q => wild(x),
                "fields" if q && in_query => x.as_array().is_some_and(|a| a.iter().any(wild)),
                "multi_match" | "combined_fields" | "more_like_this" if q => x.get("fields").is_none_or(|f| f.as_array().is_none_or(|a| a.iter().any(wild))),
                _ => wide_fields(x, q && (in_query || matches!(k.as_str(), "query" | "post_filter" | "aggs" | "aggregations" | "knn" | "rescore" | "suggest" | "sort" | "collapse"))),
            }
        }),
        Value::Array(a) => a.iter().any(|x| wide_fields(x, in_query)),
        _ => false,
    }
}

const MULTI_KEY: &[&str] = &["MGET", "DEL", "UNLINK", "EXISTS", "TOUCH", "SINTER", "SUNION", "SDIFF", "SINTERCARD", "PFCOUNT", "WATCH"];

fn redis_check(text: &str, write: bool, cx: &Context) -> Result<Shaped, String> {
    let (r, conn) = (cx.rules, cx.connection);
    let args = redis::parse(text).map_err(|e| e.message)?;
    let name = redis::name(&args);
    let cat = RedisCat::of(&name);
    if cat == RedisCat::Dangerous {
        return Err(format!("{name} never runs for agents"));
    }
    if !r.redis.allows(cat) {
        return Err(format!("{} commands are off for agents on {conn} (Dataman › Agents › Permissions)", cat.label()));
    }
    let first = name.split(' ').next().unwrap_or("");
    let activity = matches!(name.as_str(), "CLIENT LIST" | "CLIENT INFO" | "SLOWLOG GET" | "INFO" | "LATENCY LATEST" | "MEMORY STATS");
    if !r.activity_on() && activity {
        return Err(format!("server activity is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    if r.view == AgentView::Structure && (write || !matches!(first, "TYPE" | "TTL" | "PTTL" | "EXISTS" | "DBSIZE" | "SCAN" | "PING")) {
        return Err(format!("{conn} shows agents its structure only: values are neither read nor written"));
    }
    let listing = matches!(first, "SCAN" | "KEYS");
    if !restricted(r) {
        return Ok(Shaped { listing, ..Default::default() });
    }
    if matches!(first, "RANDOMKEY" | "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "FCALL" | "FCALL_RO" | "SORT" | "MIGRATE" | "DUMP") {
        return Err(format!("{name} is not available to agents on {conn} while some keys are kept from them"));
    }
    let names = Names::Flat;
    let keys = redis_keys(&name, &args);
    for k in &keys {
        if !r.visible.table_visible(None, k, names) {
            return Err(format!("{k} is not visible to agents on {conn}"));
        }
    }
    if r.visible.mode == crate::privacy::VisibleMode::Except {
        for a in args.iter().skip(1) {
            if r.visible.items.iter().any(|i| i.kind == crate::privacy::ItemKind::Table && glob(&i.name, a)) {
                return Err(format!("{a} is not visible to agents on {conn}"));
            }
        }
    }
    let mut strip = Vec::new();
    for k in &keys {
        strip.extend(r.visible.hidden_column_patterns(None, k, names));
    }
    if !strip.is_empty() {
        if matches!(first, "HVALS" | "HRANDFIELD" | "GET" | "JSON.GET") {
            return Err(format!("{name} would show fields hidden from agents on {conn}; read the fields by name (HMGET)"));
        }
        if let Some(f) = strip.iter().find(|f| args.iter().skip(2).any(|a| glob(f, a))) {
            return Err(format!("{f} is hidden from agents on {conn}"));
        }
    }
    let mut mask: Vec<(String, Mask)> = Vec::new();
    if r.view == AgentView::Masked {
        for s in r.sensitive.iter().filter(|s| s.on) {
            let (t, field) = s.col.rsplit_once('.').unwrap_or(("*", s.col.as_str()));
            if t == "*" || keys.iter().any(|k| glob(t, k)) {
                mask.push((field.to_string(), s.how));
            }
        }
    }
    let mut whole = None;
    if !mask.is_empty() {
        if matches!(first, "HVALS" | "HRANDFIELD" | "GET" | "JSON.GET" | "JSON.MGET" | "GETRANGE" | "GETDEL" | "GETEX" | "LCS") {
            return Err(format!("{name} would show masked fields on {conn}; read the hash with HGETALL, or the fields by name"));
        }
        // HGET and HMGET answer values without their fields' names.
        if matches!(first, "HGET" | "HMGET" | "HGETDEL" | "HGETEX") {
            for (f, how) in &mask {
                if args.iter().skip(2).any(|a| glob(f, a)) {
                    whole = Some(whole.map_or(*how, |w| crate::agent_sql::stronger(w, *how)));
                }
            }
        }
    }
    Ok(Shaped { strip, mask, listing, whole })
}

/// The keys of a Redis command, after its key positions in `COMMAND INFO`. A command Dataman
/// does not list reads its first argument as the key (a wrong guess refuses, in Only mode).
fn redis_keys<'a>(name: &str, args: &'a [String]) -> Vec<&'a String> {
    let first = name.split(' ').next().unwrap_or("");
    let numkeys = |at: usize| -> Vec<&'a String> {
        let n = args.get(at).and_then(|n| n.parse::<usize>().ok()).unwrap_or(usize::MAX);
        if n == usize::MAX { args.iter().skip(1).collect() } else { args.iter().skip(at + 1).take(n).collect() }
    };
    match first {
        "SCAN" | "PING" | "DBSIZE" | "INFO" | "ECHO" | "TIME" | "LASTSAVE" | "ROLE" | "LOLWUT" | "COMMAND" | "SELECT" => Vec::new(),
        f if MULTI_KEY.contains(&f) => args.iter().skip(1).collect(),
        "SINTERSTORE" | "SUNIONSTORE" | "SDIFFSTORE" | "PFMERGE" => args.iter().skip(1).collect(),
        "RENAME" | "RENAMENX" | "COPY" | "SMOVE" | "LMOVE" | "BLMOVE" | "RPOPLPUSH" | "BRPOPLPUSH" | "GEOSEARCHSTORE" | "ZRANGESTORE" | "LCS" => args.iter().skip(1).take(2).collect(),
        "BITOP" => args.iter().skip(2).collect(),
        "ZUNIONSTORE" | "ZINTERSTORE" | "ZDIFFSTORE" => {
            let mut k: Vec<&String> = args.get(1).into_iter().collect();
            k.extend(numkeys(2));
            k
        }
        "ZUNION" | "ZINTER" | "ZDIFF" | "ZINTERCARD" | "LMPOP" | "ZMPOP" => numkeys(1),
        "BLMPOP" | "BZMPOP" => numkeys(2),
        "XREAD" | "XREADGROUP" => match args.iter().position(|a| a.eq_ignore_ascii_case("STREAMS")) {
            Some(p) => {
                let rest = &args[p + 1..];
                rest.iter().take(rest.len() / 2).collect()
            }
            None => args.iter().skip(1).collect(),
        },
        "MSET" | "MSETNX" => args.iter().skip(1).step_by(2).collect(),
        "JSON.MGET" => args.iter().skip(1).take(args.len().saturating_sub(2)).collect(),
        "OBJECT" | "MEMORY" | "XINFO" => args.get(2).into_iter().collect(),
        _ => args.get(1).into_iter().collect(),
    }
}

impl Shaped {
    pub fn is_empty(&self) -> bool {
        self.strip.is_empty() && self.mask.is_empty() && !self.listing
    }
}

/// Applies `Shaped` to a reply: hidden fields go, sensitive fields are masked, and in listings
/// the names `visible` refuses are dropped. Returns the masked field names.
pub fn shape(v: &mut Value, s: &Shaped, key: &HashKey, visible: &dyn Fn(&str) -> bool) -> Vec<(String, Mask)> {
    let mut masked = Vec::new();
    if let Some(how) = s.whole
        && let Some(x) = v.get_mut("value")
    {
        mask_all(x, how, key);
        masked.extend(s.mask.iter().cloned());
    }
    walk(v, s, key, &mut masked);
    if s.listing {
        drop_names(v, visible);
    }
    masked
}

fn walk(v: &mut Value, s: &Shaped, key: &HashKey, masked: &mut Vec<(String, Mask)>) {
    match v {
        Value::Array(a) => {
            // Redis replies hold a hash as [field, value, field, value…].
            if a.len() % 2 == 0 && a.iter().all(Value::is_string) && !a.is_empty() {
                let mut i = 0;
                while i + 1 < a.len() {
                    let f = a[i].as_str().unwrap_or("").to_string();
                    if s.strip.iter().any(|p| glob(p, &f)) {
                        a.drain(i..i + 2);
                        continue;
                    }
                    if let Some((_, how)) = s.mask.iter().find(|(p, _)| glob(p, &f)) {
                        let val = a[i + 1].as_str().unwrap_or("").to_string();
                        a[i + 1] = Value::String(mask_value(*how, &val, key));
                        if !masked.iter().any(|(m, _)| *m == f) {
                            masked.push((f, *how));
                        }
                    }
                    i += 2;
                }
            }
            // …or as documents {field, value} (HGETALL, HSCAN).
            a.retain(|x| !x.get("field").and_then(Value::as_str).is_some_and(|f| s.strip.iter().any(|p| glob(p, f))));
            for x in a.iter_mut() {
                let field = x.get("field").and_then(Value::as_str).map(str::to_string);
                if let Some(f) = field
                    && let Some((_, how)) = s.mask.iter().find(|(p, _)| glob(p, &f))
                    && let Some(v) = x.get_mut("value")
                {
                    mask_all(v, *how, key);
                    if !masked.iter().any(|(m, _)| *m == f) {
                        masked.push((f, *how));
                    }
                    continue;
                }
                walk(x, s, key, masked);
            }
        }
        Value::Object(m) => {
            m.retain(|k, _| !s.strip.iter().any(|p| key_is(p, k)));
            for (k, x) in m.iter_mut() {
                if let Some((_, how)) = s.mask.iter().find(|(p, _)| key_is(p, k)) {
                    mask_all(x, *how, key);
                    if !masked.iter().any(|(f, _)| f == k) {
                        masked.push((k.clone(), *how));
                    }
                } else {
                    walk(x, s, key, masked);
                }
            }
        }
        _ => {}
    }
}

fn mask_all(v: &mut Value, how: Mask, key: &HashKey) {
    match v {
        Value::Null => {}
        Value::String(x) => *x = mask_value(how, x, key),
        Value::Array(a) => a.iter_mut().for_each(|x| mask_all(x, how, key)),
        Value::Object(m) => m.values_mut().for_each(|x| mask_all(x, how, key)),
        other => *other = Value::String(mask_value(how, &other.to_string(), key)),
    }
}

fn drop_names(v: &mut Value, visible: &dyn Fn(&str) -> bool) {
    let named = |x: &Value| -> Option<String> {
        match x {
            Value::String(s) => Some(s.clone()),
            Value::Object(m) => ["name", "index", "key", "_id", "alias"].iter().find_map(|k| m.get(*k).and_then(Value::as_str)).map(str::to_string),
            _ => None,
        }
    };
    match v {
        Value::Array(a) => {
            a.retain(|x| named(x).is_none_or(|n| visible(&n)));
            for x in a {
                if !x.is_string() {
                    drop_names(x, visible);
                }
            }
        }
        Value::Object(m) => {
            m.retain(|k, _| visible(k) || k.starts_with('_') || matches!(k.as_str(), "documents" | "value" | "count" | "more" | "total" | "cursor" | "keys"));
            for x in m.values_mut() {
                drop_names(x, visible);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::{ItemKind, SensitiveCol, Visible, VisibleItem, VisibleMode};
    use serde_json::json;

    fn item(name: &str, kind: ItemKind) -> VisibleItem {
        VisibleItem { name: name.into(), kind }
    }

    fn cx<'a>(r: &'a AgentRules, engine: Engine) -> Context<'a> {
        Context { rules: r, connection: "orders-mongo", engine, database: "shop", wide: false }
    }

    #[test]
    fn mongo_visibility() {
        let r = AgentRules {
            visible: Visible { mode: VisibleMode::Except, items: vec![item("shop.payments", ItemKind::Table), item("users.ssn", ItemKind::Column), item("analytics.*", ItemKind::Schema)] },
            ..Default::default()
        };
        let c = cx(&r, Engine::MongoDb);
        assert!(check("db.payments.find({})", false, &c).unwrap_err().contains("payments is not visible"));
        assert!(check("db.orders.aggregate([{$lookup: {from: \"payments\", localField: \"p\", foreignField: \"_id\", as: \"x\"}}])", false, &c).unwrap_err().contains("payments is not visible"));
        assert!(check("db.users.find({ssn: \"1\"})", false, &c).unwrap_err().contains("users.ssn is hidden"));
        assert!(check("db.users.aggregate([{$group: {_id: \"$ssn\"}}])", false, &c).unwrap_err().contains("hidden"));
        assert!(check("db.users.aggregate([{$replaceRoot: {newRoot: \"$$ROOT\"}}])", false, &c).unwrap_err().contains("whole documents"));
        let s = check("db.users.find({city: \"Recife\"})", false, &c).unwrap();
        assert_eq!(s.strip, vec!["ssn".to_string()]);
        let mut out = json!({"documents": [{"_id": 1, "name": "Ana", "ssn": "123", "nested": {"ssn": "x"}}]});
        shape(&mut out, &s, &HashKey::fixed(b"k"), &|_| true);
        assert_eq!(out, json!({"documents": [{"_id": 1, "name": "Ana", "nested": {}}]}));
        assert!(check("const x = db.users.find(); x", false, &c).unwrap_err().contains("scripts are not available"));
        let other = Context { database: "analytics", ..cx(&r, Engine::MongoDb) };
        assert!(check("db.events.find({})", false, &other).unwrap_err().contains("analytics is not visible"));
        let s = check("show collections", false, &c).unwrap();
        assert!(s.listing);
        let mut out = json!({"documents": [{"name": "orders"}, {"name": "payments"}]});
        shape(&mut out, &s, &HashKey::fixed(b"k"), &|n| r.visible.table_visible(Some("shop"), n, Names::Qualified));
        assert_eq!(out, json!({"documents": [{"name": "orders"}]}));
    }

    #[test]
    fn mongo_masked_and_permissions() {
        let mut r = AgentRules { view: AgentView::Masked, ..Default::default() };
        r.sensitive.push(SensitiveCol { col: "customers.email".into(), how: Mask::Partial, on: true });
        let c = cx(&r, Engine::MongoDb);
        let s = check("db.customers.find({email: /@acme/})", false, &c).unwrap();
        let mut out = json!({"documents": [{"email": "ana@acme.com", "city": "Recife"}]});
        let m = shape(&mut out, &s, &HashKey::fixed(b"k"), &|_| true);
        assert_eq!(out["documents"][0]["email"], "a•••@acme.com");
        assert_eq!(m, vec![("email".to_string(), Mask::Partial)]);
        assert!(check("db.customers.aggregate([{$group: {_id: \"$email\"}}])", false, &c).unwrap_err().contains("masked"));
        assert!(check("db.customers.find({}).sort({email: 1})", false, &c).unwrap_err().contains("masked"));
        assert!(check("db.customers.distinct(\"email\")", false, &c).is_err());
        let p = AgentRules { structure: false, code: false, activity: false, ..Default::default() };
        let c = cx(&p, Engine::MongoDb);
        assert!(check("db.orders.createIndex({a: 1})", true, &c).unwrap_err().contains("changing structure"));
        assert!(check("db.orders.aggregate([{$out: \"x\"}])", true, &c).unwrap_err().contains("changing structure"));
        assert!(check("db.orders.find({$where: \"this.a > 1\"})", false, &c).unwrap_err().contains("running code"));
        assert!(check("db.runCommand({currentOp: 1})", false, &c).unwrap_err().contains("server activity"));
        assert!(check("db.orders.insertOne({a: 1})", true, &c).is_ok());
        let st = AgentRules { view: AgentView::Structure, ..Default::default() };
        let c = cx(&st, Engine::MongoDb);
        assert!(check("db.orders.find({})", false, &c).unwrap_err().contains("structure only"));
        assert!(check("db.orders.getIndexes()", false, &c).is_ok());
        assert!(check("db.orders.createIndex({a: 1})", true, &c).is_ok());
        assert!(check("db.orders.insertOne({a: 1})", true, &c).is_err());
    }

    #[test]
    fn search_visibility() {
        let r = AgentRules { visible: Visible { mode: VisibleMode::Except, items: vec![item("payments-*", ItemKind::Table), item("customers.card", ItemKind::Column)] }, ..Default::default() };
        let c = cx(&r, Engine::Elasticsearch);
        assert!(check("GET payments-2026/_search", false, &c).unwrap_err().contains("not visible"));
        assert!(check("GET orders,payments-1/_search", false, &c).unwrap_err().contains("not visible"));
        assert!(check("GET pay%6Dents-1/_search", false, &c).unwrap_err().contains("not visible"), "percent-encoded names are decoded");
        assert!(check("GET _search", false, &c).unwrap_err().contains("name the index"));
        assert!(check("GET */_search", false, &c).unwrap_err().contains("not visible"));
        assert!(check("POST _sql\n{\"query\": \"select * from payments\"}", false, &c).unwrap_err().contains("SQL"));
        assert!(check("GET customers/_search\n{\"query\": {\"match\": {\"card\": \"4242\"}}}", false, &c).unwrap_err().contains("hidden"));
        let s = check("GET customers/_search\n{\"query\": {\"match\": {\"name\": \"ana\"}}}", false, &c).unwrap();
        assert_eq!(s.strip, vec!["card".to_string()]);
        let s = check("GET _cat/indices?format=json", false, &c).unwrap();
        let mut out = json!({"value": [{"index": "orders"}, {"index": "payments-2026"}]});
        shape(&mut out, &s, &HashKey::fixed(b"k"), &|n| r.visible.table_visible(None, n, Names::Flat));
        assert_eq!(out, json!({"value": [{"index": "orders"}]}));
        let p = AgentRules { structure: false, code: false, activity: false, ..Default::default() };
        let c = cx(&p, Engine::OpenSearch);
        assert!(check("PUT orders", true, &c).unwrap_err().contains("changing structure"));
        assert!(check("PUT orders/_mapping\n{}", true, &c).unwrap_err().contains("changing structure"));
        assert!(check("POST orders/_update/1\n{\"script\": {\"source\": \"x\"}}", true, &c).unwrap_err().contains("running code"));
        assert!(check("GET _tasks", false, &c).unwrap_err().contains("server activity"));
        assert!(check("GET orders/_search", false, &c).is_ok());
    }

    #[test]
    fn redis_rules() {
        let r = AgentRules::default();
        let c = cx(&r, Engine::Redis);
        assert_eq!(check("KEYS *", false, &c).unwrap_err(), "KEYS never runs for agents");
        assert!(check("FLUSHALL", true, &c).unwrap_err().contains("never runs"));
        let mut off = AgentRules::default();
        off.redis.scripting = false;
        assert!(check("EVAL \"return 1\" 0", true, &cx(&off, Engine::Redis)).unwrap_err().contains("@scripting"));
        let v = AgentRules { visible: Visible { mode: VisibleMode::Except, items: vec![item("session:*", ItemKind::Table), item("user:*.password", ItemKind::Column)] }, ..Default::default() };
        let c = cx(&v, Engine::Redis);
        assert!(check("GET session:9", false, &c).unwrap_err().contains("not visible"));
        assert!(check("MGET a session:1", false, &c).unwrap_err().contains("not visible"));
        assert!(check("HGET user:1 password", false, &c).unwrap_err().contains("hidden"));
        assert!(check("HVALS user:1", false, &c).is_err());
        assert!(check("RANDOMKEY", false, &c).is_err());
        let s = check("HGETALL user:1", false, &c).unwrap();
        let mut out = json!({"value": ["name", "Ana", "password", "x"]});
        shape(&mut out, &s, &HashKey::fixed(b"k"), &|_| true);
        assert_eq!(out, json!({"value": ["name", "Ana"]}));
        let s = check("SCAN 0 MATCH * COUNT 100", false, &c).unwrap();
        let mut out = json!({"value": ["0", ["user:1", "session:2"]]});
        shape(&mut out, &s, &HashKey::fixed(b"k"), &|n| v.visible.table_visible(None, n, Names::Flat));
        assert_eq!(out, json!({"value": ["0", ["user:1"]]}));
    }
}
