//! An agent may ask for the agents' own database login on a connection, never shape it: the
//! login, its grants and its options come from the rules the user set in Dataman, as Create agent
//! login makes them. Nothing runs until the user's Allow, which shows every statement; the password
//! goes to the Keychain only (never a response or the log), and the login is checked by logging in
//! as it before it is kept. It only narrows what agents can do: from then on they connect as it.

use dataman_core::agent_login::{self, Manual, Options, live};
use dataman_core::{ConfigStore, McpAccess};
use serde_json::{Value, json};

use super::Builtin;
use super::connections::of_project;
use crate::tool::{Args, BoxFut, Call, Schema, ToolError};

pub fn tools() -> Vec<Builtin> {
    vec![Builtin {
        name: "create_agent_login",
        description: "Asks the user to create a database login of the agents' own on a connection, made from the connection's rules in Dataman (what agents may see and do), so the database itself refuses what the rules refuse. The agent chooses nothing of it. Always waits for the user's Allow in Dataman, which shows every statement; the password goes to the system keyring only and is never returned. The login is checked by logging in as it, and removed again if that fails. Afterwards agents connect as it.",
        schema: Schema::new().prop("connection", true, json!({"type": "string", "description": "Connection name (or id) from list_connections."})),
        need: None,
        run: create_agent_login,
    }]
}

fn failed(e: anyhow::Error) -> ToolError {
    ToolError::Failed(format!("{e:#}"))
}

fn create_agent_login(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let p = cx.hub.profile(a.str("connection")?).map_err(ToolError::Invalid)?;
        cx.note_connection(&p.name);
        if p.mcp_access() == McpAccess::Off {
            return Err(ToolError::Refused("MCP access to this connection is off (connection settings › MCP)".into()));
        }
        of_project(cx, &p)?;
        if !agent_login::supported(p.engine) {
            return Err(ToolError::Refused(format!("{} has no logins Dataman can make: {}", p.engine.label(), agent_login::FILE_NOTE)));
        }
        if let Some(l) = &p.agent_login {
            return Err(ToolError::Refused(format!("{} already has an agent login, {}: agents connect as it", p.name, l.user)));
        }
        let admin = cx.hub.connect_user(&p).await.map_err(ToolError::Failed)?;
        let opts = Options::default();
        let rv = live::review(&admin, &p, &opts).await.map_err(failed)?;
        match &rv.probe.manual {
            None => {}
            Some(Manual::NoLogins) => return Err(ToolError::Refused(agent_login::NO_LOGINS_NOTE.into())),
            Some(_) => return Err(ToolError::Refused(format!("{}'s login is set up by hand: the user does it in Dataman (Agents › Protection)", p.name))),
        }
        if let Some(l) = rv.plan.lacks.first() {
            return Err(ToolError::Refused(format!("can't create it: {l}")));
        }
        let mut reasons = vec![format!("creates {}, a database login for agents, as your login", rv.spec.user)];
        if !rv.plan.not_granted.is_empty() {
            let names: Vec<String> = rv.plan.not_granted.iter().map(|n| n.object.clone()).collect();
            reasons.push(format!("can't grant {}: agents won't see it", agent_login::short_list(&names)));
        }
        if !rv.plan.exposed.is_empty() {
            let names: Vec<String> = rv.plan.exposed.iter().map(|n| n.object.clone()).collect();
            reasons.push(format!("every login may read {}: only Dataman keeps it from agents", agent_login::short_list(&names)));
        }
        cx.ask_about(&p, &rv.plan.display(), reasons).await?;
        // As saved now: the user may have made a login, or changed the rules, while this waited.
        let store = ConfigStore::open_with(&cx.hub.dirs, cx.hub.core.approved.clone()).map_err(failed)?;
        let now = store.get(&p.id).ok_or_else(|| ToolError::Refused(format!("{} was removed meanwhile", p.name)))?;
        if now.agent_login.is_some() {
            return Err(ToolError::Refused(format!("{} got an agent login meanwhile: agents connect as it", p.name)));
        }
        if agent_login::rules_hash(&now, "") != agent_login::rules_hash(&p, "") || agent_login::server_of(&now) != agent_login::server_of(&p) {
            return Err(ToolError::Refused(format!("{}'s rules changed while this waited: nothing was created; ask again", p.name)));
        }
        let (rec, checks, warn) = live::create(&cx.hub.core, &admin, &p, &rv, &opts).await.map_err(ToolError::Failed)?;
        let (user, verified, granted, not_granted) = (rec.user.clone(), rec.verified.is_some(), rec.granted, rec.not_granted);
        let mut saved = store.get(&p.id).unwrap_or(now);
        (saved.agent_login, saved.agents_refused) = (Some(rec), false);
        store.upsert(saved).map_err(failed)?;
        cx.hub.forget(&p.id);
        if let Some(f) = cx.hub.front() {
            f.connections_changed();
        }
        let failed: Vec<Value> = checks.iter().filter(|c| !c.ok).map(|c| json!({"check": c.what, "detail": c.detail})).collect();
        let mut out = json!({"connection": p.name, "login": user, "verified": verified, "granted": granted, "not_granted": not_granted, "password": "in the system keyring"});
        if let Some(w) = warn {
            out["warning"] = json!(w);
            out["failed_checks"] = json!(failed);
        }
        Ok(out)
    })
}
