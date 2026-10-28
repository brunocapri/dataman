use std::sync::Arc;

use dataman_core::bounded::{Limits, Mode};
use dataman_core::schema::objects::human_size;
use dataman_core::schema::{ConstraintKind, RelKind, Relation};
use dataman_core::{Connection, McpAccess};
use serde_json::{Map, Value, json};

use super::{Builtin, conn_schema, result_json, split_name, table};
use crate::policy::{self, Need, Target};
use crate::tool::{Args, BoxFut, Call, Opened, ToolError};

pub fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "list_connections",
            description: "Lists Dataman's saved database connections. Each row: name (pass it as `connection` to the other tools), env (none/dev/staging/production), mcp tier (off: unusable; read: reads only; write: non-destructive writes; full: destructive writes too, each after the user's Allow in the app), read_only, host, port, database, user, ssh (tunnelled), is_lar and version (known once connected; probe: true connects first), row_limit (rows a write may change before it waits for the user), allowed (statements the user lets run without asking in any tier, matched token for token with ? for one value; null: none), yolo (anything runs without asking; read-only still refuses writes). engine (postgres, mysql, sql-server, mongodb…), language (sql, or the engine's own: use nosql_query / nosql_execute). LAR has no EXPLAIN and no declared keys. Never returns passwords.",
            schema: crate::tool::Schema::new().prop("probe", false, json!({"type": "boolean", "description": "Connect to each usable connection first to fill is_lar and version."})),
            need: None,
            run: list_connections,
        },
        Builtin {
            name: "list_schemas",
            description: "Lists the schemas of a database with how many tables and views each has. System schemas (pg_catalog, information_schema) only with include_system: true.",
            schema: conn_schema().prop("include_system", false, json!({"type": "boolean", "description": "Include pg_catalog and information_schema."})),
            need: Some(Need::Read),
            run: list_schemas,
        },
        Builtin {
            name: "list_tables",
            description: "Lists tables, views, materialized views and foreign tables with estimated row counts (from statistics, not count(*); null when unknown) and total size. Rows: [schema, name, kind, rows_est, size]. Without schema, every non-system schema.",
            schema: conn_schema()
                .prop("schema", false, json!({"type": "string", "description": "Only this schema."}))
                .prop("kind", false, json!({"type": "string", "enum": ["all", "table", "view"], "description": "table: tables, partitioned and foreign tables; view: views and materialized views (default all)."}))
                .prop("limit", false, json!({"type": "integer", "minimum": 1, "maximum": 5000, "description": "Most rows (default 500)."})),
            need: Some(Need::Read),
            run: list_tables,
        },
        Builtin {
            name: "search_objects",
            description: "Finds tables, views, materialized views, functions, types (enum, composite, domain) and columns by name, case-insensitively, across all non-system schemas. pattern is a substring, or a LIKE pattern when it contains %. Rows: [kind, schema, name, detail] where a column's name is table.column and its detail the type, a function's detail its arguments and result.",
            schema: conn_schema()
                .prop("pattern", true, json!({"type": "string", "maxLength": 200, "description": "Substring of the name, or a LIKE pattern with % and _."}))
                .prop("kinds", false, json!({"type": "array", "maxItems": 6, "items": {"type": "string", "enum": ["table", "view", "materialized_view", "function", "type", "column"]}, "description": "Kinds to search (default all)."}))
                .prop("limit", false, json!({"type": "integer", "minimum": 1, "maximum": 500, "description": "Most rows (default 50)."})),
            need: Some(Need::Read),
            run: search_objects,
        },
        Builtin {
            name: "describe_table",
            description: "Describes a table or view: columns (name, type, not_null, default, pk), primary key, foreign keys both ways (foreign_keys: its own, as constraint definitions; referenced_by: other tables pointing at it), indexes with definitions, check/unique constraints, triggers, partitions or parent, estimated rows and size; the CREATE statement with include_ddl. On LAR, keys are inferred from names and marked inferred.",
            schema: conn_schema()
                .prop("table", true, json!({"type": "string", "description": "Name or schema.name (unqualified names follow the search_path)."}))
                .prop("include_ddl", false, json!({"type": "boolean", "description": "Add the CREATE TABLE / CREATE VIEW statement."})),
            need: Some(Need::Read),
            run: describe_table,
        },
        Builtin {
            name: "get_ddl",
            description: "Returns the CREATE statement of a table (columns, keys, indexes, constraints, partitioning, as read from the catalog), view or materialized view (its definition), function or procedure (pg_get_functiondef; every overload), or type (enum, composite, domain).",
            schema: conn_schema()
                .prop("object", true, json!({"type": "string", "description": "Name or schema.name."}))
                .prop("kind", false, json!({"type": "string", "enum": ["table", "view", "function", "type"], "description": "What the name is, when ambiguous (default: tables and views first, then functions, then types)."})),
            need: Some(Need::Read),
            run: get_ddl,
        },
        Builtin {
            name: "sample_rows",
            description: "Returns a few rows of a table or view (SELECT * … LIMIT n) in a read-only transaction, in run_query's format. n defaults to 10, at most 100; random: true samples randomly (ORDER BY random(), slow on big tables).",
            schema: conn_schema()
                .prop("table", true, json!({"type": "string", "description": "Name or schema.name."}))
                .prop("n", false, json!({"type": "integer", "minimum": 1, "maximum": 100, "description": "How many rows (default 10)."}))
                .prop("random", false, json!({"type": "boolean", "description": "Random rows instead of the first ones."})),
            need: Some(Need::Read),
            run: sample_rows,
        },
    ]
}

