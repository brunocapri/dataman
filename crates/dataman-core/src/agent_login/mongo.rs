//! MongoDB: a custom role and a user in `admin`, the role's privileges per collection; a
//! collection with hidden or masked fields is read through a view next to it (`$project` drops
//! the hidden fields, `$set` masks the others). The role never gets `listCollections`, so
//! collections it has no privilege on stay out of `listCollections` with `authorizedCollections`.
//!
//! Views are read-only: writes go to the collection itself, granted without `find`, so an
//! update's filter could still probe a hidden field (Dataman refuses that). Server-side
//! JavaScript can't be turned off per user, nor time limited. MongoDB Atlas manages users only
//! through its own API: there Dataman gives the role to paste.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::*;
use crate::backend::mongo::{self as backend, Client};
use crate::nosql::bson::{Bson, Document};

const SYSTEM_DBS: &[&str] = &["admin", "local", "config"];

/// Dataman's view of `coll` (views live in the collection's database).
pub fn view_coll(space: &str, coll: &str) -> String {
    format!("{space}.{coll}")
}

fn present(f: &str) -> Value {
    json!({ "$eq": [{ "$type": format!("${f}") }, "missing"] })
}

pub fn mask_expr(f: &str, m: Mask) -> Value {
    let v = format!("${f}");
    let masked = match m {
        // No hash in an aggregation: hidden instead (Dataman still hashes on top).
        Mask::Hidden | Mask::Hash => json!("‹hidden›"),
        Mask::Partial => json!({ "$let": {
            "vars": { "s": { "$convert": { "input": v, "to": "string", "onError": "", "onNull": "" } } },
            "in": { "$cond": [
                { "$gt": [{ "$indexOfCP": ["$$s", "@"] }, 0] },
                { "$concat": [{ "$substrCP": ["$$s", 0, 1] }, "•••@", { "$arrayElemAt": [{ "$split": ["$$s", "@"] }, 1] }] },
                { "$cond": [{ "$gte": [{ "$strLenCP": "$$s" }, 8] }, { "$concat": ["•••• ", { "$substrCP": ["$$s", { "$subtract": [{ "$strLenCP": "$$s" }, 4] }, 4] }] }, "••••"] }
            ] }
        } }),
    };
    json!({ "$cond": [present(f), "$$REMOVE", { "$cond": [{ "$eq": [v, null] }, null, masked] }] })
}

pub struct Desired {
    pub privileges: Vec<Value>,
    pub views: Vec<(String, String, Value, String, ViewMap)>,
    pub access: Vec<Access>,
    pub hidden: Vec<String>,
    pub lines: Vec<String>,
}

pub fn desired(spec: &Spec, inv: &Inventory) -> Desired {
    let mut inv = inv.clone();
    with_rule_fields(spec, &mut inv);
    let (all, hidden) = access(spec, &inv);
    let (mut privileges, mut views, mut lines, mut ok) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut add = |db: &str, coll: &str, actions: &[&str], lines: &mut Vec<String>| {
        privileges.push(json!({ "resource": { "db": db, "collection": coll }, "actions": actions }));
        for a in actions {
            lines.push(format!("{a} ON {db}.{coll}"));
        }
    };
    for a in all {
        if !spec.rows {
            continue;
        }
        if a.whole {
            let mut acts = vec!["find", "listIndexes", "collStats"];
            if a.write {
                acts.extend(["insert", "update", "remove"]);
            }
            add(&a.schema, &a.name, &acts, &mut lines);
        } else {
            let view = view_coll(&spec.space, &a.name);
            let mut pipeline = Vec::new();
            if !a.hidden.is_empty() {
                let mut p = serde_json::Map::new();
                for h in &a.hidden {
                    p.insert(h.clone(), json!(0));
                }
                pipeline.push(json!({ "$project": p }));
            }
            if !a.masked.is_empty() {
                let mut s = serde_json::Map::new();
                for (c, m) in &a.masked {
                    s.insert(c.clone(), mask_expr(c, *m));
                }
                pipeline.push(json!({ "$set": s }));
            }
            let pipeline = Value::Array(pipeline);
            let h = pipeline_hash(&pipeline);
            add(&a.schema, &view, &["find"], &mut lines);
            if a.write {
                add(&a.schema, &a.name, &["insert", "update", "remove"], &mut lines);
            }
            lines.push(format!("VIEW {}.{view} -- {h}", a.schema));
            let map = ViewMap { schema: a.schema.clone(), table: a.name.clone(), view: view.clone(), masked: a.masked.clone() };
            views.push((a.schema.clone(), view, pipeline, h, map));
        }
        ok.push(a);
    }
    if spec.activity {
        let mut acts = vec!["inprog", "serverStatus"];
        if spec.kill {
            acts.push("killop");
        }
        privileges.push(json!({ "resource": { "cluster": true }, "actions": acts }));
        for a in acts {
            lines.push(format!("{a} ON cluster"));
        }
    }
    lines.sort();
    Desired { privileges, views, access: ok, hidden, lines }
}

