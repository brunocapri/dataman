//! A connection's rules for agents (Dataman › Agents › Privacy and Permissions) around the tools:
//! statements are checked (and narrowed) before they run, results are masked before they leave,
//! and listings drop what agents may not see.

use std::collections::{HashMap, HashSet};

use dataman_core::agent_nosql::{self, Shaped};
use dataman_core::agent_sql::{self, Checked, Table};
use dataman_core::privacy::{AgentRules, AgentView, Mask, Names, mask_value};
use dataman_core::schema::{RelKind, ViewSourceKind};
use dataman_core::{ConnectionProfile, bounded};
use serde_json::{Value, json};

use crate::policy::Need;
use crate::tool::{Args, Call, Opened, ToolError};

pub fn restricted(r: &AgentRules) -> bool {
    !r.visible.is_open() || (r.view == AgentView::Masked && r.sensitive.iter().any(|s| s.on))
}

/// Structure only: reads return no rows (a plan without ANALYZE is structure).
pub fn structure_read(p: &ConnectionProfile, stmts: &[String]) -> Result<(), ToolError> {
    if p.mcp_rules.view != AgentView::Structure {
        return Ok(());
    }
    let plans = !stmts.is_empty() && stmts.iter().all(|s| dataman_core::sql::statement_verb(s) == "EXPLAIN" && !dataman_core::explain::analyzes(s));
    if plans {
        return Ok(());
    }
    Err(ToolError::Refused(structure_only(p)))
}

pub fn structure_only(p: &ConnectionProfile) -> String {
    format!("{} shows agents its structure only (Dataman › Agents › Privacy): no rows; describe_table, get_ddl and explain describe it", p.name)
}

pub fn activity(p: &ConnectionProfile) -> Result<(), ToolError> {
    if p.mcp_rules.activity_on() {
        return Ok(());
    }
    let why = if p.mcp_rules.activity { "while some data is kept from agents" } else { "(Dataman › Agents › Permissions)" };
    Err(ToolError::Refused(format!("server activity is off for agents on {} {why}", p.name)))
}

async fn lookup(o: &Opened, schema: Option<&str>, name: &str) -> Option<(Table, RelKind)> {
    let qualified = match schema {
        Some(s) => format!("{}.{}", dataman_core::sql::quote_ident(s), dataman_core::sql::quote_ident(name)),
        None => dataman_core::sql::quote_ident(name),
    };
    let r = super::catalog::find_relation(o, &qualified).await.ok()?;
    let cols = o.conn.columns(&o.database, &r.schema, &r.name).await.ok()?;
    Some((Table { schema: r.schema, name: r.name, columns: cols.into_iter().map(|c| c.name).collect() }, r.kind))
}

/// Whether the rules keep anything of this table from agents (the table, a column, or, in
/// Masked data, a sensitive column).
fn keeps(r: &AgentRules, t: &Table) -> bool {
    let q = Names::Qualified;
    !r.visible.table_visible(Some(&t.schema), &t.name, q)
        || t.columns.iter().any(|c| r.visible.column_hidden(Some(&t.schema), &t.name, c, q))
        || (r.view == AgentView::Masked && t.columns.iter().any(|c| r.sensitive_mask(Some(&t.schema), &t.name, c, q).is_some()))
}

const VIEW_DEPTH: usize = 64;

