//! SQL Server's catalog (`sys.*`), read into the shared schema model. A connection's catalog
//! views see the database it is connected to: the cache keeps one metadata connection per
//! database, as for PostgreSQL.
//!
//! The builders return SQL and the parsers take its text rows (`backend::text_rows`): bits come
//! as `1`/`0`, numbers as decimals.

use std::collections::HashMap;

use anyhow::Result;

use super::{Column, Index, RelKind, Relation};
use crate::backend::Db;
use crate::conn::first_ok;
use crate::sql::Dialect;

const Q: Dialect = Dialect::TSql;

/// A Unicode string literal: `N'…'` with quotes doubled.
pub(crate) fn lit(s: &str) -> String {
    Q.quote_literal(s)
}

/// `[name]`, always bracketed (for `OBJECT_ID`'s argument).
fn br(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// `OBJECT_ID(N'[schema].[name]')`.
pub(crate) fn object_id(schema: &str, name: &str) -> String {
    format!("OBJECT_ID({})", lit(&format!("{}.{}", br(schema), br(name))))
}

fn get(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

fn yes(r: &[Option<String>], i: usize) -> bool {
    matches!(get(r, i).as_deref(), Some("1" | "true"))
}

/// The system databases, listed after the user's.
pub(crate) fn is_system_db(db: &str) -> bool {
    matches!(db, "master" | "model" | "msdb" | "tempdb")
}

/// The databases this login can open, the user's first (sorted), the system ones last.
pub(super) async fn databases(c: &Db) -> Result<Vec<String>> {
    let rows = first_ok(
        c,
        &[
            "SELECT name FROM sys.databases WHERE state = 0 AND HAS_DBACCESS(name) = 1",
            "SELECT name FROM sys.databases",
        ],
    )
    .await?;
    let mut v: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    v.sort_by(|a, b| is_system_db(a).cmp(&is_system_db(b)).then_with(|| a.to_lowercase().cmp(&b.to_lowercase())));
    v.dedup();
    Ok(v)
}

/// The schemas of the connection's database: the user's (sorted, the fixed roles' own schemas
/// and `guest` left out), then `INFORMATION_SCHEMA` and `sys`.
pub(super) async fn schemas(c: &Db) -> Result<Vec<String>> {
    let rows = first_ok(c, &["SELECT name FROM sys.schemas WHERE schema_id < 16384 AND name <> N'guest'"]).await?;
    let mut v: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    let system = |s: &str| matches!(s, "sys" | "INFORMATION_SCHEMA");
    v.sort_by(|a, b| system(a).cmp(&system(b)).then_with(|| a.to_lowercase().cmp(&b.to_lowercase())));
    Ok(v)
}

/// The tables and views of a schema (`sys` and `INFORMATION_SCHEMA` hold system views).
pub(super) async fn relations(c: &Db, schema: &str) -> Result<Vec<Relation>> {
    let q = format!(
        "SELECT o.name, o.type FROM sys.all_objects o WHERE o.schema_id = SCHEMA_ID({}) AND o.type IN ('U', 'V')",
        lit(schema)
    );
    Ok(first_ok(c, &[&q])
        .await?
        .into_iter()
        .filter_map(|r| {
            let kind = match get(&r, 1)?.trim() {
                "V" => RelKind::View,
                _ => RelKind::Table,
            };
            Some(Relation { schema: schema.to_string(), name: get(&r, 0)?, kind, oid: None })
        })
        .collect())
}

/// A column's type as SQL Server writes it: `nvarchar(50)`, `varbinary(max)`, `decimal(10,2)`,
/// `datetime2(3)`; `t` is `sys.types`, `c` `sys.columns` (or `sys.parameters`).
pub(crate) const TYPE_SQL: &str = "CASE \
    WHEN t.is_user_defined = 1 THEN t.name \
    WHEN t.name = 'timestamp' THEN 'rowversion' \
    WHEN t.name IN ('varchar', 'char', 'varbinary', 'binary') THEN t.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS varchar(10)) END + ')' \
    WHEN t.name IN ('nvarchar', 'nchar') THEN t.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')' \
    WHEN t.name IN ('decimal', 'numeric') THEN t.name + '(' + CAST(c.precision AS varchar(3)) + ',' + CAST(c.scale AS varchar(3)) + ')' \
    WHEN t.name IN ('datetime2', 'time', 'datetimeoffset') AND c.scale <> 7 THEN t.name + '(' + CAST(c.scale AS varchar(3)) + ')' \
    ELSE t.name END";

/// `((0))` → `0`, `(getdate())` → `getdate()`: the parentheses SQL Server keeps around a
/// default's expression.
pub(crate) fn unwrap_parens(s: &str) -> &str {
    let mut s = s.trim();
    while s.starts_with('(') && s.ends_with(')') && super::objects::balanced(&s[1..s.len() - 1]) {
        s = s[1..s.len() - 1].trim();
    }
    s
}

/// What the explorer shows as a column's default: `IDENTITY(1,1)`, `AS (a * b) PERSISTED`, the
/// default constraint's expression.
fn default_of(identity: Option<String>, computed: Option<String>, persisted: bool, default: Option<String>) -> Option<String> {
    if let Some(i) = identity {
        return Some(format!("IDENTITY{i}"));
    }
    if let Some(c) = computed {
        return Some(format!("AS {c}{}", if persisted { " PERSISTED" } else { "" }));
    }
    default.map(|d| unwrap_parens(&d).to_string())
}

/// A table's (or view's) columns in order, the primary key, identity, computed columns and
/// defaults included.
pub(crate) async fn columns(c: &Db, schema: &str, table: &str) -> Option<Vec<Column>> {
    let q = format!(
        "SELECT c.name, {TYPE_SQL}, c.is_nullable, \
                CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                                  WHERE i.object_id = c.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id) THEN 1 ELSE 0 END, \
                '(' + CAST(ic.seed_value AS varchar(40)) + ',' + CAST(ic.increment_value AS varchar(40)) + ')', \
                cc.definition, cc.is_persisted, dc.definition \
           FROM sys.all_columns c \
           JOIN sys.types t ON t.user_type_id = c.user_type_id \
           LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
           LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
           LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
          WHERE c.object_id = {} ORDER BY c.column_id",
        object_id(schema, table)
    );
    let rows = first_ok(c, &[&q]).await.ok()?;
    Some(
        rows.into_iter()
            .filter_map(|r| {
                Some(Column {
                    name: get(&r, 0)?,
                    type_name: get(&r, 1).unwrap_or_default(),
                    nullable: get(&r, 2).map(|n| n == "1"),
                    primary_key: yes(&r, 3),
                    default: default_of(get(&r, 4), get(&r, 5), yes(&r, 6), get(&r, 7)),
                })
            })
            .collect(),
    )
}

/// A table's indexes: key columns in order (`DESC` marked), then the included ones; heaps
/// (no index) left out. The ones behind a primary key or a unique constraint have no
/// `CREATE INDEX` (they come with their constraint).
pub(crate) async fn indexes(c: &Db, schema: &str, table: &str) -> Result<Vec<Index>> {
    let q = format!(
        "SELECT i.name, i.is_primary_key, i.is_unique, i.type_desc, col.name, ic.is_descending_key, ic.is_included_column, i.filter_definition, i.is_unique_constraint \
           FROM sys.indexes i \
           JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
           JOIN sys.all_columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
          WHERE i.object_id = {} AND i.type > 0 AND i.is_hypothetical = 0 \
          ORDER BY i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id",
        object_id(schema, table)
    );
    let mut out: Vec<Index> = Vec::new();
    let mut parts: HashMap<String, (Vec<String>, Vec<String>, String, Option<String>)> = HashMap::new();
    let mut constraint: Vec<String> = Vec::new();
    for r in first_ok(c, &[&q]).await? {
        let (Some(name), Some(col)) = (get(&r, 0), get(&r, 4)) else { continue };
        let included = yes(&r, 6);
        if !out.iter().any(|ix| ix.name == name) {
            let primary = yes(&r, 1);
            out.push(Index { name: name.clone(), columns: Vec::new(), unique: yes(&r, 2), primary, definition: None });
            parts.insert(name.clone(), (Vec::new(), Vec::new(), get(&r, 3).unwrap_or_default(), get(&r, 7)));
            if yes(&r, 8) {
                constraint.push(name.clone());
            }
        }
        let ix = out.iter_mut().find(|ix| ix.name == name).expect("pushed");
        let p = parts.get_mut(&name).expect("inserted");
        if included {
            p.1.push(Q.quote_ident(&col));
        } else {
            ix.columns.push(col.clone());
            p.0.push(format!("{}{}", Q.quote_ident(&col), if yes(&r, 5) { " DESC" } else { "" }));
        }
    }
    // A primary key or a unique constraint is its constraint (in the table's DDL).
    for ix in &mut out {
        if ix.primary || constraint.contains(&ix.name) {
            continue;
        }
        let (keys, include, kind, filter) = &parts[&ix.name];
        let mut d = format!(
            "CREATE {}{} INDEX {} ON {} ({})",
            if ix.unique { "UNIQUE " } else { "" },
            kind.to_uppercase().replace('_', " "),
            Q.quote_ident(&ix.name),
            Q.qualified(Some(schema), table),
            keys.join(", ")
        );
        if !include.is_empty() {
            d.push_str(&format!(" INCLUDE ({})", include.join(", ")));
        }
        if let Some(f) = filter {
            d.push_str(&format!(" WHERE {f}"));
        }
        ix.definition = Some(d);
    }
    Ok(out)
}

/// Every column of the schemas in `list` (quoted literals, comma-separated), in
/// `information_schema.columns`' layout: schema, table, column, type, `YES`/`NO`, position.
pub(super) async fn columns_of_schemas(c: &Db, list: &str) -> Result<Vec<Vec<Option<String>>>> {
    let q = format!(
        "SELECT s.name, o.name, c.name, {TYPE_SQL}, CASE c.is_nullable WHEN 1 THEN 'YES' ELSE 'NO' END, c.column_id \
           FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id \
           JOIN sys.objects o ON o.object_id = c.object_id JOIN sys.schemas s ON s.schema_id = o.schema_id \
          WHERE o.type IN ('U', 'V') AND s.name IN ({list})"
    );
    first_ok(c, &[&q]).await
}

// ---------------------------------------------------------------------------------------------
// Routines

/// The procedures and functions of the database (or of `schema`), one row per parameter:
/// schema, name, type code, parameter name, parameter type, output, is the return value
/// (`parameter_id` 0).
pub(crate) fn routines_sql(schema: Option<&str>) -> String {
    let filter = schema.map(|s| format!(" AND o.schema_id = SCHEMA_ID({})", lit(s))).unwrap_or_default();
    format!(
        "SELECT SCHEMA_NAME(o.schema_id), o.name, o.type, c.name, {TYPE_SQL}, c.is_output, CASE WHEN c.parameter_id = 0 THEN 1 ELSE 0 END \
           FROM sys.objects o \
           LEFT JOIN sys.parameters c ON c.object_id = o.object_id \
           LEFT JOIN sys.types t ON t.user_type_id = c.user_type_id \
          WHERE o.type IN ('P', 'PC', 'FN', 'FS', 'IF', 'TF', 'FT') AND o.is_ms_shipped = 0{filter} \
          ORDER BY o.name, SCHEMA_NAME(o.schema_id), c.parameter_id"
    )
}

/// The routines of [`routines_sql`]' rows, in order: their signatures.
pub(crate) fn parse_routines(rows: &[Vec<Option<String>>]) -> Vec<super::FuncSig> {
    let mut out: Vec<super::FuncSig> = Vec::new();
    for r in rows {
        let (Some(schema), Some(name), Some(ty)) = (get(r, 0), get(r, 1), get(r, 2)) else { continue };
        let ty = ty.trim().to_string();
        let procedure = matches!(ty.as_str(), "P" | "PC");
        let table = matches!(ty.as_str(), "IF" | "TF" | "FT");
        if out.last().is_none_or(|f| f.schema != schema || f.name != name) {
            out.push(super::FuncSig {
                oid: 0,
                schema,
                name,
                args: String::new(),
                result: if table { "TABLE".into() } else { String::new() },
                kind: if procedure { 'p' } else { 'f' },
                returns_set: table,
            });
        }
        let f = out.last_mut().expect("pushed");
        let Some(pname) = get(r, 3) else { continue };
        let pty = get(r, 4).unwrap_or_default();
        if yes(r, 6) {
            f.result = pty;
            continue;
        }
        if !f.args.is_empty() {
            f.args.push_str(", ");
        }
        f.args.push_str(&format!("{pname} {pty}{}", if yes(r, 5) { " OUTPUT" } else { "" }));
    }
    out
}

/// A routine as the explorer lists it. SQL Server routines' object ids are not PostgreSQL oids:
/// `n` (its place in the list) keeps them apart.
pub(super) fn function_of(n: usize, sig: super::FuncSig) -> super::Function {
    let params = super::objects::split_args(&sig.args);
    let arg_types = params
        .iter()
        .filter(|p| !p.ends_with(" OUTPUT"))
        .map(|p| p.split_once(' ').map(|(_, t)| t.to_string()).unwrap_or_default())
        .collect();
    super::Function {
        oid: n as u32 + 1,
        name: sig.name,
        kind: if sig.kind == 'p' { super::FuncKind::Procedure } else { super::FuncKind::Function },
        identity: sig.args,
        arg_types,
        result: (sig.kind == 'f').then_some(sig.result),
        returns_set: sig.returns_set,
    }
}

/// A module's definition (procedure, function, view, trigger): the text it was created with.
pub(crate) fn definition_sql(schema: &str, name: &str) -> String {
    format!("SELECT OBJECT_DEFINITION({})", object_id(schema, name))
}

// ---------------------------------------------------------------------------------------------
// The explorer's other objects

/// How many routines, sequences, user types and triggers a schema has (one query).
pub(super) fn counts_sql(schema: &str) -> String {
    let s = format!("SCHEMA_ID({})", lit(schema));
    format!(
        "SELECT (SELECT COUNT(*) FROM sys.objects WHERE schema_id = {s} AND type IN ('P', 'PC', 'FN', 'FS', 'IF', 'TF', 'FT') AND is_ms_shipped = 0), \
                (SELECT COUNT(*) FROM sys.sequences WHERE schema_id = {s}), \
                (SELECT COUNT(*) FROM sys.types WHERE schema_id = {s} AND is_user_defined = 1), \
                (SELECT COUNT(*) FROM sys.triggers tr JOIN sys.objects o ON o.object_id = tr.parent_id WHERE o.schema_id = {s})"
    )
}

pub(super) fn parse_counts(r: &[Option<String>]) -> super::ObjectCounts {
    let n = |i: usize| get(r, i).and_then(|s| s.parse().ok());
    super::ObjectCounts { functions: n(0), sequences: n(1), types: n(2), triggers: n(3) }
}

/// Row counts and reserved sizes of a schema's tables (`dm_db_partition_stats`, which needs
/// VIEW DATABASE STATE), else the row counts of `sys.partitions`.
pub(super) fn stats_sql(schema: &str) -> [String; 2] {
    let s = format!("SCHEMA_ID({})", lit(schema));
    [
        format!(
            "SELECT o.name, SUM(CASE WHEN ps.index_id IN (0, 1) THEN ps.row_count ELSE 0 END), SUM(ps.reserved_page_count) * 8192 \
               FROM sys.dm_db_partition_stats ps JOIN sys.objects o ON o.object_id = ps.object_id \
              WHERE o.schema_id = {s} AND o.type = 'U' GROUP BY o.name"
        ),
        format!(
            "SELECT o.name, SUM(p.rows), NULL FROM sys.partitions p JOIN sys.objects o ON o.object_id = p.object_id \
              WHERE o.schema_id = {s} AND o.type = 'U' AND p.index_id IN (0, 1) GROUP BY o.name"
        ),
    ]
}

pub(super) fn parse_stats(rows: &[Vec<Option<String>>]) -> HashMap<String, super::RelStats> {
    rows.iter()
        .filter_map(|r| {
            Some((get(r, 0)?, super::RelStats { rows: get(r, 1).and_then(|s| s.parse().ok()), bytes: get(r, 2).and_then(|s| s.parse().ok()) }))
        })
        .collect()
}

pub(super) fn sequences_sql(schema: &str) -> String {
    format!("SELECT s.name, TYPE_NAME(s.user_type_id) FROM sys.sequences s WHERE s.schema_id = SCHEMA_ID({}) ORDER BY s.name", lit(schema))
}

pub(super) fn parse_sequence(r: &[Option<String>]) -> Option<super::Sequence> {
    Some(super::Sequence { name: get(r, 0)?, type_name: get(r, 1), owned_by: None })
}

/// A sequence's current value (`is_called`: it was ever used).
pub(super) fn sequence_value_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT CAST(current_value AS nvarchar(40)), CASE WHEN last_used_value IS NULL THEN 0 ELSE 1 END FROM sys.sequences WHERE object_id = {}",
        object_id(schema, name)
    )
}

