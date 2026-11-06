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

#[derive(Clone)]
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

impl std::fmt::Debug for Review {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Review")
            .field("probe", &self.probe)
            .field("spec", &self.spec)
            .field("inventory", &self.inventory)
            .field("plan", &self.plan)
            .field("password", &crate::mask::MASK)
            .field("atlas", &self.atlas)
            .field("persist", &self.persist)
            .finish()
    }
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

/// Whether Dataman could make a login here: false on a PostgreSQL-protocol server without
/// logins (MotherDuck, by its host, without asking).
pub async fn has_logins(c: &Arc<Connection>, p: &ConnectionProfile) -> Result<bool> {
    match p.engine {
        Engine::Postgres if pg::motherduck(p) => Ok(false),
        Engine::Postgres => pg::has_roles(&admin_db(c, Some(&p.database)).await?).await,
        e => Ok(supported(e)),
    }
}

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
        Engine::ClickHouse => bail!("Dataman makes no agent login on ClickHouse: the connection's own login and its rules apply"),
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
    if probe.orphan {
        // Left by a database made again, and Dataman no longer knows it: dropped first, in the
        // script the user reviews.
        plan.before = ms::drop_orphan_steps(&spec.user);
        plan.notes.push(format!("A login named {} is on the server without its user in {} (the database was restored or made again since it was created): the script drops it first.", spec.user, p.database));
    } else if probe.exists == Some(false) {
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
        Engine::Sqlite | Engine::DuckDb | Engine::ClickHouse => Plan::default(),
    }
}

/// Runs a plan's steps on the user's login: in one transaction where it says so (rolled back
/// whole on the first error), else in order, stopping at the first error.
pub async fn apply(c: &Arc<Connection>, plan: &Plan, database: &str) -> Result<()> {
    let engine = plan.engine.unwrap_or(c.engine());
    if engine.is_sql() {
        let db = admin_db(c, if matches!(engine, Engine::Postgres | Engine::SqlServer) && !database.is_empty() { Some(database) } else { None }).await?;
        let d = engine.dialect();
        let (begin, commit, rollback) = (d.begin(), d.commit(), d.rollback_if_open());
        let tx = plan.one_tx;
        for s in &plan.before {
            let Action::Sql(sql) = &s.action else { continue };
            db.execute(sql).await.map_err(|e| anyhow!("{e}\n  in: {}", s.shown))?;
        }
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
        auto_sync: o.auto_sync,
        database: p.database.clone(),
        made: plan.made.clone(),
        views: plan.views.clone(),
        granted: plan.granted,
        not_granted: plan.not_granted.len(),
    }
}

/// Logs in as `user` with `pw` and runs Verify's checks; the connection is closed after.
pub async fn verify_as(core: &crate::Core, p: &ConnectionProfile, user: &str, pw: &str, rv: &Review) -> std::result::Result<Vec<Check>, String> {
    let mine = ConnectionProfile { agent_login: None, ..p.clone() };
    let agent = core.connect_agent(&mine, agent_profile(p, user), pw.to_string()).await.map_err(|e| format!("cannot log in as {user}: {e:#}"))?;
    let r = verify(&agent, &rv.spec, &rv.inventory, &rv.probe).await.map_err(|e| format!("{e:#}"));
    agent.close();
    r
}

