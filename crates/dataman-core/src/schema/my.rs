//! MySQL's catalog (`information_schema`), read into the shared schema model. A MySQL database is
//! its own (only) schema: the cache keeps the usual levels, with the schema named after the
//! database, and qualified names (`db.table`) are what MySQL writes anyway.

use anyhow::Result;

use super::{Column, Index, RelKind, Relation};
use crate::backend::Db;
use crate::conn::first_ok;
use crate::sql::Dialect;

/// A string literal for MySQL (backslashes doubled).
pub(crate) fn lit(s: &str) -> String {
    Dialect::MySql.quote_literal(s)
}

/// MySQL's own databases, listed after the user's.
pub(crate) fn is_system(db: &str) -> bool {
    matches!(db, "mysql" | "information_schema" | "performance_schema" | "sys")
}

fn get(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

/// The databases, the user's first (sorted), the system ones last.
pub(super) async fn databases(c: &Db) -> Result<Vec<String>> {
    let rows = first_ok(c, &["SELECT SCHEMA_NAME FROM information_schema.SCHEMATA", "SHOW DATABASES"]).await?;
    let mut v: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    v.sort_by(|a, b| is_system(a).cmp(&is_system(b)).then_with(|| a.cmp(b)));
    v.dedup();
    Ok(v)
}

/// The tables and views of database `schema`.
pub(super) async fn relations(c: &Db, schema: &str) -> Result<Vec<Relation>> {
    let q = format!("SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = {}", lit(schema));
    Ok(first_ok(c, &[&q])
        .await?
        .into_iter()
        .filter_map(|r| {
            let kind = match get(&r, 1)?.as_str() {
                "VIEW" | "SYSTEM VIEW" => RelKind::View,
                _ => RelKind::Table,
            };
            Some(Relation { schema: schema.to_string(), name: get(&r, 0)?, kind, oid: None })
        })
        .collect())
}

/// A default as SQL: MySQL 8 reports literals bare (`new`), expressions as written
/// (`CURRENT_TIMESTAMP`, `(uuid())`); `extra` says which (`DEFAULT_GENERATED`), and holds
/// `auto_increment` and generated columns.
fn default_of(default: Option<String>, extra: &str, generation: Option<String>) -> Option<String> {
    let extra = extra.to_ascii_lowercase();
    if extra.contains("auto_increment") {
        return Some("AUTO_INCREMENT".into());
    }
    if extra.contains("generated") && !extra.contains("default_generated") {
        return Some(match generation.filter(|g| !g.is_empty()) {
            Some(g) => format!("GENERATED ({g})"),
            None => "GENERATED".into(),
        });
    }
    let d = default?;
    let expression = extra.contains("default_generated")
        || d.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        || d.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP(")
        || d.starts_with('(');
    Some(if expression || d.parse::<f64>().is_ok() { d } else { lit(&d) })
}

const COLUMNS: &str = "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, EXTRA";

/// A table's columns in order, the primary key and defaults included.
pub(super) async fn columns(c: &Db, schema: &str, table: &str) -> Option<Vec<Column>> {
    let from = format!("FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY ORDINAL_POSITION", lit(schema), lit(table));
    // GENERATION_EXPRESSION is MySQL 5.7+ and MariaDB 10.2+.
    let q1 = format!("{COLUMNS}, GENERATION_EXPRESSION {from}");
    let q2 = format!("{COLUMNS} {from}");
    let rows = first_ok(c, &[&q1, &q2]).await.ok()?;
    Some(
        rows.into_iter()
            .filter_map(|r| {
                Some(Column {
                    name: get(&r, 0)?,
                    type_name: get(&r, 1).unwrap_or_default(),
                    nullable: get(&r, 2).map(|n| n == "YES"),
                    primary_key: get(&r, 4).as_deref() == Some("PRI"),
                    default: default_of(get(&r, 3), &get(&r, 5).unwrap_or_default(), get(&r, 6)),
                })
            })
            .collect(),
    )
}

/// A table's indexes (each column once, in key order).
pub(super) async fn indexes(c: &Db, schema: &str, table: &str) -> Result<Vec<Index>> {
    let q = format!(
        "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, INDEX_TYPE FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY INDEX_NAME, SEQ_IN_INDEX",
        lit(schema),
        lit(table)
    );
    let mut out: Vec<Index> = Vec::new();
    for r in first_ok(c, &[&q]).await? {
        let Some(name) = get(&r, 0) else { continue };
        // An expression part (MySQL 8 functional indexes) has no column name.
        let col = get(&r, 2).unwrap_or_else(|| "(expression)".into());
        match out.last_mut() {
            Some(ix) if ix.name == name => ix.columns.push(col),
            _ => {
                let unique = get(&r, 1).as_deref() == Some("0");
                let kind = get(&r, 3).unwrap_or_default();
                let q = Dialect::MySql;
                let definition = (name != "PRIMARY").then(|| {
                    let what = match kind.as_str() {
                        "FULLTEXT" => "FULLTEXT ",
                        "SPATIAL" => "SPATIAL ",
                        _ if unique => "UNIQUE ",
                        _ => "",
                    };
                    format!("CREATE {what}INDEX {} ON {}", q.quote_ident(&name), q.qualified(Some(schema), table))
                });
                out.push(Index { primary: name == "PRIMARY", unique, columns: vec![col], name, definition });
            }
        }
    }
    // The definitions name the columns too, now that they are all known.
    for ix in &mut out {
        if let Some(d) = &mut ix.definition {
            let cols: Vec<String> = ix.columns.iter().map(|c| Dialect::MySql.quote_ident(c)).collect();
            d.push_str(&format!(" ({})", cols.join(", ")));
        }
    }
    Ok(out)
}

/// Every column of the databases in `list` (quoted literals, comma-separated), in
/// `information_schema.columns`' layout: schema, table, column, type, `YES`/`NO`, position.
pub(super) async fn columns_of_schemas(c: &Db, list: &str) -> Result<Vec<Vec<Option<String>>>> {
    let q = format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, ORDINAL_POSITION \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA IN ({list})"
    );
    first_ok(c, &[&q]).await
}

/// The routines of database `db` with their parameters (`IN a int, OUT b text`), for the
/// editor: schema, name, `FUNCTION`/`PROCEDURE`, result type, parameters.
pub(crate) fn routines_sql(db: &str) -> String {
    format!(
        "SELECT r.ROUTINE_SCHEMA, r.ROUTINE_NAME, r.ROUTINE_TYPE, r.DTD_IDENTIFIER, \
           (SELECT GROUP_CONCAT(CONCAT(IF(r.ROUTINE_TYPE = 'PROCEDURE', CONCAT(p.PARAMETER_MODE, ' '), ''), p.PARAMETER_NAME, ' ', p.DTD_IDENTIFIER) \
                   ORDER BY p.ORDINAL_POSITION SEPARATOR ', ') \
              FROM information_schema.PARAMETERS p \
             WHERE p.SPECIFIC_SCHEMA = r.ROUTINE_SCHEMA AND p.SPECIFIC_NAME = r.SPECIFIC_NAME AND p.ORDINAL_POSITION > 0) \
         FROM information_schema.ROUTINES r WHERE r.ROUTINE_SCHEMA = {} ORDER BY r.ROUTINE_NAME",
        lit(db)
    )
}

pub(crate) fn parse_routine(r: &[Option<String>]) -> Option<super::FuncSig> {
    let procedure = get(r, 2)?.eq_ignore_ascii_case("PROCEDURE");
    Some(super::FuncSig {
        oid: 0,
        schema: get(r, 0)?,
        name: get(r, 1)?,
        args: get(r, 4).unwrap_or_default(),
        result: if procedure { String::new() } else { get(r, 3).unwrap_or_default() },
        kind: if procedure { 'p' } else { 'f' },
        returns_set: false,
    })
}

// ---------------------------------------------------------------------------------------------
// The explorer's other objects

/// How many routines and triggers a database has (one query).
pub(super) fn counts_sql(schema: &str) -> String {
    let s = lit(schema);
    format!(
        "SELECT (SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {s}), \
                (SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {s})"
    )
}

pub(super) fn parse_counts(r: &[Option<String>]) -> super::ObjectCounts {
    let n = |i: usize| get(r, i).and_then(|s| s.parse().ok());
    super::ObjectCounts { functions: n(0), sequences: None, types: None, triggers: n(1) }
}

/// Row estimates (`TABLE_ROWS`, InnoDB's sample) and sizes (data and indexes) of the tables.
pub(super) fn stats_sql(schema: &str) -> String {
    format!(
        "SELECT TABLE_NAME, TABLE_ROWS, DATA_LENGTH + INDEX_LENGTH FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = {} AND TABLE_TYPE = 'BASE TABLE'",
        lit(schema)
    )
}

pub(super) fn parse_stats(rows: &[Vec<Option<String>>]) -> std::collections::HashMap<String, super::RelStats> {
    rows.iter()
        .filter_map(|r| {
            Some((get(r, 0)?, super::RelStats { rows: get(r, 1).and_then(|s| s.parse().ok()), bytes: get(r, 2).and_then(|s| s.parse().ok()) }))
        })
        .collect()
}

/// The routines of a database for the explorer: name, kind, parameters (with their types and
/// modes), result type.
pub(super) fn functions_sql(schema: &str) -> String {
    routines_sql(schema)
}

/// A routine as the explorer lists it. MySQL routines have no oid: `n` (its place in the list)
/// keeps them apart.
pub(super) fn parse_function(n: usize, r: &[Option<String>]) -> Option<super::Function> {
    let sig = parse_routine(r)?;
    let params = super::objects::split_args(&sig.args);
    // `IN a int` → `int`; `b varchar(10)` → `varchar(10)`.
    let arg_types = params
        .iter()
        .filter(|p| !p.starts_with("OUT "))
        .map(|p| {
            let p = p.strip_prefix("IN ").or_else(|| p.strip_prefix("INOUT ")).unwrap_or(p);
            p.split_once(' ').map(|(_, t)| t.to_string()).unwrap_or_default()
        })
        .collect();
    Some(super::Function {
        oid: n as u32 + 1,
        name: sig.name,
        kind: if sig.kind == 'p' { super::FuncKind::Procedure } else { super::FuncKind::Function },
        identity: sig.args,
        arg_types,
        result: (sig.kind == 'f').then_some(sig.result),
        returns_set: false,
    })
}

/// `SHOW CREATE PROCEDURE|FUNCTION|TRIGGER|VIEW|TABLE db.name`.
pub(crate) fn show_create(what: &str, schema: &str, name: &str) -> String {
    format!("SHOW CREATE {what} {}", Dialect::MySql.qualified(Some(schema), name))
}

/// The definition column of `SHOW CREATE …` (its place depends on the object), with a `;`.
pub(crate) fn parse_show_create(what: &str, rows: &[Vec<Option<String>>]) -> Option<String> {
    let r = rows.first()?;
    let col = match what {
        "PROCEDURE" | "FUNCTION" | "TRIGGER" => 2,
        _ => 1,
    };
    let d = get(r, col)?.trim_end().to_string();
    Some(if d.ends_with(';') { d } else { format!("{d};") })
}

const TRIGGERS: &str = "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT, ACTION_ORIENTATION \
     FROM information_schema.TRIGGERS";

pub(super) fn triggers_sql(schema: &str) -> String {
    format!("{TRIGGERS} WHERE TRIGGER_SCHEMA = {} ORDER BY EVENT_OBJECT_TABLE, ACTION_ORDER", lit(schema))
}

pub(super) fn table_triggers_sql(schema: &str, table: &str) -> String {
    format!("{TRIGGERS} WHERE TRIGGER_SCHEMA = {} AND EVENT_OBJECT_TABLE = {} ORDER BY ACTION_ORDER", lit(schema), lit(table))
}

/// A trigger with its definition rebuilt from the catalog (what `SHOW CREATE TRIGGER` says,
/// without the DEFINER).
pub(super) fn parse_trigger(r: &[Option<String>]) -> Option<super::Trigger> {
    let (name, table) = (get(r, 0)?, get(r, 1)?);
    let (timing, event) = (get(r, 2).unwrap_or_default(), get(r, 3).unwrap_or_default());
    let q = Dialect::MySql;
    let definition = get(r, 4).map(|body| {
        format!("CREATE TRIGGER {} {timing} {event} ON {}\nFOR EACH {}\n{body}", q.quote_ident(&name), q.quote_ident(&table), get(r, 5).unwrap_or_else(|| "ROW".into()))
    });
    Some(super::Trigger { name, table, enabled: true, events: format!("{timing} {event}").to_lowercase(), definition })
}

/// A table's constraints: keys, foreign keys (with their actions), checks (MySQL 8.0.16+).
pub(super) fn constraints_sql(schema: &str, table: &str) -> [String; 2] {
    let (s, t) = (lit(schema), lit(table));
    [
        format!(
            "SELECT tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, \
                    GROUP_CONCAT(k.COLUMN_NAME ORDER BY k.ORDINAL_POSITION SEPARATOR ', '), \
                    MAX(k.REFERENCED_TABLE_SCHEMA), MAX(k.REFERENCED_TABLE_NAME), \
                    GROUP_CONCAT(k.REFERENCED_COLUMN_NAME ORDER BY k.ORDINAL_POSITION SEPARATOR ', '), \
                    MAX(rc.UPDATE_RULE), MAX(rc.DELETE_RULE) \
               FROM information_schema.TABLE_CONSTRAINTS tc \
               JOIN information_schema.KEY_COLUMN_USAGE k ON k.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                    AND k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND k.TABLE_NAME = tc.TABLE_NAME \
               LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS rc ON rc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                    AND rc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND rc.TABLE_NAME = tc.TABLE_NAME \
              WHERE tc.TABLE_SCHEMA = {s} AND tc.TABLE_NAME = {t} \
              GROUP BY tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE ORDER BY tc.CONSTRAINT_TYPE, tc.CONSTRAINT_NAME"
        ),
        format!(
            "SELECT cc.CONSTRAINT_NAME, 'CHECK', cc.CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS cc \
               JOIN information_schema.TABLE_CONSTRAINTS tc ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME \
              WHERE tc.TABLE_SCHEMA = {s} AND tc.TABLE_NAME = {t} AND tc.CONSTRAINT_TYPE = 'CHECK'"
        ),
    ]
}

/// Constraints in PostgreSQL's `pg_get_constraintdef` form (`FOREIGN KEY (a) REFERENCES t(b)
/// ON DELETE CASCADE`), which the explorer summarizes.
pub(super) fn parse_constraint(schema: &str, r: &[Option<String>]) -> Option<super::Constraint> {
    use super::ConstraintKind::*;
    let name = get(r, 0)?;
    let q = Dialect::MySql;
    let cols = || get(r, 2).unwrap_or_default().split(", ").map(|c| q.quote_ident(c)).collect::<Vec<_>>().join(", ");
    let (kind, definition) = match get(r, 1)?.as_str() {
        "PRIMARY KEY" => (Primary, format!("PRIMARY KEY ({})", cols())),
        "UNIQUE" => (Unique, format!("UNIQUE ({})", cols())),
        "FOREIGN KEY" => {
            let rs = get(r, 3).unwrap_or_default();
            let target = if rs == schema || rs.is_empty() { q.quote_ident(&get(r, 4)?) } else { q.qualified(Some(&rs), &get(r, 4)?) };
            let refs = get(r, 5).unwrap_or_default().split(", ").map(|c| q.quote_ident(c)).collect::<Vec<_>>().join(", ");
            let mut d = format!("FOREIGN KEY ({}) REFERENCES {target}({refs})", cols());
            for (i, on) in [(6, "UPDATE"), (7, "DELETE")] {
                if let Some(rule) = get(r, i).filter(|x| x != "NO ACTION" && x != "RESTRICT") {
                    d.push_str(&format!(" ON {on} {rule}"));
                }
            }
            (Foreign, d)
        }
        "CHECK" => (Check, format!("CHECK ({})", get(r, 2).unwrap_or_default())),
        _ => return None,
    };
    Some(super::Constraint { name, kind, definition: Some(definition) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        assert_eq!(default_of(Some("new".into()), "", None).as_deref(), Some("'new'"));
        assert_eq!(default_of(Some("0.00".into()), "", None).as_deref(), Some("0.00"));
        assert_eq!(default_of(Some("CURRENT_TIMESTAMP".into()), "DEFAULT_GENERATED", None).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(default_of(None, "auto_increment", None).as_deref(), Some("AUTO_INCREMENT"));
        assert_eq!(default_of(None, "STORED GENERATED", Some("(`a` + 1)".into())).as_deref(), Some("GENERATED ((`a` + 1))"));
        assert_eq!(default_of(None, "", None), None);
        assert_eq!(lit("a\\b'c"), "'a\\\\b''c'");
        assert!(is_system("sys") && !is_system("shop"));
    }
}
