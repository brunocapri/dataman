//! The extension point: a [`Tool`] is a name, a description for the agent, a JSON schema for
//! its arguments, what it needs on its connection, and an async `call`. Built-in tools are
//! registrations like any other; user-defined tools (`mcp-tools.toml`, [`crate::user`]) are
//! reloaded when their file changes.
//!
//! ```ignore
//! struct ServerTime;
//! impl Tool for ServerTime {
//!     fn name(&self) -> &str { "server_time" }
//!     fn description(&self) -> &str { "The database server's clock (now())." }
//!     fn schema(&self) -> &Schema { &SCHEMA } // Schema::new().conn()
//!     fn need(&self) -> Option<Need> { Some(Need::Read) }
//!     fn call<'a>(&'a self, cx: &'a Call, args: Args) -> BoxFut<'a, Result<Value, ToolError>> {
//!         Box::pin(async move { let r = cx.read(&args, "SELECT now()", vec![], None).await?; Ok(r) })
//!     }
//! }
//! registry.register(Arc::new(ServerTime));
//! ```

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use dataman_core::bounded::{self, Limits, Mode, Stop};
use dataman_core::guard::{self, Preview, WriteClass};
use dataman_core::{Connection, ConnectionProfile};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::audit::{AllowedBy, ApprovalNote, Decision};
use crate::hub::{Approval, DRAFTS_NEED_APP, Drafted, Front, Hub, TabDraft, TabRead, TabWrite, WriteShown};
use crate::policy::{self, Need, Target, Verdict};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a call failed; the message is for the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    /// Bad arguments.
    Invalid(String),
    /// Permissions, the guard, the global switch.
    Refused(String),
    /// The server's error, or a connection that failed.
    Failed(String),
    Cancelled,
    Timeout(String),
}

impl ToolError {
    pub fn outcome(&self) -> crate::audit::Outcome {
        use crate::audit::Outcome;
        match self {
            ToolError::Invalid(_) => Outcome::Invalid,
            ToolError::Refused(_) => Outcome::Refused,
            ToolError::Failed(_) => Outcome::Error,
            ToolError::Cancelled => Outcome::Cancelled,
            ToolError::Timeout(_) => Outcome::Timeout,
        }
    }

    pub fn message(&self) -> String {
        match self {
            ToolError::Invalid(m) => format!("invalid arguments: {m}"),
            ToolError::Refused(m) => format!("refused: {m}"),
            ToolError::Failed(m) => m.clone(),
            ToolError::Cancelled => "cancelled".into(),
            ToolError::Timeout(m) => m.clone(),
        }
    }
}

