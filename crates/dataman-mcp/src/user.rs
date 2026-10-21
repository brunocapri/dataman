//! A template's `$1 … $n` are bound to typed, validated parameters by the server, never
//! spliced into the SQL.

use std::sync::Arc;
use std::time::Duration;

use dataman_core::bounded::{Limits, Mode};
use dataman_core::guard;
use dataman_core::sql::{Dialect, TokenKind, tokenize};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::policy::Need;
use crate::tool::{Args, BoxFut, Call, Schema, Tool, ToolError, Write};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    tool: Vec<Def>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Def {
    pub name: String,
    pub description: String,
    pub connection: String,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub mode: UserMode,
    pub sql: String,
    #[serde(default)]
    pub max_rows: Option<usize>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default, rename = "param")]
    pub params: Vec<Param>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserMode {
    #[default]
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    String,
    Int,
    Number,
    Bool,
    Enum,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: Option<bool>,
    #[serde(default)]
    pub default: Option<toml::Value>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub max_length: Option<usize>,
    #[serde(default)]
    pub values: Vec<String>,
}

impl Param {
    fn required(&self) -> bool {
        self.required.unwrap_or(self.default.is_none())
    }

    fn schema(&self) -> Value {
        let mut m = Map::new();
        let ty = match self.kind {
            Kind::String | Kind::Enum => "string",
            Kind::Int => "integer",
            Kind::Number => "number",
            Kind::Bool => "boolean",
        };
        m.insert("type".into(), json!(ty));
        let mut desc = self.description.clone().unwrap_or_else(|| self.name.replace('_', " "));
        if let Some(d) = &self.default {
            desc.push_str(&format!(" (default {d})"));
        }
        m.insert("description".into(), json!(desc));
        if self.kind == Kind::Enum {
            m.insert("enum".into(), json!(self.values));
        }
        if let Some(x) = self.min {
            m.insert("minimum".into(), json!(x));
        }
        if let Some(x) = self.max {
            m.insert("maximum".into(), json!(x));
        }
        if let Some(x) = self.max_length {
            m.insert("maxLength".into(), json!(x));
        }
        Value::Object(m)
    }

    pub fn bind(&self, v: Option<&Value>) -> Result<Option<String>, String> {
        let v = match (v.filter(|v| !v.is_null()), &self.default) {
            (Some(v), _) => v.clone(),
            (None, Some(d)) => toml_to_json(d),
            (None, None) if self.required() => return Err(format!("{:?} is required", self.name)),
            (None, None) => return Ok(None),
        };
        let n = &self.name;
        let range = |x: f64| -> Result<(), String> {
            if self.min.is_some_and(|m| x < m) {
                return Err(format!("{n:?} must be at least {}", self.min.unwrap()));
            }
            if self.max.is_some_and(|m| x > m) {
                return Err(format!("{n:?} must be at most {}", self.max.unwrap()));
            }
            Ok(())
        };
        Ok(Some(match self.kind {
            Kind::Int => {
                let i = v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15).map(|f| f as i64));
                let i = i.ok_or_else(|| format!("{n:?} must be an integer"))?;
                range(i as f64)?;
                i.to_string()
            }
            Kind::Number => {
                let f = v.as_f64().filter(|f| f.is_finite()).ok_or_else(|| format!("{n:?} must be a number"))?;
                range(f)?;
                serde_json::Number::from_f64(f).map(|x| x.to_string()).unwrap_or_else(|| f.to_string())
            }
            Kind::Bool => v.as_bool().ok_or_else(|| format!("{n:?} must be true or false"))?.to_string(),
            Kind::String => {
                let s = v.as_str().ok_or_else(|| format!("{n:?} must be a string"))?;
                if self.max_length.is_some_and(|m| s.chars().count() > m) {
                    return Err(format!("{n:?} is longer than {} characters", self.max_length.unwrap()));
                }
                s.to_string()
            }
            Kind::Enum => {
                let s = v.as_str().ok_or_else(|| format!("{n:?} must be a string"))?;
                if !self.values.iter().any(|x| x == s) {
                    return Err(format!("{n:?} must be one of {}", self.values.join(", ")));
                }
                s.to_string()
            }
        }))
    }
}

fn toml_to_json(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

pub fn max_placeholder(sql: &str) -> usize {
    tokenize(sql).into_iter().filter(|t| t.kind == TokenKind::Param).filter_map(|t| t.text(sql)[1..].parse::<usize>().ok()).max().unwrap_or(0)
}

fn valid_name(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase()) && s.len() <= 64 && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_')
}

