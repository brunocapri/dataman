//! The agent login against a server: what the user's login can do, the plan over what exists
//! now, running it (one transaction where the engine has them), logging in as the agent to check
//! it, what changed since (drift), a new password, and removing it. Every function here runs on
//! the user's own connection, but [`verify`], which runs on the agent's.

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::Utc;
use serde_json::{Value, json};

use super::*;
use crate::backend::{Db, text_rows};
use crate::conn::Connection;
use crate::nosql::Client;

/// What the review sheet shows, and what Create runs.
#[derive(Clone, Debug)]
pub struct Review {
    pub probe: Probe,
    pub spec: Spec,
    pub inventory: Inventory,
    pub plan: Plan,
    /// The new password: kept by the caller in the Keychain once the login exists.
    pub password: String,
    /// MongoDB Atlas: the role to paste there.
    pub atlas: Option<String>,
    pub persist: Option<redis::Persist>,
}

fn nosql(c: &Connection) -> Result<&Client> {
    c.nosql.as_ref().ok_or_else(|| anyhow!("not a document or key-value connection"))
}

async fn admin_db(c: &Connection, database: Option<&str>) -> Result<Db> {
    Ok(c.dedicated(database).await?.db())
}

fn flavor_db(spec: &Spec) -> Option<&str> {
    match spec.engine {
        Engine::Postgres | Engine::SqlServer => Some(spec.database.as_str()),
        _ => None,
    }
}

/// What the user's login can do here, and whether the login already exists.
pub async fn probe(c: &Arc<Connection>, p: &ConnectionProfile) -> Result<(Probe, Option<redis::Persist>)> {
    let user = login_name(&p.id);
    Ok(match p.engine {
        Engine::Postgres => (pg::probe(&admin_db(c, Some(&p.database)).await?, &user, &p.id).await?, None),
        Engine::MySql => (my::probe(&admin_db(c, None).await?, &user, &p.id).await?, None),
        Engine::SqlServer => (ms::probe(&admin_db(c, Some(&p.database)).await?, &user, &p.id).await?, None),
        Engine::MongoDb => match nosql(c)? {
            Client::Mongo(m) => (mongo::probe(m, p, &user).await?, None),
            _ => bail!("not MongoDB"),
        },
        Engine::Redis => match nosql(c)? {
            Client::Redis(r) => {
                let (pr, persist) = redis::probe(r, &user, &p.id).await?;
                (pr, Some(persist))
            }
            _ => bail!("not Redis"),
        },
        Engine::Elasticsearch | Engine::OpenSearch => match nosql(c)? {
            Client::Search(s) => (search::probe(s, &user, &p.id).await?, None),
            _ => bail!("not a search server"),
        },
        Engine::Sqlite | Engine::DuckDb => bail!("{FILE_NOTE}"),
    })
}

pub async fn inventory(c: &Arc<Connection>, spec: &Spec, probe: &Probe) -> Result<Inventory> {
    Ok(match spec.engine {
        Engine::Postgres => pg::inventory(&admin_db(c, Some(&spec.database)).await?).await?,
        Engine::MySql => my::inventory(&admin_db(c, None).await?, &probe.login).await?,
        Engine::SqlServer => ms::inventory(&admin_db(c, Some(&spec.database)).await?).await?,
        Engine::MongoDb => match nosql(c)? {
            Client::Mongo(m) => mongo::inventory(m).await?,
            _ => bail!("not MongoDB"),
        },
        Engine::Elasticsearch | Engine::OpenSearch => match nosql(c)? {
            Client::Search(s) => search::inventory(s).await?,
            _ => bail!("not a search server"),
        },
        _ => Inventory::default(),
    })
}

/// The review before Create: the probe, then the plan over what exists now.
pub async fn review(c: &Arc<Connection>, p: &ConnectionProfile, o: &Options) -> Result<Review> {
    let (probe, persist) = probe(c, p).await?;
    let spec = compile(p, "", o);
    let inventory = if probe.manual.is_none() || probe.manual == Some(Manual::Atlas) { inventory(c, &spec, &probe).await? } else { Inventory::default() };
    let password = hash::password();
    let prod = p.env.is_production();
    let mut plan = create_plan(&spec, &inventory, &probe, persist, &password, prod);
    plan.notes.extend(probe.notes.iter().cloned());
    if probe.exists == Some(false) {
        plan.lacks.push(format!("a login named {} already exists and isn't Dataman's: Dataman never changes it. Drop or rename it to create the agent login", spec.user));
    } else if probe.exists == Some(true) {
        plan.lacks.push(format!("{} already exists (Dataman made it): Verify picks it up, or Remove it first", spec.user));
    }
    let atlas = (probe.manual == Some(Manual::Atlas)).then(|| mongo::atlas_text(&spec, &inventory));
    Ok(Review { probe, spec, inventory, plan, password, atlas, persist })
}