impl From<Stop> for ToolError {
    fn from(s: Stop) -> Self {
        match s {
            Stop::Refused(r) => ToolError::Refused(r),
            Stop::Timeout(_) => ToolError::Timeout(s.to_string()),
            Stop::Cancelled => ToolError::Cancelled,
            Stop::Server(e) => ToolError::Failed(e.to_string()),
            Stop::Other(e) => ToolError::Failed(format!("{e:#}")),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Schemas

/// A tool's argument schema (a JSON Schema object with `additionalProperties: false`), built
/// with small helpers so every tool reads the same.
#[derive(Clone, Debug)]
pub struct Schema(pub Map<String, Value>);

impl Default for Schema {
    fn default() -> Self {
        Schema::new()
    }
}

impl Schema {
    pub fn new() -> Schema {
        let mut m = Map::new();
        m.insert("type".into(), json!("object"));
        m.insert("properties".into(), json!({}));
        m.insert("additionalProperties".into(), json!(false));
        Schema(m)
    }

    pub fn prop(mut self, name: &str, required: bool, spec: Value) -> Schema {
        self.0["properties"].as_object_mut().unwrap().insert(name.into(), spec);
        if required {
            let req = self.0.entry("required").or_insert_with(|| json!([]));
            req.as_array_mut().unwrap().push(json!(name));
        }
        self
    }

    /// `connection` (required) and `database` (optional).
    pub fn conn(self) -> Schema {
        self.prop("connection", true, json!({"type": "string", "description": "Connection name (or id) from list_connections."}))
            .prop("database", false, json!({"type": "string", "description": "Database on that server; default: the connection's own."}))
    }

    pub fn properties(&self) -> &Map<String, Value> {
        self.0["properties"].as_object().unwrap()
    }

    pub fn required(&self) -> Vec<&str> {
        self.0.get("required").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
    }

    /// Checks `args` against the schema: required keys, no unknown keys, and each value's type,
    /// enum, bounds and length.
    pub fn validate(&self, args: &Map<String, Value>) -> Result<(), String> {
        let props = self.properties();
        for k in args.keys() {
            if !props.contains_key(k) {
                let known: Vec<&str> = props.keys().map(String::as_str).collect();
                return Err(format!("unknown argument {k:?} (arguments: {})", known.join(", ")));
            }
        }
        for r in self.required() {
            if args.get(r).is_none_or(Value::is_null) {
                return Err(format!("{r:?} is required"));
            }
        }
        for (k, v) in args {
            if v.is_null() && !self.required().contains(&k.as_str()) {
                continue;
            }
            check_value(k, v, &props[k])?;
        }
        Ok(())
    }
}

/// The database a call is about: its `database`, else the connection's own (as saved).
fn target_database<'a>(args: &'a Args, p: &'a ConnectionProfile) -> &'a str {
    args.opt_str("database").unwrap_or(p.database.as_str())
}

fn type_ok(ty: &str, v: &Value) -> bool {
    match ty {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0 && f.is_finite()),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn check_value(name: &str, v: &Value, spec: &Value) -> Result<(), String> {
    match spec.get("type") {
        Some(Value::String(t)) if !type_ok(t, v) => return Err(format!("{name:?} must be a {t}")),
        Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)) => {
            let ts: Vec<&str> = ts.iter().filter_map(Value::as_str).collect();
            return Err(format!("{name:?} must be one of: {}", ts.join(", ")));
        }
        _ => {}
    }
    if let Some(Value::Array(vals)) = spec.get("enum")
        && !vals.contains(v)
    {
        let vals: Vec<String> = vals.iter().map(|x| x.to_string()).collect();
        return Err(format!("{name:?} must be one of {}", vals.join(", ")));
    }
    if let Some(n) = v.as_f64() {
        if let Some(min) = spec.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return Err(format!("{name:?} must be at least {min}"));
        }
        if let Some(max) = spec.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return Err(format!("{name:?} must be at most {max}"));
        }
    }
    if let (Some(s), Some(max)) = (v.as_str(), spec.get("maxLength").and_then(Value::as_u64))
        && s.chars().count() as u64 > max
    {
        return Err(format!("{name:?} is longer than {max} characters"));
    }
    if let (Some(a), Some(items)) = (v.as_array(), spec.get("items")) {
        if let Some(max) = spec.get("maxItems").and_then(Value::as_u64)
            && a.len() as u64 > max
        {
            return Err(format!("{name:?} has more than {max} items"));
        }
        for (i, x) in a.iter().enumerate() {
            check_value(&format!("{name}[{i}]"), x, items)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Arguments

/// A call's arguments, validated against the tool's schema already.
#[derive(Clone, Debug, Default)]
pub struct Args(pub Map<String, Value>);

impl Args {
    pub fn get(&self, k: &str) -> Option<&Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    pub fn str(&self, k: &str) -> Result<&str, ToolError> {
        self.opt_str(k).ok_or_else(|| ToolError::Invalid(format!("{k:?} is required")))
    }

    pub fn opt_str(&self, k: &str) -> Option<&str> {
        self.get(k).and_then(Value::as_str)
    }

    pub fn bool(&self, k: &str) -> bool {
        self.get(k).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn u64_or(&self, k: &str, default: u64) -> u64 {
        self.get(k).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(default)
    }

    /// `params`: scalars bound to `$1 … $n` as text (`null` is NULL).
    pub fn params(&self) -> Result<Vec<Option<String>>, ToolError> {
        let Some(v) = self.get("params") else { return Ok(Vec::new()) };
        let a = v.as_array().ok_or_else(|| ToolError::Invalid("\"params\" must be an array".into()))?;
        Ok(a.iter()
            .map(|x| match x {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                Value::Bool(b) => Some(b.to_string()),
                Value::Number(n) => Some(n.to_string()),
                // Arrays and objects go as JSON text (for json/jsonb parameters).
                other => Some(other.to_string()),
            })
            .collect())
    }
}

pub fn params_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": 100,
        "items": {"type": ["string", "number", "integer", "boolean", "null", "array", "object"]},
        "description": "Values for $1, $2, … in order (bound by the server, never spliced into the SQL; null is NULL)."
    })
}

// ---------------------------------------------------------------------------------------------
// The call context

#[derive(Default, Clone, Debug)]
pub struct Note {
    pub connection: Option<String>,
    pub database: Option<String>,
    pub sql: Option<String>,
    pub rows: Option<u64>,
    pub approval: Option<ApprovalNote>,
    /// A draft: placed in an agent tab, not run.
    pub draft: bool,
    /// The favorite query `run_favorite` resolved.
    pub favorite: Option<String>,
    /// What let a write through beyond the tier (allowed queries, YOLO).
    pub allowed_by: Option<AllowedBy>,
}

/// An agent's write as [`Call::write`] runs it.
pub struct Write<'a> {
    pub sql: &'a str,
    /// The statement the tiers judge in place of `sql` (EXPLAIN ANALYZE's: the one it runs).
    pub judged: Option<&'a str>,
    pub params: Vec<Option<String>>,
    pub limits: Limits,
    /// Rolled back whatever happens (EXPLAIN ANALYZE): no row limit, but the tiers still apply.
    pub rollback: bool,
    /// Shown in the agent tab while the app runs.
    pub visible: bool,
    pub title: Option<String>,
}

