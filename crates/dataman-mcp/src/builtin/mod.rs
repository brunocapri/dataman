mod activity;
mod catalog;
mod connections;
mod favorites;
mod login;
mod nosql;
mod query;
pub(crate) mod rules;
mod show;

use dataman_core::bounded;
use serde_json::{Map, Value, json};

use crate::policy::{Effect, Need};
use crate::tool::{Args, BoxFut, Call, Registry, Schema, Tool, ToolError};

pub(crate) use catalog::view_open;

pub type Handler =for<'a> fn(&'a Call, Args) -> BoxFut<'a, Result<Value, ToolError>>;

/// Every built-in tool: what it does, and what its `connection` must be allowed before the
/// handler runs (None: the handler judges its connection itself). Writes through the tier are
/// gated for reads: an allowed statement runs in a read tier too, and the tier judges the rest.
/// run_favorite's connection is the favorite's; update_connection and create_agent_login judge
/// theirs themselves, and show_in_dataman judges the one its destination takes.
pub const META: &[(&str, Effect, Option<Need>)] = &[
    ("list_connections", Effect::Local, None),
    ("list_schemas", Effect::Read, Some(Need::Read)),
    ("list_tables", Effect::Read, Some(Need::Read)),
    ("search_objects", Effect::Read, Some(Need::Read)),
    ("describe_table", Effect::Read, Some(Need::Read)),
    ("get_ddl", Effect::Read, Some(Need::Read)),
    ("sample_rows", Effect::Read, Some(Need::Read)),
    ("run_query", Effect::Read, Some(Need::Read)),
    ("open_query", Effect::MaybeWrite, Some(Need::Read)),
    ("explain", Effect::Read, Some(Need::Read)),
    ("explain_analyze", Effect::MaybeWrite, Some(Need::Read)),
    ("execute", Effect::Write, Some(Need::Read)),
    ("activity_sessions", Effect::Read, Some(Need::Read)),
    ("activity_locks", Effect::Read, Some(Need::Read)),
    ("cancel_backend", Effect::Write, Some(Need::Write)),
    ("terminate_backend", Effect::Write, Some(Need::Write)),
    ("list_favorites", Effect::Local, None),
    ("run_favorite", Effect::MaybeWrite, None),
    ("add_favorite", Effect::LocalEdit, None),
    ("remove_favorite", Effect::LocalRemove, None),
    ("rename_favorite", Effect::LocalEdit, None),
    ("import_favorites", Effect::LocalEdit, None),
    ("create_connection", Effect::LocalEdit, None),
    ("update_connection", Effect::LocalEdit, None),
    ("create_agent_login", Effect::LocalEdit, None),
    ("show_in_dataman", Effect::Local, None),
    ("nosql_query", Effect::Read, Some(Need::Read)),
    ("nosql_execute", Effect::Write, Some(Need::Write)),
];

fn meta(name: &str) -> Option<(Effect, Option<Need>)> {
    META.iter().find(|(n, _, _)| *n == name).map(|&(_, e, g)| (e, g))
}

pub struct Builtin {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Schema,
    pub need: Option<Need>,
    pub run: Handler,
}

impl Tool for Builtin {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn schema(&self) -> &Schema {
        &self.schema
    }
    fn need(&self) -> Option<Need> {
        self.need
    }
    // A tool missing from META is the most careful: a write that needs its connection gated.
    fn effect(&self) -> Effect {
        meta(self.name).map_or(Effect::Write, |m| m.0)
    }
    fn gate(&self) -> Option<Need> {
        meta(self.name).map_or(Some(Need::Write), |m| m.1)
    }
    fn call<'a>(&'a self, cx: &'a Call, args: Args) -> BoxFut<'a, Result<Value, ToolError>> {
        (self.run)(cx, args)
    }
}

pub fn register(r: &mut Registry) {
    for t in catalog::tools().into_iter().chain(query::tools()).chain(activity::tools()).chain(favorites::tools()).chain(connections::tools()).chain(login::tools()).chain(show::tools()).chain(nosql::tools()) {
        r.register(std::sync::Arc::new(t));
    }
}

pub fn result_json(o: &bounded::Outcome, max_rows: usize) -> Value {
    let mut m = Map::new();
    m.insert("columns".into(), Value::Array(o.columns.iter().map(|c| json!([c.name, c.type_name])).collect()));
    m.insert("rows".into(), Value::Array(o.rows.iter().map(|r| Value::Array(r.clone())).collect()));
    m.insert("row_count".into(), json!(o.rows.len()));
    if let Some(t) = o.truncated {
        m.insert("truncated".into(), json!(t.label()));
        let hint = match t {
            bounded::Truncation::Rows => format!("more rows exist past {max_rows}; narrow with WHERE/LIMIT or raise max_rows"),
            bounded::Truncation::Bytes => "the result grew too large; select fewer columns or rows".into(),
        };
        m.insert("note".into(), json!(hint));
    }
    if o.cut_cells > 0 {
        m.insert("cut_cells".into(), json!(o.cut_cells));
    }
    m.insert("ms".into(), json!(ms(o.elapsed)));
    Value::Object(m)
}

