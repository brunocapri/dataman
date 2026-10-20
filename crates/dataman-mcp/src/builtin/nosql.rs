use std::time::Duration;

use dataman_core::guard::WriteClass;
use dataman_core::nosql::{self, Body, ItemKind, Output, Page};
use serde_json::{Map, Value, json};

use std::sync::Arc;

use super::{Builtin, confirm_prop, conn_schema, ms, rules, table};
use crate::hub::{Front, TabWrite};
use crate::policy::Need;
use crate::tool::{Args, BoxFut, Call, Opened, ToolError};

const MAX_BYTES: usize = 256 * 1024;

fn query_prop() -> Value {
    json!({"type": "string", "maxLength": 200000, "description": "One statement in the connection's language (list_connections' language). MongoDB's shell: db.users.find({age: {$gt: 30}}).sort({name: 1}).limit(20), db.orders.aggregate([...]), db.users.countDocuments({}), show collections; or a mongosh script (JavaScript: const/let, new Date(), Date.now(), loops, functions, cursor.forEach / toArray / map, print / printjson; the last value is the result, a cursor's as documents; printed lines come back as printed). A script's writes (by method name and written-out filter) decide its tier before it runs, and every call is held to that as it runs. Elasticsearch / OpenSearch, Kibana's console: a method line, then the JSON body on the next lines (GET /orders/_search, newline, {\"query\": {\"match\": {\"status\": \"paid\"}}}), or GET _cat/indices. Redis: one command as redis-cli takes it (HGETALL user:1, SCAN 0 MATCH user:* COUNT 100, SET k v EX 60)."})
}

pub fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "nosql_query",
            description: "Reads from a MongoDB, Elasticsearch, OpenSearch or Redis connection in its own language (one statement). Redis: read commands (GET, HGETALL, LRANGE, SCAN, TYPE, INFO…; a key's whole value comes back as entries). MongoDB: find/findOne with sort, skip, limit, projection; aggregate (without $out/$merge); countDocuments, distinct, getIndexes, stats; show dbs / collections; read commands. Elasticsearch / OpenSearch: GET requests, _search, _count, _msearch, _mget, _cat/…. Writes are refused here (use nosql_execute). Returns {documents: [...], count, total?, more, printed?, ms} (MongoDB's as relaxed extended JSON, a search's hits as their _source) or {value, ms} for counts, aggregations and other answers. At most max_rows documents (default 50) and ~256 KB.",
            schema: conn_schema()
                .prop("query", true, query_prop())
                .prop("max_rows", false, json!({"type": "integer", "minimum": 1, "maximum": 1000, "description": "Most documents returned (default 50)."}))
                .prop("skip", false, json!({"type": "integer", "minimum": 0, "description": "Documents to skip past the statement's own skip (paging)."}))
                .prop("timeout_ms", false, json!({"type": "integer", "minimum": 100, "maximum": 300000, "description": "Time limit in ms (default 30000)."})),
            need: Some(Need::Read),
            run: nosql_query,
        },
        Builtin {
            name: "nosql_execute",
            description: "Writes to a MongoDB, Elasticsearch, OpenSearch or Redis connection in its own language (one statement: insertOne, updateMany…; PUT /idx/_doc/1, _delete_by_query…; SET, HSET, DEL…), as the connection's MCP tier allows (set by the user in Dataman; default read). Tier write: inserts and updates/deletes with a filter or query, changing at most the row limit (counted before it runs; more waits for the user's Allow); anything destructive (no filter, match_all, drops, index creation and deletion, settings, other commands) is refused. Tier full: destructive statements wait for the user's Allow in the app. On production every write waits for the user. A MongoDB script that writes waits for the user's Allow (its writes cannot be counted before it runs). Returns {written, affected, detail?, ms}, or a script's last value with writes: [what each write changed] and printed.",
            schema: confirm_prop(conn_schema().prop("query", true, query_prop()).prop("timeout_ms", false, json!({"type": "integer", "minimum": 100, "maximum": 300000, "description": "Time limit in ms (default 30000)."}))),
            need: Some(Need::Write),
            run: nosql_execute,
        },
    ]
}

