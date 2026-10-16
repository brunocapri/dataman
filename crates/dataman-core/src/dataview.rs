//! A table opened as data, not as SQL: column filters, a free-form `WHERE`, a server-side sort
//! and a page, turned into one `SELECT` the editor shows (and the user may take over).
//!
//! Paging is `LIMIT … OFFSET …`: it works with any filter and any sort, and the statement stays
//! readable. Without a sort the rows come in primary-key order, so the pages never overlap. The
//! total is estimated from `pg_class.reltuples` at once and counted (`count(*)`) on a backend of
//! its own, cancellable.

use std::sync::Arc;

use anyhow::anyhow;

use crate::Task;
use crate::config::Engine;
use crate::conn::{Connection, first_ok, text_rows};
use crate::result::{ResultSet, SortDir};
use crate::schema::{ms, my};
use crate::sql::Dialect;
use crate::sqlgen::{CellValue, qualified, quote_literal, value_literal_in};
use crate::value::Kind;

/// A filter's comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    /// Case-insensitive substring (`ILIKE '%v%'`).
    Contains,
    /// Case-insensitive prefix (`ILIKE 'v%'`).
    StartsWith,
    IsNull,
    NotNull,
    /// A comma-separated list (`'a, b'` items may be quoted; an unquoted `NULL` matches NULL).
    In,
}

impl Op {
    pub const ALL: [Op; 11] =
        [Op::Eq, Op::Ne, Op::Lt, Op::Gt, Op::Le, Op::Ge, Op::Contains, Op::StartsWith, Op::In, Op::IsNull, Op::NotNull];

    pub fn label(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "≠",
            Op::Lt => "<",
            Op::Gt => ">",
            Op::Le => "≤",
            Op::Ge => "≥",
            Op::Contains => "contains",
            Op::StartsWith => "starts with",
            Op::IsNull => "is null",
            Op::NotNull => "is not null",
            Op::In => "in",
        }
    }

    /// Whether the operator compares with a typed value.
    pub fn takes_value(self) -> bool {
        !matches!(self, Op::IsNull | Op::NotNull)
    }
}

/// One column filter. `kind` is the column's, so numbers and booleans go unquoted.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Filter {
    pub column: String,
    pub kind: Kind,
    pub op: Op,
    pub value: String,
}

/// A typed value without one pair of surrounding single quotes (`'paid'` → `paid`, doubled
/// quotes inside undone): what the user meant when they quoted it themselves.
pub fn unquote(v: &str) -> String {
    let t = v.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return t[1..t.len() - 1].replace("''", "'");
    }
    v.to_string()
}

/// Items of an `IN` list: comma-separated, trimmed; `'…'` or `"…"` keep commas inside. Each item
/// is `Some(text)`, or `None` for an unquoted `NULL`.
pub fn parse_list(s: &str) -> Vec<Option<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = s.chars().peekable();
    let push = |cur: &mut String, quoted: &mut bool, out: &mut Vec<Option<String>>| {
        let t = cur.trim();
        if *quoted {
            out.push(Some(cur.clone()));
        } else if t.eq_ignore_ascii_case("null") {
            out.push(None);
        } else if !t.is_empty() {
            out.push(Some(t.to_string()));
        }
        cur.clear();
        *quoted = false;
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' if cur.trim().is_empty() && !quoted => {
                // A quoted item: up to the closing quote (doubled quotes are one).
                cur.clear();
                quoted = true;
                while let Some(d) = chars.next() {
                    if d == c {
                        if chars.peek() == Some(&c) {
                            chars.next();
                            cur.push(c);
                            continue;
                        }
                        break;
                    }
                    cur.push(d);
                }
                // Anything after the closing quote up to the comma is dropped.
                while let Some(&d) = chars.peek() {
                    if d == ',' {
                        break;
                    }
                    chars.next();
                }
            }
            ',' => push(&mut cur, &mut quoted, &mut out),
            c => cur.push(c),
        }
    }
    push(&mut cur, &mut quoted, &mut out);
    out
}