/// A view's pipeline as the server keeps it (through BSON, so numbers read back alike), hashed.
pub fn pipeline_hash(p: &Value) -> String {
    let canon = crate::nosql::bson::to_bson(p).map(|b| b.into_relaxed_extjson()).unwrap_or_else(|_| p.clone());
    hash::sha256_hex(canon.to_string().as_bytes())[..12].to_string()
}

fn rules(spec: &Spec, d: &Desired, prod: bool) -> Vec<RuleLine> {
    let mut v = vec![line("A login apart from yours", "createUser with a custom role", Who::Database)];
    if !spec.rows {
        v.push(line("Structure only", "no collection granted", Who::Database));
    } else if spec.write {
        v.push(line("Documents: read and write, no DDL", "find, insert, update, remove only", Who::Database));
    } else {
        v.push(line("Documents: read only", "find only", Who::Database));
    }
    if !d.hidden.is_empty() {
        v.push(line(hidden_label(&d.hidden), "no privilege; listCollections shows only what it may use", Who::Database));
    }
    let writes = d.access.iter().any(|a| !a.whole && a.write);
    v.extend(column_lines(
        &d.access,
        (if writes { "views with $project; writes go to the collection, whose filters could probe" } else { "views with $project" }, Who::Partly),
        ("views with $set (Hash shows as hidden)", Who::Partly),
        spec.rows,
    ));
    v.push(line(if spec.code { "Scripts" } else { "Run code off" }, "$where and $function can't be turned off per user", Who::Dataman));
    v.push(line(if spec.activity { "Server activity" } else { "No server activity" }, if spec.activity { "inprog, serverStatus" } else { "no inprog" }, Who::Database));
    v.push(line(time_label(spec.time_limit_s), "no time limit per user", Who::Dataman));
    if spec.conn_limit.is_some() {
        v.push(line("Sessions at once", "no cap per user", Who::Dataman));
    }
    v.extend(dataman_lines(spec, prod));
    v
}

fn cmd(db: &str, group: &str, cmd: Value, shown: Option<Value>) -> Step {
    let text = |v: &Value| format!("db.getSiblingDB({}).runCommand({})", json!(db), v);
    Step { shown: text(shown.as_ref().unwrap_or(&cmd)), copy: text(&cmd), action: Action::Mongo { db: db.into(), cmd }, group: group.into() }
}

/// MongoDB Atlas: by its hosts.
pub fn atlas(p: &ConnectionProfile) -> bool {
    p.host.split(',').any(|h| h.trim().trim_end_matches('.').to_ascii_lowercase().ends_with(".mongodb.net"))
}