fn failed(e: anyhow::Error) -> ToolError {
    ToolError::Failed(format!("{e:#}"))
}

pub fn system_schema(s: &str) -> bool {
    matches!(s, "pg_catalog" | "information_schema") || s.starts_with("pg_toast") || s.starts_with("pg_temp")
}

fn kind_label(k: RelKind) -> &'static str {
    match k {
        RelKind::Table => "table",
        RelKind::View => "view",
        RelKind::MaterializedView => "materialized_view",
        RelKind::Foreign => "foreign_table",
        RelKind::Partitioned => "partitioned_table",
    }
}

fn language(e: dataman_core::Engine) -> &'static str {
    match e {
        dataman_core::Engine::Postgres | dataman_core::Engine::MySql | dataman_core::Engine::SqlServer => "sql",
        dataman_core::Engine::Sqlite => "sql (SQLite)",
        dataman_core::Engine::DuckDb => "sql (DuckDB)",
        dataman_core::Engine::MongoDb => "mongodb shell, statements or javascript (nosql_query / nosql_execute)",
        dataman_core::Engine::Elasticsearch | dataman_core::Engine::OpenSearch => "query dsl console (nosql_query / nosql_execute)",
        dataman_core::Engine::Redis => "redis commands (nosql_query / nosql_execute)",
    }
}

fn list_connections(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let probe = a.bool("probe");
        let profiles = cx.hub.profiles();
        let mut rows = Vec::new();
        for p in profiles.iter() {
            let access = p.mcp_access();
            if access == McpAccess::Off {
                let mut row = vec![json!(p.name), json!(p.env.label()), json!("off")];
                row.resize(14, Value::Null);
                row.extend([json!(p.engine.slug()), json!(language(p.engine)), Value::Null]);
                rows.push(row);
                continue;
            }
            let usable = policy::allow(true, Target::of(p), Need::Read).is_ok();
            let mut conn = cx.hub.connected(&p.id);
            if conn.is_none() && probe && usable {
                conn = cx.hub.connect(p).await.ok();
            }
            let (is_lar, version) = match &conn {
                Some(c) => (json!(c.info.is_lar), json!(c.info.version.split(" on ").next().unwrap_or_default())),
                None => (Value::Null, Value::Null),
            };
            // Agents that do not see the data do not learn where it lives either.
            let hide = super::rules::hides_metadata(p);
            let meta = |v: Value| if hide { Value::Null } else { v };
            rows.push(vec![
                json!(p.name),
                json!(p.env.label()),
                json!(access.label()),
                json!(p.read_only),
                // A file database's "host" is its file.
                meta(if p.engine.is_file() { json!(p.path) } else { json!(p.host) }),
                meta(if p.engine.is_file() { Value::Null } else { json!(p.port) }),
                json!(p.database),
                meta(json!(p.user)),
                meta(json!(p.ssh.is_some())),
                is_lar,
                version,
                if access >= McpAccess::Write && !p.mcp_yolo_on() { json!(cx.hub.row_limit(p)) } else { Value::Null },
                if p.mcp_allowed_on().is_empty() { Value::Null } else { json!(p.mcp_allowed_on()) },
                json!(p.mcp_yolo_on()),
                json!(p.engine.slug()),
                json!(language(p.engine)),
                super::rules::summary(p),
            ]);
        }
        Ok(table(&["name", "env", "mcp", "read_only", "host", "port", "database", "user", "ssh", "is_lar", "version", "row_limit", "allowed", "yolo", "engine", "language", "agents"], rows))
    })
}

