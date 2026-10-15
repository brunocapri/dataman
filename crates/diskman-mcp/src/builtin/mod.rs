//! The built-in tools, registered like any other [`Tool`].

mod activity;
mod catalog;
mod connections;
mod favorites;
mod query;

use diskman_core::bounded;
use serde_json::{Map, Value, json};

use crate::policy::Need;
use crate::tool::{Args, BoxFut, Call, Registry, Schema, Tool, ToolError};

pub use catalog::system_schema as catalog_system_schema;

pub type Handler =for<'a> fn(&'a Call, Args) -> BoxFut<'a, Result<Value, ToolError>>;

/// A built-in tool: its definition and the function that runs it.
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
    fn call<'a>(&'a self, cx: &'a Call, args: Args) -> BoxFut<'a, Result<Value, ToolError>> {
        (self.run)(cx, args)
    }
}

pub fn register(r: &mut Registry) {
    for t in catalog::tools().into_iter().chain(query::tools()).chain(activity::tools()).chain(favorites::tools()).chain(connections::tools()) {
        r.register(std::sync::Arc::new(t));
    }
}

/// A bounded result as compact JSON: `columns` as `[name, type]` pairs, `rows` as arrays.
pub fn result_json(o: &bounded::Outcome, max_rows: usize) -> Value {
    let mut m = Map::new();
    m.insert("columns".into(), Value::Array(o.columns.iter().map(|c| json!([c.name, c.type_name])).collect()));
    m.insert("rows".into(), Value::Array(o.rows.iter().map(|r| Value::Array(r.clone())).collect()));
    m.insert("row_count".into(), json!(o.rows.len()));
    if let Some(t) = o.truncated {
        m.insert("truncated".into(), json!(t.label()));
        let hint = match t {
            bounded::Truncation::Rows => format!("more rows exist past {max_rows}; narrow with WHERE/LIMIT or raise max_rows"),
            bounded::Truncation::Bytes => "the result passed ~256 KB; select fewer columns or rows".into(),
        };
        m.insert("note".into(), json!(hint));
    }
    if o.cut_cells > 0 {
        m.insert("cut_cells".into(), json!(o.cut_cells));
    }
    m.insert("ms".into(), json!(ms(o.elapsed)));
    Value::Object(m)
}

/// A read's answer: one statement with one result set as [`result_json`] (the shape single
/// statements always had); several statements, or a T-SQL batch's several result sets, as
/// `{results: [{statement, sql, columns, rows, row_count, truncated?, note?, cut_cells?, ms}],
/// statements, ms}`. The row cap is for each result, the ~256 KB byte cap for all of them: the
/// statements after the one that reaches it are not run (or, in an agent tab, not returned),
/// which `truncated: "bytes"` and a note say at the top level.
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
        m.insert("note".into(), json!(format!("the results passed ~256 KB in all at statement {reached}: {rest} not returned; run them on their own or select less")));
    }
    m.insert("ms".into(), json!(ms(o.elapsed)));
    Value::Object(m)
}

/// A statement's first line that is not a comment, up to 120 characters.
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

/// `"rows"` style tables: a header and arrays.
pub fn table(columns: &[&str], rows: Vec<Vec<Value>>) -> Value {
    json!({"columns": columns, "rows": rows})
}

/// `schema.name` split (a quoted part keeps its dots); `None` schema when unqualified.
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
        json!({"type": "boolean", "description": "Ignored (kept for older clients): destructive writes wait for the user's Allow in Diskman instead."}),
    )
}

pub fn limits_props(s: Schema, default_rows: u64) -> Schema {
    s.prop("max_rows", false, json!({"type": "integer", "minimum": 1, "maximum": 100000, "description": format!("Most rows returned (default {default_rows}; the app's setting caps it, 1000 by default).")}))
        .prop("timeout_ms", false, json!({"type": "integer", "minimum": 100, "maximum": 300000, "description": "Statement time limit in ms (default 30000)."}))
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

    /// Every built-in has a precise description and a valid object schema with the connection
    /// argument when it needs one.
    #[test]
    fn builtin_schemas() {
        let r = Registry::standard(None);
        let tools = r.all();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        for want in [
            "list_connections", "list_schemas", "list_tables", "search_objects", "describe_table", "get_ddl", "sample_rows", "run_query",
            "open_query", "explain", "explain_analyze", "execute", "activity_sessions", "activity_locks", "cancel_backend", "terminate_backend",
            "list_favorites", "run_favorite",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for t in &tools {
            let s = t.schema();
            assert_eq!(s.0["type"], "object", "{}", t.name());
            assert_eq!(s.0["additionalProperties"], false, "{}", t.name());
            assert!(t.description().len() > 40 && t.description().len() < 900, "{}: {} chars", t.name(), t.description().len());
            assert!(t.name().chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{}", t.name());
            assert_eq!(t.need().is_some(), s.required().contains(&"connection"), "{}", t.name());
            for (k, v) in s.properties() {
                assert!(v.get("type").is_some(), "{}.{k}", t.name());
                assert!(v.get("description").is_some_and(|d| d.as_str().is_some_and(|d| !d.is_empty())), "{}.{k} has no description", t.name());
            }
            let write_tool = t.need() == Some(Need::Write);
            assert_eq!(write_tool, s.properties().contains_key("confirm") && t.name() != "explain_analyze", "{}", t.name());
        }
    }
}
