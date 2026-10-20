use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use dataman_core::McpAccess;
use dataman_core::bounded::{Limits, Mode};
use serde_json::{Value, json};

use crate::builtin::catalog_system_schema;
use crate::policy::Need;
use crate::tool::{Args, Call, Opened, ToolError};

pub const SCHEME: &str = "dataman://";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uri {
    pub connection: String,
    pub database: Option<String>,
    pub json: bool,
    pub refresh: bool,
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

pub fn uri(connection: &str, database: &str, json: bool) -> String {
    format!("{SCHEME}{}/{}/schema{}", encode(connection), encode(database), if json { ".json" } else { "" })
}

pub fn parse(u: &str) -> Option<Uri> {
    let rest = u.strip_prefix(SCHEME)?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let parts: Vec<&str> = path.split('/').collect();
    let [conn, db, last] = parts.as_slice() else { return None };
    let json = match *last {
        "schema" => false,
        "schema.json" => true,
        _ => return None,
    };
    let refresh = query.split('&').any(|kv| kv == "refresh" || kv == "refresh=true" || kv == "refresh=1");
    let db = decode(db);
    Some(Uri { connection: decode(conn), database: (!db.is_empty() && db != "-").then_some(db), json, refresh })
}

pub fn list(cx: &Call) -> Vec<(String, String, bool)> {
    if !cx.hub.settings().mcp_enabled {
        return Vec::new();
    }
    let mut v = Vec::new();
    for p in cx.hub.profiles().iter().filter(|p| p.mcp_access() != McpAccess::Off) {
        for json in [false, true] {
            v.push((uri(&p.name, &p.database, json), format!("{} · {} schema{}", p.name, p.database, if json { " (JSON)" } else { "" }), json));
        }
    }
    v
}

#[derive(Default)]
struct Rel {
    kind: String,
    rows: Option<i64>,
    columns: Vec<(String, String, bool, bool)>,
    fks: Vec<String>,
}

const COLUMNS_SQL: &str = "SELECT n.nspname, c.relname, c.relkind::text, a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, \
  EXISTS (SELECT 1 FROM pg_catalog.pg_index i WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey)), \
  CASE WHEN c.reltuples < 0 THEN NULL ELSE c.reltuples::int8 END \
  FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
  JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
  WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
    AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%' AND NOT c.relispartition \
  ORDER BY 1, 2, a.attnum";

const FKS_SQL: &str = "SELECT n.nspname, c.relname, pg_catalog.pg_get_constraintdef(con.oid) FROM pg_catalog.pg_constraint con \
  JOIN pg_catalog.pg_class c ON c.oid = con.conrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
  WHERE con.contype = 'f' AND n.nspname NOT IN ('pg_catalog', 'information_schema') ORDER BY 1, 2, con.conname";

fn relkind(k: &str) -> &'static str {
    match k {
        "v" => "view",
        "m" => "materialized view",
        "f" => "foreign table",
        "p" => "partitioned table",
        _ => "table",
    }
}

type Tree = BTreeMap<String, BTreeMap<String, Rel>>;

async fn read_pg(cx: &Call, o: &Opened) -> Result<(Tree, bool), ToolError> {
    let limits = Limits { max_rows: 100_000, max_bytes: 32 * 1024 * 1024, ..Limits::default() };
    let cols = cx.run(o, COLUMNS_SQL, vec![], Mode::Read, limits).await?;
    let mut t: Tree = BTreeMap::new();
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    for r in &cols.rows {
        let rel = t.entry(s(&r[0])).or_default().entry(s(&r[1])).or_default();
        rel.kind = relkind(r[2].as_str().unwrap_or("r")).into();
        rel.rows = r[7].as_i64();
        rel.columns.push((s(&r[3]), s(&r[4]), r[6].as_bool() == Some(true), r[5].as_bool() == Some(true)));
    }
    if let Ok(fks) = cx.run(o, FKS_SQL, vec![], Mode::Read, Limits { max_rows: 20_000, ..limits }).await {
        for r in &fks.rows {
            if let Some(rel) = t.get_mut(&s(&r[0])).and_then(|m| m.get_mut(&s(&r[1]))) {
                rel.fks.push(s(&r[2]));
            }
        }
    }
    Ok((t, cols.truncated.is_some()))
}

async fn read_walk(o: &Opened) -> Result<(Tree, bool), ToolError> {
    let f = |e: anyhow::Error| ToolError::Failed(format!("{e:#}"));
    let (db, c) = (o.database.as_str(), &o.conn);
    let mut t: Tree = BTreeMap::new();
    for s in c.schemas(db).await.map_err(f)? {
        if catalog_system_schema(&s) {
            continue;
        }
        let rels = c.relations(db, &s).await.map_err(f)?;
        if rels.is_empty() {
            continue;
        }
        let stats = c.rel_stats(db, &s).await.unwrap_or_default();
        let links = c.relationships(db, &s).await.ok();
        for r in rels {
            let cols = c.columns(db, &s, &r.name).await.map_err(f)?;
            let mut rel = Rel { kind: r.kind.label().into(), rows: stats.get(&r.name).and_then(|x| x.rows).map(|n| n as i64), ..Default::default() };
            rel.columns = cols.into_iter().map(|x| (x.name, x.type_name, x.primary_key, x.nullable == Some(false))).collect();
            if let Some(g) = &links {
                for l in g.links.iter().filter(|l| l.from_table == r.name) {
                    rel.fks.push(format!(
                        "({}) → {}.{}({}){}",
                        l.from_columns.join(", "),
                        l.to_schema,
                        l.to_table,
                        l.to_columns.join(", "),
                        if l.inferred { " inferred" } else { "" }
                    ));
                }
            }
            t.entry(s.clone()).or_default().insert(r.name, rel);
        }
    }
    Ok((t, false))
}

