//! Local autocomplete, signature help and go to definition: tables, columns, functions, types
//! and keywords from the cached schema and catalog, no server trip.
//!
//! The statement under the cursor is read into scopes (`sql::scope`): CTE names and columns,
//! subquery aliases and their columns, `JOIN … USING` and lateral sources all resolve. What is
//! offered depends on where the cursor is: tables after FROM/JOIN/UPDATE/INTO, columns and
//! functions in expressions, types after `::` and `CAST(… AS`, enum labels inside a string
//! compared with an enum column, and keywords ranked by what usually comes next.

use std::collections::HashSet;
use std::ops::Range;

use crate::schema::catalog::BUILTIN_TYPES;
use crate::schema::{FuncSig, SchemaSnapshot};
use crate::sql::lexer::{KEYWORDS, Token, TokenKind, eq_kw, is_ident_cont, tokenize_in, unterminated_in};
use crate::sql::scope::{self, Analysis, Clause, SourceKind};
use crate::sql::{Dialect, ident_value, ident_value_in, quote_ident, split::split_tokens_in};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CandidateKind {
    Column,
    Table,
    View,
    Cte,
    Schema,
    Function,
    Type,
    /// An enum label inside a string.
    Value,
    Keyword,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub label: String,
    pub kind: CandidateKind,
    /// Column type, the schema of a table, a function's result type.
    pub detail: String,
    /// Text inserted in place of `Completion::replace`.
    pub insert: String,
    /// Where the cursor lands inside `insert` (`None`: after it), e.g. between a call's parens.
    pub caret: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Completion {
    /// Byte range of the partial word being completed (empty at a fresh position).
    pub replace: Range<usize>,
    pub items: Vec<Candidate>,
}

pub const MAX_ITEMS: usize = 50;

/// Keywords that start a statement.
const START: &[&str] = &[
    "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "CREATE", "ALTER", "DROP", "EXPLAIN", "BEGIN", "COMMIT", "ROLLBACK",
    "SET", "SHOW", "TRUNCATE", "GRANT", "REVOKE", "VACUUM", "ANALYZE", "COPY", "CALL", "DO", "VALUES", "TABLE", "LISTEN",
    "NOTIFY", "SUBSCRIBE",
];
/// After a source in FROM.
const AFTER_SOURCE: &[&str] = &[
    "WHERE", "JOIN", "LEFT", "INNER", "RIGHT", "FULL", "CROSS", "NATURAL", "ON", "USING", "GROUP", "ORDER", "LIMIT", "AS",
    "UNION", "OFFSET", "HAVING", "WINDOW", "FOR", "LATERAL",
];
/// Keywords that start an expression (when no operand is complete yet).
const EXPR_START: &[&str] = &[
    "NOT", "EXISTS", "CASE", "NULL", "TRUE", "FALSE", "DISTINCT", "CAST", "INTERVAL", "ARRAY", "CURRENT_DATE",
    "CURRENT_TIMESTAMP", "CURRENT_USER", "SELECT",
];
const OBJECTS: &[&str] = &[
    "TABLE", "VIEW", "INDEX", "FUNCTION", "SCHEMA", "SEQUENCE", "TYPE", "MATERIALIZED", "EXTENSION", "TRIGGER", "PROCEDURE",
    "ROLE", "DATABASE", "OR", "UNIQUE", "TEMP", "TEMPORARY", "IF",
];

/// What usually follows a complete operand in each clause.
fn follow(clause: Clause) -> &'static [&'static str] {
    match clause {
        Clause::Select => &["FROM", "AS", "AND", "OR", "IS", "NOT", "IN", "LIKE", "ILIKE", "BETWEEN", "OVER", "FILTER", "INTO"],
        Clause::Where | Clause::Having => &[
            "AND", "OR", "IS", "NOT", "IN", "LIKE", "ILIKE", "BETWEEN", "GROUP", "ORDER", "LIMIT", "OFFSET", "UNION", "RETURNING",
            "SIMILAR", "ISNULL", "NOTNULL",
        ],
        Clause::On => &["AND", "OR", "IS", "NOT", "IN", "JOIN", "LEFT", "INNER", "WHERE", "GROUP", "ORDER", "LIMIT"],
        Clause::GroupBy => &["HAVING", "ORDER", "LIMIT", "OFFSET", "UNION"],
        Clause::OrderBy => &["ASC", "DESC", "NULLS", "LIMIT", "OFFSET", "FETCH", "FOR"],
        Clause::Set => &["WHERE", "FROM", "RETURNING"],
        Clause::Values => &["ON", "RETURNING"],
        Clause::Returning => &["AS"],
        Clause::From => AFTER_SOURCE,
        Clause::Window | Clause::Other => &["OFFSET", "FETCH", "FOR", "UNION", "AND", "OR"],
    }
}

enum Ctx {
    /// After `x.` (or `s.x.`): columns of a source or table, or a schema's tables.
    Qualified { schema: Option<String>, name: String },
    Tables,
    Types,
    /// Inside `JOIN t USING (`.
    UsingCols,
    /// Inside `INSERT INTO t (`.
    InsertCols,
    /// `UPDATE t SET |`.
    SetTargets,
    Expr { clause: Clause, operand: bool, in_case: bool },
    AfterSource,
    Start,
    Keywords(&'static [&'static str]),
}