/// Alias types (a base type, like a domain) and table types (columns, like a composite).
pub(super) fn types_sql(schema: &str) -> String {
    format!(
        "SELECT c.name, c.is_table_type, CASE WHEN c.is_table_type = 1 THEN NULL ELSE {} END \
           FROM sys.types c LEFT JOIN sys.types t ON t.user_type_id = c.system_type_id \
          WHERE c.is_user_defined = 1 AND c.schema_id = SCHEMA_ID({}) ORDER BY c.name",
        TYPE_SQL.replace("t.is_user_defined = 1", "1 = 0"),
        lit(schema)
    )
}

pub(super) fn parse_type(r: &[Option<String>]) -> Option<super::UserType> {
    let table = yes(r, 1);
    Some(super::UserType {
        name: get(r, 0)?,
        kind: if table { super::TypeKind::Composite } else { super::TypeKind::Domain },
        base: get(r, 2),
        labels: Vec::new(),
    })
}

/// An alias type's `CREATE TYPE … FROM base [NOT NULL]`, or a table type's columns.
pub(super) fn type_definition_sql(schema: &str, name: &str) -> String {
    let id = format!("TYPE_ID({})", lit(&format!("{}.{}", br(schema), br(name))));
    format!(
        "SELECT c.is_table_type, c.is_nullable, {base}, col.name, {coltype}, col.is_nullable \
           FROM sys.types c LEFT JOIN sys.types t ON t.user_type_id = c.system_type_id \
           LEFT JOIN sys.table_types tt ON tt.user_type_id = c.user_type_id \
           LEFT JOIN sys.columns col ON col.object_id = tt.type_table_object_id \
           LEFT JOIN sys.types ct ON ct.user_type_id = col.user_type_id \
          WHERE c.user_type_id = {id} ORDER BY col.column_id",
        base = TYPE_SQL.replace("t.is_user_defined = 1", "1 = 0"),
        coltype = TYPE_SQL.replace("t.", "ct.").replace("c.", "col.")
    )
}