fn markdown(head: &str, t: &Tree, truncated: bool) -> String {
    let mut out = format!("# {head}\n");
    for (schema, rels) in t {
        let _ = write!(out, "\n## {schema}\n");
        for (name, r) in rels {
            let rows = r.rows.map(|n| format!(" ~{} rows", n)).unwrap_or_default();
            let cols: Vec<String> = r
                .columns
                .iter()
                .map(|(n, ty, pk, nn)| format!("{n} {ty}{}{}", if *pk { " PK" } else { "" }, if *nn && !*pk { " NOT NULL" } else { "" }))
                .collect();
            let _ = writeln!(out, "- {name} [{}{rows}]: {}", r.kind, cols.join(", "));
            for fk in &r.fks {
                let _ = writeln!(out, "  - {fk}");
            }
        }
    }
    if truncated {
        out.push_str("\n(truncated: too many columns)\n");
    }
    out
}

fn as_json(o: &Opened, t: &Tree, truncated: bool) -> Value {
    let schemas: serde_json::Map<String, Value> = t
        .iter()
        .map(|(s, rels)| {
            let m: serde_json::Map<String, Value> = rels
                .iter()
                .map(|(n, r)| {
                    let cols: Vec<Value> = r
                        .columns
                        .iter()
                        .map(|(c, ty, pk, nn)| {
                            let mut v = vec![json!(c), json!(ty)];
                            if *pk {
                                v.push(json!("pk"));
                            }
                            if *nn {
                                v.push(json!("not_null"));
                            }
                            Value::Array(v)
                        })
                        .collect();
                    let mut x = json!({"kind": r.kind, "columns": cols});
                    if let Some(n) = r.rows {
                        x["rows_est"] = json!(n);
                    }
                    if !r.fks.is_empty() {
                        x["foreign_keys"] = json!(r.fks);
                    }
                    (n.clone(), x)
                })
                .collect();
            (s.clone(), Value::Object(m))
        })
        .collect();
    let mut v = json!({"connection": o.profile.name, "database": o.database, "server": o.conn.info.version, "schemas": schemas});
    if truncated {
        v["truncated"] = json!(true);
    }
    v
}

pub async fn read(cx: &Call, u: &Uri) -> Result<Arc<String>, ToolError> {
    let mut m = serde_json::Map::new();
    m.insert("connection".into(), json!(u.connection));
    if let Some(d) = &u.database {
        m.insert("database".into(), json!(d));
    }
    let o = cx.open(&Args(m), Need::Read).await?;
    cx.hub.fresh_schema(&o.profile.id, &o.database, &o.conn, u.refresh);
    let key = (o.profile.id.clone(), o.database.clone(), u.json);
    if let Some(s) = cx.hub.snapshots.lock().get(&key) {
        return Ok(s.clone());
    }
    let (t, truncated) = if o.conn.info.is_lar || !o.conn.engine().is_postgres() {
        read_walk(&o).await?
    } else {
        match read_pg(cx, &o).await {
            Ok(x) => x,
            Err(ToolError::Cancelled) => return Err(ToolError::Cancelled),
            Err(_) => read_walk(&o).await?,
        }
    };
    let text = if u.json {
        serde_json::to_string(&as_json(&o, &t, truncated)).unwrap_or_default()
    } else {
        let server = o.conn.info.version.split(" on ").next().unwrap_or("").to_string();
        markdown(&format!("{} · {} · {server}", o.profile.name, o.database), &t, truncated)
    };
    cx.note_rows(t.values().map(|r| r.len()).sum());
    let text = Arc::new(text);
    cx.hub.snapshots.lock().insert(key, text.clone());
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris() {
        let u = uri("local pg", "dataman", false);
        assert_eq!(u, "dataman://local%20pg/dataman/schema");
        assert_eq!(parse(&u), Some(Uri { connection: "local pg".into(), database: Some("dataman".into()), json: false, refresh: false }));
        let j = parse("dataman://x/-/schema.json?refresh=true").unwrap();
        assert_eq!((j.connection.as_str(), j.database, j.json, j.refresh), ("x", None, true, true));
        assert_eq!(parse("dataman://x/db/other"), None);
        assert_eq!(parse("file:///etc/passwd"), None);
        assert_eq!(decode("a%2Fb%zz"), "a/b%zz");
    }

    #[test]
    fn markdown_is_compact() {
        let mut t: Tree = BTreeMap::new();
        t.entry("public".into()).or_default().insert(
            "orders".into(),
            Rel {
                kind: "table".into(),
                rows: Some(48_000),
                columns: vec![("id".into(), "bigint".into(), true, true), ("note".into(), "text".into(), false, false), ("customer_id".into(), "bigint".into(), false, true)],
                fks: vec!["FOREIGN KEY (customer_id) REFERENCES customers(id)".into()],
            },
        );
        let md = markdown("shop", &t, false);
        assert_eq!(
            md,
            "# shop\n\n## public\n- orders [table ~48000 rows]: id bigint PK, note text, customer_id bigint NOT NULL\n  - FOREIGN KEY (customer_id) REFERENCES customers(id)\n"
        );
    }
}