pub(super) async fn schemas_of(o: &Opened, only: Option<&str>, system: bool) -> Result<Vec<String>, ToolError> {
    if let Some(s) = only {
        return Ok(vec![s.to_string()]);
    }
    let all = o.conn.schemas(&o.database).await.map_err(failed)?;
    let sp = o.conn.schema.search_path(&o.database);
    let mut v: Vec<String> = sp.into_iter().filter(|s| all.contains(s)).collect();
    for s in all {
        if !v.contains(&s) && (system || !system_schema(&s)) {
            v.push(s);
        }
    }
    Ok(v)
}

fn list_schemas(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        if !o.conn.engine().is_sql() {
            return super::nosql::databases(cx, &o).await;
        }
        cx.hub.fresh_schema(&o.profile.id, &o.database, &o.conn, false);
        let mut rows = Vec::new();
        for s in schemas_of(&o, None, a.bool("include_system")).await? {
            if !super::rules::schema_visible(&o.profile, &s) {
                continue;
            }
            let rels: Vec<Relation> = o.conn.relations(&o.database, &s).await.map_err(failed)?.into_iter().filter(|r| super::rules::table_visible(&o.profile, &s, &r.name)).collect();
            let tables = rels.iter().filter(|r| !r.kind.is_view()).count();
            rows.push(vec![json!(s), json!(tables), json!(rels.len() - tables)]);
        }
        cx.note_rows(rows.len());
        Ok(json!({"database": o.database, "schemas": table(&["schema", "tables", "views"], rows)}))
    })
}

fn list_tables(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        if !o.conn.engine().is_sql() {
            return super::nosql::collections(cx, &o, a.u64_or("limit", 500) as usize).await;
        }
        cx.hub.fresh_schema(&o.profile.id, &o.database, &o.conn, false);
        let limit = a.u64_or("limit", 500) as usize;
        let kind = a.opt_str("kind").unwrap_or("all");
        let mut rows = Vec::new();
        let mut more = false;
        'outer: for s in schemas_of(&o, a.opt_str("schema"), false).await? {
            let rels = o.conn.relations(&o.database, &s).await.map_err(failed)?;
            let rels: Vec<Relation> = rels
                .into_iter()
                .filter(|r| super::rules::table_visible(&o.profile, &s, &r.name))
                .filter(|r| match kind {
                    "table" => !r.kind.is_view(),
                    "view" => r.kind.is_view(),
                    _ => true,
                })
                .collect();
            if rels.is_empty() {
                continue;
            }
            let stats = o.conn.rel_stats(&o.database, &s).await.unwrap_or_default();
            for r in rels {
                if rows.len() >= limit {
                    more = true;
                    break 'outer;
                }
                if !view_open(&o, &r).await {
                    continue;
                }
                let st = stats.get(&r.name);
                rows.push(vec![
                    json!(s),
                    json!(r.name),
                    json!(kind_label(r.kind)),
                    st.and_then(|x| x.rows).map_or(Value::Null, |n| json!(n.round() as i64)),
                    st.and_then(|x| x.bytes).map_or(Value::Null, |b| json!(human_size(b))),
                ]);
            }
        }
        cx.note_rows(rows.len());
        let mut v = table(&["schema", "name", "kind", "rows_est", "size"], rows);
        if more {
            v["truncated"] = json!(true);
        }
        Ok(v)
    })
}

fn like(s: &str, p: &str) -> bool {
    fn go(s: &[char], p: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('%') => (0..=s.len()).any(|i| go(&s[i..], &p[1..])),
            Some('_') => !s.is_empty() && go(&s[1..], &p[1..]),
            Some(c) => s.first() == Some(c) && go(&s[1..], &p[1..]),
        }
    }
    let s: Vec<char> = s.to_lowercase().chars().collect();
    let p: Vec<char> = p.chars().collect();
    go(&s, &p)
}

pub fn like_matches(s: &str, p: &str) -> bool {
    like(s, p)
}

pub fn like_pattern(p: &str) -> String {
    let p = p.trim().to_lowercase();
    if p.contains('%') { p } else { format!("%{p}%") }
}