/// What [`Call::write`] did.
pub struct Written {
    pub opened: Opened,
    pub outcome: bounded::Outcome,
}

/// Why a held write did not commit, as its decision saw it.
#[derive(Clone, Debug)]
struct Held {
    decision: Decision,
    reasons: Vec<String>,
    affected: u64,
}

/// One tool call: the hub, the client, the cancellation and what the audit line will say.
pub struct Call {
    pub hub: Arc<Hub>,
    pub client: String,
    pub transport: &'static str,
    pub cancel: CancellationToken,
    pub tool: String,
    note: Mutex<Note>,
}

/// A connection a tool may use, with the database it asked for.
#[derive(Clone)]
pub struct Opened {
    pub profile: ConnectionProfile,
    pub conn: Arc<Connection>,
    pub database: String,
}

impl Call {
    pub fn new(hub: Arc<Hub>, client: String, transport: &'static str, tool: &str, cancel: CancellationToken) -> Call {
        Call { hub, client, transport, cancel, tool: tool.into(), note: Mutex::new(Note::default()) }
    }

    pub fn note(&self) -> Note {
        self.note.lock().clone()
    }

    pub fn note_sql(&self, sql: &str) {
        self.note.lock().sql = Some(sql.to_string());
    }

    pub fn note_rows(&self, n: usize) {
        self.note.lock().rows = Some(n as u64);
    }

    pub fn note_favorite(&self, name: &str) {
        self.note.lock().favorite = Some(name.to_string());
    }

    /// The connection the audit line is about (tools that resolve it themselves).
    pub fn note_connection(&self, name: &str) {
        self.note.lock().connection = Some(name.to_string());
    }

    /// The profile named by `connection`, if the policy lets this tool use it for `need`.
    pub fn target(&self, args: &Args, need: Need) -> Result<ConnectionProfile, ToolError> {
        let name = args.str("connection")?;
        let p = self.hub.profile(name).map_err(ToolError::Invalid)?;
        self.note.lock().connection = Some(p.name.clone());
        let enabled = self.hub.settings().mcp_enabled;
        policy::allow(enabled, Target::of_in(&p, target_database(args, &p)), need).map_err(ToolError::Refused)?;
        Ok(p)
    }

    /// Whether the call's database (`database`, else the connection's) is read-only in
    /// Dataman, before connecting.
    pub fn read_only(&self, args: &Args, p: &ConnectionProfile) -> bool {
        p.read_only_for(target_database(args, p))
    }

