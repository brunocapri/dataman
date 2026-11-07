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
    /// The agent login's views: reads of these tables go there (their masked columns come back
    /// masked by the server; the login can't read the table's).
    pub views: &'a [crate::agent_login::ViewMap],
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// What runs: the agent's text, or its rewrite.
    pub sql: String,
    pub rewritten: bool,
    /// Result columns to mask (Masked data), by name.
    pub masked: Vec<(String, Mask)>,
}

/// A statement that acts on the tables it names without a query to read them from.
fn names_its_tables(f: &guard::Facts) -> bool {
    f.effect == Effect::Ddl || matches!(f.verb.as_str(), "TRUNCATE" | "OPTIMIZE" | "CHECK")
}

const CATALOG_SCHEMAS: &[&str] = &["information_schema", "pg_catalog", "sys", "mysql", "performance_schema", "pg_toast"];

/// Statements that run SQL from a string; the functions that do, or read tables and files
/// whole, are `functions::READS_AROUND` and `STRING_SQL`.
const DYNAMIC: &[&str] = &["EXECUTE", "EXEC", "PREPARE"];

fn dynamic(word: &str) -> bool {
    DYNAMIC.contains(&word) || crate::sql::functions::does(word).has(crate::sql::functions::READS_AROUND | crate::sql::functions::STRING_SQL)
}

/// MySQL `SHOW` that tells nothing about tables, columns or their values. Not WARNINGS or ERRORS:
/// they quote the values a cast choked on, and the plan with the values of the rows it read.
const MYSQL_SHOW_OK: &[&str] = &["VARIABLES", "COLLATION", "CHARACTER", "CHARSET", "ENGINES"];

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

/// What is neither a read nor a write and returns no rows. Any other such verb may (`EXECUTE` of a
/// prepared read, `WATCH`, T-SQL's `DBCC` and `READTEXT`).
const NO_ROWS: &[&str] = &["SET", "RESET", "USE", "PREPARE", "DEALLOCATE", "ANALYZE", "VACUUM", "CHECK", "DISCARD", "LISTEN", "UNLISTEN", "NOTIFY", "WAITFOR"];

/// ClickHouse settings that change what a name in the text means: without them a WITH query stops
/// reaching subqueries and later branches, or later queries of its clause.
const CLICKHOUSE_NAME_SETTINGS: &[&str] = &["ENABLE_GLOBAL_WITH_STATEMENT", "ENABLE_ANALYZER", "ALLOW_EXPERIMENTAL_ANALYZER"];

/// The functions that are `IN` by name: their second argument may be a table.
const CLICKHOUSE_IN_FUNCTIONS: &[&str] = &[
    "NOTIN", "NULLIN", "NOTNULLIN", "GLOBALIN", "GLOBALNOTIN", "GLOBALNULLIN", "GLOBALNOTNULLIN", "INIGNORESET", "NOTINIGNORESET", "NULLINIGNORESET", "NOTNULLINIGNORESET",
    "GLOBALINIGNORESET", "GLOBALNOTINIGNORESET", "GLOBALNULLINIGNORESET", "GLOBALNOTNULLINIGNORESET",
];

/// `FINAL`, `SAMPLE 1/10 OFFSET 1/2` or both, after a table that ends at `end` and its alias.
fn clickhouse_table_modifiers(sql: &str, toks: &[Token], end: usize) -> Option<std::ops::Range<usize>> {
    let is = |i: usize, w: &str| toks.get(i).and_then(|t| word(sql, t, Dialect::ClickHouse)).is_some_and(|x| x == w);
    let mut i = toks.iter().position(|t| t.start >= end)?;
    if is(i, "AS") {
        i += 1;
    }
    if !is(i, "FINAL") && !is(i, "SAMPLE") {
        i += 1;
    }
    let first = i;
    let fraction = |mut k: usize| {
        while toks.get(k).is_some_and(|t| t.kind == TokenKind::Number || t.text(sql) == "/") {
            k += 1;
        }
        k
    };
    if is(i, "FINAL") {
        i += 1;
    }
    if is(i, "SAMPLE") {
        i = fraction(i + 1);
        if is(i, "OFFSET") {
            i = fraction(i + 1);
        }
    }
    (i > first).then(|| toks[first].start..toks[i - 1].end)
}

/// ClickHouse reads a table on the right of `IN` with no FROM for the analysis to follow: `x IN t`,
/// `x IN (db.t)`, `in(x, t)`, `globalNotIn(x, t)`. The last thing in the parentheses is what a call
/// would take as its set; a list that ends in a bare name means nothing else to the server.
fn clickhouse_in_table(sql: &str, toks: &[Token]) -> bool {
    let punct = |i: usize, p: &str| toks.get(i).is_some_and(|t| t.kind == TokenKind::Punct && t.text(sql) == p);
    let name = |i: usize| toks.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword));
    // Whether what starts at `i` and ends before `to` is `a` or `a.b`, in any number of parentheses.
    let bare = |mut i: usize, mut to: usize| {
        while punct(i, "(") && to > i + 1 && punct(to - 1, ")") {
            i += 1;
            to -= 1;
        }
        name(i) && toks.get(i).and_then(|t| word(sql, t, Dialect::ClickHouse)).is_none_or(|w| w != "SELECT" && w != "WITH") && {
            let mut k = i + 1;
            while punct(k, ".") && name(k + 1) {
                k += 2;
            }
            k == to
        }
    };
    toks.iter().enumerate().any(|(i, t)| {
        let Some(w) = word(sql, t, Dialect::ClickHouse) else { return false };
        if CLICKHOUSE_IN_FUNCTIONS.contains(&w.as_str()) {
            return punct(i + 1, "(");
        }
        if w != "IN" {
            return false;
        }
        if !punct(i + 1, "(") {
            return name(i + 1) && {
                let mut k = i + 2;
                while punct(k, ".") && name(k + 1) {
                    k += 2;
                }
                !punct(k, "(")
            };
        }
        let (mut depth, mut last, mut close) = (0usize, i + 2, None);
        for (k, t) in toks.iter().enumerate().skip(i + 1) {
            match (t.kind, t.text(sql)) {
                (TokenKind::Punct, "(") => depth += 1,
                (TokenKind::Punct, ")") => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(k);
                        break;
                    }
                }
                (TokenKind::Punct, ",") if depth == 1 => last = k + 1,
                _ => {}
            }
        }
        bare(last, close.unwrap_or(toks.len()))
    })
}