pub(super) fn parse_type_definition(schema: &str, name: &str, rows: &[Vec<Option<String>>]) -> Option<String> {
    let first = rows.first()?;
    let full = Q.qualified(Some(schema), name);
    if !yes(first, 0) {
        let base = get(first, 2)?;
        return Some(format!("CREATE TYPE {full} FROM {base}{};", if yes(first, 1) { "" } else { " NOT NULL" }));
    }
    let cols: Vec<String> = rows
        .iter()
        .filter_map(|r| Some(format!("    {} {}{}", Q.quote_ident(&get(r, 3)?), get(r, 4).unwrap_or_default(), if yes(r, 5) { "" } else { " NOT NULL" })))
        .collect();
    Some(format!("CREATE TYPE {full} AS TABLE (\n{}\n);", cols.join(",\n")))
}

const TRIGGERS: &str = "SELECT tr.name, o.name, tr.is_disabled, tr.is_instead_of_trigger, \
       OBJECTPROPERTY(tr.object_id, 'ExecIsInsertTrigger'), OBJECTPROPERTY(tr.object_id, 'ExecIsUpdateTrigger'), \
       OBJECTPROPERTY(tr.object_id, 'ExecIsDeleteTrigger'), OBJECT_DEFINITION(tr.object_id) \
  FROM sys.triggers tr JOIN sys.objects o ON o.object_id = tr.parent_id";