/// A view reads what it is defined on, under its own names: through it an agent must not reach
/// what the rules keep (`CREATE VIEW v AS SELECT password_hash AS h FROM users`). Followed down
/// views of views; what cannot be followed is refused.
pub async fn view_reads_kept(o: &Opened, r: &AgentRules, view: &Table, kind: RelKind) -> Result<(), String> {
    let conn = &o.profile.name;
    let mut todo = vec![(view.schema.clone(), view.name.clone(), kind)];
    let mut seen: HashSet<(String, String)> = HashSet::new();
    while let Some((schema, name, kind)) = todo.pop() {
        if !matches!(kind, RelKind::View | RelKind::MaterializedView) || !seen.insert((schema.clone(), name.clone())) {
            continue;
        }
        if seen.len() > VIEW_DEPTH {
            return Err(format!("{} reads through more views than Dataman follows, and {conn} keeps some data from agents: query its tables directly", view.name));
        }
        let sources = o.conn.view_sources(&o.database, &schema, &name).await.map_err(|e| format!("Dataman could not check what {} reads ({e:#}), and {conn} keeps some data from agents", view.name))?;
        for src in sources {
            match src.kind {
                ViewSourceKind::Function => {
                    return Err(format!("{} calls {}(), a function of your database; agents cannot read it on {conn} while some data is kept from them", view.name, src.name));
                }
                ViewSourceKind::Unknown => {
                    return Err(format!("{} reads {}, which Dataman cannot check, and {conn} keeps some data from agents", view.name, src.name));
                }
                ViewSourceKind::Relation => {
                    let Some((t, k)) = lookup(o, Some(&src.schema), &src.name).await else {
                        return Err(format!("{} reads {}.{}, which Dataman cannot check, and {conn} keeps some data from agents", view.name, src.schema, src.name));
                    };
                    if keeps(r, &t) {
                        return Err(format!("{} reads {}.{}, which keeps data from agents on {conn}: query {} directly", view.name, t.schema, t.name, t.name));
                    }
                    todo.push((t.schema, t.name, k));
                }
            }
        }
    }
    Ok(())
}

async fn user_routines(o: &Opened) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(schemas) = super::catalog::schemas_of(o, None, false).await else {
        return out;
    };
    for s in schemas {
        match o.conn.functions(&o.database, &s).await {
            Ok(fs) => out.extend(fs.iter().map(|f| f.name.to_lowercase())),
            Err(e) => log::debug!("functions of {s}: {e:#}"),
        }
    }
    out
}

/// The statement as the connection's rules let agents run it.
pub async fn checked_sql(cx: &Call, a: &Args, p: &ConnectionProfile, sql: &str, write: bool) -> Result<Checked, ToolError> {
    let r = &p.mcp_rules;
    let d = p.engine.dialect();
    let mut tables: HashMap<(Option<String>, String), Option<Table>> = HashMap::new();
    let mut routines = HashSet::new();
    if restricted(r) {
        let o = cx.open(a, Need::Read).await?;
        for (s, n) in agent_sql::tables_named(sql, d) {
            let found = lookup(&o, s.as_deref(), &n).await;
            if let Some((t, kind)) = &found {
                view_reads_kept(&o, r, t, *kind).await.map_err(ToolError::Refused)?;
            }
            tables.insert((s, n), found.map(|(t, _)| t));
        }
        routines = user_routines(&o).await;
    }
    let find = |s: Option<&str>, n: &str| tables.get(&(s.map(str::to_string), n.to_string())).cloned().flatten();
    let ctx = agent_sql::Context { rules: r, connection: &p.name, dialect: d, tables: &find, routines: &routines };
    let c = agent_sql::check(sql, write, &ctx).map_err(ToolError::Refused)?;
    if c.rewritten {
        cx.note_rewritten(&c.sql);
    }
    Ok(c)
}

/// Cells are read this many characters past the agent's cut, so masks and sensitive types see
/// whole values: a value cut first could leave a card number or a CPF too short to match, and a
/// partial mask would show the cut's last characters, from inside the value.
pub const TAIL: usize = 1024;

fn privacy_on(cx: &Call, p: &ConnectionProfile) -> bool {
    restricted(&p.mcp_rules) || !cx.hub.types(Some(p)).is_empty()
}

/// The limits a read runs with: cells TAIL characters longer where something may be masked;
/// `mask_outcome` cuts them to `agent.max_cell` once masked.
pub fn read_limits(cx: &Call, p: &ConnectionProfile, agent: bounded::Limits) -> bounded::Limits {
    if privacy_on(cx, p) { bounded::Limits { max_cell: agent.max_cell.saturating_add(TAIL), ..agent } } else { agent }
}

fn cut_text(s: &str, max: usize) -> Option<String> {
    s.char_indices().nth(max).map(|(at, _)| format!("{}…", &s[..at]))
}