    /// [`Call::target`], connected.
    pub async fn open(&self, args: &Args, need: Need) -> Result<Opened, ToolError> {
        let profile = self.target(args, need)?;
        let conn = tokio::select! {
            c = self.hub.connect(&profile) => c.map_err(ToolError::Failed)?,
            _ = self.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        let database = args.opt_str("database").map(str::to_string).unwrap_or_else(|| conn.database().to_string());
        self.note.lock().database = Some(database.clone());
        Ok(Opened { profile, conn, database })
    }

    /// The global switch, without a connection.
    pub fn enabled(&self) -> Result<(), ToolError> {
        if self.hub.settings().mcp_enabled { Ok(()) } else { Err(ToolError::Refused("MCP access is turned off in Dataman (Settings › MCP)".into())) }
    }

    /// The app's side when this call shows in an agent tab: always for `open_query` (`open`),
    /// else as the setting "Show agent queries in tabs" says; never without the app.
    pub fn tab_front(&self, open: bool) -> Option<Arc<dyn Front>> {
        let f = self.hub.front().filter(|f| f.tabs())?;
        (open || self.hub.settings().mcp_show_in_tabs).then_some(f)
    }

    /// A read the caller has checked: in the agent tab when `front` is given (the tab's own
    /// session, the agent gets the bounded view), else bounded on the agent's own backend.
    pub async fn read(&self, args: &Args, sql: &str, params: Vec<Option<String>>, limits: Limits, front: Option<Arc<dyn Front>>, title: Option<String>) -> Result<bounded::Outcome, ToolError> {
        let Some(f) = front else {
            let o = self.open(args, Need::Read).await?;
            return self.run(&o, sql, params, Mode::Read, limits).await;
        };
        let p = self.target(args, Need::Read)?;
        let database = target_database(args, &p).to_string();
        self.note.lock().database = Some(database.clone());
        self.note_sql(sql);
        let _slots = tokio::select! {
            p = self.hub.permit(&p.id) => p,
            _ = self.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        let req = TabRead { client: self.client.clone(), connection_id: p.id.clone(), database, sql: sql.to_string(), params, title, limits, cancel: self.cancel.clone() };
        tokio::select! {
            r = f.run_read(req) => r.map_err(ToolError::from),
            _ = self.cancel.cancelled() => Err(ToolError::Cancelled),
        }
    }

    /// A draft: `sql` into the client's agent tab for the connection, bound to it and the
    /// database, run by nobody (no session, no transaction, no classification, no question: the
    /// user runs it later, as the user). Every tier but off; only with the app open.
    pub async fn draft(&self, args: &Args, sql: &str, title: Option<String>) -> Result<(ConnectionProfile, Drafted), ToolError> {
        self.note_sql(sql);
        let name = args.str("connection")?;
        let p = self.hub.profile(name).map_err(ToolError::Invalid)?;
        let database = target_database(args, &p).to_string();
        {
            let mut n = self.note.lock();
            n.connection = Some(p.name.clone());
            n.database = Some(database.clone());
        }
        policy::draft(self.hub.settings().mcp_enabled, Target::of_in(&p, &database)).map_err(ToolError::Refused)?;
        let Some(f) = self.hub.front().filter(|f| f.tabs()) else { return Err(ToolError::Refused(DRAFTS_NEED_APP.into())) };
        let req = TabDraft { client: self.client.clone(), connection_id: p.id.clone(), database, sql: sql.to_string(), title };
        let d = tokio::select! {
            r = f.draft(req) => r.map_err(ToolError::Failed)?,
            _ = self.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        let mut n = self.note.lock();
        n.draft = true;
        n.database = Some(d.database.clone());
        drop(n);
        Ok((p, d))
    }

    fn note_decision(&self, decision: Decision, reasons: Vec<String>, rows: Option<u64>) {
        let by = matches!(decision, Decision::Approved | Decision::Denied).then(crate::audit::local_user);
        self.note.lock().approval = Some(ApprovalNote { decision, by, reasons, rows });
    }

    fn approval(&self, o: &Opened, sql: &str, reasons: Vec<String>, affected: Option<u64>, preview: Option<(Vec<String>, Vec<Vec<String>>)>, tab: Option<u64>) -> Approval {
        Approval {
            client: self.client.clone(),
            tool: self.tool.clone(),
            connection: o.profile.name.clone(),
            connection_id: o.profile.id.clone(),
            database: o.database.clone(),
            env: o.profile.env,
            sql: sql.to_string(),
            reasons,
            destructive: false,
            affected,
            preview,
            tab,
            allowable: Vec::new(),
        }
    }

    /// Asks the human in the app, within the hub's approval timeout (denied after it); an
    /// agent's cancel withdraws the question.
    async fn ask(&self, a: Approval) -> Result<Decision, ToolError> {
        let Some(f) = self.hub.front() else { return Ok(Decision::Unavailable) };
        let wait = tokio::time::timeout(self.hub.approval_timeout(), f.approve(a));
        tokio::select! {
            r = wait => Ok(match r {
                Ok(true) => Decision::Approved,
                Ok(false) => Decision::Denied,
                Err(_) => Decision::Timeout,
            }),
            _ = self.cancel.cancelled() => Err(ToolError::Cancelled),
        }
    }

    /// Asks the human in the app about an action the tier lets through only with an Allow
    /// (a write over the row limit that cannot be held in a transaction): refused unless
    /// allowed; the decision goes to the audit line.
    pub async fn ask_human(&self, o: &Opened, sql: &str, reasons: Vec<String>, affected: Option<u64>, destructive: bool) -> Result<(), ToolError> {
        let a = Approval { destructive, ..self.approval(o, sql, reasons.clone(), affected, None, None) };
        let d = self.ask(a).await?;
        self.note_decision(d, reasons.clone(), affected);
        if d == Decision::Approved { Ok(()) } else { Err(refusal_of(d, &reasons, false)) }
    }

    /// The tier's verdict on an action that cannot wait in a transaction (a signal, a write
    /// asked about before it runs): refused, allowed, or asked about now.
    pub async fn gate(&self, o: &Opened, class: &WriteClass, sql: &str, tab: Option<u64>) -> Result<(), ToolError> {
        let t = Target::of_in(&o.profile, &o.database);
        match policy::write_verdict(self.hub.settings().mcp_enabled, t, class) {
            Verdict::Refuse(e) => Err(ToolError::Refused(e)),
            Verdict::Run => {
                if t.yolo && class.is_write() {
                    self.note.lock().allowed_by = Some(AllowedBy::Yolo);
                }
                Ok(())
            }
            Verdict::Ask(r) => {
                let a = Approval { destructive: class.is_destructive(), ..self.approval(o, sql, r.clone(), None, None, tab) };
                let d = self.ask(a).await?;
                self.note_decision(d, r.clone(), None);
                if d == Decision::Approved { Ok(()) } else { Err(refusal_of(d, &r, false)) }
            }
        }
    }

    /// An agent's write through the tiers (see [`crate::policy`]): classified, refused before
    /// anything connects when its tier says so (or when it needs a human and no app runs), else
    /// run. A write that must be asked about runs in a transaction held open for the answer
    /// (its rows counted, its RETURNING read) when it can be, else is asked about first; one that
    /// changes more rows than the connection's limit asks too. The connection's allowed queries
    /// and YOLO run without a question and without the limit ([`policy::judge`]). Committed only
    /// when allowed; shown in the agent tab when `w.visible`; the decision (or what let it
    /// through) goes to the audit line.
    pub async fn write(&self, args: &Args, w: Write<'_>) -> Result<Written, ToolError> {
        self.note_sql(w.sql);
        let p = self.hub.profile(args.str("connection")?).map_err(ToolError::Invalid)?;
        self.note.lock().connection = Some(p.name.clone());
        let d = p.engine.dialect();
        let stmts = bounded::statements_in(w.sql, d);
        if stmts.is_empty() {
            return Err(ToolError::Invalid("empty statement".into()));
        }
        let ro = self.read_only(args, &p);
        let judged_sql = w.judged.unwrap_or(w.sql);
        let class = guard::agent_write_class(judged_sql, d);
        let preview = guard::agent_preview(w.sql, d);
        let judged = policy::judge(self.hub.settings().mcp_enabled, Target { read_only: ro, ..Target::of(&p) }, judged_sql, p.mcp_allowed_on());
        let ask: Option<Vec<String>> = match judged.verdict {
            Verdict::Refuse(e) => return Err(ToolError::Refused(e)),
            Verdict::Ask(r) => Some(r),
            Verdict::Run => None,
        };
        let mode = match (w.rollback, preview) {
            (true, _) => Mode::Rollback,
            (false, Preview::Outside) => Mode::Autocommit,
            _ => Mode::Write,
        };
        if let Some(r) = bounded::refusal_in(&stmts, w.params.len(), mode, ro, d) {
            return Err(ToolError::Refused(r));
        }
        self.note.lock().allowed_by = judged.by.as_ref().map(policy::By::audit);
        // What "Always allow" in the app would add to the connection's allowed queries.
        let allowable = guard::allowlist_candidates(judged_sql, d);
        let front = self.hub.front();
        if let Some(r) = &ask
            && front.is_none()
        {
            self.note_decision(Decision::Unavailable, r.clone(), None);
            return Err(ToolError::Refused(policy::needs_app(r)));
        }
        // The matrix has judged the write already (an allowed query runs in a read tier too).
        let o = self.open(args, Need::Read).await?;
        let tab = match front.as_ref().filter(|f| w.visible && f.tabs()) {
            Some(f) => {
                let req = TabWrite { client: self.client.clone(), connection_id: p.id.clone(), database: o.database.clone(), sql: w.sql.to_string(), title: w.title.clone() };
                f.show_write(req).await
            }
            None => None,
        };
        let show = |shown: WriteShown| {
            if let (Some(f), Some(tab)) = (&front, tab) {
                f.write_done(tab, w.sql.to_string(), shown);
            }
        };
        // Asked about before it runs: nothing reaches the server until the answer.
        if let Some(r) = ask.clone()
            && (preview != Preview::Held || mode == Mode::Rollback)
        {
            let a = Approval { destructive: class.is_destructive(), allowable: allowable.clone(), ..self.approval(&o, w.sql, r.clone(), None, None, tab) };
            let dec = match self.ask(a).await {
                Ok(d) => d,
                Err(e) => {
                    show(WriteShown::Failed(e.message()));
                    return Err(e);
                }
            };
            self.note_decision(dec, r.clone(), None);
            if dec != Decision::Approved {
                let e = refusal_of(dec, &r, false);
                show(WriteShown::Failed(e.message()));
                return Err(e);
            }
            return match self.run(&o, w.sql, w.params, mode, w.limits).await {
                Ok(out) => {
                    show(WriteShown::Ran { outcome: out.clone(), note: None });
                    Ok(Written { opened: o, outcome: out })
                }
                Err(e) => {
                    show(WriteShown::Failed(e.message()));
                    Err(e)
                }
            };
        }
        // Held: the statements run, then the decision commits or rolls back.
        let limit = if mode == Mode::Rollback || !judged.row_limit { u64::MAX } else { self.hub.row_limit(&p) };
        let held: Arc<Mutex<Option<Held>>> = Arc::new(Mutex::new(None));
        let decide: bounded::Decide = {
            let (held, front, ask) = (held.clone(), front.clone(), ask.clone());
            let base = Approval { destructive: class.is_destructive(), allowable: allowable.clone(), ..self.approval(&o, w.sql, Vec::new(), None, None, tab) };
            let timeout = self.hub.approval_timeout();
            Box::new(move |out: &bounded::Outcome| {
                let affected = out.affected();
                let mut why = ask.unwrap_or_default();
                if let Some(x) = policy::over_limit(affected, limit) {
                    why.push(x);
                }
                if why.is_empty() {
                    return Box::pin(async { true });
                }
                let Some(front) = front else {
                    *held.lock() = Some(Held { decision: Decision::Unavailable, reasons: why, affected });
                    return Box::pin(async { false });
                };
                let a = Approval { reasons: why.clone(), affected: Some(affected), preview: preview_rows(out), ..base };
                Box::pin(async move {
                    let decision = match tokio::time::timeout(timeout, front.approve(a)).await {
                        Ok(true) => Decision::Approved,
                        Ok(false) => Decision::Denied,
                        Err(_) => Decision::Timeout,
                    };
                    *held.lock() = Some(Held { decision, reasons: why, affected });
                    decision == Decision::Approved
                })
            })
        };
        let r = self.run_held(&o, w.sql, w.params, mode, w.limits, Some(decide)).await;
        let held = held.lock().take();
        if let Some(h) = &held {
            self.note_decision(h.decision, h.reasons.clone(), Some(h.affected));
        }
        match r {
            Ok(out) if out.committed || mode == Mode::Rollback => {
                show(WriteShown::Ran { outcome: out.clone(), note: None });
                Ok(Written { opened: o, outcome: out })
            }
            Ok(out) => {
                let e = match &held {
                    Some(h) => refusal_of(h.decision, &h.reasons, true),
                    None => ToolError::Failed("the write was not committed".into()),
                };
                show(WriteShown::Ran { outcome: out, note: Some(e.message()) });
                Err(e)
            }
            Err(e) => {
                show(WriteShown::Failed(e.message()));
                Err(e)
            }
        }
    }

    /// The limits for this call: rows asked (capped by the settings), timeout asked (≤ 5 min).
    pub fn limits(&self, args: &Args, default_rows: u64) -> Limits {
        let cap = self.hub.settings().mcp_max_rows.max(1) as u64;
        Limits {
            max_rows: args.u64_or("max_rows", default_rows).clamp(1, cap) as usize,
            timeout: Duration::from_millis(args.u64_or("timeout_ms", 30_000).clamp(100, 300_000)),
            ..Limits::default()
        }
    }

    /// Runs SQL bounded on an opened connection, holding one of its agent slots.
    pub async fn run(&self, o: &Opened, sql: &str, params: Vec<Option<String>>, mode: Mode, limits: Limits) -> Result<bounded::Outcome, ToolError> {
        self.run_held(o, sql, params, mode, limits, None).await
    }

    /// [`Call::run`] with a decision before the commit ([`Connection::bounded_held`]).
    pub async fn run_held(&self, o: &Opened, sql: &str, params: Vec<Option<String>>, mode: Mode, limits: Limits, decide: Option<bounded::Decide>) -> Result<bounded::Outcome, ToolError> {
        if self.note.lock().sql.is_none() {
            self.note_sql(sql);
        }
        let _slots = tokio::select! {
            p = self.hub.permit(&o.profile.id) => p,
            _ = self.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        let cancel = self.cancel.clone().cancelled_owned();
        Ok(o.conn.bounded_held(Some(&o.database), sql, params, mode, limits, cancel, decide).await?)
    }
}

/// The agent's error for a write a human (or nobody) did not allow.
fn refusal_of(d: Decision, reasons: &[String], rolled_back: bool) -> ToolError {
    let rb = if rolled_back { "; rolled back" } else { "" };
    match d {
        Decision::Denied => ToolError::Refused(format!("denied in Dataman ({}){rb}", reasons.join(", "))),
        Decision::Timeout => ToolError::Timeout(format!("no answer in Dataman in time ({}){rb}", reasons.join(", "))),
        Decision::Unavailable => ToolError::Refused(format!("{}{rb}", policy::needs_app(reasons))),
        Decision::Approved => ToolError::Failed("the write was not committed".into()),
    }
}

/// The first RETURNING / OUTPUT rows of a held write, as text, for the approval sheet.
fn preview_rows(o: &bounded::Outcome) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    if o.columns.is_empty() || o.rows.is_empty() {
        return None;
    }
    let text = |v: &Value| match v {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let rows = o.rows.iter().take(5).map(|r| r.iter().map(text).collect()).collect();
    Some((o.columns.iter().map(|c| c.name.clone()).collect(), rows))
}

// ---------------------------------------------------------------------------------------------
// The trait and the registry

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    /// For the agent: what it does, what it returns, its limits. Short and exact.
    fn description(&self) -> &str;
    fn schema(&self) -> &Schema;
    /// What it needs on its connection (`None`: no connection).
    fn need(&self) -> Option<Need>;
    fn call<'a>(&'a self, cx: &'a Call, args: Args) -> BoxFut<'a, Result<Value, ToolError>>;
}

/// The tools: built-ins registered once, user tools reloaded from their file when it changes.
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
    file: Option<PathBuf>,
    user: Mutex<UserSet>,
}

#[derive(Default)]
struct UserSet {
    stamp: Option<SystemTime>,
    tools: Vec<Arc<dyn Tool>>,
    errors: Vec<String>,
}

impl Registry {
    pub fn new(user_file: Option<PathBuf>) -> Registry {
        Registry { tools: Vec::new(), file: user_file, user: Mutex::new(UserSet::default()) }
    }

    /// All built-in tools, and the user's from `user_file`.
    pub fn standard(user_file: Option<PathBuf>) -> Registry {
        let mut r = Registry::new(user_file);
        crate::builtin::register(&mut r);
        r
    }

    pub fn register(&mut self, t: Arc<dyn Tool>) {
        assert!(!self.tools.iter().any(|x| x.name() == t.name()), "tool {} registered twice", t.name());
        self.tools.push(t);
    }

    pub fn builtin_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Reloads the user tools when their file changed; true when the list changed.
    pub fn refresh(&self) -> bool {
        let Some(file) = &self.file else { return false };
        let stamp = std::fs::metadata(file).and_then(|m| m.modified()).ok();
        let mut g = self.user.lock();
        if g.stamp == stamp {
            return false;
        }
        g.stamp = stamp;
        let (tools, errors) = match std::fs::read_to_string(file) {
            Ok(text) => crate::user::load(&text, &self.builtin_names()),
            Err(_) => (Vec::new(), Vec::new()),
        };
        for e in &errors {
            log::warn!("{}: {e}", file.display());
        }
        let before: Vec<String> = g.tools.iter().map(|t| format!("{}\n{}\n{}", t.name(), t.description(), serde_json::Value::Object(t.schema().0.clone()))).collect();
        let after: Vec<String> = tools.iter().map(|t| format!("{}\n{}\n{}", t.name(), t.description(), serde_json::Value::Object(t.schema().0.clone()))).collect();
        g.tools = tools;
        g.errors = errors;
        before != after
    }

    /// The problems of the user tools file, as last read.
    pub fn user_errors(&self) -> Vec<String> {
        self.user.lock().errors.clone()
    }

    pub fn all(&self) -> Vec<Arc<dyn Tool>> {
        self.refresh();
        let mut v = self.tools.clone();
        v.extend(self.user.lock().tools.iter().cloned());
        v
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.refresh();
        self.tools.iter().find(|t| t.name() == name).cloned().or_else(|| self.user.lock().tools.iter().find(|t| t.name() == name).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn validation() {
        let s = Schema::new()
            .conn()
            .prop("sql", true, json!({"type": "string", "maxLength": 10}))
            .prop("max_rows", false, json!({"type": "integer", "minimum": 1, "maximum": 5}))
            .prop("format", false, json!({"type": "string", "enum": ["text", "json"]}))
            .prop("params", false, params_schema());
        assert!(s.validate(&args(json!({"connection": "c", "sql": "select"}))).is_ok());
        let e = |v: Value| s.validate(&args(v)).unwrap_err();
        assert!(e(json!({"sql": "x"})).contains("\"connection\" is required"));
        assert!(e(json!({"connection": "c", "sql": "x", "sq": 1})).contains("unknown argument \"sq\""));
        assert!(e(json!({"connection": "c", "sql": 3})).contains("must be a string"));
        assert!(e(json!({"connection": "c", "sql": "01234567890"})).contains("longer than 10"));
        assert!(e(json!({"connection": "c", "sql": "x", "max_rows": 9})).contains("at most 5"));
        assert!(e(json!({"connection": "c", "sql": "x", "max_rows": 1.5})).contains("integer"));
        assert!(e(json!({"connection": "c", "sql": "x", "format": "xml"})).contains("one of"));
        assert!(s.validate(&args(json!({"connection": "c", "sql": "x", "max_rows": 3.0}))).is_ok());
        assert!(s.validate(&args(json!({"connection": "c", "sql": "x", "format": null}))).is_ok(), "null is absent");
        assert!(s.validate(&args(json!({"connection": "c", "sql": "x", "params": [1, "a", null, true, {"k": 1}]}))).is_ok());
        let p = Args(args(json!({"params": [1, "a", null, true, 2.5, {"k": 1}]}))).params().unwrap();
        assert_eq!(p, vec![Some("1".into()), Some("a".into()), None, Some("true".into()), Some("2.5".into()), Some("{\"k\":1}".into())]);
    }
}
