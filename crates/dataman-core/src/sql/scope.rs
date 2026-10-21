use std::ops::Range;

use super::lexer::{Token, TokenKind};
use super::{Dialect, ident_value, ident_value_in, quote_ident};
use crate::schema::SchemaSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clause {
    Select,
    From,
    On,
    Where,
    GroupBy,
    Having,
    OrderBy,
    Set,
    Returning,
    Values,
    Window,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceKind {
    Table { schema: Option<String>, name: String },
    Subquery(usize),
    Function(String),
    Values,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub alias: Option<String>,
    pub kind: SourceKind,
    pub name_range: Range<usize>,
    pub alias_columns: Option<Vec<String>>,
}

impl Source {
    pub fn visible_name(&self) -> Option<&str> {
        match (&self.alias, &self.kind) {
            (Some(a), _) => Some(a),
            (None, SourceKind::Table { name, .. }) => Some(name),
            (None, SourceKind::Function(f)) => Some(f),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Star(Option<String>),
    Named(String),
    Expr,
}

/// Where a scope's rows go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    /// The statement's own query.
    #[default]
    Root,
    /// A WITH query's body.
    Cte,
    /// A set operation's later branch; its columns take the names of the branch before it.
    Branch(usize),
    /// A derived table in FROM.
    Derived,
    /// The rows of INSERT … SELECT.
    Insert,
    /// A subquery inside an expression of this clause of the parent.
    Expr(Clause),
}

#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub range: Range<usize>,
    pub parent: Option<usize>,
    pub cte_parent: Option<usize>,
    pub role: Role,
    pub sources: Vec<Source>,
    pub items: Vec<Item>,
    pub item_refs: Vec<Option<(Option<String>, String)>>,
    pub ctes: Vec<usize>,
    pub using: Vec<String>,
    pub natural: bool,
    pub open: bool,
}

#[derive(Clone, Debug)]
pub struct Cte {
    pub name: String,
    pub columns: Option<Vec<String>>,
    pub scope: Option<usize>,
    pub name_range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColRef {
    pub scope: usize,
    pub qualifier: Option<String>,
    pub name: String,
    pub range: Range<usize>,
    pub name_range: Range<usize>,
    pub clause: Clause,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TargetCol {
    pub scope: usize,
    pub source: usize,
    pub name: String,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub scopes: Vec<Scope>,
    pub ctes: Vec<Cte>,
    pub refs: Vec<ColRef>,
    pub targets: Vec<TargetCol>,
    pub missing_on: Vec<Range<usize>>,
    pub calls: Vec<(Option<String>, String, Range<usize>)>,
    pub ci: bool,
    pub unsure: bool,
    pub distrust: std::collections::HashSet<String>,
}

impl Analysis {
    pub fn col_eq(&self, a: &str, b: &str) -> bool {
        if self.ci { a.eq_ignore_ascii_case(b) } else { a == b }
    }
}

const NOT_COLUMNS: &[&str] = &[
    "precision", "varying", "zone", "at", "local", "localtime", "localtimestamp", "current_date", "current_time",
    "current_timestamp", "current_user", "current_role", "current_schema", "current_catalog", "session_user", "user",
    "year", "month", "day", "hour", "minute", "second", "epoch", "dow", "doy", "isodow", "isoyear", "week", "quarter",
    "decade", "century", "millennium", "microseconds", "milliseconds", "timezone", "timezone_hour", "timezone_minute",
    "julian", "placing", "leading", "trailing", "both", "escape", "nulls", "first", "last", "ordinality", "true",
    "false", "null", "unknown", "default", "value", "nfc", "nfd", "nfkc", "nfkd", "normalized", "symmetric",
    "asymmetric", "isnull", "notnull", "sets", "ties", "only", "rows", "range", "groups", "preceding", "following",
    "unbounded", "current", "exclude", "others", "no", "row", "array", "interval", "distinct", "all", "any", "some",
    "excluded", "new", "old", "collate", "uescape", "xmlnamespaces", "passing", "columns", "path", "ref", "by",
    "grouping", "cube", "rollup", "character", "utc_timestamp", "utc_date", "utc_time",
];

const DATE_PART_FUNCS: &[&str] = &["dateadd", "datediff", "datediff_big", "datepart", "datename", "datetrunc", "date_bucket", "timestampadd", "timestampdiff"];

const SPECIAL_ARGS: &[&str] = &[
    "extract", "overlay", "position", "substring", "trim", "normalize", "xmlelement", "xmlforest", "xmlparse",
    "xmlserialize", "xmltable", "json_table", "json_object", "json_array", "json_value", "json_query", "json_exists",
    "collation",
];

pub const UNMODELED: &[&str] = &[
    "APPLY", "PIVOT", "UNPIVOT", "MERGE", "OUTPUT", "TRY", "CATCH", "IF", "ELSE", "WHILE", "BREAK", "CONTINUE", "GOTO",
    "RETURN", "THROW", "RAISERROR", "PRINT", "WAITFOR", "DECLARE", "EXEC", "EXECUTE", "CURSOR", "OPENJSON", "OPENROWSET",
    "OPENQUERY", "OPENXML", "OPENDATASOURCE", "CONTAINSTABLE", "FREETEXTTABLE", "CHANGETABLE", "PREDICT", "WITHIN",
    "MATCH", "AGAINST",
];

fn unmodeled(src: &str, toks: &[Token], close: &[usize], dialect: Dialect) -> bool {
    let word = |i: usize, w: &str| toks.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(src).eq_ignore_ascii_case(w));
    let mut verbs: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) {
            let w = t.text(src);
            let function = dialect == Dialect::MySql && w.eq_ignore_ascii_case("IF") && toks.get(i + 1).is_some_and(|n| n.text(src) == "(");
            if !function && UNMODELED.iter().any(|u| w.eq_ignore_ascii_case(u)) {
                return true;
            }
            if w.eq_ignore_ascii_case("NEXT") && word(i + 1, "VALUE") && word(i + 2, "FOR") {
                return true;
            }
            if w.eq_ignore_ascii_case("UPDATE") && word(i + 1, "STATISTICS") {
                return true;
            }
            if (w.eq_ignore_ascii_case("OVERRIDING") && (word(i + 1, "SYSTEM") || word(i + 1, "USER")) && word(i + 2, "VALUE"))
                || (w.eq_ignore_ascii_case("SEARCH") && (word(i + 1, "DEPTH") || word(i + 1, "BREADTH")) && word(i + 2, "FIRST"))
                || (w.eq_ignore_ascii_case("CYCLE") && i > 0 && toks[i - 1].text(src) == ")")
            {
                return true;
            }
        }
        i += 1;
    }
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t.kind == TokenKind::Punct && matches!(t.text(src), "(" | "[") {
            i = close.get(i).copied().unwrap_or(toks.len()).max(i) + 1;
            continue;
        }
        let prev = |ws: &[&str]| i > 0 && ws.iter().any(|w| word(i - 1, w));
        if word(i, "SELECT") && !prev(&["UNION", "ALL", "DISTINCT", "EXCEPT", "INTERSECT"]) {
            verbs.push("SELECT");
        } else if word(i, "INSERT") {
            verbs.push("INSERT");
        } else if word(i, "UPDATE") && !prev(&["DO", "FOR", "KEY", "ON"]) {
            verbs.push("UPDATE");
        } else if word(i, "DELETE") && !prev(&["ON"]) {
            verbs.push("DELETE");
        }
        i += 1;
    }
    !(verbs.len() <= 1 || verbs == ["INSERT", "SELECT"])
}