pub(super) fn triggers_sql(schema: &str) -> String {
    format!("{TRIGGERS} WHERE o.schema_id = SCHEMA_ID({}) ORDER BY o.name, tr.name", lit(schema))
}

pub(super) fn table_triggers_sql(schema: &str, table: &str) -> String {
    format!("{TRIGGERS} WHERE tr.parent_id = {} ORDER BY tr.name", object_id(schema, table))
}

pub(super) fn parse_trigger(r: &[Option<String>]) -> Option<super::Trigger> {
    let events: Vec<&str> = [(4, "insert"), (5, "update"), (6, "delete")].into_iter().filter(|(i, _)| yes(r, *i)).map(|(_, e)| e).collect();
    let timing = if yes(r, 3) { "instead of" } else { "after" };
    Some(super::Trigger {
        name: get(r, 0)?,
        table: get(r, 1)?,
        enabled: !yes(r, 2),
        events: format!("{timing} {}", events.join("/")),
        definition: get(r, 7).map(|d| d.trim().to_string()),
    })
}

/// A table's keys, foreign keys (with their actions) and checks, one row per column: name,
/// kind (`PK`, `UQ`, `F`, `C`), column (a check's definition), referenced schema, table and
/// column, update and delete actions.
pub(crate) fn constraints_sql(schema: &str, table: &str) -> String {
    let id = object_id(schema, table);
    format!(
        "SELECT kc.name, kc.type, c.name, NULL, NULL, NULL, NULL, NULL, ic.key_ordinal \
           FROM sys.key_constraints kc \
           JOIN sys.index_columns ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id \
           JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
          WHERE kc.parent_object_id = {id} AND ic.is_included_column = 0 \
         UNION ALL \
         SELECT fk.name, 'F', pc.name, SCHEMA_NAME(rt.schema_id), rt.name, rc.name, fk.update_referential_action_desc, fk.delete_referential_action_desc, fkc.constraint_column_id \
           FROM sys.foreign_keys fk \
           JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
           JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
           JOIN sys.objects rt ON rt.object_id = fkc.referenced_object_id \
           JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
          WHERE fk.parent_object_id = {id} \
         UNION ALL \
         SELECT cc.name, 'C', cc.definition, NULL, NULL, NULL, NULL, NULL, 0 FROM sys.check_constraints cc WHERE cc.parent_object_id = {id} \
         ORDER BY 1, 9"
    )
}