pub fn create_plan(spec: &Spec, inv: &Inventory, probe: &Probe, password: &str, prod: bool) -> Plan {
    let d = desired(spec, inv);
    let mut steps = vec![cmd("admin", "the role: what the agent may do", json!({ "createRole": spec.user, "privileges": d.privileges, "roles": [] }), None)];
    for (db, view, pipeline, _, map) in &d.views {
        steps.push(cmd(db, "hidden or masked fields: views the agent reads instead", json!({ "create": view, "viewOn": map.table, "pipeline": pipeline }), None));
    }
    let user = json!({ "createUser": spec.user, "pwd": password, "roles": [{ "role": spec.user, "db": "admin" }], "mechanisms": ["SCRAM-SHA-256"], "customData": { "dataman": MARK, "conn": spec.conn } });
    let mut shown = user.clone();
    shown["pwd"] = json!("<password from the Keychain>");
    steps.push(cmd("admin", "the login, last (MongoDB hashes the password; the copied script holds it)", user, Some(shown)));
    if !d.hidden.is_empty() {
        steps.push(Step::note("", format!("hidden: {} → no privilege at all", short_list(&d.hidden))));
    }
    let mut plan = seal(spec, d, probe, steps, prod);
    if probe.manual == Some(Manual::Atlas) {
        plan.manual = Some(Manual::Atlas);
    }
    plan
}

fn seal(spec: &Spec, d: Desired, probe: &Probe, steps: Vec<Step>, prod: bool) -> Plan {
    let rules = rules(spec, &d, prod);
    let made: Vec<String> = std::iter::once(format!("role:{}", spec.user)).chain(d.views.iter().map(|(db, v, ..)| format!("view:{db}.{v}"))).collect();
    let mut notes = Vec::new();
    if probe.manual == Some(Manual::Atlas) {
        notes.push("MongoDB Atlas manages users and roles only through Atlas (the UI, the Atlas CLI or its Admin API): copy the role and create it there, then Verify.".into());
    }
    Plan {
        engine: Some(Engine::MongoDb),
        user: spec.user.clone(),
        space: spec.space.clone(),
        steps,
        one_tx: false,
        rules,
        lines: d.lines,
        lacks: probe.lacks.clone(),
        notes,
        made,
        granted: d.access.len(),
        views: d.views.into_iter().map(|v| v.4).collect(),
        hidden: d.hidden,
        ..Default::default()
    }
    .seal()
}

/// The role's privileges replaced whole (atomic), and the views that changed made again.
pub fn sync_plan(spec: &Spec, inv: &Inventory, probe: &Probe, have_views: &[(String, String, String)], prod: bool) -> Plan {
    let d = desired(spec, inv);
    let mut steps = vec![cmd("admin", "the role's privileges, replaced whole", json!({ "updateRole": spec.user, "privileges": d.privileges }), None)];
    for (db, name, h) in have_views {
        if !d.views.iter().any(|(vdb, v, _, vh, _)| vdb == db && v == name && vh == h) {
            steps.push(cmd(db, "views", json!({ "drop": name }), None));
        }
    }
    for (db, view, pipeline, h, map) in &d.views {
        if !have_views.iter().any(|(hdb, n, hh)| hdb == db && n == view && hh == h) {
            steps.push(cmd(db, "views", json!({ "create": view, "viewOn": map.table, "pipeline": pipeline }), None));
        }
    }
    seal(spec, d, probe, steps, prod)
}

pub fn remove_plan(user: &str, views: &[(String, String)]) -> Plan {
    let mut steps = vec![cmd("admin", "the login", json!({ "dropUser": user }), None)];
    steps.push(cmd("admin", "its role", json!({ "dropRole": user }), None));
    for (db, v) in views {
        steps.push(cmd(db, "Dataman's views", json!({ "drop": v }), None));
    }
    Plan { engine: Some(Engine::MongoDb), user: user.into(), steps, ..Default::default() }
}

