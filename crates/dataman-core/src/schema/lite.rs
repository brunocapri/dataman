//! SQLite's catalog: `sqlite_master` and the table-valued PRAGMAs. One database and one schema,
//! both `main` (attached ones are left out of the explorer).

use std::collections::HashMap;

use anyhow::Result;

use super::{Column, Index, RelKind, Relation};
use crate::backend::Db;
use crate::conn::first_ok;
use crate::sql::{Dialect, TokenKind, tokenize_in};

pub(crate) fn lit(s: &str) -> String {
    Dialect::Sqlite.quote_literal(s)
}

pub(crate) fn ident(s: &str) -> String {
    Dialect::Sqlite.quote_ident(s)
}

fn get(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

/// SQLite's own tables (`sqlite_sequence`, `sqlite_stat1`…) stay out of the lists.
pub(crate) const USER_OBJECTS: &str = "name NOT LIKE 'sqlite\\_%' ESCAPE '\\'";

pub(super) async fn databases(_c: &Db) -> Result<Vec<String>> {
    Ok(vec!["main".into()])
}

pub(super) async fn relations(c: &Db, schema: &str) -> Result<Vec<Relation>> {
    let q = format!("SELECT name, type FROM {}.sqlite_master WHERE type IN ('table', 'view') AND {USER_OBJECTS} ORDER BY name", ident(schema));
    Ok(first_ok(c, &[&q])
        .await?
        .into_iter()
        .filter_map(|r| {
            let kind = if get(&r, 1)? == "view" { RelKind::View } else { RelKind::Table };
            Some(Relation { schema: schema.to_string(), name: get(&r, 0)?, kind, oid: None })
        })
        .collect())
}

/// `hidden` of `table_xinfo`: 2 and 3 are generated columns (virtual, stored).
pub(crate) fn is_generated(hidden: Option<&str>) -> bool {
    matches!(hidden, Some("2" | "3"))
}

pub(super) async fn columns(c: &Db, schema: &str, table: &str) -> Option<Vec<Column>> {
    let q = format!(
        "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo({}, {}) WHERE hidden <> 1 ORDER BY cid",
        lit(table),
        lit(schema)
    );
    let rows = first_ok(c, &[&q]).await.ok()?;
    let generated = if rows.iter().any(|r| is_generated(get(r, 5).as_deref())) { generated_columns(c, schema, table).await } else { HashMap::new() };
    Some(
        rows.into_iter()
            .filter_map(|r| {
                let name = get(&r, 0)?;
                let default = if is_generated(get(&r, 5).as_deref()) {
                    Some(match generated.get(&name.to_lowercase()) {
                        Some(e) => format!("GENERATED ({e})"),
                        None => "GENERATED".into(),
                    })
                } else {
                    get(&r, 3)
                };
                Some(Column {
                    type_name: get(&r, 1).unwrap_or_default(),
                    nullable: get(&r, 2).map(|n| n == "0"),
                    primary_key: get(&r, 4).is_some_and(|p| p != "0"),
                    default,
                    name,
                })
            })
            .collect(),
    )
}

/// The generated columns of a table and their expressions, read from its `CREATE TABLE`.
pub(crate) async fn generated_columns(c: &Db, schema: &str, table: &str) -> HashMap<String, String> {
    let q = format!("SELECT sql FROM {}.sqlite_master WHERE type = 'table' AND name = {}", ident(schema), lit(table));
    match first_ok(c, &[&q]).await {
        Ok(rows) => rows.first().and_then(|r| get(r, 0)).map(|sql| generated_exprs(&sql)).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// `name … [GENERATED ALWAYS] AS (expr)` of each column definition in a `CREATE TABLE`, by
/// lowercased name.
pub(crate) fn generated_exprs(create: &str) -> HashMap<String, String> {
    let toks: Vec<_> = tokenize_in(create, Dialect::Sqlite).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let Some(open) = toks.iter().position(|t| t.text(create) == "(") else { return HashMap::new() };
    let mut out = HashMap::new();
    let mut depth = 0;
    let mut seg_start = open + 1;
    let mut segments = Vec::new();
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t.text(create) {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    segments.push(seg_start..i);
                    break;
                }
            }
            "," if depth == 1 => {
                segments.push(seg_start..i);
                seg_start = i + 1;
            }
            _ => {}
        }
    }
    const CONSTRAINTS: &[&str] = &["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];
    for seg in segments {
        let Some(first) = toks.get(seg.start) else { continue };
        if seg.is_empty() || (first.kind == TokenKind::Keyword && CONSTRAINTS.iter().any(|k| first.text(create).eq_ignore_ascii_case(k))) {
            continue;
        }
        let name = crate::sql::ident_value_in(create, first, Dialect::Sqlite).to_lowercase();
        let Some(as_at) = (seg.start + 1..seg.end).find(|&i| toks[i].text(create).eq_ignore_ascii_case("AS") && toks.get(i + 1).is_some_and(|n| n.text(create) == "(")) else {
            continue;
        };
        let mut d = 0;
        for j in as_at + 1..seg.end {
            match toks[j].text(create) {
                "(" => d += 1,
                ")" => {
                    d -= 1;
                    if d == 0 {
                        out.insert(name, create[toks[as_at + 1].end..toks[j].start].trim().to_string());
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    out
}

pub(super) async fn indexes(c: &Db, schema: &str, table: &str) -> Result<Vec<Index>> {
    let q = format!(
        "SELECT il.name, il.\"unique\", il.origin, ii.name, m.sql \
           FROM pragma_index_list({t}, {s}) il \
           JOIN pragma_index_info(il.name, {s}) ii \
           LEFT JOIN {si}.sqlite_master m ON m.type = 'index' AND m.name = il.name \
          ORDER BY il.name, ii.seqno",
        t = lit(table),
        s = lit(schema),
        si = ident(schema),
    );
    let mut out: Vec<Index> = Vec::new();
    for r in first_ok(c, &[&q]).await? {
        let Some(name) = get(&r, 0) else { continue };
        let col = get(&r, 3).unwrap_or_else(|| "(expression)".into());
        match out.last_mut() {
            Some(ix) if ix.name == name => ix.columns.push(col),
            _ => {
                let primary = get(&r, 2).as_deref() == Some("pk");
                // Indexes SQLite makes for PRIMARY KEY and UNIQUE have no SQL of their own.
                let definition = get(&r, 4);
                out.push(Index { primary, unique: get(&r, 1).as_deref() == Some("1"), columns: vec![col], name, definition });
            }
        }
    }
    Ok(out)
}

pub(super) async fn columns_of_schemas(c: &Db, _list: &str) -> Result<Vec<Vec<Option<String>>>> {
    let q = format!(
        "SELECT 'main', m.name, p.name, p.type, CASE p.\"notnull\" WHEN 0 THEN 'YES' ELSE 'NO' END, p.cid + 1 \
           FROM main.sqlite_master m JOIN pragma_table_xinfo(m.name, 'main') p \
          WHERE m.type IN ('table', 'view') AND m.{USER_OBJECTS} AND p.hidden <> 1"
    );
    first_ok(c, &[&q]).await
}

/// SQLite's built-in functions, when `pragma_function_list` is not compiled in.
const BUILTINS: &[&str] = &[
    "abs", "avg", "changes", "char", "coalesce", "count", "date", "datetime", "format", "glob", "group_concat", "hex", "ifnull", "iif",
    "instr", "json", "json_array", "json_extract", "json_group_array", "json_group_object", "json_object", "julianday", "last_insert_rowid",
    "length", "like", "lower", "ltrim", "max", "min", "nullif", "printf", "quote", "random", "randomblob", "replace", "round", "rtrim",
    "sign", "strftime", "string_agg", "substr", "substring", "sum", "time", "total", "trim", "typeof", "unhex", "unicode", "unixepoch",
    "upper", "zeroblob",
];

pub(crate) async fn functions(c: &Db) -> Vec<super::FuncSig> {
    let rows = first_ok(c, &["SELECT DISTINCT name FROM pragma_function_list WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name"]).await;
    let names: Vec<String> = match rows {
        Ok(r) if !r.is_empty() => r.into_iter().filter_map(|r| get(&r, 0)).collect(),
        _ => BUILTINS.iter().map(|s| s.to_string()).collect(),
    };
    names
        .into_iter()
        .map(|name| super::FuncSig { oid: 0, schema: String::new(), name, args: String::new(), result: String::new(), kind: 'f', returns_set: false })
        .collect()
}

pub(super) fn triggers_sql(schema: &str, table: Option<&str>) -> String {
    let on = table.map(|t| format!(" AND tbl_name = {}", lit(t))).unwrap_or_default();
    format!("SELECT name, tbl_name, sql FROM {}.sqlite_master WHERE type = 'trigger'{on} ORDER BY name", ident(schema))
}

/// `BEFORE INSERT`, `AFTER UPDATE OF a`, `INSTEAD OF DELETE`… as the trigger lists show events.
pub(crate) fn trigger_events(create: &str) -> String {
    let words: Vec<String> = tokenize_in(create, Dialect::Sqlite)
        .into_iter()
        .filter(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident))
        .map(|t| t.text(create).to_ascii_lowercase())
        .take(16)
        .collect();
    let at = |w: &str| words.iter().position(|x| x == w);
    let when = match (at("before"), at("after"), at("instead")) {
        (Some(_), _, _) => "before",
        (_, Some(_), _) => "after",
        (_, _, Some(_)) => "instead of",
        _ => "before",
    };
    let event = ["insert", "update", "delete"].into_iter().find(|e| words.iter().any(|w| w == e)).unwrap_or("");
    format!("{when} {event}").trim_end().to_string()
}

pub(super) fn parse_trigger(r: &[Option<String>]) -> Option<super::Trigger> {
    let sql = get(r, 2);
    Some(super::Trigger {
        name: get(r, 0)?,
        table: get(r, 1).unwrap_or_default(),
        enabled: true,
        events: sql.as_deref().map(trigger_events).unwrap_or_default(),
        definition: sql.map(|s| if s.trim_end().ends_with(';') { s } else { format!("{s};") }),
    })
}

/// Primary key, unique and foreign keys of a table (CHECKs live in its `CREATE TABLE`).
pub(super) async fn constraints(c: &Db, schema: &str, table: &str) -> Vec<super::Constraint> {
    use super::{Constraint, ConstraintKind};
    let d = Dialect::Sqlite;
    let mut out = Vec::new();
    let q = format!("SELECT name FROM pragma_table_info({}, {}) WHERE pk > 0 ORDER BY pk", lit(table), lit(schema));
    let pk: Vec<String> = first_ok(c, &[&q]).await.unwrap_or_default().iter().filter_map(|r| get(r, 0)).collect();
    if !pk.is_empty() {
        let cols: Vec<String> = pk.iter().map(|c| d.quote_ident(c)).collect();
        out.push(Constraint { name: format!("{table}_pkey"), kind: ConstraintKind::Primary, definition: Some(format!("PRIMARY KEY ({})", cols.join(", "))) });
    }
    let q = format!(
        "SELECT il.name, ii.name FROM pragma_index_list({t}, {s}) il JOIN pragma_index_info(il.name, {s}) ii WHERE il.origin = 'u' ORDER BY il.name, ii.seqno",
        t = lit(table),
        s = lit(schema)
    );
    for r in first_ok(c, &[&q]).await.unwrap_or_default() {
        let (Some(name), col) = (get(&r, 0), get(&r, 1).map(|c| d.quote_ident(&c)).unwrap_or_else(|| "(expression)".into())) else { continue };
        match out.last_mut() {
            Some(k) if k.name == name && k.kind == ConstraintKind::Unique => {
                if let Some(def) = &mut k.definition {
                    def.insert_str(def.len() - 1, &format!(", {col}"));
                }
            }
            _ => out.push(Constraint { name, kind: ConstraintKind::Unique, definition: Some(format!("UNIQUE ({col})")) }),
        }
    }
    for fk in foreign_keys(c, schema, table).await {
        let cols: Vec<String> = fk.from.iter().map(|c| d.quote_ident(c)).collect();
        let to: Vec<String> = fk.to.iter().map(|c| d.quote_ident(c)).collect();
        let mut def = format!("FOREIGN KEY ({}) REFERENCES {}", cols.join(", "), d.quote_ident(&fk.table));
        if !to.is_empty() {
            def.push_str(&format!("({})", to.join(", ")));
        }
        for (what, action) in [("UPDATE", &fk.on_update), ("DELETE", &fk.on_delete)] {
            if !action.eq_ignore_ascii_case("NO ACTION") && !action.is_empty() {
                def.push_str(&format!(" ON {what} {action}"));
            }
        }
        out.push(Constraint { name: format!("{table}_fk{}", fk.id), kind: ConstraintKind::Foreign, definition: Some(def) });
    }
    out
}

pub(crate) struct ForeignKey {
    pub id: String,
    pub table: String,
    pub from: Vec<String>,
    /// Empty: the parent's primary key.
    pub to: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

pub(crate) async fn foreign_keys(c: &Db, schema: &str, table: &str) -> Vec<ForeignKey> {
    let q = format!(
        "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list({}, {}) ORDER BY id, seq",
        lit(table),
        lit(schema)
    );
    let mut out: Vec<ForeignKey> = Vec::new();
    for r in first_ok(c, &[&q]).await.unwrap_or_default() {
        let id = get(&r, 0).unwrap_or_default();
        if out.last().is_none_or(|f| f.id != id) {
            out.push(ForeignKey {
                id: id.clone(),
                table: get(&r, 1).unwrap_or_default(),
                from: Vec::new(),
                to: Vec::new(),
                on_update: get(&r, 4).unwrap_or_default(),
                on_delete: get(&r, 5).unwrap_or_default(),
            });
        }
        let fk = out.last_mut().expect("pushed above");
        fk.from.extend(get(&r, 2));
        fk.to.extend(get(&r, 3));
    }
    out
}

/// The tables a view reads, by the names its SQL uses (SQLite keeps no dependency list).
pub(crate) fn view_tables(view_sql: &str) -> Vec<String> {
    let toks: Vec<_> = tokenize_in(view_sql, Dialect::Sqlite).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let a = crate::sql::scope::analyze_in(view_sql, &toks, Dialect::Sqlite);
    let mut out: Vec<String> = Vec::new();
    for s in a.scopes.iter().flat_map(|s| s.sources.iter()) {
        if let crate::sql::scope::SourceKind::Table { name, .. } = &s.kind
            && !out.iter().any(|o| o.eq_ignore_ascii_case(name))
        {
            out.push(name.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_expressions() {
        let sql = "CREATE TABLE \"t\" (id INTEGER PRIMARY KEY, a TEXT, \"Full Name\" TEXT GENERATED ALWAYS AS (a || ' ' || upper(a)) STORED, \
                   b INT AS (length(a)) VIRTUAL, c TEXT DEFAULT ('x'), CONSTRAINT k CHECK (a <> ''), UNIQUE (a))";
        let g = generated_exprs(sql);
        assert_eq!(g.get("full name").map(String::as_str), Some("a || ' ' || upper(a)"));
        assert_eq!(g.get("b").map(String::as_str), Some("length(a)"));
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn triggers_and_views() {
        assert_eq!(trigger_events("CREATE TRIGGER t AFTER UPDATE OF a ON x BEGIN SELECT 1; END"), "after update");
        assert_eq!(trigger_events("CREATE TEMP TRIGGER IF NOT EXISTS t INSTEAD OF DELETE ON v BEGIN SELECT 1; END"), "instead of delete");
        assert_eq!(trigger_events("CREATE TRIGGER t INSERT ON x BEGIN SELECT 1; END"), "before insert");
        let v = view_tables("CREATE VIEW v AS WITH c AS (SELECT * FROM orders) SELECT * FROM c JOIN \"Customers\" k ON k.id = c.customer WHERE c.id IN (SELECT id FROM items)");
        // Names as the analyzer folds them; the caller matches them to relations whatever the case.
        for t in ["orders", "customers", "items"] {
            assert!(v.iter().any(|x| x.eq_ignore_ascii_case(t)), "{t} in {v:?}");
        }
    }
}