/// Candidates for the cursor at byte `cursor` of `src`.
pub fn complete(src: &str, cursor: usize, schema: &SchemaSnapshot) -> Completion {
    let cursor = cursor.min(src.len());
    let tokens = tokenize_in(src, schema.dialect);
    let empty = Completion { replace: cursor..cursor, items: vec![] };

    // Inside a string: enum labels, if the string is compared with an enum column.
    if let Some(i) = tokens.iter().position(|t| t.start < cursor && (cursor < t.end || (cursor == t.end && unterminated_in(src, t, schema.dialect).is_some())))
        && matches!(tokens[i].kind, TokenKind::String | TokenKind::DollarString | TokenKind::LineComment | TokenKind::BlockComment | TokenKind::QuotedIdent)
    {
        if tokens[i].kind == TokenKind::String {
            return enum_labels(src, &tokens, i, cursor, schema).unwrap_or(empty);
        }
        return empty;
    }
    // A line comment that ends right at the cursor.
    if tokens.iter().any(|t| t.end == cursor && t.kind == TokenKind::LineComment) {
        return empty;
    }

    // The partial word ends at the cursor.
    let b = src.as_bytes();
    let mut ws = cursor;
    while ws > 0 && is_ident_cont(b[ws - 1]) {
        ws -= 1;
    }
    // Do not complete right after a number like `1`, nor a parameter like `$1`.
    if ws < cursor && b[ws].is_ascii_digit() || ws > 0 && b[ws - 1] == b'$' {
        return empty;
    }
    let partial = &src[ws..cursor];

    let d = schema.dialect;
    let stmt_tokens = statement_tokens(src, &tokens, ws, d);
    let before: Vec<&Token> = stmt_tokens.iter().filter(|t| t.end <= ws).collect();
    let analysis = scope::analyze_in(src, &stmt_tokens, d);
    let scope_id = analysis.scope_at(ws).unwrap_or(0);
    let ctx = context(src, &before, d);

    // (rank, order within a likely-keyword list, candidate)
    let mut items: Vec<(u8, u16, Candidate)> = Vec::new();
    let mut kw: Vec<(Candidate, u8, u16)> = Vec::new();
    let mut push = |c: Candidate, prio: u8| {
        if let Some(score) = match_score(&c.label, partial) {
            items.push((score * 8 + prio.min(7), u16::MAX, c));
        }
    };
    let keywords = |kw: &mut Vec<(Candidate, u8, u16)>, first: &[&str], rest: Option<u8>| {
        // The likely ones first, in their list's order.
        let rank = |k: &str| first.iter().position(|f| *f == k);
        let prio = |i: usize| (i / 3).min(2) as u8;
        if partial.is_empty() && rest.is_some() {
            // Forced at a fresh position: only the likely ones.
            for (i, k) in first.iter().enumerate() {
                kw.push((keyword(k, partial), prio(i), i as u16));
            }
            return;
        }
        for k in KEYWORDS {
            if let Some(i) = rank(k) {
                kw.push((keyword(k, partial), prio(i), i as u16));
            } else if let Some(p) = rest {
                kw.push((keyword(k, partial), p, u16::MAX));
            }
        }
        for (i, k) in first.iter().enumerate().filter(|(_, k)| !KEYWORDS.contains(k)) {
            kw.push((keyword(k, partial), prio(i), i as u16));
        }
    };
    let no_scopes = analysis.scopes.is_empty();

    match ctx {
        Ctx::Qualified { schema: qs, name } => {
            let mut done = false;
            if qs.is_none() && !no_scopes {
                if let Some((s, i)) = analysis.find_source(scope_id, &name)
                    && let Some(cols) = analysis.source_columns(s, &analysis.scopes[s].sources[i], schema)
                {
                    for (n, t) in cols {
                        push(column(d, &n, &t), 0);
                    }
                    done = true;
                }
            }
            if !done && let Some(rel) = schema.resolve(qs.as_deref(), &name) {
                for c in schema.columns_of(rel) {
                    push(column(d, &c.name, &c.type_name), 0);
                }
                done = true;
            }
            if !done && qs.is_none() && let Some(s) = schema.is_schema(&name) {
                for r in schema.relations.iter().filter(|r| r.schema == s) {
                    push(table_candidate(d, r, false), 0);
                }
                if let Some(cat) = &schema.catalog {
                    let mut seen = HashSet::new();
                    for f in cat.functions.iter().filter(|f| f.schema == s && f.kind != 'p') {
                        if seen.insert(f.name.as_str()) {
                            push(function(d, f), 1);
                        }
                    }
                }
            }
        }
        Ctx::Tables => {
            if !no_scopes {
                for c in analysis.visible_ctes(scope_id) {
                    let name = &analysis.ctes[c].name;
                    push(Candidate { label: name.clone(), kind: CandidateKind::Cte, detail: "cte".into(), insert: quoted(d, name), caret: None }, 0);
                }
            }
            for r in &schema.relations {
                let qualify = !schema.search_path.contains(&r.schema);
                push(table_candidate(d, r, qualify), if qualify { 2 } else { 0 });
            }
            for s in &schema.schemas {
                push(Candidate { label: s.clone(), kind: CandidateKind::Schema, detail: "schema".into(), insert: quoted(d, s), caret: None }, 3);
            }
        }
        Ctx::Types => {
            if let Some(cat) = &schema.catalog {
                for t in &cat.types {
                    let qualify = !schema.search_path.contains(&t.schema);
                    let insert = if qualify { format!("{}.{}", quote_ident(&t.schema), quote_ident(&t.name)) } else { quote_ident(&t.name) };
                    let detail = match t.kind {
                        'e' => "enum",
                        'd' => "domain",
                        'c' => "composite",
                        'r' | 'm' => "range",
                        _ => "type",
                    };
                    push(Candidate { label: t.name.clone(), kind: CandidateKind::Type, detail: detail.into(), insert, caret: None }, 0);
                }
            }
            for t in BUILTIN_TYPES {
                push(Candidate { label: t.to_string(), kind: CandidateKind::Type, detail: String::new(), insert: t.to_string(), caret: None }, 1);
            }
        }
        Ctx::UsingCols if !no_scopes => {
            let s = &analysis.scopes[scope_id];
            let cols_of = |i: usize| analysis.source_columns(scope_id, &s.sources[i], schema).unwrap_or_default();
            if let Some(last) = s.sources.iter().rposition(|x| x.name_range.end <= ws) {
                let right = cols_of(last);
                let left: HashSet<String> = (0..last).flat_map(|i| cols_of(i).into_iter().map(|c| c.0)).collect();
                let common: Vec<_> = right.iter().filter(|c| left.contains(&c.0)).cloned().collect();
                for (n, t) in if common.is_empty() { right } else { common } {
                    push(column(d, &n, &t), 0);
                }
            }
        }
        Ctx::InsertCols | Ctx::SetTargets if !no_scopes => {
            if let Some(src) = analysis.scopes[scope_id].sources.first() {
                for (n, t) in analysis.source_columns(scope_id, src, schema).unwrap_or_default() {
                    push(column(d, &n, &t), 0);
                }
            }
        }
        Ctx::UsingCols | Ctx::InsertCols | Ctx::SetTargets => {}
        Ctx::Expr { clause, operand, in_case } => {
            let shift = if operand { 3 } else { 0 };
            if !no_scopes {
                for (depth, s) in analysis.chain(scope_id).into_iter().enumerate() {
                    for src in &analysis.scopes[s].sources {
                        for (n, t) in analysis.source_columns(s, src, schema).unwrap_or_default() {
                            push(column(d, &n, &t), (depth.min(1) as u8) + shift);
                        }
                    }
                }
                if matches!(clause, Clause::OrderBy | Clause::GroupBy) {
                    for item in &analysis.scopes[scope_id].items {
                        if let scope::Item::Named(n) = item {
                            push(column(d, n, ""), shift);
                        }
                    }
                }
            }
            if !partial.is_empty()
                && let Some(cat) = &schema.catalog
            {
                let mut seen = HashSet::new();
                for f in &cat.functions {
                    if f.kind == 'p' || !(schema.search_path.contains(&f.schema) || f.schema == "pg_catalog") {
                        continue;
                    }
                    if seen.insert(f.name.as_str()) {
                        push(function(d, f), 2 + shift);
                    }
                }
            }
            let mut first: Vec<&str> = if operand { follow(clause).to_vec() } else { Vec::new() };
            if in_case {
                first.extend(["WHEN", "THEN", "ELSE", "END"]);
            }
            if !partial.is_empty() {
                let rest = if operand { 4 } else { 3 };
                keywords(&mut kw, &first, Some(rest));
                if !operand {
                    for k in EXPR_START {
                        push(keyword(k, partial), 3);
                    }
                    for r in &schema.relations {
                        if schema.search_path.contains(&r.schema) {
                            push(table_candidate(d, r, false), 4);
                        }
                    }
                }
            } else if operand {
                keywords(&mut kw, &first, Some(4));
            }
        }
        Ctx::AfterSource => keywords(&mut kw, AFTER_SOURCE, None),
        Ctx::Start => keywords(&mut kw, START, Some(4)),
        Ctx::Keywords(list) => keywords(&mut kw, list, Some(5)),
    }
    for (c, prio, order) in kw {
        if let Some(score) = match_score(&c.label, partial) {
            items.push((score * 8 + prio.min(7), order, c));
        }
    }

    items.sort_by(|a, b| {
        a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then_with(|| a.2.label.len().cmp(&b.2.label.len())).then_with(|| a.2.label.cmp(&b.2.label))
    });
    let mut out: Vec<Candidate> = Vec::with_capacity(MAX_ITEMS);
    let mut seen: HashSet<(String, CandidateKind)> = HashSet::new();
    for (_, _, c) in items {
        if out.len() == MAX_ITEMS {
            break;
        }
        if seen.insert((c.label.clone(), c.kind)) {
            out.push(c);
        }
    }
    // A lone exact match of what is already typed is noise.
    if out.len() == 1 && out[0].label == partial {
        out.clear();
    }
    Completion { replace: ws..cursor, items: out }
}