pub const SYSTEM_COLUMNS: &[&str] = &["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid", "oid"];

const CLAUSE_WORDS: &[&str] = &[
    "FROM", "WHERE", "GROUP", "HAVING", "WINDOW", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR", "UNION", "INTERSECT",
    "EXCEPT", "RETURNING", "INTO", "ON", "USING", "JOIN", "LEFT", "RIGHT", "FULL", "INNER", "CROSS", "NATURAL", "SET",
    "VALUES", "SELECT", "OUTER", "LATERAL", "TABLESAMPLE", "DO", "WITH",
];

pub fn analyze(src: &str, toks: &[Token]) -> Analysis {
    analyze_in(src, toks, Dialect::Postgres)
}

pub fn analyze_in(src: &str, toks: &[Token], dialect: Dialect) -> Analysis {
    let toks: Vec<Token> = toks.iter().copied().filter(|t| !t.kind.is_trivia()).collect();
    let mut close = vec![usize::MAX; toks.len()];
    let mut stack = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t.kind == TokenKind::Punct {
            match t.text(src) {
                "(" | "[" => stack.push(i),
                ")" | "]" => {
                    if let Some(o) = stack.pop() {
                        close[o] = i;
                    }
                }
                _ => {}
            }
        }
    }
    let n = toks.len();
    for c in close.iter_mut() {
        if *c == usize::MAX {
            *c = n;
        }
    }
    let unsure = unmodeled(src, &toks, &close, dialect);
    let mut p = P { src, t: &toks, close, a: Analysis { ci: dialect != Dialect::Postgres, ..Default::default() }, dialect, qualified: Vec::new() };
    if !toks.is_empty() {
        p.query(0, n, None, None);
    }
    p.a.unsure = unsure || p.a.scopes.iter().any(|s| s.open);
    p.a
}

struct P<'a> {
    src: &'a str,
    t: &'a [Token],
    close: Vec<usize>,
    a: Analysis,
    dialect: Dialect,
    qualified: Vec<(TargetCol, String)>,
}