pub fn create_plan(spec: &Spec, inv: &Inventory, probe: &Probe, persist: Option<redis::Persist>, password: &str, prod: bool) -> Plan {
    match spec.engine {
        Engine::Postgres => pg::create_plan(spec, inv, probe, password, prod),
        Engine::MySql => my::create_plan(spec, inv, probe, password, prod),
        Engine::SqlServer => ms::create_plan(spec, inv, probe, password, prod),
        Engine::MongoDb => mongo::create_plan(spec, inv, probe, password, prod),
        Engine::Redis => redis::create_plan(spec, probe, persist.unwrap_or(redis::Persist::None), password, prod),
        Engine::Elasticsearch | Engine::OpenSearch => search::create_plan(spec, inv, probe, password, prod),
        Engine::Sqlite | Engine::DuckDb => Plan::default(),
    }
}

/// Runs a plan's steps on the user's login: in one transaction where it says so (rolled back
/// whole on the first error), else in order, stopping at the first error.
pub async fn apply(c: &Arc<Connection>, plan: &Plan, database: &str) -> Result<()> {
    let engine = plan.engine.unwrap_or(c.engine());
    if engine.is_sql() {
        let db = admin_db(c, if matches!(engine, Engine::Postgres | Engine::SqlServer) && !database.is_empty() { Some(database) } else { None }).await?;
        let (begin, commit, rollback) = match engine {
            Engine::SqlServer => ("BEGIN TRANSACTION", "COMMIT TRANSACTION", "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION"),
            _ => ("BEGIN", "COMMIT", "ROLLBACK"),
        };
        let tx = plan.one_tx;
        if tx {
            db.execute(begin).await.map_err(|e| anyhow!("{e}"))?;
        }
        for s in plan.steps_run() {
            let Action::Sql(sql) = &s.action else { continue };
            if let Err(e) = db.execute(sql).await {
                if tx {
                    let _ = db.execute(rollback).await;
                }
                bail!("{e}\n  in: {}{}", s.shown, if tx { "\n  nothing was changed (rolled back)" } else { "" });
            }
        }
        if tx {
            db.execute(commit).await.map_err(|e| anyhow!("{e}"))?;
        }
        return Ok(());
    }
    let client = nosql(c)?;
    for s in plan.steps_run() {
        let r: Result<()> = match (&s.action, client) {
            (Action::Mongo { db, cmd }, Client::Mongo(m)) => mongo::run(m, db, cmd).await.map(|_| ()),
            (Action::Redis(a), Client::Redis(r)) => redis::call(r, a).await.map(|_| ()),
            (Action::Http { method, path, body }, Client::Search(x)) => search::call(x, method, path, body.as_ref()).await.map(|_| ()),
            _ => Err(anyhow!("this step is not for {}", engine.label())),
        };
        r.with_context(|| format!("in: {}", s.shown.lines().next().unwrap_or("")))?;
    }
    Ok(())
}

/// What the profile keeps once the plan ran.
pub fn record(p: &ConnectionProfile, plan: &Plan, o: &Options) -> AgentLogin {
    let now = Utc::now();
    AgentLogin {
        user: plan.user.clone(),
        server: server_of(p),
        created: now,
        synced: now,
        verified: None,
        rules: rules_hash(p, ""),
        plan: plan.hash.clone(),
        rotate_days: o.rotate_days,
        rotated: Some(now),
        conn_limit: o.conn_limit,
        database: p.database.clone(),
        made: plan.made.clone(),
        views: plan.views.clone(),
        granted: plan.granted,
        not_granted: plan.not_granted.len(),
    }
}

