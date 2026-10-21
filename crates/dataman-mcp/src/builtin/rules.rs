//! A connection's rules for agents (Dataman › Agents › Privacy and Permissions) around the tools:
//! statements are checked (and narrowed) before they run, results are masked before they leave,
//! and listings drop what agents may not see.

use std::collections::{HashMap, HashSet};

use dataman_core::agent_nosql::{self, Shaped};
use dataman_core::agent_sql::{self, Checked, Table};
use dataman_core::privacy::{AgentRules, AgentView, Generated, Mask, Names, mask_value};
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

const PG_GENERATED: &str = "SELECT n.nspname, c.relname, a.attname, d.attname FROM pg_catalog.pg_attribute a \
  JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
  JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
  JOIN pg_catalog.pg_depend dep ON dep.classid = 'pg_catalog.pg_attrdef'::regclass AND dep.objid = ad.oid \
   AND dep.refclassid = 'pg_catalog.pg_class'::regclass AND dep.refobjid = a.attrelid AND dep.refobjsubid > 0 \
  JOIN pg_catalog.pg_attribute d ON d.attrelid = a.attrelid AND d.attnum = dep.refobjsubid \
  WHERE a.attgenerated <> '' AND NOT a.attisdropped";

const MY_GENERATED: &str = "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, GENERATION_EXPRESSION FROM information_schema.COLUMNS \
  WHERE (EXTRA LIKE '%VIRTUAL GENERATED%' OR EXTRA LIKE '%STORED GENERATED%') \
   AND TABLE_SCHEMA NOT IN ('mysql', 'sys', 'performance_schema', 'information_schema')";

const MS_GENERATED: &str = "SELECT s.name, t.name, c.name, d.name FROM sys.computed_columns c \
  JOIN sys.objects t ON t.object_id = c.object_id JOIN sys.schemas s ON s.schema_id = t.schema_id \
  JOIN sys.sql_expression_dependencies x ON x.referencing_id = c.object_id AND x.referencing_minor_id = c.column_id \
  JOIN sys.columns d ON d.object_id = x.referenced_id AND d.column_id = x.referenced_minor_id";