/// A table's `CREATE TABLE`, rebuilt from its columns ([`columns`]), constraints
/// ([`parse_constraints`]) and indexes ([`indexes`]); the other indexes follow it.
pub(crate) fn table_ddl(schema: &str, name: &str, cols: &[Column], cons: &[super::Constraint], idx: &[Index]) -> String {
    let mut lines: Vec<String> = cols
        .iter()
        .map(|c| {
            let n = Q.quote_ident(&c.name);
            match c.default.as_deref() {
                // A computed column has no type of its own.
                Some(d) if d.starts_with("AS ") => format!("{n} {d}"),
                d => {
                    let mut s = format!("{n} {}", c.type_name);
                    match d {
                        Some(i) if i.starts_with("IDENTITY") => s.push_str(&format!(" {i}")),
                        _ => {}
                    }
                    if c.nullable == Some(false) && c.type_name != "rowversion" {
                        s.push_str(" NOT NULL");
                    }
                    if let Some(def) = d.filter(|d| !d.starts_with("IDENTITY")) {
                        s.push_str(&format!(" DEFAULT {def}"));
                    }
                    s
                }
            }
        })
        .collect();
    lines.extend(cons.iter().filter_map(|c| Some(format!("CONSTRAINT {} {}", Q.quote_ident(&c.name), c.definition.as_ref()?))));
    let mut out = format!("CREATE TABLE {} (\n{}\n);\n", Q.qualified(Some(schema), name), lines.iter().map(|l| format!("    {l}")).collect::<Vec<_>>().join(",\n"));
    let other: Vec<&String> = idx.iter().filter_map(|i| i.definition.as_ref()).collect();
    if !other.is_empty() {
        out.push('\n');
        for d in other {
            out.push_str(d);
            out.push_str(";\n");
        }
    }
    out
}

