use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use futures_util::StreamExt as _;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};

use crate::config::{ConnectionProfile, SslMode};
use crate::guard::WriteClass;
use crate::nosql::mql::Op;
use crate::nosql::{Body, Doc, Docs, Item, ItemKind, Output, Page, Source};
use crate::ssh::Tunnel;

pub(crate) struct Client {
    pub client: mongodb::Client,
    pub default_db: String,
    pub tls_active: bool,
}

const SELECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn options(p: &ConnectionProfile, password: Option<&str>, tunnel: Option<&Tunnel>) -> Result<ClientOptions> {
    let opt = |k: &str| p.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let srv = opt("srv").is_some_and(|v| v == "true" || v == "1");
    let mut o = if srv && tunnel.is_none() {
        ClientOptions::parse(format!("mongodb+srv://{}/", p.host)).await.with_context(|| format!("resolving {}", p.host))?
    } else {
        ClientOptions::default()
    };
    if !srv || tunnel.is_some() {
        o.hosts = match tunnel {
            Some(t) => vec![ServerAddress::Tcp { host: "127.0.0.1".into(), port: Some(t.local_addr().port()) }],
            None => p
                .host
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(|h| match h.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()) {
                    Some((host, port)) => ServerAddress::Tcp { host: host.into(), port: port.parse().ok() },
                    None => ServerAddress::Tcp { host: h.into(), port: Some(p.port) },
                })
                .collect(),
        };
    }
    let direct = match opt("directConnection") {
        Some(v) => v == "true",
        None => tunnel.is_some() || (o.hosts.len() == 1 && !srv && opt("replicaSet").is_none()),
    };
    o.direct_connection = Some(direct);
    if let Some(rs) = opt("replicaSet") {
        o.repl_set_name = Some(rs.into());
    }
    if !p.user.is_empty() {
        let mut c = Credential::default();
        c.username = Some(p.user.clone());
        c.password = password.map(str::to_string);
        c.source = Some(opt("authSource").unwrap_or("admin").to_string());
        if let Some(m) = opt("authMechanism") {
            c.mechanism = Some(m.parse().map_err(|e| anyhow!("authMechanism: {e}"))?);
        }
        o.credential = Some(c);
    }
    o.tls = match p.sslmode {
        // MongoDB has no "TLS if offered": prefer is plain, as disable.
        SslMode::Disable | SslMode::Prefer => (srv && tunnel.is_none()).then(|| Tls::Enabled(TlsOptions::default())),
        SslMode::Require => {
            let mut t = TlsOptions::default();
            t.allow_invalid_certificates = Some(true);
            Some(Tls::Enabled(t))
        }
        SslMode::VerifyFull => Some(Tls::Enabled(TlsOptions::default())),
    };
    o.app_name = Some("Dataman".into());
    o.server_selection_timeout = Some(SELECT_TIMEOUT);
    o.connect_timeout = Some(SELECT_TIMEOUT);
    Ok(o)
}

pub(crate) async fn connect(p: &ConnectionProfile, password: Option<&str>, tunnel: Option<&Tunnel>) -> Result<Client> {
    let o = options(p, password, tunnel).await?;
    let tls_active = matches!(o.tls, Some(Tls::Enabled(_)));
    let client = mongodb::Client::with_options(o)?;
    let default_db = if p.database.trim().is_empty() { "test".to_string() } else { p.database.trim().to_string() };
    let ping = client.database(&default_db).run_command(doc! { "ping": 1 }).await;
    if let Err(e) = ping {
        if let Some(t) = tunnel
            && let Some(te) = t.take_error()
        {
            return Err(te.into());
        }
        return Err(anyhow!(message(&e.into())));
    }
    Ok(Client { client, default_db, tls_active })
}

pub(crate) async fn version(c: &Client) -> Result<String> {
    let r = c.client.database("admin").run_command(doc! { "buildInfo": 1 }).await?;
    Ok(format!("MongoDB {}", r.get_str("version").unwrap_or("")).trim().to_string())
}

