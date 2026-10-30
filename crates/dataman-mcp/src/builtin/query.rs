use dataman_core::bounded::{self, Limits, Mode};
use dataman_core::explain::{self, Plan};
use dataman_core::guard::{self, WriteClass};
use dataman_core::sql::Dialect;
use serde_json::{Map, Value, json};

use super::{Builtin, confirm_prop, conn_schema, limits_props, ms, read_json, result_json, rules};
use crate::policy::Need;
use crate::tool::{Args, BoxFut, Call, Opened, ToolError, Write, params_schema};

fn sql_prop(desc: &'static str) -> Value {
    json!({"type": "string", "maxLength": 200000, "description": desc})
}

pub fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "run_query",
            description: "Runs read-only SQL (SELECT, WITH … SELECT, TABLE, VALUES, SHOW, EXPLAIN) inside a read-only transaction that is rolled back. Writes, row locks, transaction control and functions with side effects are refused before reaching the server (also on LAR, which does not enforce READ ONLY). One statement returns {columns: [[name, type]], rows: [[…]], row_count, truncated?, ms}: JSON values, null is SQL NULL (distinct from \"\"), numeric/dates/times as text, json embedded. Several reads separated by ; (or a T-SQL batch returning several result sets) return {results: [{statement, sql, columns, rows, row_count, truncated?, ms}], statements, ms}. At most max_rows rows (default 200) per result; text over 2000 chars is cut with …. Pass values as params ($1, $2, …; one statement only).",
            schema: limits_props(conn_schema().prop("sql", true, sql_prop("Read-only statements, separated by ;.")).prop("params", false, params_schema()), 200),
            need: Some(Need::Read),
            run: run_query,
        },
        Builtin {
            name: "open_query",
            description: "Runs SQL visibly in Dataman: while the app is open, the statement goes into this client's agent tab for the connection (one per connection and client, reused), runs there, and the full result stays there for the user (a result tab per statement); you get run_query's bounded result. Reads run as run_query does; a write follows the connection's MCP tier as execute does. Without the app it runs headless; writes that need approval are then refused. run: false only places the statement in the user's tab for them to review and run themselves (nothing runs; any tier but off; needs the app open): use it for writes the user wants to run personally. On MongoDB, Elasticsearch, OpenSearch and Redis, sql is the connection's own language (as nosql_query takes it), shown with its result in the tab's document view.",
            schema: limits_props(
                conn_schema()
                    .prop("sql", true, sql_prop("The statements (reads, or a write's), separated by ;."))
                    .prop("params", false, params_schema())
                    .prop("title", false, json!({"type": "string", "maxLength": 80, "description": "A name for what this does, shown in the agent tab's history (optional)."}))
                    .prop("run", false, json!({"type": "boolean", "description": "false: put the SQL in the user's agent tab without running it (a draft the user reviews and runs). Default true."})),
                200,
            ),
            need: Some(Need::Read),
            run: open_query,
        },
        Builtin {
            name: "explain",
            description: "Shows the plan of one statement without running it: EXPLAIN (FORMAT JSON, VERBOSE, COSTS) on PostgreSQL, LAR's own plan on LAR, EXPLAIN QUERY PLAN on SQLite, EXPLAIN on DuckDB; MySQL and SQL Server have no plans here. format: text (default, compact, as psql prints it) or json (PostgreSQL's plan). A write is planned, never run.",
            schema: conn_schema()
                .prop("sql", true, sql_prop("The statement to plan."))
                .prop("params", false, params_schema())
                .prop("format", false, json!({"type": "string", "enum": ["text", "json"], "description": "text (default) or json."})),
            need: Some(Need::Read),
            run: explain_plain,
        },
        Builtin {
            name: "explain_analyze",
            description: "PostgreSQL, LAR and DuckDB (SQLite has no ANALYZE: its query plan only). Runs one statement under EXPLAIN (ANALYZE, BUFFERS, …) and returns the plan with actual times, rows and loops, the totals, and hints (seq scans filtering most rows, sorts or hashes spilling to disk, nested loops with many loops, misestimates, missing workers). A read runs in a read-only transaction. A write (INSERT, UPDATE, DELETE, MERGE…) runs in a transaction that is always rolled back and follows the connection's MCP tier like execute: write allows non-destructive statements, full asks the user in Dataman for destructive ones, production always asks.",
            schema: confirm_prop(
                conn_schema()
                    .prop("sql", true, sql_prop("The statement to run and measure."))
                    .prop("params", false, params_schema())
                    .prop("format", false, json!({"type": "string", "enum": ["text", "json"], "description": "text (default) or json."}))
                    .prop("timeout_ms", false, json!({"type": "integer", "minimum": 100, "maximum": 300000, "description": "Time limit in ms (default 30000)."})),
            ),
            need: Some(Need::Read),
            run: explain_analyze,
        },
        Builtin {
            name: "execute",
            description: "Runs writes (one or more statements, separated by ;) in one transaction, committed only if all succeed, as the connection's MCP tier allows (set by the user in Dataman; the default is read). Tier write: INSERT, and UPDATE/DELETE/MERGE with a real WHERE (not 1=1 and the like), changing at most the connection's row limit (100 by default); anything destructive (no WHERE, TRUNCATE, DROP, ALTER, CREATE, GRANT, procedures, SELECT INTO, COPY…) is refused. Tier full: destructive statements and writes over the row limit wait for the user's Allow in the Dataman app (up to 120 s; denied or unanswered: rolled back). On production every write waits for the user. list_connections' allowed and yolo never wait. BEGIN/COMMIT are refused: each call is its own transaction. Returns each statement's command and affected rows, and the RETURNING rows of the last one (at most max_rows, default 50).",
            schema: confirm_prop(limits_props(conn_schema().prop("sql", true, sql_prop("The statements, separated by ;.")).prop("params", false, params_schema()), 50)),
            need: Some(Need::Write),
            run: execute,
        },
    ]
}

