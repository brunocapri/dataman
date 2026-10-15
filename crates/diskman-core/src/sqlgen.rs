//! Building SQL text: identifiers, literals per [`Kind`], unambiguous (UTC) time literals.
//!
//! Used by export (`INSERT` statements), grid editing, search and the LAR helpers. Literals assume
//! `standard_conforming_strings = on` (PostgreSQL's default since 9.1, and LAR's only mode):
//! backslashes inside `'…'` are plain characters.
//!
//! Everything here writes PostgreSQL; quoting comes from [`Dialect`], whose other dialects these
//! functions will take as a parameter when their engines arrive.

use crate::result::{Cell, format_cell};
use crate::schema::Column;
use crate::sql::Dialect;
use crate::value::{self, Kind};

/// Quotes an identifier when PostgreSQL would fold or reject it unquoted (reserved words
/// included): [`Dialect::quote_ident`] of PostgreSQL.
pub fn quote_ident(name: &str) -> String {
    Dialect::Postgres.quote_ident(name)
}

/// `schema.table`, each part quoted when needed; no schema → just the table.
pub fn qualified(schema: Option<&str>, name: &str) -> String {
    Dialect::Postgres.qualified(schema, name)
}

/// `'text'` with quotes doubled.
pub fn quote_literal(s: &str) -> String {
    Dialect::Postgres.quote_literal(s)
}

/// A cell value given as text, or NULL: what the user typed in the grid.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CellValue {
    Null,
    Text(String),
}

impl CellValue {
    pub fn text(s: impl Into<String>) -> CellValue {
        CellValue::Text(s.into())
    }

    pub fn as_deref(&self) -> Option<&str> {
        match self {
            CellValue::Null => None,
            CellValue::Text(s) => Some(s),
        }
    }

    /// The value of a cell as the grid displays it (timestamps in UTC, see [`cell_text_utc`]).
    pub fn from_cell(kind: Kind, cell: Cell<'_>) -> CellValue {
        match cell_text_utc(kind, cell) {
            Some(s) => CellValue::Text(s),
            None => CellValue::Null,
        }
    }
}

/// Microseconds since 2000-01-01 UTC → `2024-01-02 03:04:05.5+00`.
pub fn timestamptz_utc(micros: i64) -> String {
    let mut s = String::new();
    value::format_timestamp(micros, &mut s);
    if micros != i64::MAX && micros != i64::MIN {
        s.push_str("+00");
    }
    s
}

/// Microseconds since 2000-01-01 UTC → RFC 3339 (`2024-01-02T03:04:05.5Z`).
pub fn timestamptz_rfc3339(micros: i64) -> String {
    let mut s = String::new();
    value::format_timestamp(micros, &mut s);
    if micros == i64::MAX || micros == i64::MIN {
        return s;
    }
    if let Some(sp) = s.find(' ') {
        s.replace_range(sp..sp + 1, "T");
    }
    s.push('Z');
    s
}

/// A cell as text, like the grid, except `timestamptz`, which comes out in UTC (`+00`) so it
/// does not depend on this machine's zone.
pub fn cell_text_utc(kind: Kind, cell: Cell<'_>) -> Option<String> {
    match (kind, cell) {
        (_, Cell::Null) => None,
        (Kind::TimestampTz, Cell::Int(v)) => Some(timestamptz_utc(v)),
        (k, c) => format_cell(k, c).map(|s| s.into_owned()),
    }
}

fn is_integer(s: &str) -> bool {
    let d = s.strip_prefix('-').unwrap_or(s);
    !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit())
}

/// A plain decimal number (what can go unquoted): `-12`, `3.5`, `1e+300`, `.5`.
pub fn is_decimal(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = |p: &str| p.bytes().all(|c| c.is_ascii_digit());
    if int.is_empty() && frac.is_empty() || !digits(int) || !digits(frac) {
        return false;
    }
    match exp {
        None => true,
        Some(e) => {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            !e.is_empty() && digits(e)
        }
    }
}

/// The SQL literal for a stored cell of `kind`.
pub fn literal(kind: Kind, cell: Cell<'_>) -> String {
    literal_in(Dialect::Postgres, kind, cell)
}

/// `2024-01-02 03:04:05.5` → `2024-01-02T03:04:05.5`: T-SQL reads the `T` form the same way
/// whatever the session's language and date format (`datetime` misreads the space form under
/// some languages).
fn tsql_time(s: &str) -> String {
    let mut out = match s.split_once(' ') {
        Some((date, time)) if date.len() == 10 && date.as_bytes().get(4) == Some(&b'-') => format!("{date}T{}", time.trim()),
        _ => s.to_string(),
    };
    // A UTC offset without its minutes (`+00`, as the grid writes one) needs them.
    let b = out.as_bytes();
    if b.len() > 3 && matches!(b[b.len() - 3], b'+' | b'-') && b[b.len() - 2..].iter().all(u8::is_ascii_digit) && out.contains('T') {
        out.push_str(":00");
    }
    out
}

/// [`literal`] in `d` (MySQL: backslashes escaped, bytes as `X'…'`; T-SQL: `N'…'`, bits as
/// `1`/`0`, bytes as `0x…`, times in the `T` form, a UTC offset as `+00:00`).
pub fn literal_in(d: Dialect, kind: Kind, cell: Cell<'_>) -> String {
    let ms = d == Dialect::TSql;
    match cell {
        Cell::Null => "NULL".into(),
        Cell::Bool(b) if ms => if b { "1" } else { "0" }.into(),
        Cell::Bool(b) => if b { "TRUE" } else { "FALSE" }.into(),
        Cell::Int(v) => match kind {
            Kind::Timestamp if ms && !value::is_odd_date(v) => d.quote_literal(&tsql_time(&format_cell(kind, cell).map(|s| s.into_owned()).unwrap_or_default())),
            Kind::Date | Kind::Time | Kind::Timestamp => {
                d.quote_literal(&format_cell(kind, cell).map(|s| s.into_owned()).unwrap_or_default())
            }
            Kind::TimestampTz if ms => {
                let mut s = String::new();
                value::format_timestamp(v, &mut s);
                d.quote_literal(&format!("{}+00:00", tsql_time(&s)))
            }
            Kind::TimestampTz => d.quote_literal(&timestamptz_utc(v)),
            _ => v.to_string(),
        },
        Cell::Float(f) => {
            let mut s = String::new();
            value::format_float(f, kind == Kind::Float4, &mut s);
            if f.is_finite() { s } else { d.quote_literal(&s) }
        }
        Cell::Text(s) => text_literal(d, kind, s),
    }
}