/// The significant tokens of the statement around byte `at` (between the top-level `;`s).
fn statement_tokens(src: &str, tokens: &[Token], at: usize, d: Dialect) -> Vec<Token> {
    let mut seg = 0..src.len();
    for s in split_tokens_in(src, tokens, d.script()).iter().filter(|s| s.full.end > s.range.end) {
        if s.full.end <= at {
            seg.start = s.full.end;
        } else {
            seg.end = s.full.end;
            break;
        }
    }
    tokens.iter().filter(|t| !t.kind.is_trivia() && t.start >= seg.start && t.end <= seg.end).copied().collect()
}

fn keyword(k: &str, partial: &str) -> Candidate {
    Candidate { label: k.to_string(), kind: CandidateKind::Keyword, detail: String::new(), insert: keyword_case(k, partial), caret: None }
}

/// The name a token denotes, as the scope analysis reads it (MySQL: backticks, no case).
fn ident(d: Dialect, src: &str, t: &Token) -> String {
    if d == Dialect::Postgres { ident_value(src, t) } else { ident_value_in(src, t, d).to_lowercase() }
}

/// A name as inserted: quoted the way the connection's engine needs it.
fn quoted(d: Dialect, name: &str) -> String {
    if d == Dialect::Postgres { quote_ident(name) } else { d.quote_ident(name) }
}

fn column(d: Dialect, name: &str, ty: &str) -> Candidate {
    Candidate { label: name.to_string(), kind: CandidateKind::Column, detail: ty.to_string(), insert: quoted(d, name), caret: None }
}

fn function(d: Dialect, f: &FuncSig) -> Candidate {
    let name = quoted(d, &f.name);
    let insert = format!("{name}()");
    // Inside the parens, unless it takes no arguments at all.
    let caret = (!f.args.is_empty()).then_some(insert.len() - 1);
    let detail = f.result.strip_prefix("SETOF ").unwrap_or(&f.result).to_string();
    Candidate { label: f.name.clone(), kind: CandidateKind::Function, detail, insert, caret }
}

