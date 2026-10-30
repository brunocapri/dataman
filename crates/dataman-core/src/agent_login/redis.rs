//! Redis: an ACL user, set in one command that starts with `reset` (so a sync converges in one
//! atomic step): its command categories from the rules, never `@dangerous` (the few reads Redis
//! files there, INFO among them, added back by name), and its keys where the rules list them
//! (Only these). A key pattern only allows: "everything except" can't be said in an ACL, nor can
//! the fields of a hash be hidden, so those stay Dataman's. Scripts the user allows run on the
//! user's login; the agent's never gets `@scripting`.
//!
//! The mark is a selector that allows nothing (`(%R~dataman-managed:<id> -@all)`, Redis 7).

use anyhow::{Result, anyhow};

use super::*;
use crate::backend::redis::Client;
use crate::privacy::{ItemKind, VisibleMode};

/// Reads Dataman allows that Redis files under `@dangerous` or `@admin`.
const READS_BACK: &[&str] = &["+info", "+role", "+latency|latest", "+memory|usage", "+memory|stats", "+cluster|info", "+cluster|nodes", "+acl|whoami", "+select", "+ping", "+echo"];

pub fn marker(conn: &str) -> String {
    format!("(%R~dataman-managed:{} -@all)", key_pattern(conn))
}