impl<'a> P<'a> {
    fn text(&self, i: usize) -> &'a str {
        self.t.get(i).map_or("", |t| t.text(self.src))
    }

    fn word(&self, i: usize, w: &str) -> bool {
        self.t.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(self.src).eq_ignore_ascii_case(w))
    }

    fn any_word(&self, i: usize, ws: &[&str]) -> bool {
        ws.iter().any(|w| self.word(i, w))
    }

    fn punct(&self, i: usize, p: &str) -> bool {
        self.t.get(i).is_some_and(|t| t.kind == TokenKind::Punct && t.text(self.src) == p)
    }

    fn is_name(&self, i: usize) -> bool {
        self.t.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent) && !t.text(self.src).starts_with('@'))
    }

    fn value(&self, i: usize) -> String {
        match self.dialect {
            Dialect::Postgres => ident_value(self.src, &self.t[i]),
            d => ident_value_in(self.src, &self.t[i], d).to_lowercase(),
        }
    }

    fn end_byte(&self, hi: usize) -> usize {
        self.t.get(hi).map_or(self.src.len(), |t| t.start)
    }

    fn starts_query(&self, i: usize) -> bool {
        self.any_word(i, &["SELECT", "WITH", "VALUES", "TABLE"]) || (self.punct(i, "(") && self.starts_query(i + 1))
    }

    fn new_scope(&mut self, lo: usize, hi: usize, parent: Option<usize>, cte_parent: Option<usize>) -> usize {
        let start = self.t.get(lo).map_or(self.src.len(), |t| t.start);
        self.a.scopes.push(Scope { range: start..self.end_byte(hi), parent, cte_parent, ..Default::default() });
        self.a.scopes.len() - 1
    }

    fn query(&mut self, lo: usize, hi: usize, parent: Option<usize>, cte_parent: Option<usize>) -> usize {
        let id = self.new_scope(lo, hi, parent, cte_parent);
        let mut i = lo;
        if self.word(i, "WITH") {
            i = self.with(i + 1, hi, id);
        }
        self.body(i, hi, id);
        id
    }

    fn with(&mut self, mut i: usize, hi: usize, id: usize) -> usize {
        if self.word(i, "RECURSIVE") {
            i += 1;
        }
        while i < hi && (self.is_name(i) || self.t[i].kind == TokenKind::Keyword) && !self.word(i, "SELECT") {
            let name = self.value(i);
            let name_range = self.t[i].range();
            i += 1;
            let mut columns = None;
            if self.punct(i, "(") {
                let j = self.close[i];
                columns = Some(self.name_list(i + 1, j.min(hi)));
                i = j + 1;
            }
            if self.word(i, "AS") {
                i += 1;
            }
            if self.word(i, "NOT") {
                i += 1;
            }
            if self.word(i, "MATERIALIZED") {
                i += 1;
            }
            let cte = self.a.ctes.len();
            self.a.ctes.push(Cte { name, columns, scope: None, name_range });
            self.a.scopes[id].ctes.push(cte);
            if self.punct(i, "(") {
                let j = self.close[i].min(hi);
                let parent = self.a.scopes[id].parent;
                let body = self.query(i + 1, j, parent, Some(id));
                self.a.scopes[body].role = Role::Cte;
                self.a.ctes[cte].scope = Some(body);
                i = j + 1;
            } else {
                self.a.scopes[id].open = true;
                break;
            }
            if self.punct(i, ",") {
                i += 1;
            } else {
                break;
            }
        }
        i
    }

    fn name_list(&self, lo: usize, hi: usize) -> Vec<String> {
        (lo..hi).filter(|&k| self.is_name(k) || (self.t[k].kind == TokenKind::Keyword && !self.punct(k, ","))).map(|k| self.value(k)).collect()
    }

    fn body(&mut self, mut i: usize, hi: usize, id: usize) {
        let mut clause = Clause::Other;
        let mut dml: Option<usize> = None;
        while i < hi {
            let t = self.t[i];
            if t.kind == TokenKind::Punct && t.text(self.src) == "(" {
                i = self.paren(i, hi, id, clause, false);
                continue;
            }
            if matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) {
                let w = t.text(self.src).to_ascii_uppercase();
                match w.as_str() {
                    "SELECT" => {
                        i = self.select_list(i + 1, hi, id, Clause::Select);
                        clause = Clause::Select;
                        continue;
                    }
                    "FROM" if !self.word(i.wrapping_sub(1), "DISTINCT") => {
                        i = self.from_list(i + 1, hi, id);
                        clause = Clause::From;
                        continue;
                    }
                    "USING" if clause == Clause::Other || clause == Clause::From => {
                        i = self.from_list(i + 1, hi, id);
                        clause = Clause::From;
                        continue;
                    }
                    "WHERE" => clause = Clause::Where,
                    "GROUP" => clause = Clause::GroupBy,
                    "HAVING" => clause = Clause::Having,
                    "ORDER" => clause = Clause::OrderBy,
                    "WINDOW" => clause = Clause::Window,
                    "FOR" if self.dialect == Dialect::TSql && self.any_word(i + 1, &["XML", "JSON", "BROWSE"]) => {
                        i = hi;
                        continue;
                    }
                    "FOR" if self.any_word(i + 1, &["UPDATE", "SHARE", "NO", "KEY"]) => {
                        clause = Clause::Other;
                        i = hi;
                        continue;
                    }
                    "LIMIT" | "OFFSET" | "FETCH" | "FOR" => clause = Clause::Other,
                    "NEXT" | "FIRST" if self.word(i.wrapping_sub(1), "FETCH") => {
                        i += 1;
                        continue;
                    }
                    "ROW" | "ROWS" | "ONLY" | "TIES" if clause == Clause::Other => {
                        i += 1;
                        continue;
                    }
                    "RETURNING" => {
                        i = self.select_list(i + 1, hi, id, Clause::Returning);
                        clause = Clause::Returning;
                        continue;
                    }
                    "VALUES" => clause = Clause::Values,
                    "UPDATE" => {
                        clause = Clause::Other;
                        if !self.any_word(i.wrapping_sub(1), &["DO", "FOR", "KEY"]) {
                            dml.get_or_insert(self.a.scopes[id].sources.len());
                            i = self.from_list(self.top(i + 1, hi), hi, id);
                            continue;
                        }
                    }
                    "SET" => {
                        i = self.set_list(i + 1, hi, id);
                        clause = Clause::Set;
                        continue;
                    }
                    "INTO" | "INSERT"
                        if (w == "INTO" && self.any_word(i.wrapping_sub(1), &["INSERT", "REPLACE", "IGNORE", "LOW_PRIORITY", "HIGH_PRIORITY", "DELAYED"])) || (w == "INSERT" && !self.word(i + 1, "INTO") && self.is_name(i + 1)) =>
                    {
                        i = self.insert_target(i + 1, hi, id);
                        if i < hi && self.starts_query(i) {
                            let end = (i..hi).find(|&k| self.word(k, "ON") && self.word(k + 1, "CONFLICT") || self.word(k, "RETURNING")).unwrap_or(hi);
                            let end = self.depth0(i, end, hi);
                            let rows = self.query(i, end, Some(id), Some(id));
                            self.a.scopes[rows].role = Role::Insert;
                            i = end;
                        }
                        clause = Clause::Values;
                        continue;
                    }
                    "INTO" if clause == Clause::Select => {
                        clause = Clause::Other;
                        i += 1;
                        while i < hi && (self.is_name(i) || self.punct(i, ".") || self.any_word(i, &["TEMP", "TEMPORARY", "UNLOGGED", "TABLE"])) {
                            i += 1;
                        }
                        continue;
                    }
                    "CONFLICT" => {
                        clause = Clause::Other;
                        if let Some(tgt) = self.a.scopes[id].sources.first().cloned() {
                            let mut ex = tgt;
                            ex.alias = Some("excluded".into());
                            self.a.scopes[id].sources.push(ex);
                        }
                    }
                    "DELETE" if self.word(self.top(i + 1, hi), "FROM") => {
                        dml.get_or_insert(self.a.scopes[id].sources.len());
                        i = self.source(self.top(i + 1, hi) + 1, hi, id);
                        clause = Clause::Other;
                        continue;
                    }
                    "UNION" | "INTERSECT" | "EXCEPT" => {
                        let mut k = i + 1;
                        if self.any_word(k, &["ALL", "DISTINCT"]) {
                            k += 1;
                        }
                        let parent = self.a.scopes[id].parent;
                        let next = self.query(k, hi, parent, Some(id));
                        self.a.scopes[next].role = Role::Branch(id);
                        self.settle_targets(id, dml);
                        return;
                    }
                    "AS" if clause == Clause::Window => {
                        i += 1;
                        continue;
                    }
                    _ => {}
                }
                if t.kind == TokenKind::Keyword {
                    i += 1;
                    continue;
                }
            }
            if self.is_name(i) {
                i = self.name_ref(i, hi, id, clause, false);
                continue;
            }
            i += 1;
        }
        self.settle_targets(id, dml);
    }

    fn top(&self, mut i: usize, hi: usize) -> usize {
        if self.dialect == Dialect::TSql && self.word(i, "TOP") {
            i = if self.punct(i + 1, "(") { self.close[i + 1].min(hi) + 1 } else { i + 2 };
            if self.word(i, "PERCENT") {
                i += 1;
            }
        }
        i
    }

    fn settle_targets(&mut self, id: usize, dml: Option<usize>) {
        if self.dialect != Dialect::Postgres
            && let Some(d) = dml
            && let Some(Source { alias: None, kind: SourceKind::Table { schema, name }, .. }) = self.a.scopes[id].sources.get(d)
        {
            let same = |x: &Source| match (&x.alias, &x.kind) {
                (Some(a), _) => schema.is_none() && a == name,
                (None, SourceKind::Table { schema: s, name: n }) => n == name && (s == schema || s.is_none() || schema.is_none()),
                _ => false,
            };
            let srcs = &self.a.scopes[id].sources;
            if let Some(j) = (0..srcs.len()).find(|&j| j != d && same(&srcs[j])) {
                self.a.scopes[id].sources.remove(d);
                let j = if j > d { j - 1 } else { j };
                for t in self.a.targets.iter_mut().filter(|t| t.scope == id) {
                    if t.source == d {
                        t.source = j;
                    } else if t.source > d {
                        t.source -= 1;
                    }
                }
            }
        }
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.qualified).into_iter().partition(|(t, _)| t.scope == id);
        self.qualified = rest;
        for (mut t, q) in mine {
            let srcs = &self.a.scopes[id].sources;
            let found = srcs.iter().position(|x| x.alias.as_deref() == Some(&q)).or_else(|| srcs.iter().position(|x| x.alias.is_none() && x.visible_name() == Some(&q)));
            if let Some(s) = found {
                t.source = s;
                self.a.targets.push(t);
            }
        }
    }

    fn depth0(&self, lo: usize, end: usize, hi: usize) -> usize {
        let mut k = lo;
        while k < end.min(hi) {
            if self.punct(k, "(") {
                let c = self.close[k];
                if c >= end {
                    return end;
                }
                k = c + 1;
                continue;
            }
            k += 1;
        }
        end
    }

    fn paren(&mut self, i: usize, hi: usize, id: usize, clause: Clause, special: bool) -> usize {
        let j = self.close[i].min(hi);
        if self.starts_query(i + 1) {
            let sub = self.query(i + 1, j, Some(id), Some(id));
            self.a.scopes[sub].role = Role::Expr(clause);
        } else {
            self.expr(i + 1, j, id, clause, special);
        }
        j + 1
    }

    fn expr(&mut self, lo: usize, hi: usize, id: usize, clause: Clause, special: bool) {
        let mut i = lo;
        while i < hi {
            if self.punct(i, "(") {
                i = self.paren(i, hi, id, clause, special);
                continue;
            }
            if self.is_name(i) {
                i = self.name_ref(i, hi, id, clause, special);
                continue;
            }
            i += 1;
        }
    }

    fn name_ref(&mut self, i: usize, hi: usize, id: usize, clause: Clause, special: bool) -> usize {
        let mut parts = vec![i];
        let mut k = i + 1;
        while k + 1 < hi + 1 && self.punct(k, ".") && (self.is_name(k + 1) || self.t.get(k + 1).is_some_and(|t| t.kind == TokenKind::Keyword)) {
            parts.push(k + 1);
            k += 2;
        }
        if self.punct(k, ".") && self.text(k + 1) == "*" {
            return k + 2;
        }
        let last = *parts.last().unwrap();
        if k < hi && self.punct(k, "(") {
            let name = self.value(last);
            let schema = (parts.len() > 1).then(|| self.value(parts[parts.len() - 2]));
            self.a.calls.push((schema, name.clone(), self.t[last].range()));
            let sp = SPECIAL_ARGS.contains(&name.as_str());
            let keyword_first = DATE_PART_FUNCS.contains(&name.as_str()) || (self.dialect == Dialect::TSql && matches!(name.as_str(), "convert" | "try_convert" | "try_parse" | "parse"));
            if keyword_first && parts.len() == 1 && matches!(self.t.get(k + 1).map(|t| t.kind), Some(TokenKind::Ident | TokenKind::Keyword)) && self.punct(k + 2, ",") {
                let j = self.close[k].min(hi);
                self.expr(k + 3, j, id, clause, sp);
                return j + 1;
            }
            return self.paren(k, hi, id, clause, sp);
        }
        let prev = i.checked_sub(1).map(|p| self.t[p]);
        let skip = special && parts.len() == 1
            || prev.is_some_and(|p| {
                let s = p.text(self.src);
                s == "::"
                    || s == "."
                    || (p.kind == TokenKind::Punct && s == ":")
                    || (p.kind == TokenKind::Keyword && ["AS", "OVER", "CONSTRAINT", "COLLATE", "OF", "WINDOW", "CHARSET"].iter().any(|w| s.eq_ignore_ascii_case(w)))
            })
            || self.t.get(k).is_some_and(|t| t.kind == TokenKind::String || t.text(self.src) == "=>")
            || (parts.len() == 1 && i >= 2 && prev.is_some_and(|p| p.text(self.src).eq_ignore_ascii_case("SET")) && self.t[i - 2].text(self.src).eq_ignore_ascii_case("CHARACTER"))
            || (parts.len() == 1 && prev.is_some_and(|p| p.text(self.src).eq_ignore_ascii_case("USING")) && !self.punct(k, "("))
            || (self.dialect == Dialect::TSql && parts.len() == 1 && self.punct(k, ":") && self.punct(k + 1, ":"))
            || (parts.len() == 1 && NOT_COLUMNS.contains(&self.value(i).to_ascii_lowercase().as_str()));
        if !skip {
            let name = self.value(last);
            let qualifier = (parts.len() > 1).then(|| self.value(parts[parts.len() - 2]));
            self.a.refs.push(ColRef {
                scope: id,
                qualifier,
                name,
                range: self.t[i].start..self.t[last].end,
                name_range: self.t[last].range(),
                clause,
            });
        }
        k
    }

    fn select_list(&mut self, mut i: usize, hi: usize, id: usize, clause: Clause) -> usize {
        if self.word(i, "ALL") {
            i += 1;
        } else if self.word(i, "DISTINCT") {
            i += 1;
            if self.word(i, "ON") && self.punct(i + 1, "(") {
                let j = self.close[i + 1].min(hi);
                self.expr(i + 2, j, id, clause, false);
                i = j + 1;
            }
        }
        while self.any_word(i, &["DISTINCTROW", "HIGH_PRIORITY", "STRAIGHT_JOIN", "SQL_SMALL_RESULT", "SQL_BIG_RESULT", "SQL_BUFFER_RESULT", "SQL_NO_CACHE", "SQL_CACHE", "SQL_CALC_FOUND_ROWS"]) {
            i += 1;
        }
        let mut start = i;
        loop {
            let mut k = start;
            while k < hi && !self.punct(k, ",") && !self.punct(k, ";") && !(self.is_clause_end(k) && !self.in_case(start, k)) {
                if self.punct(k, "(") {
                    k = self.close[k] + 1;
                    continue;
                }
                k += 1;
            }
            let k = k.min(hi);
            if k > start {
                self.item(start, k, id, clause);
            }
            if k < hi && self.punct(k, ",") {
                start = k + 1;
                continue;
            }
            return k;
        }
    }

    fn in_case(&self, lo: usize, k: usize) -> bool {
        let mut depth = 0i32;
        let mut j = lo;
        while j < k {
            if self.punct(j, "(") {
                j = self.close[j] + 1;
                continue;
            }
            if self.word(j, "CASE") {
                depth += 1;
            } else if self.word(j, "END") {
                depth -= 1;
            }
            j += 1;
        }
        depth > 0
    }

    fn is_clause_end(&self, k: usize) -> bool {
        self.t.get(k).is_some_and(|t| t.kind == TokenKind::Keyword)
            && !(self.word(k, "FROM") && self.word(k.wrapping_sub(1), "DISTINCT"))
            && ["FROM", "INTO", "WHERE", "GROUP", "HAVING", "WINDOW", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR", "UNION", "INTERSECT", "EXCEPT", "RETURNING", "ON"]
                .iter()
                .any(|w| self.word(k, w))
    }

    fn item(&mut self, lo: usize, hi: usize, id: usize, clause: Clause) {
        let n = hi - lo;
        if self.dialect == Dialect::TSql && n >= 3 && self.is_name(lo) && self.text(lo + 1) == "=" {
            let alias = self.value(lo);
            self.a.scopes[id].items.push(Item::Named(alias));
            self.a.scopes[id].item_refs.push(None);
            self.expr(lo + 2, hi, id, clause, false);
            return;
        }
        let item = if n == 1 && self.text(lo) == "*" {
            Item::Star(None)
        } else if n == 3 && self.is_name(lo) && self.punct(lo + 1, ".") && self.text(lo + 2) == "*" {
            Item::Star(Some(self.value(lo)))
        } else if n >= 3 && self.text(hi - 1) == "*" && self.punct(hi - 2, ".") {
            Item::Star(Some(String::new()))
        } else if n >= 2 && self.word(hi - 2, "AS") && (self.is_name(hi - 1) || self.t[hi - 1].kind == TokenKind::Keyword) {
            Item::Named(self.value(hi - 1))
        } else if n >= 2
            && self.is_name(hi - 1)
            && !self.punct(hi - 2, ".")
            && !matches!(self.t[hi - 2].kind, TokenKind::Operator)
            && !self.word(hi - 2, "AS")
        {
            Item::Named(self.value(hi - 1))
        } else if self.is_name(hi - 1) && (n == 1 || self.punct(hi - 2, ".")) {
            Item::Named(self.value(hi - 1))
        } else if self.is_name(lo) && self.punct(lo + 1, "(") && self.close[lo + 1] == hi - 1 {
            Item::Named(self.value(lo))
        } else if self.word(lo, "CASE") {
            Item::Named("case".into())
        } else if self.is_name(lo) && self.text(lo + 1) == "::" {
            Item::Named(self.value(lo))
        } else {
            Item::Expr
        };
        let alias_at = match &item {
            Item::Named(_) if n >= 2 && (self.word(hi - 2, "AS") || (self.is_name(hi - 1) && !self.punct(hi - 2, ".") && !matches!(self.t[hi - 2].kind, TokenKind::Operator))) => {
                Some(if self.word(hi - 2, "AS") { hi - 2 } else { hi - 1 })
            }
            _ => None,
        };
        let e = alias_at.unwrap_or(hi);
        let plain = match e - lo {
            1 if self.is_name(lo) => Some((None, self.value(lo))),
            3 if self.is_name(lo) && self.punct(lo + 1, ".") && self.is_name(lo + 2) => Some((Some(self.value(lo)), self.value(lo + 2))),
            _ => None,
        };
        self.a.scopes[id].items.push(item);
        self.a.scopes[id].item_refs.push(plain);
        self.expr(lo, e, id, clause, false);
    }

    fn set_list(&mut self, mut i: usize, hi: usize, id: usize) -> usize {
        loop {
            if self.punct(i, "(") {
                let j = self.close[i].min(hi);
                for k in i + 1..j {
                    if self.is_name(k) {
                        self.target(k, id);
                    }
                }
                i = j + 1;
            } else if self.dialect != Dialect::Postgres && self.is_name(i) && self.punct(i + 1, ".") && self.is_name(i + 2) {
                // `alias.col` (PostgreSQL reads `col.field` of a composite column instead).
                let t = TargetCol { scope: id, source: 0, name: self.value(i + 2), range: self.t[i + 2].range() };
                self.qualified.push((t, self.value(i)));
                i += 3;
            } else if self.is_name(i) {
                self.target(i, id);
                i += 1;
            }
            while i < hi && self.text(i) != "=" && !self.punct(i, ",") && !self.is_clause_end(i) {
                i += 1;
            }
            if self.text(i) == "=" {
                i += 1;
            }
            let start = i;
            while i < hi && !self.punct(i, ",") && !self.is_clause_end(i) {
                if self.punct(i, "(") {
                    i = self.close[i] + 1;
                    continue;
                }
                i += 1;
            }
            let i2 = i.min(hi);
            self.expr(start, i2, id, Clause::Set, false);
            i = i2;
            if i < hi && self.punct(i, ",") {
                i += 1;
                continue;
            }
            return i;
        }
    }

    fn target(&mut self, k: usize, id: usize) {
        if !self.a.scopes[id].sources.is_empty() {
            self.a.targets.push(TargetCol { scope: id, source: 0, name: self.value(k), range: self.t[k].range() });
        }
    }

    fn insert_target(&mut self, i: usize, hi: usize, id: usize) -> usize {
        if !self.is_name(i) {
            self.a.scopes[id].open = true;
            return i;
        }
        let (schema, name, mut k) = self.qualified(i);
        let mut alias = None;
        if self.word(k, "AS") && self.is_name(k + 1) {
            alias = Some(self.value(k + 1));
            k += 2;
        }
        self.a.scopes[id].sources.push(Source {
            alias,
            kind: SourceKind::Table { schema, name },
            name_range: self.t[i].start..self.t[k - 1].end,
            alias_columns: None,
        });
        if self.punct(k, "(") && !self.starts_query(k + 1) {
            let j = self.close[k].min(hi);
            for c in k + 1..j {
                if self.is_name(c) {
                    self.target(c, id);
                }
            }
            k = j + 1;
        }
        k
    }

    fn qualified(&self, i: usize) -> (Option<String>, String, usize) {
        let mut parts = vec![self.value(i)];
        let mut k = i + 1;
        let mut gap = false;
        loop {
            if self.punct(k, ".") && self.is_name(k + 1) {
                parts.push(self.value(k + 1));
                k += 2;
            } else if self.dialect == Dialect::TSql && self.punct(k, ".") && self.punct(k + 1, ".") && self.is_name(k + 2) {
                gap = true;
                parts.push(self.value(k + 2));
                k += 3;
            } else {
                break;
            }
        }
        let name = parts.pop().unwrap_or_default();
        let schema = if gap && parts.len() == 1 { Some(format!("{}..", parts[0])) } else if parts.is_empty() { None } else { Some(parts.join(".")) };
        (schema, name, k)
    }

    fn from_list(&mut self, mut i: usize, hi: usize, id: usize) -> usize {
        i = self.source(i, hi, id);
        loop {
            if i < hi && self.punct(i, ",") {
                i = self.source(i + 1, hi, id);
                continue;
            }
            let start = i;
            let mut k = i;
            let natural = self.word(k, "NATURAL");
            if natural {
                k += 1;
            }
            let cross = self.word(k, "CROSS");
            if cross {
                k += 1;
            }
            if self.any_word(k, &["LEFT", "RIGHT", "FULL"]) {
                k += 1;
            }
            if self.any_word(k, &["INNER", "OUTER"]) {
                k += 1;
            }
            if self.dialect == Dialect::TSql && k > i && self.any_word(k, &["LOOP", "HASH", "MERGE", "REMOTE"]) && self.word(k + 1, "JOIN") {
                k += 1;
            }
            if !self.word(k, "JOIN") {
                return i;
            }
            k += 1;
            let lateral = self.word(k, "LATERAL");
            let after = self.source(k, hi, id);
            if natural {
                self.a.scopes[id].natural = true;
            }
            i = after;
            if cross || natural {
                continue;
            }
            if self.word(i, "ON") {
                let mut e = i + 1;
                while e < hi && !self.punct(e, ",") && !self.is_join_start(e) && !self.is_clause_end_strict(e) {
                    if self.punct(e, "(") {
                        e = self.close[e] + 1;
                        continue;
                    }
                    e += 1;
                }
                let e = e.min(hi);
                self.expr(i + 1, e, id, Clause::On, false);
                i = e;
            } else if self.word(i, "USING") && self.punct(i + 1, "(") {
                let j = self.close[i + 1].min(hi);
                let cols = self.name_list(i + 2, j);
                self.a.scopes[id].using.extend(cols);
                i = j + 1;
            } else if after > k + usize::from(lateral) {
                let end = self.t.get(after - 1).map_or(self.src.len(), |t| t.end);
                self.a.missing_on.push(self.t[start].start..end);
            }
        }
    }

    fn is_join_start(&self, k: usize) -> bool {
        self.any_word(k, &["JOIN", "LEFT", "RIGHT", "FULL", "INNER", "CROSS", "NATURAL"])
    }

    fn is_clause_end_strict(&self, k: usize) -> bool {
        self.punct(k, ";")
            || ["WHERE", "GROUP", "HAVING", "WINDOW", "ORDER", "LIMIT", "OFFSET", "FETCH", "UNION", "INTERSECT", "EXCEPT", "RETURNING", "SET"]
                .iter()
                .any(|w| self.word(k, w))
    }

    fn source(&mut self, mut i: usize, hi: usize, id: usize) -> usize {
        while self.any_word(i, &["ONLY", "LATERAL"]) {
            i += 1;
        }
        if i >= hi {
            return i;
        }
        let start = i;
        let kind;
        if self.punct(i, "(") {
            let j = self.close[i].min(hi);
            if self.starts_query(i + 1) {
                let child = self.query(i + 1, j, Some(id), Some(id));
                self.a.scopes[child].role = Role::Derived;
                kind = SourceKind::Subquery(child);
            } else {
                let end = self.from_list(i + 1, j, id);
                if end < j {
                    self.a.scopes[id].open = true;
                }
                i = j + 1;
                return self.alias(i, hi, id, None, start);
            }
            i = j + 1;
        } else if self.word(i, "ROWS") && self.word(i + 1, "FROM") {
            let j = self.close.get(i + 2).copied().unwrap_or(hi).min(hi);
            self.expr(i + 3, j, id, Clause::From, false);
            kind = SourceKind::Function("rows from".into());
            i = j + 1;
        } else if self.is_name(i) || self.t[i].kind == TokenKind::Keyword && !self.is_clause_word(i) {
            let (schema, name, k) = self.qualified(i);
            i = k;
            if self.punct(i, "(") {
                self.a.calls.push((schema.clone(), name.clone(), self.t[i - 1].range()));
                let j = self.close[i].min(hi);
                self.expr(i + 1, j, id, Clause::From, false);
                i = j + 1;
                if self.word(i, "WITH") && self.word(i + 1, "ORDINALITY") {
                    i += 2;
                }
                kind = SourceKind::Function(name);
            } else {
                if self.text(i) == "*" {
                    i += 1;
                }
                kind = SourceKind::Table { schema, name };
            }
        } else if self.word(i, "VALUES") {
            self.a.scopes[id].open = true;
            return i;
        } else {
            self.a.scopes[id].open = true;
            return i;
        }
        let name_end = self.t.get(i - 1).map_or(self.src.len(), |t| t.end);
        let k = self.alias(i, hi, id, Some(kind), start);
        if let Some(s) = self.a.scopes[id].sources.last_mut()
            && self.t.get(start).is_some_and(|t| s.name_range.start == t.start)
        {
            s.name_range.end = s.name_range.end.min(name_end);
        }
        k
    }

    fn is_clause_word(&self, i: usize) -> bool {
        CLAUSE_WORDS.iter().any(|w| self.word(i, w))
    }

    fn alias(&mut self, mut i: usize, hi: usize, id: usize, kind: Option<SourceKind>, start: usize) -> usize {
        let name_end = self.t.get(i.saturating_sub(1)).map_or(0, |t| t.end);
        let mut alias = None;
        let mut alias_columns = None;
        if self.word(i, "AS") && i + 1 < hi {
            i += 1;
        }
        if i < hi && self.is_name(i) && !self.is_clause_word(i) {
            alias = Some(self.value(i));
            i += 1;
            if self.punct(i, "(") {
                let j = self.close[i].min(hi);
                alias_columns = Some(self.name_list(i + 1, j));
                i = j + 1;
            }
        }
        if let Some(kind) = kind {
            self.a.scopes[id].sources.push(Source {
                alias,
                kind,
                name_range: self.t[start].start..name_end,
                alias_columns,
            });
        }
        if self.word(i, "TABLESAMPLE") {
            i += 2;
            if self.punct(i, "(") {
                i = self.close[i] + 1;
            }
            if self.word(i, "REPEATABLE") && self.punct(i + 1, "(") {
                i = self.close[i + 1] + 1;
            }
        }
        if self.word(i, "FOR") && self.word(i + 1, "SYSTEM_TIME") {
            i += 2;
            while i < hi && !self.punct(i, ",") && !self.is_join_start(i) && !self.is_clause_end_strict(i) && !self.word(i, "ON") {
                if self.punct(i, "(") {
                    i = self.close[i] + 1;
                    continue;
                }
                i += 1;
            }
        }
        i.min(hi.max(i))
    }
}

