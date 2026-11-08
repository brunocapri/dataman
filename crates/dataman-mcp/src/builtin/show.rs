//! Takes the user to a place in Dataman when an agent needs them there. It navigates and fills a
//! sheet, from a fixed list of places: nothing is saved until the user clicks, and nothing an agent
//! sends here changes a setting. A connection closed to agents (MCP off, agents refused) is not a
//! place it can send anyone, and what comes back says no more of a connection than
//! list_connections does.

use dataman_core::{ConnectionProfile, McpAccess, agent_login};
use serde_json::{Value, json};

use super::Builtin;
use super::connections::{apply, engine_prop, guarded_twin};
use crate::hub::{Place, SHOW_NEEDS_APP, invisible, refusal};
use crate::policy::Need;
use crate::tool::{Args, BoxFut, Call, Schema, ToolError};

type Build = fn(&Call, &Args) -> Result<Place, ToolError>;

const DESTINATIONS: [(&str, Build); 7] = [
    ("new_connection", new_connection),
    ("connection", |cx, a| Ok(Place::Connection(saved(cx, a)?.id))),
    ("agents_protection", |cx, a| Ok(Place::Protection(saved(cx, a)?.id))),
    ("agent_login", agent_login_of),
    ("agents_general", |_, _| Ok(Place::AgentsGeneral)),
    ("settings_mcp", |_, _| Ok(Place::SettingsMcp)),
    ("outside_review", |_, _| Ok(Place::OutsideReview)),
];

/// What new_connection fills in: where a connection goes, never how it signs in or what agents
/// may do on it.
const FILLS: [&str; 6] = ["name", "engine", "host", "port", "user", "database"];

pub fn tools() -> Vec<Builtin> {
    let engines: Vec<&str> = dataman_core::server::names().into_iter().filter(|n| dataman_core::server::named(n).is_some_and(|(e, _)| !e.is_file())).collect();
    vec![Builtin {
        name: "show_in_dataman",
        description: "Takes the user to a place in Dataman and brings its window forward, when you need them there. Answers at once, without waiting for them: check afterwards (list_connections). It only opens the place: nothing is saved until the user clicks, and it changes no setting. Dataman must be open; if it is closed this says so, and you ask the user to open it. Refused on a connection with MCP access off or agents refused, while a sheet is open in Dataman, and past a few times a minute.",
        schema: Schema::new()
            .prop(
                "destination",
                true,
                json!({"type": "string", "enum": DESTINATIONS.map(|d| d.0), "description": "new_connection: the new connection dialog, filled with the fields given, for the user to type the password and save. connection: a saved connection's dialog (its password, or to adjust it). agents_protection: Agents › Protection on that connection (tiers, writes, what agents see). agent_login: its agent login. agents_general: Agents › General. settings_mcp: Settings › MCP. outside_review: edits made outside Dataman that wait for review."}),
            )
            .prop("connection", false, json!({"type": "string", "description": "connection, agents_protection, agent_login: the connection's name (or id) from list_connections."}))
            .prop("name", false, json!({"type": "string", "maxLength": 200, "description": "new_connection: a short label, unique (case-insensitive)."}))
            .prop("engine", false, engine_prop(engines, "new_connection: default postgres. A sqlite or duckdb file has no password: use create_connection."))
            .prop("host", false, json!({"type": "string", "maxLength": 255, "description": "new_connection: the host."}))
            .prop("port", false, json!({"type": "integer", "minimum": 1, "maximum": 65535, "description": "new_connection: default: the engine's own."}))
            .prop("user", false, json!({"type": "string", "maxLength": 200, "description": "new_connection: the database user."}))
            .prop("database", false, json!({"type": "string", "maxLength": 200, "description": "new_connection: the database."})),
        need: None,
        run: show_in_dataman,
    }]
}

fn saved(cx: &Call, a: &Args) -> Result<ConnectionProfile, ToolError> {
    let p = cx.target(a, Need::Read)?;
    refusal(&p).map_err(ToolError::Refused)?;
    Ok(p)
}

fn agent_login_of(cx: &Call, a: &Args) -> Result<Place, ToolError> {
    let p = saved(cx, a)?;
    if !agent_login::supported(p.engine) {
        return Err(ToolError::Refused(format!("{} has no logins Dataman can make: {}", p.engine.label(), agent_login::FILE_NOTE)));
    }
    Ok(Place::AgentLogin(p.id))
}

/// The sheet as an agent filled it. A twin of a guarded connection (see `guarded_twin`) is not
/// refused, as that would tell the agent where a closed connection points: it starts with MCP off,
/// and what saving it gives agents is what the user then picks.
fn new_connection(cx: &Call, a: &Args) -> Result<Place, ToolError> {
    for k in ["name", "host", "user", "database"] {
        if a.opt_str(k).is_some_and(|v| v.chars().any(|c| invisible(c) || c.is_control())) {
            return Err(ToolError::Invalid(format!("{k:?} holds characters that draw as nothing: the user must see what they save")));
        }
    }
    let name = a.opt_str("name").unwrap_or_default().trim();
    if !name.is_empty() {
        cx.note_connection(name);
    }
    let profiles = cx.hub.profiles();
    if !name.is_empty() && profiles.iter().any(|p| p.name.eq_ignore_ascii_case(name)) {
        return Err(ToolError::Refused(format!("a connection named {name:?} already exists; its dialog is the destination connection")));
    }
    // Filled as create_connection fills one, and marked: the host is the agent's choice until saved.
    let mut p = ConnectionProfile { agent_made: true, ..ConnectionProfile::new(name) };
    apply(&mut p, a)?;
    if guarded_twin(&profiles, &p).is_some() {
        p.mcp = Some(McpAccess::Off);
    }
    Ok(Place::NewConnection(Box::new(p)))
}

fn show_in_dataman(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let dest = a.str("destination")?;
        let Some((_, build)) = DESTINATIONS.iter().find(|d| d.0 == dest) else {
            return Err(ToolError::Invalid(format!("unknown destination {dest:?}")));
        };
        let place = build(cx, &a)?;
        let fills = matches!(place, Place::NewConnection(_));
        let on_saved = matches!(place, Place::Connection(_) | Place::Protection(_) | Place::AgentLogin(_));
        let wrong = FILLS.iter().filter(|_| !fills).chain(["connection"].iter().filter(|_| !on_saved)).find(|k| a.get(k).is_some());
        if let Some(k) = wrong {
            return Err(ToolError::Invalid(format!("{dest} does not take {k:?}")));
        }
        let Some(front) = cx.hub.front() else { return Err(ToolError::Refused(SHOW_NEEDS_APP.into())) };
        cx.hub.show_slot(&cx.client).map_err(ToolError::Refused)?;
        let shown = tokio::select! {
            r = front.show(place) => r,
            _ = cx.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        if let Err(e) = shown {
            cx.hub.show_not_used();
            return Err(ToolError::Refused(e));
        }
        Ok(match cx.note().connection {
            Some(c) => json!({"shown": dest, "connection": c}),
            None => json!({"shown": dest}),
        })
    })
}