/// `%`, `_` and `\` escaped for a LIKE pattern (backslash is LIKE's default escape). T-SQL's
/// `[` (a character class there) too; its LIKE has no default escape: the caller adds
/// `ESCAPE '\'`.
fn like_escape(s: &str, d: Dialect) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') || (c == '[' && d == Dialect::TSql) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Filter {
    pub fn new(column: impl Into<String>, kind: Kind, op: Op, value: impl Into<String>) -> Filter {
        Filter { column: column.into(), kind, op, value: value.into() }
    }

    /// The condition, its column and values quoted (PostgreSQL).
    pub fn sql(&self) -> String {
        self.sql_in(Dialect::Postgres)
    }

    /// The condition in `d`.
    pub fn sql_in(&self, d: Dialect) -> String {
        let col = d.quote_ident(&self.column);
        let v = unquote(&self.value);
        // MySQL compares JSON with JSON only: the typed value is cast.
        let my_json = d == Dialect::MySql && self.kind == Kind::Json;
        let lit = |s: &str| {
            let l = value_literal_in(d, self.kind, &CellValue::text(s));
            if my_json { format!("CAST({l} AS JSON)") } else { l }
        };
        // PostgreSQL's json has no equality operator: compared as jsonb.
        let lhs = if d == Dialect::Postgres && self.kind == Kind::Json && matches!(self.op, Op::Eq | Op::Ne | Op::In) {
            format!("{col}::jsonb")
        } else {
            col.clone()
        };
        let is_null_word = self.value.trim().eq_ignore_ascii_case("null");
        match self.op {
            Op::Eq if is_null_word => format!("{col} IS NULL"),
            Op::Ne if is_null_word => format!("{col} IS NOT NULL"),
            Op::Eq => format!("{lhs} = {}", lit(&v)),
            Op::Ne => format!("{lhs} <> {}", lit(&v)),
            Op::Lt => format!("{col} < {}", lit(&v)),
            Op::Gt => format!("{col} > {}", lit(&v)),
            Op::Le => format!("{col} <= {}", lit(&v)),
            Op::Ge => format!("{col} >= {}", lit(&v)),
            Op::Contains | Op::StartsWith => {
                // Any type matches on its text (numbers, uuids, json…).
                let text = if self.kind == Kind::Text { col } else { d.text_cast(&col) };
                let pat = like_escape(&v, d);
                let pat = if self.op == Op::Contains { format!("%{pat}%") } else { format!("{pat}%") };
                let escape = if d == Dialect::TSql { " ESCAPE '\\'" } else { "" };
                format!("{text} {} {}{escape}", d.ilike(), d.quote_literal(&pat))
            }
            Op::IsNull => format!("{col} IS NULL"),
            Op::NotNull => format!("{col} IS NOT NULL"),
            Op::In => {
                let items = parse_list(&self.value);
                let null = items.iter().any(Option::is_none);
                let vals: Vec<String> = items.into_iter().flatten().map(|s| lit(&s)).collect();
                match (vals.is_empty(), null) {
                    (true, false) if d == Dialect::TSql => "1 = 0".into(),
                    (true, false) => "false".into(),
                    (true, true) => format!("{col} IS NULL"),
                    (false, false) => format!("{lhs} IN ({})", vals.join(", ")),
                    (false, true) => format!("({lhs} IN ({}) OR {col} IS NULL)", vals.join(", ")),
                }
            }
        }
    }

    /// `status = paid`, `total > 100`, `name is null`: the chip text.
    pub fn label(&self) -> String {
        let v: String = {
            let s = self.value.trim();
            if s.chars().count() > 24 { format!("{}…", s.chars().take(23).collect::<String>()) } else { s.to_string() }
        };
        if self.op.takes_value() { format!("{} {} {v}", self.column, self.op.label()) } else { format!("{} {}", self.column, self.op.label()) }
    }
}

/// A table's data view: what it shows and which page.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TableView {
    pub schema: String,
    pub table: String,
    pub filters: Vec<Filter>,
    /// Free-form condition (power users); empty for none.
    pub where_raw: String,
    /// Server-side sort (a header click).
    pub sort: Option<(String, SortDir)>,
    /// The order without a sort (the primary key), so pages never overlap.
    pub key: Vec<String>,
    pub page: u64,
    pub page_size: u64,
    /// The statements' dialect (PostgreSQL unless set).
    pub dialect: Dialect,
}