pub fn rotate_step(user: &str, password: &str) -> Step {
    let c = json!({ "updateUser": user, "pwd": password });
    let mut shown = c.clone();
    shown["pwd"] = json!("<password from the Keychain>");
    cmd("admin", "a new password", c, Some(shown))
}

/// The role and user as Atlas takes them (Admin API), for the user to create there.
pub fn atlas_text(spec: &Spec, inv: &Inventory) -> String {
    let d = desired(spec, inv);
    let actions: Vec<Value> = d
        .privileges
        .iter()
        .flat_map(|p| {
            let res = &p["resource"];
            p["actions"].as_array().cloned().unwrap_or_default().into_iter().map(move |a| {
                let action = a.as_str().unwrap_or("").to_string();
                let upper = camel_to_upper(&action);
                if res["cluster"] == json!(true) { json!({ "action": upper, "resources": [{ "cluster": true }] }) } else { json!({ "action": upper, "resources": [{ "db": res["db"], "collection": res["collection"] }] }) }
            })
        })
        .collect();
    let role = json!({ "roleName": spec.user, "actions": actions });
    format!(
        "# Atlas › Database Access › Custom Roles (or POST /api/atlas/v2/groups/{{projectId}}/customDBRoles/roles):\n{}\n\n# then a database user {} with that role, scoped to this cluster; its password: Dataman keeps it in the Keychain (Rotate password shows a new one to set there).\n",
        serde_json::to_string_pretty(&role).unwrap_or_default(),
        spec.user
    )
}