/// Create, once the user said yes to the review: runs its plan on the user's login (`admin`),
/// keeps the password in the Keychain, and logs in as the new login to check it. A login that
/// fails the check is removed again; where removing fails too it is kept, with why (the third).
pub async fn create(core: &crate::Core, admin: &Arc<Connection>, p: &ConnectionProfile, rv: &Review, o: &Options) -> std::result::Result<(AgentLogin, Vec<Check>, Option<String>), String> {
    if let Some(l) = rv.plan.lacks.first() {
        return Err(format!("can't create it: {l}"));
    }
    apply(admin, &rv.plan, &p.database).await.map_err(|e| format!("{e:#}"))?;
    let account = secret_account(&p.id);
    core.secrets.set(&account, &rv.password).map_err(|e| format!("the login was created but its password could not be kept in the {}: {e:#}", crate::secret::STORE_NAME))?;
    let mut rec = record(p, &rv.plan, o);
    let checks = verify_as(core, p, &rec.user, &rv.password, rv).await;
    if let Ok(c) = &checks
        && c.iter().all(|x| x.ok)
    {
        rec.verified = Some(Utc::now());
        return Ok((rec, checks.unwrap_or_default(), None));
    }
    let why = match &checks {
        Ok(c) => c.iter().filter(|x| !x.ok).map(|x| format!("{}: {}", x.line(), x.detail)).collect::<Vec<_>>().join("; "),
        Err(e) => e.clone(),
    };
    let undone = match removal(admin, p, &rec).await {
        Ok(plan) => apply(admin, &plan, &p.database).await.is_ok(),
        Err(_) => false,
    };
    if undone {
        let _ = core.secrets.delete(&account);
        return Err(format!("logging in as {} did not go as the rules say ({why}); it was removed again", rec.user));
    }
    Ok((rec, checks.unwrap_or_default(), Some(format!("logging in as it did not go as the rules say: {why}"))))
}

/// The profile agents connect with: the agent login, its password given by the caller (never
/// the user's, nor a `.pgpass` line).
pub fn agent_profile(p: &ConnectionProfile, user: &str) -> ConnectionProfile {
    let mut a = p.clone();
    a.user = user.to_string();
    a.save_password = false;
    // The agent signs in with its own password, never the user's cloud sign-in; the TLS a token
    // required stays, since `tls_mode` only raises it for a token.
    a.sslmode = p.tls_mode();
    a.auth = crate::config::Auth::Password;
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
    pub group: CheckGroup,
    /// What it holds, the same words whatever was tried ("Can't read hidden tables").
    pub what: String,
    /// What it was tried on: one table, column or command picked from the rules ("" for none).
    pub on: String,
    pub tip: &'static str,
    /// A refusal it holds (what it can't do), not something it can.
    pub guard: bool,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CheckGroup {
    Login,
    Reading,
    Writing,
    Privacy,
    Structure,
}

impl CheckGroup {
    pub const ALL: [CheckGroup; 5] = [CheckGroup::Login, CheckGroup::Reading, CheckGroup::Writing, CheckGroup::Privacy, CheckGroup::Structure];

    pub fn label(self) -> &'static str {
        match self {
            CheckGroup::Login => "Login",
            CheckGroup::Reading => "Reading",
            CheckGroup::Writing => "Writing",
            CheckGroup::Privacy => "Privacy",
            CheckGroup::Structure => "Structure",
        }
    }
}

impl Check {
    pub fn line(&self) -> String {
        if self.on.is_empty() { self.what.clone() } else { format!("{} ({})", self.what, self.on) }
    }
}

fn check(group: CheckGroup, what: &str, on: impl Into<String>, tip: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check { group, what: what.into(), on: on.into(), tip, guard: guards(what), ok, detail: detail.into() }
}

pub fn guards(what: &str) -> bool {
    what.starts_with("Can't") || matches!(what, "Hidden fields are left out" | "Sessions start read-only")
}

use CheckGroup as G;