fn table_candidate(d: Dialect, r: &crate::schema::Relation, qualify: bool) -> Candidate {
    Candidate {
        label: r.name.clone(),
        kind: if r.kind.is_view() { CandidateKind::View } else { CandidateKind::Table },
        detail: r.schema.clone(),
        insert: if qualify { format!("{}.{}", quoted(d, &r.schema), quoted(d, &r.name)) } else { quoted(d, &r.name) },
        caret: None,
    }
}

/// Keywords follow the case the user is typing in.
fn keyword_case(k: &str, partial: &str) -> String {
    if partial.chars().any(|c| c.is_ascii_uppercase()) { k.to_string() } else { k.to_ascii_lowercase() }
}

/// 0 = exact-case prefix, 1 = prefix ignoring case, 2 = substring, 3 = subsequence.
fn match_score(label: &str, partial: &str) -> Option<u8> {
    if partial.is_empty() || label.starts_with(partial) {
        return Some(0);
    }
    let l = label.to_lowercase();
    let p = partial.to_lowercase();
    if l.starts_with(&p) {
        return Some(1);
    }
    if l.contains(&p) {
        return Some(2);
    }
    let mut it = l.chars();
    if p.len() >= 2 && p.chars().all(|c| it.any(|x| x == c)) {
        return Some(3);
    }
    None
}

fn is_name(t: &Token) -> bool {
    matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent)
}

fn is_punct(src: &str, t: &Token, c: &str) -> bool {
    t.kind == TokenKind::Punct && t.text(src) == c
}

fn is_kw(src: &str, t: &Token, k: &str) -> bool {
    matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && eq_kw(t.text(src), k)
}