/// Masks the agent's copy (the user's tab keeps the values), then cuts cells to the agent's size:
/// sensitive columns in Masked data by name, and the switched-on sensitive types inside every
/// cell, both on the whole value. A masked column's value longer than was read is hidden whole.
pub fn mask_outcome(cx: &Call, p: &ConnectionProfile, out: &mut bounded::Outcome, masks: &[(String, Mask)], read: bounded::Limits, agent: bounded::Limits) {
    let key = cx.hub.mask_key();
    let types = cx.hub.types(Some(p));
    let mut counts = dataman_core::privacy::Counts::new();
    let mut used: Vec<(String, Mask)> = Vec::new();
    let was_cut = |s: &str| read.max_cell > agent.max_cell && s.ends_with('…') && s.chars().count() == read.max_cell + 1;
    let mut apply = |columns: &[bounded::Column], rows: &mut Vec<Vec<Value>>, cut_cells: &mut usize| {
        let at: Vec<(usize, Mask)> = columns.iter().enumerate().filter_map(|(i, c)| masks.iter().find(|(m, _)| m.eq_ignore_ascii_case(&c.name)).map(|(_, how)| (i, *how))).collect();
        for (i, how) in &at {
            let name = columns[*i].name.clone();
            if !used.iter().any(|(n, _)| *n == name) {
                used.push((name, *how));
            }
        }
        for row in rows.iter_mut() {
            for (i, v) in row.iter_mut().enumerate() {
                if v.is_null() {
                    continue;
                }
                if let Some((_, how)) = at.iter().find(|(c, _)| *c == i) {
                    let text = match &*v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let how = if was_cut(&text) { Mask::Hidden } else { *how };
                    *v = Value::String(mask_value(how, &text, &key));
                    continue;
                }
                if read.max_cell == agent.max_cell {
                    continue;
                }
                if let Value::String(s) = &*v {
                    let earlier = was_cut(s);
                    let scrubbed = types.scrub(s, &key, &mut counts).into_owned();
                    *v = Value::String(match cut_text(&scrubbed, agent.max_cell) {
                        Some(c) => {
                            *cut_cells += usize::from(!earlier);
                            c
                        }
                        None => scrubbed,
                    });
                } else if (v.is_object() || v.is_array()) && v.to_string().len() > agent.max_cell {
                    // JSON past the agent's size comes as cut text, as it did before.
                    let text = types.scrub(&v.to_string(), &key, &mut counts).into_owned();
                    *v = Value::String(cut_text(&text, agent.max_cell).unwrap_or(text));
                    *cut_cells += 1;
                }
            }
        }
    };
    let mut cut = out.cut_cells;
    apply(&out.columns, &mut out.rows, &mut cut);
    for part in out.parts.iter_mut() {
        let mut c = part.cut_cells;
        apply(&part.columns, &mut part.rows, &mut c);
        part.cut_cells = c;
    }
    out.cut_cells = cut;
    if !counts.is_empty() {
        cx.note_masked_values(&counts);
    }
    cx.note_columns(out.columns.len());
    cx.note_masked_columns(&used);
}

/// Tells the agent what the rules did to its reply.
pub fn tell(v: &mut Value, c: &Checked, masked: &[(String, Mask)]) {
    let Some(m) = v.as_object_mut() else { return };
    if c.rewritten {
        m.insert("narrowed".into(), json!("tables with columns hidden from agents were narrowed to their visible columns"));
    }
    if !masked.is_empty() {
        m.insert("masked_columns".into(), json!(masked.iter().map(|(c, how)| format!("{c} ({})", how.key())).collect::<Vec<_>>()));
    }
}

pub fn nosql_check(a: &Args, p: &ConnectionProfile, text: &str, write: bool) -> Result<Shaped, ToolError> {
    let database = a.opt_str("database").unwrap_or(&p.database);
    let ctx = agent_nosql::Context { rules: &p.mcp_rules, connection: &p.name, engine: p.engine, database };
    agent_nosql::check(text, write, &ctx).map_err(ToolError::Refused)
}

/// Shapes a NoSQL reply; listings drop hidden names (databases for `show dbs`).
pub fn nosql_shape(cx: &Call, a: &Args, p: &ConnectionProfile, text: &str, v: &mut Value, s: &Shaped) {
    if s.is_empty() {
        return;
    }
    let r = &p.mcp_rules;
    let names = Names::of(p.engine);
    let db = a.opt_str("database").unwrap_or(&p.database).to_string();
    let dbs = {
        let t = text.trim_start().to_ascii_lowercase();
        t.starts_with("show dbs") || t.starts_with("show databases")
    };
    let visible = |n: &str| {
        if dbs { r.visible.schema_visible(n, names) } else { r.visible.table_visible((names == Names::Qualified).then_some(db.as_str()), n, names) }
    };
    let masked = agent_nosql::shape(v, s, &cx.hub.mask_key(), &visible);
    cx.note_masked_columns(&masked);
    if !masked.is_empty()
        && let Some(m) = v.as_object_mut()
    {
        m.insert("masked_fields".into(), json!(masked.iter().map(|(c, how)| format!("{c} ({})", how.key())).collect::<Vec<_>>()));
    }
}