pub fn is_nosql(cx: &Call, a: &Args) -> bool {
    a.opt_str("connection").and_then(|c| cx.hub.profile(c).ok()).is_some_and(|p| !p.engine.is_sql())
}

fn failed(e: anyhow::Error) -> ToolError {
    match e.downcast_ref::<nosql::Failed>() {
        Some(f) => ToolError::Failed(f.message.clone()),
        None => ToolError::Failed(format!("{e:#}")),
    }
}

fn bytes_of(cx: &Call, a: &Args) -> usize {
    cx.rules_of(a).read_bytes.map_or(MAX_BYTES, |b| (b as usize).min(MAX_BYTES))
}

async fn run(cx: &Call, o: &Opened, text: &str, page: Page, timeout: Duration) -> Result<nosql::Ran, ToolError> {
    let r = o.conn.run_nosql(&o.database, text, page);
    tokio::select! {
        r = tokio::time::timeout(timeout, r) => match r {
            Ok(r) => r.map_err(failed),
            Err(_) => Err(ToolError::Timeout(format!("the statement passed {} ms and was stopped", timeout.as_millis()))),
        },
        _ = cx.cancel.cancelled() => Err(ToolError::Cancelled),
    }
}

fn output_json(ran: &nosql::Ran, max_bytes: usize) -> Value {
    let mut m = Map::new();
    match &ran.output {
        Output::Docs(d) => {
            let mut bytes = 0;
            let mut docs = Vec::new();
            let mut cut = false;
            for doc in &d.docs {
                let v = doc.body.to_json();
                bytes += v.to_string().len();
                if bytes > max_bytes && !docs.is_empty() {
                    cut = true;
                    break;
                }
                docs.push(v);
            }
            m.insert("count".into(), json!(docs.len()));
            m.insert("documents".into(), Value::Array(docs));
            if let Some(t) = d.total {
                m.insert("total".into(), json!(t));
            }
            m.insert("more".into(), json!(d.more || cut));
            if cut {
                m.insert("truncated".into(), json!("bytes"));
                m.insert("note".into(), json!(format!("the documents passed ~{} KB; project fewer fields or lower max_rows", max_bytes / 1024)));
            }
        }
        Output::Value(v) => {
            m.insert("value".into(), v.to_json());
        }
        Output::Written { affected, summary, detail } => {
            m.insert("written".into(), json!(summary));
            m.insert("affected".into(), json!(affected));
            if let Some(d) = detail {
                m.insert("detail".into(), d.to_json());
            }
        }
        Output::Use(db) => {
            m.insert("note".into(), json!(format!("pass database: {db:?} to the next calls instead of use")));
        }
    }
    if !ran.printed.is_empty() {
        m.insert("printed".into(), json!(ran.printed));
    }
    if !ran.writes.is_empty() {
        m.insert("writes".into(), json!(ran.writes));
    }
    if let Some(db) = &ran.moved {
        m.insert("note".into(), json!(format!("the script moved db to {db:?}: pass database: {db:?} to the next calls")));
    }
    m.insert("ms".into(), json!(ms(ran.elapsed)));
    Value::Object(m)
}

struct Shown {
    front: Option<Arc<dyn Front>>,
    tab: Option<u64>,
}

impl Shown {
    async fn new(cx: &Call, a: &Args, text: &str, open: bool) -> Result<Shown, ToolError> {
        let Some(front) = cx.tab_front(open) else { return Ok(Shown { front: None, tab: None }) };
        let p = cx.target(a, Need::Read)?;
        let database = a.opt_str("database").map(str::to_string).unwrap_or_else(|| p.database.clone());
        let w = TabWrite { client: cx.client.clone(), connection_id: p.id.clone(), database, sql: text.to_string(), title: a.opt_str("title").map(str::to_string) };
        let tab = front.show_doc(w).await;
        Ok(Shown { front: Some(front), tab })
    }

