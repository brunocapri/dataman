//! SQL as agents may run it on a connection with rules: tables and schemas hidden from agents
//! are refused, tables with hidden columns are narrowed to a derived table of their visible
//! columns (so `*`, whole rows and functions over them never see the rest), sensitive columns in
//! Masked data can only be selected as they are (and reach the reply under their own names) or
//! compared as they are in filters, and what Agents may do is held.
//!
//! Fail closed: a statement Dataman cannot follow on a connection that keeps something from
//! agents is refused, never run unchecked.

use std::collections::HashSet;

use crate::guard::{self, Effect};
use crate::privacy::{AgentRules, AgentView, Mask, Names};
use crate::sql::Dialect;
use crate::sql::lexer::{Token, TokenKind, tokenize_in};
use crate::sql::scope::{self, Analysis, Clause, Item, Role, SourceKind};

/// A table as the server knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub columns: Vec<String>,
}

pub struct Context<'a> {
    pub rules: &'a AgentRules,
    pub connection: &'a str,
    pub dialect: Dialect,
    /// `(schema as written, name)` → the table it means, when there is one.
    pub tables: &'a dyn Fn(Option<&str>, &str) -> Option<Table>,
    /// User-defined functions and procedures (lower case names).
    pub routines: &'a HashSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// What runs: the agent's text, or its rewrite.
    pub sql: String,
    pub rewritten: bool,
    /// Result columns to mask (Masked data), by name.
    pub masked: Vec<(String, Mask)>,
}

const CATALOG_SCHEMAS: &[&str] = &["information_schema", "pg_catalog", "sys", "mysql", "performance_schema", "pg_toast"];

const DYNAMIC: &[&str] = &[
    "EXECUTE",
    "EXEC",
    "PREPARE",
    "SP_EXECUTESQL",
    "DBLINK",
    "DBLINK_EXEC",
    "OPENQUERY",
    "OPENROWSET",
    "OPENDATASOURCE",
    "LOAD_FILE",
    "PG_READ_FILE",
    "PG_READ_BINARY_FILE",
    "LO_IMPORT",
    "LO_GET",
    "QUERY_TO_XML",
    "QUERY_TO_XMLSCHEMA",
    "QUERY_TO_XML_AND_XMLSCHEMA",
    "CURSOR_TO_XML",
    "CURSOR_TO_XMLSCHEMA",
    "TABLE_TO_XML",
    "TABLE_TO_XMLSCHEMA",
    "TABLE_TO_XML_AND_XMLSCHEMA",
    "SCHEMA_TO_XML",
    "SCHEMA_TO_XMLSCHEMA",
    "SCHEMA_TO_XML_AND_XMLSCHEMA",
    "DATABASE_TO_XML",
    "DATABASE_TO_XMLSCHEMA",
    "DATABASE_TO_XML_AND_XMLSCHEMA",
    "TS_STAT",
    "TS_REWRITE",
    "XPATH_TABLE",
];

/// MySQL `SHOW` that tells nothing about tables, columns or their values.
const MYSQL_SHOW_OK: &[&str] = &["VARIABLES", "WARNINGS", "ERRORS", "COLLATION", "CHARACTER", "CHARSET", "ENGINES", "COUNT"];

/// Operators a sensitive column may stand next to in a filter: comparisons, nothing that
/// computes a new value from it.
const COMPARE: &[&str] = &["=", "==", "<>", "!=", "<", ">", "<=", ">=", "<=>", "~", "~*", "!~", "!~*"];

const ACTIVITY: &[&str] = &[
    "pg_stat_activity",
    "pg_locks",
    "pg_stat_statements",
    "pg_stat_replication",
    "pg_prepared_xacts",
    "pg_stat_ssl",
    "pg_stat_get_activity",
    "pg_blocking_pids",
    "pg_stat_progress_vacuum",
    "processlist",
    "innodb_trx",
    "innodb_lock_waits",
    "sysprocesses",
    "dm_exec_sessions",
    "dm_exec_requests",
    "dm_exec_connections",
    "dm_exec_sql_text",
    "dm_tran_locks",
    "dm_os_waiting_tasks",
    "sp_who",
    "sp_who2",
];

/// The tables a statement names, to be looked up before `check` (schema as written, name).
pub fn tables_named(sql: &str, dialect: Dialect) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    for_each_statement(sql, dialect, |text, _, toks| {
        let a = scope::analyze_in(text, toks, dialect);
        for (i, s) in a.scopes.iter().enumerate() {
            for src in &s.sources {
                if let SourceKind::Table { schema, name } = &src.kind
                    && !(schema.is_none() && a.visible_cte(i, name).is_some())
                    && !out.contains(&(schema.clone(), name.clone()))
                {
                    out.push((schema.clone(), name.clone()));
                }
            }
        }
    });
    out
}