/// The profile agents connect with: the agent login, its password given by the caller (never
/// the user's, nor a `.pgpass` line).
pub fn agent_profile(p: &ConnectionProfile, user: &str) -> ConnectionProfile {
    let mut a = p.clone();
    a.user = user.to_string();
    a.save_password = false;
    // Never a pgpass line: the password is the Keychain's.
    a.agent_made = true;
    if p.engine == Engine::MongoDb {
        a.options.insert("authSource".into(), "admin".into());
        a.options.remove("authMechanism");
        a.options.remove("authMechanismProperties");
    }
    a
}

/// One check of Verify: what was tried as the agent, whether it went as the rules say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub what: String,
    pub ok: bool,
    pub detail: String,
}

fn check(what: impl Into<String>, ok: bool, detail: impl Into<String>) -> Check {
    Check { what: what.into(), ok, detail: detail.into() }
}

/// A permission error, as each engine says it (a hidden object is "not found" on some).
fn refused(engine: Engine, code: Option<&str>, msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    match engine {
        Engine::Postgres => code == Some("42501") || m.contains("permission denied"),
        Engine::MySql => matches!(code, Some("1142" | "1143" | "1044" | "1045" | "1227" | "1370" | "1356" | "1146")) || m.contains("denied"),
        Engine::SqlServer => matches!(code, Some("229" | "230" | "262" | "297" | "300" | "916" | "208" | "4701" | "15247")) || m.contains("permission was denied") || m.contains("invalid object name"),
        _ => m.contains("unauthorized") || m.contains("not authorized") || m.contains("noperm") || m.contains("security_exception") || m.contains("403"),
    }
}

/// Logs in as the agent (`agent`, opened with its password) and tries what the rules allow and
/// what they don't, on the server itself: the identity, an allowed read, a hidden object and
/// column, a masked column on the table, a write on a read tier, structure.
pub async fn verify(agent: &Arc<Connection>, spec: &Spec, inv: &Inventory, probe: &Probe) -> Result<Vec<Check>> {
    let mut out = Vec::new();
    match spec.engine {
        Engine::Postgres | Engine::MySql | Engine::SqlServer => verify_sql(agent, spec, inv, probe, &mut out).await?,
        Engine::MongoDb => verify_mongo(agent, spec, inv, &mut out).await?,
        Engine::Redis => verify_redis(agent, spec, &mut out).await?,
        Engine::Elasticsearch | Engine::OpenSearch => verify_search(agent, spec, inv, probe, &mut out).await?,
        _ => {}
    }
    Ok(out)
}

fn sql_q(e: Engine, n: &str) -> String {
    match e {
        Engine::MySql => my::q(n),
        Engine::SqlServer => ms::q(n),
        _ => pg::q(n),
    }
}

fn top0(e: Engine, cols: &str, from: &str) -> String {
    match e {
        Engine::SqlServer => format!("SELECT TOP 0 {cols} FROM {from}"),
        _ => format!("SELECT {cols} FROM {from} LIMIT 0"),
    }
}