const SEARCH_SQL: &str = "SELECT kind, schema, name, detail FROM ( \
  SELECT CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized_view' ELSE 'table' END AS kind, n.nspname AS schema, c.relname AS name, \
         CASE c.relkind WHEN 'p' THEN 'partitioned' WHEN 'f' THEN 'foreign' ELSE NULL END AS detail \
    FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
   WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND c.relname ILIKE $1 \
  UNION ALL \
  SELECT 'function', n.nspname, p.proname, '(' || pg_catalog.pg_get_function_identity_arguments(p.oid) || ') → ' || coalesce(pg_catalog.pg_get_function_result(p.oid), 'procedure') \
    FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE p.proname ILIKE $1 \
  UNION ALL \
  SELECT 'type', n.nspname, t.typname, CASE t.typtype WHEN 'e' THEN 'enum' WHEN 'c' THEN 'composite' ELSE 'domain' END \
    FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
   WHERE t.typtype IN ('e', 'd', 'c') AND (t.typtype <> 'c' OR EXISTS (SELECT 1 FROM pg_catalog.pg_class r WHERE r.oid = t.typrelid AND r.relkind = 'c')) \
     AND t.typname ILIKE $1 \
  UNION ALL \
  SELECT 'column', n.nspname, c.relname || '.' || a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod) \
    FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
   WHERE a.attnum > 0 AND NOT a.attisdropped AND c.relkind IN ('r', 'p', 'v', 'm', 'f') AND a.attname ILIKE $1 \
) o WHERE schema NOT IN ('pg_catalog', 'information_schema') AND schema NOT LIKE 'pg\\_toast%' AND schema NOT LIKE 'pg\\_temp%' \
  AND kind = ANY($2::text[]) ORDER BY array_position($2::text[], kind), schema, name LIMIT $3::int8";

const KINDS: [&str; 6] = ["table", "view", "materialized_view", "function", "type", "column"];

fn search_objects(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        let pattern = like_pattern(a.str("pattern")?);
        let limit = a.u64_or("limit", 50) as usize;
        if !o.conn.engine().is_sql() {
            return super::nosql::search(cx, &o, &pattern, limit).await;
        }
        let kinds: Vec<String> = match a.get("kinds").and_then(Value::as_array) {
            Some(k) if !k.is_empty() => k.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            _ => KINDS.iter().map(|s| s.to_string()).collect(),
        };
        let header = ["kind", "schema", "name", "detail"];
        if !o.conn.info.is_lar && o.conn.engine().is_postgres() {
            let params = vec![Some(pattern.clone()), Some(format!("{{{}}}", kinds.join(","))), Some((limit + 1).to_string())];
            let limits = Limits { max_rows: limit + 1, ..Limits::default() };
            match cx.run(&o, SEARCH_SQL, params, Mode::Read, limits).await {
                Ok(r) => {
                    let more = r.rows.len() > limit;
                    let mut rows: Vec<Vec<Value>> = Vec::new();
                    let mut views: std::collections::HashMap<(String, String), bool> = std::collections::HashMap::new();
                    for row in r.rows.into_iter().filter(|row| found_visible(&o, row)) {
                        if rows.len() >= limit {
                            break;
                        }
                        // A view's own row, or one of its columns (`v.col`): kept with what the view reads.
                        let (kind, schema, name) = (row[0].as_str().unwrap_or(""), row[1].as_str().unwrap_or(""), row[2].as_str().unwrap_or(""));
                        let rel = match kind {
                            "view" | "materialized_view" | "table" => Some(name.to_string()),
                            "column" => name.rsplit_once('.').map(|(t, _)| t.to_string()),
                            _ => None,
                        };
                        if let Some(rel) = rel {
                            let key = (schema.to_string(), rel.clone());
                            let open = match views.get(&key) {
                                Some(v) => *v,
                                None => {
                                    let q = format!("{}.{}", dataman_core::sql::quote_ident(schema), dataman_core::sql::quote_ident(&rel));
                                    let v = match find_relation(&o, &q).await {
                                        Ok(r) => view_open(&o, &r).await,
                                        Err(_) => !super::rules::restricted(&o.profile.mcp_rules),
                                    };
                                    views.insert(key, v);
                                    v
                                }
                            };
                            if !open {
                                continue;
                            }
                        }
                        rows.push(row);
                    }
                    cx.note_rows(rows.len());
                    let mut v = table(&header, rows);
                    if more {
                        v["truncated"] = json!(true);
                    }
                    return Ok(v);
                }
                Err(ToolError::Cancelled) => return Err(ToolError::Cancelled),
                Err(e) => log::debug!("catalog search failed, walking the tree: {}", e.message()),
            }
        }
        cx.hub.fresh_schema(&o.profile.id, &o.database, &o.conn, false);
        let mut rows = Vec::new();
        let want = |k: &str| kinds.iter().any(|x| x == k);
        'outer: for s in schemas_of(&o, None, false).await? {
            for r in o.conn.relations(&o.database, &s).await.map_err(failed)? {
                if rows.len() > limit {
                    break 'outer;
                }
                let k = match r.kind {
                    RelKind::View => "view",
                    RelKind::MaterializedView => "materialized_view",
                    _ => "table",
                };
                if !super::rules::table_visible(&o.profile, &s, &r.name) || !view_open(&o, &r).await {
                    continue;
                }
                if want(k) && like(&r.name, &pattern) {
                    rows.push(vec![json!(k), json!(s), json!(r.name), Value::Null]);
                }
                if want("column") {
                    for c in o.conn.columns(&o.database, &s, &r.name).await.map_err(failed)? {
                        if like(&c.name, &pattern) && !super::rules::column_hidden(&o.profile, &s, &r.name, &c.name) {
                            rows.push(vec![json!("column"), json!(s), json!(format!("{}.{}", r.name, c.name)), json!(c.type_name)]);
                        }
                    }
                }
            }
        }
        let more = rows.len() > limit;
        rows.truncate(limit);
        cx.note_rows(rows.len());
        let mut v = table(&header, rows);
        if more {
            v["truncated"] = json!(true);
        }
        Ok(v)
    })
}