/// In every text the server may run for `sql` (MySQL's `/*! … */` opened, and skipped where a
/// version decides), whose offsets are the agent's.
fn for_each_statement(sql: &str, dialect: Dialect, mut f: impl FnMut(&str, std::ops::Range<usize>, &[Token])) {
    for text in crate::sql::exec_readings(sql, dialect) {
        let tokens = tokenize_in(&text, dialect);
        for s in crate::sql::split_in(&text, dialect) {
            let toks: Vec<Token> = tokens.iter().filter(|t| t.start >= s.range.start && t.end <= s.range.end && !t.kind.is_trivia()).copied().collect();
            if !toks.is_empty() {
                f(&text, s.range.clone(), &toks);
            }
        }
    }
}

fn word(sql: &str, t: &Token) -> Option<String> {
    matches!(t.kind, TokenKind::Ident | TokenKind::Keyword | TokenKind::QuotedIdent).then(|| t.text(sql).trim_matches(['"', '`', '[', ']']).to_ascii_uppercase())
}

fn activity_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ACTIVITY.contains(&n.as_str()) || n.starts_with("dm_exec_") || n.starts_with("events_statements") || n.starts_with("events_waits")
}

pub fn stronger(a: Mask, b: Mask) -> Mask {
    let rank = |m: Mask| match m {
        Mask::Hidden => 2,
        Mask::Hash => 1,
        Mask::Partial => 0,
    };
    if rank(a) >= rank(b) { a } else { b }
}

struct Narrowed {
    table: Table,
    hidden: Vec<String>,
    sensitive: Vec<(String, Mask)>,
}