impl TableView {
    pub fn new(schema: impl Into<String>, table: impl Into<String>, key: Vec<String>, page_size: u64) -> TableView {
        TableView {
            schema: schema.into(),
            table: table.into(),
            filters: Vec::new(),
            where_raw: String::new(),
            sort: None,
            key,
            page: 0,
            page_size: page_size.max(1),
            dialect: Dialect::Postgres,
        }
    }

    pub fn relation(&self) -> String {
        self.dialect.qualified(Some(&self.schema), &self.table)
    }

    /// The conditions, one per line item (the free-form one in parentheses when not alone).
    pub fn conditions(&self) -> Vec<String> {
        let mut v: Vec<String> = self.filters.iter().map(|f| f.sql_in(self.dialect)).collect();
        let raw = self.where_raw.trim().trim_end_matches(';').trim();
        if !raw.is_empty() {
            v.push(if v.is_empty() { raw.to_string() } else { format!("({raw})") });
        }
        v
    }

    pub fn is_filtered(&self) -> bool {
        !self.conditions().is_empty()
    }

    pub fn offset(&self) -> u64 {
        self.page * self.page_size
    }

    fn where_lines(&self, out: &mut String) {
        for (i, c) in self.conditions().iter().enumerate() {
            out.push_str(if i == 0 { "\n WHERE " } else { "\n   AND " });
            out.push_str(c);
        }
    }

    /// The page's statement:
    ///
    /// ```sql
    /// SELECT * FROM public.orders
    ///  WHERE status = 'paid'
    ///  ORDER BY total DESC
    ///  LIMIT 1000 OFFSET 2000;
    /// ```
    pub fn sql(&self) -> String {
        let mut s = format!("SELECT * FROM {}", self.relation());
        self.where_lines(&mut s);
        let d = self.dialect;
        let mut order: Vec<String> = match &self.sort {
            Some((c, dir)) => vec![format!("{}{}", d.quote_ident(c), if *dir == SortDir::Desc { " DESC" } else { "" })],
            None => self.key.iter().map(|k| d.quote_ident(k)).collect(),
        };
        // T-SQL's OFFSET … FETCH needs an ORDER BY: a table without a key is paged as it comes.
        if order.is_empty() && d == Dialect::TSql {
            order.push("(SELECT NULL)".into());
        }
        if !order.is_empty() {
            s.push_str("\n ORDER BY ");
            s.push_str(&order.join(", "));
        }
        s.push_str("\n ");
        s.push_str(&d.page_clause(self.page_size, self.offset()));
        s.push(';');
        s
    }

    /// The total under the same conditions (`COUNT_BIG` in T-SQL, whose `count(*)` is an int).
    pub fn count_sql(&self) -> String {
        let count = if self.dialect == Dialect::TSql { "COUNT_BIG(*)" } else { "count(*)" };
        let mut s = format!("SELECT {count} FROM {}", self.relation());
        self.where_lines(&mut s);
        s
    }

    /// The last page for `total` rows (0-based).
    pub fn last_page(&self, total: u64) -> u64 {
        total.saturating_sub(1) / self.page_size
    }
}

/// A `count(*)` running on its own backend: its result, and the handle whose drop cancels the
/// statement on the server.
pub struct RowCount {
    pub task: Task<u64>,
    pub stop: CountStop,
}