/// Index of the innermost `(` left open in `toks`.
fn open_paren(src: &str, toks: &[&Token]) -> Option<usize> {
    let mut depth = 0;
    for (i, t) in toks.iter().enumerate().rev() {
        if is_punct(src, t, ")") {
            depth += 1;
        } else if is_punct(src, t, "(") {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

fn context(src: &str, before: &[&Token], d: Dialect) -> Ctx {
    let n = before.len();
    if n == 0 {
        return Ctx::Start;
    }
    let last = before[n - 1];
    if n >= 2 && is_punct(src, last, ".") && (is_name(before[n - 2]) || before[n - 2].kind == TokenKind::Keyword) {
        let name = ident(d, src, before[n - 2]);
        let schema = (n >= 4 && is_punct(src, before[n - 3], ".") && is_name(before[n - 4])).then(|| ident(d, src, before[n - 4]));
        return Ctx::Qualified { schema, name };
    }
    if last.text(src) == "::" {
        return Ctx::Types;
    }
    let po = open_paren(src, before);
    let lo = po.map_or(0, |p| p + 1);
    if is_kw(src, last, "AS") && po.is_some_and(|p| p > 0 && is_kw(src, before[p - 1], "CAST")) {
        return Ctx::Types;
    }
    if let Some(p) = po {
        let subquery = before[lo..].iter().any(|t| is_kw(src, t, "SELECT") || is_kw(src, t, "VALUES"));
        if !subquery {
            if p > 0 && is_kw(src, before[p - 1], "USING") {
                return Ctx::UsingCols;
            }
            // INSERT INTO [s.]t (
            let mut k = p;
            if k > 0 && is_name(before[k - 1]) {
                k -= 1;
                if k >= 2 && is_punct(src, before[k - 1], ".") {
                    k -= 2;
                }
                if k > 0 && is_kw(src, before[k - 1], "INTO") {
                    return Ctx::InsertCols;
                }
            }
        }
    }
    // The clause keyword at the cursor's paren depth.
    let mut clause = Clause::Other;
    let mut clause_kw = "";
    let mut depth = 0;
    let mut cases = 0i32;
    for t in before[lo..].iter().rev() {
        if is_punct(src, t, ")") {
            depth += 1;
            continue;
        }
        if is_punct(src, t, "(") {
            depth -= 1;
            continue;
        }
        if depth > 0 || t.kind != TokenKind::Keyword {
            continue;
        }
        let w = t.text(src).to_ascii_uppercase();
        if w == "CASE" {
            cases += 1;
        } else if w == "END" {
            cases -= 1;
        }
        let c = match w.as_str() {
            "SELECT" => Clause::Select,
            "FROM" | "JOIN" | "UPDATE" | "INTO" | "USING" => Clause::From,
            "ON" => Clause::On,
            "WHERE" => Clause::Where,
            "GROUP" => Clause::GroupBy,
            "HAVING" => Clause::Having,
            "ORDER" => Clause::OrderBy,
            "SET" => Clause::Set,
            "VALUES" => Clause::Values,
            "RETURNING" => Clause::Returning,
            "LIMIT" | "OFFSET" => Clause::Other,
            _ => continue,
        };
        clause = c;
        clause_kw = match w.as_str() {
            "UPDATE" => "UPDATE",
            "INTO" => "INTO",
            _ => "",
        };
        break;
    }
    let lw = last.text(src).to_ascii_uppercase();
    if last.kind == TokenKind::Keyword {
        match lw.as_str() {
            "FROM" | "JOIN" | "UPDATE" | "INTO" | "TABLE" | "ONLY" | "LATERAL" => return Ctx::Tables,
            "USING" if clause == Clause::From && !clause_kw.is_empty() => return Ctx::Tables,
            "SET" if clause == Clause::Set => return Ctx::SetTargets,
            "GROUP" | "ORDER" | "PARTITION" => return Ctx::Keywords(&["BY"]),
            "INSERT" => return Ctx::Keywords(&["INTO"]),
            "DELETE" => return Ctx::Keywords(&["FROM"]),
            "CREATE" | "DROP" | "ALTER" => return Ctx::Keywords(OBJECTS),
            "LEFT" | "RIGHT" | "FULL" => return Ctx::Keywords(&["JOIN", "OUTER"]),
            "INNER" | "CROSS" | "OUTER" | "NATURAL" => return Ctx::Keywords(&["JOIN"]),
            "UNION" | "INTERSECT" | "EXCEPT" => return Ctx::Keywords(&["SELECT", "ALL"]),
            _ => {}
        }
    }
    if is_punct(src, last, ",") {
        if clause == Clause::From && clause_kw != "UPDATE" {
            return Ctx::Tables;
        }
        if clause == Clause::Set {
            return Ctx::SetTargets;
        }
    }
    let operand = is_name(last)
        || matches!(last.kind, TokenKind::Number | TokenKind::String | TokenKind::Param | TokenKind::DollarString)
        || is_punct(src, last, ")")
        || last.text(src) == "*"
        || (last.kind == TokenKind::Keyword && ["NULL", "TRUE", "FALSE", "END"].contains(&lw.as_str()));
    if clause == Clause::From && operand {
        return if clause_kw == "INTO" { Ctx::Keywords(&["VALUES", "SELECT", "DEFAULT", "OVERRIDING"]) } else if clause_kw == "UPDATE" { Ctx::Keywords(&["SET", "AS"]) } else { Ctx::AfterSource };
    }
    let clause = if clause == Clause::From { Clause::On } else { clause };
    Ctx::Expr { clause, operand, in_case: cases > 0 }
}

/// Inside the string at `tokens[i]`: the labels of the enum column it is compared with
/// (`col = '…'`, `col <> '…'`, `col IN ('a', '…'`).
fn enum_labels(src: &str, tokens: &[Token], i: usize, cursor: usize, snap: &SchemaSnapshot) -> Option<Completion> {
    let cat = snap.catalog.as_ref()?;
    let t = tokens[i];
    if !t.text(src).starts_with('\'') {
        return None;
    }
    let sig: Vec<&Token> = tokens[..i].iter().filter(|t| !t.kind.is_trivia()).collect();
    let mut k = sig.len();
    // Back over `'a', '…'` inside IN ( … ).
    let mut in_list = false;
    while k >= 2 && is_punct(src, sig[k - 1], ",") && sig[k - 2].kind == TokenKind::String {
        k -= 2;
        in_list = true;
    }
    if k >= 2 && is_punct(src, sig[k - 1], "(") && is_kw(src, sig[k - 2], "IN") {
        k -= 2;
        if k >= 1 && is_kw(src, sig[k - 1], "NOT") {
            k -= 1;
        }
    } else if in_list {
        return None;
    } else if k >= 1 && ["=", "<>", "!="].contains(&sig[k - 1].text(src)) {
        k -= 1;
    } else {
        return None;
    }
    // The column: `name` or `qualifier.name`.
    if k == 0 || !is_name(sig[k - 1]) {
        return None;
    }
    let d = snap.dialect;
    let name = ident(d, src, sig[k - 1]);
    let qualifier = (k >= 3 && is_punct(src, sig[k - 2], ".") && is_name(sig[k - 3])).then(|| ident(d, src, sig[k - 3]));
    let stmt = statement_tokens(src, tokens, t.start, d);
    let a: Analysis = scope::analyze_in(src, &stmt, d);
    let scope_id = a.scope_at(t.start).unwrap_or(0);
    let ty = if a.scopes.is_empty() { None } else { a.ref_type(scope_id, qualifier.as_deref(), &name, snap) }?;
    // MySQL's labels are in the column's own type (`enum('a','b')`).
    let mysql;
    let labels: &[String] = match cat.enum_labels(&ty, &snap.search_path) {
        Some(l) => l,
        None if d == Dialect::MySql => {
            mysql = crate::dataview::mysql_enum_labels(&ty);
            if mysql.is_empty() {
                return None;
            }
            &mysql
        }
        None => return None,
    };
    let open = unterminated_in(src, &t, d).is_some();
    let replace = t.start + 1..cursor;
    let partial = &src[replace.clone()];
    let mut items: Vec<(u8, Candidate)> = labels
        .iter()
        .filter_map(|l| {
            let s = match_score(l, partial)?;
            let mut insert = l.replace('\'', "''");
            if open {
                insert.push('\'');
            }
            Some((s, Candidate { label: l.clone(), kind: CandidateKind::Value, detail: ty.clone(), insert, caret: None }))
        })
        .collect();
    items.sort_by_key(|(s, _)| *s);
    let items: Vec<Candidate> = items.into_iter().map(|(_, c)| c).take(MAX_ITEMS).collect();
    if items.len() == 1 && items[0].label == partial {
        return None;
    }
    Some(Completion { replace, items })
}

// ---------------------------------------------------------------------------------------------
// Signature help

/// The function call around the cursor and the parameter being typed.
#[derive(Clone, Debug, PartialEq)]
pub struct SignatureHelp {
    pub function: FuncSig,
    pub params: Vec<String>,
    /// The parameter under the cursor (`None`: past the last one).
    pub active: Option<usize>,
    /// How many overloads the name has.
    pub overloads: usize,
    /// Byte offset of the call's `(`.
    pub paren: usize,
}

/// Signature help for the cursor at byte `cursor`, from already lexed `tokens` of `src`.
pub fn signature_help(src: &str, tokens: &[Token], cursor: usize, snap: &SchemaSnapshot) -> Option<SignatureHelp> {
    let cat = snap.catalog.as_ref()?;
    let end = tokens.partition_point(|t| t.end <= cursor);
    if let Some(t) = tokens.get(end)
        && t.start < cursor
        && !matches!(t.kind, TokenKind::Whitespace)
        && !matches!(t.kind, TokenKind::Ident | TokenKind::Keyword | TokenKind::Number | TokenKind::QuotedIdent)
    {
        return None;
    }
    // Right after an open string or comment, or a line comment.
    if let Some(t) = end.checked_sub(1).map(|i| &tokens[i])
        && t.end == cursor
        && (t.kind == TokenKind::LineComment || unterminated_in(src, t, snap.dialect).is_some())
    {
        return None;
    }
    let mut depth = 0i32;
    let mut commas = 0usize;
    let mut seen = 0;
    let mut i = end;
    while i > 0 {
        i -= 1;
        let t = &tokens[i];
        if t.kind.is_trivia() {
            continue;
        }
        seen += 1;
        if seen > 2000 {
            return None;
        }
        match (t.kind, t.text(src)) {
            (TokenKind::Punct, ")") => depth += 1,
            (TokenKind::Punct, "(") if depth > 0 => depth -= 1,
            (TokenKind::Punct, "(") => {
                // The name before the paren.
                let mut j = i;
                let prev = loop {
                    if j == 0 {
                        return None;
                    }
                    j -= 1;
                    if !tokens[j].kind.is_trivia() {
                        break j;
                    }
                };
                let pt = &tokens[prev];
                if !is_name(pt) {
                    return None;
                }
                let name = ident(snap.dialect, src, pt);
                let mut back = tokens[..prev].iter().rev().filter(|t| !t.kind.is_trivia());
                let schema = match (back.next(), back.next()) {
                    (Some(d), Some(s)) if is_punct(src, d, ".") && is_name(s) => Some(ident(snap.dialect, src, s)),
                    _ => None,
                };
                let all: Vec<&FuncSig> = cat.functions_named(schema.as_deref(), &name).collect();
                if all.is_empty() {
                    return None;
                }
                let pick = all
                    .iter()
                    .copied()
                    .filter(|f| f.params().len() > commas || f.variadic())
                    .min_by_key(|f| f.params().len())
                    .or_else(|| all.iter().copied().max_by_key(|f| f.params().len()))?;
                let params = pick.params();
                let active = if commas < params.len() { Some(commas) } else if pick.variadic() { Some(params.len() - 1) } else { None };
                return Some(SignatureHelp { function: (*pick).clone(), params, active, overloads: all.len(), paren: t.start });
            }
            (TokenKind::Punct, ",") if depth == 0 => commas += 1,
            (TokenKind::Punct, ";") => return None,
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Go to definition

/// What a name in the editor refers to.
#[derive(Clone, Debug, PartialEq)]
pub enum Definition {
    /// A table or view (and the column, when the name was one).
    Relation { schema: String, name: String, column: Option<String> },
    Function { schema: String, name: String, oid: u32 },
    /// Defined in the text itself (a CTE): its name's byte range.
    Local(Range<usize>),
}

/// The definition of the name at byte `cursor` of `src`.
pub fn definition_at(src: &str, cursor: usize, snap: &SchemaSnapshot) -> Option<Definition> {
    let tokens = tokenize_in(src, snap.dialect);
    let i = tokens.iter().position(|t| t.start <= cursor && cursor <= t.end && is_name(t) || t.start <= cursor && cursor < t.end && t.kind == TokenKind::Keyword)?;
    let t = tokens[i];
    let next = tokens[i + 1..].iter().find(|t| !t.kind.is_trivia());
    let stmt = statement_tokens(src, &tokens, t.start, snap.dialect);
    let a = scope::analyze_in(src, &stmt, snap.dialect);
    let scope_id = a.scope_at(t.start).unwrap_or(0);
    let name = ident(snap.dialect, src, &t);
    // A call.
    if next.is_some_and(|n| is_punct(src, n, "(")) {
        if let Some((schema, _, _)) = a.calls.iter().find(|c| c.2 == t.range()) {
            let f = snap.catalog.as_ref()?.functions_named(schema.as_deref(), &name).next()?;
            return Some(Definition::Function { schema: f.schema.clone(), name: f.name.clone(), oid: f.oid });
        }
        return None;
    }
    if a.scopes.is_empty() {
        return None;
    }
    let relation = |schema: Option<&str>, n: &str, column: Option<String>| {
        let r = snap.resolve(schema, n)?;
        let r = &snap.relations[r];
        Some(Definition::Relation { schema: r.schema.clone(), name: r.name.clone(), column })
    };
    // A source's own name.
    for (si, s) in a.scopes.iter().enumerate() {
        for src_ in &s.sources {
            if src_.name_range.start <= t.start && t.end <= src_.name_range.end
                && let SourceKind::Table { schema, name: n } = &src_.kind
            {
                if schema.is_none()
                    && let Some(c) = a.visible_cte(si, n)
                {
                    return Some(Definition::Local(a.ctes[c].name_range.clone()));
                }
                return relation(schema.as_deref(), n, None);
            }
        }
    }
    // A column reference (or its qualifier).
    let to_def = |s: usize, idx: usize, column: Option<String>| -> Option<Definition> {
        match &a.scopes[s].sources[idx].kind {
            SourceKind::Table { schema: None, name: n } if a.visible_cte(s, n).is_some() => {
                Some(Definition::Local(a.ctes[a.visible_cte(s, n)?].name_range.clone()))
            }
            SourceKind::Table { schema, name: n } => relation(schema.as_deref(), n, column),
            _ => None,
        }
    };
    if let Some(r) = a.refs.iter().find(|r| r.range.start <= t.start && t.end <= r.range.end) {
        let on_name = r.name_range == t.range();
        match &r.qualifier {
            Some(q) => {
                if let Some((s, idx)) = a.find_source(r.scope, q) {
                    return to_def(s, idx, on_name.then(|| r.name.clone()));
                }
                return relation(None, q, on_name.then(|| r.name.clone()));
            }
            None => {
                for s in a.chain(r.scope) {
                    let hits: Vec<usize> = (0..a.scopes[s].sources.len())
                        .filter(|&idx| {
                            a.source_columns(s, &a.scopes[s].sources[idx], snap).is_some_and(|c| c.iter().any(|(n, _)| a.col_eq(n, &r.name)))
                        })
                        .collect();
                    if hits.len() == 1 {
                        return to_def(s, hits[0], Some(r.name.clone()));
                    }
                    if !hits.is_empty() {
                        return None;
                    }
                }
            }
        }
    }
    let _ = scope_id;
    relation(None, &name, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::catalog::{Catalog, TypeEntry};
    use crate::sql::tokenize;
    use crate::schema::{Column, RelKind, Relation};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn col(n: &str, t: &str) -> Column {
        Column { name: n.into(), type_name: t.into(), nullable: Some(true), primary_key: false, default: None }
    }

    fn func(schema: &str, name: &str, args: &str, result: &str) -> FuncSig {
        FuncSig { oid: 7, schema: schema.into(), name: name.into(), args: args.into(), result: result.into(), kind: 'f', returns_set: false }
    }

    fn snap() -> SchemaSnapshot {
        let rels = vec![
            Relation { schema: "public".into(), name: "orders".into(), kind: RelKind::Table, oid: None },
            Relation { schema: "public".into(), name: "customer".into(), kind: RelKind::Table, oid: None },
            Relation { schema: "public".into(), name: "orders_view".into(), kind: RelKind::View, oid: None },
            Relation { schema: "sales".into(), name: "goal".into(), kind: RelKind::Table, oid: None },
        ];
        let mut cols = HashMap::new();
        cols.insert(0, vec![col("id", "bigint"), col("customer", "bigint"), col("total", "numeric"), col("status", "status_type")]);
        cols.insert(1, vec![col("id", "bigint"), col("name", "text")]);
        cols.insert(3, vec![col("yr", "integer")]);
        let mut s = SchemaSnapshot::new(vec!["public".into()], vec!["public".into(), "sales".into()], rels, cols);
        s.catalog = Some(Arc::new(Catalog::new(
            vec![
                func("pg_catalog", "date_trunc", "text, timestamp with time zone", "timestamp with time zone"),
                func("pg_catalog", "date_trunc", "text, interval", "interval"),
                func("pg_catalog", "coalesce", "VARIADIC \"any\"", "anyelement"),
                func("pg_catalog", "now", "", "timestamp with time zone"),
                func("sales", "goal_total", "a integer", "numeric"),
            ],
            vec![TypeEntry { schema: "public".into(), name: "status_type".into(), kind: 'e', labels: vec!["open".into(), "paid".into(), "shipped".into()] }],
        )));
        s
    }

    fn comp(src: &str) -> Completion {
        let cursor = src.find('|').unwrap();
        let text = src.replace('|', "");
        complete(&text, cursor, &snap())
    }

    fn labels(src: &str) -> Vec<String> {
        comp(src).items.into_iter().map(|c| c.label).collect()
    }

    #[test]
    fn tables_after_from() {
        let l = labels("select * from ord|");
        assert_eq!(l[0], "orders");
        assert!(l.contains(&"orders_view".to_string()));
        assert!(!l.contains(&"customer".to_string()));
        let l = labels("select * from orders p, |");
        assert!(l.contains(&"customer".to_string()));
        assert!(l.contains(&"goal".to_string()));
        let l = labels("update |");
        assert!(l.contains(&"orders".to_string()));
        let l = labels("insert into cu|");
        assert_eq!(l[0], "customer");
    }

    #[test]
    fn columns_via_alias() {
        let l = labels("select p.| from orders p join customer c on c.id = p.customer");
        assert_eq!(l, vec!["id", "total", "status", "customer"]);
        let l = labels("select c.na| from orders p join customer c on c.id = p.customer");
        assert_eq!(l, vec!["name"]);
        let l = labels("select orders.sta| from orders");
        assert_eq!(l, vec!["status"]);
    }

    #[test]
    fn schema_qualified() {
        assert_eq!(labels("select * from sales.|"), vec!["goal", "goal_total"]);
        assert_eq!(labels("select m.| from sales.goal m"), vec!["yr"]);
        assert_eq!(labels("select sales.goal.| from sales.goal"), vec!["yr"]);
    }

    #[test]
    fn general_context_mixes_columns_and_keywords() {
        let l = labels("select tot| from orders");
        assert_eq!(l[0], "total");
        let l = labels("select * from orders wh|");
        assert_eq!(l[0], "WHERE");
        let c = complete("select * from orders wh", 23, &snap());
        assert_eq!(c.items[0].insert, "where");
        assert_eq!(c.replace, 21..23);
    }

    #[test]
    fn statement_scope() {
        // Columns come from the statement under the cursor only.
        let l = labels("select * from customer; select na| from orders;");
        assert!(!l.contains(&"name".to_string()));
    }

    #[test]
    fn nothing_in_strings_or_comments() {
        assert!(labels("select 'ord|' from orders").is_empty());
        assert!(labels("-- from ord|").is_empty());
        assert!(labels("select 1 /* ord| */").is_empty());
    }

    #[test]
    fn ctes_and_subqueries_complete() {
        // A CTE is a table after FROM, and its columns resolve through its alias.
        let l = labels("with big as (select id, total as t from orders) select * from b|");
        assert_eq!(l[0], "big");
        let l = labels("with big as (select id, total as t from orders) select b.| from big b");
        assert_eq!(l, vec!["t", "id"]);
        let l = labels("select x.| from (select p.id, c.name as who from orders p join customer c on c.id = p.customer) x");
        assert_eq!(l, vec!["id", "who"]);
        // Unqualified: the subquery's own sources come first, the outer query's after.
        let l = labels("select * from customer c where exists (select 1 from orders p where tot|)");
        assert_eq!(l[0], "total");
        let l = labels("select * from customer c where exists (select 1 from orders p where p.customer = |)");
        let pos = |n: &str| l.iter().position(|x| x == n).unwrap();
        assert!(pos("total") < pos("name"), "{l:?}");
    }

    #[test]
    fn using_insert_and_set() {
        let l = labels("select * from orders join customer using (|");
        assert_eq!(l, vec!["id"]);
        let l = labels("insert into customer (|");
        assert_eq!(l, vec!["id", "name"]);
        let l = labels("update orders set |");
        assert_eq!(l, vec!["id", "total", "status", "customer"]);
        let l = labels("update orders set total = 1, sta|");
        assert_eq!(l, vec!["status"]);
    }

    #[test]
    fn functions_types_and_labels() {
        let c = comp("select date_t| from orders");
        assert_eq!(c.items[0].label, "date_trunc");
        assert_eq!(c.items[0].kind, CandidateKind::Function);
        assert_eq!(c.items[0].insert, "date_trunc()");
        assert_eq!(c.items[0].caret, Some("date_trunc(".len()));
        let c = comp("select no| ");
        let now = c.items.iter().find(|c| c.label == "now").unwrap();
        assert_eq!(now.caret, None, "no arguments: the caret goes after the parens");
        let l = labels("select total::num|");
        assert_eq!(l[0], "numeric");
        let l = labels("select cast(total as status|");
        assert_eq!(l[0], "status_type");
        let c = comp("select * from orders where status = 'pa|");
        assert_eq!(c.items.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), vec!["paid"]);
        assert_eq!(c.items[0].insert, "paid'");
        let c = comp("select * from orders p where p.status in ('open', '|')");
        assert_eq!(c.items.len(), 3);
        assert_eq!(c.items[0].insert, "open");
        assert!(labels("select * from customer where name = '|'").is_empty());
    }

    #[test]
    fn keywords_by_context() {
        assert_eq!(labels("sel|")[0], "SELECT");
        assert_eq!(labels("select * from orders order |"), vec!["BY"]);
        let l = labels("select * from orders where id = 1 |");
        assert_eq!(l[0], "AND");
        let l = labels("select id fr|");
        assert_eq!(l[0], "FROM");
        let l = labels("select case when id = 1 th|");
        assert_eq!(l[0], "THEN");
        assert_eq!(labels("delete |"), vec!["FROM"]);
    }

    #[test]
    fn signature_help_tracks_the_argument() {
        let s = snap();
        let at = |src: &str| {
            let cursor = src.find('|').unwrap();
            let text = src.replace('|', "");
            signature_help(&text, &tokenize(&text), cursor, &s)
        };
        let h = at("select date_trunc('day', |) from orders").unwrap();
        assert_eq!(h.function.name, "date_trunc");
        assert_eq!(h.active, Some(1));
        assert_eq!(h.overloads, 2);
        assert_eq!(h.params.len(), 2);
        let h = at("select date_trunc(|").unwrap();
        assert_eq!(h.active, Some(0));
        let h = at("select coalesce(a, b, c, |").unwrap();
        assert_eq!(h.active, Some(0), "variadic stays on its last parameter");
        assert!(at("select date_trunc('day', now()) |").is_none());
        let h = at("select date_trunc(lower('x', 'y'), |").unwrap();
        assert_eq!(h.active, Some(1), "commas inside a nested call do not count");
        let h = at("select date_trunc('x', (1 + 2), |").unwrap();
        assert_eq!(h.active, None, "past the last parameter");
        assert!(at("select nope(|").is_none());
        assert!(at("select date_trunc('a|").is_none(), "inside a string");
    }

    #[test]
    fn definitions() {
        let s = snap();
        let at = |src: &str| {
            let cursor = src.find('|').unwrap();
            let text = src.replace('|', "");
            definition_at(&text, cursor, &s)
        };
        assert_eq!(at("select * from ord|ers p"), Some(Definition::Relation { schema: "public".into(), name: "orders".into(), column: None }));
        assert_eq!(
            at("select p.tot|al from orders p"),
            Some(Definition::Relation { schema: "public".into(), name: "orders".into(), column: Some("total".into()) })
        );
        assert_eq!(at("select |p.total from orders p"), Some(Definition::Relation { schema: "public".into(), name: "orders".into(), column: None }));
        assert_eq!(at("select na|me from customer"), Some(Definition::Relation { schema: "public".into(), name: "customer".into(), column: Some("name".into()) }));
        let src = "with big as (select 1) select * from bi|g";
        let text = src.replace('|', "");
        assert_eq!(at(src), Some(Definition::Local(5..8)));
        assert_eq!(&text[5..8], "big");
        assert_eq!(at("select now|()"), Some(Definition::Function { schema: "pg_catalog".into(), name: "now".into(), oid: 7 }));
        assert_eq!(at("select * from nope|"), None);
    }

    #[test]
    fn fast_on_a_big_schema() {
        let mut rels = Vec::new();
        let mut cols = HashMap::new();
        for i in 0..1000 {
            rels.push(Relation { schema: "public".into(), name: format!("table_{i:04}"), kind: RelKind::Table, oid: None });
            cols.insert(i, (0..20).map(|c| col(&format!("column_{c}"), "integer")).collect());
        }
        let mut s = SchemaSnapshot::new(vec!["public".into()], vec!["public".into()], rels, cols);
        let funcs = (0..2500).map(|i| func("pg_catalog", &format!("fn_{i}"), "a integer", "integer")).collect();
        s.catalog = Some(Arc::new(Catalog::new(funcs, vec![])));
        let script = "select t.column_1, x.column_2 from table_0001 t join table_0999 x on t.column_0 = x.column_0 where t.co";
        let t0 = std::time::Instant::now();
        let n = 200;
        for _ in 0..n {
            let c = complete(script, script.len(), &s);
            assert!(!c.items.is_empty());
            let c = complete("select * from tab", 17, &s);
            assert_eq!(c.items.len(), MAX_ITEMS);
            let c = complete("select fn_1", 11, &s);
            assert!(!c.items.is_empty());
        }
        let per = t0.elapsed() / (3 * n);
        eprintln!("complete() on 1000 tables x 20 columns, 2500 functions: {per:?} per call");
        assert!(per < std::time::Duration::from_millis(5), "{per:?}");
    }
}