pub fn check(sql: &str, write: bool, cx: &Context) -> Result<Checked, String> {
    let rules = cx.rules;
    let conn = cx.connection;
    let d = cx.dialect;
    let restricted = !rules.visible.is_open() || (rules.view == AgentView::Masked && rules.sensitive.iter().any(|s| s.on));
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut masked: Vec<(String, Mask)> = Vec::new();
    let mut err: Option<String> = None;
    for_each_statement(sql, d, |text, range, toks| {
        if err.is_some() {
            return;
        }
        if let Err(e) = statement(text, range, toks, write, restricted, cx, &mut edits, &mut masked) {
            err = Some(e);
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    if write && rules.view == AgentView::Masked && (sql.contains('‹') || sql.contains('•')) {
        return Err(format!("this statement carries masked values (‹…›, •); agents cannot write them back on {conn}"));
    }
    if edits.is_empty() {
        return Ok(Checked { sql: sql.to_string(), rewritten: false, masked });
    }
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    edits.dedup();
    let mut out = sql.to_string();
    for (r, text) in edits {
        out.replace_range(r, &text);
    }
    Ok(Checked { sql: out, rewritten: true, masked })
}

#[allow(clippy::too_many_arguments)]
fn statement(
    sql: &str,
    range: std::ops::Range<usize>,
    toks: &[Token],
    write: bool,
    restricted: bool,
    cx: &Context,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
    masked: &mut Vec<(String, Mask)>,
) -> Result<(), String> {
    let (rules, conn, d) = (cx.rules, cx.connection, cx.dialect);
    let text = &sql[range];
    let facts = guard::classify_in(text, d);
    let verb = facts.verb.clone();
    let words: Vec<Option<String>> = toks.iter().map(|t| word(sql, t)).collect();
    let has = |w: &str| words.iter().any(|x| x.as_deref() == Some(w));

    // What agents may do.
    if !rules.structure && facts.effect == Effect::Ddl {
        return Err(format!("changing structure ({verb}) is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    if !rules.code && (matches!(verb.as_str(), "CALL" | "DO" | "EXEC" | "EXECUTE") || has("SP_EXECUTESQL")) {
        return Err(format!("running code ({verb}) is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    if write && rules.view == AgentView::Structure && facts.effect == Effect::Write {
        return Err(format!("{conn} shows agents its structure only: rows are neither read nor written"));
    }
    let a = scope::analyze_in(sql, toks, d);
    if !rules.activity_on() {
        let named = a.scopes.iter().flat_map(|s| &s.sources).filter_map(|s| match &s.kind {
            SourceKind::Table { name, .. } | SourceKind::Function(name) => Some(name.as_str()),
            _ => None,
        });
        let called = a.calls.iter().map(|(_, n, _)| n.as_str());
        let shows = verb == "SHOW" && (has("PROCESSLIST") || has("STATUS") || has("ENGINE"));
        if shows || named.chain(called).any(activity_name) || words.iter().flatten().any(|w| matches!(w.as_str(), "SP_WHO" | "SP_WHO2")) {
            return Err(format!("server activity is off for agents on {conn} (Dataman › Agents › Permissions)"));
        }
    }
    if !restricted {
        return Ok(());
    }

    // Visible to agents, and sensitive columns in Masked data.
    if let Some(e) = table_command(sql, toks, d, verb == "INSERT") {
        return Err(format!("{e} is not available to agents on {conn} while some data is kept from them: write SELECT … FROM, or use list_tables and describe_table"));
    }
    if a.unsure {
        return Err(format!("Dataman could not follow this statement, and {conn} keeps some data from agents: write it more plainly"));
    }
    if let Some(w) = words.iter().flatten().find(|w| DYNAMIC.contains(&w.as_str())) {
        return Err(format!("{w} is not available to agents on {conn} while some data is kept from them"));
    }
    for (_, name, _) in &a.calls {
        if cx.routines.contains(&name.to_lowercase()) {
            return Err(format!("{name}() is a function of your database; agents cannot call your functions on {conn} while some data is kept from them"));
        }
    }
    let names = Names::Qualified;
    let mut narrowed: Vec<(usize, usize, Narrowed)> = Vec::new();
    for (si, s) in a.scopes.iter().enumerate() {
        for (k, src) in s.sources.iter().enumerate() {
            let (schema, name) = match &src.kind {
                SourceKind::Table { schema: None, name } if a.visible_cte(si, name).is_some() => {
                    continue;
                }
                SourceKind::Table { schema, name } => (schema.as_deref(), name.as_str()),
                SourceKind::Function(f) => {
                    if cx.routines.contains(&f.to_lowercase()) {
                        return Err(format!("{f}() is a function of your database; agents cannot call your functions on {conn} while some data is kept from them"));
                    }
                    // `sys.dm_db_stats_histogram(…)` and the like read the catalog and its statistics.
                    let written = sql[src.name_range.clone()].split('(').next().unwrap_or("");
                    let parts: Vec<String> = written.split('.').map(|p| p.trim().trim_matches(['"', '`', '[', ']']).to_lowercase()).collect();
                    let catalog = parts.len() > 1 && CATALOG_SCHEMAS.contains(&parts[parts.len() - 2].as_str());
                    let lower = f.to_lowercase();
                    if catalog || lower.starts_with("pg_") || lower.starts_with("dm_") || lower.starts_with("fn_") {
                        return Err(format!("{f}() reads the system catalog: on {conn} agents list and describe through list_tables, describe_table and search_objects"));
                    }
                    continue;
                }
                _ => continue,
            };
            let lower = name.to_lowercase();
            if schema.is_some_and(|x| CATALOG_SCHEMAS.contains(&x.to_lowercase().as_str()))
                || (schema.is_none() && (lower.starts_with("pg_") || matches!(lower.as_str(), "sqlite_master" | "sqlite_schema" | "sysobjects" | "syscolumns")))
            {
                return Err(format!("{name} is a system catalog: on {conn} agents list and describe through list_tables, describe_table and search_objects"));
            }
            let written = &sql[src.name_range.clone()];
            if dots_outside_quotes(written) >= 2 {
                return Err(format!("{written} names another database: not available to agents on {conn}"));
            }
            let Some(t) = (cx.tables)(schema, name) else {
                return Err(format!("{name} is not a table Dataman knows on {conn}; list_tables shows what agents can use"));
            };
            if !rules.visible.table_visible(Some(&t.schema), &t.name, names) {
                return Err(format!("{} is not visible to agents on {conn}", t.name));
            }
            let hidden: Vec<String> = t.columns.iter().filter(|c| rules.visible.column_hidden(Some(&t.schema), &t.name, c, names)).cloned().collect();
            let sensitive: Vec<(String, Mask)> = t.columns.iter().filter_map(|c| rules.sensitive_mask(Some(&t.schema), &t.name, c, names).map(|m| (c.clone(), m))).collect();
            if !hidden.is_empty() || !sensitive.is_empty() {
                narrowed.push((si, k, Narrowed { table: t, hidden, sensitive }));
            }
        }
    }
    if narrowed.is_empty() {
        return Ok(());
    }
    let eq = |x: &str, y: &str| {
        if a.ci { x.eq_ignore_ascii_case(y) } else { x == y || x.eq_ignore_ascii_case(y) }
    };
    for r in &a.refs {
        for (_, _, n) in &narrowed {
            if let Some(c) = n.hidden.iter().find(|c| eq(c, &r.name)) {
                return Err(format!("{}.{c} is hidden from agents on {conn}", n.table.name));
            }
        }
    }
    for t in &a.targets {
        for (_, _, n) in &narrowed {
            if let Some(c) = n.hidden.iter().find(|c| eq(c, &t.name)) {
                return Err(format!("{}.{c} is hidden from agents on {conn}", n.table.name));
            }
        }
    }
    let any_hidden = narrowed.iter().any(|(_, _, n)| !n.hidden.is_empty());
    let reads = !write || facts.effect == Effect::Read;
    let star_inside = toks.windows(4).any(|w| w[0].text(sql) == "(" && w[2].text(sql) == "." && w[3].text(sql) == "*");
    if any_hidden && !reads {
        let star = toks.windows(2).any(|w| w[0].text(sql).eq_ignore_ascii_case("RETURNING") && w[1].text(sql) == "*") || a.scopes.iter().any(|s| s.items.iter().any(|i| matches!(i, Item::Star(_))));
        if star {
            return Err(format!("name the columns: some columns are hidden from agents on {conn}"));
        }
        if verb == "INSERT" && a.targets.is_empty() {
            return Err(format!("name the columns of this INSERT: some columns are hidden from agents on {conn}"));
        }
        // A write reads the table itself, not a narrowed copy: whole rows (`users::text`,
        // `RETURNING users`) and columns named where Dataman does not follow them (inside
        // `substring(… FROM …)` and the like) would carry the hidden values.
        for (_, _, n) in narrowed.iter().filter(|(_, _, n)| !n.hidden.is_empty()) {
            if star_inside || whole_row(&a, &n.table, &eq) {
                return Err(format!("whole rows of {} are not available to agents on {conn}: some columns are hidden; name the columns", n.table.name));
            }
            if let Some(c) = unfollowed(&a, sql, toks, &n.hidden, &eq) {
                return Err(format!("{}.{c} is hidden from agents on {conn}", n.table.name));
            }
        }
    }
    // A table with hidden columns becomes a derived table of the others, under the same name.
    if any_hidden && reads {
        for (si, k, n) in &narrowed {
            if n.hidden.is_empty() {
                continue;
            }
            let src = &a.scopes[*si].sources[*k];
            let cols: Vec<String> = n.table.columns.iter().filter(|c| !n.hidden.contains(c)).map(|c| d.quote_ident(c)).collect();
            if cols.is_empty() {
                return Err(format!("no column of {} is visible to agents on {conn}", n.table.name));
            }
            let mut text = format!("(SELECT {} FROM {})", cols.join(", "), d.qualified(Some(&n.table.schema), &n.table.name));
            if src.alias.is_none() {
                text.push(' ');
                text.push_str(&d.quote_ident(&n.table.name));
            }
            edits.push((src.name_range.clone(), text));
        }
    }
    if rules.view != AgentView::Masked {
        return Ok(());
    }
    // Masked data: a sensitive column is selected as it is, and comes back masked by its name, or
    // compared as it is in a filter. Its value may not reach the reply under another name (an
    // alias, a scalar subquery, a later UNION branch, a column list, FOR JSON), nor be copied
    // into a table, nor be computed on: a function or a cast would also quote it in its errors.
    let sensitive: Vec<&Narrowed> = narrowed.iter().map(|(_, _, n)| n).filter(|n| !n.sensitive.is_empty()).collect();
    if sensitive.is_empty() {
        return Ok(());
    }
    let calls = call_spans(sql, toks);
    for n in &sensitive {
        if let Some(src) = a.scopes.iter().flat_map(|s| &s.sources).find(|s| s.alias_columns.is_some() && matches!(&s.kind, SourceKind::Table { name, .. } if eq(name, &n.table.name))) {
            return Err(format!("{} is renamed by a column list ({}): masked columns of {} cannot be renamed on {conn}", src.alias.as_deref().unwrap_or(&n.table.name), src.alias_columns.as_ref().map(|c| c.join(", ")).unwrap_or_default(), n.table.name));
        }
        let cols: Vec<String> = n.sensitive.iter().map(|(c, _)| c.clone()).collect();
        for (col, how) in &n.sensitive {
            let label = format!("{}.{col}", n.table.name);
            let refuse = || format!("{label} is masked for agents on {conn}: select it as it is, or compare it as it is in a filter; it cannot be renamed, copied, grouped, sorted or computed");
            if a.refs.iter().any(|r| eq(&r.name, col) && !matches!(r.clause, Clause::Select | Clause::Where | Clause::On)) {
                return Err(refuse());
            }
            for (si, s) in a.scopes.iter().enumerate() {
                let refs = a.refs.iter().filter(|r| r.scope == si && r.clause == Clause::Select && eq(&r.name, col)).count();
                let bare: Vec<usize> = s.items.iter().zip(&s.item_refs).enumerate().filter(|(_, (item, r))| matches!((item, r), (Item::Named(out), Some((_, c))) if eq(out, col) && eq(c, col))).map(|(i, _)| i).collect();
                if refs > bare.len() {
                    return Err(refuse());
                }
                if bare.iter().any(|&i| !reaches(&a, si, i, col, reads, &eq)) {
                    return Err(refuse());
                }
            }
            for r in a.refs.iter().filter(|r| eq(&r.name, col) && matches!(r.clause, Clause::Where | Clause::On)) {
                if !compared(sql, toks, r.range.clone(), &calls) {
                    return Err(refuse());
                }
            }
            match masked.iter_mut().find(|(c, _)| eq(c, col)) {
                Some(m) => m.1 = stronger(m.1, *how),
                None => masked.push((col.clone(), *how)),
            }
        }
        if let Some(c) = unfollowed(&a, sql, toks, &cols, &eq) {
            return Err(format!("{}.{c} is masked for agents on {conn}: select it as it is, or compare it as it is in a filter; it cannot be renamed, copied, grouped, sorted or computed", n.table.name));
        }
        // Whole rows carry every column: `SELECT u`, `row_to_json(u)`, `to_jsonb(u.*)`, and so do
        // the rows of a subquery or a WITH query that selects a masked column.
        if star_inside || whole_row(&a, &n.table, &eq) || any_source_row(&a, &eq) {
            return Err(format!("whole rows of {} are not available to agents in Masked data on {conn}", n.table.name));
        }
    }
    let by_position = positional_order(sql, toks);
    if let Some(w) = by_position {
        return Err(format!("{w} BY a position is not available to agents in Masked data on {conn}: name the columns (masked columns cannot be sorted or grouped)"));
    }
    if d == Dialect::TSql && toks.windows(2).any(|w| w[0].text(sql).eq_ignore_ascii_case("FOR") && ["JSON", "XML"].iter().any(|x| w[1].text(sql).eq_ignore_ascii_case(x))) {
        return Err(format!("FOR JSON and FOR XML are not available to agents in Masked data on {conn}: masked columns come back masked only as columns"));
    }
    Ok(())
}

/// `TABLE t` (a query: PostgreSQL, MySQL) and MySQL's `SHOW`, `DESCRIBE` and `EXPLAIN t`, which
/// read a table or its definition whole, under names Dataman does not narrow.
fn table_command(sql: &str, toks: &[Token], d: Dialect, insert: bool) -> Option<String> {
    let w = |i: usize| toks.get(i).and_then(|t| word(sql, t)).unwrap_or_default();
    for (i, t) in toks.iter().enumerate().filter(|_| d != Dialect::TSql) {
        if word(sql, t).as_deref() != Some("TABLE") || toks.get(i + 1).is_none_or(|n| !matches!(n.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword)) {
            continue;
        }
        let query = match i.checked_sub(1).map(|p| toks[p]) {
            None => true,
            Some(p) if p.kind == TokenKind::Punct => matches!(p.text(sql), "(" | ")"),
            Some(p) if matches!(p.kind, TokenKind::Ident | TokenKind::QuotedIdent) => insert,
            Some(p) => matches!(word(sql, &p).as_deref(), Some("UNION" | "INTERSECT" | "EXCEPT" | "ALL" | "DISTINCT" | "AS")),
        };
        if query {
            return Some(format!("TABLE {}", toks[i + 1].text(sql)));
        }
    }
    // SQLite's and DuckDB's PRAGMAs (and their table functions) read a table's definition whole.
    if matches!(d, Dialect::Sqlite | Dialect::DuckDb) {
        if w(0) == "PRAGMA" {
            return Some("PRAGMA".into());
        }
        if let Some(t) = toks.iter().enumerate().find(|(i, t)| word(sql, t).is_some_and(|x| x.starts_with("PRAGMA_")) && toks.get(i + 1).is_some_and(|n| n.text(sql) == "(")) {
            return Some(format!("{}()", t.1.text(sql)));
        }
    }
    if d == Dialect::DuckDb {
        return match w(0).as_str() {
            "DESCRIBE" | "SUMMARIZE" | "SHOW" => Some(w(0)),
            // FROM-first and PIVOT statements are not narrowed.
            "FROM" | "PIVOT" | "UNPIVOT" => Some(format!("{} first", w(0))),
            _ => None,
        };
    }
    if d != Dialect::MySql {
        return None;
    }
    match w(0).as_str() {
        "DESC" | "DESCRIBE" => Some(w(0)),
        "SHOW" => {
            let k = if matches!(w(1).as_str(), "GLOBAL" | "SESSION") { 2 } else { 1 };
            (!MYSQL_SHOW_OK.contains(&w(k).as_str())).then(|| format!("SHOW {}", w(k)))
        }
        "EXPLAIN" if toks.get(1).is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent)) => Some("EXPLAIN of a table".into()),
        _ => None,
    }
}

/// An unqualified reference that names a source of `t` rather than one of its columns.
fn whole_row(a: &Analysis, t: &Table, eq: &dyn Fn(&str, &str) -> bool) -> bool {
    let src_names: Vec<&str> = a.scopes.iter().flat_map(|s| &s.sources).filter(|s| matches!(&s.kind, SourceKind::Table { name, .. } if eq(name, &t.name))).filter_map(|s| s.visible_name()).collect();
    a.refs.iter().any(|r| r.qualifier.is_none() && src_names.iter().any(|x| eq(x, &r.name)) && !t.columns.iter().any(|c| eq(c, &r.name)))
}

/// A whole row of a subquery, a WITH query or a function: `row_to_json(x) FROM (SELECT …) x`.
fn any_source_row(a: &Analysis, eq: &dyn Fn(&str, &str) -> bool) -> bool {
    let mut names: Vec<&str> = a.scopes.iter().flat_map(|s| &s.sources).filter(|s| !matches!(s.kind, SourceKind::Table { .. })).filter_map(|s| s.visible_name()).collect();
    names.extend(a.ctes.iter().map(|c| c.name.as_str()));
    a.refs.iter().any(|r| r.qualifier.is_none() && names.iter().any(|x| eq(x, &r.name)))
}

/// Whether item `item` of scope `si`, a sensitive column selected as it is, reaches the reply
/// under its own name (or only a filter).
fn reaches(a: &Analysis, si: usize, item: usize, col: &str, reads: bool, eq: &dyn Fn(&str, &str) -> bool) -> bool {
    let s = &a.scopes[si];
    match s.role {
        Role::Root => reads,
        Role::Cte => a.ctes.iter().find(|c| c.scope == Some(si)).is_some_and(|c| c.columns.is_none()),
        Role::Derived => a.scopes.iter().flat_map(|p| &p.sources).find(|x| x.kind == SourceKind::Subquery(si)).is_some_and(|x| x.alias_columns.is_none()),
        Role::Branch(prev) => {
            let mut first = prev;
            while let Role::Branch(p) = a.scopes[first].role {
                first = p;
            }
            matches!(a.scopes[first].items.get(item), Some(Item::Named(n)) if eq(n, col)) && reaches(a, first, item, col, reads, eq)
        }
        Role::Insert => false,
        Role::Expr(c) => matches!(c, Clause::Where | Clause::On),
    }
}

/// Words a parenthesis may follow in a filter without being a call's (or a cast's) arguments.
const FILTER_WORDS: &[&str] = &["WHERE", "ON", "AND", "OR", "NOT", "IN", "WHEN", "THEN", "ELSE", "BETWEEN", "HAVING", "EXISTS", "ANY", "ALL", "SOME", "SELECT", "FROM", "JOIN", "AS", "LATERAL"];

/// The byte spans of argument lists: function calls, and keyword forms such as `CAST(…)`.
fn call_spans(sql: &str, toks: &[Token]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    for i in 1..toks.len() {
        let (p, t) = (toks[i - 1], toks[i]);
        if t.kind != TokenKind::Punct || t.text(sql) != "(" || !matches!(p.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword) {
            continue;
        }
        if word(sql, &p).is_some_and(|w| FILTER_WORDS.contains(&w.as_str())) {
            continue;
        }
        let mut depth = 0;
        for t in &toks[i..] {
            match t.text(sql) {
                "(" if t.kind == TokenKind::Punct => depth += 1,
                ")" if t.kind == TokenKind::Punct => {
                    depth -= 1;
                    if depth == 0 {
                        out.push(toks[i].start..t.end);
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// A column in a filter compared as it is: not inside a function call, a cast or arithmetic.
fn compared(sql: &str, toks: &[Token], r: std::ops::Range<usize>, calls: &[std::ops::Range<usize>]) -> bool {
    if calls.iter().any(|c| c.start < r.start && r.end < c.end) {
        return false;
    }
    let (Some(p), Some(q)) = (toks.iter().position(|t| t.start == r.start), toks.iter().position(|t| t.end == r.end)) else {
        return false;
    };
    let ok = |t: Option<&Token>| match t {
        None => true,
        Some(t) => match t.kind {
            TokenKind::Operator => COMPARE.contains(&t.text(sql)),
            TokenKind::Punct => matches!(t.text(sql), "(" | ")" | "," | ";"),
            TokenKind::Keyword => !["COLLATE", "AT", "DIV", "MOD", "XOR", "MEMBER", "AS"].iter().any(|w| t.text(sql).eq_ignore_ascii_case(w)),
            _ => false,
        },
    };
    ok(p.checked_sub(1).map(|i| &toks[i])) && ok(toks.get(q + 1))
}

/// Identifiers that name one of `cols` where the analysis recorded no reference: inside
/// `substring(x FROM …)`, `JSON_OBJECT(…)`, `extract(…)` and the like.
fn unfollowed(a: &Analysis, sql: &str, toks: &[Token], cols: &[String], eq: &dyn Fn(&str, &str) -> bool) -> Option<String> {
    for (i, t) in toks.iter().enumerate() {
        if !matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent) {
            continue;
        }
        let v = t.text(sql).trim_matches(['"', '`', '[', ']']);
        let Some(c) = cols.iter().find(|c| eq(c, v)) else { continue };
        let known = a.refs.iter().any(|r| r.name_range == t.range())
            || a.targets.iter().any(|x| x.range == t.range())
            || a.refs.iter().any(|r| r.range.start <= t.start && t.end <= r.range.end)
            || i.checked_sub(1).is_some_and(|p| word(sql, &toks[p]).as_deref() == Some("AS"));
        if !known {
            return Some(c.clone());
        }
    }
    None
}

/// `ORDER BY 1` and `GROUP BY 2`: a sort by position names no column.
fn positional_order(sql: &str, toks: &[Token]) -> Option<&'static str> {
    for (i, w) in toks.windows(2).enumerate() {
        let by = word(sql, &w[1]).as_deref() == Some("BY");
        let what = match word(sql, &w[0]).as_deref() {
            Some("ORDER") if by => "ORDER",
            Some("GROUP") if by => "GROUP",
            _ => continue,
        };
        let mut k = i + 2;
        let mut depth = 0i32;
        let mut item_start = true;
        while let Some(t) = toks.get(k) {
            match (t.kind, t.text(sql)) {
                (TokenKind::Punct, "(") => depth += 1,
                (TokenKind::Punct, ")") if depth == 0 => break,
                (TokenKind::Punct, ")") => depth -= 1,
                (TokenKind::Punct, ";") => break,
                (TokenKind::Punct, ",") if depth == 0 => {
                    item_start = true;
                    k += 1;
                    continue;
                }
                (TokenKind::Number, _) if depth == 0 && item_start => return Some(what),
                (TokenKind::Keyword, x) if depth == 0 && ["LIMIT", "OFFSET", "FETCH", "FOR", "HAVING", "WINDOW", "UNION", "INTERSECT", "EXCEPT", "ORDER", "RETURNING"].iter().any(|c| x.eq_ignore_ascii_case(c)) => break,
                _ => {}
            }
            item_start = false;
            k += 1;
        }
    }
    None
}

fn dots_outside_quotes(s: &str) -> usize {
    let mut n = 0;
    let mut q: Option<char> = None;
    for c in s.chars() {
        match (q, c) {
            (None, '"' | '`' | '[') => q = Some(if c == '[' { ']' } else { c }),
            (Some(e), _) if c == e => q = None,
            (None, '.') => n += 1,
            _ => {}
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::{ItemKind, SensitiveCol, Visible, VisibleItem, VisibleMode};

    fn tables(schema: Option<&str>, name: &str) -> Option<Table> {
        let t = |s: &str, n: &str, c: &[&str]| Some(Table { schema: s.into(), name: n.into(), columns: c.iter().map(|x| x.to_string()).collect() });
        match (schema.map(str::to_lowercase).as_deref(), name.to_lowercase().as_str()) {
            (None | Some("public"), "users") => t("public", "users", &["id", "name", "email", "password_hash", "created_at"]),
            (None | Some("public"), "orders") => t("public", "orders", &["id", "user_id", "total", "notes"]),
            (None | Some("public"), "payments") => t("public", "payments", &["id", "card"]),
            (None | Some("public"), "customers") => t("public", "customers", &["id", "name", "email", "ssn", "city"]),
            (Some("audit"), "log") => t("audit", "log", &["id", "what"]),
            _ => None,
        }
    }

    fn rules() -> AgentRules {
        AgentRules {
            visible: Visible {
                mode: VisibleMode::Except,
                items: vec![
                    VisibleItem { name: "public.payments".into(), kind: ItemKind::Table },
                    VisibleItem { name: "audit.*".into(), kind: ItemKind::Schema },
                    VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column },
                ],
            },
            ..Default::default()
        }
    }

    fn run(r: &AgentRules, sql: &str, write: bool) -> Result<Checked, String> {
        let routines: HashSet<String> = ["leak".to_string()].into();
        let cx = Context { rules: r, connection: "prod-pg", dialect: Dialect::Postgres, tables: &tables, routines: &routines };
        check(sql, write, &cx)
    }

    #[test]
    fn open_rules_change_nothing() {
        let r = AgentRules::default();
        for sql in ["SELECT * FROM users", "SELECT password_hash FROM users", "SELECT * FROM pg_catalog.pg_class", "SELECT leak()"] {
            assert_eq!(run(&r, sql, false).unwrap(), Checked { sql: sql.into(), rewritten: false, masked: vec![] }, "{sql}");
        }
    }

    #[test]
    fn hidden_tables_and_schemas_are_refused() {
        let r = rules();
        for sql in [
            "SELECT * FROM payments",
            "SELECT 1 FROM public.payments p",
            "SELECT * FROM audit.log",
            "SELECT * FROM users u JOIN payments p ON p.id = u.id",
            "WITH x AS (SELECT * FROM payments) SELECT * FROM x",
            "SELECT * FROM users WHERE id IN (SELECT id FROM \"payments\")",
        ] {
            let e = run(&r, sql, false).unwrap_err();
            assert!(e.contains("not visible to agents on prod-pg"), "{sql}: {e}");
        }
        assert!(run(&r, "DELETE FROM payments WHERE id = 1", true).unwrap_err().contains("not visible"));
        assert!(run(&r, "INSERT INTO audit.log (what) VALUES ('x')", true).unwrap_err().contains("not visible"));
        let only = AgentRules { visible: Visible { mode: VisibleMode::Only, items: vec![VisibleItem { name: "public.orders".into(), kind: ItemKind::Table }] }, ..Default::default() };
        assert!(run(&only, "SELECT * FROM orders", false).is_ok());
        assert!(run(&only, "SELECT * FROM users", false).unwrap_err().contains("not visible"));
        assert!(run(&only, "SELECT * FROM nowhere", false).unwrap_err().contains("not a table Dataman knows"));
    }

    #[test]
    fn hidden_columns_do_not_exist_for_agents() {
        let r = rules();
        let c = run(&r, "SELECT * FROM users u WHERE u.id = 1", false).unwrap();
        assert!(c.rewritten);
        assert_eq!(c.sql, "SELECT * FROM (SELECT id, name, email, created_at FROM public.users) u WHERE u.id = 1");
        let c = run(&r, "SELECT users.* FROM users", false).unwrap();
        assert_eq!(c.sql, "SELECT users.* FROM (SELECT id, name, email, created_at FROM public.users) users");
        let c = run(&r, "SELECT row_to_json(u) FROM users u", false).unwrap();
        assert!(c.sql.contains("(SELECT id, name, email, created_at FROM public.users) u"), "{}", c.sql);
        for sql in [
            "SELECT upper(password_hash) FROM users LIMIT 5",
            "SELECT id FROM users WHERE password_hash LIKE 'a%'",
            "SELECT u.password_hash FROM users u",
            "SELECT count(*) FROM users GROUP BY password_hash",
            "UPDATE users SET password_hash = 'x' WHERE id = 1",
            "UPDATE users SET name = 'x' WHERE password_hash = 'y'",
        ] {
            let e = run(&r, sql, sql.starts_with("UPDATE")).unwrap_err();
            assert!(e.contains("users.password_hash is hidden from agents"), "{sql}: {e}");
        }
        assert!(run(&r, "DELETE FROM users WHERE id = 1 RETURNING *", true).unwrap_err().contains("name the columns"));
        assert!(run(&r, "INSERT INTO users VALUES (1, 'a', 'b', 'c', now())", true).unwrap_err().contains("name the columns"));
        assert!(!run(&r, "INSERT INTO users (name, email) VALUES ('a', 'b')", true).unwrap().rewritten);
        assert!(run(&r, "UPDATE users SET name = 'x' WHERE id = 1 RETURNING id, name", true).is_ok());
    }

    #[test]
    fn what_cannot_be_followed_is_refused() {
        let r = rules();
        for (sql, want) in [
            ("SELECT * FROM pg_catalog.pg_attribute", "system catalog"),
            ("SELECT * FROM information_schema.columns", "system catalog"),
            ("SELECT * FROM pg_class", "system catalog"),
            ("SELECT leak(1)", "function of your database"),
            ("SELECT * FROM dblink('x', 'select 1') AS t(a int)", "DBLINK"),
            ("SELECT * FROM otherdb.public.users", "another database"),
        ] {
            let e = run(&r, sql, false).unwrap_err();
            assert!(e.contains(want), "{sql}: {e}");
        }
        assert!(run(&r, "SELECT now(), upper('x'), count(*) FROM orders", false).is_ok(), "built-ins stay");
    }

    #[test]
    fn masked_data_selects_and_filters_only() {
        let mut r = AgentRules { view: AgentView::Masked, ..Default::default() };
        r.sensitive.push(SensitiveCol { col: "customers.email".into(), how: Mask::Partial, on: true });
        r.sensitive.push(SensitiveCol { col: "*ssn*".into(), how: Mask::Hash, on: true });
        r.sensitive.push(SensitiveCol { col: "orders.notes".into(), how: Mask::Hidden, on: false });
        let c = run(&r, "SELECT id, name, email, ssn, city FROM customers WHERE city = 'Recife' AND email LIKE 'a%'", false).unwrap();
        assert!(!c.rewritten);
        assert_eq!(c.masked, vec![("email".to_string(), Mask::Partial), ("ssn".to_string(), Mask::Hash)]);
        assert_eq!(run(&r, "SELECT * FROM customers", false).unwrap().masked.len(), 2);
        for sql in [
            "SELECT upper(email) FROM customers",
            "SELECT email AS e FROM customers",
            "SELECT email, upper(email) FROM customers",
            "SELECT ssn, count(*) FROM customers GROUP BY ssn",
            "SELECT id FROM customers ORDER BY email",
            "SELECT c FROM customers c",
            "SELECT row_to_json(c) FROM customers c",
            "SELECT to_jsonb(c.*) FROM customers c",
        ] {
            assert!(run(&r, sql, false).is_err(), "{sql}");
        }
        assert!(run(&r, "SELECT notes, upper(notes) FROM orders", false).is_ok(), "unchecked columns are not masked");
        assert!(run(&r, "UPDATE customers SET email = 'a•••@acme.com' WHERE id = 1", true).unwrap_err().contains("masked values"));
        let data = AgentRules { view: AgentView::Data, ..r.clone() };
        assert!(run(&data, "SELECT upper(email) FROM customers", false).unwrap().masked.is_empty(), "columns mask in Masked data only");
    }

    #[test]
    fn agents_may() {
        let r = AgentRules { structure: false, code: false, activity: false, ..Default::default() };
        assert!(run(&r, "CREATE TABLE t (a int)", true).unwrap_err().contains("changing structure"));
        assert!(run(&r, "DROP TABLE t", true).unwrap_err().contains("changing structure"));
        assert!(run(&r, "CALL p()", true).unwrap_err().contains("running code"));
        assert!(run(&r, "DO $$ BEGIN END $$", true).unwrap_err().contains("running code"));
        assert!(run(&r, "SELECT * FROM pg_stat_activity", false).unwrap_err().contains("server activity"));
        assert!(run(&r, "SELECT pg_blocking_pids(1)", false).unwrap_err().contains("server activity"));
        assert!(run(&r, "INSERT INTO t VALUES (1)", true).is_ok());
        let s = AgentRules { view: AgentView::Structure, ..Default::default() };
        assert!(run(&s, "INSERT INTO t VALUES (1)", true).unwrap_err().contains("structure only"));
        assert!(run(&s, "CREATE INDEX i ON t (a)", true).is_ok());
        let masked = AgentRules { view: AgentView::Masked, ..Default::default() };
        assert!(run(&masked, "SELECT * FROM pg_stat_activity", false).unwrap_err().contains("server activity"), "forced off while data is masked");
    }

    #[test]
    fn named_tables() {
        let mut got = tables_named("WITH x AS (SELECT * FROM a) SELECT * FROM x JOIN s.b ON true; DELETE FROM c WHERE id = 1", Dialect::Postgres);
        got.sort();
        assert_eq!(got, vec![(None, "a".to_string()), (None, "c".to_string()), (Some("s".to_string()), "b".to_string())]);
    }
}
