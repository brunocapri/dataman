//! DuckDB's catalog: `duckdb_databases()`, `duckdb_tables()`, `duckdb_views()`,
//! `duckdb_columns()`, `duckdb_indexes()`, `duckdb_constraints()`, always filtered by catalog
//! (database) and schema. Internal objects stay out.

use anyhow::Result;

use super::{Column, Index, RelKind, Relation};
use crate::backend::Db;
use crate::conn::first_ok;
use crate::sql::{Dialect, TokenKind, tokenize_in};

pub(crate) fn lit(s: &str) -> String {
    Dialect::DuckDb.quote_literal(s)
}

/// The catalog: a name, or the connection's own (its meta connection has `USE`d it).
fn cat(db: &str) -> String {
    if db.is_empty() { "current_database()".into() } else { lit(db) }
}

fn get(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

/// The schemas DuckDB keeps for itself.
pub(crate) fn is_system(schema: &str) -> bool {
    matches!(schema, "information_schema" | "pg_catalog")
}

/// The primary key's columns, in order (the current catalog).
pub(crate) fn primary_key_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT unnest(constraint_column_names) FROM duckdb_constraints() \
          WHERE database_name = current_database() AND schema_name = {} AND table_name = {} AND constraint_type = 'PRIMARY KEY'",
        lit(schema),
        lit(table)
    )
}

pub(super) async fn databases(c: &Db) -> Result<Vec<String>> {
    let rows = first_ok(c, &["SELECT database_name FROM duckdb_databases() WHERE NOT internal AND database_name NOT IN ('system', 'temp') ORDER BY database_name = current_database() DESC, database_name"]).await?;
    Ok(rows.into_iter().filter_map(|r| get(&r, 0)).collect())
}

pub(super) async fn schemas(c: &Db, db: &str) -> Result<Vec<String>> {
    let q = format!("SELECT schema_name FROM duckdb_schemas() WHERE database_name = {} AND (NOT internal OR schema_name = 'main') ORDER BY schema_name <> 'main', schema_name", lit(db));
    let mut v: Vec<String> = first_ok(c, &[&q]).await?.into_iter().filter_map(|r| get(&r, 0)).collect();
    v.sort_by_key(|s| is_system(s));
    Ok(v)
}

pub(super) async fn relations(c: &Db, db: &str, schema: &str) -> Result<Vec<Relation>> {
    let (d, s) = (cat(db), lit(schema));
    let q = format!(
        "SELECT table_name, 'table' FROM duckdb_tables() WHERE database_name = {d} AND schema_name = {s} AND NOT internal \
         UNION ALL SELECT view_name, 'view' FROM duckdb_views() WHERE database_name = {d} AND schema_name = {s} AND NOT internal \
         ORDER BY 1"
    );
    Ok(first_ok(c, &[&q])
        .await?
        .into_iter()
        .filter_map(|r| {
            let kind = if get(&r, 1)? == "view" { RelKind::View } else { RelKind::Table };
            Some(Relation { schema: schema.to_string(), name: get(&r, 0)?, kind, oid: None })
        })
        .collect())
}

pub(super) async fn columns(c: &Db, db: &str, schema: &str, table: &str) -> Option<Vec<Column>> {
    let (d, s, t) = (cat(db), lit(schema), lit(table));
    let q = format!(
        "SELECT c.column_name, c.data_type, c.is_nullable, c.column_default, \
                EXISTS (SELECT 1 FROM duckdb_constraints() k WHERE k.database_name = {d} AND k.schema_name = {s} AND k.table_name = {t} \
                        AND k.constraint_type = 'PRIMARY KEY' AND list_contains(k.constraint_column_names, c.column_name)) \
           FROM duckdb_columns() c WHERE c.database_name = {d} AND c.schema_name = {s} AND c.table_name = {t} ORDER BY c.column_index"
    );
    let rows = first_ok(c, &[&q]).await.ok()?;
    Some(
        rows.into_iter()
            .filter_map(|r| {
                Some(Column {
                    name: get(&r, 0)?,
                    type_name: get(&r, 1).unwrap_or_default(),
                    nullable: get(&r, 2).map(|n| n == "true"),
                    primary_key: get(&r, 4).as_deref() == Some("true"),
                    default: get(&r, 3),
                })
            })
            .collect(),
    )
}

/// The names inside an index's `( … )`.
fn index_columns(sql: &str) -> Vec<String> {
    let toks: Vec<_> = tokenize_in(sql, Dialect::DuckDb).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let Some(open) = toks.iter().position(|t| t.text(sql) == "(") else { return Vec::new() };
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for t in &toks[open..] {
        match t.text(sql) {
            "(" => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            }
            ")" => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            "," if depth == 1 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&if t.kind == TokenKind::QuotedIdent { crate::sql::ident_value_in(sql, t, Dialect::DuckDb) } else { t.text(sql).to_string() });
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Indexes, and the primary key and UNIQUE constraints (DuckDB indexes those without listing them).
pub(super) async fn indexes(c: &Db, db: &str, schema: &str, table: &str) -> Result<Vec<Index>> {
    let (d, s, t) = (cat(db), lit(schema), lit(table));
    let mut out = Vec::new();
    let q = format!(
        "SELECT constraint_type, array_to_string(constraint_column_names, chr(31)), constraint_name FROM duckdb_constraints() \
          WHERE database_name = {d} AND schema_name = {s} AND table_name = {t} AND constraint_type IN ('PRIMARY KEY', 'UNIQUE') ORDER BY constraint_index"
    );
    for r in first_ok(c, &[&q]).await? {
        let primary = get(&r, 0).as_deref() == Some("PRIMARY KEY");
        let cols: Vec<String> = get(&r, 1).unwrap_or_default().split('\u{1f}').map(str::to_string).collect();
        let name = get(&r, 2).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("{table}_{}", if primary { "pkey" } else { "key" }));
        out.push(Index { name, columns: cols, unique: true, primary, definition: None });
    }
    let q = format!("SELECT index_name, is_unique, sql FROM duckdb_indexes() WHERE database_name = {d} AND schema_name = {s} AND table_name = {t} ORDER BY index_name");
    for r in first_ok(c, &[&q]).await? {
        let Some(name) = get(&r, 0) else { continue };
        let sql = get(&r, 2);
        out.push(Index {
            columns: sql.as_deref().map(index_columns).unwrap_or_default(),
            unique: get(&r, 1).as_deref() == Some("true"),
            primary: false,
            definition: sql.map(|s| s.trim_end_matches(';').to_string()),
            name,
        });
    }
    Ok(out)
}