/// A Dataman glob (`*` any run, case ignored by Dataman) as a Redis key pattern: `?`, `[`, `]`
/// and `\` are literal here.
pub fn key_pattern(glob: &str) -> String {
    let mut out = String::new();
    for c in glob.chars() {
        if matches!(c, '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The ACL rules after `reset on <password>`, as tokens.
pub fn rules_for(spec: &Spec) -> (Vec<String>, bool) {
    let r = &spec.rules;
    let mut t: Vec<String> = vec!["resetchannels".into()];
    let mut keys_held = true;
    let only = r.visible.mode == VisibleMode::Only;
    if !spec.rows {
        // Structure only: no key at all.
    } else if only {
        for i in r.visible.items.iter().filter(|i| i.kind != ItemKind::Column) {
            t.push(format!("~{}", key_pattern(&i.name)));
        }
    } else {
        t.push("~*".into());
        keys_held = r.visible.items.iter().all(|i| i.kind == ItemKind::Column) || r.visible.items.is_empty();
    }
    t.push("-@all".into());
    t.push("+@connection".into());
    let c = &spec.redis;
    if c.read {
        t.push("+@read".into());
    }
    if spec.write && c.write {
        t.push("+@write".into());
    }
    if spec.write && c.keyspace {
        t.push("+@keyspace".into());
    }
    if spec.write && c.pubsub {
        t.push("+@pubsub".into());
        t.push("allchannels".into());
    }
    t.push("-@dangerous".into());
    t.push("-@scripting".into());
    if !spec.write {
        t.push("-@write".into());
    }
    t.extend(READS_BACK.iter().map(|s| s.to_string()));
    if spec.activity {
        t.push("+client|list".into());
        t.push("+client|info".into());
        // The slow log holds others' commands with their arguments: keys and values outside
        // the agent's patterns. Server activity only.
        t.push("+slowlog|get".into());
        t.push("+slowlog|len".into());
    }
    (t, keys_held)
}

fn args(spec: &Spec, password_hash: &str) -> Vec<String> {
    let mut a = vec!["ACL".into(), "SETUSER".into(), spec.user.clone(), "reset".into(), "on".into(), format!("#{password_hash}")];
    a.extend(rules_for(spec).0);
    a.push(marker(&spec.conn));
    a
}

fn shown(a: &[String]) -> String {
    a.iter().map(|x| if x.starts_with('#') && x.len() == 65 { "#<sha256 of the password>".to_string() } else if x.contains(' ') { format!("\"{x}\"") } else { x.clone() }).collect::<Vec<_>>().join(" ")
}

fn step(group: &str, a: Vec<String>) -> Step {
    let text = a.iter().map(|x| if x.contains(' ') { format!("\"{x}\"") } else { x.clone() }).collect::<Vec<_>>().join(" ");
    Step { shown: shown(&a), copy: text, action: Action::Redis(a), group: group.into() }
}

fn rules(spec: &Spec, keys_held: bool, persist: &Persist, prod: bool) -> Vec<RuleLine> {
    let r = &spec.rules;
    let mut v = vec![line("A login apart from yours", "ACL SETUSER, its own password", Who::Database)];
    let c = &spec.redis;
    let mut cats = Vec::new();
    if c.read {
        cats.push("@read");
    }
    if spec.write {
        if c.write {
            cats.push("@write");
        }
        if c.keyspace {
            cats.push("@keyspace");
        }
        if c.pubsub {
            cats.push("@pubsub");
        }
    }
    v.push(line(format!("Commands: {}", if cats.is_empty() { "none".into() } else { cats.join(" ") }), "the user's categories, never @dangerous", Who::Database));
    v.push(line("Scripts after your Allow", "run on your login; the agent's has no @scripting", Who::Database));
    if !r.visible.is_open() {
        if keys_held {
            v.push(line("Only these keys", "key patterns", Who::Database));
        } else {
            v.push(line("Hidden keys", "an ACL can only allow patterns, not except them", Who::Dataman));
        }
        v.push(line("Key names in SCAN", "SCAN lists names beyond the patterns", Who::Partly));
    }
    if r.visible.has_columns() || r.view == AgentView::Masked {
        v.push(line("Hidden or masked fields", "an ACL has no fields", Who::Dataman));
    }
    v.push(line(if spec.activity { "Server activity" } else { "No server activity" }, if spec.activity { "+client|list" } else { "no CLIENT LIST" }, Who::Database));
    v.push(line(time_label(spec.time_limit_s), "no time limit per user", Who::Dataman));
    match persist {
        Persist::AclFile | Persist::Config => {}
        Persist::None => v.push(line("Kept across restarts", "no aclfile nor config file: the user goes when Redis restarts", Who::Dataman)),
    }
    v.extend(dataman_lines(spec, prod));
    v
}

/// How a user set now lasts: the ACL file, the config file, or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Persist {
    AclFile,
    Config,
    None,
}

fn persist_step(p: Persist) -> Option<Step> {
    match p {
        Persist::AclFile => Some(step("kept across restarts", vec!["ACL".into(), "SAVE".into()])),
        Persist::Config => Some(step("kept across restarts", vec!["CONFIG".into(), "REWRITE".into()])),
        Persist::None => None,
    }
}

pub fn create_plan(spec: &Spec, probe: &Probe, persist: Persist, password: &str, prod: bool) -> Plan {
    let a = args(spec, &hash::redis_sha(password));
    let (tokens, keys_held) = rules_for(spec);
    let mut steps = vec![step("the user, whole, in one command (its password sent as SHA-256)", a)];
    steps.extend(persist_step(persist));
    plan_of(spec, probe, steps, tokens, keys_held, persist, prod)
}

fn plan_of(spec: &Spec, probe: &Probe, steps: Vec<Step>, tokens: Vec<String>, keys_held: bool, persist: Persist, prod: bool) -> Plan {
    let mut notes = Vec::new();
    if persist == Persist::None {
        notes.push("This Redis has no aclfile and no config file: the user goes away when it restarts (Verify then offers to create it again).".into());
    }
    Plan {
        engine: Some(Engine::Redis),
        user: spec.user.clone(),
        steps,
        rules: rules(spec, keys_held, &persist, prod),
        lines: tokens,
        lacks: probe.lacks.clone(),
        notes,
        ..Default::default()
    }
    .seal()
}

/// A sync is the same one command: `reset` and the rules, the password kept.
pub fn sync_plan(spec: &Spec, probe: &Probe, persist: Persist, password: &str, prod: bool) -> Plan {
    create_plan(spec, probe, persist, password, prod)
}

pub fn remove_plan(user: &str, persist: Persist) -> Plan {
    let mut steps = vec![step("the user", vec!["ACL".into(), "DELUSER".into(), user.into()])];
    steps.extend(persist_step(persist));
    Plan { engine: Some(Engine::Redis), user: user.into(), steps, ..Default::default() }
}

/// The old password dropped and the new one set, in one command.
pub fn rotate_step(user: &str, new: &str) -> Step {
    step("a new password", vec!["ACL".into(), "SETUSER".into(), user.into(), "resetpass".into(), format!("#{}", hash::redis_sha(new))])
}

// ---- Live ----

pub(crate) async fn call(c: &Client, a: &[String]) -> Result<crate::backend::redis::Reply> {
    c.call(c.default_db, a).await
}

pub(crate) async fn probe(c: &Client, user: &str, conn: &str) -> Result<(Probe, Persist)> {
    let mut p = Probe { version: c.version.clone(), tls: c.tls_active, ..Default::default() };
    p.flavor.major = c.version.split_whitespace().nth(1).and_then(|v| v.split('.').next()).and_then(|v| v.parse().ok()).unwrap_or(0);
    let who = call(c, &["ACL".into(), "WHOAMI".into()]).await;
    let login = match who {
        Ok(r) => r.text().unwrap_or_default(),
        Err(e) => {
            p.manual = Some(Manual::NoAcl(format!("{e}")));
            p.lacks.push("this Redis has no ACL (Redis 6 or later), or it is a managed Redis that blocks it (ElastiCache, Memorystore, Azure Cache: create the user in its console)".into());
            p.checks.push((false, "no ACL".into()));
            return Ok((p, Persist::None));
        }
    };
    p.login = login.clone();
    if p.flavor.major > 0 && p.flavor.major < 7 {
        p.lacks.push("Redis 7 or later is needed (selectors mark Dataman's users)".into());
    }
    let can = call(c, &["ACL".into(), "DRYRUN".into(), login.clone(), "ACL".into(), "SETUSER".into(), "x".into()]).await.map(|r| r.text().unwrap_or_default() == "OK").unwrap_or(false)
        || call(c, &["ACL".into(), "LIST".into()]).await.is_ok();
    p.can_create = can && p.lacks.is_empty();
    if !can {
        p.lacks.push(format!("{login} can't manage ACL users: it needs +acl (the default user has it)"));
    }
    p.flavor.activity = true;
    p.checks.push((p.can_create, if p.can_create { format!("{login} can create ACL users") } else { format!("{login} can't create ACL users") }));
    p.checks.push((p.tls, if p.tls { "TLS on".into() } else { "TLS off".into() }));
    let persist = persistence(c).await;
    if persist == Persist::None {
        p.checks.push((false, "no aclfile: users go on restart".into()));
    }
    p.exists = exists(c, user, conn).await?;
    Ok((p, persist))
}

async fn persistence(c: &Client) -> Persist {
    let aclfile = call(c, &["CONFIG".into(), "GET".into(), "aclfile".into()]).await.ok().map(|r| r.texts()).unwrap_or_default();
    if aclfile.get(1).is_some_and(|f| !f.is_empty()) {
        return Persist::AclFile;
    }
    let info = call(c, &["INFO".into(), "server".into()]).await.ok().and_then(|r| r.text()).unwrap_or_default();
    let conf = info.lines().find_map(|l| l.strip_prefix("config_file:")).map(str::trim).unwrap_or("");
    if conf.is_empty() { Persist::None } else { Persist::Config }
}

pub(crate) async fn exists(c: &Client, user: &str, conn: &str) -> Result<Option<bool>> {
    let list = call(c, &["ACL".into(), "LIST".into()]).await?.texts();
    let prefix = format!("user {user} ");
    // The selector exactly: `%R~dataman-managed:<id>` (another id that starts the same is not this one).
    let mark = format!("%R~dataman-managed:{}", key_pattern(conn));
    Ok(list.iter().find(|l| l.starts_with(&prefix)).map(|l| l.split(|c: char| c.is_whitespace() || c == '(' || c == ')').any(|t| t == mark)))
}

/// The user's rules as Redis prints them, and those the rules want, made comparable: the want
/// side is set on a throwaway user that can't log in, read back, and deleted.
pub(crate) async fn have_and_want(c: &Client, spec: &Spec) -> Result<(Vec<String>, Vec<String>)> {
    let canon = |l: &str| -> Vec<String> { l.split_whitespace().skip(2).filter(|t| !matches!(*t, "on" | "off" | "nopass" | "resetpass") && !t.starts_with('#') && !t.starts_with('>')).map(str::to_string).collect() };
    let list = call(c, &["ACL".into(), "LIST".into()]).await?.texts();
    let have = list.iter().find(|l| l.starts_with(&format!("user {} ", spec.user))).map(|l| canon(l)).ok_or_else(|| anyhow!("{} is not on this server", spec.user))?;
    let tmp = format!("{}_check", spec.user);
    let mut a = vec!["ACL".into(), "SETUSER".into(), tmp.clone(), "reset".into(), "off".into()];
    a.extend(rules_for(spec).0);
    a.push(marker(&spec.conn));
    call(c, &a).await?;
    let list = call(c, &["ACL".into(), "LIST".into()]).await?.texts();
    let _ = call(c, &["ACL".into(), "DELUSER".into(), tmp.clone()]).await;
    let want = list.iter().find(|l| l.starts_with(&format!("user {tmp} "))).map(|l| canon(l)).unwrap_or_default();
    Ok((have, want))
}