/// The tables a statement names, to be looked up before `check` (schema as written, name).
pub fn tables_named(sql: &str, dialect: Dialect) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    for_each_statement(sql, dialect, |text, range, toks| {
        // DDL names its tables anywhere (ALTER TABLE t, CREATE INDEX … ON t, a macro's body):
        // every name in it is looked up.
        if names_its_tables(&guard::classify_in(&text[range], dialect)) {
            for n in ddl_names(text, toks, dialect) {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        let a = scope::analyze_in(text, toks, dialect);
        for s in &a.scopes {
            for src in &s.sources {
                if let SourceKind::Table { schema, name } = &src.kind
                    && src.cte.is_none()
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

/// The names in a statement, `a` or `s.a`, as `(schema, name)`.
fn ddl_names(sql: &str, toks: &[Token], d: Dialect) -> Vec<(Option<String>, String)> {
    let name = |t: &Token| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword).then(|| crate::sql::ident_value_in(sql, t, d));
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let Some(n) = name(t) else { continue };
        out.push((None, n.clone()));
        if toks.get(i + 1).is_some_and(|d| d.text(sql) == ".")
            && let Some(m) = toks.get(i + 2).and_then(name)
        {
            out.push((Some(n), m));
        }
    }
    out
}

/// A word as the server reads it, quoted or not (`` `i\x6E` `` and `U&"i\006E"` are `in`).
fn word(sql: &str, t: &Token, d: Dialect) -> Option<String> {
    matches!(t.kind, TokenKind::Ident | TokenKind::Keyword | TokenKind::QuotedIdent).then(|| crate::sql::ident_value_in(sql, t, d).to_ascii_uppercase())
}

fn activity_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ACTIVITY.contains(&n.as_str()) || n.starts_with("dm_exec_") || n.starts_with("events_statements") || n.starts_with("events_waits")
}

pub fn stronger(a: Mask, b: Mask) -> Mask {
    Mask::stronger(a, b)
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
    let restricted = rules.narrows();
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
    let words: Vec<Option<String>> = toks.iter().map(|t| word(sql, t, d)).collect();
    let has = |w: &str| words.iter().any(|x| x.as_deref() == Some(w));

    if !rules.structure && (facts.effect == Effect::Ddl || guard::changes_structure(text, d)) {
        return Err(format!("changing structure ({verb}) is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    if !rules.code && (matches!(verb.as_str(), "CALL" | "DO" | "EXEC" | "EXECUTE") || has("SP_EXECUTESQL") || guard::runs_code(text, d)) {
        return Err(format!("running code ({verb}) is off for agents on {conn} (Dataman › Agents › Permissions)"));
    }
    // A read sent with a write (execute, open_query, a write tool) returns its rows too: only a
    // plan is structure, as on the read tools.
    let rows = facts.effect == Effect::Write || (facts.effect == Effect::Read && !(verb == "EXPLAIN" && !crate::explain::analyzes(text))) || (facts.effect == Effect::Other && !NO_ROWS.contains(&verb.as_str()));
    if write && rules.view == AgentView::Structure && rows {
        return Err(format!("{conn} shows agents its structure only: rows are neither read nor written"));
    }
    let a = scope::analyze_in(sql, toks, d);
    if verb == "EXPLAIN" && rules.restricted() && plan_reads_rows(sql, toks, &a, d) {
        return Err(format!("this EXPLAIN is not available to agents on {conn} while some data is kept from them: the server prints in the plan what it read from the rows"));
    }
    if !rules.activity_on() {
        let named = a.scopes.iter().flat_map(|s| &s.sources).filter_map(|s| match &s.kind {
            SourceKind::Table { name, .. } | SourceKind::Function(name) => Some(name.as_str()),
            _ => None,
        });
        let called = a.calls.iter().map(|(_, n, _)| n.as_str());
        let shows = verb == "SHOW" && (has("PROCESSLIST") || has("STATUS") || has("ENGINE"));
        // Only under `system`: elsewhere a table may just be named so.
        let system_table = d == Dialect::ClickHouse
            && a.scopes.iter().flat_map(|s| &s.sources).any(|s| matches!(&s.kind, SourceKind::Table { schema: Some(x), name } if x.eq_ignore_ascii_case("system") && !crate::sql::functions::clickhouse_plain_system_table(name)));
        // `KILL … TEST` lists what it would stop, statements included.
        if shows || system_table || verb == "KILL" || named.chain(called).any(activity_name) || words.iter().flatten().any(|w| matches!(w.as_str(), "SP_WHO" | "SP_WHO2")) {
            return Err(format!("server activity is off for agents on {conn} (Dataman › Agents › Permissions)"));
        }
    }
    if !restricted {
        return Ok(());
    }

    if let Some(e) = table_command(sql, toks, d, verb == "INSERT") {
        return Err(format!("{e} is not available to agents on {conn} while some data is kept from them: write SELECT … FROM, or use list_tables and describe_table"));
    }
    if a.unsure {
        return Err(format!("Dataman could not follow this statement, and {conn} keeps some data from agents: write it more plainly"));
    }
    if d == Dialect::ClickHouse {
        // Names that only the values say: nothing here could check them.
        if toks.iter().any(|t| t.kind == TokenKind::Param && t.text(sql).contains("Identifier")) {
            return Err(format!("{{name:Identifier}} parameters are not available to agents on {conn} while some data is kept from them: write the names in the statement"));
        }
        if clickhouse_in_table(sql, toks) {
            return Err(format!("IN with a table is not available to agents on {conn} while some data is kept from them: write IN (SELECT … FROM …)"));
        }
        if let Some(w) = words.iter().flatten().find(|w| CLICKHOUSE_NAME_SETTINGS.contains(&w.as_str())) {
            return Err(format!("the setting {} is not available to agents on {conn} while some data is kept from them: it changes what the names in a statement mean", w.to_ascii_lowercase()));
        }
    }
    // Renaming, dropping or rebuilding what is kept (a hidden table, a hidden or masked column)
    // would take it out from under the rules, which go by name.
    if names_its_tables(&facts) {
        let names = Names::Qualified;
        let found = ddl_names(sql, toks, d);
        for (schema, name) in &found {
            let Some(t) = (cx.tables)(schema.as_deref(), name) else { continue };
            if !rules.visible.table_visible(Some(&t.schema), &t.name, names) {
                return Err(format!("{} is not visible to agents on {conn}", t.name));
            }
            let kept = t.columns.iter().find(|c| {
                (rules.visible.column_hidden(Some(&t.schema), &t.name, c, names) || rules.sensitive_mask(Some(&t.schema), &t.name, c, names).is_some())
                    && found.iter().any(|(_, n)| n.eq_ignore_ascii_case(c))
            });
            if let Some(c) = kept {
                return Err(format!("{verb} names {}.{c}, which is kept from agents on {conn}: its structure is the user's to change", t.name));
            }
        }
    }
    if let Some(w) = words.iter().flatten().find(|w| dynamic(w)) {
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
                SourceKind::Table { .. } if src.cte.is_some() => continue,
                SourceKind::Table { schema, name } => (schema.as_deref(), name.as_str()),
                SourceKind::Function(f) => {
                    if cx.routines.contains(&f.to_lowercase()) {
                        return Err(format!("{f}() is a function of your database; agents cannot call your functions on {conn} while some data is kept from them"));
                    }
                    // `sys.dm_db_stats_histogram(…)` and the like read the catalog and its statistics.
                    let schema = a.calls.iter().find(|(_, _, r)| src.name_range.start <= r.start && r.end <= src.name_range.end).and_then(|(s, _, _)| s.as_deref());
                    let catalog = schema.is_some_and(|s| CATALOG_SCHEMAS.contains(&s.rsplit('.').next().unwrap_or(s).to_lowercase().as_str()));
                    let lower = f.to_lowercase();
                    // DuckDB's `duckdb_tables()`, `duckdb_columns()`… hold definitions, defaults and
                    // checks; SQLite's `pragma_table_info()` and the like too.
                    if catalog || ["pg_", "dm_", "fn_", "duckdb_", "pragma_"].iter().any(|p| lower.starts_with(p)) {
                        return Err(format!("{f}() reads the system catalog: on {conn} agents list and describe through list_tables, describe_table and search_objects"));
                    }
                    // DuckDB's table macros that take a table by name read all of it.
                    if cx.dialect == Dialect::DuckDb && matches!(lower.as_str(), "histogram" | "histogram_values" | "summary") {
                        return Err(format!("{f}() reads a whole table by name: not available to agents on {conn} while some data is kept from them"));
                    }
                    continue;
                }
                _ => continue,
            };
            let lower = name.to_lowercase();
            // ClickHouse's `system` holds every table's columns and definition; elsewhere a schema
            // may just be named so.
            if schema.is_some_and(|x| CATALOG_SCHEMAS.contains(&x.to_lowercase().as_str()) || (cx.dialect == Dialect::ClickHouse && x.eq_ignore_ascii_case("system")))
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
    // `*` hands a call every column under no column's name, past what goes by names; `COLUMNS(…)`
    // and `* REPLACE (…)` pick and compute them where no name is read.
    let stars = stars(sql, toks, d);
    let picks = matches!(d, Dialect::ClickHouse | Dialect::DuckDb) && words.iter().zip(toks.iter().skip(1)).any(|(w, n)| w.as_deref() == Some("COLUMNS") && n.text(sql) == "(");
    if stars.loose || stars.computed || picks {
        return Err(format!("this * is not available to agents on {conn} while some data is kept from them: select * alone or name the columns"));
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
    if any_hidden && !reads {
        if stars.item || a.scopes.iter().any(|s| s.items.iter().any(|i| matches!(i, Item::Star(_)))) {
            return Err(format!("name the columns: some columns are hidden from agents on {conn}"));
        }
        if verb == "INSERT" && a.targets.is_empty() {
            return Err(format!("name the columns of this INSERT: some columns are hidden from agents on {conn}"));
        }
        // A write reads the table itself, not a narrowed copy: whole rows (`users::text`,
        // `RETURNING users`) and columns named where Dataman does not follow them (inside
        // `substring(… FROM …)` and the like) would carry the hidden values.
        for (_, _, n) in narrowed.iter().filter(|(_, _, n)| !n.hidden.is_empty()) {
            if whole_row(&a, &n.table, &eq) {
                return Err(format!("whole rows of {} are not available to agents on {conn}: some columns are hidden; name the columns", n.table.name));
            }
            if let Some(c) = unfollowed(&a, sql, toks, &n.hidden, &eq, d) {
                return Err(format!("{}.{c} is hidden from agents on {conn}", n.table.name));
            }
        }
    }
    // Read through the agent login's view where it has one: the view holds the visible
    // columns, the masked ones masked by the server.
    let view_of = |t: &Table| cx.views.iter().find(|v| v.schema.eq_ignore_ascii_case(&t.schema) && v.table.eq_ignore_ascii_case(&t.name));
    let mut on_view: Vec<String> = Vec::new();
    if reads {
        for (si, k, n) in &narrowed {
            let Some(v) = view_of(&n.table) else { continue };
            let src = &a.scopes[*si].sources[*k];
            let mut text = v.view.clone();
            if src.alias.is_none() {
                text.push(' ');
                text.push_str(&d.quote_ident(&n.table.name));
            }
            edits.push((src.name_range.clone(), text));
            on_view.push(n.table.name.clone());
        }
    }
    // A table with hidden columns becomes a derived table of the others, under the same name.
    if any_hidden && reads {
        for (si, k, n) in &narrowed {
            if n.hidden.is_empty() || on_view.contains(&n.table.name) {
                continue;
            }
            let src = &a.scopes[*si].sources[*k];
            let cols: Vec<String> = n.table.columns.iter().filter(|c| !n.hidden.contains(c)).map(|c| d.quote_ident(c)).collect();
            if cols.is_empty() {
                return Err(format!("no column of {} is visible to agents on {conn}", n.table.name));
            }
            // `FINAL` and `SAMPLE` belong to a table, and a derived table takes neither: they go in.
            let modifiers = if d == Dialect::ClickHouse { clickhouse_table_modifiers(sql, toks, src.name_range.end) } else { None };
            let inside = modifiers.as_ref().map_or(String::new(), |r| format!(" {}", &sql[r.clone()]));
            if let Some(r) = modifiers {
                edits.push((r, String::new()));
            }
            let mut text = format!("(SELECT {} FROM {}{inside})", cols.join(", "), d.qualified(Some(&n.table.schema), &n.table.name));
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
    let calls = call_spans(sql, toks, d);
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
                if bare.iter().any(|&i| !reaches(&a, si, i, col, reads, &eq)) || (!bare.is_empty() && answers_one_value(sql, toks, s)) {
                    return Err(refuse());
                }
            }
            for r in a.refs.iter().filter(|r| eq(&r.name, col) && matches!(r.clause, Clause::Where | Clause::On)) {
                if !compared(sql, toks, r.range.clone(), &calls) {
                    return Err(refuse());
                }
            }
            // Masked by the server's view as the rules mask it: not masked twice.
            let by_view = on_view.contains(&n.table.name) && view_of(&n.table).is_some_and(|v| v.masked.iter().any(|(c, m)| eq(c, col) && m == how));
            if by_view {
                continue;
            }
            match masked.iter_mut().find(|(c, _)| eq(c, col)) {
                Some(m) => m.1 = stronger(m.1, *how),
                None => masked.push((col.clone(), *how)),
            }
        }
        if let Some(c) = unfollowed(&a, sql, toks, &cols, &eq, d) {
            return Err(format!("{}.{c} is masked for agents on {conn}: select it as it is, or compare it as it is in a filter; it cannot be renamed, copied, grouped, sorted or computed", n.table.name));
        }
        // Whole rows carry every column: `SELECT u`, `row_to_json(u)`, `to_jsonb(u.*)`, and so do
        // the rows of a subquery or a WITH query that selects a masked column.
        if whole_row(&a, &n.table, &eq) || any_source_row(&a, &eq) {
            return Err(format!("whole rows of {} are not available to agents in Masked data on {conn}", n.table.name));
        }
    }
    let by_position = positional_order(sql, toks, d);
    if let Some(w) = by_position {
        return Err(format!("{w} BY a position is not available to agents in Masked data on {conn}: name the columns (masked columns cannot be sorted or grouped)"));
    }
    if d == Dialect::TSql && toks.windows(2).any(|w| w[0].text(sql).eq_ignore_ascii_case("FOR") && ["JSON", "XML"].iter().any(|x| w[1].text(sql).eq_ignore_ascii_case(x))) {
        return Err(format!("FOR JSON and FOR XML are not available to agents in Masked data on {conn}: masked columns come back masked only as columns"));
    }
    if d == Dialect::ClickHouse {
        // A statement's own format comes back as lines of text in one column, under no column's name.
        let ends = |i: usize| toks.get(i).is_none_or(|t| t.text(sql) == ";" || word(sql, t, d).as_deref() == Some("SETTINGS"));
        if toks.iter().enumerate().any(|(i, t)| word(sql, t, d).as_deref() == Some("FORMAT") && toks.get(i + 1).is_some_and(|n| matches!(n.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword | TokenKind::Param | TokenKind::String)) && ends(i + 2)) {
            return Err(format!("FORMAT is not available to agents in Masked data on {conn}: masked columns come back masked only as columns"));
        }
        let kept: Vec<&str> = sensitive.iter().flat_map(|n| &n.sensitive).map(|(c, _)| c.as_str()).collect();
        if let Some(c) = clickhouse_read_into(sql, toks, &kept, &eq) {
            return Err(format!("{c} is masked for agents on {conn}: select it as it is, on its own in the select list; it cannot be renamed, copied, grouped, sorted, compared or computed"));
        }
    }
    Ok(())
}

/// Whether the server reads rows while it plans this EXPLAIN, and prints what it read. ClickHouse
/// runs scalar subqueries and fills the sets of IN; MySQL reads a table a key narrows to one row
/// and writes its values into the conditions of the rest of the statement. PostgreSQL, SQL Server,
/// SQLite and DuckDB read no row for a plan without ANALYZE. Probed on the versions in `WithReach`.
fn plan_reads_rows(sql: &str, toks: &[Token], a: &Analysis, d: Dialect) -> bool {
    let subqueries = a.scopes.len() > 1;
    match d {
        Dialect::ClickHouse => subqueries || clickhouse_in_table(sql, toks),
        // `EXPLAIN FOR CONNECTION n` is the plan of another session's statement, values included.
        Dialect::MySql => {
            subqueries
                || a.scopes.first().is_some_and(|s| s.sources.len() > 1)
                || toks.windows(2).any(|w| word(sql, &w[0], d).as_deref() == Some("FOR") && word(sql, &w[1], d).as_deref() == Some("CONNECTION"))
        }
        Dialect::Postgres | Dialect::TSql | Dialect::Sqlite | Dialect::DuckDb => false,
    }
}

/// The `*`s of a statement that stand for columns (not a multiplication).
#[derive(Default)]
struct Stars {
    /// As a select or RETURNING item.
    item: bool,
    /// Anywhere else: in a call, in parentheses, in a clause. `count(*)` is none of them.
    loose: bool,
    /// Followed by what computes the columns: `* REPLACE (…)`, `* RENAME (…)`, `* APPLY (…)`.
    computed: bool,
}

/// Words that end a select list.
const LIST_ENDS: &[&str] = &["FROM", "INTO", "WHERE", "PREWHERE", "GROUP", "HAVING", "WINDOW", "QUALIFY", "ORDER", "LIMIT", "OFFSET", "FETCH", "SETTINGS"];

fn stars(sql: &str, toks: &[Token], d: Dialect) -> Stars {
    let is = |t: &Token, w: &str| matches!(t.kind, TokenKind::Ident | TokenKind::Keyword) && t.text(sql).eq_ignore_ascii_case(w);
    let mut out = Stars::default();
    // Per parenthesis: whether a select list is open there.
    let mut lists = vec![false];
    for (i, t) in toks.iter().enumerate() {
        let text = t.text(sql);
        if t.kind == TokenKind::Punct && matches!(text, "(" | "[") {
            lists.push(false);
        } else if t.kind == TokenKind::Punct && matches!(text, ")" | "]") {
            if lists.len() > 1 {
                lists.pop();
            }
        } else if let Some(open) = lists.last_mut() {
            *open = (*open && !LIST_ENDS.iter().any(|w| is(t, w))) || is(t, "SELECT") || is(t, "RETURNING");
        }
        if text != "*" {
            continue;
        }
        let prev = i.checked_sub(1).map(|p| &toks[p]);
        let next = toks.get(i + 1);
        // Where a value would start; or, after what may be a value (`TOP 5 *`), where it would have
        // to go on.
        let starts = prev.is_none_or(|p| (p.kind == TokenKind::Punct && matches!(p.text(sql), "(" | "[" | "," | ".")) || ["SELECT", "DISTINCT", "ALL", "BY", "RETURNING"].iter().any(|w| is(p, w)));
        let ended = next.is_none_or(|n| (n.kind == TokenKind::Punct && matches!(n.text(sql), ")" | "," | ";")) || LIST_ENDS.iter().any(|w| is(n, w)));
        if !starts && !ended {
            continue;
        }
        if !lists.last().copied().unwrap_or(false) {
            let count = i >= 2 && next.is_some_and(|n| n.text(sql) == ")") && toks[i - 1].text(sql) == "(" && word(sql, &toks[i - 2], d).is_some_and(|w| w == "COUNT" || w == "COUNT_BIG");
            out.loose |= !count;
            continue;
        }
        out.item = true;
        // `* EXCEPT (a) REPLACE (…)`: what only leaves columns out comes first.
        let mut k = i + 1;
        if toks.get(k).is_some_and(|n| is(n, "EXCEPT") || is(n, "EXCLUDE")) {
            k += 1;
            if toks.get(k).is_some_and(|n| n.text(sql) == "(") {
                let mut depth = 0;
                while let Some(n) = toks.get(k) {
                    depth += i32::from(n.text(sql) == "(") - i32::from(n.text(sql) == ")");
                    k += 1;
                    if depth == 0 {
                        break;
                    }
                }
            } else {
                k += 1;
            }
        }
        out.computed |= toks.get(k).is_some_and(|n| ["REPLACE", "RENAME", "APPLY"].iter().any(|w| is(n, w)));
    }
    out
}

/// `TABLE t` (a query: PostgreSQL, MySQL) and MySQL's `SHOW`, `DESCRIBE` and `EXPLAIN t`, which
/// read a table or its definition whole, under names Dataman does not narrow.
fn table_command(sql: &str, toks: &[Token], d: Dialect, insert: bool) -> Option<String> {
    let w = |i: usize| toks.get(i).and_then(|t| word(sql, t, d)).unwrap_or_default();
    for (i, t) in toks.iter().enumerate().filter(|_| d != Dialect::TSql) {
        if word(sql, t, d).as_deref() != Some("TABLE") || toks.get(i + 1).is_none_or(|n| !matches!(n.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword)) {
            continue;
        }
        let query = match i.checked_sub(1).map(|p| toks[p]) {
            None => true,
            Some(p) if p.kind == TokenKind::Punct => matches!(p.text(sql), "(" | ")"),
            Some(p) if matches!(p.kind, TokenKind::Ident | TokenKind::QuotedIdent) => insert,
            Some(p) => matches!(word(sql, &p, d).as_deref(), Some("UNION" | "INTERSECT" | "EXCEPT" | "ALL" | "DISTINCT" | "AS")),
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
        if let Some(t) = toks.iter().enumerate().find(|(i, t)| word(sql, t, d).is_some_and(|x| x.starts_with("PRAGMA_")) && toks.get(i + 1).is_some_and(|n| n.text(sql) == "(")) {
            return Some(format!("{}()", t.1.text(sql)));
        }
    }
    if d == Dialect::ClickHouse {
        return matches!(w(0).as_str(), "DESC" | "DESCRIBE" | "SHOW" | "EXISTS").then(|| w(0));
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

/// A subquery in a filter whose one value is compared or converted by the outer statement, not
/// a set or a test (`IN`, `ANY`, `EXISTS`). A server converts that value to the other side's type,
/// and its error says what it could not convert.
fn answers_one_value(sql: &str, toks: &[Token], s: &scope::Scope) -> bool {
    if !matches!(s.role, Role::Expr(_)) {
        return false;
    }
    let Some(mut at) = toks.iter().position(|t| t.start >= s.range.start) else { return true };
    while at > 0 && toks[at - 1].kind == TokenKind::Punct && toks[at - 1].text(sql) == "(" {
        at -= 1;
    }
    // Quoted, it is a function of that name (`any`), not the operator.
    let before = at.checked_sub(1).filter(|&p| toks[p].kind != TokenKind::QuotedIdent).and_then(|p| word(sql, &toks[p], Dialect::ClickHouse));
    !before.is_some_and(|w| matches!(w.as_str(), "IN" | "ANY" | "ALL" | "SOME" | "EXISTS"))
}

/// ClickHouse reads into a column with a dot (`meta.keys`, `doc.a.b`, `t.x`, `doc.a.:Int64`), the
/// parts of a Nested are columns with a dot (`nest.a`) and its name reads them all (`nest`). The
/// analysis takes those for `alias.column`, so its rules for a column do not see them. Such a
/// name comes back masked only as a column of the first SELECT's list, alone and with no alias;
/// anywhere else: what it names.
fn clickhouse_read_into(sql: &str, toks: &[Token], kept: &[&str], eq: &dyn Fn(&str, &str) -> bool) -> Option<String> {
    let d = Dialect::ClickHouse;
    let punct = |t: Option<&Token>, p: &str| t.is_some_and(|t| t.kind == TokenKind::Punct && t.text(sql) == p);
    let name = |t: &Token| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword);
    let keyword = |t: &Token, k: &str| t.kind != TokenKind::QuotedIdent && word(sql, t, d).as_deref() == Some(k);
    let first_select = toks.iter().position(|t| keyword(t, "SELECT"))?;
    let mut depth = 0usize;
    let mut list_end = toks.len();
    for (i, t) in toks.iter().enumerate().skip(first_select) {
        match (t.kind, t.text(sql)) {
            (TokenKind::Punct, "(") => depth += 1,
            (TokenKind::Punct, ")") => depth = depth.saturating_sub(1),
            _ if depth == 0 && keyword(t, "FROM") => {
                list_end = i;
                break;
            }
            _ => {}
        }
    }
    let mut depth = 0usize;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if punct(Some(t), "(") {
            depth += 1;
        } else if punct(Some(t), ")") {
            depth = depth.saturating_sub(1);
        }
        if !name(t) || (i > 0 && punct(Some(&toks[i - 1]), ".")) {
            i += 1;
            continue;
        }
        let mut chain = vec![crate::sql::ident_value_in(sql, t, d)];
        let mut end = i;
        while punct(toks.get(end + 1), ".") && toks.get(end + 2).is_some_and(name) {
            end += 2;
            chain.push(crate::sql::ident_value_in(sql, &toks[end], d));
        }
        // `doc.a.:Int64`, `t.1`, `doc.^a`: read further than a name says.
        let dangling = toks.get(end + 1).is_some_and(|n| n.text(sql).starts_with('.'));
        let hit = kept.iter().find(|c| {
            let want: Vec<&str> = c.split('.').collect();
            let inside = chain.windows(want.len()).position(|w| w.iter().zip(&want).all(|(a, b)| eq(a, b)));
            let parent = chain.len() < want.len() && chain.iter().zip(&want).all(|(a, b)| eq(a, b));
            // The last name of a chain, a column with no dots: the analysis follows that.
            let plain = want.len() == 1 && inside == Some(chain.len() - 1) && !dangling;
            (inside.is_some() || parent) && !plain
        });
        if hit.is_some() {
            let alone = depth == 0
                && i > first_select
                && end < list_end
                && !dangling
                && (i == first_select + 1 || punct(Some(&toks[i - 1]), ",") || keyword(&toks[i - 1], "DISTINCT"))
                && (end + 1 == list_end || punct(toks.get(end + 1), ","));
            if !alone {
                return Some(chain.join("."));
            }
        }
        i = end + 1;
    }
    None
}

/// Words a parenthesis may follow in a filter without being a call's (or a cast's) arguments.
const FILTER_WORDS: &[&str] = &["WHERE", "ON", "AND", "OR", "NOT", "IN", "WHEN", "THEN", "ELSE", "BETWEEN", "HAVING", "EXISTS", "ANY", "ALL", "SOME", "SELECT", "FROM", "JOIN", "AS", "LATERAL"];

/// The byte spans of argument lists: function calls, and keyword forms such as `CAST(…)`.
fn call_spans(sql: &str, toks: &[Token], d: Dialect) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    for i in 1..toks.len() {
        let (p, t) = (toks[i - 1], toks[i]);
        if t.kind != TokenKind::Punct || t.text(sql) != "(" || !matches!(p.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword) {
            continue;
        }
        if word(sql, &p, d).is_some_and(|w| FILTER_WORDS.contains(&w.as_str())) {
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
fn unfollowed(a: &Analysis, sql: &str, toks: &[Token], cols: &[String], eq: &dyn Fn(&str, &str) -> bool, d: Dialect) -> Option<String> {
    for (i, t) in toks.iter().enumerate() {
        if !matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent) {
            continue;
        }
        let v = crate::sql::ident_value_in(sql, t, d);
        let Some(c) = cols.iter().find(|c| eq(c, &v)) else { continue };
        let known = a.refs.iter().any(|r| r.name_range == t.range())
            || a.targets.iter().any(|x| x.range == t.range())
            || a.refs.iter().any(|r| r.range.start <= t.start && t.end <= r.range.end)
            || i.checked_sub(1).is_some_and(|p| word(sql, &toks[p], d).as_deref() == Some("AS"));
        if !known {
            return Some(c.clone());
        }
    }
    None
}

/// `ORDER BY 1` and `GROUP BY 2`: a sort by position names no column.
fn positional_order(sql: &str, toks: &[Token], d: Dialect) -> Option<&'static str> {
    for (i, w) in toks.windows(2).enumerate() {
        let by = word(sql, &w[1], d).as_deref() == Some("BY");
        let what = match word(sql, &w[0], d).as_deref() {
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
        let cx = Context { rules: r, connection: "prod-pg", dialect: Dialect::Postgres, tables: &tables, routines: &routines, views: &[] };
        check(sql, write, &cx)
    }

    #[test]
    fn ddl_keeps_its_hands_off_what_is_kept() {
        let r = rules();
        for sql in [
            "ALTER TABLE users RENAME COLUMN password_hash TO note",
            "ALTER TABLE users ADD COLUMN pw2 text GENERATED ALWAYS AS (\"password_hash\") STORED",
            "ALTER TABLE public.payments RENAME TO pays",
            "DROP TABLE payments",
            "CREATE INDEX i ON users (password_hash)",
            "CREATE INDEX i ON payments (id)",
            "CREATE VIEW v AS SELECT id FROM payments",
            "CREATE VIEW v AS SELECT password_hash FROM users",
        ] {
            let e = run(&r, sql, true).unwrap_err();
            assert!(e.contains("kept from agents") || e.contains("not visible") || e.contains("hidden"), "{sql}: {e}");
        }
        assert!(run(&r, "CREATE VIEW v AS SELECT * FROM users", true).unwrap_err().contains("name the columns"));
        for sql in ["ALTER TABLE users ADD COLUMN nickname text", "CREATE INDEX i ON users (email)", "ALTER TABLE orders RENAME COLUMN notes TO memo"] {
            assert!(run(&r, sql, true).is_ok(), "{sql}");
        }
    }

    #[test]
    fn duckdb_catalog_and_table_macros_are_refused() {
        let r = rules();
        let routines: HashSet<String> = ["m_pw".to_string()].into();
        let cx = Context { rules: &r, connection: "duck", dialect: Dialect::DuckDb, tables: &tables, routines: &routines, views: &[] };
        for sql in ["SELECT sql FROM duckdb_tables()", "SELECT * FROM duckdb_columns() WHERE table_name = 'users'", "FROM histogram(users, email)", "SELECT m_pw()", "SELECT * FROM pragma_table_info('users')"] {
            assert!(check(sql, false, &cx).is_err(), "{sql}");
        }
        assert!(check("SELECT name FROM users", false, &cx).is_ok());
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
    fn clickhouse_keeps_its_catalog_from_agents_too() {
        let r = rules();
        let routines = HashSet::new();
        let cx = |dialect| Context { rules: &r, connection: "ch", dialect, tables: &tables, routines: &routines, views: &[] };
        for sql in ["SELECT * FROM system.columns", "SELECT name FROM SYSTEM.tables"] {
            assert!(check(sql, false, &cx(Dialect::ClickHouse)).unwrap_err().contains("system catalog"), "{sql}");
        }
        assert!(check("DESCRIBE TABLE payments", false, &cx(Dialect::ClickHouse)).is_err());
        assert!(check("SELECT id FROM orders", false, &cx(Dialect::ClickHouse)).is_ok());
        assert!(!check("SELECT id FROM system.orders", false, &cx(Dialect::Postgres)).is_err_and(|e| e.contains("system catalog")), "elsewhere a schema may be named so");
    }

    #[test]
    fn clickhouse_reads_what_it_names_in_its_own_syntax() {
        let r = rules();
        let routines = HashSet::new();
        let cx = Context { rules: &r, connection: "ch", dialect: Dialect::ClickHouse, tables: &tables, routines: &routines, views: &[] };
        let refused = [
            // The right of IN is a table, with no FROM to follow (answers rows on 25.8).
            "SELECT id FROM orders WHERE id IN payments",
            "SELECT id FROM orders WHERE id IN public.payments",
            "SELECT id FROM orders WHERE id GLOBAL NOT IN payments",
            "SELECT id FROM orders WHERE id IN (payments)",
            "SELECT id FROM orders WHERE id IN ((public.payments))",
            "SELECT in(id, payments) FROM orders",
            "SELECT id FROM orders WHERE in(id, public.payments)",
            "SELECT globalNotIn(id, payments) FROM orders",
            "SELECT id FROM orders WHERE NOT in(id, payments)",
            "SELECT id FROM orders QUALIFY in(lower(notes), public.payments)",
            "SELECT id FROM orders WHERE id IN system.one",
            // Names that only the parameters' values say.
            "SELECT * FROM {t:Identifier}",
            "SELECT {c:Identifier} FROM users",
            "SELECT id FROM orders WHERE id IN {t:Identifier}",
            // Every way to write a join reaches the table.
            "SELECT 1 FROM orders ALL INNER JOIN payments ON 1",
            "SELECT 1 FROM orders ANY LEFT JOIN payments USING id",
            "SELECT 1 FROM orders GLOBAL ANY LEFT JOIN payments ON 1",
            "SELECT 1 FROM orders LEFT ANTI JOIN payments USING id",
            "SELECT 1 FROM orders ASOF JOIN payments USING id",
            "SELECT 1 FROM orders GLOBAL CROSS JOIN payments",
            "SELECT 1 FROM orders PASTE JOIN payments",
            "SELECT 1 FROM orders FINAL JOIN payments ON 1",
            "SELECT 1 FROM orders SAMPLE 1/10 OFFSET 1/2 JOIN payments ON 1",
            "SELECT 1 FROM orders ARRAY JOIN [1] AS x JOIN payments ON 1",
            "SELECT 1 FROM orders LEFT ARRAY JOIN [1] AS x JOIN payments ON 1",
            // Names as the server reads them.
            "SELECT id FROM `pay\\x6Dents`",
            "SELECT id FROM `pay\\Nments`",
            "SELECT id FROM “payments”",
            "SELECT id FROM `sys\\x74em`.`one`",
            "SELECT id FROM “system”.“one”",
            "SELECT u.`password\\x5Fhash` FROM users u",
            "SELECT id FROM users WHERE `password\\x5Fhash` = 'x'",
        ];
        for sql in refused {
            assert!(check(sql, false, &cx).is_err(), "{sql}");
        }
        // Acting on what is kept: nothing reads it, and it is gone all the same.
        for sql in ["TRUNCATE TABLE payments", "TRUNCATE payments", "OPTIMIZE TABLE payments FINAL", "CHECK TABLE payments", "INSERT INTO TABLE payments VALUES (1)"] {
            assert!(check(sql, true, &cx).is_err(), "{sql}");
        }
        for sql in [
            "SELECT id FROM orders WHERE id IN (1, 2)",
            "SELECT id FROM orders WHERE id NOT IN (SELECT id FROM orders)",
            "SELECT id FROM orders WHERE id IN (WITH 1 AS x SELECT x)",
            "SELECT id FROM orders WHERE (id) IN (1)",
            "SELECT id FROM orders WHERE id IN [1, 2]",
            "SELECT id FROM orders WHERE id IN (toUInt64(1))",
            "SELECT id FROM orders WHERE date IN ('2024-01-01')",
            "SELECT id FROM orders WHERE id NOT IN (1, 2)",
            "SELECT id FROM orders WHERE (id, 1) IN ((1, 1), (2, 1))",
            "SELECT id FROM orders WHERE lower(notes) IN (lower('A'), 'b')",
            "SELECT id FROM orders WHERE id IN {v:Array(UInt8)}",
            "SELECT id FROM orders WHERE id = {v:UInt64}",
            "SELECT o.id FROM orders o ANY LEFT JOIN users u USING id",
            "SELECT o.id FROM orders o ALL INNER JOIN users u ON o.id = u.id",
            "SELECT id FROM orders FINAL SAMPLE 0.1",
            "SELECT id FROM orders ARRAY JOIN [1] AS x",
        ] {
            assert!(check(sql, false, &cx).is_ok(), "{sql}: {:?}", check(sql, false, &cx));
        }
        assert!(check("INSERT INTO TABLE orders (id) VALUES (1)", true, &cx).is_ok());
    }

    #[test]
    fn clickhouse_modifiers_go_inside_a_narrowed_table() {
        let r = AgentRules { visible: Visible { mode: VisibleMode::Except, items: vec![VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column }] }, ..Default::default() };
        let routines = HashSet::new();
        let cx = Context { rules: &r, connection: "ch", dialect: Dialect::ClickHouse, tables: &tables, routines: &routines, views: &[] };
        for (sql, want) in [
            ("SELECT id FROM users FINAL", "SELECT id FROM (SELECT id, name, email, created_at FROM public.users FINAL) users "),
            ("SELECT u.id FROM users AS u FINAL SAMPLE 1/10 OFFSET 1/2 WHERE u.id = 1", "SELECT u.id FROM (SELECT id, name, email, created_at FROM public.users FINAL SAMPLE 1/10 OFFSET 1/2) AS u  WHERE u.id = 1"),
            ("SELECT u.id FROM users u SAMPLE 0.1", "SELECT u.id FROM (SELECT id, name, email, created_at FROM public.users SAMPLE 0.1) u "),
        ] {
            assert_eq!(check(sql, false, &cx).unwrap().sql, want, "{sql}");
        }
    }

    #[test]
    fn clickhouse_keeps_masked_values_in_their_columns() {
        let mut r = AgentRules { view: AgentView::Masked, ..Default::default() };
        r.sensitive.push(SensitiveCol { col: "customers.email".into(), how: Mask::PARTIAL, on: true });
        let routines = HashSet::new();
        let cx = Context { rules: &r, connection: "ch", dialect: Dialect::ClickHouse, tables: &tables, routines: &routines, views: &[] };
        for sql in [
            // A format of its own comes back as lines under `result`: nothing masks that by name.
            "SELECT email FROM customers FORMAT CSV",
            "SELECT * FROM customers FORMAT JSONEachRow",
            "SELECT id FROM customers FORMAT `Vertical`",
            // EXPLAIN runs a scalar subquery and prints what it answered (probed on 25.8).
            "EXPLAIN PLAN actions = 1 SELECT id FROM customers WHERE (SELECT email FROM customers LIMIT 1) = name",
            "EXPLAIN QUERY TREE SELECT id FROM orders WHERE (SELECT email FROM customers LIMIT 1) = notes",
            // Renamed or computed, however the name is written.
            "SELECT `em\\x61il` AS e FROM customers",
            "SELECT upper(`em\\x61il`) FROM customers",
            "SELECT “email” AS e FROM customers",
            "SELECT {c:Identifier} AS e FROM customers",
            "SELECT id FROM customers PREWHERE upper(email) = 'A'",
            "SELECT id FROM customers LIMIT 1 BY email",
            "SELECT x FROM customers ARRAY JOIN [email] AS x",
            "SELECT 1 FROM customers ANY LEFT JOIN orders ON upper(email) = 'a'",
        ] {
            assert!(check(sql, false, &cx).is_err(), "{sql}");
        }
        assert_eq!(check("SELECT id, email FROM customers", false, &cx).unwrap().masked.len(), 1);
        assert!(check("SELECT email FROM customers WHERE email = {v:String}", false, &cx).is_ok());
        assert!(check("SELECT format FROM orders", false, &cx).is_ok(), "a column named so");
        assert!(check("SELECT format, email FROM customers WHERE format = 'x'", false, &cx).is_ok(), "a column of the masked table too");
        assert!(check("EXPLAIN SELECT id FROM customers", false, &cx).is_ok());
    }

    #[test]
    fn clickhouse_server_activity_is_an_agent_s_only_when_on() {
        let routines = HashSet::new();
        let on = |rules: AgentRules, sql: &str| check(sql, false, &Context { rules: &rules, connection: "ch", dialect: Dialect::ClickHouse, tables: &tables, routines: &routines, views: &[] });
        let off = || AgentRules { activity: false, ..Default::default() };
        for sql in [
            "SELECT query FROM system.processes",
            "SELECT query FROM system.query_log",
            "SELECT 1 FROM `system`.`query_views_log`",
            "SHOW PROCESSLIST",
            "SELECT query FROM system.query_cache",
            "SELECT query FROM system.asynchronous_inserts",
            "SELECT query FROM system.backups",
            "SELECT 1 FROM `sys\\x74em`.`query_cache`",
            "SELECT 1 FROM system.a_table_of_a_later_server",
            "KILL QUERY WHERE 1 TEST",
        ] {
            assert!(on(off(), sql).unwrap_err().contains("server activity"), "{sql}");
        }
        assert!(on(off(), "SELECT id FROM processes").is_ok(), "only under system: a table may be named so");
        assert!(on(off(), "SELECT name FROM system.tables").is_ok());
        assert!(on(AgentRules::default(), "SELECT query FROM system.processes").is_ok());
        assert!(on(rules(), "SELECT query FROM system.query_log").unwrap_err().contains("server activity"), "off while anything is kept");
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
        r.sensitive.push(SensitiveCol { col: "customers.email".into(), how: Mask::PARTIAL, on: true });
        r.sensitive.push(SensitiveCol { col: "*ssn*".into(), how: Mask::Hidden, on: true });
        r.sensitive.push(SensitiveCol { col: "orders.notes".into(), how: Mask::Hidden, on: false });
        let c = run(&r, "SELECT id, name, email, ssn, city FROM customers WHERE city = 'Recife' AND email LIKE 'a%'", false).unwrap();
        assert!(!c.rewritten);
        assert_eq!(c.masked, vec![("email".to_string(), Mask::PARTIAL), ("ssn".to_string(), Mask::Hidden)]);
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
        // A read sent as a write (execute, a write tool) returns its rows all the same.
        for sql in ["SELECT * FROM t", "CREATE TABLE x (a int); SELECT * FROM t", "TABLE t", "WITH x AS (SELECT 1) SELECT * FROM x", "EXPLAIN ANALYZE SELECT * FROM t"] {
            assert!(run(&s, sql, true).unwrap_err().contains("structure only"), "{sql}");
        }
        assert!(run(&s, "EXPLAIN SELECT * FROM t", true).is_ok(), "a plan is structure");
        let masked = AgentRules { view: AgentView::Masked, ..Default::default() };
        assert!(run(&masked, "SELECT * FROM pg_stat_activity", false).unwrap_err().contains("server activity"), "forced off while data is masked");
    }

    /// Refused where the server reads the table of that name (or may), each dialect as it was
    /// seen to resolve WITH names. `payments` and `users.password_hash` are hidden.
    #[test]
    fn a_with_name_is_a_query_only_where_the_server_says_so() {
        let r = rules();
        let routines = HashSet::new();
        use Dialect::{ClickHouse as Ch, DuckDb as Duck, MySql as My, Postgres as Pg, Sqlite as Lite, TSql as Ms};
        let all = [Pg, My, Ms, Lite, Duck, Ch];
        let cases: &[(&str, &[Dialect])] = &[
            // Its own name: the table on these, an error or a recursion on SQL Server and SQLite.
            ("WITH payments AS (SELECT * FROM payments) SELECT * FROM payments", &all),
            ("WITH x AS (SELECT 1), payments AS (SELECT * FROM (SELECT * FROM payments) q) SELECT * FROM payments", &all),
            ("WITH payments AS (WITH x AS (SELECT * FROM payments) SELECT * FROM x) SELECT * FROM payments", &[Pg, My, Duck, Ch]),
            ("WITH payments AS (SELECT 1 AS id UNION ALL SELECT id FROM payments) SELECT * FROM payments", &[Pg, My, Duck, Ch]),
            // A later query's name.
            ("WITH a AS (SELECT * FROM payments), payments AS (SELECT 1) SELECT * FROM a", &[Pg, My, Ms, Duck, Ch]),
            ("WITH RECURSIVE a AS (SELECT * FROM payments), payments AS (SELECT 1) SELECT * FROM a", &[My, Duck, Ch]),
            // The first branch of a recursive query, and what is no recursive query.
            ("WITH RECURSIVE payments AS (SELECT * FROM payments UNION ALL SELECT 1) SELECT * FROM payments", &all),
            ("WITH RECURSIVE payments AS (SELECT * FROM payments) SELECT * FROM payments", &all),
            ("WITH RECURSIVE payments AS (SELECT 1 AS id, (SELECT max(card) FROM payments) AS c UNION ALL SELECT 2, 'y') SELECT * FROM payments", &all),
            ("WITH RECURSIVE payments AS (SELECT 5 AS id UNION ALL SELECT id FROM payments EXCEPT SELECT 9) SELECT * FROM payments", &all),
            ("WITH RECURSIVE payments AS (SELECT 5 AS id EXCEPT SELECT id FROM payments) SELECT * FROM payments", &all),
            ("WITH RECURSIVE payments AS (SELECT 5 AS id UNION ALL SELECT id FROM payments UNION ALL SELECT 6) SELECT * FROM payments", &[Duck]),
            ("WITH RECURSIVE payments AS (WITH b AS (SELECT 5 AS id UNION ALL SELECT id FROM payments) SELECT * FROM b) SELECT * FROM payments", &all),
            // Through a cycle of names, ClickHouse reaches the table of the name it started from.
            ("WITH payments AS (SELECT * FROM orders), orders AS (SELECT * FROM payments) SELECT * FROM payments", &[Ch]),
            ("WITH p AS (WITH q AS (SELECT * FROM r) SELECT * FROM q), r AS (SELECT * FROM p) SELECT * FROM p", &[Pg, My, Ms, Duck, Ch]),
            // A later branch's WITH is that branch's alone.
            ("SELECT 1 AS id UNION ALL WITH payments AS (SELECT 2 AS id) SELECT * FROM payments UNION ALL SELECT id FROM payments", &all),
            ("SELECT * FROM (SELECT 1 AS id UNION ALL WITH payments AS (SELECT 2 AS id) SELECT * FROM payments UNION ALL SELECT id FROM payments) q", &all),
            // ClickHouse's WITH of an expression names no query: FROM reads the table.
            ("WITH 1 AS payments SELECT * FROM payments", &[Ch]),
            ("WITH (SELECT 1) AS payments SELECT * FROM payments", &[Ch]),
            ("WITH 'x' AS payments, 2 AS y SELECT * FROM payments", &[Ch]),
            // A WITH in parentheses stops there.
            ("(WITH payments AS (SELECT 2 AS id) SELECT * FROM payments) UNION ALL SELECT id FROM payments", &[Pg, Duck, Ch]),
            ("SELECT 1 AS id UNION ALL (WITH payments AS (SELECT 2 AS id) SELECT * FROM payments) UNION ALL SELECT id FROM payments", &[Pg, Duck, Ch]),
            // Case: told apart on these.
            ("WITH PAYMENTS AS (SELECT 2 AS id) SELECT * FROM payments", &[My, Ms, Ch]),
            ("WITH payments AS (SELECT 2 AS id) SELECT * FROM PAYMENTS", &[My, Ms, Ch]),
            // A quoted name is exact where unquoted folds.
            ("WITH \"PAYMENTS\" AS (SELECT 2 AS id) SELECT * FROM payments", &[Pg, My, Ms, Ch]),
            // A write's own table, except SQL Server's, which writes through the query.
            ("WITH payments AS (SELECT 1) DELETE FROM payments", &[Pg, My, Lite, Duck]),
            ("WITH payments AS (SELECT 1) UPDATE payments SET card = 'x'", &[Pg, My, Lite, Duck]),
            ("WITH payments AS (SELECT 1) INSERT INTO payments VALUES (1)", &[Pg, My, Lite, Duck, Ch]),
            ("WITH payments AS (SELECT 1) UPDATE payments SET card = 'x' FROM payments", &[Lite, Duck]),
        ];
        for (sql, refused) in cases {
            for d in all {
                let cx = Context { rules: &r, connection: "c", dialect: d, tables: &tables, routines: &routines, views: &[] };
                let write = ["DELETE", "UPDATE", "INSERT"].iter().any(|w| sql.contains(w));
                let got = check(sql, write, &cx);
                if refused.contains(&d) {
                    assert!(got.is_err(), "{d:?} would read the table: {sql}: {got:?}");
                }
            }
        }
        // ... and a name that means a query stays one.
        for (sql, on) in [
            ("WITH x AS (SELECT 1 AS a) SELECT * FROM x", &all[..]),
            ("WITH payments AS (SELECT 1 AS a) SELECT * FROM payments", &all[..]),
            ("WITH payments AS (SELECT 1 AS a) SELECT * FROM (SELECT * FROM payments) q", &all[..]),
            ("WITH payments AS (SELECT 1 AS a) SELECT * FROM payments UNION ALL SELECT * FROM payments", &all[..]),
            ("WITH x AS (SELECT 1 AS a), y AS (SELECT * FROM x) SELECT * FROM y", &all[..]),
            ("WITH payments AS (SELECT 1 AS a), y AS (WITH payments AS (SELECT * FROM payments) SELECT * FROM payments) SELECT * FROM y", &[Pg, My, Duck, Ch][..]),
            ("WITH RECURSIVE x AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM x WHERE n < 3) SELECT * FROM x", &[Pg, My, Lite, Duck, Ch][..]),
            ("WITH x AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM x WHERE n < 3) SELECT * FROM x", &[Ms, Lite][..]),
            ("WITH x AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM (SELECT n FROM x) q WHERE n < 3) SELECT * FROM x", &[Ms, Lite][..]),
            ("WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT 1 AS x) SELECT * FROM a", &[Pg, Lite][..]),
            ("WITH a AS (SELECT * FROM b), b AS (SELECT 1 AS x) SELECT * FROM a", &[Lite][..]),
            ("WITH x AS (SELECT 1 AS a) SELECT * FROM X", &[Pg, Lite, Duck][..]),
        ] {
            for d in on {
                let cx = Context { rules: &r, connection: "c", dialect: *d, tables: &tables, routines: &routines, views: &[] };
                assert_eq!(check(sql, false, &cx).map(|c| c.sql), Ok(sql.to_string()), "{d:?}: {sql}");
            }
        }
        // The table under its query's name is narrowed like any other; the query's name stays.
        let cx = Context { rules: &r, connection: "c", dialect: Pg, tables: &tables, routines: &routines, views: &[] };
        assert_eq!(check("WITH users AS (SELECT * FROM users) SELECT * FROM users", false, &cx).unwrap().sql, "WITH users AS (SELECT * FROM (SELECT id, name, email, created_at FROM public.users) users) SELECT * FROM users");
        let named = tables_named("WITH users AS (SELECT * FROM users) SELECT * FROM users", Pg);
        assert_eq!(named, vec![(None, "users".to_string())], "looked up, or the rules have nothing to go by");
    }

    #[test]
    fn plans_that_print_rows_are_refused_wherever_rules_apply() {
        let routines = HashSet::new();
        let on = |view, d, sql: &str| {
            let r = AgentRules { view, ..Default::default() };
            let cx = Context { rules: &r, connection: "c", dialect: d, tables: &tables, routines: &routines, views: &[] };
            check(sql, false, &cx)
        };
        for view in [AgentView::Structure, AgentView::Masked] {
            for sql in [
                "EXPLAIN SYNTAX run_query_tree_passes=1 SELECT (SELECT card FROM payments LIMIT 1)",
                "EXPLAIN PLAN actions=1 SELECT 1 WHERE (SELECT card FROM payments LIMIT 1) = 'x'",
                "EXPLAIN QUERY TREE WITH (SELECT card FROM payments LIMIT 1) AS c SELECT c",
                "EXPLAIN PLAN actions=1 SELECT 1 FROM numbers(1) WHERE 'x' IN payments",
            ] {
                assert!(on(view, Dialect::ClickHouse, sql).is_err_and(|e| e.contains("EXPLAIN")), "{view:?}: {sql}");
            }
            for sql in [
                "EXPLAIN FORMAT=TREE SELECT o.id FROM orders o JOIN payments p ON p.id = 1 WHERE o.notes = p.card",
                "EXPLAIN FORMAT=TREE SELECT o.id FROM orders o, payments p WHERE p.id = 1 AND o.notes = p.card",
                "EXPLAIN FORMAT=JSON SELECT id FROM orders WHERE notes = (SELECT card FROM payments WHERE id = 1)",
                "EXPLAIN FOR CONNECTION 5",
                "EXPLAIN FORMAT=JSON FOR CONNECTION 5",
            ] {
                assert!(on(view, Dialect::MySql, sql).is_err_and(|e| e.contains("EXPLAIN")), "{view:?}: {sql}");
            }
            assert!(on(view, Dialect::ClickHouse, "EXPLAIN SELECT id FROM orders").is_ok());
            assert!(on(view, Dialect::MySql, "EXPLAIN FORMAT=TREE SELECT id FROM orders WHERE id = 1").is_ok());
            for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::DuckDb] {
                assert!(on(view, d, "EXPLAIN SELECT (SELECT card FROM payments LIMIT 1)").is_ok(), "{d:?} reads no row for a plan");
            }
        }
        assert!(on(AgentView::Data, Dialect::ClickHouse, "EXPLAIN SELECT (SELECT 1)").is_ok(), "nothing kept");
        assert!(crate::explain::analyzes("EXPLAIN (\"analyze\") SELECT 1"));
        assert!(crate::explain::analyzes("EXPLAIN (\"analyze\" \"on\") SELECT 1"));
        assert!(crate::explain::analyzes("EXPLAIN (U&\"analyze\") SELECT 1"));
        assert!(!crate::explain::analyzes("EXPLAIN (\"analyze\" \"off\") SELECT 1"));
        assert_eq!(guard::classify_in("EXPLAIN (\"analyze\") DELETE FROM t", Dialect::Postgres).effect, Effect::Write, "it runs what it explains");
        assert_eq!(guard::classify_in("EXPLAIN (\"analyze\" \"off\") DELETE FROM t", Dialect::Postgres).effect, Effect::Read);
    }

    #[test]
    fn structure_only_keeps_rows_from_statements_of_no_known_kind() {
        let r = AgentRules { view: AgentView::Structure, ..Default::default() };
        let routines = HashSet::new();
        for (d, sql) in [
            (Dialect::Postgres, "EXECUTE leak"),
            (Dialect::MySql, "EXECUTE leak"),
            (Dialect::DuckDb, "EXECUTE leak"),
            (Dialect::TSql, "DBCC SHOW_STATISTICS('t', 'i')"),
            (Dialect::TSql, "READTEXT t.c 0 0 1"),
            (Dialect::TSql, "RECEIVE * FROM q"),
            (Dialect::ClickHouse, "WATCH v"),
        ] {
            let cx = Context { rules: &r, connection: "c", dialect: d, tables: &tables, routines: &routines, views: &[] };
            assert!(check(sql, true, &cx).is_err_and(|e| e.contains("structure only")), "{d:?}: {sql}");
        }
        let cx = Context { rules: &r, connection: "c", dialect: Dialect::Postgres, tables: &tables, routines: &routines, views: &[] };
        for sql in ["SET search_path = x", "ANALYZE t", "CREATE TABLE t (a int)", "PREPARE x AS SELECT 1"] {
            assert!(check(sql, true, &cx).is_ok(), "{sql}");
        }
    }

    #[test]
    fn a_star_stands_alone_or_in_count() {
        let hidden = rules();
        let mut masked = AgentRules { view: AgentView::Masked, ..Default::default() };
        masked.sensitive.push(SensitiveCol { col: "customers.email".into(), how: Mask::PARTIAL, on: true });
        let routines = HashSet::new();
        let on = |r: &AgentRules, d, sql: &str, write| {
            let cx = Context { rules: r, connection: "c", dialect: d, tables: &tables, routines: &routines, views: &[] };
            check(sql, write, &cx)
        };
        use Dialect::{ClickHouse as Ch, DuckDb as Duck, MySql as My, Postgres as Pg, TSql as Ms};
        for (d, inside) in [
            (Ch, "tuple(*)"),
            (Ch, "concat(*)"),
            (Ch, "formatRow('CSV', *)"),
            (Ch, "tuple(1, *)"),
            (Ch, "tuple(customers.*)"),
            (Ch, "tuple(1, customers.*)"),
            (Ch, "COLUMNS('.*')"),
            (Ch, "tuple(COLUMNS('.*'))"),
            (Ch, "`COLUMNS`('.*')"),
            (Ch, "(*)"),
            (Ch, "((*))"),
            (Ch, "(*, 1)"),
            (Ch, "[*]"),
            (Ch, "uniq(*)"),
            (Ch, "count(DISTINCT *)"),
            (Ch, "* REPLACE(lower(name) AS name)"),
            (Ch, "* EXCEPT(id) REPLACE(lower(name) AS name)"),
            (Ch, "customers.* REPLACE(lower(name) AS name)"),
            (Duck, "COLUMNS('.*')"),
            (Duck, "COLUMNS(*)"),
            (Duck, "struct_pack(*)"),
            (Duck, "* REPLACE (lower(name) AS name)"),
            (Duck, "* RENAME (name AS n)"),
            (Duck, "* EXCLUDE (id) REPLACE (lower(name) AS name)"),
            (Pg, "row(customers.*)"),
            (Pg, "to_jsonb(customers.*)"),
            (Pg, "(customers.*)::text"),
            (Ms, "BINARY_CHECKSUM(*)"),
            (Ms, "CHECKSUM(*)"),
            (My, "CONCAT_WS(',', customers.*)"),
        ] {
            let sql = format!("SELECT {inside} FROM customers");
            assert!(on(&masked, d, &sql, false).is_err(), "{d:?}: {sql}");
        }
        for sql in ["SELECT id FROM customers ORDER BY *", "SELECT id FROM customers LIMIT 1 BY *", "SELECT id FROM customers GROUP BY *"] {
            assert!(on(&masked, Ch, sql, false).is_err(), "{sql}");
        }
        // What copies rows reads the table, narrowed or not.
        for (d, select) in [
            (Ch, "tuple(*) FROM users"),
            (Ch, "COLUMNS('.*') FROM users"),
            (Ch, "(*) FROM users"),
            (Ch, "* REPLACE(toString(id) AS id) FROM users"),
            (Ch, "* EXCEPT(id) FROM users"),
            (Duck, "* EXCLUDE (id) FROM users"),
            (Ms, "TOP 5 * FROM users"),
            (Ms, "TOP (5) * FROM users"),
            (My, "SQL_NO_CACHE * FROM users"),
        ] {
            let sql = format!("CREATE TABLE copy AS SELECT {select}");
            assert!(on(&hidden, d, &sql, true).is_err(), "{d:?}: {sql}");
        }
        for (r, d, sql) in [
            (&masked, Ch, "SELECT * FROM customers"),
            (&masked, Ch, "SELECT customers.* FROM customers"),
            (&masked, Ch, "SELECT *, id FROM customers"),
            (&masked, Ch, "SELECT id, * FROM customers"),
            (&masked, Ch, "SELECT count(*) FROM customers"),
            (&masked, Ch, "SELECT COUNT(*), id FROM customers GROUP BY id"),
            (&masked, Ch, "SELECT count(*) OVER () FROM customers"),
            (&masked, Ch, "SELECT 2 * id FROM customers"),
            (&masked, Ch, "SELECT (2 * id) FROM customers"),
            (&masked, Ch, "SELECT id * (id + 1) FROM customers"),
            (&masked, Ch, "SELECT (id) * 2 FROM customers"),
            (&masked, Ch, "SELECT id FROM customers WHERE EXISTS (SELECT * FROM orders)"),
            (&masked, Ch, "SELECT * FROM (SELECT * FROM customers)"),
            (&hidden, Ch, "SELECT * EXCEPT(name) FROM users"),
            (&masked, Pg, "SELECT count(*) FILTER (WHERE id > 1) FROM customers"),
            (&masked, Ms, "SELECT TOP 5 * FROM customers"),
            (&masked, Ms, "SELECT count_big(*) FROM customers"),
            (&hidden, Pg, "SELECT * FROM users"),
            (&hidden, Pg, "SELECT u.* FROM users u"),
            // Nothing of those tables is kept.
            (&masked, Ch, "SELECT tuple(*) FROM orders"),
            (&hidden, Ch, "SELECT COLUMNS('.*') FROM orders"),
        ] {
            assert!(on(r, d, sql, false).is_ok(), "{d:?}: {sql}: {:?}", on(r, d, sql, false));
        }
    }

    #[test]
    fn names_are_compared_as_the_server_reads_them() {
        let r = rules();
        let routines = HashSet::new();
        let cx = |d| Context { rules: &r, connection: "c", dialect: d, tables: &tables, routines: &routines, views: &[] };
        for sql in [
            "SELECT \"in\"((toUInt64(1), 'guess'), payments)",
            "SELECT `in`((toUInt64(1), 'guess'), payments)",
            "SELECT `i\\x6E`((toUInt64(1), 'guess'), payments)",
            "SELECT “in”((toUInt64(1), 'guess'), payments)",
            "SELECT `globalI\\x6E`(id, payments) FROM orders",
            "SELECT `n\\x6FtIn`(id, public.payments) FROM orders",
        ] {
            assert!(check(sql, false, &cx(Dialect::ClickHouse)).is_err_and(|e| e.contains("IN with a table")), "{sql}");
        }
        // Run SQL from a string, read a table whole: however they are spelled.
        for sql in ["SELECT U&\"query\\005Fto_xml\"('select 1', true, true, '')", "SELECT U&\"dblink\"('x', 'select 1')", "SELECT U&\"table_to_xml\"('payments', true, true, '')"] {
            assert!(check(sql, false, &cx(Dialect::Postgres)).is_err(), "{sql}");
        }
        for sql in ["ALTER TABLE U&\"pay\\006Dents\" RENAME TO pays", "DROP TABLE \"payments\"", "ALTER TABLE users RENAME COLUMN U&\"password\\005Fhash\" TO note"] {
            assert!(check(sql, true, &cx(Dialect::Postgres)).is_err(), "{sql}");
        }
        assert!(check("SELECT * FROM U&\"sys\".\"objects\"()", false, &cx(Dialect::Postgres)).is_err(), "a catalog's function");
        assert!(check("SELECT * FROM [sys].[dm_exec_cursors](0)", false, &cx(Dialect::TSql)).is_err(), "a catalog's function");
        for (d, sql) in [
            (Dialect::Postgres, "SELECT U&\"pg_terminate_backend\"(1)"),
            (Dialect::Postgres, "SELECT U&\"pg\\005Fread_file\"('x')"),
            (Dialect::Postgres, "SELECT U&\"query_to_xml\"('select 1', true, true, '')"),
            (Dialect::MySql, "SELECT `get_lock`('a', 1)"),
            (Dialect::TSql, "SELECT [openrowset]('a', 'b', 'c')"),
        ] {
            assert!(guard::agent_read_refusal_in(sql, d).is_some(), "{d:?}: {sql}");
        }
        assert!(guard::server_file_access("SELECT U&\"pg_read_file\"('x')", Dialect::Postgres).is_some());
        assert!(guard::runs_code("SELECT \"query_to_xml\"('select 1', true, true, '')", Dialect::Postgres));
        assert!(guard::runs_code("SELECT U&\"query_to_xml\"('select 1', true, true, '')", Dialect::Postgres));
    }

    #[test]
    fn clickhouse_settings_that_change_names_are_refused() {
        let r = rules();
        let routines = HashSet::new();
        let cx = Context { rules: &r, connection: "c", dialect: Dialect::ClickHouse, tables: &tables, routines: &routines, views: &[] };
        for sql in ["SELECT id FROM orders SETTINGS enable_global_with_statement = 0", "SELECT id FROM orders SETTINGS `enable_analyzer` = 0", "SELECT id FROM orders SETTINGS allow_experimental_analyzer = 0", "SET enable_analyzer = 0"] {
            assert!(check(sql, true, &cx).is_err_and(|e| e.contains("setting")), "{sql}");
        }
        assert!(check("SELECT id FROM orders SETTINGS max_threads = 1", true, &cx).is_ok());
    }

    #[test]
    fn named_tables() {
        let mut got = tables_named("WITH x AS (SELECT * FROM a) SELECT * FROM x JOIN s.b ON true; DELETE FROM c WHERE id = 1", Dialect::Postgres);
        got.sort();
        assert_eq!(got, vec![(None, "a".to_string()), (None, "c".to_string()), (Some("s".to_string()), "b".to_string())]);
    }
}