pub fn read_json(o: &bounded::Outcome, max_rows: usize, statements: &[String]) -> Value {
    if o.parts.len() <= 1 && statements.len() <= 1 {
        return result_json(o, max_rows);
    }
    let results: Vec<Value> = o
        .parts
        .iter()
        .map(|p| {
            let part = bounded::Outcome { columns: p.columns.clone(), rows: p.rows.clone(), truncated: p.truncated, cut_cells: p.cut_cells, elapsed: p.elapsed, ..Default::default() };
            let mut v = result_json(&part, max_rows);
            if let Some(m) = v.as_object_mut() {
                let mut head = Map::new();
                head.insert("statement".into(), json!(p.statement + 1));
                head.insert("sql".into(), json!(first_line(&p.sql)));
                head.extend(std::mem::take(m));
                *m = head;
            }
            v
        })
        .collect();
    let mut m = Map::new();
    m.insert("results".into(), Value::Array(results));
    m.insert("statements".into(), json!(statements.len()));
    let reached = o.parts.last().map_or(0, |p| p.statement + 1);
    if o.parts.last().is_some_and(|p| p.truncated == Some(bounded::Truncation::Bytes)) && reached < statements.len() {
        m.insert("truncated".into(), json!("bytes"));
        let rest = if reached + 1 == statements.len() { format!("statement {}", statements.len()) } else { format!("statements {}–{}", reached + 1, statements.len()) };
        m.insert("note".into(), json!(format!("the results grew too large at statement {reached}: {rest} not returned; run them on their own or select less")));
    }
    m.insert("ms".into(), json!(ms(o.elapsed)));
    Value::Object(m)
}

fn first_line(sql: &str) -> String {
    let line = sql.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("--")).unwrap_or("");
    match line.char_indices().nth(120) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

pub fn ms(d: std::time::Duration) -> f64 {
    (d.as_secs_f64() * 1e4).round() / 10.0
}

pub fn table(columns: &[&str], rows: Vec<Vec<Value>>) -> Value {
    json!({"columns": columns, "rows": rows})
}

pub fn split_name(s: &str) -> (Option<String>, String) {
    let s = s.trim();
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            '.' if !quoted => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    match parts.len() {
        1 => (None, parts.pop().unwrap()),
        _ => {
            let name = parts.pop().unwrap();
            (Some(parts.pop().unwrap()), name)
        }
    }
}

pub fn conn_schema() -> Schema {
    Schema::new().conn()
}

pub fn confirm_prop(s: Schema) -> Schema {
    s.prop(
        "confirm",
        false,
        json!({"type": "boolean", "description": "Ignored (kept for older clients): destructive writes wait for the user's Allow in Dataman instead."}),
    )
}

pub fn limits_props(s: Schema, default_rows: u64) -> Schema {
    s.prop("max_rows", false, json!({"type": "integer", "minimum": 1, "maximum": 100000, "description": format!("Most rows returned (default {default_rows}; the app's setting caps it, 1000 by default).")}))
        .prop("timeout_ms", false, json!({"type": "integer", "minimum": 100, "maximum": 86400000, "description": "Statement time limit in ms (default 30000; the connection's Time per call caps it, 5 min by default)."}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(split_name("orders"), (None, "orders".into()));
        assert_eq!(split_name("public.orders"), (Some("public".into()), "orders".into()));
        assert_eq!(split_name("\"My.Schema\".\"Or\"\"ders\""), (Some("My.Schema".into()), "Or\"ders".into()));
        assert_eq!(split_name("db.s.t"), (Some("s".into()), "t".into()));
    }

    #[test]
    fn builtin_schemas() {
        let r = Registry::standard(None);
        let tools = r.all();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        for want in [
            "list_connections", "list_schemas", "list_tables", "search_objects", "describe_table", "get_ddl", "sample_rows", "run_query",
            "open_query", "explain", "explain_analyze", "execute", "activity_sessions", "activity_locks", "cancel_backend", "terminate_backend",
            "list_favorites", "run_favorite", "create_connection", "update_connection", "create_agent_login", "show_in_dataman", "nosql_query", "nosql_execute",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for t in &tools {
            let s = t.schema();
            assert_eq!(s.0["type"], "object", "{}", t.name());
            assert_eq!(s.0["additionalProperties"], false, "{}", t.name());
            assert!(t.description().len() > 40 && t.description().len() < 900, "{}: {} chars", t.name(), t.description().len());
            assert!(t.name().chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{}", t.name());
            assert_eq!(t.need().is_some(), s.required().contains(&"connection") && !matches!(t.name(), "update_connection" | "create_agent_login"), "{}", t.name());
            for (k, v) in s.properties() {
                assert!(v.get("type").is_some(), "{}.{k}", t.name());
                assert!(v.get("description").is_some_and(|d| d.as_str().is_some_and(|d| !d.is_empty())), "{}.{k} has no description", t.name());
            }
            assert!(meta(t.name()).is_some(), "{} is not in META", t.name());
            assert_eq!(t.gate().is_some(), s.required().contains(&"connection") && !matches!(t.name(), "update_connection" | "create_agent_login"), "{}", t.name());
            // Gated for writes: what no allowed statement nor tier judgement lets a read tier run.
            assert_eq!(t.gate() == Some(Need::Write), matches!(t.name(), "cancel_backend" | "terminate_backend" | "nosql_execute"), "{}", t.name());
            // What may write to a database is never announced read-only.
            let writes = t.need() == Some(Need::Write) || matches!(t.name(), "open_query" | "run_favorite" | "explain_analyze");
            assert_eq!(t.effect().hints().0, !writes && matches!(t.effect(), Effect::Read | Effect::Local), "{}", t.name());
            let ann = crate::server::describe(t.as_ref()).annotations.and_then(|a| a.read_only_hint);
            assert_eq!(ann, Some(t.effect().hints().0), "{}", t.name());
            let write_tool = t.need() == Some(Need::Write);
            assert_eq!(write_tool, s.properties().contains_key("confirm") && t.name() != "explain_analyze", "{}", t.name());
        }
    }
}