pub(crate) fn message(e: &anyhow::Error) -> String {
    for c in e.chain() {
        if let Some(e) = c.downcast_ref::<mongodb::error::Error>() {
            use mongodb::error::ErrorKind;
            return match &*e.kind {
                ErrorKind::Command(c) => format!("{} ({})", c.message, c.code_name),
                ErrorKind::Write(mongodb::error::WriteFailure::WriteError(w)) => w.message.clone(),
                ErrorKind::Authentication { message, .. } => format!("authentication failed: {message}"),
                ErrorKind::ServerSelection { message, .. } => {
                    let first = message.split(", Topology:").next().unwrap_or(message);
                    format!("cannot reach the server: {first}")
                }
                _ => e.to_string(),
            };
        }
    }
    format!("{e:#}")
}

pub(crate) async fn databases(c: &Client) -> Result<Vec<Item>> {
    let admin = c.client.database("admin");
    let r = match admin.run_command(doc! { "listDatabases": 1 }).await {
        Ok(r) => r,
        Err(_) => admin.run_command(doc! { "listDatabases": 1, "authorizedDatabases": true, "nameOnly": true }).await.map_err(|e| anyhow!(message(&e.into())))?,
    };
    let mut out: Vec<Item> = r
        .get_array("databases")
        .map(|a| {
            a.iter()
                .filter_map(Bson::as_document)
                .filter_map(|d| {
                    let name = d.get_str("name").ok()?.to_string();
                    let size = number(d.get("sizeOnDisk")).map(|n| n as u64);
                    Some(Item { name, kind: ItemKind::Database, count: None, size, note: None })
                })
                .collect()
        })
        .unwrap_or_default();
    if !out.iter().any(|i| i.name == c.default_db) {
        out.push(Item { name: c.default_db.clone(), kind: ItemKind::Database, count: None, size: None, note: None });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub(crate) async fn collections(c: &Client, db: &str) -> Result<Vec<Item>> {
    let d = c.client.database(db);
    let r = d.run_command(doc! { "listCollections": 1, "authorizedCollections": true, "nameOnly": false }).await.map_err(|e| anyhow!(message(&e.into())))?;
    let batch = r.get_document("cursor").and_then(|c| c.get_array("firstBatch")).cloned().unwrap_or_default();
    let mut items: Vec<Item> = batch
        .iter()
        .filter_map(Bson::as_document)
        .filter_map(|s| {
            let name = s.get_str("name").ok()?.to_string();
            let kind = if s.get_str("type") == Ok("view") { ItemKind::View } else { ItemKind::Collection };
            Some(Item { name, kind, count: None, size: None, note: None })
        })
        .collect();
    items.sort_by(|a, b| (a.name.starts_with("system."), &a.name).cmp(&(b.name.starts_with("system."), &b.name)));
    let names: Vec<(String, bool)> = items.iter().map(|i| (i.name.clone(), i.kind == ItemKind::View)).collect();
    let counts: Vec<Option<u64>> = futures_util::stream::iter(names.into_iter().map(|(name, view)| {
        let d = d.clone();
        async move {
            if view {
                return None;
            }
            let r = d.run_command(doc! { "count": name }).await.ok()?;
            number(r.get("n")).map(|n| n as u64)
        }
    }))
    .buffered(8)
    .collect()
    .await;
    for (i, n) in items.iter_mut().zip(counts) {
        i.count = n;
    }
    Ok(items)
}

fn number(b: Option<&Bson>) -> Option<f64> {
    match b? {
        Bson::Int32(n) => Some(*n as f64),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(f) => Some(*f),
        _ => None,
    }
}

// `singleBatch` is refused on views: what the first batch left is killed, so a page never
// leaves a cursor open on the server.
async fn close_cursor(d: &mongodb::Database, coll: &str, r: &Document) {
    if let Some(id) = r.get_document("cursor").ok().and_then(|c| c.get_i64("id").ok()).filter(|id| *id != 0) {
        let _ = d.run_command(doc! { "killCursors": coll, "cursors": [id] }).await;
    }
}

fn first_batch(r: &Document) -> Vec<Document> {
    r.get_document("cursor")
        .and_then(|c| c.get_array("firstBatch"))
        .map(|a| a.iter().filter_map(Bson::as_document).cloned().collect())
        .unwrap_or_default()
}

fn docs_of(found: Vec<Document>, page: Page, source: Option<Source>, total: Option<u64>) -> Docs {
    let more = found.len() > page.limit;
    let docs = found
        .into_iter()
        .take(page.limit)
        .map(|d| Doc { id: d.get("_id").cloned().map(Body::Bson), index: None, body: Body::doc(d) })
        .collect();
    Docs { docs, source, total, skip: page.skip, more }
}

fn cmd_err(e: mongodb::error::Error) -> anyhow::Error {
    anyhow!(message(&e.into()))
}

pub(crate) async fn run(c: &Client, db: &str, op: &Op, page: Page) -> Result<Output> {
    let d = c.client.database(db);
    Ok(match op {
        Op::Use(name) => Output::Use(name.clone()),
        Op::ShowDbs => {
            let names: Vec<Bson> = databases(c).await?.into_iter().map(|i| Bson::String(i.name)).collect();
            Output::Value(Body::Bson(Bson::Array(names)))
        }
        Op::ShowCollections => {
            let names: Vec<Bson> = collections(c, db).await?.into_iter().map(|i| Bson::String(i.name)).collect();
            Output::Value(Body::Bson(Bson::Array(names)))
        }
        Op::Find { coll, filter, projection, sort, skip, limit, one, options } => {
            let left = limit.map(|l| (l.unsigned_abs()).saturating_sub(page.skip));
            if left == Some(0) && !one {
                let source = projection.is_none().then(|| Source::Mongo { db: db.into(), coll: coll.clone() });
                return Ok(Output::Docs(Docs { docs: Vec::new(), source, total: None, skip: page.skip, more: false }));
            }
            let want = match left {
                Some(l) => (l as usize).min(page.limit),
                None => page.limit,
            };
            let mut cmd = doc! { "find": coll.clone(), "filter": filter.clone() };
            if let Some(p) = projection {
                cmd.insert("projection", p.clone());
            }
            if let Some(s) = sort {
                cmd.insert("sort", s.clone());
            }
            let first = skip + page.skip;
            if first > 0 {
                cmd.insert("skip", first as i64);
            }
            let fetch = if left.is_some_and(|l| l as usize <= page.limit) { want } else { want + 1 };
            cmd.insert("limit", fetch as i64);
            cmd.insert("batchSize", fetch as i64);
            cmd.extend(options.clone());
            let count = async {
                if page.skip > 0 || *one {
                    return None;
                }
                let mut q = doc! { "count": coll.clone(), "query": filter.clone(), "maxTimeMS": 3000 };
                for k in ["collation", "hint"] {
                    if let Some(v) = options.get(k) {
                        q.insert(k, v.clone());
                    }
                }
                if *skip > 0 {
                    q.insert("skip", *skip as i64);
                }
                if let Some(l) = limit {
                    q.insert("limit", l.abs());
                }
                d.run_command(q).await.ok().and_then(|r| number(r.get("n"))).map(|n| n as u64)
            };
            let (found, total) = futures_util::join!(std::future::IntoFuture::into_future(d.run_command(cmd)), count);
            let found = found.map_err(cmd_err)?;
            close_cursor(&d, coll, &found).await;
            let found = first_batch(&found);
            let page = Page { skip: page.skip, limit: want };
            // A projection's documents are not whole: they are shown, not saved back.
            let source = projection.is_none().then(|| Source::Mongo { db: db.into(), coll: coll.clone() });
            Output::Docs(docs_of(found, page, source, total))
        }
        Op::Aggregate { coll, pipeline } => {
            let mut stages = pipeline.clone();
            let writes = pipeline.last().is_some_and(|s| s.contains_key("$out") || s.contains_key("$merge"));
            if page.skip > 0 && !writes {
                stages.push(doc! { "$skip": page.skip as i64 });
            }
            if !writes {
                stages.push(doc! { "$limit": page.limit as i64 + 1 });
            }
            let target: Bson = match coll {
                Some(c) => Bson::String(c.clone()),
                None => Bson::Int32(1),
            };
            // `$currentOp` (and the cluster's own stages) run on admin.
            let on = if coll.is_none() && pipeline.first().is_some_and(|s| s.contains_key("$currentOp") || s.contains_key("$listSessions")) { c.client.database("admin") } else { d.clone() };
            let cmd = doc! { "aggregate": target, "pipeline": stages, "cursor": { "batchSize": page.limit as i64 + 1 } };
            let r = on.run_command(cmd).await.map_err(cmd_err)?;
            Output::Docs(docs_of(first_batch(&r), page, None, None))
        }
        Op::Count { coll, filter } => {
            let n = match filter {
                Some(f) => d.collection::<Document>(coll).count_documents(f.clone()).await.map_err(cmd_err)?,
                None => d.collection::<Document>(coll).estimated_document_count().await.map_err(cmd_err)?,
            };
            Output::Value(Body::Bson(Bson::Int64(n as i64)))
        }
        Op::Distinct { coll, field, filter } => {
            let v = d.collection::<Document>(coll).distinct(field, filter.clone()).await.map_err(cmd_err)?;
            Output::Value(Body::Bson(Bson::Array(v)))
        }
        Op::Insert { coll, docs } => {
            let r = d.collection::<Document>(coll).insert_many(docs.clone()).await.map_err(cmd_err)?;
            let mut ids: Vec<(usize, Bson)> = r.inserted_ids.into_iter().collect();
            ids.sort_by_key(|(i, _)| *i);
            let n = ids.len() as u64;
            let detail = Bson::Document(doc! { "insertedIds": ids.into_iter().map(|(_, id)| id).collect::<Vec<_>>() });
            Output::Written { affected: n, summary: plural(n, "document", "inserted"), detail: Some(Body::Bson(detail)) }
        }
        Op::Update { coll, filter, update, many, upsert, options } => {
            let r = update_cmd(&d, coll, filter, update.clone(), *many, *upsert, options).await?;
            let summary = format!("{} matched, {} modified{}", r.matched, r.modified, if r.upserted > 0 { ", 1 upserted" } else { "" });
            let mut detail = doc! { "matchedCount": r.matched as i64, "modifiedCount": r.modified as i64 };
            if let Some(id) = r.upserted_id {
                detail.insert("upsertedId", id);
            }
            Output::Written { affected: r.modified + r.upserted, summary, detail: Some(Body::doc(detail)) }
        }
        Op::Replace { coll, filter, doc, upsert, options } => {
            let r = update_cmd(&d, coll, filter, Bson::Document(doc.clone()), false, *upsert, options).await?;
            let mut detail = doc! { "matchedCount": r.matched as i64, "modifiedCount": r.modified as i64 };
            if let Some(id) = r.upserted_id {
                detail.insert("upsertedId", id);
            }
            Output::Written { affected: r.modified + r.upserted, summary: format!("{} matched, {} replaced", r.matched, r.modified), detail: Some(Body::doc(detail)) }
        }
        Op::Delete { coll, filter, many, options } => {
            let mut del = doc! { "q": filter.clone(), "limit": if *many { 0 } else { 1 } };
            let (stmt, top) = split_options(options);
            del.extend(stmt);
            let mut cmd = doc! { "delete": coll.clone(), "deletes": [del] };
            cmd.extend(top);
            let n = write_cmd(&d, cmd).await?.get("n").and_then(|n| number(Some(n))).unwrap_or(0.) as u64;
            Output::Written { affected: n, summary: plural(n, "document", "deleted"), detail: None }
        }
        Op::FindAndModify { coll, filter, update, options } => {
            let mut cmd = doc! { "findAndModify": coll.clone(), "query": filter.clone() };
            match update {
                Some(u) => {
                    cmd.insert("update", u.clone());
                }
                None => {
                    cmd.insert("remove", true);
                }
            }
            for (k, v) in options {
                match k.as_str() {
                    "returnDocument" => {
                        cmd.insert("new", v.as_str() == Some("after"));
                    }
                    "returnNewDocument" => {
                        cmd.insert("new", v.clone());
                    }
                    "projection" => {
                        cmd.insert("fields", v.clone());
                    }
                    _ => {
                        cmd.insert(k.clone(), v.clone());
                    }
                }
            }
            let r = write_cmd(&d, cmd).await?;
            let n = r.get_document("lastErrorObject").ok().and_then(|l| number(l.get("n"))).unwrap_or(0.) as u64;
            let verb = match update {
                None => "deleted",
                Some(Bson::Document(u)) if !u.keys().any(|k| k.starts_with('$')) => "replaced",
                Some(_) => "updated",
            };
            let value = r.get("value").cloned().filter(|v| *v != Bson::Null);
            Output::Written { affected: n, summary: if value.is_some() { plural(n.max(1), "document", verb) } else { "no document matched".into() }, detail: value.map(Body::Bson) }
        }
        Op::BulkWrite { coll, ops } => {
            let (mut inserted, mut matched, mut modified, mut deleted, mut upserted) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for (i, o) in ops.iter().enumerate() {
                let (kind, spec) = o.iter().next().map(|(k, v)| (k.as_str(), v.as_document().cloned().unwrap_or_default())).unwrap_or(("", Document::new()));
                let at = |e: anyhow::Error| anyhow!("operation {} ({kind}): {e}", i + 1);
                let filter = spec.get_document("filter").cloned().unwrap_or_default();
                let mut opts = spec.clone();
                for k in ["filter", "update", "replacement", "document", "upsert"] {
                    opts.remove(k);
                }
                let upsert = spec.get_bool("upsert").unwrap_or(false);
                match kind {
                    "insertOne" => {
                        let docs = vec![spec.get_document("document").cloned().map_err(|_| anyhow!("operation {}: insertOne needs its document", i + 1))?];
                        write_cmd(&d, doc! { "insert": coll.clone(), "documents": docs }).await.map_err(at)?;
                        inserted += 1;
                    }
                    "updateOne" | "updateMany" | "replaceOne" => {
                        let u = spec.get("update").or(spec.get("replacement")).cloned().unwrap_or(Bson::Document(Document::new()));
                        let r = update_cmd(&d, coll, &filter, u, kind == "updateMany", upsert, &opts).await.map_err(at)?;
                        (matched, modified, upserted) = (matched + r.matched, modified + r.modified, upserted + r.upserted);
                    }
                    "deleteOne" | "deleteMany" => {
                        let mut del = doc! { "q": filter, "limit": if kind == "deleteMany" { 0 } else { 1 } };
                        del.extend(opts);
                        let n = write_cmd(&d, doc! { "delete": coll.clone(), "deletes": [del] }).await.map_err(at)?.get("n").and_then(|n| number(Some(n))).unwrap_or(0.) as u64;
                        deleted += n;
                    }
                    _ => {}
                }
            }
            let summary = format!("{inserted} inserted, {matched} matched, {modified} modified, {deleted} deleted{}", if upserted > 0 { format!(", {upserted} upserted") } else { String::new() });
            let detail = doc! {
                "insertedCount": inserted as i64, "matchedCount": matched as i64, "modifiedCount": modified as i64,
                "deletedCount": deleted as i64, "upsertedCount": upserted as i64,
            };
            Output::Written { affected: inserted + modified + deleted + upserted, summary, detail: Some(Body::doc(detail)) }
        }
        Op::CreateIndex { coll, keys, options } => {
            let mut index = options.clone();
            index.insert("key", keys.clone());
            if !index.contains_key("name") {
                let name: Vec<String> = keys.iter().map(|(k, v)| format!("{k}_{}", crate::nosql::shell::scalar_text(v).trim_matches('"'))).collect();
                index.insert("name", name.join("_"));
            }
            let name = index.get_str("name").unwrap_or("").to_string();
            d.run_command(doc! { "createIndexes": coll.clone(), "indexes": [index] }).await.map_err(cmd_err)?;
            Output::Written { affected: 0, summary: format!("index {name} created"), detail: None }
        }
        Op::DropIndex { coll, index } => {
            d.run_command(doc! { "dropIndexes": coll.clone(), "index": index.clone() }).await.map_err(cmd_err)?;
            Output::Written { affected: 0, summary: "index dropped".into(), detail: None }
        }
        Op::Indexes { coll } => {
            let r = d.run_command(doc! { "listIndexes": coll.clone() }).await.map_err(cmd_err)?;
            Output::Value(Body::Bson(Bson::Array(first_batch(&r).into_iter().map(Bson::Document).collect())))
        }
        Op::DropCollection { coll } => {
            d.collection::<Document>(coll).drop().await.map_err(cmd_err)?;
            Output::Written { affected: 0, summary: format!("{coll} dropped"), detail: None }
        }
        Op::CreateCollection { name, options } => {
            let mut cmd = doc! { "create": name.clone() };
            cmd.extend(options.clone());
            d.run_command(cmd).await.map_err(cmd_err)?;
            Output::Written { affected: 0, summary: format!("{name} created"), detail: None }
        }
        Op::RenameCollection { coll, to } => {
            c.client.database("admin").run_command(doc! { "renameCollection": format!("{db}.{coll}"), "to": format!("{db}.{to}") }).await.map_err(cmd_err)?;
            Output::Written { affected: 0, summary: format!("{coll} renamed to {to}"), detail: None }
        }
        Op::DropDatabase => {
            d.drop().await.map_err(cmd_err)?;
            Output::Written { affected: 0, summary: format!("{db} dropped"), detail: None }
        }
        Op::Stats { coll } => {
            let cmd = match coll {
                Some(c) => doc! { "collStats": c.clone() },
                None => doc! { "dbStats": 1 },
            };
            Output::Value(Body::doc(d.run_command(cmd).await.map_err(cmd_err)?))
        }
        Op::Command { doc, admin } => {
            let target = if *admin { c.client.database("admin") } else { d };
            let r = target.run_command(doc.clone()).await.map_err(cmd_err)?;
            if r.get_document("cursor").is_ok_and(|c| c.contains_key("firstBatch")) && op.class() == WriteClass::Read {
                let found = first_batch(&r);
                let n = found.len();
                Output::Docs(docs_of(found, Page { skip: 0, limit: n }, None, None))
            } else {
                Output::Value(Body::doc(r))
            }
        }
    })
}

pub(crate) async fn command(c: &Client, db: &str, cmd: Document, admin: bool) -> Result<Document> {
    let target = c.client.database(if admin { "admin" } else { db });
    target.run_command(cmd).await.map_err(cmd_err)
}

pub(crate) async fn affects(c: &Client, db: &str, op: &Op) -> Result<Option<u64>> {
    let col = |coll: &str| c.client.database(db).collection::<Document>(coll);
    Ok(match op {
        Op::Insert { docs, .. } => Some(docs.len() as u64),
        Op::Update { coll, filter, many: true, .. } | Op::Delete { coll, filter, many: true, .. } => {
            Some(col(coll).count_documents(filter.clone()).await.map_err(cmd_err)?)
        }
        Op::Update { coll, filter, many: false, upsert, .. } | Op::Replace { coll, filter, upsert, .. } => {
            let n = col(coll).count_documents(filter.clone()).limit(1).await.map_err(cmd_err)?;
            Some(if n == 0 && *upsert { 1 } else { n })
        }
        Op::Delete { coll, filter, many: false, .. } => Some(col(coll).count_documents(filter.clone()).limit(1).await.map_err(cmd_err)?),
        Op::FindAndModify { coll, filter, .. } => Some(col(coll).count_documents(filter.clone()).limit(1).await.map_err(cmd_err)?),
        Op::BulkWrite { coll, ops } => {
            let mut n = 0;
            for o in ops {
                let Some((kind, spec)) = o.iter().next().map(|(k, v)| (k.as_str(), v.as_document().cloned().unwrap_or_default())) else { continue };
                let filter = spec.get_document("filter").cloned().unwrap_or_default();
                n += match kind {
                    "insertOne" => 1,
                    "updateMany" | "deleteMany" => col(coll).count_documents(filter).await.map_err(cmd_err)?,
                    _ => col(coll).count_documents(filter).limit(1).await.map_err(cmd_err)?.max(u64::from(spec.get_bool("upsert").unwrap_or(false))),
                };
            }
            Some(n)
        }
        _ => None,
    })
}

async fn write_cmd(d: &mongodb::Database, cmd: Document) -> Result<Document> {
    let r = d.run_command(cmd).await.map_err(cmd_err)?;
    if let Ok(errs) = r.get_array("writeErrors")
        && let Some(e) = errs.first().and_then(Bson::as_document)
    {
        return Err(anyhow!("{}", e.get_str("errmsg").unwrap_or("write error")));
    }
    if let Ok(w) = r.get_document("writeConcernError") {
        return Err(anyhow!("{}", w.get_str("errmsg").unwrap_or("write concern error")));
    }
    Ok(r)
}

fn split_options(options: &Document) -> (Document, Document) {
    let (mut stmt, mut top) = (Document::new(), Document::new());
    for (k, v) in options {
        if matches!(k.as_str(), "let" | "comment" | "writeConcern" | "bypassDocumentValidation" | "ordered") {
            top.insert(k.clone(), v.clone());
        } else {
            stmt.insert(k.clone(), v.clone());
        }
    }
    (stmt, top)
}

struct Updated {
    matched: u64,
    modified: u64,
    upserted: u64,
    upserted_id: Option<Bson>,
}

async fn update_cmd(d: &mongodb::Database, coll: &str, filter: &Document, u: Bson, many: bool, upsert: bool, options: &Document) -> Result<Updated> {
    let mut stmt = doc! { "q": filter.clone(), "u": u, "multi": many, "upsert": upsert };
    let (opts, top) = split_options(options);
    stmt.extend(opts);
    let mut cmd = doc! { "update": coll, "updates": [stmt] };
    cmd.extend(top);
    let r = write_cmd(d, cmd).await?;
    let ups = r.get_array("upserted").map(|a| a.len() as u64).unwrap_or(0);
    let upserted_id = r.get_array("upserted").ok().and_then(|a| a.first()).and_then(Bson::as_document).and_then(|u| u.get("_id").cloned());
    let n = number(r.get("n")).unwrap_or(0.) as u64;
    Ok(Updated { matched: n.saturating_sub(ups), modified: number(r.get("nModified")).unwrap_or(0.) as u64, upserted: ups, upserted_id })
}

fn plural(n: u64, noun: &str, verb: &str) -> String {
    format!("{n} {noun}{} {verb}", if n == 1 { "" } else { "s" })
}

pub(crate) async fn save(c: &Client, db: &str, coll: &str, id: Option<Bson>, mut doc: Document) -> Result<Bson> {
    let col = c.client.database(db).collection::<Document>(coll);
    match id {
        None => {
            let r = col.insert_one(doc).await.map_err(cmd_err)?;
            Ok(r.inserted_id)
        }
        Some(id) => {
            match doc.get("_id") {
                Some(new) if *new != id => return Err(anyhow!("_id cannot change (save it as a new document instead)")),
                Some(_) => {}
                None => {
                    let mut with = Document::new();
                    with.insert("_id", id.clone());
                    with.extend(doc);
                    doc = with;
                }
            }
            let r = col.replace_one(doc! { "_id": id.clone() }, doc).await.map_err(cmd_err)?;
            if r.matched_count == 0 {
                return Err(anyhow!("the document is gone (deleted elsewhere): nothing was saved"));
            }
            Ok(id)
        }
    }
}

pub(crate) async fn delete(c: &Client, db: &str, coll: &str, id: Bson) -> Result<u64> {
    let r = c.client.database(db).collection::<Document>(coll).delete_one(doc! { "_id": id }).await.map_err(cmd_err)?;
    Ok(r.deleted_count)
}

pub(crate) async fn sample_fields(c: &Client, db: &str, coll: &str, n: i64) -> Result<Vec<(String, Vec<&'static str>)>> {
    let r = c.client.database(db).run_command(doc! { "aggregate": coll, "pipeline": [{ "$sample": { "size": n } }], "cursor": {} }).await.map_err(cmd_err)?;
    let mut seen: Vec<(String, usize, Vec<&'static str>)> = Vec::new();
    fn walk(prefix: &str, d: &Document, seen: &mut Vec<(String, usize, Vec<&'static str>)>, depth: usize) {
        for (k, v) in d {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            let t = crate::nosql::shell::type_name(v);
            match seen.iter_mut().find(|(p, _, _)| *p == path) {
                Some((_, n, types)) => {
                    *n += 1;
                    if !types.contains(&t) {
                        types.push(t);
                    }
                }
                None => seen.push((path.clone(), 1, vec![t])),
            }
            if depth < 4 {
                match v {
                    Bson::Document(sub) => walk(&path, sub, seen, depth + 1),
                    Bson::Array(a) => {
                        if let Some(Bson::Document(sub)) = a.iter().find(|x| matches!(x, Bson::Document(_))) {
                            walk(&path, sub, seen, depth + 1);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    for d in first_batch(&r) {
        walk("", &d, &mut seen, 0);
    }
    seen.sort_by(|a, b| (a.0 != "_id", std::cmp::Reverse(a.1), &a.0).cmp(&(b.0 != "_id", std::cmp::Reverse(b.1), &b.0)));
    Ok(seen.into_iter().map(|(p, _, t)| (p, t)).collect())
}