fn run_query(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        if super::nosql::is_nosql(cx, &a) {
            return super::nosql::query(cx, &a, "sql", false).await;
        }
        read(cx, &a, false).await
    })
}

async fn read(cx: &Call, a: &Args, open: bool) -> Result<Value, ToolError> {
    let sql = a.str("sql")?;
    let params = a.params()?;
    cx.note_sql(sql);
    let target = cx.target(a, Need::Read)?;
    let d = target.engine.dialect();
    let stmts = bounded::statements_in(sql, d);
    if let Some(r) = bounded::refusal_in(&stmts, params.len(), Mode::Read, cx.read_only(a, &target), d) {
        return Err(ToolError::Refused(r));
    }
    rules::structure_read(&target, &stmts)?;
    let checked = rules::checked_sql(cx, a, &target, sql, false).await?;
    let limits = cx.limits(a, 200);
    let read = rules::read_limits(cx, &target, limits);
    let front = cx.tab_front();
    let shown = front.is_some();
    let mut out = cx.read(a, &checked.sql, params, read, front, a.opt_str("title").map(str::to_string)).await?;
    rules::mask_outcome(cx, &target, &mut out, &checked.masked, read, limits);
    cx.note_rows(out.total_rows());
    let mut v = read_json(&out, limits.max_rows, &stmts);
    rules::tell(&mut v, &checked, &cx.note().seen.masked);
    if open && let Some(m) = v.as_object_mut() {
        m.insert("in_tab".into(), json!(shown));
    }
    Ok(v)
}

fn open_query(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(open(cx, a))
}

pub(super) async fn open(cx: &Call, a: Args) -> Result<Value, ToolError> {
    let sql = a.str("sql")?;
    if a.get("run").and_then(Value::as_bool) == Some(false) {
        return draft(cx, &a, sql).await;
    }
    if super::nosql::is_nosql(cx, &a) {
        let p = cx.target(&a, Need::Read)?;
        let reads = dataman_core::nosql::classify(p.engine, sql).is_ok_and(|c| c == WriteClass::Read);
        return if reads { super::nosql::query(cx, &a, "sql", true).await } else { super::nosql::execute(cx, &a, "sql", true).await };
    }
    let target = cx.target(&a, Need::Read)?;
    let d = target.engine.dialect();
    let stmts = bounded::statements_in(sql, d);
    let reads = guard::agent_write_class(sql, d) == WriteClass::Read && (stmts.len() == 1 || stmts.iter().all(|s| guard::agent_read_refusal_in(s, d).is_none()));
    if reads {
        return read(cx, &a, true).await;
    }
    let mut v = write(cx, &a).await?;
    if let Some(m) = v.as_object_mut() {
        m.insert("in_tab".into(), json!(cx.tab_front().is_some()));
    }
    Ok(v)
}