/// A catalog search hit agents may see (tables and columns as Visible to agents says; functions
/// and types while nothing is kept from them).
fn found_visible(o: &Opened, row: &[Value]) -> bool {
    let p = &o.profile;
    let (kind, schema, name) = (row[0].as_str().unwrap_or(""), row[1].as_str().unwrap_or(""), row[2].as_str().unwrap_or(""));
    if !super::rules::schema_visible(p, schema) {
        return false;
    }
    match kind {
        "column" => {
            let (t, c) = name.split_once('.').unwrap_or((name, ""));
            super::rules::table_visible(p, schema, t) && !super::rules::column_hidden(p, schema, t, c)
        }
        "function" | "type" => !super::rules::restricted(&p.mcp_rules),
        _ => super::rules::table_visible(p, schema, name),
    }
}

/// The relation, refused when it is not visible to agents.
async fn visible_relation(o: &Opened, name: &str) -> Result<Relation, ToolError> {
    let r = find_relation(o, name).await?;
    if !super::rules::table_visible(&o.profile, &r.schema, &r.name) {
        return Err(super::rules::not_visible(&o.profile, &r.name));
    }
    view_check(o, &r).await.map_err(ToolError::Refused)?;
    Ok(r)
}

/// A view that reads what is kept from agents is kept too: its rows, its definition and the
/// names of its columns (`v.password_hash`).
async fn view_check(o: &Opened, r: &Relation) -> Result<(), String> {
    if !super::rules::restricted(&o.profile.mcp_rules) || !matches!(r.kind, RelKind::View | RelKind::MaterializedView) {
        return Ok(());
    }
    let cols = o.conn.columns(&o.database, &r.schema, &r.name).await.map_err(|e| format!("{e:#}"))?;
    let t = dataman_core::agent_sql::Table { schema: r.schema.clone(), name: r.name.clone(), columns: cols.into_iter().map(|c| c.name).collect() };
    super::rules::view_reads_kept(o, &o.profile.mcp_rules, &t, r.kind).await
}

pub(crate) async fn view_open(o: &Opened, r: &Relation) -> bool {
    view_check(o, r).await.is_ok()
}

fn hidden_of(o: &Opened, r: &Relation, cols: &[dataman_core::schema::Column]) -> Vec<String> {
    cols.iter().filter(|c| super::rules::column_hidden(&o.profile, &r.schema, &r.name, &c.name)).map(|c| c.name.clone()).collect()
}

pub async fn find_relation(o: &Opened, name: &str) -> Result<Relation, ToolError> {
    let (schema, rel) = split_name(name);
    let schemas = schemas_of(o, schema.as_deref(), true).await?;
    let mut loose = None;
    for s in &schemas {
        let Ok(rels) = o.conn.relations(&o.database, s).await else { continue };
        if let Some(r) = rels.iter().find(|r| r.name == rel) {
            return Ok(r.clone());
        }
        if loose.is_none() {
            loose = rels.into_iter().find(|r| r.name.eq_ignore_ascii_case(&rel));
        }
    }
    loose.ok_or_else(|| ToolError::Invalid(format!("no table or view named {name:?} in {}", o.database)))
}