impl Analysis {
    pub fn scope_at(&self, at: usize) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, s) in self.scopes.iter().enumerate() {
            if s.range.start <= at && at <= s.range.end {
                match best {
                    Some(b) if self.scopes[b].range.len() <= s.range.len() => {}
                    _ => best = Some(i),
                }
            }
        }
        best.or(if self.scopes.is_empty() { None } else { Some(0) })
    }

    pub fn chain(&self, scope: usize) -> Vec<usize> {
        let mut out = vec![scope];
        let mut s = scope;
        while let Some(p) = self.scopes[s].parent {
            if out.contains(&p) {
                break;
            }
            out.push(p);
            s = p;
        }
        out
    }

    pub fn visible_cte(&self, scope: usize, name: &str) -> Option<usize> {
        let mut s = Some(scope);
        let mut guard = 0;
        while let Some(i) = s {
            if let Some(&c) = self.scopes[i].ctes.iter().find(|&&c| self.ctes[c].name == name) {
                return Some(c);
            }
            s = self.scopes[i].cte_parent.or(self.scopes[i].parent);
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        None
    }

    pub fn visible_ctes(&self, scope: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut s = Some(scope);
        let mut guard = 0;
        while let Some(i) = s {
            out.extend(self.scopes[i].ctes.iter().copied());
            s = self.scopes[i].cte_parent.or(self.scopes[i].parent);
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        out
    }

    pub fn source_columns(&self, scope: usize, src: &Source, snap: &SchemaSnapshot) -> Option<Vec<(String, String)>> {
        self.source_columns_depth(scope, src, snap, 0)
    }

    fn source_columns_depth(&self, scope: usize, src: &Source, snap: &SchemaSnapshot, depth: usize) -> Option<Vec<(String, String)>> {
        if depth > 16 {
            return None;
        }
        let renamed = |mut cols: Vec<(String, String)>| {
            if let Some(names) = &src.alias_columns {
                for (c, n) in cols.iter_mut().zip(names) {
                    c.0 = n.clone();
                }
            }
            cols
        };
        match &src.kind {
            SourceKind::Table { schema: None, name } if self.visible_cte(scope, name).is_some() => {
                let c = self.visible_cte(scope, name)?;
                Some(renamed(self.cte_columns(c, snap, depth + 1)?))
            }
            SourceKind::Table { name, .. } if self.distrust.contains(&name.to_lowercase()) => None,
            SourceKind::Table { schema, name } => {
                let rel = snap.resolve(schema.as_deref(), name)?;
                let cols = snap.known_columns(rel)?;
                Some(renamed(cols.iter().map(|c| (c.name.clone(), c.type_name.clone())).collect()))
            }
            SourceKind::Subquery(q) => match self.outputs_depth(*q, snap, depth + 1) {
                Some(cols) => Some(renamed(cols)),
                None => src.alias_columns.as_ref().map(|v| v.iter().map(|n| (n.clone(), String::new())).collect()),
            },
            SourceKind::Function(_) | SourceKind::Values => {
                src.alias_columns.as_ref().map(|v| v.iter().map(|n| (n.clone(), String::new())).collect())
            }
        }
    }

    pub fn cte_columns(&self, cte: usize, snap: &SchemaSnapshot, depth: usize) -> Option<Vec<(String, String)>> {
        let c = &self.ctes[cte];
        let body = c.scope.and_then(|q| self.outputs_depth(q, snap, depth + 1));
        match (&c.columns, body) {
            (Some(names), Some(mut cols)) => {
                for (col, n) in cols.iter_mut().zip(names) {
                    col.0 = n.clone();
                }
                Some(cols)
            }
            (Some(names), None) => Some(names.iter().map(|n| (n.clone(), String::new())).collect()),
            (None, b) => b,
        }
    }

    #[cfg(test)]
    pub fn outputs(&self, scope: usize, snap: &SchemaSnapshot) -> Option<Vec<(String, String)>> {
        self.outputs_depth(scope, snap, 0)
    }

    fn outputs_depth(&self, scope: usize, snap: &SchemaSnapshot, depth: usize) -> Option<Vec<(String, String)>> {
        if depth > 16 {
            return None;
        }
        let s = &self.scopes[scope];
        if s.items.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for (ix, item) in s.items.iter().enumerate() {
            match item {
                Item::Named(n) => {
                    let ty = match s.item_refs.get(ix).cloned().flatten() {
                        Some((q, c)) => self.type_of_ref(scope, q.as_deref(), &c, snap, depth),
                        None => None,
                    };
                    out.push((n.clone(), ty.unwrap_or_default()));
                }
                Item::Expr => out.push(("?column?".into(), String::new())),
                Item::Star(None) => {
                    for src in &s.sources {
                        out.extend(self.source_columns_depth(scope, src, snap, depth + 1)?);
                    }
                }
                Item::Star(Some(q)) => {
                    let src = s.sources.iter().find(|x| x.visible_name() == Some(q.as_str()))?;
                    out.extend(self.source_columns_depth(scope, src, snap, depth + 1)?);
                }
            }
        }
        Some(out)
    }

    fn type_of_ref(&self, scope: usize, q: Option<&str>, n: &str, snap: &SchemaSnapshot, depth: usize) -> Option<String> {
        let s = &self.scopes[scope];
        let mut found = None;
        for src in s.sources.iter().filter(|x| q.is_none() || x.visible_name() == q) {
            if let Some(cols) = self.source_columns_depth(scope, src, snap, depth + 1)
                && let Some((_, t)) = cols.iter().find(|(c, _)| self.col_eq(c, n))
            {
                if found.is_some() {
                    return None;
                }
                found = Some(t.clone());
            }
        }
        found
    }

    pub fn find_source(&self, scope: usize, qualifier: &str) -> Option<(usize, usize)> {
        for s in self.chain(scope) {
            let sc = &self.scopes[s];
            if let Some(i) = sc.sources.iter().position(|x| x.alias.as_deref() == Some(qualifier)) {
                return Some((s, i));
            }
            if let Some(i) = sc.sources.iter().position(|x| x.alias.is_none() && x.visible_name() == Some(qualifier)) {
                return Some((s, i));
            }
        }
        None
    }

    pub fn ref_type(&self, scope: usize, qualifier: Option<&str>, name: &str, snap: &SchemaSnapshot) -> Option<String> {
        match qualifier {
            Some(q) => {
                let (s, i) = self.find_source(scope, q)?;
                let cols = self.source_columns(s, &self.scopes[s].sources[i], snap)?;
                cols.into_iter().find(|(c, _)| self.col_eq(c, name)).map(|(_, t)| t).filter(|t| !t.is_empty())
            }
            None => {
                for s in self.chain(scope) {
                    let mut hit = None;
                    let mut n = 0;
                    for src in &self.scopes[s].sources {
                        if let Some(cols) = self.source_columns(s, src, snap)
                            && let Some((_, t)) = cols.into_iter().find(|(c, _)| self.col_eq(c, name))
                        {
                            n += 1;
                            hit = Some(t);
                        }
                    }
                    if n == 1 {
                        return hit.filter(|t| !t.is_empty());
                    }
                    if n > 1 {
                        return None;
                    }
                }
                None
            }
        }
    }
}

pub fn quoted(name: &str) -> String {
    quote_ident(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, RelKind, Relation};
    use crate::sql::tokenize;
    use std::collections::HashMap;

    fn snap() -> SchemaSnapshot {
        let col = |n: &str, t: &str| Column { name: n.into(), type_name: t.into(), nullable: None, primary_key: false, default: None };
        let rel = |n: &str| Relation { schema: "public".into(), name: n.into(), kind: RelKind::Table, oid: None };
        let mut cols = HashMap::new();
        cols.insert(0, vec![col("id", "bigint"), col("customer", "bigint"), col("total", "numeric"), col("status", "text")]);
        cols.insert(1, vec![col("id", "bigint"), col("name", "text")]);
        let mut s = SchemaSnapshot::new(vec!["public".into()], vec!["public".into()], vec![rel("orders"), rel("customer")], cols);
        s.complete = true;
        s
    }

    fn an(sql: &str) -> Analysis {
        analyze(sql, &tokenize(sql))
    }

    fn names(v: Option<Vec<(String, String)>>) -> Vec<String> {
        v.unwrap_or_default().into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn ctes_and_their_columns() {
        let sql = "with big as (select id, total as t from orders where total > 10), n(x) as (select name from customer) select b.t from big b join n on true";
        let a = an(sql);
        let s = snap();
        assert_eq!(a.ctes.len(), 2);
        let main = 0;
        let big = a.visible_cte(main, "big").unwrap();
        assert_eq!(names(a.cte_columns(big, &s, 0)), vec!["id", "t"]);
        let n = a.visible_cte(main, "n").unwrap();
        assert_eq!(names(a.cte_columns(n, &s, 0)), vec!["x"]);
        let (sc, i) = a.find_source(main, "b").unwrap();
        assert_eq!(names(a.source_columns(sc, &a.scopes[sc].sources[i], &s)), vec!["id", "t"]);
        assert_eq!(a.cte_columns(big, &s, 0).unwrap()[1].1, "numeric");
        let a = an("with a as (select id from orders), b as (select id from a) select * from b");
        let b = a.visible_cte(0, "b").unwrap();
        assert_eq!(names(a.cte_columns(b, &s, 0)), vec!["id"]);
        assert_eq!(names(a.outputs(0, &s)), vec!["id"]);
    }

    #[test]
    fn subqueries_and_stars() {
        let s = snap();
        let a = an("select x.* from (select p.*, c.name from orders p join customer c on c.id = p.customer) x");
        let (sc, i) = a.find_source(0, "x").unwrap();
        assert_eq!(names(a.source_columns(sc, &a.scopes[sc].sources[i], &s)), vec!["id", "customer", "total", "status", "name"]);
        let a = an("select * from (select id, name from customer) as c(a, b)");
        assert_eq!(names(a.outputs(0, &s)), vec!["a", "b"]);
        let a = an("select * from orders p where exists (select 1 from customer c where c.id = p.customer)");
        let inner = a.scopes.iter().position(|x| x.parent == Some(0)).unwrap();
        assert!(a.find_source(inner, "p").is_some() && a.find_source(inner, "c").is_some());
        assert!(a.find_source(0, "c").is_none());
    }

    #[test]
    fn using_lateral_and_joins() {
        let a = an("select id from orders join customer using (id) cross join lateral (select 1 as one) l left join customer c2 on c2.id = orders.customer");
        assert_eq!(a.scopes[0].using, vec!["id"]);
        assert_eq!(a.scopes[0].sources.iter().filter_map(|s| s.visible_name().map(String::from)).collect::<Vec<_>>(), vec!["orders", "customer", "l", "c2"]);
        assert!(a.missing_on.is_empty());
        let sql = "select * from orders p join customer c where c.id = 1";
        let a = an(sql);
        assert_eq!(a.missing_on.len(), 1);
        assert_eq!(&sql[a.missing_on[0].clone()], "join customer c");
        let a = an("select * from orders natural join customer, generate_series(1, 3) g(n) cross join customer x");
        assert!(a.missing_on.is_empty() && a.scopes[0].natural);
        assert_eq!(a.scopes[0].sources[2].alias_columns, Some(vec!["n".into()]));
    }

    #[test]
    fn references_with_clauses() {
        let sql = "select p.id, total as t, count(*) n from orders p where p.status = 'x' and extract(year from now()) > 1 group by 1 order by t";
        let a = an(sql);
        let refs: Vec<(Option<&str>, &str, Clause)> = a.refs.iter().map(|r| (r.qualifier.as_deref(), r.name.as_str(), r.clause)).collect();
        assert_eq!(
            refs,
            vec![
                (Some("p"), "id", Clause::Select),
                (None, "total", Clause::Select),
                (Some("p"), "status", Clause::Where),
                (None, "t", Clause::OrderBy),
            ]
        );
        assert_eq!(a.scopes[0].items, vec![Item::Named("id".into()), Item::Named("t".into()), Item::Named("n".into())]);
        assert!(a.calls.iter().any(|c| c.1 == "count") && a.calls.iter().any(|c| c.1 == "extract"));
    }

    #[test]
    fn dml_targets() {
        let a = an("update orders set status = 'x', total = total + 1 where id = 1 returning id");
        assert_eq!(a.targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["status", "total"]);
        assert!(a.refs.iter().any(|r| r.name == "total" && r.clause == Clause::Set));
        let a = an("insert into orders (id, status) select id, name from customer on conflict (id) do update set status = excluded.status");
        assert_eq!(a.targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["id", "status", "status"]);
        let q = a.scopes.iter().position(|s| s.parent == Some(0)).unwrap();
        assert_eq!(a.scopes[q].sources[0].visible_name(), Some("customer"));
        let a = an("delete from orders p using customer c where c.id = p.customer");
        assert_eq!(a.scopes[0].sources.len(), 2);
    }

    #[test]
    fn unions_and_unfinished() {
        let a = an("select id from orders union all select id from customer order by 1");
        assert_eq!(a.scopes.len(), 2);
        assert_eq!(a.scopes[1].sources[0].visible_name(), Some("customer"));
        let a = an("select * from orders p where p.id in (select c.");
        assert!(a.scopes.len() >= 2);
        let a = an("with x as (select");
        assert_eq!(a.ctes.len(), 1);
    }
}