/// The statement as `dialect` reads it: one statement, and in read mode a read.
fn check_sql(d: &Def, dialect: Dialect) -> Result<String, String> {
    let stmts = dataman_core::bounded::statements_in(&d.sql, dialect);
    if stmts.len() != 1 {
        return Err("sql must be exactly one statement".into());
    }
    match d.mode {
        UserMode::Read => {
            if let Some(r) = guard::agent_read_refusal_in(&stmts[0], dialect) {
                return Err(format!("mode is read but {r}"));
            }
        }
        UserMode::Write => {
            let f = guard::classify_in(&stmts[0], dialect);
            if f.tx.is_some() || f.outside_tx {
                return Err(format!("{} cannot run in a tool's transaction", f.verb));
            }
        }
    }
    Ok(stmts[0].clone())
}

/// `dialect`: the SQL dialect of the tool's connection; `None` (connection "any", or one not
/// saved) checks the statement as each engine reads it, and the call checks it again on the
/// connection it runs on.
pub fn check(d: &Def, taken: &[String], dialect: Option<Dialect>) -> Result<(), String> {
    if !valid_name(&d.name) {
        return Err("names are lower case letters, digits and _, starting with a letter (64 at most)".into());
    }
    if taken.contains(&d.name) {
        return Err("the name is taken (a built-in tool or another user tool)".into());
    }
    if d.description.trim().is_empty() {
        return Err("a description is required (the agent reads it to pick the tool)".into());
    }
    if d.connection.trim().is_empty() {
        return Err("connection is required (a connection name, or \"any\")".into());
    }
    match dialect {
        Some(dl) => {
            check_sql(d, dl)?;
        }
        None => {
            let all: Vec<Result<String, String>> = [Dialect::Postgres, Dialect::MySql, Dialect::TSql].into_iter().map(|dl| check_sql(d, dl)).collect();
            if !all.iter().any(Result::is_ok) {
                return Err(all.into_iter().find_map(Result::err).unwrap_or_default());
            }
        }
    }
    let n = max_placeholder(&d.sql);
    if n != d.params.len() {
        return Err(format!("the SQL uses ${n} as its last placeholder but {} param(s) are declared", d.params.len()));
    }
    let mut seen: Vec<&str> = vec!["connection", "database", "confirm"];
    for p in &d.params {
        if !valid_name(&p.name) || seen.contains(&p.name.as_str()) {
            return Err(format!("param {:?}: invalid or repeated name", p.name));
        }
        seen.push(&p.name);
        if p.kind == Kind::Enum && p.values.is_empty() {
            return Err(format!("param {:?}: an enum needs values", p.name));
        }
        if let Some(def) = &p.default {
            p.bind(Some(&toml_to_json(def))).map_err(|e| format!("param {:?}: bad default: {e}", p.name))?;
        }
    }
    Ok(())
}

/// `dialect_of`: the dialect of a saved connection, by name or id.
pub fn load(text: &str, builtins: &[String], dialect_of: &dyn Fn(&str) -> Option<Dialect>) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let file: File = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => return (Vec::new(), vec![format!("{e}")]),
    };
    let mut taken: Vec<String> = builtins.to_vec();
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut errors = Vec::new();
    for d in file.tool {
        let dialect = if d.connection.eq_ignore_ascii_case("any") { None } else { dialect_of(&d.connection) };
        match check(&d, &taken, dialect) {
            Ok(()) => {
                taken.push(d.name.clone());
                tools.push(Arc::new(UserTool::new(d)));
            }
            Err(e) => errors.push(format!("tool {:?}: {e}", d.name)),
        }
    }
    (tools, errors)
}

pub struct UserTool {
    pub def: Def,
    schema: Schema,
}

impl UserTool {
    pub fn new(def: Def) -> UserTool {
        let mut s = Schema::new();
        let any = def.connection.eq_ignore_ascii_case("any");
        if any {
            s = s.conn();
        }
        for p in &def.params {
            s = s.prop(&p.name, p.required(), p.schema());
        }
        if def.mode == UserMode::Write {
            s = crate::builtin::confirm_prop(s);
        }
        UserTool { def, schema: s }
    }

    pub fn bind(&self, a: &Args) -> Result<Vec<Option<String>>, ToolError> {
        self.def.params.iter().map(|p| p.bind(a.get(&p.name)).map_err(ToolError::Invalid)).collect()
    }
}