fn describe_table(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        if !o.conn.engine().is_sql() {
            return super::nosql::describe(cx, &o, a.str("table")?).await;
        }
        cx.hub.fresh_schema(&o.profile.id, &o.database, &o.conn, false);
        let r = visible_relation(&o, a.str("table")?).await?;
        let (db, c) = (o.database.as_str(), &o.conn);
        let mut cols = c.columns(db, &r.schema, &r.name).await.map_err(failed)?;
        let hidden = hidden_of(&o, &r, &cols);
        cols.retain(|c| !hidden.contains(&c.name));
        let mut out = Map::new();
        out.insert("table".into(), json!(format!("{}.{}", r.schema, r.name)));
        out.insert("kind".into(), json!(kind_label(r.kind)));
        let stats = c.rel_stats(db, &r.schema).await.unwrap_or_default();
        if let Some(st) = stats.get(&r.name) {
            if let Some(n) = st.rows {
                out.insert("rows_est".into(), json!(n.round() as i64));
            }
            if let Some(b) = st.bytes {
                out.insert("size".into(), json!(human_size(b)));
            }
        }
        let columns: Vec<Value> = cols
            .iter()
            .map(|col| {
                let mut m = Map::new();
                m.insert("name".into(), json!(col.name));
                m.insert("type".into(), json!(col.type_name));
                if col.nullable == Some(false) {
                    m.insert("not_null".into(), json!(true));
                }
                if let Some(d) = &col.default {
                    m.insert("default".into(), json!(d));
                }
                if col.primary_key {
                    m.insert("pk".into(), json!(true));
                }
                Value::Object(m)
            })
            .collect();
        out.insert("columns".into(), Value::Array(columns));
        let pk: Vec<&str> = cols.iter().filter(|x| x.primary_key).map(|x| x.name.as_str()).collect();
        if !pk.is_empty() {
            out.insert("primary_key".into(), json!(pk));
        }
        if c.info.is_lar {
            lar_links(c, db, &r, &mut out).await;
        } else {
            pg_details(cx, &o, &r, &mut out).await?;
        }
        if a.bool("include_ddl") {
            out.insert("ddl".into(), json!(super::rules::ddl_without(&ddl_of(c, db, &r).await?, &hidden)));
        }
        if super::rules::restricted(&o.profile.mcp_rules) {
            hide_details(&o, &hidden, &mut out);
        }
        Ok(Value::Object(out))
    })
}

/// Indexes, keys and triggers that name a hidden column go; links to hidden tables read
/// `(not visible)`.
fn hide_details(o: &Opened, hidden: &[String], out: &mut Map<String, Value>) {
    let names_hidden = |v: &Value| {
        let t = v.to_string().to_lowercase();
        hidden.iter().any(|h| {
            let h = h.to_lowercase();
            t.match_indices(&h).any(|(i, _)| {
                let ok = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
                ok(t[..i].chars().last()) && ok(t[i + h.len()..].chars().next())
            })
        })
    };
    for k in ["indexes", "foreign_keys", "constraints", "triggers"] {
        if let Some(Value::Array(a)) = out.get_mut(k) {
            a.retain(|x| !names_hidden(x));
        }
    }
    let p = &o.profile;
    let shown = |q: &str| {
        let (s, t) = q.split('(').next().unwrap_or(q).split_once('.').unwrap_or(("", q));
        super::rules::table_visible(p, s, t.trim())
    };
    for k in ["referenced_by", "foreign_keys"] {
        if let Some(Value::Array(a)) = out.get_mut(k) {
            for x in a.iter_mut() {
                let target = x.get(1).and_then(Value::as_str).or_else(|| x.get("from").and_then(Value::as_str)).or_else(|| x.get("references").and_then(Value::as_str)).map(str::to_string);
                if let Some(t) = target
                    && !shown(&t)
                {
                    *x = json!("(not visible)");
                }
            }
        }
    }
    out.retain(|_, v| !matches!(v, Value::Array(a) if a.is_empty()));
}