fn camel_to_upper(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

// ---- Live ----

pub(crate) fn doc(v: &Value) -> Result<Document> {
    crate::nosql::bson::to_document(v).map_err(|e| anyhow!("{e}"))
}

pub(crate) async fn run(c: &Client, db: &str, cmd: &Value) -> Result<Document> {
    backend::command(c, db, doc(cmd)?, false).await
}

pub(crate) async fn probe(c: &Client, p: &ConnectionProfile, user: &str) -> Result<Probe> {
    let st = run(c, "admin", &json!({ "connectionStatus": 1, "showPrivileges": true })).await?;
    let info: Value = Bson::Document(st).into_relaxed_extjson();
    let login = info["authInfo"]["authenticatedUsers"][0]["user"].as_str().unwrap_or("").to_string();
    let privs = info["authInfo"]["authenticatedUserPrivileges"].as_array().cloned().unwrap_or_default();
    let can = |action: &str| {
        privs.iter().any(|p| {
            let r = &p["resource"];
            let on_admin = r["anyResource"] == json!(true) || r["db"] == json!("admin") || r["db"] == json!("");
            on_admin && p["actions"].as_array().is_some_and(|a| a.iter().any(|x| x == action))
        })
    };
    let version = backend::version(c).await.unwrap_or_else(|_| "MongoDB".into());
    let mut probe = Probe { login: login.clone(), version, tls: c.tls_active, ..Default::default() };
    if atlas(p) {
        probe.manual = Some(Manual::Atlas);
        probe.checks.push((false, "MongoDB Atlas: users only through Atlas".into()));
        probe.lacks.push("Atlas creates users and roles only through its UI, CLI or Admin API, so Dataman can't create this login here".into());
        return Ok(probe);
    }
    let needed = ["createRole", "createUser", "grantRole", "dropRole", "dropUser"];
    let missing: Vec<&str> = needed.iter().copied().filter(|a| !can(a)).collect();
    probe.can_create = missing.is_empty();
    if !missing.is_empty() {
        let who = if login.is_empty() { "this connection" } else { login.as_str() };
        probe.lacks.push(format!("{who} lacks {} on admin (the userAdmin role has them)", missing.join(", ")));
    }
    probe.flavor.superuser = can("createRole");
    probe.flavor.activity = true;
    probe.checks.push((probe.can_create, if probe.can_create { format!("{login} can create users and roles") } else { format!("{} can't create users", if login.is_empty() { "this login" } else { &login }) }));
    probe.checks.push((probe.tls, if probe.tls { "TLS on".into() } else { "TLS off".into() }));
    probe.exists = exists(c, user, &p.id).await?;
    Ok(probe)
}

pub(crate) async fn exists(c: &Client, user: &str, conn: &str) -> Result<Option<bool>> {
    let r = run(c, "admin", &json!({ "usersInfo": { "user": user, "db": "admin" } })).await?;
    let v: Value = Bson::Document(r).into_relaxed_extjson();
    let Some(u) = v["users"].as_array().and_then(|a| a.first()) else {
        let roles = run(c, "admin", &json!({ "rolesInfo": { "role": user, "db": "admin" } })).await?;
        let rv: Value = Bson::Document(roles).into_relaxed_extjson();
        return Ok(rv["roles"].as_array().filter(|a| !a.is_empty()).map(|_| false));
    };
    Ok(Some(u["customData"]["dataman"] == MARK && u["customData"]["conn"] == conn))
}

/// Every collection of every database but MongoDB's own, with the fields a sample shows.
pub(crate) async fn inventory(c: &Client) -> Result<Inventory> {
    let mut out = Inventory::default();
    for d in backend::databases(c).await? {
        if SYSTEM_DBS.contains(&d.name.as_str()) {
            continue;
        }
        for coll in backend::collections(c, &d.name).await? {
            if coll.name.starts_with("system.") || coll.name.starts_with(PREFIX) {
                continue;
            }
            let view = coll.kind == crate::nosql::ItemKind::View;
            let fields = if view { Vec::new() } else { backend::sample_fields(c, &d.name, &coll.name, 200).await.unwrap_or_default() };
            out.objects.push(Obj {
                schema: d.name.clone(),
                name: coll.name.clone(),
                kind: if view { ObjKind::View } else { ObjKind::Collection },
                columns: fields.into_iter().map(|(f, _)| Col::new(&f)).filter(|c| c.name != "_id").collect(),
                grantable: true,
                owner: String::new(),
                sequences: Vec::new(),
            });
        }
    }
    Ok(out)
}

/// The role's privileges as lines, and Dataman's views with their pipelines' hashes.
pub(crate) async fn have(c: &Client, user: &str, space: &str) -> Result<(Vec<String>, Vec<(String, String, String)>)> {
    let r = run(c, "admin", &json!({ "rolesInfo": { "role": user, "db": "admin" }, "showPrivileges": true })).await?;
    let v: Value = Bson::Document(r).into_relaxed_extjson();
    let mut lines = Vec::new();
    for p in v["roles"][0]["privileges"].as_array().cloned().unwrap_or_default() {
        let res = &p["resource"];
        let on = if res["cluster"] == json!(true) { "cluster".to_string() } else { format!("{}.{}", res["db"].as_str().unwrap_or(""), res["collection"].as_str().unwrap_or("")) };
        for a in p["actions"].as_array().cloned().unwrap_or_default() {
            lines.push(format!("{} ON {on}", a.as_str().unwrap_or("")));
        }
    }
    let mut views = Vec::new();
    for d in backend::databases(c).await? {
        if SYSTEM_DBS.contains(&d.name.as_str()) {
            continue;
        }
        let r = run(c, &d.name, &json!({ "listCollections": 1, "filter": { "type": "view", "name": { "$regex": format!("^{}\\.", regex::escape(space)) } } })).await?;
        let v: Value = Bson::Document(r).into_relaxed_extjson();
        for coll in v["cursor"]["firstBatch"].as_array().cloned().unwrap_or_default() {
            let name = coll["name"].as_str().unwrap_or("").to_string();
            let h = pipeline_hash(&coll["options"]["pipeline"]);
            lines.push(format!("VIEW {}.{name} -- {h}", d.name));
            views.push((d.name.clone(), name, h));
        }
    }
    lines.sort();
    Ok((lines, views))
}