async fn draft(cx: &Call, a: &Args, sql: &str) -> Result<Value, ToolError> {
    if sql.trim().is_empty() {
        return Err(ToolError::Invalid("empty statement".into()));
    }
    if a.get("params").and_then(Value::as_array).is_some_and(|p| !p.is_empty()) {
        return Err(ToolError::Invalid("params are not used with run: false; the user runs the text as it is".into()));
    }
    let (p, d) = cx.draft(a, sql, a.opt_str("title").map(str::to_string)).await?;
    Ok(json!({"drafted": true, "tab": d.tab, "connection": p.name, "database": d.database}))
}

fn one(sql: &str, d: Dialect) -> Result<String, ToolError> {
    let mut s = bounded::statements_in(sql, d);
    match s.len() {
        1 => Ok(s.pop().unwrap()),
        0 => Err(ToolError::Invalid("empty statement".into())),
        _ => Err(ToolError::Invalid("one statement at a time".into())),
    }
}

fn plan_limits(timeout_ms: u64) -> Limits {
    Limits { max_rows: 10_000, max_bytes: 8 * 1024 * 1024, max_cell: 8 * 1024 * 1024, timeout: std::time::Duration::from_millis(timeout_ms.clamp(100, 300_000)) }
}

fn plan_json(out: &bounded::Outcome, format: &str, rolled_back: bool) -> Value {
    let columns: Vec<String> = out.columns.iter().map(|c| c.name.clone()).collect();
    let rows: Vec<Vec<Option<String>>> = out
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                })
                .collect()
        })
        .collect();
    let source = Plan::source_of(&columns, &rows);
    let plan = Plan::parse(&source);
    let mut m = Map::new();
    let as_json = format == "json" && plan.format == explain::Format::Json;
    m.insert("plan".into(), if as_json { serde_json::from_str(&source).unwrap_or(Value::String(source.clone())) } else { json!(explain::render_text(&plan)) });
    if !plan.hints.is_empty() {
        let hints: Vec<Value> = plan
            .hints
            .iter()
            .map(|h| {
                let mut x = Map::new();
                x.insert("node".into(), json!(plan.nodes.get(h.node).map(|n| n.title.clone()).unwrap_or_default()));
                x.insert("finding".into(), json!(h.text));
                if let Some(f) = &h.fix {
                    x.insert("fix".into(), json!(f));
                }
                Value::Object(x)
            })
            .collect();
        m.insert("hints".into(), Value::Array(hints));
    }
    if let Some(p) = plan.totals.planning_ms {
        m.insert("planning_ms".into(), json!(p));
    }
    if let Some(e) = plan.totals.execution_ms {
        m.insert("execution_ms".into(), json!(e));
    }
    if rolled_back {
        m.insert("rolled_back".into(), json!(true));
    }
    m.insert("ms".into(), json!(ms(out.elapsed)));
    Value::Object(m)
}

/// The statement to plan, as the connection's rules let agents see it (EXPLAIN's own options
/// are dropped when the statement had to be narrowed).
async fn narrowed(cx: &Call, a: &Args, o: &Opened, stmt: String) -> Result<String, ToolError> {
    let inner = explain::inner(&stmt).map(str::to_string).unwrap_or_else(|| stmt.clone());
    let writes = guard::classify_in(&inner, o.profile.engine.dialect()).is_write();
    let c = rules::checked_sql(cx, a, &o.profile, &inner, writes).await?;
    Ok(if c.rewritten { c.sql } else { stmt })
}

fn plans(o: &Opened) -> Result<(), ToolError> {
    if o.conn.caps().explain {
        Ok(())
    } else {
        Err(ToolError::Invalid(format!("plans are not available on {} connections", o.conn.engine().label())))
    }
}

fn explain_plain(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        plans(&o)?;
        let stmt = narrowed(cx, &a, &o, one(a.str("sql")?, o.profile.engine.dialect())?).await?;
        let sql = explain::sql(&stmt, false, explain::Target::of(&o.conn.info));
        if explain::analyzes(&sql) {
            return Err(ToolError::Invalid("this EXPLAIN runs the statement (ANALYZE): use explain_analyze".into()));
        }
        let out = cx.run(&o, &sql, a.params()?, Mode::Read, plan_limits(cx.time_limit(&a).as_millis() as u64)).await?;
        Ok(plan_json(&out, a.opt_str("format").unwrap_or("text"), false))
    })
}