async fn pg_details(cx: &Call, o: &Opened, r: &Relation, out: &mut Map<String, Value>) -> Result<(), ToolError> {
    let (db, c) = (o.database.as_str(), &o.conn);
    let idx = c.indexes(db, &r.schema, &r.name).await.map_err(failed)?;
    if !idx.is_empty() {
        let v: Vec<Value> = idx
            .iter()
            .map(|i| match &i.definition {
                Some(d) => json!([i.name, d]),
                None => json!([i.name, format!("{}{}({})", if i.primary { "PRIMARY " } else { "" }, if i.unique { "UNIQUE " } else { "" }, i.columns.join(", "))]),
            })
            .collect();
        out.insert("indexes".into(), Value::Array(v));
    }
    let ex = c.table_extras(db, &r.schema, &r.name).await.map_err(failed)?;
    if let Some(cons) = &ex.constraints {
        let fks: Vec<Value> = cons.iter().filter(|k| k.kind == ConstraintKind::Foreign).map(|k| json!([k.name, k.definition])).collect();
        if !fks.is_empty() {
            out.insert("foreign_keys".into(), Value::Array(fks));
        }
        let other: Vec<Value> = cons
            .iter()
            .filter(|k| matches!(k.kind, ConstraintKind::Check | ConstraintKind::Unique | ConstraintKind::Exclusion))
            .map(|k| json!([k.name, k.definition]))
            .collect();
        if !other.is_empty() {
            out.insert("constraints".into(), Value::Array(other));
        }
    }
    if let Some(oid) = r.oid {
        let sql = "SELECT con.conname, n.nspname || '.' || t.relname, pg_catalog.pg_get_constraintdef(con.oid) \
                   FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class t ON t.oid = con.conrelid \
                   JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
                   WHERE con.contype = 'f' AND con.confrelid = $1::oid ORDER BY 2, 1";
        let limits = Limits { max_rows: 200, ..Limits::default() };
        if let Ok(rows) = cx.run(o, sql, vec![Some(oid.to_string())], Mode::Read, limits).await
            && !rows.rows.is_empty()
        {
            out.insert("referenced_by".into(), Value::Array(rows.rows.into_iter().map(Value::Array).collect()));
        }
    }
    if let Some(t) = ex.triggers.as_ref().filter(|t| !t.is_empty()) {
        out.insert("triggers".into(), Value::Array(t.iter().map(|t| json!([t.name, t.events, if t.enabled { "enabled" } else { "disabled" }])).collect()));
    }
    if let Some(p) = ex.partitions.as_ref().filter(|p| !p.is_empty()) {
        out.insert("partitions".into(), Value::Array(p.iter().map(|p| json!([format!("{}.{}", p.schema, p.name), p.bound])).collect()));
    }
    if let Some((s, n)) = &ex.parent {
        out.insert("parent".into(), json!(format!("{s}.{n}")));
    }
    Ok(())
}

async fn lar_links(c: &Arc<Connection>, db: &str, r: &Relation, out: &mut Map<String, Value>) {
    let Ok(g) = c.relationships(db, &r.schema).await else { return };
    let fmt = |s: &str, t: &str, cols: &[String]| format!("{s}.{t}({})", cols.join(", "));
    let own: Vec<Value> = g
        .links
        .iter()
        .filter(|l| l.from_table == r.name && l.from_schema == r.schema)
        .map(|l| json!({"columns": l.from_columns, "references": fmt(&l.to_schema, &l.to_table, &l.to_columns), "inferred": l.inferred}))
        .collect();
    let refs: Vec<Value> = g
        .links
        .iter()
        .filter(|l| l.to_table == r.name && l.to_schema == r.schema)
        .map(|l| json!({"from": fmt(&l.from_schema, &l.from_table, &l.from_columns), "columns": l.to_columns, "inferred": l.inferred}))
        .collect();
    if !own.is_empty() {
        out.insert("foreign_keys".into(), Value::Array(own));
    }
    if !refs.is_empty() {
        out.insert("referenced_by".into(), Value::Array(refs));
    }
}

async fn ddl_of(c: &Arc<Connection>, db: &str, r: &Relation) -> Result<String, ToolError> {
    if r.kind.is_view() {
        return Ok(c.view_definition(db, &r.schema, &r.name).await.map_err(failed)?.unwrap_or_else(|| "-- the server does not report the view's definition".into()));
    }
    c.table_ddl(db, &r.schema, &r.name).await.map_err(failed)
}