async fn verify_sql(agent: &Arc<Connection>, spec: &Spec, inv: &Inventory, probe: &Probe, out: &mut Vec<Check>) -> Result<()> {
    let e = spec.engine;
    let db = admin_db(agent, flavor_db(spec)).await?;
    let who = match e {
        Engine::Postgres => "SELECT current_user",
        Engine::MySql => "SELECT SUBSTRING_INDEX(CURRENT_USER(), '@', 1)",
        _ => "SELECT SUSER_SNAME()",
    };
    let me = text_rows(&db, who).await?.first().and_then(|r| r[0].clone()).unwrap_or_default();
    out.push(check("Logged in as the agent", me == spec.user, me.clone()));
    let (acc, hidden) = access(spec, inv);
    let qual = |s: &str, n: &str| if s.is_empty() { sql_q(e, n) } else { format!("{}.{}", sql_q(e, s), sql_q(e, n)) };
    let try_sql = |sql: String| {
        let db = db.clone();
        async move { db.execute(&sql).await }
    };
    let views: Vec<ViewMap> = match e {
        Engine::Postgres => pg::desired(spec, inv, probe).views.into_iter().map(|v| v.map).collect(),
        Engine::MySql => my::desired(spec, inv, probe).views.into_iter().map(|v| v.map).collect(),
        _ => ms::desired(spec, inv, probe).views.into_iter().map(|v| v.map).collect(),
    };
    if spec.rows {
        if let Some(a) = acc.iter().filter(|a| a.whole && a.grantable && !a.plain.is_empty()).min_by_key(|a| a.kind == ObjKind::View) {
            let r = try_sql(top0(e, &sql_q(e, &a.plain[0]), &qual(&a.schema, &a.name))).await;
            out.push(check(format!("Reads {}", a.label()), r.is_ok(), r.err().map(|x| x.to_string()).unwrap_or_default()));
            if !a.write && a.kind != ObjKind::View {
                let c = sql_q(e, &a.plain[0]);
                let r = try_sql(format!("UPDATE {} SET {c} = {c} WHERE 1 = 0", qual(&a.schema, &a.name))).await;
                out.push(check(format!("Can't write {} (read tier)", a.label()), r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "the write went through".into())));
            }
        }
        if let Some(v) = views.first() {
            let a = acc.iter().find(|a| a.schema == v.schema && a.name == v.table);
            let col = a.and_then(|a| a.masked.first().map(|m| m.0.clone()).or_else(|| a.plain.first().cloned())).unwrap_or_default();
            let r = try_sql(top0(e, &sql_q(e, &col), &v.view)).await;
            out.push(check(format!("Reads {} through its view", v.table), r.is_ok(), r.err().map(|x| x.to_string()).unwrap_or_default()));
            if let Some((m, _)) = a.and_then(|a| a.masked.first()) {
                let r = try_sql(top0(e, &sql_q(e, m), &qual(&v.schema, &v.table))).await;
                out.push(check(format!("Can't read {}.{m} unmasked", v.table), r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "it read the real values".into())));
            }
        }
        if let Some(a) = acc.iter().find(|a| !a.hidden.is_empty()) {
            let h = &a.hidden[0];
            let r = try_sql(top0(e, &sql_q(e, h), &qual(&a.schema, &a.name))).await;
            out.push(check(format!("Can't read {}.{h} (hidden)", a.name), r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "it read the hidden column".into())));
        }
    }
    // Not granted (another owner's): out of reach until its owner grants it.
    if let Some(a) = acc.iter().find(|a| !a.grantable && !probe.flavor.superuser) {
        let r = try_sql(top0(e, "1", &qual(&a.schema, &a.name))).await;
        out.push(check(format!("Can't read {} (not granted)", a.label()), r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "it read a table it was not granted".into())));
    }
    if let Some(h) = hidden.first() {
        let name = h.split(" (").next().unwrap_or(h);
        let (s, n) = name.split_once('.').unwrap_or(("", name));
        let r = try_sql(top0(e, "1", &qual(s, n))).await;
        out.push(check(format!("Can't read {name} (hidden)"), r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "it read the hidden table".into())));
    }
    let probe_table = format!("dataman_probe_{}", &hash::hex(&hash::random_bytes(4)));
    let schema = match e {
        Engine::Postgres => "public".to_string(),
        Engine::SqlServer => "dbo".to_string(),
        _ => acc.first().map(|a| a.schema.clone()).unwrap_or_else(|| agent.database().to_string()),
    };
    let tx = e != Engine::MySql;
    if tx {
        let _ = db.execute(if e == Engine::SqlServer { "BEGIN TRANSACTION" } else { "BEGIN" }).await;
    }
    let r = try_sql(format!("CREATE TABLE {} (i INT)", qual(&schema, &probe_table))).await;
    let ok = r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message));
    if tx {
        let _ = db.execute(if e == Engine::SqlServer { "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION" } else { "ROLLBACK" }).await;
    } else if r.is_ok() {
        let _ = db.execute(&format!("DROP TABLE {}", qual(&schema, &probe_table))).await;
    }
    let pg_public = e == Engine::Postgres && probe.flavor.major > 0 && probe.flavor.major < 15;
    out.push(check("Can't change structure", ok || pg_public, r.err().map(|x| x.message).unwrap_or_else(|| if pg_public { "PUBLIC may create in schema public before PostgreSQL 15".into() } else { "it created a table (rolled back)".into() })));
    Ok(())
}