/// What list_connections tells agents about a connection's rules.
pub fn summary(p: &ConnectionProfile) -> Value {
    let r = &p.mcp_rules;
    let mut m = serde_json::Map::new();
    m.insert("sees".into(), json!(r.view.key()));
    if !r.visible.is_open() {
        m.insert(
            "visible".into(),
            json!(match r.visible.mode {
                dataman_core::privacy::VisibleMode::Except => "everything except some tables, schemas or columns",
                dataman_core::privacy::VisibleMode::Only => "only some tables and schemas",
            }),
        );
    }
    let mut off = Vec::new();
    if !r.structure {
        off.push("change structure");
    }
    if !r.code {
        off.push("run code");
    }
    if !r.activity_on() {
        off.push("see server activity");
    }
    if !off.is_empty() {
        m.insert("not_allowed".into(), json!(off));
    }
    if let Some(s) = r.time_limit_s {
        m.insert("time_limit_s".into(), json!(s));
    }
    if let Some(n) = r.read_rows {
        m.insert("rows_per_read".into(), json!(n));
    }
    if let Some(b) = r.read_bytes {
        m.insert("bytes_per_read".into(), json!(b));
    }
    Value::Object(m)
}

/// Connection details are kept from agents that do not see the data.
pub fn hides_metadata(p: &ConnectionProfile) -> bool {
    p.mcp_rules.view != AgentView::Data
}

pub fn table_visible(p: &ConnectionProfile, schema: &str, name: &str) -> bool {
    p.mcp_rules.visible.table_visible(Some(schema), name, Names::of(p.engine))
}

pub fn schema_visible(p: &ConnectionProfile, schema: &str) -> bool {
    p.mcp_rules.visible.schema_visible(schema, Names::of(p.engine))
}

pub fn column_hidden(p: &ConnectionProfile, schema: &str, table: &str, column: &str) -> bool {
    p.mcp_rules.visible.column_hidden(Some(schema), table, column, Names::of(p.engine))
}

pub fn not_visible(p: &ConnectionProfile, name: &str) -> ToolError {
    ToolError::Refused(format!("{name} is not visible to agents on {}", p.name))
}

/// Drops the lines of a definition that name a hidden column, and says how many there were.
pub fn ddl_without(ddl: &str, hidden: &[String]) -> String {
    if hidden.is_empty() {
        return ddl.to_string();
    }
    let names = |l: &str| {
        let low = l.to_lowercase();
        hidden.iter().any(|h| {
            let h = h.to_lowercase();
            low.match_indices(&h).any(|(i, _)| {
                let before = low[..i].chars().last().is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
                let after = low[i + h.len()..].chars().next().is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
                before && after
            })
        })
    };
    let mut out: Vec<&str> = ddl.lines().filter(|l| !names(l)).collect();
    // A comma left before the closing parenthesis of a column list.
    let mut fixed: Vec<String> = Vec::with_capacity(out.len());
    for l in out.drain(..) {
        if l.trim_start().starts_with(')')
            && let Some(prev) = fixed.last_mut()
            && prev.trim_end().ends_with(',')
        {
            let t = prev.trim_end().trim_end_matches(',').to_string();
            *prev = t;
        }
        fixed.push(l.to_string());
    }
    let n = hidden.len();
    format!("-- {n} column{} not visible to agents\n{}", if n == 1 { " is" } else { "s are" }, fixed.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_drops_hidden_columns() {
        let ddl = "CREATE TABLE public.users (\n    id bigint NOT NULL,\n    email text,\n    password_hash text\n);\nCREATE INDEX users_pw ON public.users (password_hash);";
        let out = ddl_without(ddl, &["password_hash".to_string()]);
        assert_eq!(out, "-- 1 column is not visible to agents\nCREATE TABLE public.users (\n    id bigint NOT NULL,\n    email text\n);");
        assert_eq!(ddl_without(ddl, &[]), ddl);
        assert!(ddl_without("a password_hash_old b", &["password_hash".to_string()]).contains("password_hash_old"));
    }
}
