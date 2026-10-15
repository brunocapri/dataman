//! PostgreSQL's functions and types for the editor (`schema::catalog`): `pg_proc` and `pg_type`.

use super::super::catalog::{FuncSig, TypeEntry};

/// `pg_proc` without the internals nobody calls (type I/O, operator implementations,
/// handlers); the later variants are for older or partial catalogs.
pub fn functions_sql() -> [String; 3] {
    let skip_types = ["internal", "cstring", "trigger", "event_trigger", "language_handler", "fdw_handler", "index_am_handler", "table_am_handler", "tsm_handler"]
        .iter()
        .map(|t| format!("'pg_catalog.{t}'::pg_catalog.regtype"))
        .collect::<Vec<_>>()
        .join(", ");
    let from = format!(
        "FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%' \
         AND p.proname !~ '^_' AND p.prorettype NOT IN ({skip_types}) \
         AND NOT ('pg_catalog.internal'::pg_catalog.regtype = ANY (p.proargtypes)) \
         AND NOT ('pg_catalog.cstring'::pg_catalog.regtype = ANY (p.proargtypes)) \
         AND (n.nspname <> 'pg_catalog' OR NOT EXISTS (SELECT 1 FROM pg_catalog.pg_operator o WHERE o.oprcode = p.oid))"
    );
    [
        format!(
            "SELECT p.oid, n.nspname, p.proname, pg_catalog.pg_get_function_arguments(p.oid), \
             pg_catalog.pg_get_function_result(p.oid), p.prokind, p.proretset {from}"
        ),
        // Before PostgreSQL 11: no prokind.
        format!(
            "SELECT p.oid, n.nspname, p.proname, pg_catalog.pg_get_function_arguments(p.oid), \
             pg_catalog.pg_get_function_result(p.oid), CASE WHEN p.proisagg THEN 'a' ELSE 'f' END, p.proretset {from}"
        ),
        "SELECT p.oid, n.nspname, p.proname, pg_catalog.oidvectortypes(p.proargtypes), \
         pg_catalog.format_type(p.prorettype, NULL), 'f', p.proretset \
         FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace"
            .to_string(),
    ]
}

pub fn types_sql() -> String {
    "SELECT n.nspname, t.typname, t.typtype, \
     (SELECT pg_catalog.string_agg(e.enumlabel, chr(31) ORDER BY e.enumsortorder) FROM pg_catalog.pg_enum e WHERE e.enumtypid = t.oid) \
     FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
     WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_%' \
     AND t.typtype IN ('e', 'd', 'c', 'r', 'm', 'b') AND t.typname !~ '^_' \
     AND (t.typrelid = 0 OR (SELECT c.relkind FROM pg_catalog.pg_class c WHERE c.oid = t.typrelid) = 'c')"
        .to_string()
}

fn cell(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

pub fn parse_function(r: &[Option<String>]) -> Option<FuncSig> {
    Some(FuncSig {
        oid: cell(r, 0)?.parse().ok()?,
        schema: cell(r, 1)?,
        name: cell(r, 2)?,
        args: cell(r, 3).unwrap_or_default(),
        result: cell(r, 4).unwrap_or_default(),
        kind: cell(r, 5).and_then(|k| k.chars().next()).unwrap_or('f'),
        returns_set: matches!(cell(r, 6).as_deref(), Some("t" | "true")),
    })
}

pub fn parse_type(r: &[Option<String>]) -> Option<TypeEntry> {
    Some(TypeEntry {
        schema: cell(r, 0)?,
        name: cell(r, 1)?,
        kind: cell(r, 2).and_then(|k| k.chars().next()).unwrap_or('b'),
        labels: cell(r, 3).map(|l| l.split('\u{1f}').map(String::from).collect()).unwrap_or_default(),
    })
}
