//! What agents' rules read of a database's catalog, per engine: generated columns and what they
//! are computed from, DuckDB's macros, and the PostgreSQL searches the MCP tools run.

use std::future::Future;

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::Connection;
use crate::bounded::{Limits, Mode};
use crate::config::Engine;
use crate::privacy::Generated;

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

/// Each generated column with its table's `CREATE TABLE`, where its expression is (SQLite keeps
/// no dependencies).
const LITE_GENERATED: &str = "SELECT 'main', m.name, p.name, m.sql FROM main.sqlite_master m JOIN pragma_table_xinfo(m.name, 'main') p \
  WHERE m.type = 'table' AND p.hidden IN (2, 3)";

/// DuckDB's tables whose SQL has a generated column (its expression is read from there).
const DUCK_GENERATED: &str = "SELECT schema_name, table_name, NULL, sql FROM duckdb_tables() \
  WHERE database_name = current_database() AND NOT internal AND regexp_matches(sql, '\\bAS\\s*\\(', 'i')";

const MS_GENERATED: &str = "SELECT s.name, t.name, c.name, d.name FROM sys.computed_columns c \
  JOIN sys.objects t ON t.object_id = c.object_id JOIN sys.schemas s ON s.schema_id = t.schema_id \
  JOIN sys.sql_expression_dependencies x ON x.referencing_id = c.object_id AND x.referencing_minor_id = c.column_id \
  JOIN sys.columns d ON d.object_id = x.referenced_id AND d.column_id = x.referenced_minor_id";

/// SQL Server without VIEW DEFINITION (an agent login): sql_expression_dependencies is refused
/// and a computed column's expression reads as NULL, so it counts as computed from every other
/// column of its table (a row each), erring on more.
const MS_GENERATED_EXPR: &str = "SELECT s.name, t.name, c.name, COALESCE(c.definition, QUOTENAME(o.name)) FROM sys.computed_columns c \
  JOIN sys.objects t ON t.object_id = c.object_id JOIN sys.schemas s ON s.schema_id = t.schema_id \
  LEFT JOIN sys.columns o ON c.definition IS NULL AND o.object_id = c.object_id AND o.column_id <> c.column_id";

/// The names an expression may read: quoted (`` `a` ``, `[a]`, `"a"`) and bare words. MySQL
/// keeps no dependencies for generated columns, so its expression is read, erring on more.
fn expression_names(e: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in crate::sql::tokenize_in(e, crate::Dialect::MySql) {
        if matches!(t.kind, crate::sql::TokenKind::Ident | crate::sql::TokenKind::QuotedIdent | crate::sql::TokenKind::Keyword) {
            let w = t.text(e).trim_matches(['`', '"', '[', ']']).to_string();
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// DuckDB's macros (`CREATE MACRO`) are kept in the file: the user's functions, like any other.
const DUCK_MACROS: &str = "SELECT DISTINCT function_name FROM duckdb_functions() WHERE NOT internal AND database_name = current_database()";

/// search_objects on PostgreSQL: $1 the ILIKE pattern, $2 the kinds (a text[]), $3 the limit.
pub const PG_SEARCH: &str = "SELECT kind, schema, name, detail FROM ( \
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

/// The foreign keys that point at a PostgreSQL table: $1 its oid. Rows: name, schema.table, definition.
pub const PG_REFERENCED_BY: &str = "SELECT con.conname, n.nspname || '.' || t.relname, pg_catalog.pg_get_constraintdef(con.oid) \
  FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class t ON t.oid = con.conrelid \
  JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
  WHERE con.contype = 'f' AND con.confrelid = $1::oid ORDER BY 2, 1";

impl Connection {
    /// Generated (computed) columns of the database and the columns each is computed from, by the
    /// engine's catalog: PostgreSQL's pg_depend, SQL Server's sql_expression_dependencies, the
    /// generation expression elsewhere. Read as a bounded read on `db`.
    pub async fn generated_columns(&self, db: &str, cancel: impl Future<Output = ()> + Send) -> Result<Vec<Generated>> {
        let engine = self.engine();
        let (sql, by_expression) = match engine {
            Engine::Postgres => (PG_GENERATED, false),
            Engine::MySql => (MY_GENERATED, true),
            Engine::SqlServer => (MS_GENERATED, false),
            Engine::Sqlite => (LITE_GENERATED, true),
            Engine::DuckDb => (DUCK_GENERATED, true),
            Engine::MongoDb | Engine::Elasticsearch | Engine::OpenSearch | Engine::Redis => return Ok(Vec::new()),
        };
        let limits = Limits { max_rows: 100_000, max_bytes: 32 * 1024 * 1024, max_cell: 64 * 1024, ..Limits::default() };
        let mut by_expression = by_expression;
        let mut cancel = std::pin::pin!(cancel);
        let out = match self.bounded(Some(db), sql, vec![], Mode::Read, limits.clone(), cancel.as_mut()).await {
            // Refused the dependencies (229, no VIEW DEFINITION): the expressions, erring on more.
            Err(e) if engine == Engine::SqlServer && e.to_string().contains("[229]") => {
                by_expression = true;
                self.bounded(Some(db), MS_GENERATED_EXPR, vec![], Mode::Read, limits, cancel.as_mut()).await.map_err(|e| anyhow!("{e}"))?
            }
            r => r.map_err(|e| anyhow!("{e}"))?,
        };
        let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let mut cols: Vec<Generated> = Vec::new();
        // DuckDB: a row per table, its generated columns read from its SQL.
        let rows: Vec<Vec<Value>> = if engine == Engine::DuckDb {
            out.rows
                .iter()
                .flat_map(|row| {
                    super::lite::generated_exprs(&text(&row[3]))
                        .into_iter()
                        .map(|(c, e)| vec![row[0].clone(), row[1].clone(), Value::String(c), Value::String(e)])
                        .collect::<Vec<_>>()
                })
                .collect()
        } else {
            out.rows
        };
        for row in &rows {
            let (schema, table, column) = (text(&row[0]), text(&row[1]), text(&row[2]));
            let from = match engine {
                Engine::Sqlite => {
                    let exprs = super::lite::generated_exprs(&text(&row[3]));
                    // An expression that cannot be read counts as reading every column of the table.
                    exprs.get(&column.to_lowercase()).map(|e| expression_names(e)).unwrap_or_else(|| expression_names(&text(&row[3])))
                }
                _ if by_expression => expression_names(&text(&row[3])),
                _ => vec![text(&row[3])],
            };
            match cols.iter_mut().find(|g| g.schema == schema && g.table == table && g.column == column) {
                Some(g) => g.from.extend(from),
                None => cols.push(Generated { schema, table, column, from }),
            }
        }
        Ok(cols)
    }

    /// DuckDB's macros in `db` (none elsewhere: other engines' functions are `functions`').
    pub async fn duckdb_macros(&self, db: &str, cancel: impl Future<Output = ()> + Send) -> Result<Vec<String>> {
        if self.engine() != Engine::DuckDb {
            return Ok(Vec::new());
        }
        let limits = Limits { max_rows: 100_000, max_bytes: 8 * 1024 * 1024, ..Limits::default() };
        let r = self.bounded(Some(db), DUCK_MACROS, vec![], Mode::Read, limits, cancel).await.map_err(|e| anyhow!("{e}"))?;
        Ok(r.rows.iter().filter_map(|row| row.first()?.as_str().map(str::to_string)).collect())
    }
}