const TIP_LOGIN: &str = "Dataman connected with the login's own password, from the Keychain, and the server named it back.";
const TIP_READ_ONLY: &str = "Its role starts every session read-only, a second lock on top of its grants.";
const TIP_REDIS_PING: &str = "PING as the login: its ACL user is on and its password works.";
const TIP_READS: &str = "A read that returns no rows, on one table the rules let agents read.";
const TIP_SEARCHES: &str = "A one-hit search on one index the rules let agents read.";
const TIP_VIEW: &str = "Masked columns are read through a view Dataman made, which masks them on the server.";
const TIP_NOT_GRANTED: &str = "A table your login can't grant stays out of its reach: agents don't see it.";
const TIP_WRITE: &str = "Read only: a write that matches no row, refused by the database for lack of privilege.";
const TIP_HIDDEN_TABLE: &str = "Tables Privacy hides get no grant at all: the database itself refuses them.";
const TIP_HIDDEN_COLUMN: &str = "Columns Privacy hides are left out of its grants, so selecting one is refused.";
const TIP_MASKED: &str = "The table itself doesn't give it the masked column: only the view, which masks it.";
const TIP_HIDDEN_FIELD: &str = "Field level security leaves hidden fields out of what it reads.";
const TIP_STRUCTURE: &str = "Creating a table as it is refused (and rolled back if it wasn't).";
const TIP_INDICES: &str = "Creating an index as it is refused.";
const TIP_DANGEROUS: &str = "Commands in @dangerous (CONFIG, KEYS…) are off in its ACL.";

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
    out.push(check(G::Login, "Logs in as itself", me.clone(), TIP_LOGIN, me == spec.user, me.clone()));
    // PostgreSQL: a read login's sessions start read-only (its role's setting), which turns a
    // write away before its privileges are looked at. Said as a check of its own, then off for
    // this session, so what refuses the tries below is the privileges.
    if e == Engine::Postgres && !spec.write {
        let ro = text_rows(&db, "SHOW default_transaction_read_only").await?.first().and_then(|r| r[0].clone()).unwrap_or_default();
        out.push(check(G::Login, "Sessions start read-only", "", TIP_READ_ONLY, ro == "on", ro));
        db.execute("SET default_transaction_read_only = off").await.map_err(|x| anyhow!("{}", x.message))?;
    }
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
            out.push(check(G::Reading, "Reads what the rules allow", a.label(), TIP_READS, r.is_ok(), r.err().map(|x| x.to_string()).unwrap_or_default()));
            if !a.write && a.kind != ObjKind::View {
                // A DELETE names no column: only the privilege can refuse it (an UPDATE of an
                // identity column is refused by SQL Server for that before any permission).
                let r = try_sql(format!("DELETE FROM {} WHERE 1 = 0", qual(&a.schema, &a.name))).await;
                out.push(check(G::Writing, "Can't write rows", a.label(), TIP_WRITE, r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "the write went through".into())));
            }
        }
        if let Some(v) = views.first() {
            let a = acc.iter().find(|a| a.schema == v.schema && a.name == v.table);
            let col = a.and_then(|a| a.masked.first().map(|m| m.0.clone()).or_else(|| a.plain.first().cloned())).unwrap_or_default();
            let r = try_sql(top0(e, &sql_q(e, &col), &v.view)).await;
            out.push(check(G::Reading, "Reads masked tables through its view", v.table.clone(), TIP_VIEW, r.is_ok(), r.err().map(|x| x.to_string()).unwrap_or_default()));
            // What every login reads anyway (the review says so) is not tried.
            if let Some((m, _)) = a.and_then(|a| a.masked.iter().find(|(m, _)| !inv.open_to_all(&a.schema, &a.name, Some(m)))) {
                let r = try_sql(top0(e, &sql_q(e, m), &qual(&v.schema, &v.table))).await;
                out.push(check(G::Privacy, "Can't read masked values unmasked", format!("{}.{m}", v.table), TIP_MASKED, r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "it read the real values".into())));
            }
        }
        if let Some((a, h)) = acc.iter().find_map(|a| a.hidden.iter().find(|h| !inv.open_to_all(&a.schema, &a.name, Some(h))).map(|h| (a, h))) {
            let r = try_sql(top0(e, &sql_q(e, h), &qual(&a.schema, &a.name))).await;
            out.push(check(G::Privacy, "Can't read hidden columns", format!("{}.{h}", a.name), TIP_HIDDEN_COLUMN, r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "it read the hidden column".into())));
        }
    }
    // Not granted (another owner's): out of reach until its owner grants it.
    if let Some(a) = acc.iter().find(|a| !a.grantable && !probe.flavor.superuser) {
        let r = try_sql(top0(e, "1", &qual(&a.schema, &a.name))).await;
        out.push(check(G::Reading, "Can't read tables it wasn't granted", a.label(), TIP_NOT_GRANTED, r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "it read a table it was not granted".into())));
    }
    let open_table = |h: &String| {
        let name = h.split(" (").next().unwrap_or(h);
        let (s, n) = name.split_once('.').unwrap_or(("", name));
        inv.open_to_all(s, n, None)
    };
    if let Some(h) = hidden.iter().find(|h| !open_table(h)) {
        let name = h.split(" (").next().unwrap_or(h);
        let (s, n) = name.split_once('.').unwrap_or(("", name));
        let r = try_sql(top0(e, "1", &qual(s, n))).await;
        out.push(check(G::Privacy, "Can't read hidden tables", name, TIP_HIDDEN_TABLE, r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message)), r.err().map(|x| x.message).unwrap_or_else(|| "it read the hidden table".into())));
    }
    let probe_table = format!("dataman_probe_{}", hash::hex(&hash::random_bytes(4)));
    let schema = match e {
        Engine::Postgres => "public".to_string(),
        Engine::SqlServer => "dbo".to_string(),
        _ => acc.first().map(|a| a.schema.clone()).unwrap_or_else(|| agent.database().to_string()),
    };
    let tx = e != Engine::MySql;
    if tx {
        let _ = db.execute(e.dialect().begin()).await;
    }
    let r = try_sql(format!("CREATE TABLE {} (i INT)", qual(&schema, &probe_table))).await;
    let ok = r.as_ref().is_err_and(|x| refused(e, x.code.as_deref(), &x.message));
    if tx {
        let _ = db.execute(e.dialect().rollback_if_open()).await;
    } else if r.is_ok() {
        let _ = db.execute(&format!("DROP TABLE {}", qual(&schema, &probe_table))).await;
    }
    let pg_public = e == Engine::Postgres && probe.flavor.major > 0 && probe.flavor.major < 15;
    out.push(check(G::Structure, "Can't change structure", "", TIP_STRUCTURE, ok || pg_public, r.err().map(|x| x.message).unwrap_or_else(|| if pg_public { "PUBLIC may create in schema public before PostgreSQL 15".into() } else { "it created a table (rolled back)".into() })));
    Ok(())
}

