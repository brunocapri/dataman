use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    Invalid(String),
    Refused(String),
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

    /// The same error with its text passed through `f` (errors are cleaned before they leave).
    pub fn map_text(self, f: impl FnOnce(&str) -> String) -> ToolError {
        match self {
            ToolError::Invalid(m) => ToolError::Invalid(f(&m)),
            ToolError::Refused(m) => ToolError::Refused(f(&m)),
            ToolError::Failed(m) => ToolError::Failed(f(&m)),
            ToolError::Timeout(m) => ToolError::Timeout(f(&m)),
            ToolError::Cancelled => ToolError::Cancelled,
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

    pub fn params(&self) -> Result<Vec<Option<String>>, ToolError> {
        let Some(v) = self.get("params") else { return Ok(Vec::new()) };
        let a = v.as_array().ok_or_else(|| ToolError::Invalid("\"params\" must be an array".into()))?;
        Ok(a.iter()
            .map(|x| match x {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                Value::Bool(b) => Some(b.to_string()),
                Value::Number(n) => Some(n.to_string()),
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

#[derive(Default, Clone, Debug)]
pub struct Note {
    pub connection: Option<String>,
    pub database: Option<String>,
    pub sql: Option<String>,
    pub rows: Option<u64>,
    pub approval: Option<ApprovalNote>,
    pub draft: bool,
    pub favorite: Option<String>,
    pub allowed_by: Option<AllowedBy>,
    pub rewritten: Option<String>,
    pub seen: crate::audit::Seen,
}

pub struct Write<'a> {
    pub sql: &'a str,
    pub judged: Option<&'a str>,
    pub params: Vec<Option<String>>,
    pub limits: Limits,
    /// Rolled back whatever happens (EXPLAIN ANALYZE): no row limit, but the tiers still apply.
    pub rollback: bool,
    pub visible: bool,
    pub title: Option<String>,
}

pub struct Written {
    pub opened: Opened,
    pub outcome: bounded::Outcome,
}

#[derive(Clone, Debug)]
struct Counted {
    why: Vec<String>,
    affected: u64,
    preview: Option<(Vec<String>, Vec<Vec<String>>)>,
}

pub struct Call {
    pub hub: Arc<Hub>,
    pub client: String,
    pub transport: &'static str,
    pub cancel: CancellationToken,
    pub tool: String,
    note: Mutex<Note>,
}

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

    pub fn note_connection(&self, name: &str) {
        self.note.lock().connection = Some(name.to_string());
    }

    pub fn note_rewritten(&self, sql: &str) {
        self.note.lock().rewritten = Some(sql.to_string());
    }

    pub fn note_columns(&self, n: usize) {
        self.note.lock().seen.columns = Some(n as u64);
    }

    pub fn note_masked_columns(&self, cols: &[(String, dataman_core::privacy::Mask)]) {
        let mut g = self.note.lock();
        for c in cols {
            if !g.seen.masked.contains(c) {
                g.seen.masked.push(c.clone());
            }
        }
    }

    pub fn note_masked_values(&self, counts: &dataman_core::privacy::Counts) {
        let mut g = self.note.lock();
        for (k, n) in counts {
            *g.seen.values.entry(k.clone()).or_default() += n;
        }
    }

    /// The connection's rules (none for a call without one).
    pub fn rules_of(&self, args: &Args) -> dataman_core::privacy::AgentRules {
        args.opt_str("connection").and_then(|c| self.hub.profile(c).ok()).map(|p| p.mcp_rules).unwrap_or_default()
    }

    /// Time per call: what the agent asks (`timeout_ms`, 30 s by default) within the connection's limit.
    pub fn time_limit(&self, args: &Args) -> Duration {
        let cap = self.rules_of(args).time_limit().as_millis() as u64;
        Duration::from_millis(args.u64_or("timeout_ms", 30_000).min(cap).clamp(100, 300_000))
    }

    pub fn target(&self, args: &Args, need: Need) -> Result<ConnectionProfile, ToolError> {
        let name = args.str("connection")?;
        let p = self.hub.profile(name).map_err(ToolError::Invalid)?;
        self.note.lock().connection = Some(p.name.clone());
        let enabled = self.hub.settings().mcp_enabled;
        policy::allow(enabled, Target::of_in(&p, target_database(args, &p)), need).map_err(ToolError::Refused)?;
        Ok(p)
    }

    pub fn read_only(&self, args: &Args, p: &ConnectionProfile) -> bool {
        p.read_only_for(target_database(args, p))
    }

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

    pub fn enabled(&self) -> Result<(), ToolError> {
        if self.hub.settings().mcp_enabled { Ok(()) } else { Err(ToolError::Refused("MCP access is turned off in Dataman (Settings › MCP)".into())) }
    }

    pub fn tab_front(&self, open: bool) -> Option<Arc<dyn Front>> {
        let f = self.hub.front().filter(|f| f.tabs())?;
        (open || self.hub.settings().mcp_show_in_tabs).then_some(f)
    }

    pub async fn read(&self, args: &Args, sql: &str, params: Vec<Option<String>>, limits: Limits, front: Option<Arc<dyn Front>>, title: Option<String>) -> Result<bounded::Outcome, ToolError> {
        let Some(f) = front else {
            let o = self.open(args, Need::Read).await?;
            return self.run(&o, sql, params, Mode::Read, limits).await;
        };
        let p = self.target(args, Need::Read)?;
        let database = target_database(args, &p).to_string();
        self.note.lock().database = Some(database.clone());
        if self.note.lock().sql.is_none() {
            self.note_sql(sql);
        }
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
            counted: false,
        }
    }

    async fn ask(&self, a: Approval) -> Result<Decision, ToolError> {
        let Some(f) = self.hub.front() else { return Ok(Decision::Unavailable) };
        let _slot = self.hub.ask_slot(&self.client).map_err(ToolError::Refused)?;
        let wait = tokio::time::timeout(self.hub.approval_timeout(), f.approve(a));
        let d = tokio::select! {
            r = wait => match r {
                Ok(true) => Decision::Approved,
                Ok(false) => Decision::Denied,
                Err(_) => Decision::Timeout,
            },
            _ = self.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        if d == Decision::Denied {
            self.hub.denied(&self.client);
        }
        Ok(d)
    }

    pub async fn ask_human(&self, o: &Opened, sql: &str, reasons: Vec<String>, affected: Option<u64>, destructive: bool) -> Result<(), ToolError> {
        let a = Approval { destructive, ..self.approval(o, sql, reasons.clone(), affected, None, None) };
        let d = self.ask(a).await?;
        self.note_decision(d, reasons.clone(), affected);
        if d == Decision::Approved { Ok(()) } else { Err(refusal_of(d, &reasons, false)) }
    }

    /// The user's Allow for a change that is not a statement (`what` is shown as the text).
    pub async fn ask_about(&self, p: &ConnectionProfile, what: &str, reasons: Vec<String>) -> Result<(), ToolError> {
        let a = Approval { connection: p.name.clone(), connection_id: p.id.clone(), database: p.database.clone(), env: p.env, ..self.plain_approval(what, &reasons) };
        self.decide(a, reasons).await
    }

    /// The same for something on this Mac, not a connection: `title` heads the sheet.
    pub async fn ask_local(&self, title: &str, what: &str, reasons: Vec<String>) -> Result<(), ToolError> {
        let a = Approval { connection: title.to_string(), ..self.plain_approval(what, &reasons) };
        self.decide(a, reasons).await
    }

    fn plain_approval(&self, what: &str, reasons: &[String]) -> Approval {
        Approval { client: self.client.clone(), tool: self.tool.clone(), sql: what.to_string(), reasons: reasons.to_vec(), destructive: true, ..Approval::default() }
    }

    async fn decide(&self, a: Approval, reasons: Vec<String>) -> Result<(), ToolError> {
        let d = self.ask(a).await?;
        self.note_decision(d, reasons.clone(), None);
        if d == Decision::Approved { Ok(()) } else { Err(refusal_of(d, &reasons, false)) }
    }

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
        let limit = if mode == Mode::Rollback || !judged.row_limit { u64::MAX } else { self.hub.row_limit(&p) };
        // First run: nothing to ask, it commits. Something to ask: counted, then rolled back at
        // once, so no row stays locked while the user decides.
        let counted: Arc<Mutex<Option<Counted>>> = Arc::new(Mutex::new(None));
        let decide: bounded::Decide = {
            let (counted, ask) = (counted.clone(), ask.clone());
            Box::new(move |out: &bounded::Outcome| {
                let affected = out.affected();
                let mut why = ask.unwrap_or_default();
                if let Some(x) = policy::over_limit(affected, limit) {
                    why.push(x);
                }
                let commit = why.is_empty();
                if !commit {
                    *counted.lock() = Some(Counted { why, affected, preview: preview_rows(out) });
                }
                Box::pin(async move { commit })
            })
        };
        let r = self.run_held(&o, w.sql, w.params.clone(), mode, w.limits, Some(decide)).await;
        let counted = counted.lock().take();
        let out = match r {
            Ok(out) if out.committed || mode == Mode::Rollback => {
                show(WriteShown::Ran { outcome: out.clone(), note: None });
                return Ok(Written { opened: o, outcome: out });
            }
            Ok(out) => out,
            Err(e) => {
                show(WriteShown::Failed(e.message()));
                return Err(e);
            }
        };
        let Some(c) = counted else {
            let e = ToolError::Failed("the write was not committed".into());
            show(WriteShown::Ran { outcome: out, note: Some(e.message()) });
            return Err(e);
        };
        let a = Approval {
            destructive: class.is_destructive(),
            allowable: allowable.clone(),
            affected: Some(c.affected),
            preview: c.preview.clone(),
            counted: true,
            ..self.approval(&o, w.sql, c.why.clone(), None, None, tab)
        };
        let dec = match self.ask(a).await {
            Ok(d) => d,
            Err(e) => {
                show(WriteShown::Failed(e.message()));
                return Err(e);
            }
        };
        self.note_decision(dec, c.why.clone(), Some(c.affected));
        if dec != Decision::Approved {
            let e = refusal_of(dec, &c.why, true);
            show(WriteShown::Ran { outcome: out, note: Some(e.message()) });
            return Err(e);
        }
        // Run again: committed only if it changes no more rows than the user saw.
        let allowed = c.affected;
        let grew: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let decide: bounded::Decide = {
            let grew = grew.clone();
            Box::new(move |out: &bounded::Outcome| {
                let n = out.affected();
                let commit = n <= allowed;
                if !commit {
                    *grew.lock() = Some(n);
                }
                Box::pin(async move { commit })
            })
        };
        let r = self.run_held(&o, w.sql, w.params, mode, w.limits, Some(decide)).await;
        match r {
            Ok(out) if out.committed => {
                show(WriteShown::Ran { outcome: out.clone(), note: None });
                Ok(Written { opened: o, outcome: out })
            }
            Ok(out) => {
                let e = match grew.lock().take() {
                    Some(n) => ToolError::Refused(format!("by the time it ran it changed {n} rows, {allowed} were allowed in Dataman; rolled back, call again to ask anew")),
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

    pub fn limits(&self, args: &Args, default_rows: u64) -> Limits {
        let rules = self.rules_of(args);
        let cap = (self.hub.settings().mcp_max_rows.max(1) as u64).min(rules.read_rows.unwrap_or(u64::MAX)).max(1);
        let base = Limits::default();
        Limits {
            max_rows: args.u64_or("max_rows", default_rows).clamp(1, cap) as usize,
            timeout: self.time_limit(args),
            max_bytes: rules.read_bytes.map_or(base.max_bytes, |b| (b as usize).min(base.max_bytes)),
            ..base
        }
    }

    pub async fn run(&self, o: &Opened, sql: &str, params: Vec<Option<String>>, mode: Mode, limits: Limits) -> Result<bounded::Outcome, ToolError> {
        self.run_held(o, sql, params, mode, limits, None).await
    }

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

fn refusal_of(d: Decision, reasons: &[String], rolled_back: bool) -> ToolError {
    let rb = if rolled_back { "; rolled back" } else { "" };
    match d {
        Decision::Denied => ToolError::Refused(format!("denied in Dataman ({}){rb}", reasons.join(", "))),
        Decision::Timeout => ToolError::Timeout(format!("no answer in Dataman in time ({}){rb}", reasons.join(", "))),
        Decision::Unavailable => ToolError::Refused(format!("{}{rb}", policy::needs_app(reasons))),
        Decision::Approved => ToolError::Failed("the write was not committed".into()),
    }
}

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

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> &Schema;
    fn need(&self) -> Option<Need>;
    fn call<'a>(&'a self, cx: &'a Call, args: Args) -> BoxFut<'a, Result<Value, ToolError>>;
}

pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
    file: Option<PathBuf>,
    user: Mutex<UserSet>,
}

#[derive(Default)]
struct UserSet {
    stamp: Option<dataman_core::stamp::FileStamp>,
    tools: Vec<Arc<dyn Tool>>,
    errors: Vec<String>,
}

impl Registry {
    pub fn new(user_file: Option<PathBuf>) -> Registry {
        Registry { tools: Vec::new(), file: user_file, user: Mutex::new(UserSet::default()) }
    }

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

    pub fn refresh(&self) -> bool {
        let Some(file) = &self.file else { return false };
        let stamp = dataman_core::stamp::FileStamp::of(file);
        let mut g = self.user.lock();
        if g.stamp == stamp {
            return false;
        }
        g.stamp = stamp;
        let (tools, errors) = match std::fs::read_to_string(file) {
            Ok(text) => {
                // The tools file sits next to connections.toml: each tool's SQL is read in its
                // connection's dialect.
                let profiles = file.parent().and_then(|d| dataman_core::ConfigStore::open(&dataman_core::Dirs::at(d)).ok()).map(|s| s.connections()).unwrap_or_default();
                let dialect_of = |c: &str| profiles.iter().find(|p| p.name == c || p.id == c).filter(|p| p.engine.is_sql()).map(|p| p.engine.dialect());
                crate::user::load(&text, &self.builtin_names(), &dialect_of)
            }
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