fn get_ddl(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        if !o.conn.engine().is_sql() {
            return Err(ToolError::Invalid(format!("{} has no DDL: describe_table lists a collection's fields and indexes", o.conn.engine().label())));
        }
        cx.hub.fresh_schema(&o.profile.id, &o.database, &o.conn, false);
        let name = a.str("object")?;
        let kind = a.opt_str("kind");
        let (db, c) = (o.database.as_str(), &o.conn);
        if kind.is_none_or(|k| k == "table" || k == "view") {
            match visible_relation(&o, name).await {
                Ok(r) => {
                    let cols = c.columns(db, &r.schema, &r.name).await.map_err(failed)?;
                    let hidden = hidden_of(&o, &r, &cols);
                    let ddl = super::rules::ddl_without(&ddl_of(c, db, &r).await?, &hidden);
                    return Ok(json!({"object": format!("{}.{}", r.schema, r.name), "kind": kind_label(r.kind), "ddl": ddl}));
                }
                Err(e @ ToolError::Refused(_)) => return Err(e),
                Err(e) if kind.is_some() => return Err(e),
                Err(_) => {}
            }
        }
        let (schema, base) = split_name(name);
        let schemas = schemas_of(&o, schema.as_deref(), true).await?;
        // A function's body can read anything: not shown while something is kept from agents.
        if super::rules::restricted(&o.profile.mcp_rules) {
            return Err(ToolError::Refused(format!("definitions of functions and types are not shown to agents on {} while some data is kept from them", o.profile.name)));
        }
        if kind.is_none_or(|k| k == "function") {
            for s in &schemas {
                let fs = c.functions(db, s).await.map_err(failed)?;
                let hits: Vec<_> = fs.iter().filter(|f| f.name == base).collect();
                if hits.is_empty() {
                    continue;
                }
                let mut defs = Vec::new();
                for f in hits {
                    if let Some(d) = c.routine_definition(db, s, f).await.map_err(failed)? {
                        defs.push(d);
                    }
                }
                return Ok(json!({"object": format!("{s}.{base}"), "kind": "function", "ddl": defs.join("\n\n")}));
            }
        }
        if kind.is_none_or(|k| k == "type") {
            for s in &schemas {
                if let Some(d) = c.type_definition(db, s, &base).await.map_err(failed)? {
                    return Ok(json!({"object": format!("{s}.{base}"), "kind": "type", "ddl": d}));
                }
            }
        }
        Err(ToolError::Invalid(format!("no {} named {name:?} in {db}", kind.unwrap_or("table, view, function or type"))))
    })
}

fn sample_rows(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        // The connection's limits (Rows per read, Size per read, Time per call), as run_query's.
        let limits = cx.limits(&a, a.u64_or("n", 10).clamp(1, 100));
        let n = limits.max_rows as u64;
        if !o.conn.engine().is_sql() {
            return super::nosql::sample(cx, &o, a.str("table")?, n, a.bool("random")).await;
        }
        if o.profile.mcp_rules.view == dataman_core::privacy::AgentView::Structure {
            return Err(ToolError::Refused(super::rules::structure_only(&o.profile)));
        }
        let r = visible_relation(&o, a.str("table")?).await?;
        let d = o.conn.dialect();
        let order = match (a.bool("random"), d) {
            (false, _) => "",
            (true, dataman_core::sql::Dialect::MySql) => " ORDER BY RAND()",
            (true, dataman_core::sql::Dialect::TSql) => " ORDER BY NEWID()",
            (true, _) => " ORDER BY random()",
        };
        let table = d.qualified(Some(&r.schema), &r.name);
        let sql = if d == dataman_core::sql::Dialect::TSql {
            format!("SELECT TOP ({n}) * FROM {table}{order}")
        } else {
            format!("SELECT * FROM {table}{order} LIMIT {n}")
        };
        let checked = super::rules::checked_sql(cx, &a, &o.profile, &sql, false).await?;
        let read = super::rules::read_limits(cx, &o.profile, limits);
        let mut out = cx.run(&o, &checked.sql, vec![], Mode::Read, read).await?;
        super::rules::mask_outcome(cx, &o.profile, &mut out, &checked.masked, read, limits);
        cx.note_rows(out.rows.len());
        let mut v = result_json(&out, n as usize);
        super::rules::tell(&mut v, &checked, &cx.note().seen.masked);
        Ok(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert_eq!(like_pattern("Order"), "%order%");
        assert_eq!(like_pattern("ord%"), "ord%");
        assert!(like("order_items", &like_pattern("ITEM")));
        assert!(like("Orders", "ord%"));
        assert!(!like("customer", "ord%"));
        assert!(like("a_b", "a_b") && like("axb", "a_b") && !like("ab", "a_b"));
    }
}