impl Tool for UserTool {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn schema(&self) -> &Schema {
        &self.schema
    }
    fn need(&self) -> Option<Need> {
        Some(if self.def.mode == UserMode::Write { Need::Write } else { Need::Read })
    }
    fn call<'a>(&'a self, cx: &'a Call, args: Args) -> BoxFut<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let mut a = args;
            if !self.def.connection.eq_ignore_ascii_case("any") {
                a.0.insert("connection".into(), json!(self.def.connection));
                if let Some(db) = &self.def.database {
                    a.0.insert("database".into(), json!(db));
                }
            }
            let params = self.bind(&a)?;
            let cap = cx.hub.settings().mcp_max_rows.max(1);
            let limits = Limits {
                max_rows: self.def.max_rows.unwrap_or(200).clamp(1, cap),
                timeout: Duration::from_millis(self.def.timeout_ms.unwrap_or(30_000).clamp(100, 300_000)),
                ..Limits::default()
            };
            // The connection's own dialect, and its rules for agents, as for run_query and
            // execute: a tool the user wrote runs for an agent, so what the rules keep stays kept.
            let target = cx.target(&a, Need::Read)?;
            if !target.engine.is_sql() {
                return Err(ToolError::Invalid(format!("{} is not a SQL connection", target.name)));
            }
            let stmt = check_sql(&self.def, target.engine.dialect()).map_err(ToolError::Invalid)?;
            let write = self.def.mode == UserMode::Write;
            if !write {
                crate::builtin::rules::structure_read(&target, std::slice::from_ref(&stmt))?;
            }
            let checked = crate::builtin::rules::checked_sql(cx, &a, &target, &self.def.sql, write).await?;
            let read = crate::builtin::rules::read_limits(cx, &target, limits);
            let mut out = match self.def.mode {
                UserMode::Read => {
                    let o = cx.open(&a, Need::Read).await?;
                    cx.run(&o, &checked.sql, params, Mode::Read, read).await?
                }
                // The same tiers as execute: what the template is decides, not the file.
                UserMode::Write => {
                    let visible = cx.tab_front(false).is_some();
                    let w = Write { sql: &checked.sql, judged: None, params, limits: read, rollback: false, visible, title: Some(self.def.name.clone()) };
                    cx.write(&a, w).await?.outcome
                }
            };
            crate::builtin::rules::mask_outcome(cx, &target, &mut out, &checked.masked, read, limits);
            cx.note_rows(out.rows.len());
            let mut v = crate::builtin::result_json(&out, limits.max_rows);
            crate::builtin::rules::tell(&mut v, &checked, &cx.note().seen.masked);
            if self.def.mode == UserMode::Write {
                v["statements"] = Value::Array(out.statements.iter().map(|d| json!([d.verb, d.affected])).collect());
                v["committed"] = json!(true);
            }
            Ok(v)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[[tool]]
name = "orders_by_customer"
description = "A customer's latest orders, newest first."
connection = "local pg"
database = "shop"
sql = "SELECT id, total FROM orders WHERE customer_id = $1 AND status = $2 ORDER BY id DESC"
max_rows = 50

[[tool.param]]
name = "customer_id"
type = "int"
description = "The customer's id."
min = 1

[[tool.param]]
name = "status"
type = "enum"
values = ["open", "paid"]
default = "open"

[[tool]]
name = "close_order"
description = "Marks an order closed."
connection = "any"
mode = "write"
sql = "UPDATE orders SET status = 'closed' WHERE id = $1"

[[tool.param]]
name = "id"
type = "int"
"#;

    fn args(v: Value) -> Args {
        Args(v.as_object().unwrap().clone())
    }

    #[test]
    fn parses_and_builds_schemas() {
        let (tools, errors) = load(EXAMPLE, &["run_query".to_string()], &|_| None);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(tools.iter().map(|t| t.name()).collect::<Vec<_>>(), ["orders_by_customer", "close_order"]);
        let s = tools[0].schema();
        assert_eq!(s.required(), ["customer_id"], "a param with a default is optional");
        assert!(!s.properties().contains_key("connection"), "a fixed connection is not an argument");
        assert_eq!(s.properties()["status"]["enum"], json!(["open", "paid"]));
        assert_eq!(s.properties()["customer_id"]["minimum"], json!(1.0));
        assert_eq!(tools[0].need(), Some(Need::Read));
        let w = tools[1].schema();
        assert_eq!(w.required(), ["connection", "id"]);
        assert!(w.properties().contains_key("confirm") && w.properties().contains_key("database"));
        assert_eq!(tools[1].need(), Some(Need::Write));
        assert!(s.validate(&args(json!({"customer_id": 0})).0).unwrap_err().contains("at least 1"));
        assert!(s.validate(&args(json!({"customer_id": 3, "status": "lost"})).0).unwrap_err().contains("one of"));
        assert!(s.validate(&args(json!({"customer_id": 3, "x": 1})).0).unwrap_err().contains("unknown"));
    }