/// Dropping it cancels the count (a CancelRequest to its backend).
pub struct CountStop(#[allow(dead_code)] tokio::sync::oneshot::Sender<()>);

impl Connection {
    /// The planner's row estimate of a table (`pg_class.reltuples`); `None` when the table was
    /// never analyzed or the catalog does not say.
    pub fn estimate_rows(self: &Arc<Self>, db: &str, schema: &str, table: &str) -> Task<Option<u64>> {
        let (this, db) = (self.clone(), db.to_string());
        let rel = quote_literal(&qualified(Some(schema), table));
        let my = match self.engine() {
            // InnoDB's sampled estimate (`ANALYZE TABLE` refreshes it); views have none.
            Engine::MySql => Some(format!(
                "SELECT TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND TABLE_TYPE = 'BASE TABLE'",
                my::lit(schema),
                my::lit(table)
            )),
            // The rows of the heap or clustered index, as the storage engine counts them.
            Engine::SqlServer => Some(format!(
                "SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = {} AND p.index_id IN (0, 1)",
                ms::object_id(schema, table)
            )),
            Engine::Postgres => None,
        };
        self.spawn(async move {
            let client = this.meta(&db).await?;
            let q = my.unwrap_or_else(|| format!("SELECT reltuples::bigint FROM pg_catalog.pg_class WHERE oid = {rel}::regclass"));
            let rows = first_ok(&client, &[&q]).await?;
            let n = rows.first().and_then(|r| r.first().cloned().flatten()).and_then(|s| s.parse::<i64>().ok());
            Ok(n.filter(|n| *n >= 0).map(|n| n as u64))
        })
    }

    /// `sql` (a `SELECT count(*) …`) on a backend of its own: a long count never holds up the
    /// catalog queries or the tab's session. Drop the handle to cancel it.
    pub fn count_rows(self: &Arc<Self>, db: &str, sql: String) -> RowCount {
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let (this, db) = (self.clone(), db.to_string());
        let task = self.spawn(async move {
            let d = this.dedicated(Some(&db)).await?;
            let client = d.db();
            let q = text_rows(&client, &sql);
            tokio::pin!(q);
            tokio::select! {
                r = &mut q => {
                    let rows = r?;
                    let n = rows.iter().find_map(|r| r.first().cloned().flatten().and_then(|s| s.parse::<u64>().ok()));
                    n.ok_or_else(|| anyhow!("count(*) returned no row"))
                }
                _ = &mut rx => {
                    let _ = d.cancel().await;
                    Err(anyhow!("count cancelled"))
                }
            }
        });
        RowCount { task, stop: CountStop(tx) }
    }

    /// The primary key's columns of a table, in key order (empty without one, or when the
    /// catalog does not say).
    pub fn primary_key(self: &Arc<Self>, db: &str, schema: &str, table: &str) -> Task<Vec<String>> {
        let (this, db) = (self.clone(), db.to_string());
        let rel = quote_literal(&qualified(Some(schema), table));
        let other = match self.engine() {
            Engine::MySql => Some(format!(
                "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} \
                 AND CONSTRAINT_NAME = 'PRIMARY' ORDER BY ORDINAL_POSITION",
                my::lit(schema),
                my::lit(table)
            )),
            Engine::SqlServer => Some(format!(
                "SELECT c.name FROM sys.indexes i \
                   JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                   JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                  WHERE i.object_id = {} AND i.is_primary_key = 1 AND ic.is_included_column = 0 ORDER BY ic.key_ordinal",
                ms::object_id(schema, table)
            )),
            Engine::Postgres => None,
        };
        if let Some(q) = other {
            return self.spawn(async move {
                let client = this.meta(&db).await?;
                Ok(first_ok(&client, &[&q]).await.map(|rows| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()).unwrap_or_default())
            });
        }
        self.spawn(async move {
            let client = this.meta(&db).await?;
            let base = format!(
                "SELECT a.attname FROM pg_catalog.pg_index i JOIN pg_catalog.pg_attribute a \
                 ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey) \
                 WHERE i.indrelid = {rel}::regclass AND i.indisprimary"
            );
            let ordered = format!("{base} ORDER BY array_position(i.indkey::int2[], a.attnum)");
            Ok(match first_ok(&client, &[&ordered, &base]).await {
                Ok(rows) => rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect(),
                Err(_) => Vec::new(),
            })
        })
    }

    /// The labels of an enum type, in order (empty for any other type). Cached per database.
    pub fn enum_labels(self: &Arc<Self>, db: &str, type_oid: u32) -> Task<Arc<Vec<String>>> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            let key = ("enum", db.clone(), type_oid.to_string());
            if let Some(v) = this.caches.lock().get(&key).cloned()
                && let Ok(v) = v.downcast::<Vec<String>>()
            {
                return Ok(v);
            }
            if type_oid == 0 {
                return Ok(Arc::new(Vec::new()));
            }
            let client = this.meta(&db).await?;
            let q = format!("SELECT enumlabel FROM pg_catalog.pg_enum WHERE enumtypid = {type_oid} ORDER BY enumsortorder");
            let labels: Vec<String> = match first_ok(&client, &[&q]).await {
                Ok(rows) => rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect(),
                Err(_) => Vec::new(),
            };
            let labels = Arc::new(labels);
            this.caches.lock().insert(key, labels.clone());
            Ok(labels)
        })
    }

    /// The enum labels of result column `col`, in order (empty when it is not an enum):
    /// PostgreSQL's enum type, or MySQL's `ENUM('…')` column the result shows.
    pub fn result_enum_labels(self: &Arc<Self>, db: &str, rs: &ResultSet, col: usize) -> Task<Arc<Vec<String>>> {
        let Some(info) = rs.columns().and_then(|c| c.get(col).cloned()) else { return self.spawn(async { Ok(Arc::new(Vec::new())) }) };
        if self.engine() != Engine::MySql {
            return self.enum_labels(db, info.type_oid);
        }
        let origin = rs.named_origins().and_then(|o| o.get(col).cloned().flatten());
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            let Some(o) = origin.filter(|_| info.type_name == "enum") else { return Ok(Arc::new(Vec::new())) };
            let key = ("enum", db.clone(), format!("{}.{}.{}", o.schema, o.table, o.column));
            if let Some(v) = this.caches.lock().get(&key).cloned()
                && let Ok(v) = v.downcast::<Vec<String>>()
            {
                return Ok(v);
            }
            let client = this.meta(&db).await?;
            let q = format!(
                "SELECT COLUMN_TYPE FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND COLUMN_NAME = {}",
                my::lit(&o.schema),
                my::lit(&o.table),
                my::lit(&o.column)
            );
            let ty = first_ok(&client, &[&q]).await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next().flatten());
            let labels = Arc::new(ty.map(|t| mysql_enum_labels(&t)).unwrap_or_default());
            this.caches.lock().insert(key, labels.clone());
            Ok(labels)
        })
    }
}