/// The SQL literal for a value typed as text into a column of `kind`: numbers and booleans go
/// unquoted when they look like one (so servers without implicit casts from text accept them),
/// everything else is a quoted string the server converts.
pub fn value_literal(kind: Kind, v: &CellValue) -> String {
    value_literal_in(Dialect::Postgres, kind, v)
}

/// [`value_literal`] in `d`.
pub fn value_literal_in(d: Dialect, kind: Kind, v: &CellValue) -> String {
    let mut out = String::new();
    push_value_literal_in(d, &mut out, kind, v.as_deref());
    out
}

/// [`value_literal`] appended to `out` (no allocation per value: the import's `INSERT`s).
pub fn push_value_literal(out: &mut String, kind: Kind, v: Option<&str>) {
    push_value_literal_in(Dialect::Postgres, out, kind, v)
}

/// [`push_value_literal`] in `d`.
pub fn push_value_literal_in(d: Dialect, out: &mut String, kind: Kind, v: Option<&str>) {
    let Some(s) = v else {
        out.push_str("NULL");
        return;
    };
    let t = s.trim();
    let ms = d == Dialect::TSql;
    match kind {
        Kind::Bool if matches!(t.len(), 1..=5) => match t.to_ascii_lowercase().as_str() {
            "t" | "true" | "yes" | "y" | "on" | "1" => out.push_str(if ms { "1" } else { "TRUE" }),
            "f" | "false" | "no" | "n" | "off" | "0" => out.push_str(if ms { "0" } else { "FALSE" }),
            _ => push_quoted(d, out, s),
        },
        Kind::Int if is_integer(t) => out.push_str(t),
        Kind::Float | Kind::Float4 | Kind::Numeric if is_decimal(t) => out.push_str(t),
        Kind::Bytes if d == Dialect::MySql && hex_bytes(t).is_some() => {
            out.push_str("X'");
            out.push_str(hex_bytes(t).unwrap_or_default());
            out.push('\'');
        }
        Kind::Bytes if ms && hex_bytes(t).is_some() => {
            out.push_str("0x");
            out.push_str(hex_bytes(t).unwrap_or_default());
        }
        Kind::Timestamp | Kind::TimestampTz if ms => push_quoted(d, out, &tsql_time(t)),
        _ => push_quoted(d, out, s),
    }
}