async fn verify_mongo(agent: &Arc<Connection>, spec: &Spec, inv: &Inventory, out: &mut Vec<Check>) -> Result<()> {
    let Client::Mongo(m) = nosql(agent)? else { bail!("not MongoDB") };
    let st = mongo::run(m, "admin", &json!({ "connectionStatus": 1 })).await?;
    let v: Value = crate::nosql::bson::Bson::Document(st).into_relaxed_extjson();
    let me = v["authInfo"]["authenticatedUsers"][0]["user"].as_str().unwrap_or("").to_string();
    out.push(check("Logged in as the agent", me == spec.user, me));
    let d = mongo::desired(spec, inv);
    let find = |db: String, coll: String| {
        let m = m.clone();
        async move { mongo::run(&m, &db, &json!({ "find": coll, "limit": 1 })).await }
    };
    if let Some(a) = d.access.iter().find(|a| a.whole) {
        let r = find(a.schema.clone(), a.name.clone()).await;
        out.push(check(format!("Reads {}", a.label()), r.is_ok(), r.err().map(|x| format!("{x:#}")).unwrap_or_default()));
        if !a.write {
            let r = mongo::run(m, &a.schema, &json!({ "insert": a.name, "documents": [{ "dataman_probe": 1 }] })).await;
            out.push(check(format!("Can't write {} (read tier)", a.label()), r.as_ref().is_err_and(|x| refused(Engine::MongoDb, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "the insert went through".into())));
        }
    }
    if let Some((db, view, _, _, map)) = d.views.first() {
        let r = find(db.clone(), view.clone()).await;
        out.push(check(format!("Reads {} through its view", map.table), r.is_ok(), r.err().map(|x| format!("{x:#}")).unwrap_or_default()));
        let r = find(db.clone(), map.table.clone()).await;
        out.push(check(format!("Can't read {} itself", map.table), r.as_ref().is_err_and(|x| refused(Engine::MongoDb, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it read the collection".into())));
    }
    if let Some(h) = d.hidden.first() {
        let (db, coll) = h.split_once('.').unwrap_or(("", h));
        let r = find(db.to_string(), coll.split(" (").next().unwrap_or(coll).to_string()).await;
        out.push(check(format!("Can't read {h} (hidden)"), r.as_ref().is_err_and(|x| refused(Engine::MongoDb, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it read the hidden collection".into())));
    }
    let db = d.access.first().map(|a| a.schema.clone()).unwrap_or_else(|| agent.database().to_string());
    let r = mongo::run(m, &db, &json!({ "create": format!("dataman_probe_{}", hash::hex(&hash::random_bytes(4))) })).await;
    out.push(check("Can't change structure", r.as_ref().is_err_and(|x| refused(Engine::MongoDb, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it created a collection".into())));
    Ok(())
}

async fn verify_redis(agent: &Arc<Connection>, spec: &Spec, out: &mut Vec<Check>) -> Result<()> {
    let Client::Redis(r) = nosql(agent)? else { bail!("not Redis") };
    let me = redis::call(r, &["ACL".into(), "WHOAMI".into()]).await?.text().unwrap_or_default();
    out.push(check("Logged in as the agent", me == spec.user, me));
    let ping = redis::call(r, &["PING".into()]).await;
    out.push(check("Runs commands", ping.is_ok(), ping.err().map(|x| x.to_string()).unwrap_or_default()));
    // Only refusals are tried here: what would change the server never runs.
    for (what, a) in [("Can't run CONFIG (@dangerous)", vec!["CONFIG", "GET", "maxmemory"]), ("Can't run KEYS (@dangerous)", vec!["KEYS", "dataman-probe-*"])] {
        let x = redis::call(r, &a.iter().map(|s| s.to_string()).collect::<Vec<_>>()).await;
        out.push(check(what, x.as_ref().is_err_and(|e| e.to_string().contains("NOPERM")), x.err().map(|e| e.to_string()).unwrap_or_else(|| "it ran".into())));
    }
    if !spec.write {
        let x = redis::call(r, &["SET".into(), format!("dataman-probe-{}", hash::hex(&hash::random_bytes(4))), "x".into()]).await;
        out.push(check("Can't write (read tier)", x.as_ref().is_err_and(|e| e.to_string().contains("NOPERM")), x.err().map(|e| e.to_string()).unwrap_or_else(|| "it wrote".into())));
    }
    Ok(())
}

async fn verify_search(agent: &Arc<Connection>, spec: &Spec, inv: &Inventory, probe: &Probe, out: &mut Vec<Check>) -> Result<()> {
    let Client::Search(s) = nosql(agent)? else { bail!("not a search server") };
    let os = probe.flavor.opensearch;
    let who = if os { search::call(s, "GET", "/_plugins/_security/authinfo", None).await? } else { search::call(s, "GET", "/_security/_authenticate", None).await? };
    let me = (if os { who["user_name"].as_str() } else { who["username"].as_str() }).unwrap_or("").to_string();
    out.push(check("Logged in as the agent", me == spec.user, me));
    let d = search::desired(spec, inv, os, probe.flavor.superuser);
    if let Some(a) = d.access.first() {
        let r = search::call(s, "POST", &format!("/{}/_search", a.name), Some(&json!({ "size": 1 }))).await;
        out.push(check(format!("Searches {}", a.name), r.is_ok(), r.as_ref().err().map(|x| format!("{x:#}")).unwrap_or_default()));
        if let (Ok(v), Some(h)) = (&r, a.hidden.first()) {
            let src = &v["hits"]["hits"][0]["_source"];
            out.push(check(format!("{}.{h} is left out (hidden)", a.name), src.get(h).is_none(), String::new()));
        }
    }
    if let Some(h) = d.hidden.first() {
        let r = search::call(s, "POST", &format!("/{}/_search", h.split(" (").next().unwrap_or(h)), Some(&json!({ "size": 1 }))).await;
        out.push(check(format!("Can't search {h} (hidden)"), r.as_ref().is_err_and(|x| refused(spec.engine, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it searched the hidden index".into())));
    }
    let r = search::call(s, "PUT", &format!("/dataman-probe-{}", hash::hex(&hash::random_bytes(4))), None).await;
    let ok = r.as_ref().is_err_and(|x| refused(spec.engine, None, &format!("{x:#}")));
    out.push(check("Can't create indices", ok, r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it created an index".into())));
    Ok(())
}

/// The agent login on the server now: there and Dataman's, there and not Dataman's, or gone.
pub async fn exists(c: &Arc<Connection>, p: &ConnectionProfile, user: &str) -> Result<Option<bool>> {
    Ok(match p.engine {
        Engine::Postgres => pg::exists(&admin_db(c, Some(&p.database)).await?, user, &p.id).await?,
        Engine::MySql => {
            let db = admin_db(c, None).await?;
            let maria = text_rows(&db, "SELECT VERSION()").await?.first().and_then(|r| r[0].clone()).unwrap_or_default().to_ascii_lowercase().contains("mariadb");
            my::exists(&db, user, &p.id, maria).await?
        }
        Engine::SqlServer => ms::exists(&admin_db(c, Some(&p.database)).await?, user, &p.id).await?,
        Engine::MongoDb => match nosql(c)? {
            Client::Mongo(m) => mongo::exists(m, user, &p.id).await?,
            _ => None,
        },
        Engine::Redis => match nosql(c)? {
            Client::Redis(r) => redis::exists(r, user, &p.id).await?,
            _ => None,
        },
        Engine::Elasticsearch | Engine::OpenSearch => match nosql(c)? {
            Client::Search(s) => search::exists(s, user, &p.id, s.version.starts_with("OpenSearch")).await?,
            _ => None,
        },
        _ => None,
    })
}

/// What the login has against what the rules want now: the sync's plan and its changes (none:
/// in sync).
#[derive(Clone, Debug)]
pub struct Drift {
    pub plan: Plan,
    pub changes: Vec<Change>,
    pub probe: Probe,
    pub spec: Spec,
    pub inventory: Inventory,
}

pub async fn drift(c: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin, password: &str) -> Result<Drift> {
    let (probe, persist) = probe(c, p).await?;
    if probe.exists != Some(true) {
        bail!("{} is not on the server as Dataman's any more: remove it in Dataman and create it again", login.user);
    }
    let o = Options { rotate_days: login.rotate_days, conn_limit: login.conn_limit, verify: true };
    let mut spec = compile(p, &login.database, &o);
    spec.user = login.user.clone();
    let inventory = inventory(c, &spec, &probe).await?;
    let prod = p.env.is_production();
    let (plan, have): (Plan, Vec<String>) = match p.engine {
        Engine::Postgres => {
            let (have, views, schema) = pg::have(&admin_db(c, Some(&spec.database)).await?, &spec.user, &spec.database, &spec.space).await?;
            (pg::sync_plan(&spec, &inventory, &probe, &have, &views, schema, prod), have.iter().map(pg::text).collect())
        }
        Engine::MySql => {
            let (have, views, db) = my::have(&admin_db(c, None).await?, &spec.user, &spec.space, probe.flavor.mariadb).await?;
            (my::sync_plan(&spec, &inventory, &probe, &have, &views, db, prod), have.iter().map(my::text).collect())
        }
        Engine::SqlServer => {
            let (have, views, schema) = ms::have(&admin_db(c, Some(&spec.database)).await?, &spec.user, &spec.space, probe.flavor.azure).await?;
            (ms::sync_plan(&spec, &inventory, &probe, &have, &views, schema, prod), have.iter().map(ms::text).collect())
        }
        Engine::MongoDb => {
            let Client::Mongo(m) = nosql(c)? else { bail!("not MongoDB") };
            let (lines, views) = mongo::have(m, &spec.user, &spec.space).await?;
            (mongo::sync_plan(&spec, &inventory, &probe, &views, prod), lines)
        }
        Engine::Redis => {
            let Client::Redis(r) = nosql(c)? else { bail!("not Redis") };
            let (have, want) = redis::have_and_want(r, &spec).await?;
            let mut plan = redis::sync_plan(&spec, &probe, persist.unwrap_or(redis::Persist::None), password, prod);
            plan.lines = want;
            (plan, have)
        }
        Engine::Elasticsearch | Engine::OpenSearch => {
            let Client::Search(s) = nosql(c)? else { bail!("not a search server") };
            let have = search::have(s, &spec.user, probe.flavor.opensearch).await?;
            (search::sync_plan(&spec, &inventory, &probe, prod), have)
        }
        _ => bail!("{FILE_NOTE}"),
    };
    let changes = diff(&have, &plan.lines);
    Ok(Drift { plan, changes, probe, spec, inventory })
}

/// Removing it: only what Dataman made, checked on the server first.
pub async fn removal(c: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin) -> Result<Plan> {
    let user = &login.user;
    let space = space_of(user);
    let space = space.as_str();
    let there = exists(c, p, user).await?;
    if there == Some(false) {
        bail!("{user} on the server isn't marked as Dataman's (for this connection): Dataman won't drop it");
    }
    Ok(match p.engine {
        Engine::Postgres => {
            let database = if login.database.is_empty() { p.database.clone() } else { login.database.clone() };
            let db = admin_db(c, Some(&database)).await?;
            let (views, salt, only_ours, _) = pg::made_here(&db, space).await?;
            let (have, _, _) = pg::have(&db, user, &database, space).await.unwrap_or_default();
            let sup = text_rows(&db, "SELECT rolsuper FROM pg_roles WHERE rolname = current_user").await?.first().is_some_and(|r| r[0].as_deref() == Some("t"));
            let mut plan = pg::remove_plan(user, space, &database, &have, &views, salt, only_ours, sup);
            if there.is_none() {
                plan.steps.retain(|s| s.group.starts_with("Dataman"));
            }
            plan
        }
        Engine::MySql => {
            let db = admin_db(c, None).await?;
            let (views, salt, only_ours) = my::made_here(&db, space).await?;
            let mut plan = my::remove_plan(user, space, &views, salt, only_ours && login.made.iter().any(|m| m.starts_with("database:")));
            if there.is_none() {
                plan.steps.retain(|s| s.group.starts_with("Dataman"));
            }
            plan
        }
        Engine::SqlServer => {
            let db = admin_db(c, Some(&p.database)).await?;
            let (views, salt, only_ours) = ms::made_here(&db, space).await?;
            let azure = text_rows(&db, "SELECT CAST(SERVERPROPERTY('EngineEdition') AS INT)").await?.first().and_then(|r| r[0].clone()).as_deref() == Some("5");
            ms::remove_plan(user, space, &views, salt, only_ours, azure)
        }
        Engine::MongoDb => {
            let views: Vec<(String, String)> = login.made.iter().filter_map(|m| m.strip_prefix("view:")).filter_map(|v| v.split_once('.')).filter(|(_, c)| c.starts_with(&format!("{space}."))).map(|(d, c)| (d.to_string(), c.to_string())).collect();
            let mut plan = mongo::remove_plan(user, &views);
            if there.is_none() {
                plan.steps.retain(|s| s.group.starts_with("Dataman"));
            }
            plan
        }
        Engine::Redis => {
            let Client::Redis(r) = nosql(c)? else { bail!("not Redis") };
            let (_, persist) = redis::probe(r, user, &p.id).await?;
            redis::remove_plan(user, persist)
        }
        Engine::Elasticsearch | Engine::OpenSearch => {
            let Client::Search(s) = nosql(c)? else { bail!("not a search server") };
            search::remove_plan(user, s.version.starts_with("OpenSearch"))
        }
        _ => bail!("{FILE_NOTE}"),
    })
}

/// The SQL to drop it, without the server (it can't be reached): what Dataman recorded.
pub fn removal_offline(p: &ConnectionProfile, login: &AgentLogin) -> Plan {
    let user = &login.user;
    let space = space_of(user);
    let views: Vec<String> = login.views.iter().map(|v| v.view.rsplit('.').next().unwrap_or(&v.view).trim_matches(['"', '`', '[', ']']).to_string()).collect();
    let made_space = login.made.iter().any(|m| m.starts_with("schema:") || m.starts_with("database:"));
    match p.engine {
        Engine::Postgres => pg::remove_plan(user, &space, &login.database, &[], &views, true, made_space, true),
        Engine::MySql => my::remove_plan(user, &space, &views, true, made_space),
        Engine::SqlServer => ms::remove_plan(user, &space, &views, true, made_space, false),
        Engine::MongoDb => mongo::remove_plan(user, &login.views.iter().map(|v| (v.schema.clone(), v.view.clone())).collect::<Vec<_>>()),
        Engine::Redis => redis::remove_plan(user, redis::Persist::AclFile),
        Engine::Elasticsearch => search::remove_plan(user, false),
        Engine::OpenSearch => search::remove_plan(user, true),
        _ => Plan::default(),
    }
}

/// A new password: the server first, then the caller keeps it (the old one stays in the
/// Keychain until the new one is set here).
pub async fn rotation(c: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin, _old: &str, new: &str) -> Result<Plan> {
    let user = &login.user;
    let expires = login.rotate_days.map(|d| d + GRACE_DAYS);
    let step = match p.engine {
        Engine::Postgres => pg::rotate_step(user, new, expires),
        Engine::MySql => {
            let maria = text_rows(&admin_db(c, None).await?, "SELECT VERSION()").await?.first().and_then(|r| r[0].clone()).unwrap_or_default().to_ascii_lowercase().contains("mariadb");
            my::rotate_step(user, new, maria)
        }
        Engine::SqlServer => {
            let azure = text_rows(&admin_db(c, Some(&p.database)).await?, "SELECT CAST(SERVERPROPERTY('EngineEdition') AS INT)").await?.first().and_then(|r| r[0].clone()).as_deref() == Some("5");
            ms::rotate_step(user, new, azure)
        }
        Engine::MongoDb => mongo::rotate_step(user, new),
        Engine::Redis => redis::rotate_step(user, new),
        Engine::Elasticsearch => search::rotate_step(user, &p.id, new, false),
        Engine::OpenSearch => search::rotate_step(user, &p.id, new, true),
        _ => bail!("{FILE_NOTE}"),
    };
    if exists(c, p, user).await? != Some(true) {
        bail!("{user} is not on the server as Dataman's: nothing to rotate");
    }
    Ok(Plan { engine: Some(p.engine), user: user.clone(), steps: vec![step], one_tx: p.engine.is_sql(), ..Default::default() })
}

/// The database a plan's statements run in.
pub fn database_for(p: &ConnectionProfile, login: Option<&AgentLogin>) -> String {
    login.map(|l| l.database.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| p.database.clone())
}

/// A login's password in the Keychain, refused when missing (never the user's instead).
pub fn password_of(secrets: &dyn crate::SecretStore, conn_id: &str, user: &str) -> Result<String> {
    match secrets.get(&secret_account(conn_id)) {
        Ok(Some(pw)) if !pw.is_empty() => Ok(pw),
        Ok(_) => bail!("the agent login {user}'s password is not in the {}: agents are refused on this connection until it is set again (Agents › Database login › Rotate password)", crate::secret::STORE_NAME),
        Err(e) => bail!("the agent login {user}'s password could not be read from the {}: {e:#}", crate::secret::STORE_NAME),
    }
}