    fn done<T>(&self, r: &Result<T, ToolError>, ran: Option<&nosql::Ran>) {
        if let (Some(f), Some(tab)) = (&self.front, self.tab) {
            f.doc_done(tab, match (r, ran) {
                (Ok(_), Some(ran)) => Ok(ran.clone()),
                (Err(e), _) => Err(e.message()),
                (Ok(_), None) => Err("nothing ran".into()),
            });
        }
    }
}

fn nosql_query(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move { query(cx, &a, "query", false).await })
}

pub(super) async fn query(cx: &Call, a: &Args, key: &str, open: bool) -> Result<Value, ToolError> {
    let text = a.str(key)?;
    cx.note_sql(text);
    let target = cx.target(a, Need::Read)?;
    match nosql::classify(target.engine, text) {
        Err(f) => return Err(ToolError::Invalid(f.message)),
        Ok(WriteClass::Read) => {}
        Ok(_) => {
            return Err(ToolError::Refused("this statement writes: use nosql_execute".into()));
        }
    }
    let shaped = rules::nosql_check(a, &target, text, false)?;
    let shown = Shown::new(cx, a, text, open).await?;
    let r = read(cx, a, text).await;
    shown.done(&r, r.as_ref().ok());
    let ran = r?;
    let mut v = output_json(&ran, bytes_of(cx, a));
    rules::nosql_shape(cx, a, &target, text, &mut v, &shaped);
    if open && let Some(m) = v.as_object_mut() {
        m.insert("in_tab".into(), json!(shown.tab.is_some()));
    }
    Ok(v)
}

async fn read(cx: &Call, a: &Args, text: &str) -> Result<nosql::Ran, ToolError> {
    let o = cx.open(a, Need::Read).await?;
    let _slot = tokio::select! {
        p = cx.hub.permit(&o.profile.id) => p,
        _ = cx.cancel.cancelled() => return Err(ToolError::Cancelled),
    };
    let cap = (cx.hub.settings().mcp_max_rows.max(1) as u64).min(o.profile.mcp_rules.read_rows.unwrap_or(u64::MAX)).max(1);
    let page = Page { skip: a.u64_or("skip", 0), limit: a.u64_or("max_rows", 50).clamp(1, cap.min(1000)) as usize };
    let ran = run(cx, &o, text, page, cx.time_limit(a)).await?;
    if let Output::Docs(d) = &ran.output {
        cx.note_rows(d.docs.len());
    }
    Ok(ran)
}

fn nosql_execute(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move { execute(cx, &a, "query", false).await })
}

pub(super) async fn execute(cx: &Call, a: &Args, key: &str, open: bool) -> Result<Value, ToolError> {
    let text = a.str(key)?;
    cx.note_sql(text);
    let target = cx.target(a, Need::Write)?;
    let class = nosql::classify(target.engine, text).map_err(|f| ToolError::Invalid(f.message))?;
    let shaped = rules::nosql_check(a, &target, text, true)?;
    let shown = Shown::new(cx, a, text, open).await?;
    let r = write(cx, a, text, &class, shown.tab).await;
    shown.done(&r, r.as_ref().ok());
    let ran = r?;
    let mut v = output_json(&ran, bytes_of(cx, a));
    rules::nosql_shape(cx, a, &target, text, &mut v, &shaped);
    if let Some(m) = v.as_object_mut() {
        if let Some(n) = cx.note().approval.filter(|n| n.decision == crate::audit::Decision::Approved) {
            m.insert("approved".into(), json!({"by": n.by, "reasons": n.reasons}));
        }
        if open {
            m.insert("in_tab".into(), json!(shown.tab.is_some()));
        }
    }
    Ok(v)
}