pub(super) async fn columns_of_schemas(c: &Db, db: &str, list: &str) -> Result<Vec<Vec<Option<String>>>> {
    let q = format!(
        "SELECT schema_name, table_name, column_name, data_type, CASE WHEN is_nullable THEN 'YES' ELSE 'NO' END, column_index \
           FROM duckdb_columns() WHERE database_name = {} AND schema_name IN ({list}) AND NOT internal",
        cat(db)
    );
    first_ok(c, &[&q]).await
}

/// SQL as DuckDB keeps it (`duckdb_tables().sql`, `duckdb_views().sql`).
pub(crate) fn definition_sql(kind: &str, db: &str, schema: &str, name: &str) -> String {
    let (f, col) = if kind == "view" { ("duckdb_views()", "view_name") } else { ("duckdb_tables()", "table_name") };
    format!("SELECT sql FROM {f} WHERE database_name = {} AND schema_name = {} AND {col} = {}", cat(db), lit(schema), lit(name))
}

/// The indexes' own SQL, after a table's.
pub(crate) fn index_sql(db: &str, schema: &str, table: &str) -> String {
    format!("SELECT sql FROM duckdb_indexes() WHERE database_name = {} AND schema_name = {} AND table_name = {} AND sql IS NOT NULL ORDER BY index_name", cat(db), lit(schema), lit(table))
}

/// Foreign keys of a schema: table, name, column, referenced schema, table, column (one row per
/// column pair, in order).
pub(crate) fn foreign_keys_sql(db: &str, schema: &str) -> String {
    format!(
        "SELECT table_name, constraint_name, unnest(constraint_column_names), schema_name, referenced_table, unnest(referenced_column_names) \
           FROM duckdb_constraints() WHERE database_name = {} AND schema_name = {} AND constraint_type = 'FOREIGN KEY' ORDER BY table_name, constraint_index",
        cat(db),
        lit(schema)
    )
}

/// Constraints of a table: kind, name, columns, definition text.
pub(crate) fn constraints_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        "SELECT constraint_type, constraint_name, constraint_text FROM duckdb_constraints() \
          WHERE database_name = {} AND schema_name = {} AND table_name = {} AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY', 'CHECK') \
          ORDER BY constraint_index",
        cat(db),
        lit(schema),
        lit(table)
    )
}

pub(super) fn parse_constraint(table: &str, r: &[Option<String>]) -> Option<super::Constraint> {
    use super::ConstraintKind::*;
    let kind = match get(r, 0)?.as_str() {
        "PRIMARY KEY" => Primary,
        "UNIQUE" => Unique,
        "FOREIGN KEY" => Foreign,
        "CHECK" => Check,
        _ => return None,
    };
    let name = get(r, 1).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("{table}_{}", get(r, 0).unwrap_or_default().to_lowercase().replace(' ', "_")));
    Some(super::Constraint { name, kind, definition: get(r, 2) })
}

/// Unique keys (primary first) of a table, for editing: each with its columns, all NOT NULL.
pub(crate) fn keys_sql(db: &str, schema: &str, table: &str) -> String {
    let (d, s, t) = (cat(db), lit(schema), lit(table));
    format!(
        "SELECT k.constraint_type = 'PRIMARY KEY', array_to_string(k.constraint_column_names, chr(31)), \
                bool_and(NOT c.is_nullable OR k.constraint_type = 'PRIMARY KEY') \
           FROM duckdb_constraints() k JOIN duckdb_columns() c \
             ON c.database_name = k.database_name AND c.schema_name = k.schema_name AND c.table_name = k.table_name \
            AND list_contains(k.constraint_column_names, c.column_name) \
          WHERE k.database_name = {d} AND k.schema_name = {s} AND k.table_name = {t} AND k.constraint_type IN ('PRIMARY KEY', 'UNIQUE') \
          GROUP BY k.constraint_index, k.constraint_type, k.constraint_column_names ORDER BY k.constraint_type <> 'PRIMARY KEY', len(k.constraint_column_names)"
    )
}

pub(crate) async fn functions(c: &Db) -> Vec<super::FuncSig> {
    let q = "SELECT DISTINCT function_name, function_type FROM duckdb_functions() WHERE NOT internal OR function_type IN ('scalar', 'aggregate', 'table') ORDER BY 1 LIMIT 5000";
    first_ok(c, &[q])
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let name = get(&r, 0)?;
            Some(super::FuncSig { oid: 0, schema: String::new(), name, args: String::new(), result: String::new(), kind: 'f', returns_set: get(&r, 1).as_deref() == Some("table") })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_column_lists() {
        assert_eq!(index_columns("CREATE INDEX i ON t(a, \"B c\", lower(d));"), ["a", "B c", "lower ( d )"]);
        assert_eq!(index_columns("CREATE UNIQUE INDEX i ON s.t (x)"), ["x"]);
    }
}