async fn verify_mongo(agent: &Arc<Connection>, spec: &Spec, inv: &Inventory, out: &mut Vec<Check>) -> Result<()> {
    let Client::Mongo(m) = nosql(agent)? else { bail!("not MongoDB") };
    let st = mongo::run(m, "admin", &json!({ "connectionStatus": 1 })).await?;
    let v: Value = crate::nosql::bson::Bson::Document(st).into_relaxed_extjson();
    let me = v["authInfo"]["authenticatedUsers"][0]["user"].as_str().unwrap_or("").to_string();
    out.push(check(G::Login, "Logs in as itself", me.clone(), TIP_LOGIN, me == spec.user, me));
    let d = mongo::desired(spec, inv);
    let find = |db: String, coll: String| {
        let m = m.clone();
        async move { mongo::run(&m, &db, &json!({ "find": coll, "limit": 1 })).await }
    };
    if let Some(a) = d.access.iter().find(|a| a.whole) {
        let r = find(a.schema.clone(), a.name.clone()).await;
        out.push(check(G::Reading, "Reads what the rules allow", a.label(), TIP_READS, r.is_ok(), r.err().map(|x| format!("{x:#}")).unwrap_or_default()));
        if !a.write {
            let r = mongo::run(m, &a.schema, &json!({ "insert": a.name, "documents": [{ "dataman_probe": 1 }] })).await;
            out.push(check(G::Writing, "Can't write documents", a.label(), TIP_WRITE, r.as_ref().is_err_and(|x| refused(Engine::MongoDb, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "the insert went through".into())));
        }
    }
    if let Some((db, view, _, _, map)) = d.views.first() {
        let r = find(db.clone(), view.clone()).await;
        out.push(check(G::Reading, "Reads masked collections through its view", map.table.clone(), TIP_VIEW, r.is_ok(), r.err().map(|x| format!("{x:#}")).unwrap_or_default()));
        let r = find(db.clone(), map.table.clone()).await;
        out.push(check(G::Privacy, "Can't read masked values unmasked", map.table.clone(), TIP_MASKED, r.as_ref().is_err_and(|x| refused(Engine::MongoDb, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it read the collection".into())));
    }
    if let Some(h) = d.hidden.first() {
        let (db, coll) = h.split_once('.').unwrap_or(("", h));
        let r = find(db.to_string(), coll.split(" (").next().unwrap_or(coll).to_string()).await;
        out.push(check(G::Privacy, "Can't read hidden collections", h.clone(), TIP_HIDDEN_TABLE, r.as_ref().is_err_and(|x| refused(Engine::MongoDb, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it read the hidden collection".into())));
    }
    let db = d.access.first().map(|a| a.schema.clone()).unwrap_or_else(|| agent.database().to_string());
    let r = mongo::run(m, &db, &json!({ "create": format!("dataman_probe_{}", hash::hex(&hash::random_bytes(4))) })).await;
    out.push(check(G::Structure, "Can't change structure", "", TIP_STRUCTURE, r.as_ref().is_err_and(|x| refused(Engine::MongoDb, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it created a collection".into())));
    Ok(())
}

async fn verify_redis(agent: &Arc<Connection>, spec: &Spec, out: &mut Vec<Check>) -> Result<()> {
    let Client::Redis(r) = nosql(agent)? else { bail!("not Redis") };
    let me = redis::call(r, &["ACL".into(), "WHOAMI".into()]).await?.text().unwrap_or_default();
    out.push(check(G::Login, "Logs in as itself", me.clone(), TIP_LOGIN, me == spec.user, me));
    let ping = redis::call(r, &["PING".into()]).await;
    out.push(check(G::Login, "Runs commands", "PING", TIP_REDIS_PING, ping.is_ok(), ping.err().map(|x| x.to_string()).unwrap_or_default()));
    // Only refusals are tried here: what would change the server never runs.
    for (on, a) in [("CONFIG", vec!["CONFIG", "GET", "maxmemory"]), ("KEYS", vec!["KEYS", "dataman-probe-*"])] {
        let x = redis::call(r, &a.iter().map(|s| s.to_string()).collect::<Vec<_>>()).await;
        out.push(check(G::Structure, "Can't run dangerous commands", on, TIP_DANGEROUS, x.as_ref().is_err_and(|e| e.to_string().contains("NOPERM")), x.err().map(|e| e.to_string()).unwrap_or_else(|| "it ran".into())));
    }
    if !spec.write {
        let x = redis::call(r, &["SET".into(), format!("dataman-probe-{}", hash::hex(&hash::random_bytes(4))), "x".into()]).await;
        out.push(check(G::Writing, "Can't write keys", "SET", TIP_WRITE, x.as_ref().is_err_and(|e| e.to_string().contains("NOPERM")), x.err().map(|e| e.to_string()).unwrap_or_else(|| "it wrote".into())));
    }
    Ok(())
}

async fn verify_search(agent: &Arc<Connection>, spec: &Spec, inv: &Inventory, probe: &Probe, out: &mut Vec<Check>) -> Result<()> {
    let Client::Search(s) = nosql(agent)? else { bail!("not a search server") };
    let os = probe.flavor.opensearch;
    let who = if os { search::call(s, "GET", "/_plugins/_security/authinfo", None).await? } else { search::call(s, "GET", "/_security/_authenticate", None).await? };
    let me = (if os { who["user_name"].as_str() } else { who["username"].as_str() }).unwrap_or("").to_string();
    out.push(check(G::Login, "Logs in as itself", me.clone(), TIP_LOGIN, me == spec.user, me));
    let d = search::desired(spec, inv, os, probe.flavor.superuser);
    if let Some(a) = d.access.first() {
        let r = search::call(s, "POST", &format!("/{}/_search", a.name), Some(&json!({ "size": 1 }))).await;
        out.push(check(G::Reading, "Searches what the rules allow", a.name.clone(), TIP_SEARCHES, r.is_ok(), r.as_ref().err().map(|x| format!("{x:#}")).unwrap_or_default()));
        if let (Ok(v), Some(h)) = (&r, a.hidden.first()) {
            let src = &v["hits"]["hits"][0]["_source"];
            out.push(check(G::Privacy, "Hidden fields are left out", format!("{}.{h}", a.name), TIP_HIDDEN_FIELD, src.get(h).is_none(), String::new()));
        }
    }
    if let Some(h) = d.hidden.first() {
        let r = search::call(s, "POST", &format!("/{}/_search", h.split(" (").next().unwrap_or(h)), Some(&json!({ "size": 1 }))).await;
        out.push(check(G::Privacy, "Can't search hidden indices", h.clone(), TIP_HIDDEN_TABLE, r.as_ref().is_err_and(|x| refused(spec.engine, None, &format!("{x:#}"))), r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it searched the hidden index".into())));
    }
    let r = search::call(s, "PUT", &format!("/dataman-probe-{}", hash::hex(&hash::random_bytes(4))), None).await;
    let ok = r.as_ref().is_err_and(|x| refused(spec.engine, None, &format!("{x:#}")));
    out.push(check(G::Structure, "Can't create indices", "", TIP_INDICES, ok, r.err().map(|x| format!("{x:#}")).unwrap_or_else(|| "it created an index".into())));
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

pub fn lost_user_text(user: &str, database: &str) -> String {
    format!("{user} lost its user in {database} (the database was restored or made again): Verify puts it back, as Auto sync does on its own")
}

/// The login left without its user in its database (SQL Server: the database restored or made
/// again), put back in it with the user's login (`admin`): its user, the mark and the grants.
/// Only Dataman's own: the password the Keychain keeps for it still logs it in. `None`: nothing
/// to put back.
pub async fn reattach(core: &crate::Core, admin: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin, password: &str) -> Result<Option<Plan>> {
    if p.engine != Engine::SqlServer {
        return Ok(None);
    }
    let p = &in_login_db(p, login);
    if !ms::orphan(&admin_db(admin, Some(&p.database)).await?, &login.user).await? {
        return Ok(None);
    }
    // Its database has no user for it now: logged in to master, where every login may connect.
    let master = ConnectionProfile { agent_login: None, database: "master".into(), ..p.clone() };
    let agent = core
        .connect_agent(&master, agent_profile(&master, &login.user), password.to_string())
        .await
        .map_err(|e| anyhow!("{} lost its user in {}, and Dataman's password for it no longer logs it in ({e:#}), so Dataman can't tell it is its own: remove it and create it again", login.user, p.database))?;
    let me = text_rows(&admin_db(&agent, Some("master")).await?, "SELECT SUSER_SNAME()").await;
    agent.close();
    if me?.first().and_then(|r| r[0].clone()).as_deref() != Some(login.user.as_str()) {
        bail!("{} lost its user in {}, and logging in with Dataman's password says another name: Dataman leaves it alone", login.user, p.database);
    }
    let (probe, _) = probe(admin, p).await?;
    if let Some(l) = probe.lacks.first() {
        bail!("{} lost its user in {}, and your login can't put it back: {l}", login.user, p.database);
    }
    let o = Options { rotate_days: login.rotate_days, conn_limit: login.conn_limit, auto_sync: login.auto_sync };
    let mut spec = compile(p, &login.database, &o);
    spec.user = login.user.clone();
    let inv = inventory(admin, &spec, &probe).await?;
    let plan = ms::reattach_plan(&spec, &inv, &probe, p.env.is_production());
    apply(admin, &plan, &p.database).await?;
    Ok(Some(plan))
}

pub fn note_synced(l: &mut AgentLogin, hash: String, plan: &Plan) {
    (l.rules, l.plan, l.synced, l.views, l.granted, l.not_granted) = (hash, plan.hash.clone(), Utc::now(), plan.views.clone(), plan.granted, plan.not_granted.len());
    for m in &plan.made {
        if !l.made.contains(m) {
            l.made.push(m.clone());
        }
    }
}

/// A connection error that says the login can't open its database: what a SQL Server login
/// whose user went with the database gets (4060), or the server closing on it mid-handshake.
pub fn lost_database(e: &str) -> bool {
    e.contains("code: 4060") || e.contains("Cannot open database") || e.contains("[4060]")
}

/// Whether the agent's connection reaches its database (a SQL Server login whose user went with
/// the database it was in doesn't).
pub async fn reaches(agent: &Arc<Connection>, database: &str) -> bool {
    match admin_db(agent, Some(database)).await {
        Ok(db) => text_rows(&db, "SELECT 1").await.is_ok(),
        Err(_) => false,
    }
}

/// Why Dataman won't touch `user` on `p`'s server (in the login's database): not marked as
/// its own, or (SQL Server) a login whose user is gone.
pub async fn not_ours(c: &Arc<Connection>, p: &ConnectionProfile, user: &str) -> String {
    if p.engine == Engine::SqlServer
        && let Ok(db) = admin_db(c, Some(&p.database)).await
        && ms::orphan(&db, user).await.unwrap_or(false)
    {
        return lost_user_text(user, &p.database);
    }
    format!("{user} on the server isn't marked as Dataman's (for this connection): Dataman leaves it alone")
}

/// `p` in the database the login was made in (its user and grants are there, whatever the
/// connection's database is now).
pub fn in_login_db(p: &ConnectionProfile, login: &AgentLogin) -> ConnectionProfile {
    ConnectionProfile { database: database_for(p, Some(login)), ..p.clone() }
}

pub async fn drift(c: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin, password: &str) -> Result<Drift> {
    let p = &in_login_db(p, login);
    let (probe, persist) = probe(c, p).await?;
    if probe.orphan {
        bail!("{}", lost_user_text(&login.user, &p.database));
    }
    if probe.exists != Some(true) {
        bail!("{} is not on the server as Dataman's any more: remove it in Dataman and create it again", login.user);
    }
    let o = Options { rotate_days: login.rotate_days, conn_limit: login.conn_limit, auto_sync: login.auto_sync };
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
    let p = &in_login_db(p, login);
    let user = &login.user;
    let space = space_of(user);
    let space = space.as_str();
    let there = exists(c, p, user).await?;
    if there == Some(false) {
        bail!("{}", not_ours(c, p, user).await);
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

/// A new password: the server first, then the caller keeps it (the old one stays in the
/// Keychain until the new one is set here).
pub async fn rotation(c: &Arc<Connection>, p: &ConnectionProfile, login: &AgentLogin, _old: &str, new: &str) -> Result<Plan> {
    let p = &in_login_db(p, login);
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

/// Ends the sessions the login has open (after a new password or before it goes): a session
/// opened with the old password would otherwise go on working. Returns how many it ended.
/// MongoDB can't end another user's connections and Elasticsearch/OpenSearch keep none (each
/// request carries the password): nothing to do there.
pub async fn end_sessions(c: &Arc<Connection>, p: &ConnectionProfile, user: &str) -> Result<usize> {
    Ok(match p.engine {
        Engine::Postgres => {
            let db = admin_db(c, Some(&p.database)).await?;
            let kill = format!("SELECT count(*) FILTER (WHERE pg_terminate_backend(pid)) FROM pg_stat_activity WHERE usename = {} AND pid <> pg_backend_pid()", pg::lit(user));
            let count = |r: Vec<Vec<Option<String>>>| r.first().and_then(|x| x[0].as_deref()).and_then(|n| n.parse().ok()).unwrap_or(0);
            match text_rows(&db, &kill).await {
                Ok(r) => count(r),
                // A CREATEROLE admin (not a superuser) holds ADMIN on the role it made, not its
                // privileges (PostgreSQL 16+): it takes them for this transaction only.
                // 42501 (insufficient_privilege), whatever language the server answers in: the
                // query reads only pg_stat_activity, so it is the terminate that was refused.
                Err(e) if crate::server_error_class(&e) == Some(crate::ErrorClass::Permission) => {
                    let u = pg::q(user);
                    db.execute("BEGIN").await.map_err(|e| anyhow!("{e}"))?;
                    let r = async {
                        db.execute(&format!("GRANT {u} TO CURRENT_USER WITH INHERIT TRUE")).await.map_err(|e| anyhow!("{e}"))?;
                        let n = count(text_rows(&db, &kill).await?);
                        db.execute(&format!("REVOKE {u} FROM CURRENT_USER")).await.map_err(|e| anyhow!("{e}"))?;
                        Ok::<usize, anyhow::Error>(n)
                    }
                    .await;
                    match r {
                        Ok(n) => {
                            db.execute("COMMIT").await.map_err(|e| anyhow!("{e}"))?;
                            n
                        }
                        Err(e) => {
                            let _ = db.execute("ROLLBACK").await;
                            return Err(e);
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Engine::MySql => {
            let db = admin_db(c, None).await?;
            let ids = text_rows(&db, &format!("SELECT ID FROM information_schema.PROCESSLIST WHERE USER = {} AND ID <> CONNECTION_ID()", my::lit(user))).await?;
            let mut n = 0;
            for id in ids.iter().filter_map(|r| r[0].as_deref()).filter_map(|x| x.parse::<u64>().ok()) {
                match db.execute(&format!("KILL {id}")).await {
                    Ok(_) => n += 1,
                    Err(e) if e.code.as_deref() == Some("1094") => {}
                    Err(e) => bail!("{e}"),
                }
            }
            n
        }
        Engine::SqlServer => {
            let db = admin_db(c, Some(&p.database)).await?;
            let r = text_rows(&db, &format!("SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE login_name = {} AND session_id <> @@SPID", ms::lit(user))).await?;
            db.execute(&format!("DECLARE @k NVARCHAR(MAX) = N''; SELECT @k += N'KILL ' + CAST(session_id AS NVARCHAR(10)) + N';' FROM sys.dm_exec_sessions WHERE login_name = {} AND session_id <> @@SPID; EXEC (@k)", ms::lit(user))).await.map_err(|e| anyhow!("{e}"))?;
            r.first().and_then(|x| x[0].as_deref()).and_then(|n| n.parse().ok()).unwrap_or(0)
        }
        Engine::Redis => match nosql(c)? {
            Client::Redis(r) => redis::call(r, &["CLIENT".into(), "KILL".into(), "USER".into(), user.to_string()]).await.ok().and_then(|x| match x {
                crate::backend::redis::Reply::Int(n) => Some(n as usize),
                _ => None,
            }).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    })
}

pub fn database_for(p: &ConnectionProfile, login: Option<&AgentLogin>) -> String {
    login.map(|l| l.database.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| p.database.clone())
}

/// A login's password in the Keychain, refused when missing (never the user's instead).
pub fn password_of(secrets: &dyn crate::SecretStore, conn_id: &str, user: &str) -> Result<String> {
    match secrets.get(&secret_account(conn_id)) {
        Ok(Some(pw)) if !pw.is_empty() => Ok(pw),
        Ok(_) => bail!("the agent login {user}'s password is not in the {}: agents are refused on this connection until it is set again (Agents › Permissions › Rotate password)", crate::secret::STORE_NAME),
        Err(e) => bail!("the agent login {user}'s password could not be read from the {}: {e:#}", crate::secret::STORE_NAME),
    }
}