fn explain_analyze(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let target = cx.target(&a, Need::Read)?;
        let stmt = one(a.str("sql")?, target.engine.dialect())?;
        let inner = explain::inner(&stmt).map(str::to_string).unwrap_or_else(|| stmt.clone());
        let writes = guard::classify_in(&inner, target.engine.dialect()).is_write();
        let limits = plan_limits(cx.time_limit(&a).as_millis() as u64);
        if target.mcp_rules.view == dataman_core::privacy::AgentView::Structure {
            return Err(ToolError::Refused(rules::structure_only(&target)));
        }
        if !writes {
            let o: Opened = cx.open(&a, Need::Read).await?;
            plans(&o)?;
            let stmt = narrowed(cx, &a, &o, stmt).await?;
            let sql = explain::sql(&stmt, true, explain::Target::of(&o.conn.info));
            let out = cx.run(&o, &sql, a.params()?, Mode::Read, limits).await?;
            return Ok(plan_json(&out, a.opt_str("format").unwrap_or("text"), false));
        }
        // EXPLAIN ANALYZE of a write runs it: the tiers judge the statement (allowed queries and YOLO
        // count), refused before connecting; always rolled back.
        rules::checked_sql(cx, &a, &target, &inner, true).await?;
        let t = crate::policy::Target { read_only: cx.read_only(&a, &target), ..crate::policy::Target::of(&target) };
        if let crate::policy::Verdict::Refuse(e) = crate::policy::judge(cx.hub.settings().mcp_enabled, t, &inner, target.mcp_allowed_on()).verdict {
            return Err(ToolError::Refused(e));
        }
        let o: Opened = cx.open(&a, Need::Read).await?;
        plans(&o)?;
        let sql = explain::sql(&stmt, true, explain::Target::of(&o.conn.info));
        let w = cx.write(&a, Write { sql: &sql, judged: Some(&inner), params: a.params()?, limits, rollback: true, visible: false, title: None }).await?;
        Ok(plan_json(&w.outcome, a.opt_str("format").unwrap_or("text"), true))
    })
}

fn execute(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        if super::nosql::is_nosql(cx, &a) {
            return super::nosql::execute(cx, &a, "sql", false).await;
        }
        write(cx, &a).await
    })
}

async fn write(cx: &Call, a: &Args) -> Result<Value, ToolError> {
    let sql = a.str("sql")?;
    let params = a.params()?;
    // `confirm` from older clients is accepted and ignored: only the user's Allow in the app
    // lets a destructive write through.
    let limits = cx.limits(a, 50);
    let visible = cx.tab_front().is_some();
    let title = a.opt_str("title").map(str::to_string);
    let target = cx.target(a, Need::Read)?;
    let checked = rules::checked_sql(cx, a, &target, sql, true).await?;
    let read = rules::read_limits(cx, &target, limits);
    let r = cx.write(a, Write { sql: &checked.sql, judged: None, params, limits: read, rollback: false, visible, title }).await;
    // DDL changes what the catalog tools see (even when it failed half-way on LAR, whose DDL is
    // not transactional).
    if let Ok(p) = cx.hub.profile(a.str("connection")?) {
        let d = p.engine.dialect();
        if bounded::statements_in(sql, d).iter().any(|s| guard::classify_in(s, d).effect == guard::Effect::Ddl)
            && let Some(conn) = cx.hub.connected(&p.id)
        {
            let db = a.opt_str("database").map(str::to_string).unwrap_or_else(|| conn.database().to_string());
            cx.hub.fresh_schema(&p.id, &db, &conn, true);
        }
    }
    let mut out = r?.outcome;
    // RETURNING rows are the agent's copy too.
    rules::mask_outcome(cx, &target, &mut out, &checked.masked, read, limits);
    {
        let affected = out.affected();
        cx.note_rows(affected as usize);
        let mut m = Map::new();
        m.insert("committed".into(), json!(true));
        m.insert("statements".into(), Value::Array(out.statements.iter().map(|d| json!([d.verb, d.affected])).collect()));
        if !out.columns.is_empty() {
            let mut ret = result_json(&out, limits.max_rows);
            if let Some(obj) = ret.as_object_mut() {
                obj.remove("ms");
            }
            m.insert("returning".into(), ret);
        }
        m.insert("ms".into(), json!(ms(out.elapsed)));
        if let Some(n) = cx.note().approval.filter(|n| n.decision == crate::audit::Decision::Approved) {
            m.insert("approved".into(), json!({"by": n.by, "reasons": n.reasons}));
        }
        Ok(Value::Object(m))
    }
}
