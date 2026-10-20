//! MongoDB, Elasticsearch/OpenSearch and Redis statements as agents may run them: hidden
//! collections, indices and keys are refused, hidden fields are refused when named and removed
//! from what comes back, sensitive fields are masked in Masked data, and what Agents may do is
//! held. Fail closed: what cannot be checked (scripts, cross-index searches) is refused while a
//! connection keeps something from agents.

use regex::Regex;
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
}

pub struct Context<'a> {
    pub rules: &'a AgentRules,
    pub connection: &'a str,
    pub engine: Engine,
    pub database: &'a str,
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
    // Other collections a pipeline reads or writes.
    static TARGETS: OnceLock<Regex> = OnceLock::new();
    let targets = TARGETS.get_or_init(|| Regex::new(r#"(?:\bfrom|\bcoll|\$unionWith|\$out|\binto|getCollection\(|getSiblingDB\()\s*:?\s*["']([^"']+)["']"#).expect("targets"));
    for t in targets.captures_iter(text).filter_map(|c| c.get(1)) {
        if !r.visible.table_visible(Some(db), t.as_str(), names) {
            return Err(format!("{} is not visible to agents on {conn}", t.as_str()));
        }
    }
    let w = words(text);
    let Some(coll) = coll else {
        return Ok(Shaped { listing, ..Default::default() });
    };
    let hidden = r.visible.hidden_column_patterns(Some(db), coll, names);
    if !hidden.is_empty() {
        if let Some(f) = hidden.iter().find(|f| names_field(&w, f)) {
            return Err(format!("{coll}.{f} is hidden from agents on {conn}"));
        }
        if ["$$root", "$$current", "$objecttoarray", "$replaceroot", "$replacewith"].iter().any(|x| lower.contains(x)) {
            return Err(format!("whole documents of {coll} are not available to agents on {conn}: some fields are hidden"));
        }
    }
    let mut mask = Vec::new();
    if r.view == AgentView::Masked {
        for s in r.sensitive.iter().filter(|s| s.on) {
            let field = s.col.rsplit_once('.').map_or(s.col.as_str(), |(t, f)| if sensitive_match(&s.col, Some(db), coll, f, names) || glob(t, coll) { f } else { "" });
            if field.is_empty() {
                continue;
            }
            let computes = !matches!(op, mql::Op::Find { .. } | mql::Op::Count { .. }) || matches!(op, mql::Op::Find { sort: Some(d), .. } if d.keys().any(|k| glob(field, k)));
            if computes && names_field(&w, field) {
                return Err(format!("{coll}.{field} is masked for agents on {conn}: find and filter on it; it cannot be grouped, sorted or computed"));
            }
            mask.push((field.to_string(), s.how));
        }
        if !mask.is_empty() && ["$$root", "$$current", "$objecttoarray"].iter().any(|x| lower.contains(x)) {
            return Err(format!("whole documents of {coll} are not available to agents in Masked data on {conn}"));
        }
    }
    Ok(Shaped { strip: hidden, mask, listing })
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
    let indices: Vec<String> = if first.starts_with('_') { Vec::new() } else { first.split(',').map(str::to_string).collect() };
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
    // _mget and _msearch name more indices inside the body.
    for b in &body {
        for k in ["_index", "index"] {
            if let Some(Value::String(i)) = b.get(k).or_else(|| b.get("docs").and_then(|d| d.get(0)).and_then(|d| d.get(k)))
                && !r.visible.table_visible(None, i, names)
            {
                return Err(format!("{i} is not visible to agents on {conn}"));
            }
        }
    }
    let mut seen: Vec<String> = Vec::new();
    for b in &body {
        json_names(b, &mut seen);
    }
    let mut strip = Vec::new();
    for i in &indices {
        strip.extend(r.visible.hidden_column_patterns(None, i, names));
    }
    if let Some(f) = strip.iter().find(|f| names_field(&seen, f)) {
        return Err(format!("{f} is hidden from agents on {conn}"));
    }
    let mut mask = Vec::new();
    if r.view == AgentView::Masked {
        for s in r.sensitive.iter().filter(|s| s.on) {
            let (t, field) = s.col.rsplit_once('.').unwrap_or(("*", s.col.as_str()));
            if !indices.iter().any(|i| glob(t, i)) && t != "*" {
                continue;
            }
            let mut computed = Vec::new();
            for b in &body {
                for k in ["aggs", "aggregations", "sort", "collapse", "script_fields", "runtime_mappings"] {
                    if let Some(x) = b.get(k) {
                        json_names(x, &mut computed);
                    }
                }
            }
            if names_field(&computed, field) {
                return Err(format!("{field} is masked for agents on {conn}: search and filter on it; it cannot be aggregated or sorted"));
            }
            mask.push((field.to_string(), s.how));
        }
    }
    Ok(Shaped { strip, mask, listing })
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
    let keys: Vec<&String> = match first {
        "SCAN" | "PING" | "DBSIZE" | "INFO" | "ECHO" | "TIME" => Vec::new(),
        f if MULTI_KEY.contains(&f) => args.iter().skip(1).collect(),
        "MSET" | "MSETNX" => args.iter().skip(1).step_by(2).collect(),
        _ => args.get(1).into_iter().collect(),
    };
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
    let mut mask = Vec::new();
    if r.view == AgentView::Masked {
        for s in r.sensitive.iter().filter(|s| s.on) {
            let (t, field) = s.col.rsplit_once('.').unwrap_or(("*", s.col.as_str()));
            if t == "*" || keys.iter().any(|k| glob(t, k)) {
                mask.push((field.to_string(), s.how));
            }
        }
    }
    Ok(Shaped { strip, mask, listing })
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
            m.retain(|k, _| !s.strip.iter().any(|p| glob(p, k)));
            for (k, x) in m.iter_mut() {
                if let Some((_, how)) = s.mask.iter().find(|(p, _)| glob(p, k)) {
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
        Context { rules: r, connection: "orders-mongo", engine, database: "shop" }
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