/// `enum('a','it''s','b\\c')` → `a`, `it's`, `b\c` (MySQL's `COLUMN_TYPE`).
pub fn mysql_enum_labels(column_type: &str) -> Vec<String> {
    let t = column_type.trim();
    let Some(inner) = t.strip_prefix("enum(").or_else(|| t.strip_prefix("ENUM(")).and_then(|s| s.strip_suffix(')')) else { return Vec::new() };
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut s = String::new();
        while let Some(d) = chars.next() {
            match d {
                '\'' if chars.peek() == Some(&'\'') => {
                    chars.next();
                    s.push('\'');
                }
                '\'' => break,
                '\\' => {
                    if let Some(e) = chars.next() {
                        s.push(e);
                    }
                }
                d => s.push(d),
            }
        }
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(col: &str, kind: Kind, op: Op, v: &str) -> String {
        Filter::new(col, kind, op, v).sql()
    }

    #[test]
    fn filters_quote_columns_and_values() {
        assert_eq!(f("status", Kind::Text, Op::Eq, "paid"), "status = 'paid'");
        assert_eq!(f("status", Kind::Text, Op::Eq, "'paid'"), "status = 'paid'", "quotes typed by the user");
        assert_eq!(f("name", Kind::Text, Op::Eq, "O'Brien"), "name = 'O''Brien'");
        assert_eq!(f("Status", Kind::Text, Op::Ne, "x"), "\"Status\" <> 'x'");
        assert_eq!(f("order", Kind::Int, Op::Gt, "10"), "\"order\" > 10");
        assert_eq!(f("total", Kind::Numeric, Op::Le, " 1.5 "), "total <= 1.5");
        assert_eq!(f("total", Kind::Numeric, Op::Ge, "abc"), "total >= 'abc'", "not a number: the server says so");
        assert_eq!(f("id", Kind::Int, Op::Lt, "-3"), "id < -3");
        assert_eq!(f("active", Kind::Bool, Op::Eq, "true"), "active = TRUE");
        assert_eq!(f("active", Kind::Bool, Op::Eq, "f"), "active = FALSE");
        assert_eq!(f("at", Kind::TimestampTz, Op::Gt, "2024-01-01"), "at > '2024-01-01'");
        assert_eq!(f("doc", Kind::Json, Op::Eq, "{\"a\": 1}"), "doc::jsonb = '{\"a\": 1}'");
    }

    #[test]
    fn null_handling() {
        assert_eq!(f("x", Kind::Text, Op::IsNull, ""), "x IS NULL");
        assert_eq!(f("x", Kind::Text, Op::NotNull, "ignored"), "x IS NOT NULL");
        assert_eq!(f("x", Kind::Int, Op::Eq, "NULL"), "x IS NULL", "= NULL means IS NULL");
        assert_eq!(f("x", Kind::Int, Op::Ne, "null"), "x IS NOT NULL");
        assert_eq!(f("x", Kind::Text, Op::Eq, "'NULL'"), "x = 'NULL'", "a quoted NULL is the text");
        assert_eq!(f("x", Kind::Text, Op::Eq, ""), "x = ''", "empty is not NULL");
    }

    #[test]
    fn like_patterns_are_escaped() {
        assert_eq!(f("name", Kind::Text, Op::Contains, "ana"), "name ILIKE '%ana%'");
        assert_eq!(f("name", Kind::Text, Op::StartsWith, "50%_off\\"), "name ILIKE '50\\%\\_off\\\\%'");
        assert_eq!(f("id", Kind::Int, Op::Contains, "42"), "id::text ILIKE '%42%'");
        assert_eq!(f("name", Kind::Text, Op::Contains, "d'or"), "name ILIKE '%d''or%'");
    }

    #[test]
    fn in_lists() {
        assert_eq!(parse_list("1, 2,3"), vec![Some("1".into()), Some("2".into()), Some("3".into())]);
        assert_eq!(parse_list("'a, b', \"c\" , NULL, ''"), vec![Some("a, b".into()), Some("c".into()), None, Some(String::new())]);
        assert_eq!(parse_list("'it''s'"), vec![Some("it's".into())]);
        assert_eq!(parse_list(" , ,"), Vec::<Option<String>>::new());
        assert_eq!(f("id", Kind::Int, Op::In, "1, 2, 3"), "id IN (1, 2, 3)");
        assert_eq!(f("s", Kind::Text, Op::In, "paid, 'in transit'"), "s IN ('paid', 'in transit')");
        assert_eq!(f("s", Kind::Text, Op::In, "paid, null"), "(s IN ('paid') OR s IS NULL)");
        assert_eq!(f("s", Kind::Text, Op::In, "NULL"), "s IS NULL");
        assert_eq!(f("s", Kind::Text, Op::In, ""), "false", "an empty list matches nothing");
    }

    #[test]
    fn labels() {
        assert_eq!(Filter::new("status", Kind::Text, Op::Eq, "paid").label(), "status = paid");
        assert_eq!(Filter::new("x", Kind::Text, Op::IsNull, "").label(), "x is null");
        assert!(Filter::new("x", Kind::Text, Op::Contains, "a".repeat(40)).label().ends_with('…'));
    }

    #[test]
    fn mysql_filters_and_pages() {
        let m = |col: &str, kind: Kind, op: Op, v: &str| Filter::new(col, kind, op, v).sql_in(Dialect::MySql);
        assert_eq!(m("Status", Kind::Text, Op::Eq, "a\\b"), "Status = 'a\\\\b'");
        assert_eq!(m("order", Kind::Int, Op::Gt, "10"), "`order` > 10");
        assert_eq!(m("name", Kind::Text, Op::Contains, "50%"), "name LIKE '%50\\\\%%'");
        assert_eq!(m("id", Kind::Int, Op::StartsWith, "4"), "CAST(id AS CHAR) LIKE '4%'");
        assert_eq!(m("profile", Kind::Json, Op::Eq, "{\"a\": 1}"), "profile = CAST('{\"a\": 1}' AS JSON)");
        assert_eq!(m("photo", Kind::Bytes, Op::Eq, "\\x00ff"), "photo = X'00ff'");
        let mut v = TableView::new("shop", "orders", vec!["id".into()], 100);
        v.dialect = Dialect::MySql;
        v.page = 3;
        v.sort = Some(("total".into(), SortDir::Desc));
        assert_eq!(v.sql(), "SELECT * FROM shop.orders\n ORDER BY total DESC\n LIMIT 100 OFFSET 300;");
        assert_eq!(mysql_enum_labels("enum('bronze','it''s','a\\\\b')"), vec!["bronze", "it's", "a\\b"]);
        assert!(mysql_enum_labels("varchar(10)").is_empty());
    }

    #[test]
    fn tsql_filters_and_pages() {
        let m = |col: &str, kind: Kind, op: Op, v: &str| Filter::new(col, kind, op, v).sql_in(Dialect::TSql);
        assert_eq!(m("name", Kind::Text, Op::Eq, "Ærø"), "name = N'Ærø'");
        assert_eq!(m("order", Kind::Int, Op::Gt, "10"), "[order] > 10");
        assert_eq!(m("active", Kind::Bool, Op::Eq, "true"), "active = 1");
        assert_eq!(m("name", Kind::Text, Op::Contains, "50%[x]"), "name LIKE N'%50\\%\\[x]%' ESCAPE '\\'");
        assert_eq!(m("id", Kind::Int, Op::StartsWith, "4"), "CAST(id AS NVARCHAR(MAX)) LIKE N'4%' ESCAPE '\\'");
        assert_eq!(m("photo", Kind::Bytes, Op::Eq, "\\x00ff"), "photo = 0x00ff");
        assert_eq!(m("s", Kind::Text, Op::In, ""), "1 = 0");
        let mut v = TableView::new("dbo", "orders", vec!["id".into()], 100);
        v.dialect = Dialect::TSql;
        v.page = 3;
        assert_eq!(v.sql(), "SELECT * FROM dbo.orders\n ORDER BY id\n OFFSET 300 ROWS FETCH NEXT 100 ROWS ONLY;");
        assert_eq!(v.count_sql(), "SELECT COUNT_BIG(*) FROM dbo.orders");
        // Without a key or a sort, OFFSET still needs an ORDER BY.
        let mut h = TableView::new("sales", "event", vec![], 50);
        h.dialect = Dialect::TSql;
        assert_eq!(h.sql(), "SELECT * FROM sales.event\n ORDER BY (SELECT NULL)\n OFFSET 0 ROWS FETCH NEXT 50 ROWS ONLY;");
    }

    #[test]
    fn paging_sql() {
        let mut v = TableView::new("public", "orders", vec!["id".into()], 1000);
        assert_eq!(v.sql(), "SELECT * FROM public.orders\n ORDER BY id\n LIMIT 1000;");
        assert_eq!(v.count_sql(), "SELECT count(*) FROM public.orders");
        assert!(!v.is_filtered());
        v.page = 2;
        assert_eq!(v.offset(), 2000);
        assert_eq!(v.sql(), "SELECT * FROM public.orders\n ORDER BY id\n LIMIT 1000 OFFSET 2000;");
        v.filters.push(Filter::new("status", Kind::Text, Op::Eq, "paid"));
        v.where_raw = "total > 100 OR customer = 7;".into();
        v.sort = Some(("total".into(), SortDir::Desc));
        v.page = 0;
        assert_eq!(
            v.sql(),
            "SELECT * FROM public.orders\n WHERE status = 'paid'\n   AND (total > 100 OR customer = 7)\n ORDER BY total DESC\n LIMIT 1000;"
        );
        assert_eq!(v.count_sql(), "SELECT count(*) FROM public.orders\n WHERE status = 'paid'\n   AND (total > 100 OR customer = 7)");
        // The free-form condition alone goes in as typed.
        v.filters.clear();
        v.sort = Some(("Total".into(), SortDir::Asc));
        assert_eq!(v.sql(), "SELECT * FROM public.orders\n WHERE total > 100 OR customer = 7\n ORDER BY \"Total\"\n LIMIT 1000;");
        // No key, no sort: no ORDER BY; quoted names.
        let w = TableView::new("Sales", "order", vec![], 50);
        assert_eq!(w.sql(), "SELECT * FROM \"Sales\".\"order\"\n LIMIT 50;");
        let k = TableView::new("s", "t", vec!["a".into(), "b".into()], 10);
        assert!(k.sql().contains("ORDER BY a, b"));
        assert_eq!(k.last_page(0), 0);
        assert_eq!(k.last_page(10), 0);
        assert_eq!(k.last_page(11), 1);
    }
}