    #[test]
    fn binding() {
        let (tools, _) = load(EXAMPLE, &[], &|_| None);
        let t = tools.into_iter().next().unwrap();
        let def = &EXAMPLE_DEF.with(|d| d.clone());
        let u = UserTool::new(def.clone());
        assert_eq!(u.bind(&args(json!({"customer_id": 7}))).unwrap(), vec![Some("7".into()), Some("open".into())], "defaults fill in");
        assert_eq!(u.bind(&args(json!({"customer_id": 7.0, "status": "paid"}))).unwrap(), vec![Some("7".into()), Some("paid".into())]);
        assert!(u.bind(&args(json!({"customer_id": "7; DROP TABLE x"}))).is_err(), "values are typed, never SQL");
        assert!(u.bind(&args(json!({"status": "paid"}))).unwrap_err().message().contains("required"));
        assert_eq!(t.name(), "orders_by_customer");
        let p = |kind, extra: Value| -> Param {
            let mut v = json!({"name": "p", "type": kind});
            v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            serde_json::from_value(v).unwrap()
        };
        assert_eq!(p("number", json!({"max": 2.5})).bind(Some(&json!(2.5))).unwrap(), Some("2.5".into()));
        assert!(p("number", json!({"max": 2.5})).bind(Some(&json!(3))).is_err());
        assert_eq!(p("bool", json!({})).bind(Some(&json!(true))).unwrap(), Some("true".into()));
        assert!(p("string", json!({"max_length": 3})).bind(Some(&json!("abcd"))).is_err());
        assert_eq!(p("string", json!({"required": false})).bind(None).unwrap(), None, "optional and absent is NULL");
        assert_eq!(p("string", json!({})).bind(Some(&json!("it's"))).unwrap(), Some("it's".into()), "quotes are data");
    }

    thread_local! {
        static EXAMPLE_DEF: Def = toml::from_str::<File>(EXAMPLE).unwrap().tool.remove(0);
    }

    #[test]
    fn problems_are_reported() {
        let bad = |extra: &str| {
            let text = format!("[[tool]]\nname = \"t\"\ndescription = \"d\"\nconnection = \"c\"\n{extra}");
            let (tools, errors) = load(&text, &["run_query".into()], &|_| None);
            assert!(tools.is_empty(), "{extra}");
            errors.join("; ")
        };
        assert!(bad("sql = \"SELECT $1\"").contains("1 param") || bad("sql = \"SELECT $1\"").contains("$1"));
        assert!(bad("sql = \"DELETE FROM t WHERE id = 1\"").contains("mode is read"));
        assert!(bad("sql = \"SELECT 1; SELECT 2\"").contains("one statement"));
        assert!(bad("mode = \"write\"\nsql = \"VACUUM t\"").contains("cannot run"));
        assert!(bad("sql = \"SELECT 1\"\nunknown = 1").contains("unknown field"));
        assert!(bad("sql = \"SELECT $1\"\n[[tool.param]]\nname = \"x\"\ntype = \"enum\"").contains("needs values"));
        assert!(bad("sql = \"SELECT $1\"\n[[tool.param]]\nname = \"x\"\ntype = \"int\"\ndefault = \"a\"").contains("bad default"));
        let (_, e) = load("[[tool]]\nname = \"run_query\"\ndescription = \"d\"\nconnection = \"c\"\nsql = \"SELECT 1\"", &["run_query".into()], &|_| None);
        assert!(e[0].contains("taken"), "{e:?}");
        let (_, e) = load("[[tool]]\nname = \"Bad Name\"\ndescription = \"d\"\nconnection = \"c\"\nsql = \"SELECT 1\"", &[], &|_| None);
        assert!(e[0].contains("lower case"), "{e:?}");
        assert_eq!(max_placeholder("SELECT $1, '$9', $2 -- $7"), 2, "strings and comments are not placeholders");
    }
}