/// The names an expression may read: quoted (`` `a` ``, `[a]`, `"a"`) and bare words. MySQL
/// keeps no dependencies for generated columns, so its expression is read, erring on more.
fn expression_names(e: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in dataman_core::sql::tokenize_in(e, dataman_core::Dialect::MySql) {
        if matches!(t.kind, dataman_core::sql::TokenKind::Ident | dataman_core::sql::TokenKind::QuotedIdent | dataman_core::sql::TokenKind::Keyword) {
            let w = t.text(e).trim_matches(['`', '"', '[', ']']).to_string();
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// Generated (computed) columns of the database and the columns each is computed from, by the
/// engine's catalog: PostgreSQL's pg_depend, SQL Server's sql_expression_dependencies, MySQL's
/// generation expression.
pub async fn generated_columns(cx: &Call, o: &Opened) -> Result<Vec<Generated>, String> {
    let (sql, by_expression) = match o.conn.engine() {
        dataman_core::Engine::Postgres => (PG_GENERATED, false),
        dataman_core::Engine::MySql => (MY_GENERATED, true),
        dataman_core::Engine::SqlServer => (MS_GENERATED, false),
        _ => return Ok(Vec::new()),
    };
    let limits = bounded::Limits { max_rows: 100_000, max_bytes: 32 * 1024 * 1024, max_cell: 64 * 1024, ..bounded::Limits::default() };
    let cancel = cx.cancel.clone().cancelled_owned();
    let out = o.conn.bounded(Some(&o.database), sql, vec![], bounded::Mode::Read, limits, cancel).await.map_err(|e| format!("{e}"))?;
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let mut cols: Vec<Generated> = Vec::new();
    for row in &out.rows {
        let (schema, table, column) = (text(&row[0]), text(&row[1]), text(&row[2]));
        let from = if by_expression { expression_names(&text(&row[3])) } else { vec![text(&row[3])] };
        match cols.iter_mut().find(|g| g.schema == schema && g.table == table && g.column == column) {
            Some(g) => g.from.extend(from),
            None => cols.push(Generated { schema, table, column, from }),
        }
    }
    Ok(cols)
}

/// The connection's rules as they apply on this database: generated columns protected as the
/// columns they are computed from.
pub async fn effective_rules(cx: &Call, o: &Opened) -> Result<AgentRules, ToolError> {
    let r = &o.profile.mcp_rules;
    if !restricted(r) || !o.conn.engine().is_sql() || o.conn.info.is_lar {
        return Ok(r.clone());
    }
    let cols = generated_columns(cx, o).await.map_err(|e| ToolError::Failed(format!("Dataman could not read the generated columns of {} ({e}), and {} keeps some data from agents", o.database, o.profile.name)))?;
    Ok(r.with_generated(&cols, Names::Qualified))
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
    let mut r = p.mcp_rules.clone();
    let d = p.engine.dialect();
    let mut tables: HashMap<(Option<String>, String), Option<Table>> = HashMap::new();
    let mut routines = HashSet::new();
    if restricted(&r) {
        let o = cx.open(a, Need::Read).await?;
        r = o.profile.mcp_rules.clone();
        for (s, n) in agent_sql::tables_named(sql, d) {
            let found = lookup(&o, s.as_deref(), &n).await;
            if let Some((t, kind)) = &found {
                view_reads_kept(&o, &r, t, *kind).await.map_err(ToolError::Refused)?;
            }
            tables.insert((s, n), found.map(|(t, _)| t));
        }
        routines = user_routines(&o).await;
    }
    let find = |s: Option<&str>, n: &str| tables.get(&(s.map(str::to_string), n.to_string())).cloned().flatten();
    let ctx = agent_sql::Context { rules: &r, connection: &p.name, dialect: d, tables: &find, routines: &routines };
    let c = agent_sql::check(sql, write, &ctx).map_err(ToolError::Refused)?;
    if c.rewritten {
        cx.note_rewritten(&c.sql);
    }
    Ok(c)
}

/// Cells are read this many characters past the agent's cut, so masks and sensitive types see
/// whole values: a value cut first could leave a card or phone number too short to match, and a
/// partial mask would show the cut's last characters, from inside the value.
pub const TAIL: usize = 1024;

fn privacy_on(cx: &Call, p: &ConnectionProfile) -> bool {
    restricted(&p.mcp_rules) || !cx.hub.types().is_empty()
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
pub fn mask_outcome(cx: &Call, out: &mut bounded::Outcome, masks: &[(String, Mask)], read: bounded::Limits, agent: bounded::Limits) {
    let key = cx.hub.mask_key();
    let types = cx.hub.types();
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

fn nosql_check_as(a: &Args, p: &ConnectionProfile, text: &str, write: bool, wide: bool) -> Result<Shaped, ToolError> {
    let database = a.opt_str("database").unwrap_or(&p.database);
    let ctx = agent_nosql::Context { rules: &p.mcp_rules, connection: &p.name, engine: p.engine, database, wide };
    agent_nosql::check(text, write, &ctx).map_err(ToolError::Refused)
}

/// The statement as the connection's rules let agents run it. Where something is kept, names
/// that stand for other data are looked up first: a MongoDB view reads its collection under
/// its own names, and an Elasticsearch/OpenSearch alias or data stream reads its indices.
pub async fn nosql_check(cx: &Call, a: &Args, p: &ConnectionProfile, text: &str, write: bool) -> Result<Shaped, ToolError> {
    let shaped = nosql_check_as(a, p, text, write, false)?;
    let r = &p.mcp_rules;
    if !restricted(r) || dataman_core::nosql::is_script(p.engine, text) {
        return Ok(shaped);
    }
    match p.engine {
        dataman_core::Engine::MongoDb => {
            let database = a.opt_str("database").unwrap_or(&p.database).to_string();
            let reads = agent_nosql::mongo_reads(text, &database);
            if reads.is_empty() {
                return Ok(shaped);
            }
            let o = cx.open(a, Need::Read).await?;
            mongo_views(&o, r, reads).await.map_err(ToolError::Refused)?;
            Ok(shaped)
        }
        dataman_core::Engine::Elasticsearch | dataman_core::Engine::OpenSearch => {
            let o = cx.open(a, Need::Read).await?;
            if es_aliases(&o, r, text).await.map_err(ToolError::Refused)? { nosql_check_as(a, p, text, write, true) } else { Ok(shaped) }
        }
        _ => Ok(shaped),
    }
}

fn ran_docs(ran: &dataman_core::nosql::Ran) -> Vec<Value> {
    use dataman_core::nosql::Output;
    match &ran.output {
        Output::Docs(d) => d.docs.iter().map(|x| x.body.to_json()).collect(),
        Output::Value(b) => {
            let v = b.to_json();
            match v.pointer("/cursor/firstBatch") {
                Some(Value::Array(a)) => a.clone(),
                _ => vec![v],
            }
        }
        _ => Vec::new(),
    }
}

/// Views among the collections read, followed to what they read: a view over a collection that
/// keeps something from agents is refused whole, as SQL views are.
async fn mongo_views(o: &Opened, r: &AgentRules, reads: Vec<(String, String)>) -> Result<(), String> {
    let conn = &o.profile.name;
    let mut views: HashMap<String, Vec<(String, Value)>> = HashMap::new();
    let mut todo = reads;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    while let Some((db, coll)) = todo.pop() {
        if !seen.insert((db.clone(), coll.clone())) {
            continue;
        }
        if seen.len() > VIEW_DEPTH {
            return Err(format!("{coll} reads through more views than Dataman follows, and {conn} keeps some data from agents"));
        }
        if !views.contains_key(&db) {
            let stmt = "db.runCommand({listCollections: 1, filter: {type: \"view\"}})";
            let ran = o.conn.run_nosql(&db, stmt, dataman_core::nosql::Page::default()).await.map_err(|e| format!("Dataman could not check the views of {db} ({e:#}), and {conn} keeps some data from agents"))?;
            let found = ran_docs(&ran).into_iter().filter_map(|d| Some((d.get("name")?.as_str()?.to_string(), d.get("options").cloned().unwrap_or(Value::Null)))).collect();
            views.insert(db.clone(), found);
        }
        let Some((_, options)) = views[&db].iter().find(|(n, _)| *n == coll) else { continue };
        let on = options.get("viewOn").and_then(Value::as_str).unwrap_or_default().to_string();
        let pipeline = options.get("pipeline").cloned().unwrap_or(Value::Array(Vec::new()));
        let mut under = vec![(db.clone(), on)];
        under.extend(agent_nosql::mongo_reads(&format!("db.x.aggregate({pipeline})"), &db).into_iter().filter(|(_, c)| c != "x"));
        for (udb, u) in under {
            if agent_nosql::mongo_keeps(r, &udb, &u) {
                return Err(format!("{coll} is a view that reads {u}, which keeps data from agents on {conn}: query {u} directly"));
            }
            todo.push((udb, u));
        }
    }
    Ok(())
}

/// Whether the request names an alias or a data stream; an alias over an index agents may not
/// see is refused.
async fn es_aliases(o: &Opened, r: &AgentRules, text: &str) -> Result<bool, String> {
    let conn = &o.profile.name;
    let Ok(req) = dataman_core::nosql::console::parse(text) else { return Ok(false) };
    let path = req.path.split('?').next().unwrap_or("").trim_start_matches('/');
    let first = path.split('/').next().unwrap_or("");
    let mut names: Vec<String> = if first.starts_with('_') { Vec::new() } else { first.split(',').map(str::to_string).collect() };
    let bodies: Vec<Value> = match &req.body {
        dataman_core::nosql::console::Body::Json(v) => vec![v.clone()],
        dataman_core::nosql::console::Body::Lines(l) => l.clone(),
        dataman_core::nosql::console::Body::None => Vec::new(),
    };
    for b in &bodies {
        for k in ["_index", "index"] {
            let mut vals: Vec<&Value> = b.get(k).into_iter().collect();
            if let Some(Value::Array(docs)) = b.get("docs") {
                vals.extend(docs.iter().filter_map(|d| d.get(k)));
            }
            for v in vals {
                match v {
                    Value::String(x) => names.extend(x.split(',').map(str::to_string)),
                    Value::Array(a) => names.extend(a.iter().filter_map(Value::as_str).map(str::to_string)),
                    _ => {}
                }
            }
        }
    }
    names.retain(|n| !n.is_empty() && !n.contains('*') && !n.starts_with('-') && n != "_all");
    if names.is_empty() {
        return Ok(false);
    }
    names.sort();
    names.dedup();
    let stmt = format!("GET _resolve/index/{}?expand_wildcards=all", names.join(","));
    let ran = match o.conn.run_nosql("", &stmt, dataman_core::nosql::Page::default()).await {
        Ok(r) => r,
        // A name that does not exist resolves to nothing: the request itself will say so.
        Err(e) if format!("{e:#}").contains("index_not_found") => return Ok(false),
        Err(e) => return Err(format!("Dataman could not resolve {} ({e:#}), and {conn} keeps some data from agents", names.join(","))),
    };
    let v = ran_docs(&ran).into_iter().next().unwrap_or(Value::Null);
    let names_of = |key: &str| -> Vec<(String, Vec<String>)> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let n = x.get("name")?.as_str()?.to_string();
                        let list = x.get("indices").or_else(|| x.get("backing_indices")).and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
                        Some((n, list))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut wide = false;
    for (alias, indices) in names_of("aliases") {
        if !names.contains(&alias) {
            continue;
        }
        wide = true;
        if let Some(i) = indices.iter().find(|i| !r.visible.table_visible(None, i, Names::Flat)) {
            return Err(format!("{alias} is an alias of {i}, which is not visible to agents on {conn}"));
        }
    }
    wide |= names_of("data_streams").iter().any(|(d, _)| names.contains(d));
    Ok(wide)
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