async fn write(cx: &Call, a: &Args, text: &str, class: &WriteClass, tab: Option<u64>) -> Result<nosql::Ran, ToolError> {
    let o = cx.open(a, Need::Write).await?;
    let _slot = tokio::select! {
        p = cx.hub.permit(&o.profile.id) => p,
        _ = cx.cancel.cancelled() => return Err(ToolError::Cancelled),
    };
    // What it would change, counted first: past the row limit it waits for the user. A script's
    // writes cannot be counted before it runs: the user says yes to it first.
    let script = nosql::is_script(o.conn.engine(), text);
    let over = if *class == WriteClass::SafeWrite && !o.profile.mcp_yolo_on() {
        if script {
            Some((None, "a script: what it changes is known only as it runs".to_string()))
        } else {
            let n = o.conn.nosql_affects(&o.database, text).await.map_err(failed)?;
            n.and_then(|n| crate::policy::over_limit(n, cx.hub.row_limit(&o.profile)).map(|why| (Some(n), why)))
        }
    } else {
        None
    };
    cx.gate(&o, class, text, tab).await?;
    let asked = cx.note().approval.is_some_and(|n| n.decision == crate::audit::Decision::Approved);
    if let Some((n, why)) = over
        && !asked
    {
        cx.ask_human(&o, text, vec![why], n, false).await?;
    }
    let ran = run(cx, &o, text, Page::default(), cx.time_limit(a)).await?;
    if let Output::Written { affected, .. } = &ran.output {
        cx.note_rows(*affected as usize);
    }
    Ok(ran)
}

pub(super) async fn databases(cx: &Call, o: &Opened) -> Result<Value, ToolError> {
    let items = o.conn.nosql_tree(None).await.map_err(failed)?;
    let rows: Vec<Vec<Value>> = items
        .iter()
        .filter(|i| i.kind == ItemKind::Database && rules::schema_visible(&o.profile, &i.name))
        .map(|i| vec![json!(i.name), i.size.map_or(Value::Null, |b| json!(dataman_core::schema::objects::human_size(b as i64)))])
        .collect();
    cx.note_rows(rows.len());
    let note = if o.conn.engine() == dataman_core::Engine::MongoDb { "a database holds collections: pass it as database to the other tools" } else { "no databases here: list_tables lists the indices" };
    Ok(json!({"databases": table(&["database", "size"], rows), "note": note}))
}

pub(super) async fn collections(cx: &Call, o: &Opened, limit: usize) -> Result<Value, ToolError> {
    let items: Vec<_> = o.conn.nosql_tree(Some(&o.database)).await.map_err(failed)?.iter().filter(|i| rules::table_visible(&o.profile, &o.database, &i.name)).cloned().collect();
    let more = items.len() > limit;
    let rows: Vec<Vec<Value>> = items
        .iter()
        .take(limit)
        .map(|i| {
            let kind = match (&i.note, i.kind) {
                (Some(t), _) => t.as_str(),
                (None, ItemKind::View) => "view",
                _ => "collection",
            };
            vec![json!(o.database), json!(i.name), json!(kind), i.count.map_or(Value::Null, |n| json!(n)), Value::Null]
        })
        .collect();
    cx.note_rows(rows.len());
    let mut v = table(&["database", "name", "kind", "rows_est", "size"], rows);
    if more {
        v["truncated"] = json!(true);
    }
    Ok(v)
}

pub(super) async fn search(cx: &Call, o: &Opened, pattern: &str, limit: usize) -> Result<Value, ToolError> {
    let like = |s: &str| super::catalog::like_matches(s, pattern);
    let mut rows = Vec::new();
    for db in o.conn.nosql_tree(None).await.map_err(failed)?.iter().filter(|i| i.kind == ItemKind::Database && rules::schema_visible(&o.profile, &i.name)) {
        for c in o.conn.nosql_tree(Some(&db.name)).await.map_err(failed)?.iter() {
            if like(&c.name) && rules::table_visible(&o.profile, &db.name, &c.name) {
                rows.push(vec![json!(if c.kind == ItemKind::View { "view" } else { "collection" }), json!(db.name), json!(c.name), c.count.map_or(Value::Null, |n| json!(format!("{n} documents")))]);
            }
            if rows.len() > limit {
                break;
            }
        }
    }
    let more = rows.len() > limit;
    rows.truncate(limit);
    cx.note_rows(rows.len());
    let mut v = table(&["kind", "database", "name", "detail"], rows);
    if more {
        v["truncated"] = json!(true);
    }
    Ok(v)
}