/// Constraints in PostgreSQL's `pg_get_constraintdef` form (`FOREIGN KEY (a) REFERENCES t(b)
/// ON DELETE CASCADE`), which the explorer summarizes.
pub(crate) fn parse_constraints(schema: &str, rows: &[Vec<Option<String>>]) -> Vec<super::Constraint> {
    use super::ConstraintKind::*;
    let mut out: Vec<super::Constraint> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let r = &rows[i];
        let (Some(name), Some(kind)) = (get(r, 0), get(r, 1)) else {
            i += 1;
            continue;
        };
        let group: Vec<&Vec<Option<String>>> = rows[i..].iter().take_while(|x| get(x, 0).as_deref() == Some(name.as_str())).collect();
        i += group.len();
        let cols = |at: usize| group.iter().filter_map(|x| get(x, at)).map(|c| Q.quote_ident(&c)).collect::<Vec<_>>().join(", ");
        let (kind, definition) = match kind.trim() {
            "PK" => (Primary, format!("PRIMARY KEY ({})", cols(2))),
            "UQ" => (Unique, format!("UNIQUE ({})", cols(2))),
            "F" => {
                let rs = get(r, 3).unwrap_or_default();
                let rt = get(r, 4).unwrap_or_default();
                let target = if rs == schema || rs.is_empty() { Q.quote_ident(&rt) } else { Q.qualified(Some(&rs), &rt) };
                let mut d = format!("FOREIGN KEY ({}) REFERENCES {target}({})", cols(2), cols(5));
                for (at, on) in [(6, "UPDATE"), (7, "DELETE")] {
                    if let Some(rule) = get(r, at).filter(|x| x != "NO_ACTION") {
                        d.push_str(&format!(" ON {on} {}", rule.replace('_', " ")));
                    }
                }
                (Foreign, d)
            }
            "C" => (Check, format!("CHECK {}", get(r, 2).unwrap_or_default())),
            _ => continue,
        };
        out.push(super::Constraint { name, kind, definition: Some(definition) });
    }
    out.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|s| s.map(str::to_string)).collect()
    }

    #[test]
    fn literals_and_names() {
        assert_eq!(lit("o'k é"), "N'o''k é'");
        assert_eq!(object_id("dbo", "a]b"), "OBJECT_ID(N'[dbo].[a]]b]')");
        assert!(is_system_db("tempdb") && !is_system_db("shop"));
    }

    #[test]
    fn defaults() {
        assert_eq!(unwrap_parens("((0))"), "0");
        assert_eq!(unwrap_parens("(getdate())"), "getdate()");
        assert_eq!(unwrap_parens("(N'new')"), "N'new'");
        assert_eq!(unwrap_parens("((1)+(2))"), "(1)+(2)");
        assert_eq!(default_of(Some("(1,1)".into()), None, false, None).as_deref(), Some("IDENTITY(1,1)"));
        assert_eq!(default_of(None, Some("([qty]*[price])".into()), true, None).as_deref(), Some("AS ([qty]*[price]) PERSISTED"));
        assert_eq!(default_of(None, None, false, Some("((0))".into())).as_deref(), Some("0"));
    }

    #[test]
    fn routines() {
        let rows = vec![
            row(&[Some("dbo"), Some("orders_of"), Some("P "), Some("@customer"), Some("int"), Some("0"), Some("0")]),
            row(&[Some("dbo"), Some("orders_of"), Some("P "), Some("@total"), Some("money"), Some("1"), Some("0")]),
            row(&[Some("dbo"), Some("total"), Some("FN"), Some(""), Some("decimal(12,2)"), Some("1"), Some("1")]),
            row(&[Some("dbo"), Some("total"), Some("FN"), Some("@id"), Some("int"), Some("0"), Some("0")]),
            row(&[Some("sales"), Some("items"), Some("IF"), None, None, None, None]),
        ];
        let f = parse_routines(&rows);
        assert_eq!(f.len(), 3);
        assert_eq!((f[0].kind, f[0].args.as_str()), ('p', "@customer int, @total money OUTPUT"));
        assert_eq!((f[1].kind, f[1].args.as_str(), f[1].result.as_str()), ('f', "@id int", "decimal(12,2)"));
        assert_eq!((f[2].schema.as_str(), f[2].result.as_str(), f[2].returns_set), ("sales", "TABLE", true));
        let e = function_of(0, f[0].clone());
        assert_eq!((e.kind, e.arg_types.clone(), e.result.clone()), (super::super::FuncKind::Procedure, vec!["int".to_string()], None));
    }

    #[test]
    fn constraints() {
        let rows = vec![
            row(&[Some("CK_qty"), Some("C"), Some("([qty]>(0))"), None, None, None, None, None, Some("0")]),
            row(&[Some("FK_orders_customer"), Some("F"), Some("customer_id"), Some("dbo"), Some("customer"), Some("id"), Some("NO_ACTION"), Some("CASCADE"), Some("1")]),
            row(&[Some("PK_orders"), Some("PK"), Some("id"), None, None, None, None, None, Some("1")]),
            row(&[Some("UQ_x"), Some("UQ"), Some("a"), None, None, None, None, None, Some("1")]),
            row(&[Some("UQ_x"), Some("UQ"), Some("b c"), None, None, None, None, None, Some("2")]),
        ];
        let c = parse_constraints("dbo", &rows);
        let defs: Vec<(String, String)> = c.iter().map(|c| (c.name.clone(), c.definition.clone().unwrap())).collect();
        assert_eq!(
            defs,
            vec![
                ("PK_orders".into(), "PRIMARY KEY (id)".into()),
                ("UQ_x".into(), "UNIQUE (a, [b c])".into()),
                ("FK_orders_customer".into(), "FOREIGN KEY (customer_id) REFERENCES customer(id) ON DELETE CASCADE".into()),
                ("CK_qty".into(), "CHECK ([qty]>(0))".into()),
            ]
        );
    }

    #[test]
    fn table_ddl_from_the_catalog() {
        let col = |n: &str, t: &str, nullable: bool, d: Option<&str>| Column {
            name: n.into(),
            type_name: t.into(),
            nullable: Some(nullable),
            primary_key: false,
            default: d.map(str::to_string),
        };
        let cols = vec![
            col("id", "int", false, Some("IDENTITY(1,1)")),
            col("status", "varchar(12)", false, Some("'open'")),
            col("sub total", "decimal(20,2)", true, Some("AS ([qty]*[price]) PERSISTED")),
            col("row_version", "rowversion", false, None),
        ];
        let cons = vec![super::super::Constraint { name: "PK_t".into(), kind: super::super::ConstraintKind::Primary, definition: Some("PRIMARY KEY (id)".into()) }];
        let idx = vec![Index { name: "IX".into(), columns: vec!["status".into()], unique: false, primary: false, definition: Some("CREATE NONCLUSTERED INDEX IX ON dbo.t (status)".into()) }];
        assert_eq!(
            table_ddl("dbo", "t", &cols, &cons, &idx),
            "CREATE TABLE dbo.t (\n    id int IDENTITY(1,1) NOT NULL,\n    status varchar(12) NOT NULL DEFAULT 'open',\n    [sub total] AS ([qty]*[price]) PERSISTED,\n    row_version rowversion,\n    CONSTRAINT PK_t PRIMARY KEY (id)\n);\n\nCREATE NONCLUSTERED INDEX IX ON dbo.t (status);\n"
        );
    }

    #[test]
    fn triggers_and_types() {
        let t = parse_trigger(&row(&[Some("trg"), Some("orders"), Some("0"), Some("0"), Some("1"), Some("1"), Some("0"), Some("CREATE TRIGGER trg …")])).unwrap();
        assert_eq!((t.enabled, t.events.as_str()), (true, "after insert/update"));
        let rows = vec![row(&[Some("1"), Some("0"), None, Some("id"), Some("int"), Some("0")]), row(&[Some("1"), Some("0"), None, Some("name"), Some("nvarchar(50)"), Some("1")])];
        assert_eq!(parse_type_definition("dbo", "Items", &rows).unwrap(), "CREATE TYPE dbo.Items AS TABLE (\n    id int NOT NULL,\n    name nvarchar(50)\n);");
        let rows = vec![row(&[Some("0"), Some("0"), Some("varchar(11)"), None, None, None])];
        assert_eq!(parse_type_definition("dbo", "ssn", &rows).unwrap(), "CREATE TYPE dbo.ssn FROM varchar(11) NOT NULL;");
    }
}