/// `\xdeadbeef` (how the grid shows bytes) → `deadbeef`.
fn hex_bytes(s: &str) -> Option<&str> {
    let h = s.strip_prefix("\\x")?;
    (h.len() % 2 == 0 && h.bytes().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

fn push_quoted(d: Dialect, out: &mut String, s: &str) {
    out.reserve(s.len() + 3);
    if d == Dialect::TSql {
        out.push('N');
    }
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || (c == '\\' && d == Dialect::MySql) {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
}

fn text_literal(d: Dialect, kind: Kind, s: &str) -> String {
    match kind {
        Kind::Numeric | Kind::Int | Kind::Float | Kind::Float4 if is_decimal(s) => s.to_string(),
        Kind::Bytes if d == Dialect::MySql && hex_bytes(s).is_some() => format!("X'{}'", hex_bytes(s).unwrap_or_default()),
        Kind::Bytes if d == Dialect::TSql && hex_bytes(s).is_some() => format!("0x{}", hex_bytes(s).unwrap_or_default()),
        _ => d.quote_literal(s),
    }
}

// ---------------------------------------------------------------------------------------------
// Templates (the explorer's context menu). They only build text: nothing here runs SQL, and the
// destructive ones start with a warning comment and are never run by the app.

/// `SELECT * FROM schema.table LIMIT n;` (no limit with `None`).
pub fn select_star(schema: &str, name: &str, limit: Option<u64>) -> String {
    select_star_in(Dialect::Postgres, schema, name, limit)
}

/// [`select_star`] in `d` (T-SQL: `SELECT TOP (n)`).
pub fn select_star_in(d: Dialect, schema: &str, name: &str, limit: Option<u64>) -> String {
    match limit {
        Some(n) if d == Dialect::TSql => format!("SELECT TOP ({n}) * FROM {};", d.qualified(Some(schema), name)),
        Some(n) => format!("SELECT * FROM {} LIMIT {n};", d.qualified(Some(schema), name)),
        None => format!("SELECT * FROM {};", d.qualified(Some(schema), name)),
    }
}

/// `SELECT count(*) FROM schema.table;`
pub fn count_rows(schema: &str, name: &str) -> String {
    count_rows_in(Dialect::Postgres, schema, name)
}

/// [`count_rows`] in `d`.
pub fn count_rows_in(d: Dialect, schema: &str, name: &str) -> String {
    format!("SELECT count(*) FROM {};", d.qualified(Some(schema), name))
}

/// A value of the column's type that the server accepts (a typed placeholder for templates).
/// PostgreSQL's type names and MySQL's (`int unsigned`, `datetime(3)`, `enum(…)`).
pub fn placeholder(type_name: &str) -> &'static str {
    let t = type_name.trim().to_ascii_lowercase();
    if t.ends_with("[]") {
        return "'{}'";
    }
    // `numeric(12,2)` → `numeric`, `character varying(40)` → `character varying`, `int
    // unsigned` → `int`.
    let base = t.split('(').next().unwrap_or("").trim();
    let base = base.trim_end_matches(" zerofill").trim_end_matches(" unsigned").trim_end_matches(" signed");
    match base {
        "smallint" | "integer" | "bigint" | "int" | "int2" | "int4" | "int8" | "smallserial" | "serial" | "bigserial" | "oid" => "0",
        "tinyint" | "mediumint" => "0",
        "numeric" | "decimal" | "real" | "double precision" | "float4" | "float8" | "money" | "float" | "double" => "0",
        "boolean" | "bool" => "false",
        "text" | "character varying" | "varchar" | "character" | "char" | "bpchar" | "name" | "citext" => "''",
        "tinytext" | "mediumtext" | "longtext" | "enum" | "set" => "''",
        "date" => "CURRENT_DATE",
        "timestamp with time zone" | "timestamp without time zone" | "timestamp" | "timestamptz" | "datetime" => "now()",
        "time with time zone" | "time without time zone" | "time" | "timetz" => "CURRENT_TIME",
        "year" => "YEAR(CURRENT_DATE)",
        "bit" => "b'0'",
        "interval" => "'0 seconds'",
        "uuid" => "'00000000-0000-0000-0000-000000000000'",
        "json" | "jsonb" => "'{}'",
        "bytea" => "'\\x'",
        "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" => "X''",
        "inet" | "cidr" => "'0.0.0.0'",
        _ => "NULL",
    }
}

/// [`placeholder`] for a column of `d`: SQL Server's types and functions (`0` for a bit,
/// `SYSDATETIME()` for a time, `N''` for text, `0x` for bytes).
pub fn placeholder_in(d: Dialect, type_name: &str) -> &'static str {
    if d != Dialect::TSql {
        return placeholder(type_name);
    }
    let t = type_name.trim().to_ascii_lowercase();
    match t.split('(').next().unwrap_or("").trim() {
        "bit" | "tinyint" | "smallint" | "int" | "bigint" | "decimal" | "numeric" | "money" | "smallmoney" | "real" | "float" => "0",
        "char" | "varchar" | "text" => "''",
        "nchar" | "nvarchar" | "ntext" | "xml" | "sysname" => "N''",
        "date" => "CAST(SYSDATETIME() AS date)",
        "time" => "CAST(SYSDATETIME() AS time)",
        "datetime" | "smalldatetime" => "GETDATE()",
        "datetime2" => "SYSDATETIME()",
        "datetimeoffset" => "SYSDATETIMEOFFSET()",
        "uniqueidentifier" => "NEWID()",
        "binary" | "varbinary" | "image" => "0x",
        _ => "NULL",
    }
}

/// `-- name type NOT NULL · default …`: what a template line stands for.
fn column_note(c: &Column) -> String {
    let mut s = format!("{} {}", c.name, c.type_name);
    if c.nullable == Some(false) {
        s.push_str(" NOT NULL");
    }
    if let Some(d) = &c.default {
        let d: String = if d.chars().count() > 48 { format!("{}…", d.chars().take(47).collect::<String>()) } else { d.clone() };
        s.push_str(" · default ");
        s.push_str(&d);
    }
    s
}

/// `SELECT a, b, … FROM schema.table;` with every column listed.
pub fn select_columns(schema: &str, name: &str, cols: &[Column]) -> String {
    select_columns_in(Dialect::Postgres, schema, name, cols)
}

/// [`select_columns`] in `d`.
pub fn select_columns_in(d: Dialect, schema: &str, name: &str, cols: &[Column]) -> String {
    if cols.is_empty() {
        return select_star_in(d, schema, name, None);
    }
    let list: Vec<String> = cols.iter().map(|c| d.quote_ident(&c.name)).collect();
    format!("SELECT {}\n  FROM {};", list.join(",\n       "), d.qualified(Some(schema), name))
}

/// Lines `value,  -- note`, the comments aligned.
fn commented_values(values: &[(String, String)], indent: &str) -> String {
    let w = values.iter().map(|(v, _)| v.chars().count() + 1).max().unwrap_or(0);
    let last = values.len().saturating_sub(1);
    values
        .iter()
        .enumerate()
        .map(|(i, (v, note))| {
            let v = if i == last { v.clone() } else { format!("{v},") };
            format!("{indent}{v:<w$}  -- {note}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `INSERT INTO schema.table (…) VALUES (…);` with a typed placeholder per column, `DEFAULT`
/// where the column has a default (identity and generated columns included), and a comment
/// with each column's type.
pub fn insert_template(schema: &str, name: &str, cols: &[Column]) -> String {
    insert_template_in(Dialect::Postgres, schema, name, cols)
}

/// [`insert_template`] in `d` (MySQL has no `DEFAULT VALUES`: `() VALUES ()`).
pub fn insert_template_in(d: Dialect, schema: &str, name: &str, cols: &[Column]) -> String {
    let qualified = |s: Option<&str>, n: &str| d.qualified(s, n);
    // SQL Server fills identity, computed and rowversion columns itself (naming them is an
    // error, even with DEFAULT).
    let generated = |c: &&Column| {
        d == Dialect::TSql
            && (c.default.as_deref().is_some_and(|x| x.starts_with("IDENTITY") || x.starts_with("AS ")) || matches!(c.type_name.as_str(), "timestamp" | "rowversion"))
    };
    let cols: Vec<&Column> = cols.iter().filter(|c| !generated(c)).collect();
    if cols.is_empty() {
        return match d {
            Dialect::MySql => format!("INSERT INTO {} () VALUES ();", qualified(Some(schema), name)),
            _ => format!("INSERT INTO {} DEFAULT VALUES;", qualified(Some(schema), name)),
        };
    }
    let names: Vec<String> = cols.iter().map(|c| d.quote_ident(&c.name)).collect();
    let values: Vec<(String, String)> = cols
        .iter()
        .map(|c| {
            let v = if c.default.is_some() { "DEFAULT".to_string() } else { placeholder_in(d, &c.type_name).to_string() };
            (v, column_note(c))
        })
        .collect();
    format!(
        "INSERT INTO {} ({})\nVALUES (\n{}\n);",
        qualified(Some(schema), name),
        names.join(", "),
        commented_values(&values, "    ")
    )
}

/// The row condition of UPDATE and DELETE templates: the primary key when the catalog reports
/// one; else a column named `id` (said to be a guess); else `WHERE false`, which matches no row,
/// so a template run as is never touches the whole table.
fn key_condition(d: Dialect, cols: &[Column]) -> String {
    let pk: Vec<&Column> = cols.iter().filter(|c| c.primary_key).collect();
    if !pk.is_empty() {
        let parts: Vec<String> = pk.iter().map(|c| format!("{} = {}", d.quote_ident(&c.name), placeholder_in(d, &c.type_name))).collect();
        return format!(" WHERE {};", parts.join("\n   AND "));
    }
    let never = if d == Dialect::TSql { "1 = 0" } else { "false" };
    match cols.iter().find(|c| c.name == "id") {
        Some(c) => format!(" WHERE id = {};  -- key guessed: the catalog reports no primary key", placeholder_in(d, &c.type_name)),
        None => format!(" WHERE {never};  -- no primary key reported: write the row condition"),
    }
}

/// `UPDATE schema.table SET … WHERE <pk> = …;`: every non-key column with a typed placeholder.
pub fn update_template(schema: &str, name: &str, cols: &[Column]) -> String {
    update_template_in(Dialect::Postgres, schema, name, cols)
}

/// [`update_template`] in `d`.
pub fn update_template_in(d: Dialect, schema: &str, name: &str, cols: &[Column]) -> String {
    let generated = |c: &Column| {
        d == Dialect::TSql
            && (c.default.as_deref().is_some_and(|x| x.starts_with("IDENTITY") || x.starts_with("AS ")) || matches!(c.type_name.as_str(), "timestamp" | "rowversion"))
    };
    let set: Vec<&Column> = {
        let non_key: Vec<&Column> = cols.iter().filter(|c| !c.primary_key && !generated(c)).collect();
        if non_key.is_empty() { cols.iter().collect() } else { non_key }
    };
    let sets: Vec<String> = set.iter().map(|c| format!("{} = {}", d.quote_ident(&c.name), placeholder_in(d, &c.type_name))).collect();
    let sets = if sets.is_empty() { "column = NULL".to_string() } else { sets.join(",\n       ") };
    format!("UPDATE {}\n   SET {}\n{}", d.qualified(Some(schema), name), sets, key_condition(d, cols))
}

/// `DELETE FROM schema.table WHERE <pk> = …;`
pub fn delete_template(schema: &str, name: &str, cols: &[Column]) -> String {
    delete_template_in(Dialect::Postgres, schema, name, cols)
}

/// [`delete_template`] in `d`.
pub fn delete_template_in(d: Dialect, schema: &str, name: &str, cols: &[Column]) -> String {
    format!("DELETE FROM {}\n{}", d.qualified(Some(schema), name), key_condition(d, cols))
}

/// A view's definition as a statement: `CREATE OR REPLACE VIEW s.v AS …;` (materialized views
/// cannot be replaced in place: `CREATE MATERIALIZED VIEW`, with a note).
pub fn view_ddl(schema: &str, name: &str, materialized: bool, definition: &str) -> String {
    let body = definition.trim().trim_end_matches(';').trim_end();
    let q = qualified(Some(schema), name);
    if materialized {
        format!("-- A materialized view cannot be replaced in place: DROP MATERIALIZED VIEW {q} first.\nCREATE MATERIALIZED VIEW {q} AS\n{body};")
    } else {
        format!("CREATE OR REPLACE VIEW {q} AS\n{body};")
    }
}

/// `CREATE SCHEMA` template for database `db`.
pub fn create_schema_template(db: &str) -> String {
    format!("-- A new schema in {}. Rename it, then run it with ⌘↩.\nCREATE SCHEMA new_schema;", quote_ident(db))
}

/// `CREATE TABLE` template in `schema` (LAR: no identity column, which it does not support).
pub fn create_table_template(schema: &str, lar: bool) -> String {
    let q = qualified(Some(schema), "new_table");
    if lar {
        format!(
            "-- A new table in {}. Edit the columns, then run it with ⌘↩.\nCREATE TABLE {q} (\n    id         bigint PRIMARY KEY,\n    name       text NOT NULL,\n    created_at timestamp\n);",
            quote_ident(schema)
        )
    } else {
        format!(
            "-- A new table in {}. Edit the columns, then run it with ⌘↩.\nCREATE TABLE {q} (\n    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n    name       text NOT NULL,\n    created_at timestamptz NOT NULL DEFAULT now()\n);",
            quote_ident(schema)
        )
    }
}

/// `CREATE TABLE` template in MySQL database `schema`.
pub fn create_table_template_mysql(schema: &str) -> String {
    let d = Dialect::MySql;
    format!(
        "-- A new table in {}. Edit the columns, then run it with ⌘↩.\nCREATE TABLE {} (\n    id         BIGINT AUTO_INCREMENT PRIMARY KEY,\n    name       VARCHAR(255) NOT NULL,\n    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP\n);",
        d.quote_ident(schema),
        d.qualified(Some(schema), "new_table")
    )
}

/// `CREATE TABLE` template in SQL Server schema `schema`.
pub fn create_table_template_tsql(schema: &str) -> String {
    let d = Dialect::TSql;
    format!(
        "-- A new table in {}. Edit the columns, then run it with ⌘↩.\nCREATE TABLE {} (\n    id         bigint IDENTITY(1, 1) PRIMARY KEY,\n    name       nvarchar(255) NOT NULL,\n    created_at datetime2 NOT NULL DEFAULT SYSUTCDATETIME()\n);",
        d.quote_ident(schema),
        d.qualified(Some(schema), "new_table")
    )
}

/// The warning every destructive template starts with.
fn danger(what: &str, statement: String) -> String {
    format!("-- ⚠ {what}\n-- Nothing has run. Review it, then run it yourself with ⌘↩.\n{statement}")
}

/// [`drop_database_template`] in `d` (MySQL drops it at once, whoever is connected; SQL Server
/// refuses while sessions use it).
pub fn drop_database_template_in(d: Dialect, db: &str) -> String {
    let q = d.quote_ident(db);
    match d {
        Dialect::Postgres => drop_database_template(db),
        Dialect::MySql => danger(&format!("DROP DATABASE deletes {q} and everything in it, for good."), format!("DROP DATABASE {q};")),
        Dialect::TSql => danger(
            &format!(
                "DROP DATABASE deletes {q} and everything in it, for good.\n-- It fails while sessions use {q}: ALTER DATABASE {q} SET SINGLE_USER WITH ROLLBACK IMMEDIATE ends them.\n-- Run it from another database (USE master)."
            ),
            format!("DROP DATABASE {q};"),
        ),
    }
}

/// [`truncate_template`] in `d`.
pub fn truncate_template_in(d: Dialect, schema: &str, name: &str) -> String {
    let q = d.qualified(Some(schema), name);
    danger(&format!("TRUNCATE deletes every row of {q} at once: no WHERE, no per-row triggers."), format!("TRUNCATE TABLE {q};"))
}

/// [`drop_table_template`] in `d`.
pub fn drop_table_template_in(d: Dialect, schema: &str, name: &str) -> String {
    if d == Dialect::Postgres {
        return drop_table_template(schema, name);
    }
    let q = d.qualified(Some(schema), name);
    danger(&format!("DROP TABLE deletes {q} and its rows. Foreign keys that reference it make it fail."), format!("DROP TABLE {q};"))
}

/// [`drop_view_template`] in `d`.
pub fn drop_view_template_in(d: Dialect, schema: &str, name: &str) -> String {
    if d == Dialect::Postgres {
        return drop_view_template(schema, name, false);
    }
    let q = d.qualified(Some(schema), name);
    danger(&format!("DROP VIEW deletes {q}."), format!("DROP VIEW {q};"))
}

pub fn drop_database_template(db: &str) -> String {
    let q = quote_ident(db);
    danger(
        &format!(
            "DROP DATABASE deletes {q} and everything in it, for good.\n-- It fails while sessions are connected to {q} (PostgreSQL 13+: WITH (FORCE) ends them),\n-- and it cannot run inside a transaction."
        ),
        format!("DROP DATABASE {q};"),
    )
}

/// [`drop_schema_template`] in `d` (T-SQL has no CASCADE: the schema must be empty).
pub fn drop_schema_template_in(d: Dialect, schema: &str) -> String {
    if d != Dialect::TSql {
        return drop_schema_template(schema);
    }
    let q = d.quote_ident(schema);
    danger(&format!("DROP SCHEMA deletes {q}. It fails while the schema holds any object."), format!("DROP SCHEMA {q};"))
}

pub fn drop_schema_template(schema: &str) -> String {
    let q = quote_ident(schema);
    danger(
        &format!("DROP SCHEMA deletes {q}. It fails if the schema is not empty; CASCADE also drops every object in it."),
        format!("DROP SCHEMA {q};"),
    )
}

pub fn truncate_template(schema: &str, name: &str) -> String {
    let q = qualified(Some(schema), name);
    danger(&format!("TRUNCATE deletes every row of {q} at once: no WHERE, no per-row triggers."), format!("TRUNCATE TABLE {q};"))
}

pub fn drop_table_template(schema: &str, name: &str) -> String {
    let q = qualified(Some(schema), name);
    danger(
        &format!("DROP TABLE deletes {q} and its rows. Views and foreign keys that depend on it make it fail\n-- (CASCADE drops them too)."),
        format!("DROP TABLE {q};"),
    )
}

pub fn drop_view_template(schema: &str, name: &str, materialized: bool) -> String {
    let q = qualified(Some(schema), name);
    let kind = if materialized { "MATERIALIZED VIEW" } else { "VIEW" };
    danger(&format!("DROP {kind} deletes {q}. Views that depend on it make it fail (CASCADE drops them too)."), format!("DROP {kind} {q};"))
}

/// The grantee a template names: the role picked in the privileges panel, or a name to edit.
fn grantee(role: Option<&str>) -> String {
    match role {
        Some("PUBLIC") => "PUBLIC".to_string(),
        Some(r) => quote_ident(r),
        None => "role_name".to_string(),
    }
}

/// `GRANT … ON TABLE t` (or `ON SCHEMA s` when `table` is `None`) as a template.
pub fn grant_template(schema: &str, table: Option<&str>, role: Option<&str>) -> String {
    let (on, privs, what) = match table {
        Some(t) => (format!("TABLE {}", qualified(Some(schema), t)), "SELECT, INSERT, UPDATE, DELETE", qualified(Some(schema), t)),
        None => (format!("SCHEMA {}", quote_ident(schema)), "USAGE", quote_ident(schema)),
    };
    format!("-- Grant on {what}. Edit the privileges and the role, then run it with ⌘↩.\nGRANT {privs} ON {on} TO {};", grantee(role))
}

/// `REVOKE ALL ON …` as a template, with the destructive warning.
pub fn revoke_template(schema: &str, table: Option<&str>, role: Option<&str>) -> String {
    let (on, what) = match table {
        Some(t) => (format!("TABLE {}", qualified(Some(schema), t)), qualified(Some(schema), t)),
        None => (format!("SCHEMA {}", quote_ident(schema)), quote_ident(schema)),
    };
    let g = grantee(role);
    danger(
        &format!("REVOKE takes privileges on {what} away from {g}: its sessions lose that access at once."),
        format!("REVOKE ALL ON {on} FROM {g};"),
    )
}

/// A call of a function or procedure with a typed placeholder per argument:
/// `SELECT s.f(…)`, `SELECT * FROM s.f(…)` for set-returning functions, `CALL s.p(…)`.
pub fn function_call(schema: &str, f: &crate::schema::Function) -> String {
    function_call_in(Dialect::Postgres, schema, f)
}

/// [`function_call`] in `d`.
pub fn function_call_in(d: Dialect, schema: &str, f: &crate::schema::Function) -> String {
    use crate::schema::FuncKind;
    let q = d.qualified(Some(schema), &f.name);
    // T-SQL runs a procedure with EXEC and its arguments by name (`@a = …`).
    if d == Dialect::TSql && f.kind == FuncKind::Procedure {
        let params = crate::schema::objects::split_args(&f.identity);
        if params.is_empty() {
            return format!("EXEC {q};");
        }
        let values: Vec<(String, String)> = params
            .iter()
            .map(|p| {
                let (name, ty) = p.split_once(' ').unwrap_or((p.as_str(), ""));
                let ty = ty.trim_end_matches(" OUTPUT");
                (format!("{name} = {}", placeholder_in(d, ty)), p.clone())
            })
            .collect();
        return format!("EXEC {q}\n{}\n;", commented_values(&values, "    "));
    }
    let head = match (f.kind, f.returns_set) {
        (FuncKind::Procedure, _) => format!("CALL {q}"),
        (_, true) => format!("SELECT * FROM {q}"),
        _ => format!("SELECT {q}"),
    };
    if f.arg_types.is_empty() {
        return format!("{head}();");
    }
    let named = crate::schema::objects::split_args(&f.identity);
    let values: Vec<(String, String)> = f
        .arg_types
        .iter()
        .enumerate()
        .map(|(i, ty)| {
            let note = named.get(i).filter(|_| named.len() == f.arg_types.len()).cloned().unwrap_or_else(|| ty.clone());
            (placeholder_in(d, ty).to_string(), note)
        })
        .collect();
    format!("{head}(\n{}\n);", commented_values(&values, "    "))
}

/// `ALTER SEQUENCE … RESTART` as a template; for an owned sequence, the `setval` that
/// continues after the column's largest value.
pub fn sequence_reset_template(schema: &str, name: &str, owned_by: Option<(&str, &str)>) -> String {
    let q = qualified(Some(schema), name);
    let mut s = danger(
        &format!("RESTART sets {q} back to its start: new rows can collide with existing keys."),
        format!("ALTER SEQUENCE {q} RESTART;"),
    );
    if let Some((t, c)) = owned_by {
        s.push_str(&format!(
            "\n\n-- Or continue after the largest {}:\n-- SELECT setval({}, (SELECT coalesce(max({}), 0) + 1 FROM {}), false);",
            quote_ident(c),
            quote_literal(&q),
            quote_ident(c),
            qualified(Some(schema), t)
        ));
    }
    s
}

/// `ALTER TABLE … ENABLE/DISABLE TRIGGER` as a template.
pub fn trigger_toggle_template(schema: &str, table: &str, name: &str, enable: bool) -> String {
    trigger_toggle_template_in(Dialect::Postgres, schema, table, name, enable)
}

/// [`trigger_toggle_template`] in `d` (the same statement in T-SQL, its names quoted there).
pub fn trigger_toggle_template_in(d: Dialect, schema: &str, table: &str, name: &str, enable: bool) -> String {
    let t = d.qualified(Some(schema), table);
    let n = d.quote_ident(name);
    let stmt = format!("ALTER TABLE {t} {} TRIGGER {n};", if enable { "ENABLE" } else { "DISABLE" });
    if enable {
        format!("-- {n} fires again on {t}. Run it with ⌘↩.\n{stmt}")
    } else {
        danger(&format!("DISABLE stops {n} on {t}: what it enforces or records stops until it is enabled."), stmt)
    }
}

/// [`sequence_reset_template`] in `d` (T-SQL: no owned sequences).
pub fn sequence_reset_template_in(d: Dialect, schema: &str, name: &str, owned_by: Option<(&str, &str)>) -> String {
    if d == Dialect::Postgres {
        return sequence_reset_template(schema, name, owned_by);
    }
    let q = d.qualified(Some(schema), name);
    danger(&format!("RESTART sets {q} back to its start: new rows can collide with existing keys."), format!("ALTER SEQUENCE {q} RESTART;"))
}

/// `ALTER ROLE` options, commented out, to pick from.
pub fn alter_role_template(name: &str) -> String {
    let r = quote_ident(name);
    format!(
        "-- ALTER ROLE {r}. Uncomment what you need, then run it with ⌘↩.\n\
         -- ALTER ROLE {r} WITH PASSWORD 'new password';\n\
         -- ALTER ROLE {r} WITH LOGIN;\n\
         -- ALTER ROLE {r} WITH NOLOGIN;\n\
         -- ALTER ROLE {r} CONNECTION LIMIT 10;\n\
         -- ALTER ROLE {r} VALID UNTIL '2027-01-01';\n\
         -- ALTER ROLE {r} SET search_path TO public;\n\
         -- ALTER ROLE {r} RENAME TO new_name;"
    )
}

/// `ALTER EXTENSION … UPDATE` as a template.
pub fn extension_update_template(name: &str, to: Option<&str>) -> String {
    let e = quote_ident(name);
    let to = to.map(|v| format!(" TO {}", quote_literal(v))).unwrap_or_default();
    format!("-- Update {e}. Run it with ⌘↩.\nALTER EXTENSION {e} UPDATE{to};")
}

/// `SET search_path` for a tab opened on a schema (the schema first, then `public`).
pub fn search_path_for(schema: &str) -> String {
    if schema == "public" {
        "SET search_path TO public;".to_string()
    } else {
        format!("SET search_path TO {}, public;", quote_ident(schema))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_per_kind() {
        assert_eq!(literal(Kind::Int, Cell::Int(-5)), "-5");
        assert_eq!(literal(Kind::Bool, Cell::Bool(true)), "TRUE");
        assert_eq!(literal(Kind::Text, Cell::Text("o'k\\")), "'o''k\\'");
        assert_eq!(literal(Kind::Text, Cell::Null), "NULL");
        assert_eq!(literal(Kind::Numeric, Cell::Text("12.50")), "12.50");
        assert_eq!(literal(Kind::Numeric, Cell::Text("NaN")), "'NaN'");
        assert_eq!(literal(Kind::Float, Cell::Float(0.5)), "0.5");
        assert_eq!(literal(Kind::Float, Cell::Float(f64::INFINITY)), "'Infinity'");
        assert_eq!(literal(Kind::Float, Cell::Float(1e300)), "1e+300");
        assert_eq!(literal(Kind::Date, Cell::Int(0)), "'2000-01-01'");
        assert_eq!(literal(Kind::TimestampTz, Cell::Int(1_500_000)), "'2000-01-01 00:00:01.5+00'");
        assert_eq!(literal(Kind::Bytes, Cell::Text("\\xdead")), "'\\xdead'");
        assert_eq!(timestamptz_rfc3339(0), "2000-01-01T00:00:00Z");
    }

    #[test]
    fn mysql_literals_and_templates() {
        let my = Dialect::MySql;
        assert_eq!(literal_in(my, Kind::Text, Cell::Text("o'k\\")), "'o''k\\\\'");
        assert_eq!(literal_in(my, Kind::Bytes, Cell::Text("\\xdead")), "X'dead'");
        assert_eq!(value_literal_in(my, Kind::Bytes, &CellValue::text("\\x00ff")), "X'00ff'");
        assert_eq!(value_literal_in(my, Kind::Bytes, &CellValue::text("plain")), "'plain'");
        assert_eq!(value_literal_in(my, Kind::Text, &CellValue::text("a\\b")), "'a\\\\b'");
        assert_eq!(value_literal_in(my, Kind::Int, &CellValue::text("7")), "7");
        assert_eq!(literal_in(my, Kind::Date, Cell::Int(crate::value::date_cell(0, 0, 0))), "'0000-00-00'");
        assert_eq!(select_star_in(my, "shop", "order", Some(10)), "SELECT * FROM shop.`order` LIMIT 10;");
        let cols = vec![col("id", "int unsigned", true, Some(false), Some("AUTO_INCREMENT")), col("Name", "varchar(80)", false, Some(false), None)];
        assert_eq!(insert_template_in(my, "shop", "t", &[]), "INSERT INTO shop.t () VALUES ();");
        assert!(insert_template_in(my, "shop", "t", &cols).starts_with("INSERT INTO shop.t (id, Name)\nVALUES (\n    DEFAULT,"));
        assert_eq!(update_template_in(my, "shop", "t", &cols), "UPDATE shop.t\n   SET Name = ''\n WHERE id = 0;");
        assert_eq!(placeholder("datetime(3)"), "now()");
        assert_eq!(placeholder("bigint unsigned"), "0");
        assert_eq!(placeholder("enum('a','b')"), "''");
        assert!(create_table_template_mysql("shop").contains("AUTO_INCREMENT"));
        assert!(drop_database_template_in(my, "shop").ends_with("DROP DATABASE shop;"));
        assert!(truncate_template_in(my, "s", "t").ends_with("TRUNCATE TABLE s.t;"));
    }

    #[test]
    fn tsql_literals_and_templates() {
        let ms = Dialect::TSql;
        assert_eq!(literal_in(ms, Kind::Text, Cell::Text("Ærø 'x'")), "N'Ærø ''x'''");
        assert_eq!(literal_in(ms, Kind::Bool, Cell::Bool(true)), "1");
        assert_eq!(literal_in(ms, Kind::Bytes, Cell::Text("\\xdead")), "0xdead");
        let ts = value::timestamp_cell(2026, 1, 5, 9, 15, 0, 250_000);
        assert_eq!(literal_in(ms, Kind::Timestamp, Cell::Int(ts)), "N'2026-01-05T09:15:00.25'");
        assert_eq!(literal_in(ms, Kind::TimestampTz, Cell::Int(ts)), "N'2026-01-05T09:15:00.25+00:00'");
        assert_eq!(literal_in(ms, Kind::Date, Cell::Int(value::date_cell(2026, 1, 5))), "N'2026-01-05'");
        assert_eq!(value_literal_in(ms, Kind::Bool, &CellValue::text("false")), "0");
        assert_eq!(value_literal_in(ms, Kind::Bytes, &CellValue::text("\\x00ff")), "0x00ff");
        assert_eq!(value_literal_in(ms, Kind::Timestamp, &CellValue::text("2026-01-05 09:15:00")), "N'2026-01-05T09:15:00'");
        assert_eq!(value_literal_in(ms, Kind::TimestampTz, &CellValue::text("2026-01-05 12:15:00+00")), "N'2026-01-05T12:15:00+00:00'");
        assert_eq!(value_literal_in(ms, Kind::Text, &CellValue::text("東京")), "N'東京'");
        assert_eq!(select_star_in(ms, "dbo", "order", Some(100)), "SELECT TOP (100) * FROM dbo.[order];");
        let cols = vec![
            col("id", "int", true, Some(false), Some("IDENTITY(1,1)")),
            col("name", "nvarchar(80)", false, Some(false), None),
            col("active", "bit", false, Some(false), Some("1")),
            col("created", "datetime2(3)", false, Some(false), None),
            col("row_version", "timestamp", false, Some(false), None),
            col("sub", "decimal(20,2)", false, None, Some("AS ([a]*[b])")),
        ];
        let ins = insert_template_in(ms, "dbo", "customer", &cols);
        assert!(ins.starts_with("INSERT INTO dbo.customer (name, active, created)\nVALUES ("), "{ins}");
        assert!(ins.contains("N'',") && ins.contains("DEFAULT,") && ins.contains("SYSDATETIME()"), "{ins}");
        let upd = update_template_in(ms, "dbo", "customer", &cols);
        assert!(upd.contains("SET name = N''") && !upd.contains("row_version") && !upd.contains("sub =") && upd.ends_with("WHERE id = 0;"), "{upd}");
        assert!(delete_template_in(ms, "dbo", "t", &[col("x", "int", false, None, None)]).contains("WHERE 1 = 0;"));
        assert!(create_table_template_tsql("dbo").contains("IDENTITY(1, 1) PRIMARY KEY"));
        assert!(drop_database_template_in(ms, "shop").contains("SINGLE_USER"));
        assert!(drop_table_template_in(ms, "dbo", "t").ends_with("DROP TABLE dbo.t;"));
        assert_eq!(trigger_toggle_template_in(ms, "dbo", "orders", "trg x", true), "-- [trg x] fires again on dbo.orders. Run it with ⌘↩.\nALTER TABLE dbo.orders ENABLE TRIGGER [trg x];");
        let p = crate::schema::Function {
            oid: 1,
            name: "customer_orders".into(),
            kind: crate::schema::FuncKind::Procedure,
            identity: "@customer int, @total money OUTPUT".into(),
            arg_types: vec!["int".into()],
            result: None,
            returns_set: false,
        };
        assert_eq!(
            function_call_in(ms, "dbo", &p),
            "EXEC dbo.customer_orders\n    @customer = 0,  -- @customer int\n    @total = 0      -- @total money OUTPUT\n;"
        );
        let f = crate::schema::Function { kind: crate::schema::FuncKind::Function, result: Some("decimal(12,2)".into()), ..p };
        assert!(function_call_in(ms, "dbo", &f).starts_with("SELECT dbo.customer_orders(\n    0"));
    }

    #[test]
    fn typed_values() {
        let t = CellValue::text;
        assert_eq!(value_literal(Kind::Int, &t(" 42 ")), "42");
        assert_eq!(value_literal(Kind::Int, &t("4x")), "'4x'");
        assert_eq!(value_literal(Kind::Bool, &t("f")), "FALSE");
        assert_eq!(value_literal(Kind::Float, &t("-1.5e3")), "-1.5e3");
        assert_eq!(value_literal(Kind::Text, &t("42")), "'42'");
        assert_eq!(value_literal(Kind::Json, &CellValue::Null), "NULL");
        assert!(is_decimal(".5") && is_decimal("1.") && !is_decimal(".") && !is_decimal("1e") && !is_decimal("--1"));
        assert_eq!(qualified(Some("Sales"), "orders"), "\"Sales\".orders");
        assert_eq!(qualified(None, "order"), "\"order\"");
    }

    fn col(name: &str, ty: &str, pk: bool, nullable: Option<bool>, default: Option<&str>) -> Column {
        Column { name: name.into(), type_name: ty.into(), nullable, primary_key: pk, default: default.map(str::to_string) }
    }

    fn orders() -> Vec<Column> {
        vec![
            col("id", "bigint", true, Some(false), Some("nextval('orders_id_seq'::regclass)")),
            col("customer", "integer", false, Some(false), None),
            col("total", "numeric(12,2)", false, Some(true), None),
            col("Status", "text", false, None, Some("'new'::text")),
            col("at", "timestamp with time zone", false, None, None),
            col("tags", "text[]", false, None, None),
        ]
    }

    #[test]
    fn select_templates_quote_identifiers() {
        assert_eq!(select_star("public", "orders", Some(1000)), "SELECT * FROM public.orders LIMIT 1000;");
        assert_eq!(select_star("Sales", "order", None), "SELECT * FROM \"Sales\".\"order\";");
        assert_eq!(count_rows("s", "t"), "SELECT count(*) FROM s.t;");
        assert_eq!(
            select_columns("public", "orders", &orders()),
            "SELECT id,\n       customer,\n       total,\n       \"Status\",\n       at,\n       tags\n  FROM public.orders;"
        );
        assert_eq!(select_columns("public", "t", &[]), "SELECT * FROM public.t;");
    }

    #[test]
    fn placeholders_per_type() {
        assert_eq!(placeholder("bigint"), "0");
        assert_eq!(placeholder("numeric(12,2)"), "0");
        assert_eq!(placeholder("character varying(40)"), "''");
        assert_eq!(placeholder("boolean"), "false");
        assert_eq!(placeholder("timestamp with time zone"), "now()");
        assert_eq!(placeholder("date"), "CURRENT_DATE");
        assert_eq!(placeholder("jsonb"), "'{}'");
        assert_eq!(placeholder("integer[]"), "'{}'");
        assert_eq!(placeholder("bytea"), "'\\x'");
        assert_eq!(placeholder("geometry"), "NULL");
    }

    #[test]
    fn insert_uses_default_and_typed_placeholders() {
        let sql = insert_template("public", "orders", &orders());
        let expected = "INSERT INTO public.orders (id, customer, total, \"Status\", at, tags)\nVALUES (\n    DEFAULT,  -- id bigint NOT NULL · default nextval('orders_id_seq'::regclass)\n    0,        -- customer integer NOT NULL\n    0,        -- total numeric(12,2)\n    DEFAULT,  -- Status text · default 'new'::text\n    now(),    -- at timestamp with time zone\n    '{}'      -- tags text[]\n);";
        assert_eq!(sql, expected);
        assert_eq!(insert_template("public", "t", &[]), "INSERT INTO public.t DEFAULT VALUES;");
    }

    #[test]
    fn update_and_delete_use_the_key() {
        let cols = orders();
        assert_eq!(
            update_template("public", "orders", &cols),
            "UPDATE public.orders\n   SET customer = 0,\n       total = 0,\n       \"Status\" = '',\n       at = now(),\n       tags = '{}'\n WHERE id = 0;"
        );
        assert_eq!(delete_template("public", "orders", &cols), "DELETE FROM public.orders\n WHERE id = 0;");
        // A composite key: every part.
        let two = vec![col("a", "int", true, None, None), col("b", "text", true, None, None), col("v", "int", false, None, None)];
        assert_eq!(delete_template("s", "t", &two), "DELETE FROM s.t\n WHERE a = 0\n   AND b = '';");
        // No key reported (LAR): `id` is a guess, said so; nothing like it → WHERE false.
        let lar = vec![col("id", "bigint", false, None, None), col("v", "text", false, None, None)];
        assert!(delete_template("s", "t", &lar).ends_with(" WHERE id = 0;  -- key guessed: the catalog reports no primary key"));
        let none = vec![col("v", "text", false, None, None)];
        let u = update_template("s", "t", &none);
        assert!(u.contains("SET v = ''") && u.contains(" WHERE false;"), "{u}");
    }

    #[test]
    fn destructive_templates_warn_first() {
        for t in [
            drop_database_template("diskman"),
            drop_schema_template("old"),
            truncate_template("public", "orders"),
            drop_table_template("public", "orders"),
            drop_view_template("public", "v", false),
            drop_view_template("public", "mv", true),
        ] {
            assert!(t.starts_with("-- ⚠ "), "{t}");
            assert!(t.contains("Nothing has run"), "{t}");
            let last = t.lines().last().unwrap();
            assert!(!last.starts_with("--") && last.ends_with(';'), "{t}");
        }
        assert!(drop_database_template("x").ends_with("DROP DATABASE x;"));
        assert!(drop_database_template("Prod DB").ends_with("DROP DATABASE \"Prod DB\";"));
        assert!(truncate_template("public", "order").ends_with("TRUNCATE TABLE public.\"order\";"));
        assert!(drop_view_template("s", "mv", true).ends_with("DROP MATERIALIZED VIEW s.mv;"));
    }

    #[test]
    fn creation_templates() {
        assert!(create_schema_template("diskman").ends_with("CREATE SCHEMA new_schema;"));
        let pg = create_table_template("public", false);
        assert!(pg.contains("CREATE TABLE public.new_table (") && pg.contains("GENERATED ALWAYS AS IDENTITY"));
        assert!(!create_table_template("public", true).contains("IDENTITY"));
        assert_eq!(search_path_for("public"), "SET search_path TO public;");
        assert_eq!(search_path_for("Sales"), "SET search_path TO \"Sales\", public;");
        assert_eq!(view_ddl("public", "v", false, " SELECT 1 AS x;"), "CREATE OR REPLACE VIEW public.v AS\nSELECT 1 AS x;");
        assert!(view_ddl("public", "mv", true, "SELECT 1").contains("CREATE MATERIALIZED VIEW public.mv AS\nSELECT 1;"));
    }

    #[test]
    fn object_templates() {
        use crate::schema::{FuncKind, Function};
        // Grants: the picked role, or a name to edit; REVOKE warns first.
        assert!(grant_template("public", Some("orders"), Some("app")).ends_with("GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE public.orders TO app;"));
        assert!(grant_template("Sales", None, None).ends_with("GRANT USAGE ON SCHEMA \"Sales\" TO role_name;"));
        assert!(grant_template("s", Some("t"), Some("PUBLIC")).ends_with("TO PUBLIC;"));
        let r = revoke_template("public", Some("orders"), Some("app"));
        assert!(r.starts_with("-- ⚠ ") && r.contains("Nothing has run") && r.ends_with("REVOKE ALL ON TABLE public.orders FROM app;"), "{r}");
        assert!(revoke_template("s", None, None).ends_with("REVOKE ALL ON SCHEMA s FROM role_name;"));
        // Calls.
        let f = |kind, set, identity: &str, types: &[&str]| Function {
            oid: 1,
            name: "f".into(),
            kind,
            identity: identity.into(),
            arg_types: types.iter().map(|s| s.to_string()).collect(),
            result: None,
            returns_set: set,
        };
        assert_eq!(function_call("s", &f(FuncKind::Function, false, "", &[])), "SELECT s.f();");
        let two = function_call("s", &f(FuncKind::Function, false, "a integer, b text", &["integer", "text"]));
        assert_eq!(two, "SELECT s.f(\n    0,   -- a integer\n    ''   -- b text\n);");
        assert!(function_call("s", &f(FuncKind::Function, true, "", &[])).starts_with("SELECT * FROM s.f("));
        assert!(function_call("s", &f(FuncKind::Procedure, false, "x date", &["date"])).starts_with("CALL s.f(\n    CURRENT_DATE"));
        // Sequences, triggers, roles, extensions.
        let seq = sequence_reset_template("public", "orders_id_seq", Some(("orders", "id")));
        assert!(seq.starts_with("-- ⚠ ") && seq.contains("ALTER SEQUENCE public.orders_id_seq RESTART;"), "{seq}");
        assert!(seq.contains("SELECT setval('public.orders_id_seq', (SELECT coalesce(max(id), 0) + 1 FROM public.orders), false);"), "{seq}");
        assert!(!sequence_reset_template("public", "s", None).contains("setval"));
        assert!(trigger_toggle_template("public", "orders", "audit", false).starts_with("-- ⚠ "));
        assert!(trigger_toggle_template("public", "orders", "audit", false).ends_with("ALTER TABLE public.orders DISABLE TRIGGER audit;"));
        assert!(trigger_toggle_template("public", "orders", "audit", true).ends_with("ALTER TABLE public.orders ENABLE TRIGGER audit;"));
        let alter = alter_role_template("app");
        assert!(alter.lines().skip(1).all(|l| l.starts_with("-- ALTER ROLE app")), "nothing runs as is:\n{alter}");
        assert!(extension_update_template("pg_trgm", Some("1.6")).ends_with("ALTER EXTENSION pg_trgm UPDATE TO '1.6';"));
        assert!(extension_update_template("x", None).ends_with("ALTER EXTENSION x UPDATE;"));
    }
}