pub(super) async fn describe(cx: &Call, o: &Opened, coll: &str) -> Result<Value, ToolError> {
    let items = o.conn.nosql_tree(Some(&o.database)).await.map_err(failed)?;
    let Some(item) = items.iter().find(|i| i.name == coll) else {
        return Err(ToolError::Invalid(format!("no collection {coll:?} in {} (list_tables lists them)", o.database)));
    };
    if !rules::table_visible(&o.profile, &o.database, coll) {
        return Err(rules::not_visible(&o.profile, coll));
    }
    let mut fields = o.conn.sample_fields(&o.database, coll).await.map_err(failed)?;
    // Hidden fields go at any depth, as they do from documents.
    fields.retain(|(path, _)| !path.split('.').any(|seg| rules::column_hidden(&o.profile, &o.database, coll, seg)) && !rules::column_hidden(&o.profile, &o.database, coll, path));
    let mut out = Map::new();
    out.insert("collection".into(), json!(coll));
    out.insert("database".into(), json!(o.database));
    out.insert("kind".into(), json!(if item.kind == ItemKind::View { "view" } else { "collection" }));
    if let Some(t) = &item.note {
        out.insert("kind".into(), json!("key"));
        out.insert("type".into(), json!(t));
        out.insert("view".into(), json!(nosql::open_statement_for(&o.conn, &o.database, coll)));
        return Ok(Value::Object(out));
    }
    if let Some(n) = item.count {
        out.insert("rows_est".into(), json!(n));
    }
    out.insert("fields".into(), Value::Array(fields.iter().map(|(p, t)| json!([p, t.join(" | ")])).collect()));
    out.insert("fields_note".into(), json!("sampled from up to 100 documents: [path, types seen]"));
    if item.kind != ItemKind::View {
        let stmt = nosql::open_statement(o.conn.engine(), coll).replace(".find({})", ".getIndexes()");
        if let Ok(ran) = run(cx, o, &stmt, Page::default(), Duration::from_secs(10)).await
            && let Output::Value(Body::Bson(b)) = &ran.output
        {
            out.insert("indexes".into(), b.clone().into_relaxed_extjson());
        }
    }
    Ok(Value::Object(out))
}

pub(super) async fn sample(cx: &Call, o: &Opened, coll: &str, n: u64, random: bool) -> Result<Value, ToolError> {
    if o.profile.mcp_rules.view == dataman_core::privacy::AgentView::Structure {
        return Err(ToolError::Refused(rules::structure_only(&o.profile)));
    }
    let base = nosql::open_statement_for(&o.conn, &o.database, coll);
    let stmt = if random { base.replace(".find({})", &format!(".aggregate([{{$sample: {{size: {n}}}}}])")) } else { base };
    let mut m = Map::new();
    m.insert("connection".into(), json!(o.profile.name));
    m.insert("database".into(), json!(o.database));
    let a = Args(m);
    let shaped = rules::nosql_check(&a, &o.profile, &stmt, false)?;
    let ran = run(cx, o, &stmt, Page { skip: 0, limit: n as usize }, Duration::from_secs(30)).await?;
    if let Output::Docs(d) = &ran.output {
        cx.note_rows(d.docs.len());
    }
    let mut v = output_json(&ran, MAX_BYTES);
    rules::nosql_shape(cx, &a, &o.profile, &stmt, &mut v, &shaped);
    Ok(v)
}
