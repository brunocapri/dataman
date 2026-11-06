//! ClickHouse's catalog: `system.databases`, `system.tables`, `system.columns`,
//! `system.data_skipping_indices`. A database is its only namespace, so schema and database are
//! the same name.

use anyhow::Result;

use super::{Column, FuncSig, Index, RelKind, RelStats, Relation};
use crate::backend::Db;
use crate::conn::first_ok;
use crate::sql::{Dialect, Token, TokenKind, tokenize_in};

pub(crate) fn lit(s: &str) -> String {
    Dialect::ClickHouse.quote_literal(s)
}

fn get(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

pub(super) async fn databases(c: &Db) -> Result<Vec<String>> {
    let rows = first_ok(c, &["SELECT name FROM system.databases ORDER BY name"]).await?;
    let mut v: Vec<String> = rows.into_iter().filter_map(|r| get(&r, 0)).collect();
    v.sort_by_key(|d| crate::Engine::ClickHouse.is_system_db(d));
    Ok(v)
}

fn rel_kind(engine: &str) -> RelKind {
    match engine {
        "View" | "LiveView" | "WindowView" => RelKind::View,
        "MaterializedView" => RelKind::MaterializedView,
        _ => RelKind::Table,
    }
}

pub(super) async fn relations(c: &Db, schema: &str) -> Result<Vec<Relation>> {
    let q = format!("SELECT name, engine FROM system.tables WHERE database = {} AND NOT is_temporary ORDER BY name", lit(schema));
    Ok(first_ok(c, &[&q])
        .await?
        .into_iter()
        .filter_map(|r| Some(Relation { schema: schema.to_string(), name: get(&r, 0)?, kind: rel_kind(&get(&r, 1)?), oid: None }))
        .collect())
}

fn default_of(kind: &str, expr: Option<String>) -> Option<String> {
    let e = expr.filter(|e| !e.is_empty())?;
    Some(if kind == "DEFAULT" || kind.is_empty() { e } else { format!("{kind} {e}") })
}

pub(super) async fn columns(c: &Db, schema: &str, table: &str) -> Option<Vec<Column>> {
    let q = format!(
        "SELECT name, type, default_kind, default_expression, is_in_primary_key FROM system.columns \
          WHERE database = {} AND table = {} ORDER BY position",
        lit(schema),
        lit(table)
    );
    let rows = first_ok(c, &[&q]).await.ok()?;
    Some(
        rows.into_iter()
            .filter_map(|r| {
                let type_name = get(&r, 1).unwrap_or_default();
                Some(Column {
                    name: get(&r, 0)?,
                    nullable: Some(type_name.starts_with("Nullable(") || type_name.starts_with("LowCardinality(Nullable(")),
                    type_name,
                    primary_key: get(&r, 4).as_deref() == Some("1"),
                    default: default_of(&get(&r, 2).unwrap_or_default(), get(&r, 3)),
                })
            })
            .collect(),
    )
}

/// A key as the server writes it, cut at its own commas: not those inside a call, a string or a
/// quoted name (`toYYYYMM(c), concat(b, 'x, y'), \`p, q\``). A quoted name alone is that name.
fn key_parts(key: &str) -> Vec<String> {
    let d = Dialect::ClickHouse;
    let toks: Vec<Token> = tokenize_in(key, d);
    let mut out = Vec::new();
    let (mut depth, mut from) = (0usize, 0);
    let mut part = |from: usize, to: usize| {
        let text = key[from..to].trim();
        let inner: Vec<&Token> = toks.iter().filter(|t| t.start >= from && t.end <= to && !t.kind.is_trivia()).collect();
        out.push(match inner.as_slice() {
            [one] if one.kind == TokenKind::QuotedIdent => crate::sql::ident_value_in(key, one, d),
            _ => text.to_string(),
        });
    };
    for t in toks.iter().filter(|t| t.kind == TokenKind::Punct) {
        match t.text(key) {
            "(" | "[" => depth += 1,
            ")" | "]" => depth = depth.saturating_sub(1),
            "," if depth == 0 => {
                part(from, t.start);
                from = t.end;
            }
            _ => {}
        }
    }
    part(from, key.len());
    out
}

/// The primary key (never unique here: it sorts the data) and the data skipping indexes.
pub(super) async fn indexes(c: &Db, schema: &str, table: &str) -> Result<Vec<Index>> {
    let (s, t) = (lit(schema), lit(table));
    let mut out = Vec::new();
    let q = format!("SELECT primary_key FROM system.tables WHERE database = {s} AND name = {t}");
    if let Some(key) = first_ok(c, &[&q]).await?.first().and_then(|r| get(r, 0)).filter(|k| !k.is_empty()) {
        out.push(Index { name: "PRIMARY KEY".into(), columns: key_parts(&key), unique: false, primary: true, definition: None });
    }
    let q = format!("SELECT name, expr, type_full, granularity FROM system.data_skipping_indices WHERE database = {s} AND table = {t} ORDER BY name");
    for r in first_ok(c, &[&q]).await? {
        let Some(name) = get(&r, 0) else { continue };
        let expr = get(&r, 1).unwrap_or_default();
        let definition = format!("INDEX {} {expr} TYPE {} GRANULARITY {}", Dialect::ClickHouse.quote_ident(&name), get(&r, 2).unwrap_or_default(), get(&r, 3).unwrap_or_default());
        out.push(Index { name, columns: vec![expr], unique: false, primary: false, definition: Some(definition) });
    }
    Ok(out)
}

pub(super) async fn columns_of_schemas(c: &Db, list: &str) -> Result<Vec<Vec<Option<String>>>> {
    let q = format!(
        "SELECT database, table, name, type, if(startsWith(type, 'Nullable(') OR startsWith(type, 'LowCardinality(Nullable('), 'YES', 'NO'), position \
           FROM system.columns WHERE database IN ({list})"
    );
    first_ok(c, &[&q]).await
}

pub(super) fn stats_sql(schema: &str) -> String {
    format!("SELECT name, total_rows, total_bytes FROM system.tables WHERE database = {}", lit(schema))
}

pub(super) fn parse_stats(rows: &[Vec<Option<String>>]) -> std::collections::HashMap<String, RelStats> {
    rows.iter().filter_map(|r| Some((get(r, 0)?, RelStats { rows: get(r, 1).and_then(|s| s.parse().ok()), bytes: get(r, 2).and_then(|s| s.parse().ok()) }))).collect()
}

pub(crate) fn primary_key_sql(schema: &str, table: &str) -> String {
    format!("SELECT name FROM system.columns WHERE database = {} AND table = {} AND is_in_primary_key ORDER BY position", lit(schema), lit(table))
}

/// The statement as the server rebuilds it, for tables and views alike.
pub(crate) fn show_create_sql(schema: &str, name: &str) -> String {
    format!("SHOW CREATE TABLE {}", Dialect::ClickHouse.qualified(Some(schema), name))
}

pub(crate) async fn functions(c: &Db) -> Vec<FuncSig> {
    let q = "SELECT name, is_aggregate FROM system.functions WHERE alias_to = '' ORDER BY name LIMIT 5000";
    match first_ok(c, &[q]).await {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| {
                let name = get(&r, 0)?;
                Some(FuncSig { oid: 0, schema: String::new(), name, args: String::new(), result: String::new(), kind: if get(&r, 1).as_deref() == Some("1") { 'a' } else { 'f' }, returns_set: false })
            })
            .collect(),
        Err(e) => {
            log::debug!("functions for autocomplete unavailable: {e:#}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_say_their_kind() {
        assert_eq!(default_of("DEFAULT", Some("now()".into())).as_deref(), Some("now()"));
        assert_eq!(default_of("MATERIALIZED", Some("a + 1".into())).as_deref(), Some("MATERIALIZED a + 1"));
        assert_eq!(default_of("", Some(String::new())), None);
        assert_eq!(lit("a\\'b"), "'a\\\\''b'");
        assert_eq!(rel_kind("MaterializedView"), RelKind::MaterializedView);
        assert_eq!(rel_kind("ReplacingMergeTree"), RelKind::Table);
        assert_eq!(key_parts("id, day"), ["id", "day"]);
        assert_eq!(key_parts(r"toYYYYMM(c), intHash32(a), concat(b, \'x, y\')".replace('\\', "").as_str()), ["toYYYYMM(c)", "intHash32(a)", "concat(b, 'x, y')"]);
        assert_eq!(key_parts("`p, q`"), ["p, q"]);
    }
}
