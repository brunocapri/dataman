use crate::config::Environment;
use crate::sql::{Dialect, Token, TokenKind, tokenize_in};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    #[default]
    Read,
    Other,
    Tx,
    Write,
    Ddl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxCmd {
    Begin,
    Commit,
    Rollback,
    Savepoint,
    Release,
    RollbackTo,
    Prepared,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub verb: String,
    pub effect: Effect,
    pub target: Option<String>,
    pub no_where: bool,
    pub trivial_where: bool,
    pub merge_unbounded: bool,
    pub alter_drop: Option<String>,
    pub tx: Option<TxCmd>,
    pub outside_tx: bool,
}

impl Facts {
    pub fn is_write(&self) -> bool {
        matches!(self.effect, Effect::Write | Effect::Ddl)
    }

    pub fn ends_failed_tx(&self) -> bool {
        matches!(self.tx, Some(TxCmd::Rollback | TxCmd::RollbackTo | TxCmd::Commit | TxCmd::Prepared))
    }
}

struct Toks<'a> {
    src: &'a str,
    t: &'a [Token],
    dialect: Dialect,
}

impl<'a> Toks<'a> {
    fn word(&self, i: usize) -> Option<String> {
        let t = self.t.get(i)?;
        matches!(t.kind, TokenKind::Keyword | TokenKind::Ident).then(|| t.text(self.src).to_ascii_uppercase())
    }

    fn is(&self, i: usize, w: &str) -> bool {
        self.t.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(self.src).eq_ignore_ascii_case(w))
    }

    fn punct(&self, i: usize, p: &str) -> bool {
        self.t.get(i).is_some_and(|t| t.kind == TokenKind::Punct && t.text(self.src) == p)
    }

    fn is_name(&self, i: usize) -> bool {
        self.t.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword))
    }

    fn sub(&self, r: std::ops::Range<usize>) -> Toks<'a> {
        Toks { src: self.src, t: &self.t[r], dialect: self.dialect }
    }

    fn close_of(&self, open: usize) -> usize {
        let mut depth = 0usize;
        for i in open..self.t.len() {
            if self.punct(i, "(") || self.punct(i, "[") {
                depth += 1;
            } else if self.punct(i, ")") || self.punct(i, "]") {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i;
                }
            }
        }
        self.t.len()
    }

    fn top(&self) -> Vec<usize> {
        let mut depth = 0usize;
        let mut out = Vec::new();
        for i in 0..self.t.len() {
            if self.punct(i, "(") || self.punct(i, "[") {
                depth += 1;
            } else if self.punct(i, ")") || self.punct(i, "]") {
                depth = depth.saturating_sub(1);
            } else if depth == 0 {
                out.push(i);
            }
        }
        out
    }

    fn has_top(&self, w: &str) -> bool {
        self.top().into_iter().any(|i| self.is(i, w))
    }

    fn name(&self, mut i: usize) -> Option<(String, usize)> {
        if !self.is_name(i) {
            return None;
        }
        let mut s = self.t[i].text(self.src).to_string();
        i += 1;
        while self.punct(i, ".") && self.is_name(i + 1) {
            s.push('.');
            s.push_str(self.t[i + 1].text(self.src));
            i += 2;
        }
        Some((s, i))
    }

    fn names(&self, mut i: usize) -> Option<String> {
        let mut out = Vec::new();
        while let Some((n, next)) = self.name(i) {
            out.push(n);
            i = next;
            if self.t.get(i).is_some_and(|t| t.text(self.src) == "*") {
                i += 1;
            }
            if !self.punct(i, ",") {
                break;
            }
            i += 1;
        }
        (!out.is_empty()).then(|| out.join(", "))
    }
}

pub fn classify(sql: &str) -> Facts {
    classify_in(sql, Dialect::Postgres)
}

/// A text is its strongest statement, in every text the server may run for it.
pub fn classify_in(sql: &str, dialect: Dialect) -> Facts {
    let mut pieces = Vec::new();
    for text in crate::sql::exec_readings(sql, dialect) {
        let all = significant(&text, dialect);
        let t = Toks { src: &text, t: &all, dialect };
        pieces.extend(statements(&t).into_iter().map(|r| with_queries(&t.sub(r))).filter(|f| !f.verb.is_empty()));
    }
    combine(pieces)
}

fn significant(sql: &str, dialect: Dialect) -> Vec<Token> {
    tokenize_in(sql, dialect).into_iter().filter(|t| !t.kind.is_trivia()).collect()
}

/// The statements of a text the server reads (one of `exec_readings`), as token ranges: where
/// the splitter cuts it and, in T-SQL, where a statement starts without `;`. Every layer reads
/// its statements from here.
fn statements(t: &Toks) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    for s in crate::sql::split::split_tokens_in(t.src, t.t, t.dialect.script()) {
        let from = t.t.partition_point(|x| x.start < s.range.start);
        let to = t.t.partition_point(|x| x.end <= s.range.end);
        if t.dialect == Dialect::TSql {
            out.extend(tsql_pieces(&t.sub(from..to)).into_iter().map(|r| from + r.start..from + r.end));
        } else {
            out.push(from..to);
        }
    }
    out
}

fn combine(pieces: Vec<Facts>) -> Facts {
    if pieces.len() <= 1 {
        return pieces.into_iter().next().unwrap_or_default();
    }
    let ruled = RULES.iter().filter(|r| r.scope == Scope::Always).find_map(|r| pieces.iter().find(|f| matches(r.test, f)));
    let mut strongest = &pieces[0];
    for p in &pieces {
        if p.effect > strongest.effect {
            strongest = p;
        }
    }
    let mut f = ruled.unwrap_or(strongest).clone();
    f.effect = f.effect.max(strongest.effect);
    if f.tx.is_none() {
        f.tx = pieces.iter().find_map(|p| p.tx);
    }
    f.outside_tx = pieces.iter().any(|p| p.outside_tx);
    f
}

pub fn tsql_statements(sql: &str) -> Vec<std::ops::Range<usize>> {
    let all = significant(sql, Dialect::TSql);
    let t = Toks { src: sql, t: &all, dialect: Dialect::TSql };
    statements(&t).into_iter().filter(|r| !r.is_empty()).map(|r| all[r.start].start..all[r.end - 1].end).collect()
}

pub fn tsql_returns_rows(sql: &str) -> bool {
    let all: Vec<Token> = tokenize_in(sql, Dialect::TSql).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let t = Toks { src: sql, t: &all, dialect: Dialect::TSql };
    let top = t.top();
    let f = classify_in(sql, Dialect::TSql);
    match f.verb.as_str() {
        "SELECT" | "EXEC" => true,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => top.iter().enumerate().any(|(k, &i)| {
            t.is(i, "OUTPUT") && !top[k + 1..].iter().any(|&j| t.is(j, "INTO"))
        }),
        _ => false,
    }
}

const TSQL_HEADS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "DROP", "CREATE", "ALTER", "EXEC", "EXECUTE", "BULK", "GRANT",
    "REVOKE", "DENY", "BEGIN", "COMMIT", "ROLLBACK", "SAVE", "DECLARE", "USE", "PRINT", "IF", "WHILE", "RETURN", "WAITFOR",
    "RAISERROR", "THROW", "DBCC", "BACKUP", "RESTORE", "KILL",
];

/// Reserved words that only ever start a statement.
const TSQL_MORE_HEADS: &[&str] = &[
    "CHECKPOINT", "DEALLOCATE", "GOTO", "READTEXT", "RECONFIGURE", "SETUSER", "SHUTDOWN", "UPDATETEXT", "WRITETEXT", "REVERT",
    "OPEN", "CLOSE", "BREAK", "CONTINUE",
];

/// Statements T-SQL runs without a `;` before them, beyond `TSQL_HEADS`: what is not split off
/// is read as part of the statement before it (`SELECT 1 DISABLE TRIGGER …` is not a read).
fn tsql_head(t: &Toks, i: usize, w: &str, head: &str, prev: &str, start: usize) -> bool {
    match w {
        _ if TSQL_MORE_HEADS.contains(&w) => true,
        "DISABLE" | "ENABLE" => t.is(i + 1, "TRIGGER"),
        "SEND" => t.is(i + 1, "ON"),
        "MOVE" | "GET" | "END" => t.is(i + 1, "CONVERSATION"),
        "RECEIVE" => t.is(i + 1, "TOP") || t.punct(i + 1, "(") || t.t.get(i + 1).is_some_and(|x| x.text(t.src) == "*"),
        // `UPDATE t SET`, `THEN UPDATE SET`, `ON DELETE SET NULL`, `ALTER … SET`.
        "SET" => !(matches!(head, "ALTER" | "CREATE" | "GRANT" | "REVOKE" | "DENY")
            || matches!(prev, "UPDATE" | "DELETE")
            || head == "UPDATE" && !(start..i).any(|j| t.is(j, "SET"))),
        _ => false,
    }
}

fn tsql_pieces(t: &Toks) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut head = t.word(0).unwrap_or_default();
    let mut waiting = matches!(head.as_str(), "WITH" | "INSERT");
    let mut body = false;
    for i in 0..t.t.len() {
        if t.punct(i, "(") {
            depth += 1;
            continue;
        }
        if t.punct(i, ")") {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth > 0 || body {
            continue;
        }
        if t.punct(i, ";") {
            if i > start {
                out.push(start..i);
            }
            start = i + 1;
            head = t.word(i + 1).unwrap_or_default();
            waiting = matches!(head.as_str(), "WITH" | "INSERT");
            continue;
        }
        let Some(w) = t.word(i) else { continue };
        if i == start {
            body = matches!(w.as_str(), "CREATE" | "ALTER") && {
                let j = if t.is(i + 1, "OR") && t.is(i + 2, "ALTER") { i + 3 } else { i + 1 };
                ["PROC", "PROCEDURE", "FUNCTION", "TRIGGER", "VIEW"].iter().any(|k| t.is(j, k))
            };
            continue;
        }
        if head == "INSERT" && matches!(w.as_str(), "VALUES" | "DEFAULT") {
            waiting = false;
        }
        let prev = t.word(i - 1).unwrap_or_default();
        if !TSQL_HEADS.contains(&w.as_str()) {
            if tsql_head(t, i, &w, &head, &prev, start) {
                out.push(start..i);
                start = i;
                head = w;
                waiting = false;
            }
            continue;
        }
        let part = match w.as_str() {
            _ if matches!(head.as_str(), "GRANT" | "REVOKE" | "DENY") => true,
            _ if matches!(prev.as_str(), "FOR" | "THEN" | "UNION" | "ALL" | "EXCEPT" | "INTERSECT" | "AS") || t.punct(i - 1, ",") => true,
            "UPDATE" | "DELETE" if prev == "ON" && matches!(head.as_str(), "ALTER" | "CREATE") => true,
            "DROP" | "ALTER" if head == "ALTER" => true,
            "SELECT" | "EXEC" | "EXECUTE" | "UPDATE" | "DELETE" | "INSERT" | "MERGE" if waiting => true,
            _ => false,
        };
        if part {
            if waiting && matches!(w.as_str(), "SELECT" | "EXEC" | "EXECUTE" | "UPDATE" | "DELETE" | "INSERT" | "MERGE") {
                waiting = false;
            }
            continue;
        }
        out.push(start..i);
        start = i;
        head = w;
        waiting = matches!(head.as_str(), "WITH" | "INSERT");
    }
    if start < t.t.len() {
        out.push(start..t.t.len());
    }
    out
}

fn with_queries(t: &Toks) -> Facts {
    let mut f = if t.punct(0, "(") {
        let close = t.close_of(0).min(t.t.len());
        with_queries(&t.sub(1..close))
    } else {
        with_queries_top(t)
    };
    // A writable CTE below the top level (`SELECT (WITH d AS (DELETE …) SELECT …)`,
    // `… WHERE x IN (WITH i AS (INSERT …) SELECT …)`) still writes.
    let mut i = 0;
    while i + 1 < t.t.len() {
        if t.punct(i, "(") && t.is(i + 1, "WITH") {
            let close = t.close_of(i).min(t.t.len());
            absorb(&mut f, with_queries(&t.sub(i + 1..close)));
        }
        i += 1;
    }
    f
}

fn with_queries_top(t: &Toks) -> Facts {
    if !t.is(0, "WITH") {
        return statement(t);
    }
    let mut i = 1;
    if t.is(i, "RECURSIVE") {
        i += 1;
    }
    let mut ctes = Vec::new();
    loop {
        if !t.is_name(i) {
            break;
        }
        i += 1;
        if t.punct(i, "(") {
            i = t.close_of(i) + 1;
        }
        if !t.is(i, "AS") {
            break;
        }
        i += 1;
        if t.is(i, "NOT") {
            i += 1;
        }
        if t.is(i, "MATERIALIZED") {
            i += 1;
        }
        if !t.punct(i, "(") {
            break;
        }
        let close = t.close_of(i);
        ctes.push(with_queries(&t.sub(i + 1..close.min(t.t.len()))));
        i = close + 1;
        if t.punct(i, ",") {
            i += 1;
            continue;
        }
        break;
    }
    let mut f = statement(&t.sub(i.min(t.t.len())..t.t.len()));
    for c in ctes {
        absorb(&mut f, c);
    }
    f
}

fn absorb(f: &mut Facts, c: Facts) {
    if c.no_where && !f.no_where {
        f.no_where = true;
        f.verb = c.verb.clone();
        f.target = c.target.clone();
    }
    if c.trivial_where && !f.trivial_where && !f.no_where {
        f.trivial_where = true;
        f.verb = c.verb.clone();
        f.target = c.target.clone();
    }
    f.merge_unbounded |= c.merge_unbounded;
    if c.effect > f.effect {
        f.effect = c.effect;
        if !f.no_where && c.target.is_some() {
            f.verb = c.verb;
            f.target = c.target;
        }
    }
}

const OBJECT_WORDS: &[&str] = &[
    "TABLE", "VIEW", "MATERIALIZED", "INDEX", "SEQUENCE", "SCHEMA", "DATABASE", "FUNCTION", "PROCEDURE", "ROUTINE",
    "TRIGGER", "TYPE", "DOMAIN", "EXTENSION", "FOREIGN", "ROLE", "USER", "GROUP", "POLICY", "RULE", "OWNED", "BY",
    "AGGREGATE", "OPERATOR", "CLASS", "FAMILY", "COLLATION", "CONVERSION", "EVENT", "PUBLICATION", "SUBSCRIPTION",
    "SERVER", "TABLESPACE", "STATISTICS", "TEXT", "SEARCH", "CONFIGURATION", "DICTIONARY", "PARSER", "TEMPLATE",
    "LANGUAGE", "PROCEDURAL", "ACCESS", "METHOD", "CAST", "TRANSFORM", "DATA", "WRAPPER", "MAPPING", "FOR",
    "LARGE", "OBJECT", "SYSTEM", "DEFAULT", "PRIVILEGES",
];
const MODIFIERS: &[&str] =
    &["TEMP", "TEMPORARY", "UNLOGGED", "UNIQUE", "OR", "REPLACE", "ALTER", "GLOBAL", "LOCAL", "RECURSIVE", "TRUSTED", "CLUSTERED", "NONCLUSTERED"];

fn object_command(t: &Toks, first: &str) -> (String, Option<String>, Vec<String>) {
    let mut i = 1;
    let mut words = Vec::new();
    while let Some(w) = t.word(i) {
        if MODIFIERS.contains(&w.as_str()) {
            i += 1;
            continue;
        }
        if OBJECT_WORDS.contains(&w.as_str()) && words.len() < 3 {
            words.push(w);
            i += 1;
            continue;
        }
        break;
    }
    loop {
        if t.is(i, "IF") {
            i += 1;
            if t.is(i, "NOT") {
                i += 1;
            }
            if t.is(i, "EXISTS") {
                i += 1;
            }
        } else if t.is(i, "CONCURRENTLY") || t.is(i, "ONLY") {
            i += 1;
        } else {
            break;
        }
    }
    let shown: Vec<&str> = words.iter().filter(|w| !matches!(w.as_str(), "BY" | "FOR")).map(String::as_str).take(2).collect();
    let verb = if shown.is_empty() { first.to_string() } else { format!("{first} {}", shown.join(" ")) };
    let target = if first == "DROP" { t.names(i) } else { t.name(i).map(|(n, _)| n) };
    (verb, target, words)
}

fn statement(t: &Toks) -> Facts {
    let Some(v) = t.word(0) else { return Facts::default() };
    let mut f = Facts { verb: v.clone(), ..Default::default() };
    match v.as_str() {
        "SELECT" => {
            let top = t.top();
            if let Some(&at) = top.iter().find(|&&i| t.is(i, "INTO")) {
                f.effect = Effect::Ddl;
                f.verb = "SELECT INTO".into();
                let mut i = at + 1;
                while t.is(i, "TEMP") || t.is(i, "TEMPORARY") || t.is(i, "UNLOGGED") || t.is(i, "TABLE") {
                    i += 1;
                }
                f.target = t.name(i).map(|(n, _)| n);
            }
        }
        "TABLE" | "VALUES" | "SHOW" | "FETCH" | "MOVE" | "DECLARE" | "CLOSE" | "SUBSCRIBE" => {}
        "DESCRIBE" | "DESC" | "CHECK" | "CHECKSUM" | "HANDLER" if t.dialect == Dialect::MySql => {}
        "DESCRIBE" | "SUMMARIZE" | "FROM" | "PIVOT" | "UNPIVOT" if t.dialect == Dialect::DuckDb => {}
        "PRAGMA" if is_file_dialect(t.dialect) => pragma(t, &mut f),
        "ATTACH" | "DETACH" if is_file_dialect(t.dialect) => {
            f.effect = Effect::Ddl;
            f.outside_tx = true;
        }
        "INSTALL" | "LOAD" | "FORCE" | "SET" | "RESET" | "EXPORT" if t.dialect == Dialect::DuckDb => {
            f.effect = Effect::Ddl;
            if v == "FORCE" {
                f.verb = "FORCE INSTALL".into();
            } else if v == "EXPORT" {
                f.verb = "EXPORT DATABASE".into();
            }
        }
        "ANALYZE" | "CHECKPOINT" if is_file_dialect(t.dialect) => f.effect = Effect::Write,
        "VACUUM" if is_file_dialect(t.dialect) => {
            // `VACUUM INTO 'file'` writes a new file anywhere.
            f.effect = if t.has_top("INTO") { Effect::Ddl } else { Effect::Write };
            f.verb = if t.has_top("INTO") { "VACUUM INTO".into() } else { v.clone() };
            f.outside_tx = true;
        }
        "COPY" if t.dialect == Dialect::DuckDb => {
            f.effect = if t.has_top("TO") { Effect::Ddl } else { Effect::Write };
            f.verb = if t.has_top("TO") { "COPY TO".into() } else { "COPY FROM".into() };
            let (name, _) = t.name(1).unwrap_or_default();
            f.target = (!name.is_empty()).then_some(name);
        }
        "REPLACE" if matches!(t.dialect, Dialect::MySql | Dialect::Sqlite) => {
            f.effect = Effect::Write;
            let i = (0..t.t.len()).find(|&i| t.is(i, "INTO")).map_or(1, |i| i + 1);
            f.target = t.name(i).map(|(n, _)| n);
        }
        "LOAD" if t.dialect == Dialect::MySql && (t.is(1, "DATA") || t.is(1, "XML")) => {
            f.effect = Effect::Write;
            f.verb = format!("LOAD {}", t.word(1).unwrap_or_default());
            let i = (0..t.t.len()).find(|&i| t.is(i, "TABLE")).map_or(t.t.len(), |i| i + 1);
            f.target = t.name(i).map(|(n, _)| n);
        }
        "RENAME" if t.dialect == Dialect::MySql => {
            f.effect = Effect::Ddl;
            f.verb = format!("RENAME {}", t.word(1).unwrap_or_default()).trim_end().to_string();
            f.target = t.name(2).map(|(n, _)| n);
        }
        "START" if t.dialect == Dialect::MySql && !t.is(1, "TRANSACTION") => f.effect = Effect::Other,
        // T-SQL: a procedure may write (or do anything): a write, unless it is one of the
        // system procedures that only report.
        "EXEC" | "EXECUTE" if t.dialect == Dialect::TSql => {
            let mut i = 1;
            if t.t.get(1).is_some_and(|x| x.text(t.src).starts_with('@')) && t.t.get(2).is_some_and(|x| x.text(t.src) == "=") {
                i = 3;
            }
            f.verb = "EXEC".into();
            f.target = t.name(i).map(|(n, _)| n);
            let proc = f.target.as_deref().unwrap_or("").rsplit('.').next().unwrap_or("").trim_matches(['[', ']']).to_ascii_lowercase();
            f.effect = if READ_ONLY_PROCEDURES.contains(&proc.as_str()) { Effect::Read } else { Effect::Write };
        }
        "BULK" if t.dialect == Dialect::TSql => {
            f.effect = Effect::Write;
            f.verb = "BULK INSERT".into();
            f.target = t.name(2).map(|(n, _)| n);
        }
        "BEGIN" if t.dialect == Dialect::TSql && !(t.is(1, "TRAN") || t.is(1, "TRANSACTION") || t.is(1, "DISTRIBUTED")) => {}
        "SAVE" if t.dialect == Dialect::TSql => tx(&mut f, TxCmd::Savepoint),
        "ROLLBACK" if t.dialect == Dialect::TSql => {
            let mut i = 1;
            if t.is(i, "TRAN") || t.is(i, "TRANSACTION") || t.is(i, "WORK") {
                i += 1;
            }
            tx(&mut f, if i < t.t.len() { TxCmd::RollbackTo } else { TxCmd::Rollback });
        }
        "UPDATE" if t.dialect == Dialect::TSql && t.is(1, "STATISTICS") => f.effect = Effect::Other,
        "DENY" if t.dialect == Dialect::TSql => f.effect = Effect::Ddl,
        "RESTORE" if t.dialect == Dialect::TSql => {
            f.effect = Effect::Ddl;
            f.outside_tx = true;
        }
        "BACKUP" if t.dialect == Dialect::TSql => {
            f.effect = Effect::Other;
            f.outside_tx = true;
        }
        "EXPLAIN" => {
            // EXPLAIN ANALYZE runs the statement: it is what it explains.
            let mut i = 1;
            let mut analyze = false;
            if t.punct(i, "(") {
                let close = t.close_of(i);
                // Each option's own value: `(ANALYZE, COSTS OFF)` analyzes.
                let off = |j: usize| t.is(j, "FALSE") || t.is(j, "OFF") || t.t.get(j).is_some_and(|x| x.kind == TokenKind::Number && x.text(t.src) == "0");
                analyze = (i + 1..close).any(|j| (t.is(j, "ANALYZE") || t.is(j, "ANALYSE")) && (t.punct(j - 1, "(") || t.punct(j - 1, ",")) && !off(j + 1));
                i = close + 1;
            }
            while let Some(w) = t.word(i) {
                match w.as_str() {
                    "ANALYZE" | "ANALYSE" => analyze = true,
                    "VERBOSE" => {}
                    _ => break,
                }
                i += 1;
            }
            if analyze {
                return with_queries(&t.sub(i.min(t.t.len())..t.t.len()));
            }
        }
        "INSERT" => {
            f.effect = Effect::Write;
            let i = (0..t.t.len()).find(|&i| t.is(i, "INTO")).map_or(1, |i| i + 1);
            f.target = t.name(i).map(|(n, _)| n);
        }
        "MERGE" => {
            f.effect = Effect::Write;
            let i = top_clause(t, 1);
            let mut i = if t.is(i, "INTO") { i + 1 } else { i };
            if t.is(i, "ONLY") {
                i += 1;
            }
            f.target = t.name(i).map(|(n, _)| n);
            f.merge_unbounded = merge_unbounded(t);
        }
        "UPDATE" => {
            f.effect = Effect::Write;
            let mut i = top_clause(t, 1);
            while t.is(i, "ONLY") || t.is(i, "LOW_PRIORITY") || t.is(i, "IGNORE") {
                i += 1;
            }
            f.target = t.name(i).map(|(n, _)| n);
            f.no_where = !t.has_top("WHERE");
            f.trivial_where = where_trivial(t);
        }
        "DELETE" => {
            f.effect = Effect::Write;
            let mut i = top_clause(t, 1);
            while t.is(i, "LOW_PRIORITY") || t.is(i, "QUICK") || t.is(i, "IGNORE") {
                i += 1;
            }
            if t.is(i, "FROM") {
                i += 1;
            }
            if t.is(i, "ONLY") {
                i += 1;
            }
            f.target = t.name(i).map(|(n, _)| n);
            f.no_where = !t.has_top("WHERE");
            f.trivial_where = where_trivial(t);
        }
        "TRUNCATE" => {
            f.effect = Effect::Write;
            let mut i = 1;
            while t.is(i, "TABLE") || t.is(i, "ONLY") {
                i += 1;
            }
            f.target = t.names(i);
        }
        "COPY" => {
            let (name, _) = t.name(1).unwrap_or_default();
            f.target = (!name.is_empty()).then_some(name);
            if t.has_top("FROM") && !t.punct(1, "(") {
                f.effect = Effect::Write;
            }
        }
        "CALL" | "DO" => {
            f.effect = Effect::Write;
            if v == "CALL" {
                f.target = t.name(1).map(|(n, _)| n);
            }
        }
        "CREATE" | "DROP" | "ALTER" => {
            f.effect = Effect::Ddl;
            let (verb, target, words) = object_command(t, &v);
            f.verb = verb;
            f.target = target;
            let has = |w: &str| words.iter().any(|x| x == w);
            let concurrently = t.has_top("CONCURRENTLY");
            // SQL Server runs no ALTER DATABASE inside a user transaction either.
            f.outside_tx = (has("DATABASE") && (v != "ALTER" || t.dialect == Dialect::TSql))
                || has("TABLESPACE") && v != "ALTER"
                || (has("INDEX") && concurrently && v != "ALTER")
                || (v == "ALTER" && has("SYSTEM"));
            if v == "ALTER" {
                f.alter_drop = alter_drop(t);
            }
        }
        "COMMENT" | "GRANT" | "REVOKE" | "SECURITY" | "IMPORT" => f.effect = Effect::Ddl,
        "REINDEX" => {
            f.effect = Effect::Ddl;
            f.outside_tx = t.has_top("CONCURRENTLY") || t.is(1, "DATABASE") || t.is(1, "SYSTEM");
        }
        "CLUSTER" => {
            f.effect = Effect::Ddl;
            f.outside_tx = t.t.len() == 1;
        }
        "REFRESH" => {
            f.effect = Effect::Ddl;
            f.verb = "REFRESH MATERIALIZED VIEW".into();
            let mut i = 1;
            while matches!(t.word(i).as_deref(), Some("MATERIALIZED" | "VIEW" | "CONCURRENTLY")) {
                i += 1;
            }
            f.target = t.name(i).map(|(n, _)| n);
        }
        "VACUUM" => {
            f.effect = Effect::Other;
            f.outside_tx = true;
        }
        "DISCARD" => {
            f.effect = Effect::Other;
            f.outside_tx = t.is(1, "ALL");
        }
        "BEGIN" | "START" => tx(&mut f, TxCmd::Begin),
        "COMMIT" | "END" => tx(&mut f, if t.is(1, "PREPARED") { TxCmd::Prepared } else { TxCmd::Commit }),
        "ROLLBACK" | "ABORT" => {
            let cmd = if t.is(1, "PREPARED") {
                TxCmd::Prepared
            } else if (1..t.t.len()).any(|i| t.is(i, "TO")) {
                TxCmd::RollbackTo
            } else {
                TxCmd::Rollback
            };
            tx(&mut f, cmd);
        }
        "SAVEPOINT" => tx(&mut f, TxCmd::Savepoint),
        "RELEASE" => tx(&mut f, TxCmd::Release),
        "PREPARE" if t.is(1, "TRANSACTION") => tx(&mut f, TxCmd::Prepared),
        _ => f.effect = Effect::Other,
    }
    f
}

fn is_file_dialect(d: Dialect) -> bool {
    matches!(d, Dialect::Sqlite | Dialect::DuckDb)
}

/// PRAGMAs that only report, with or without an argument (SQLite and DuckDB names).
const READ_PRAGMAS: &[&str] = &[
    "application_id", "auto_vacuum", "automatic_index", "busy_timeout", "cache_size", "cache_spill", "case_sensitive_like",
    "cell_size_check", "checkpoint_fullfsync", "collation_list", "compile_options", "data_version", "database_list", "database_size",
    "defer_foreign_keys", "encoding", "foreign_key_check", "foreign_key_list", "foreign_keys", "freelist_count", "fullfsync",
    "function_list", "functions", "hard_heap_limit", "ignore_check_constraints", "index_info", "index_list", "index_xinfo",
    "integrity_check", "journal_mode", "journal_size_limit", "legacy_alter_table", "locking_mode", "max_page_count", "metadata_info",
    "mmap_size", "module_list", "page_count", "page_size", "platform", "pragma_list", "query_only", "quick_check", "read_uncommitted",
    "recursive_triggers", "reverse_unordered_selects", "schema_version", "secure_delete", "show", "show_databases", "show_tables",
    "show_tables_expanded", "soft_heap_limit", "storage_info", "synchronous", "table_info", "table_list", "table_xinfo", "temp_store",
    "threads", "trusted_schema", "user_agent", "user_version", "version", "wal_autocheckpoint", "writable_schema",
];

/// PRAGMAs whose argument names what to report on (a table, an index, a schema).
const ARG_READ_PRAGMAS: &[&str] = &[
    "foreign_key_check", "foreign_key_list", "index_info", "index_list", "index_xinfo", "integrity_check", "quick_check", "show",
    "storage_info", "table_info", "table_list", "table_xinfo",
];

/// PRAGMAs that act when merely named.
const ACTING_PRAGMAS: &[&str] = &["incremental_vacuum", "optimize", "shrink_memory", "wal_checkpoint"];

/// Whether `PRAGMA name` (with a value: `= v` or `(v)`) changes anything: the check SQLite's
/// authorizer makes for agents' connections, by the classifier's own lists.
pub fn pragma_sets(name: &str, valued: bool) -> bool {
    let name = name.to_ascii_lowercase();
    ACTING_PRAGMAS.contains(&name.as_str()) || (valued && !ARG_READ_PRAGMAS.contains(&name.as_str()))
}

/// `PRAGMA x = v` and `PRAGMA x(v)` set (but the report-only ones, which take a table); a bare
/// `PRAGMA x` reads (SQLite) or switches a setting (DuckDB, but its reports).
fn pragma(t: &Toks, f: &mut Facts) {
    let mut i = 1;
    if t.punct(i + 1, ".") {
        i += 2;
    }
    let name = t.word(i).unwrap_or_default().to_ascii_lowercase();
    f.verb = format!("PRAGMA {name}").trim_end().to_string();
    let reads = READ_PRAGMAS.contains(&name.as_str());
    let sets = t.op(i + 1, "=") || (t.punct(i + 1, "(") && !ARG_READ_PRAGMAS.contains(&name.as_str()));
    f.effect = if sets || ACTING_PRAGMAS.contains(&name.as_str()) || (t.dialect == Dialect::DuckDb && !reads) {
        Effect::Write
    } else {
        Effect::Read
    };
}

fn top_clause(t: &Toks, i: usize) -> usize {
    if t.dialect != Dialect::TSql || !t.is(i, "TOP") {
        return i;
    }
    let mut j = if t.punct(i + 1, "(") { t.close_of(i + 1) + 1 } else { i + 2 };
    if t.is(j, "PERCENT") {
        j += 1;
    }
    j
}

const READ_ONLY_PROCEDURES: &[&str] = &[
    "sp_help", "sp_helptext", "sp_helpindex", "sp_helpconstraint", "sp_helpdb", "sp_helpuser", "sp_helprole", "sp_helpsrvrole",
    "sp_columns", "sp_tables", "sp_stored_procedures", "sp_who", "sp_who2", "sp_lock", "sp_spaceused", "sp_depends",
    "sp_describe_first_result_set", "sp_server_info", "sp_databases", "sp_pkeys", "sp_fkeys", "sp_statistics",
];

fn tx(f: &mut Facts, cmd: TxCmd) {
    f.effect = Effect::Tx;
    f.tx = Some(cmd);
    // COMMIT PREPARED / ROLLBACK PREPARED cannot run in a transaction block.
    f.outside_tx = cmd == TxCmd::Prepared && f.verb != "PREPARE";
}

fn alter_drop(t: &Toks) -> Option<String> {
    const PROPERTIES: &[&str] = &["DEFAULT", "NOT", "IDENTITY", "EXPRESSION"];
    let top = t.top();
    for (k, &i) in top.iter().enumerate() {
        if !t.is(i, "DROP") {
            continue;
        }
        let next = top.get(k + 1).and_then(|&j| t.word(j));
        if next.as_deref().is_some_and(|w| PROPERTIES.contains(&w)) {
            continue;
        }
        let mut parts = vec!["DROP".to_string()];
        let mut j = i + 1;
        if let Some(w) = t.word(j).filter(|w| matches!(w.as_str(), "COLUMN" | "CONSTRAINT" | "ATTRIBUTE" | "VALUE")) {
            parts.push(w);
            j += 1;
        }
        if t.is(j, "IF") && t.is(j + 1, "EXISTS") {
            j += 2;
        }
        if let Some((n, _)) = t.name(j) {
            parts.push(n);
        }
        return Some(parts.join(" "));
    }
    None
}

const WHERE_END: &[&str] = &["RETURNING", "ORDER", "LIMIT", "OFFSET", "OPTION", "FOR", "GROUP", "HAVING", "WINDOW", "UNION", "EXCEPT", "INTERSECT", "ELSE"];

/// Reserved words only: a column may be named `date` or `user`.
const PREDICATE_WORDS: &[&str] = &[
    "AND", "OR", "NOT", "IS", "NULL", "TRUE", "FALSE", "UNKNOWN", "IN", "BETWEEN", "SYMMETRIC", "LIKE", "ILIKE", "SIMILAR", "TO", "ESCAPE",
    "EXISTS", "ANY", "ALL", "SOME", "CASE", "WHEN", "THEN", "ELSE", "END", "DISTINCT", "FROM", "COLLATE", "CURRENT_DATE", "CURRENT_TIME",
    "CURRENT_TIMESTAMP", "LOCALTIME", "LOCALTIMESTAMP", "CURRENT_USER", "SESSION_USER", "CURRENT_ROLE", "SYSDATE", "REGEXP", "RLIKE", "DIV",
    "XOR",
];

impl Toks<'_> {
    fn op(&self, i: usize, o: &str) -> bool {
        self.t.get(i).is_some_and(|t| t.kind == TokenKind::Operator && t.text(self.src) == o)
    }

    fn split_top(&self, word: &str) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let mut start = 0;
        for i in self.top() {
            if self.is(i, word) {
                out.push(start..i);
                start = i + 1;
            }
        }
        out.push(start..self.t.len());
        out
    }

    fn norm(&self, i: usize) -> String {
        let t = &self.t[i];
        let s = t.text(self.src);
        match t.kind {
            TokenKind::QuotedIdent => s.trim_matches(['"', '`', '[', ']']).to_ascii_lowercase(),
            TokenKind::Ident | TokenKind::Keyword => s.to_ascii_lowercase(),
            _ => s.to_string(),
        }
    }

    fn has_column(&self) -> bool {
        (0..self.t.len()).any(|i| {
            let t = &self.t[i];
            if !matches!(t.kind, TokenKind::Ident | TokenKind::Keyword | TokenKind::QuotedIdent) {
                return false;
            }
            let s = t.text(self.src);
            if t.kind != TokenKind::QuotedIdent && (s.starts_with('@') || PREDICATE_WORDS.iter().any(|w| s.eq_ignore_ascii_case(w))) {
                return false;
            }
            let after_cast = i > 0 && (self.op(i - 1, "::") || self.is(i - 1, "AS"));
            let literal = self.t.get(i + 1).is_some_and(|n| n.kind == TokenKind::String);
            !(self.punct(i + 1, "(") || after_cast || literal)
        })
    }
}

fn trivial(t: &Toks) -> bool {
    let n = t.t.len();
    if n == 0 {
        return true;
    }
    if t.punct(0, "(") && t.close_of(0) == n - 1 {
        return trivial(&t.sub(1..n - 1));
    }
    let ors = t.split_top("OR");
    if ors.len() > 1 {
        return ors.into_iter().any(|r| trivial(&t.sub(r)));
    }
    let ands = t.split_top("AND");
    if ands.len() > 1 {
        return ands.into_iter().all(|r| !r.is_empty() && trivial(&t.sub(r)));
    }
    if !t.has_column() {
        return true;
    }
    let top = t.top();
    for &i in &top {
        let cmp = t.op(i, "=") || t.op(i, ">=") || t.op(i, "<=") || t.op(i, "==") || t.is(i, "LIKE");
        if !cmp || i == 0 || i + 1 >= n || top.iter().any(|&j| j != i && (t.op(j, "=") || t.op(j, "<>") || t.op(j, "!="))) {
            continue;
        }
        let left: Vec<String> = (0..i).map(|k| t.norm(k)).collect();
        let right: Vec<String> = (i + 1..n).map(|k| t.norm(k)).collect();
        if left == right {
            return true;
        }
    }
    false
}

fn where_trivial(t: &Toks) -> bool {
    let top = t.top();
    let Some(k) = top.iter().position(|&i| t.is(i, "WHERE")) else { return false };
    let start = top[k] + 1;
    if t.is(start, "CURRENT") && t.is(start + 1, "OF") {
        return false;
    }
    let end = top[k + 1..].iter().copied().find(|&i| WHERE_END.iter().any(|w| t.is(i, w)) || t.punct(i, ";")).unwrap_or(t.t.len());
    trivial(&t.sub(start..end.max(start)))
}

fn merge_unbounded(t: &Toks) -> bool {
    let top = t.top();
    if top.iter().any(|&i| t.is(i, "NOT") && t.is(i + 1, "MATCHED") && t.is(i + 2, "BY") && t.is(i + 3, "SOURCE")) {
        return true;
    }
    let Some(k) = top.iter().position(|&i| t.is(i, "ON")) else { return true };
    let start = top[k] + 1;
    let end = top[k + 1..].iter().copied().find(|&i| t.is(i, "WHEN")).unwrap_or(t.t.len());
    trivial(&t.sub(start..end.max(start)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Always,
    Production,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Test {
    NoWhere,
    Command(&'static str),
    AlterDrop,
    Effect(Effect),
}

#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub id: &'static str,
    pub scope: Scope,
    pub test: Test,
}

pub const RULES: &[Rule] = &[
    Rule { id: "no-where", scope: Scope::Always, test: Test::NoWhere },
    Rule { id: "truncate", scope: Scope::Always, test: Test::Command("TRUNCATE") },
    Rule { id: "drop", scope: Scope::Always, test: Test::Command("DROP") },
    Rule { id: "alter-drop", scope: Scope::Always, test: Test::AlterDrop },
    Rule { id: "write", scope: Scope::Production, test: Test::Effect(Effect::Write) },
    Rule { id: "ddl", scope: Scope::Production, test: Test::Effect(Effect::Ddl) },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub index: usize,
    pub rule: &'static str,
    pub label: String,
    pub target: Option<String>,
}

fn matches(test: Test, f: &Facts) -> bool {
    match test {
        Test::NoWhere => f.no_where,
        Test::Command(c) => f.verb == c || f.verb.starts_with(&format!("{c} ")),
        Test::AlterDrop => f.alter_drop.is_some(),
        Test::Effect(e) => f.effect == e,
    }
}

pub fn check(sql: &str, env: Environment) -> Option<(&'static Rule, Facts)> {
    check_in(sql, env, Dialect::Postgres)
}

pub fn check_in(sql: &str, env: Environment, dialect: Dialect) -> Option<(&'static Rule, Facts)> {
    let f = classify_in(sql, dialect);
    RULES
        .iter()
        .filter(|r| r.scope == Scope::Always || env.is_production())
        .find(|r| matches(r.test, &f))
        .map(|r| (r, f))
}

pub fn review<S: AsRef<str>>(stmts: &[S], env: Environment) -> Vec<Finding> {
    review_in(stmts, env, Dialect::Postgres)
}

pub fn review_in<S: AsRef<str>>(stmts: &[S], env: Environment, dialect: Dialect) -> Vec<Finding> {
    stmts
        .iter()
        .enumerate()
        .filter_map(|(index, s)| {
            let (rule, f) = check_in(s.as_ref(), env, dialect)?;
            let label = match rule.test {
                Test::NoWhere => format!("{} without WHERE", f.verb),
                Test::AlterDrop => format!("{} · {}", f.verb, f.alter_drop.clone().unwrap_or_default()),
                _ => f.verb.clone(),
            };
            Some(Finding { index, rule: rule.id, label, target: f.target })
        })
        .collect()
}

pub fn read_only_refusal(sql: &str) -> Option<String> {
    read_only_refusal_in(sql, Dialect::Postgres)
}

pub fn read_only_refusal_in(sql: &str, dialect: Dialect) -> Option<String> {
    let f = classify_in(sql, dialect);
    f.is_write().then(|| format!("read-only connection: {} is not run", f.verb))
}

/// Functions with effects that a read-only transaction does not stop (signals, files, locks
/// held past the transaction, other servers) or that only PostgreSQL stops there (LAR does not
/// enforce READ ONLY).
pub const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    "pg_terminate_backend", "pg_cancel_backend", "pg_reload_conf", "pg_rotate_logfile", "pg_switch_wal", "pg_create_restore_point",
    "pg_promote", "pg_wal_replay_pause", "pg_wal_replay_resume", "pg_log_backend_memory_contexts", "pg_advisory_lock",
    "pg_advisory_lock_shared", "pg_try_advisory_lock", "pg_try_advisory_lock_shared", "pg_advisory_unlock", "pg_advisory_unlock_all",
    "pg_advisory_unlock_shared", "lo_import", "lo_export", "lo_unlink", "lo_create", "lo_creat", "lo_from_bytea", "lo_put", "lowrite",
    "pg_file_write", "pg_file_rename", "pg_file_unlink", "pg_file_sync", "pg_read_file", "pg_read_binary_file", "pg_ls_dir",
    "pg_stat_file", "dblink", "dblink_exec", "dblink_connect", "dblink_connect_u", "dblink_send_query", "dblink_open", "pg_notify",
    "nextval", "setval", "pg_stat_reset", "pg_stat_reset_shared", "pg_stat_reset_single_table_counters", "pg_stat_statements_reset",
    "pg_drop_replication_slot", "pg_create_physical_replication_slot", "pg_create_logical_replication_slot",
    "pg_logical_emit_message", "pg_replication_origin_create", "pg_replication_origin_drop", "pg_import_system_collations",
    "txid_current", "pg_current_xact_id", "set_config",
    "get_lock", "release_lock", "release_all_locks", "load_file", "master_pos_wait", "source_pos_wait", "wait_for_executed_gtid_set",
    "openrowset", "openquery", "opendatasource",
];

const TSQL_LOCK_HINTS: &[&str] = &["UPDLOCK", "XLOCK", "HOLDLOCK", "TABLOCK", "TABLOCKX", "PAGLOCK", "ROWLOCK", "SERIALIZABLE", "REPEATABLEREAD"];

const LAR_ACTION_WORDS: &[&str] = &[
    "backup", "restore", "create", "drop", "alter", "grant", "revoke", "user", "password", "warm", "compact", "checkpoint",
    "vacuum", "promote", "demote", "kill", "cancel", "terminate", "reset", "set", "flush", "import", "export", "write", "delete",
    "truncate", "reload", "shutdown", "failover", "join", "leave", "evict", "rotate",
];

pub fn agent_read_refusal(sql: &str) -> Option<String> {
    agent_read_refusal_in(sql, Dialect::Postgres)
}

/// Each statement on its own, in every text the server may run: what follows a read in a T-SQL
/// batch is no read for being there.
pub fn agent_read_refusal_in(sql: &str, dialect: Dialect) -> Option<String> {
    let mut any = false;
    for text in crate::sql::exec_readings(sql, dialect) {
        let all = significant(&text, dialect);
        let t = Toks { src: &text, t: &all, dialect };
        for r in statements(&t).into_iter().filter(|r| !r.is_empty()) {
            any = true;
            if let Some(e) = read_refusal(&text[all[r.start].start..all[r.end - 1].end], dialect) {
                return Some(e);
            }
        }
    }
    (!any).then(|| "empty statement".into())
}

fn read_refusal(sql: &str, dialect: Dialect) -> Option<String> {
    let f = classify_in(sql, dialect);
    if f.verb.is_empty() {
        return Some("empty statement".into());
    }
    if f.tx.is_some() {
        return Some(format!("transaction control ({}) is not allowed", f.verb));
    }
    let reads = matches!(f.verb.as_str(), "SELECT" | "TABLE" | "VALUES" | "SHOW" | "EXPLAIN")
        || (dialect == Dialect::MySql && matches!(f.verb.as_str(), "DESCRIBE" | "DESC"))
        || (dialect == Dialect::DuckDb && matches!(f.verb.as_str(), "DESCRIBE" | "SUMMARIZE" | "FROM" | "PIVOT" | "UNPIVOT"))
        || (is_file_dialect(dialect) && f.verb.starts_with("PRAGMA "));
    if f.effect != Effect::Read || !reads {
        return Some(format!("{} is not a read; read-only access runs SELECT, WITH, TABLE, VALUES, SHOW and EXPLAIN", f.verb));
    }
    let all: Vec<Token> = tokenize_in(sql, dialect).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let t = Toks { src: sql, t: &all, dialect };
    for (k, &i) in t.top().iter().enumerate() {
        if t.is(i, "FOR") {
            let next = t.top().get(k + 1).and_then(|&j| t.word(j));
            if matches!(next.as_deref(), Some("UPDATE" | "SHARE" | "NO" | "KEY")) {
                return Some("row locks (FOR UPDATE / FOR SHARE) are not allowed".into());
            }
        }
        if t.is(i, "LOCK") && t.is(i + 1, "IN") {
            return Some("row locks (LOCK IN SHARE MODE) are not allowed".into());
        }
    }
    if dialect == Dialect::TSql {
        // SQL Server has no read-only transactions: what a read must not do is checked here.
        for i in 0..all.len() {
            if t.is(i, "NEXT") && t.is(i + 1, "VALUE") && t.is(i + 2, "FOR") {
                return Some("NEXT VALUE FOR advances a sequence".into());
            }
            if TSQL_LOCK_HINTS.iter().any(|h| t.is(i, h)) && (t.punct(i.wrapping_sub(1), "(") || t.punct(i.wrapping_sub(1), ",")) {
                return Some("lock hints (UPDLOCK, XLOCK, HOLDLOCK…) are not allowed".into());
            }
        }
    }
    if let Some(e) = file_access_in(&t) {
        return Some(e);
    }
    match effect_call(&t, &[]) {
        Some((name, false)) => Some(format!("{name}() has effects outside a read-only transaction")),
        Some((name, true)) => Some(format!("{name}() is an administrative action")),
        None => None,
    }
}

/// SQLite functions that load code or touch files (the shell's; an application may register
/// them too).
pub const SQLITE_EFFECT_FUNCTIONS: &[&str] = &["load_extension", "readfile", "writefile", "edit", "fts3_tokenizer", "fsdir"];

/// DuckDB reads (and runs) whatever these name: files, other databases, SQL in a string,
/// secrets. Refused to agents at every level.
pub const DUCKDB_FILE_FUNCTIONS: &[&str] = &[
    "read_csv", "read_csv_auto", "csv_scan", "sniff_csv", "read_parquet", "parquet_scan", "parquet_metadata", "parquet_schema",
    "parquet_file_metadata", "parquet_kv_metadata", "parquet_bloom_probe", "read_json", "read_json_auto", "read_json_objects",
    "read_json_objects_auto", "read_ndjson", "read_ndjson_auto", "read_ndjson_objects", "json_scan", "read_text", "read_blob", "glob",
    "read_xlsx", "st_read", "iceberg_scan", "iceberg_metadata", "iceberg_snapshots", "delta_scan", "sqlite_scan", "sqlite_attach",
    "postgres_scan", "postgres_attach", "postgres_query", "mysql_scan", "mysql_query", "query", "query_table", "duckdb_secrets",
    "which_secret", "getenv", "load_extension", "read_duckdb", "parquet_full_metadata",
    // SQL inside a string, and raw memory.
    "json_execute_serialized_sql", "arrow_scan", "arrow_scan_dumb",
    // The instance's state, which every connection to the file shares, and its logs (others' statements).
    "enable_logging", "disable_logging", "truncate_duckdb_logs", "enable_profiling", "disable_profiling", "checkpoint", "force_checkpoint",
    "duckdb_logs", "duckdb_logs_parsed", "duckdb_temporary_files", "duckdb_external_file_cache", "duckdb_prepared_statements",
];

/// DuckDB statements and functions that reach files or other databases: `read_csv(…)`, a file
/// named in FROM (`FROM 'x.csv'`, `FROM "x.parquet"`), COPY, ATTACH, INSTALL, EXPORT.
pub fn file_access(sql: &str, dialect: Dialect) -> Option<String> {
    if !is_file_dialect(dialect) {
        return None;
    }
    for piece in agent_pieces(sql, dialect) {
        let all = significant(&piece, dialect);
        let t = Toks { src: &piece, t: &all, dialect };
        let f = classify_in(&piece, dialect);
        if matches!(f.verb.as_str(), "ATTACH" | "DETACH" | "INSTALL" | "FORCE INSTALL" | "LOAD" | "COPY TO" | "COPY FROM" | "EXPORT DATABASE" | "IMPORT" | "VACUUM INTO") {
            return Some(format!("{} reaches files outside the database", f.verb));
        }
        // A DuckDB file is one instance in the process: its settings (log and temp paths, memory,
        // extensions, external access) are every connection's, the user's tabs included.
        if dialect == Dialect::DuckDb && (matches!(f.verb.as_str(), "SET" | "RESET") || (f.verb.starts_with("PRAGMA") && f.effect != Effect::Read)) {
            return Some(format!("{} changes the settings of the database file, which every connection to it shares", f.verb));
        }
        if let Some(e) = file_access_in(&t) {
            return Some(e);
        }
    }
    None
}

fn file_access_in(t: &Toks) -> Option<String> {
    let list = match t.dialect {
        Dialect::DuckDb => DUCKDB_FILE_FUNCTIONS,
        Dialect::Sqlite => SQLITE_EFFECT_FUNCTIONS,
        _ => return None,
    };
    for i in 0..t.t.len() {
        if !matches!(t.t[i].kind, TokenKind::Ident | TokenKind::Keyword | TokenKind::QuotedIdent) {
            continue;
        }
        let name = t.t[i].text(t.src).trim_matches(['"', '`', '[', ']']).to_ascii_lowercase();
        // DuckDB's `duckdb_…` functions are views too (`FROM duckdb_logs`).
        if (t.punct(i + 1, "(") || name.starts_with("duckdb_")) && list.contains(&name.as_str()) {
            return Some(format!("{name}() reaches files or code outside the database"));
        }
    }
    if t.dialect != Dialect::DuckDb {
        return None;
    }
    // A string (or a quoted name that looks like a path) where a table goes: DuckDB reads the file.
    const CLAUSE_END: &[&str] = &["WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "QUALIFY", "WINDOW", "UNION", "EXCEPT", "INTERSECT", "ON", "USING", "SELECT", "SET", "VALUES", "RETURNING", "OFFSET"];
    let mut in_from = false;
    for i in 0..t.t.len() {
        let tok = &t.t[i];
        if let Some(w) = t.word(i) {
            if matches!(w.as_str(), "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" | "DESCRIBE" | "SUMMARIZE" | "PIVOT" | "UNPIVOT") {
                in_from = true;
                continue;
            }
            if CLAUSE_END.contains(&w.as_str()) {
                in_from = false;
            }
            continue;
        }
        // A qualified name (`s."x.csv"`) stands where its first part does.
        let mut j = i;
        while j >= 2 && t.punct(j - 1, ".") && matches!(t.t[j - 2].kind, TokenKind::Ident | TokenKind::QuotedIdent) {
            j -= 2;
        }
        let at_table = j > 0 && (matches!(t.word(j - 1).as_deref(), Some("FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" | "DESCRIBE" | "SUMMARIZE" | "PIVOT" | "UNPIVOT")) || (in_from && t.punct(j - 1, ",")));
        let path_like = |s: &str| s.contains(['.', '/', '\\', '~', ':', '*']);
        match tok.kind {
            TokenKind::String | TokenKind::DollarString if at_table => return Some("a file named in FROM is read from disk".into()),
            TokenKind::QuotedIdent if at_table && path_like(tok.text(t.src)) => return Some("a file named in FROM is read from disk".into()),
            _ => {}
        }
    }
    None
}

fn effect_call(t: &Toks, allowed: &[&str]) -> Option<(String, bool)> {
    if let Some(e) = file_access_in(t) {
        return Some((e.split_once("()").map_or(e.as_str(), |(f, _)| f).to_string(), false));
    }
    for i in 0..t.t.len() {
        if !t.punct(i + 1, "(") || !matches!(t.t[i].kind, TokenKind::Ident | TokenKind::Keyword | TokenKind::QuotedIdent) {
            continue;
        }
        let name = t.t[i].text(t.src).trim_matches(['"', '`', '[', ']']).to_ascii_lowercase();
        if SIDE_EFFECT_FUNCTIONS.contains(&name.as_str()) && !allowed.contains(&name.as_str()) {
            return Some((name, false));
        }
        if let Some(rest) = name.strip_prefix("lar_")
            && rest.split('_').any(|w| LAR_ACTION_WORDS.contains(&w))
        {
            return Some((name, true));
        }
    }
    None
}

/// Whatever is not understood is destructive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteClass {
    Read,
    SafeWrite,
    Destructive { reasons: Vec<String> },
}

impl WriteClass {
    pub fn is_write(&self) -> bool {
        !matches!(self, WriteClass::Read)
    }

    pub fn is_destructive(&self) -> bool {
        matches!(self, WriteClass::Destructive { .. })
    }

    pub fn reasons(&self) -> &[String] {
        match self {
            WriteClass::Destructive { reasons } => reasons,
            _ => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Preview {
    Held,
    /// A preview would hold the exclusive locks of DDL and `TRUNCATE` while waiting, and MySQL and
    /// LAR commit them anyway; procedures and functions with outside effects would act before the
    /// answer.
    AskFirst,
    Outside,
}

pub const WRITE_OK_FUNCTIONS: &[&str] = &["nextval", "pg_notify", "txid_current", "pg_current_xact_id"];

const TSQL_SESSION_SETTINGS: &[&str] = &[
    "NOCOUNT", "XACT_ABORT", "ANSI_NULLS", "ANSI_WARNINGS", "ANSI_PADDING", "ANSI_NULL_DFLT_ON", "QUOTED_IDENTIFIER", "ARITHABORT",
    "CONCAT_NULL_YIELDS_NULL", "NUMERIC_ROUNDABORT", "DATEFORMAT", "DATEFIRST", "LANGUAGE", "LOCK_TIMEOUT", "DEADLOCK_PRIORITY", "TEXTSIZE",
    "ROWCOUNT", "STATISTICS",
];

pub fn agent_pieces(sql: &str, dialect: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    for s in crate::sql::split_in(sql, dialect) {
        let stmt = &sql[s.range];
        if dialect == Dialect::TSql {
            out.extend(tsql_statements(stmt).into_iter().map(|r| stmt[r].to_string()));
        } else if !stmt.trim().is_empty() {
            out.push(stmt.to_string());
        }
    }
    out
}

/// Weakest first.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Piece {
    Neutral,
    Read,
    Safe,
    Destructive(String),
}

fn neutral(t: &Toks, f: &Facts) -> bool {
    let var = |i: usize| t.t.get(i).is_some_and(|x| x.kind == TokenKind::Ident && x.text(t.src).starts_with('@') && !x.text(t.src).starts_with("@@"));
    let setting = |i: usize| t.word(i).is_some_and(|w| TSQL_SESSION_SETTINGS.contains(&w.as_str()) || w.starts_with("ANSI_"));
    // Every item of a list: `SET @a = 1, sql_mode = …` sets more than a variable.
    let each = |first: &dyn Fn(usize) -> bool| first(1) && t.top().into_iter().all(|i| !t.punct(i, ",") || first(i + 1));
    match (t.dialect, f.verb.as_str()) {
        (Dialect::TSql, "DECLARE") => var(1),
        (Dialect::TSql, "SET") => var(1) || each(&setting),
        (Dialect::TSql, "PRINT") => true,
        (Dialect::MySql, "SET") => each(&var),
        _ => false,
    }
}

/// The strongest of the texts the server may run.
fn piece(sql: &str, dialect: Dialect) -> Piece {
    crate::sql::exec_readings(sql, dialect).iter().map(|text| piece_of(text, dialect)).max().unwrap_or(Piece::Neutral)
}

fn piece_of(sql: &str, dialect: Dialect) -> Piece {
    let all = significant(sql, dialect);
    let t = Toks { src: sql, t: &all, dialect };
    if all.iter().all(|x| x.kind == TokenKind::Punct && x.text(sql) == ";") {
        return Piece::Neutral;
    }
    let f = classify_in(sql, dialect);
    // What the user is asked to allow says so when it runs a program on the database server.
    if f.verb.starts_with("COPY") && t.has_top("PROGRAM") {
        return Piece::Destructive("COPY … PROGRAM (runs a command on the database server)".into());
    }
    if all.iter().any(|x| x.text(sql).trim_matches(['"', '[', ']']).eq_ignore_ascii_case("xp_cmdshell")) {
        return Piece::Destructive("xp_cmdshell (runs a command on the database server)".into());
    }
    if let Some((name, _)) = effect_call(&t, WRITE_OK_FUNCTIONS) {
        return Piece::Destructive(if name.contains(' ') { name } else { format!("{name}()") });
    }
    if f.tx.is_some() {
        return Piece::Destructive(format!("transaction control ({})", f.verb));
    }
    if f.no_where {
        return Piece::Destructive(format!("{} without WHERE", f.verb));
    }
    if f.trivial_where {
        return Piece::Destructive(format!("{} with a WHERE that is always true", f.verb));
    }
    if f.merge_unbounded {
        return Piece::Destructive("MERGE not bounded by its source".into());
    }
    match f.effect {
        _ if neutral(&t, &f) => Piece::Neutral,
        Effect::Read if agent_read_refusal_in(sql, dialect).is_none() => Piece::Read,
        Effect::Read if matches!(f.verb.as_str(), "SELECT" | "TABLE" | "VALUES") => Piece::Safe,
        Effect::Write => match f.verb.as_str() {
            "INSERT" if dialect == Dialect::TSql && t.top().iter().any(|&i| t.is(i, "EXEC") || t.is(i, "EXECUTE")) => {
                Piece::Destructive("INSERT … EXEC".into())
            }
            "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" => Piece::Safe,
            other => Piece::Destructive(other.to_string()),
        },
        _ => Piece::Destructive(if f.verb.is_empty() { "an unknown statement".into() } else { f.verb.clone() }),
    }
}

pub fn agent_write_class(sql: &str, dialect: Dialect) -> WriteClass {
    pieces_class(&agent_pieces(sql, dialect), dialect)
}

/// The class of pieces as `agent_pieces` cut them (what an allowlist leaves): joined again, a
/// T-SQL routine would take in what follows it.
pub fn pieces_class<S: AsRef<str>>(pieces: &[S], dialect: Dialect) -> WriteClass {
    let mut safe = false;
    let mut reasons: Vec<String> = Vec::new();
    for p in pieces {
        match piece(p.as_ref(), dialect) {
            Piece::Neutral | Piece::Read => {}
            Piece::Safe => safe = true,
            Piece::Destructive(r) => {
                if !reasons.contains(&r) {
                    reasons.push(r);
                }
            }
        }
    }
    if !reasons.is_empty() {
        WriteClass::Destructive { reasons }
    } else if safe {
        WriteClass::SafeWrite
    } else {
        WriteClass::Read
    }
}

pub fn agent_preview(sql: &str, dialect: Dialect) -> Preview {
    let pieces = agent_pieces(sql, dialect);
    pieces.iter().flat_map(|p| crate::sql::exec_readings(p, dialect)).map(|p| preview_of(&p, dialect)).max().unwrap_or(Preview::Held)
}

fn preview_of(p: &str, dialect: Dialect) -> Preview {
    let all = significant(p, dialect);
    let t = Toks { src: p, t: &all, dialect };
    let f = classify_in(p, dialect);
    if f.outside_tx {
        Preview::Outside
    } else if f.effect == Effect::Ddl
        || matches!(f.effect, Effect::Other | Effect::Tx) && !neutral(&t, &f)
        || matches!(f.verb.as_str(), "TRUNCATE" | "CALL" | "DO" | "EXEC" | "EXECUTE")
        || effect_call(&t, WRITE_OK_FUNCTIONS).is_some()
        || matches!(piece_of(p, dialect), Piece::Destructive(r) if r == "INSERT … EXEC")
    {
        Preview::AskFirst
    } else {
        Preview::Held
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AllowTok {
    Value,
    Text(TokenKind, String),
}

fn is_value(k: TokenKind) -> bool {
    matches!(k, TokenKind::String | TokenKind::DollarString | TokenKind::Number | TokenKind::Param)
}

/// PostgreSQL lexes `=?` as one operator: in an entry its `?` is split off.
fn allow_tokens(sql: &str, dialect: Dialect, entry: bool) -> Vec<AllowTok> {
    let mut out = Vec::new();
    for t in tokenize_in(sql, dialect).into_iter().filter(|t| !t.kind.is_trivia()) {
        let text = t.text(sql);
        match t.kind {
            _ if entry && text == "?" => out.push(AllowTok::Value),
            TokenKind::Operator if entry && text.len() > 1 && text.ends_with('?') => {
                out.push(AllowTok::Text(TokenKind::Operator, text[..text.len() - 1].to_string()));
                out.push(AllowTok::Value);
            }
            TokenKind::Keyword | TokenKind::Ident => out.push(AllowTok::Text(TokenKind::Ident, text.to_ascii_uppercase())),
            k => out.push(AllowTok::Text(k, text.to_string())),
        }
    }
    while matches!(out.last(), Some(AllowTok::Text(TokenKind::Punct, p)) if p == ";") {
        out.pop();
    }
    out
}

/// A `?` of the entry stands for exactly one value (a string, a number or a `$n` parameter),
/// never for a name, an expression or more SQL. Every text the server may run for the statement
/// (MySQL's `/*! … */` opened, or skipped where a version decides) is the entry.
pub fn allowlist_match(entry: &str, stmt: &str, dialect: Dialect) -> bool {
    let e = allow_tokens(&crate::sql::expand_exec_comments(entry, dialect), dialect, true);
    !e.is_empty() && crate::sql::exec_readings(stmt, dialect).iter().all(|text| takes(&e, &allow_tokens(text, dialect, false)))
}

fn takes(e: &[AllowTok], s: &[AllowTok]) -> bool {
    let mut j = 0;
    for t in e {
        match t {
            AllowTok::Value => {
                if matches!(s.get(j), Some(AllowTok::Text(TokenKind::Operator, op)) if op == "-" || op == "+")
                    && matches!(s.get(j + 1), Some(AllowTok::Text(TokenKind::Number, _)))
                {
                    j += 1;
                }
                if !matches!(s.get(j), Some(AllowTok::Text(k, _)) if is_value(*k)) {
                    return false;
                }
            }
            t => {
                if s.get(j) != Some(t) {
                    return false;
                }
            }
        }
        j += 1;
    }
    j == s.len()
}

pub fn allowlist_entry(text: &str, dialect: Dialect) -> Result<String, String> {
    let pieces = agent_pieces(text, dialect);
    let stmt = match pieces.as_slice() {
        [] => return Err("type a statement".into()),
        [one] => one.trim().trim_end_matches(';').trim_end().to_string(),
        _ => return Err("one statement per entry".into()),
    };
    if allow_tokens(&stmt, dialect, true).is_empty() {
        return Err("type a statement".into());
    }
    let f = classify_in(&stmt, dialect);
    if f.tx.is_some() {
        return Err(format!("{} is transaction control: every agent call is its own transaction", f.verb));
    }
    // A mark where DuckDB reads a file (`FROM ?`) would take any path.
    if dialect == Dialect::DuckDb && file_access(&stmt.replace('?', "'x'"), dialect).is_some() {
        return Err("a value cannot stand where DuckDB reads a file".into());
    }
    Ok(stmt)
}

pub fn allowlisted(sql: &str, dialect: Dialect, allowed: &[String]) -> (Vec<String>, Vec<String>) {
    let mut used: Vec<usize> = Vec::new();
    let mut rest = Vec::new();
    for p in agent_pieces(sql, dialect) {
        match allowed.iter().position(|e| allowlist_match(e, &p, dialect)) {
            Some(i) if !used.contains(&i) => used.push(i),
            Some(_) => {}
            None => rest.push(p),
        }
    }
    used.sort_unstable();
    (used.into_iter().map(|i| allowed[i].clone()).collect(), rest)
}

pub fn allowlist_candidates(sql: &str, dialect: Dialect) -> Vec<String> {
    agent_pieces(sql, dialect)
        .into_iter()
        .filter(|p| agent_write_class(p, dialect).is_write())
        .filter_map(|p| allowlist_entry(&p, dialect).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn commands_on_the_server_say_so() {
        let reasons = |sql: &str, d: Dialect| agent_write_class(sql, d).reasons().join(", ");
        assert!(reasons("COPY t TO PROGRAM 'rm -rf /'", Dialect::Postgres).contains("runs a command"), "{}", reasons("COPY t TO PROGRAM 'x'", Dialect::Postgres));
        assert!(reasons("EXEC xp_cmdshell 'whoami'", Dialect::TSql).contains("runs a command"));
        assert!(reasons("EXEC master..[xp_cmdshell] 'whoami'", Dialect::TSql).contains("runs a command"));
        assert!(!reasons("COPY t TO STDOUT", Dialect::Postgres).contains("runs a command"));
    }

    #[test]
    fn sqlite_and_duckdb_classes() {
        let lite = Dialect::Sqlite;
        let eff = |sql: &str, d: Dialect| classify_in(sql, d).effect;
        assert_eq!(eff("PRAGMA table_info(t)", lite), Effect::Read);
        assert_eq!(eff("PRAGMA main.index_list('t')", lite), Effect::Read);
        assert_eq!(eff("PRAGMA user_version", lite), Effect::Read);
        assert_eq!(eff("PRAGMA user_version = 3", lite), Effect::Write);
        assert_eq!(eff("PRAGMA writable_schema = ON", lite), Effect::Write);
        assert_eq!(eff("PRAGMA foreign_keys(0)", lite), Effect::Write);
        assert_eq!(eff("PRAGMA optimize", lite), Effect::Write);
        assert_eq!(eff("ATTACH '/tmp/x.db' AS x", lite), Effect::Ddl);
        assert_eq!(eff("VACUUM INTO '/tmp/y.db'", lite), Effect::Ddl);
        assert_eq!(eff("VACUUM", lite), Effect::Write);
        assert_eq!(eff("REPLACE INTO t VALUES (1)", lite), Effect::Write);
        assert_eq!(classify_in("BEGIN IMMEDIATE", lite).tx, Some(TxCmd::Begin));
        for sql in ["PRAGMA writable_schema = 1", "ATTACH 'x' AS y", "VACUUM INTO 'z'", "SELECT load_extension('e')", "SELECT writefile('f', 'x')"] {
            assert!(agent_read_refusal_in(sql, lite).is_some(), "{sql}");
            assert!(read_only_refusal_in(sql, lite).is_some() || sql.starts_with("SELECT"), "{sql}");
        }
        assert!(agent_read_refusal_in("PRAGMA table_info(t)", lite).is_none());
        assert!(agent_read_refusal_in("SELECT glob('a*', name) FROM t", lite).is_none(), "SQLite's glob() is a string function");
        let duck = Dialect::DuckDb;
        assert_eq!(eff("COPY t TO '/tmp/x.csv'", duck), Effect::Ddl);
        assert_eq!(eff("SET threads = 2", duck), Effect::Ddl);
        assert_eq!(eff("INSTALL httpfs", duck), Effect::Ddl);
        assert_eq!(eff("PRAGMA enable_profiling", duck), Effect::Write);
        assert_eq!(eff("PRAGMA table_info('t')", duck), Effect::Read);
        for sql in ["DESCRIBE t", "SUMMARIZE t", "FROM t", "SHOW TABLES", "SELECT [1, 2] FROM t"] {
            assert!(agent_read_refusal_in(sql, duck).is_none(), "{sql}: {:?}", agent_read_refusal_in(sql, duck));
        }
        for sql in [
            "SELECT * FROM read_csv('/etc/passwd')",
            "SELECT * FROM '~/.ssh/id_ed25519'",
            "SELECT * FROM t JOIN 'x.parquet' USING (id)",
            "SELECT * FROM t, \"y.csv\"",
            "SELECT * FROM query('DELETE FROM t')",
            "SELECT \"read_text\"('/etc/hosts')",
        ] {
            assert!(agent_read_refusal_in(sql, duck).is_some(), "{sql}");
            assert!(file_access(sql, duck).is_some(), "{sql}");
            assert!(agent_write_class(sql, duck).is_destructive(), "{sql}");
        }
        for sql in [
            "COPY t TO 'x'",
            "ATTACH 'x.db'",
            "INSTALL httpfs",
            "EXPORT DATABASE 'd'",
            // SQL inside a string, another database file, the shared instance's settings and logs.
            "SELECT * FROM json_execute_serialized_sql(json_serialize_sql('SELECT pw FROM users'))",
            "SELECT * FROM read_duckdb('/tmp/other.duckdb')",
            "SELECT * FROM duckdb_logs",
            "SELECT * FROM duckdb_logs()",
            "CALL enable_logging()",
            "SET log_query_path = '/tmp/log.txt'",
            "SET memory_limit = '1MB'",
            "RESET threads",
            "PRAGMA enable_profiling = 'query_tree'",
            "PRAGMA enable_profiling",
        ] {
            assert!(file_access(sql, duck).is_some(), "{sql}");
        }
        assert!(file_access("PRAGMA table_info('t')", duck).is_none());
        assert!(file_access("SELECT * FROM duckdb_tables()", duck).is_none(), "the catalog is for agent_sql to narrow");
        assert!(file_access("SELECT * FROM t WHERE name = 'x.csv'", duck).is_none());
        assert!(allowlist_entry("SELECT * FROM ?", duck).is_err());
        assert!(file_access("SELECT * FROM read_csv('x')", Dialect::Postgres).is_none());
    }

    use super::*;

    #[test]
    fn allowlist_matching() {
        use Dialect::{MySql as My, Postgres as Pg, TSql as Ms};
        let e = "DELETE FROM sessions WHERE expires_at < ?";
        for (d, ok) in [
            (Pg, "delete from sessions where expires_at < '2026-01-01'"),
            (Pg, "DELETE  FROM sessions\n WHERE expires_at < 42; "),
            (Pg, "DELETE FROM sessions /* old */ WHERE expires_at < $1 -- bound"),
            (Pg, "DELETE FROM sessions WHERE expires_at < -5"),
            (Pg, "DELETE FROM sessions WHERE expires_at < $$x$$"),
            (My, "delete from sessions where expires_at < ?"),
            (Ms, "DELETE FROM sessions WHERE expires_at < N'2026'"),
        ] {
            assert!(allowlist_match(e, ok, d), "{d:?} {ok}");
        }
        for bad in [
            "DELETE FROM sessions",
            "DELETE FROM sessions WHERE expires_at < now()",
            "DELETE FROM sessions WHERE expires_at < 1 OR true",
            "DELETE FROM sessions WHERE expires_at < 1; DROP TABLE users",
            "DELETE FROM sessions WHERE expires_at < expires_at",
            "DELETE FROM \"sessions\" WHERE expires_at < 1",
            "DELETE FROM sessions2 WHERE expires_at < 1",
        ] {
            assert!(!allowlist_match(e, bad, Pg), "{bad}");
        }
        assert!(allowlist_match("UPDATE t SET a=? WHERE id=?", "update t set a = 'x' where id = 7", Pg));
        assert!(allowlist_match("TRUNCATE audit_tmp;", "truncate AUDIT_TMP", Pg));
        assert!(!allowlist_match("UPDATE t SET s = 'Paid' WHERE id = 1", "UPDATE t SET s = 'paid' WHERE id = 1", Pg));
        assert!(!allowlist_match("DELETE FROM \"Users\" WHERE id = 1", "DELETE FROM \"users\" WHERE id = 1", Pg));
        assert!(!allowlist_match("", "SELECT 1", Pg) && !allowlist_match("  ;", "", Pg));
        assert_eq!(allowlist_entry("  TRUNCATE audit_tmp ; ", Pg).as_deref(), Ok("TRUNCATE audit_tmp"));
        assert!(allowlist_entry("DELETE FROM a; DELETE FROM b", Pg).unwrap_err().contains("one statement"));
        assert!(allowlist_entry("COMMIT", Pg).unwrap_err().contains("transaction control"));
        assert!(allowlist_entry(" -- nothing", Pg).is_err());
        let allowed = vec!["TRUNCATE audit_tmp".to_string(), e.to_string()];
        let (used, rest) = allowlisted("delete from sessions where expires_at < 9; SELECT 1; INSERT INTO t VALUES (1)", Pg, &allowed);
        assert_eq!(used, vec![e.to_string()]);
        assert_eq!(rest.iter().map(|s| s.trim()).collect::<Vec<_>>(), ["SELECT 1", "INSERT INTO t VALUES (1)"]);
        assert_eq!(allowlist_candidates("SELECT 1; DROP TABLE t; UPDATE t SET a = 1;", Pg), ["DROP TABLE t", "UPDATE t SET a = 1"]);
    }

    #[test]
    fn agent_reads() {
        for ok in [
            "SELECT * FROM t WHERE a = 'DELETE FROM x'",
            "with a as (select 1) select * from a",
            "TABLE t",
            "VALUES (1), (2)",
            "SHOW work_mem",
            "EXPLAIN SELECT 1",
            "EXPLAIN ANALYZE SELECT 1",
            "EXPLAIN INSERT INTO t VALUES (1)",
            "EXPLAIN (ANALYZE OFF, COSTS) DELETE FROM t",
            "EXPLAIN (ANALYZE FALSE) DELETE FROM t",
            "EXPLAIN (COSTS OFF) DELETE FROM t",
            "SELECT lar_history('t')",
            "SELECT pg_sleep(0.1)",
            "SELECT format('for update')",
            "SELECT current_setting('standard_conforming_strings')",
            "SELECT * FROM t FOR_x",
        ] {
            assert_eq!(agent_read_refusal(ok), None, "{ok}");
        }
        for (bad, why) in [
            ("", "empty"),
            ("INSERT INTO t VALUES (1)", "not a read"),
            ("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x", "not a read"),
            ("EXPLAIN ANALYZE DELETE FROM t", "not a read"),
            ("EXPLAIN (ANALYZE, COSTS OFF) DELETE FROM t", "not a read"),
            ("EXPLAIN (ANALYZE TRUE, TIMING FALSE) DELETE FROM t", "not a read"),
            ("EXPLAIN (VERBOSE OFF, ANALYZE 1) DELETE FROM t", "not a read"),
            ("SELECT 1 INTO t2", "not a read"),
            ("SET search_path = a", "not a read"),
            ("COPY t TO '/tmp/x'", "not a read"),
            ("CALL p()", "not a read"),
            ("DO $$ BEGIN END $$", "not a read"),
            ("VACUUM t", "not a read"),
            ("BEGIN", "transaction control"),
            ("COMMIT", "transaction control"),
            ("SELECT * FROM t FOR UPDATE", "row locks"),
            ("select * from t for no key update skip locked", "row locks"),
            ("SELECT pg_terminate_backend(123)", "effects"),
            ("SELECT pg_catalog.pg_cancel_backend(1)", "effects"),
            ("SELECT nextval('s')", "effects"),
            ("SELECT \"pg_reload_conf\"()", "effects"),
            ("SELECT set_config('role', 'postgres', false)", "effects"),
            ("SELECT lar_backup('/tmp/b')", "administrative"),
            ("SELECT lar_create_user('x', 'y')", "administrative"),
        ] {
            let r = agent_read_refusal(bad).unwrap_or_else(|| panic!("{bad} must be refused"));
            assert!(r.contains(why), "{bad}: {r}");
        }
    }

    fn nw(sql: &str) -> bool {
        classify(sql).no_where
    }

    #[test]
    fn no_where_detection() {
        assert!(nw("DELETE FROM orders"));
        assert!(nw("delete from orders;"));
        assert!(!nw("DELETE FROM orders WHERE id = 1"));
        assert!(nw("UPDATE orders SET status = 'x'"));
        assert!(!nw("update orders set status = 'x' where id = 3"));
        assert!(nw("DELETE FROM orders -- WHERE id = 1"));
        assert!(nw("DELETE FROM orders /* WHERE id = 1 */"));
        assert!(nw("UPDATE t SET note = 'see WHERE clause'"));
        assert!(nw("UPDATE t SET note = $$ WHERE $$"));
        assert!(!nw("-- clean up\nDELETE FROM t\n WHERE done"));
        assert!(nw("UPDATE t SET x = (SELECT max(y) FROM u WHERE u.id = 1)"));
        assert!(!nw("UPDATE t SET x = (SELECT 1) WHERE id IN (SELECT id FROM u WHERE z)"));
        assert!(nw("DELETE FROM t USING u"));
        assert!(!nw("DELETE FROM t USING u WHERE t.id = u.id"));
        assert!(nw("UPDATE t SET x = u.x FROM u"));
        assert!(!nw("UPDATE t SET x = u.x FROM u WHERE t.id = u.id"));
        assert!(!nw("DELETE FROM t WHERE CURRENT OF c"));
        assert!(nw("DELETE FROM t RETURNING *"));
        assert!(!nw("INSERT INTO t VALUES (1) ON CONFLICT (id) DO UPDATE SET x = 1"));
        assert!(!nw("SELECT * FROM t"));
    }

    #[test]
    fn no_where_through_with() {
        assert!(nw("WITH old AS (SELECT id FROM t WHERE x) DELETE FROM u"));
        assert!(!nw("WITH old AS (SELECT id FROM t) DELETE FROM u WHERE id IN (SELECT id FROM old)"));
        let f = classify("WITH gone AS (DELETE FROM t RETURNING *) SELECT count(*) FROM gone");
        assert!(f.no_where);
        assert_eq!((f.verb.as_str(), f.target.as_deref(), f.effect), ("DELETE", Some("t"), Effect::Write));
        assert!(!nw("WITH gone AS (DELETE FROM t WHERE old RETURNING *) SELECT count(*) FROM gone"));
        assert!(nw("WITH RECURSIVE a (n) AS (SELECT 1), b AS MATERIALIZED (UPDATE t SET x = 1 RETURNING 1) SELECT * FROM a"));
        let f = classify("WITH a AS (SELECT 1) SELECT * FROM a");
        assert_eq!((f.effect, f.no_where), (Effect::Read, false));
    }

    #[test]
    fn commands_and_targets() {
        let f = classify("UPDATE ONLY public.\"Orders\" p SET x = 1 WHERE true");
        assert_eq!((f.verb.as_str(), f.target.as_deref(), f.effect), ("UPDATE", Some("public.\"Orders\""), Effect::Write));
        assert_eq!(classify("DELETE FROM only s.t").target.as_deref(), Some("s.t"));
        assert_eq!(classify("TRUNCATE TABLE a, s.b CASCADE").target.as_deref(), Some("a, s.b"));
        let f = classify("DROP TABLE IF EXISTS a, b CASCADE");
        assert_eq!((f.verb.as_str(), f.target.as_deref(), f.effect), ("DROP TABLE", Some("a, b"), Effect::Ddl));
        assert_eq!(classify("drop materialized view mv").verb, "DROP MATERIALIZED VIEW");
        let f = classify("DROP INDEX CONCURRENTLY IF EXISTS ix");
        assert_eq!((f.verb.as_str(), f.target.as_deref(), f.outside_tx), ("DROP INDEX", Some("ix"), true));
        assert_eq!(classify("CREATE UNIQUE INDEX CONCURRENTLY i ON t (x)").outside_tx, true);
        assert_eq!(classify("CREATE TEMP TABLE IF NOT EXISTS x (a int)").verb, "CREATE TABLE");
        assert_eq!(classify("INSERT INTO s.t (a) VALUES (1)").target.as_deref(), Some("s.t"));
        assert_eq!(classify("SELECT a INTO TEMP fresh FROM t").effect, Effect::Ddl);
        assert_eq!(classify("SELECT a FROM t WHERE b IN (SELECT 1 INTO x)").effect, Effect::Read, "INTO only counts at the top");
        assert_eq!(classify("COPY t FROM STDIN").effect, Effect::Write);
        assert_eq!(classify("COPY t TO STDOUT").effect, Effect::Read);
        assert_eq!(classify("COPY (SELECT 1) TO STDOUT").effect, Effect::Read);
        assert_eq!(classify("explain select 1").effect, Effect::Read);
        let f = classify("EXPLAIN (ANALYZE, BUFFERS) DELETE FROM t");
        assert_eq!((f.verb.as_str(), f.no_where), ("DELETE", true), "EXPLAIN ANALYZE runs the statement");
        assert!(!classify("EXPLAIN DELETE FROM t").no_where);
        assert_eq!(classify("VACUUM ANALYZE t").outside_tx, true);
        assert_eq!(classify("SET search_path = x").effect, Effect::Other);
        assert_eq!(classify("  ").verb, "");
        assert_eq!(classify("CALL refresh()").effect, Effect::Write);
    }

    #[test]
    fn alter_drop() {
        assert_eq!(classify("ALTER TABLE t DROP COLUMN email").alter_drop.as_deref(), Some("DROP COLUMN email"));
        assert_eq!(classify("alter table if exists s.t drop constraint if exists c_fk").alter_drop.as_deref(), Some("DROP CONSTRAINT c_fk"));
        assert_eq!(classify("ALTER TABLE t DROP x").alter_drop.as_deref(), Some("DROP x"));
        assert_eq!(classify("ALTER TABLE t ADD COLUMN y int, DROP COLUMN x").alter_drop.as_deref(), Some("DROP COLUMN x"));
        assert_eq!(classify("ALTER TABLE t ALTER COLUMN x DROP DEFAULT").alter_drop, None);
        assert_eq!(classify("ALTER TABLE t ALTER x DROP NOT NULL").alter_drop, None);
        assert_eq!(classify("ALTER TABLE t ADD COLUMN drop_me int").alter_drop, None);
        let f = classify("ALTER TABLE ONLY public.t RENAME TO u");
        assert_eq!((f.verb.as_str(), f.target.as_deref()), ("ALTER TABLE", Some("public.t")));
        assert_eq!(classify("ALTER SYSTEM SET work_mem = '64MB'").outside_tx, true);
    }

    #[test]
    fn transaction_control() {
        assert_eq!(classify("begin").tx, Some(TxCmd::Begin));
        assert_eq!(classify("START TRANSACTION READ ONLY").tx, Some(TxCmd::Begin));
        assert_eq!(classify("END").tx, Some(TxCmd::Commit));
        assert_eq!(classify("abort").tx, Some(TxCmd::Rollback));
        assert_eq!(classify("ROLLBACK TO SAVEPOINT s1").tx, Some(TxCmd::RollbackTo));
        assert_eq!(classify("rollback to s1").tx, Some(TxCmd::RollbackTo));
        assert_eq!(classify("SAVEPOINT s1").tx, Some(TxCmd::Savepoint));
        assert_eq!(classify("RELEASE SAVEPOINT s1").tx, Some(TxCmd::Release));
        let f = classify("COMMIT PREPARED 'x'");
        assert_eq!((f.tx, f.outside_tx), (Some(TxCmd::Prepared), true));
        let f = classify("PREPARE TRANSACTION 'x'");
        assert_eq!((f.tx, f.outside_tx), (Some(TxCmd::Prepared), false));
        assert!(classify("PREPARE q AS SELECT 1").tx.is_none());
        assert!(classify("ROLLBACK").ends_failed_tx() && classify("rollback to s").ends_failed_tx());
        assert!(!classify("SELECT 1").ends_failed_tx());
        assert!(!classify("SELECT 1").outside_tx);
        assert!(classify("VACUUM").outside_tx);
        assert!(classify("CREATE DATABASE x").outside_tx);
        assert!(classify("DISCARD ALL").outside_tx);
        assert!(!classify("DISCARD PLANS").outside_tx);
    }

    #[test]
    fn rules_by_environment() {
        let ids = |sql: &str, env| review(&[sql], env).into_iter().map(|f| f.rule).collect::<Vec<_>>();
        use Environment::*;
        for env in [None, Dev, Staging] {
            assert_eq!(ids("DELETE FROM t", env), ["no-where"]);
            assert_eq!(ids("TRUNCATE t", env), ["truncate"]);
            assert_eq!(ids("DROP TABLE t", env), ["drop"]);
            assert_eq!(ids("ALTER TABLE t DROP COLUMN c", env), ["alter-drop"]);
            assert!(ids("DELETE FROM t WHERE id = 1", env).is_empty());
            assert!(ids("INSERT INTO t VALUES (1)", env).is_empty());
            assert!(ids("CREATE INDEX i ON t (x)", env).is_empty());
            assert!(ids("SELECT 1", env).is_empty());
        }
        assert_eq!(ids("DELETE FROM t", Production), ["no-where"], "the specific rule names it");
        assert_eq!(ids("DELETE FROM t WHERE id = 1", Production), ["write"]);
        assert_eq!(ids("INSERT INTO t VALUES (1)", Production), ["write"]);
        assert_eq!(ids("CREATE INDEX i ON t (x)", Production), ["ddl"]);
        assert_eq!(ids("GRANT SELECT ON t TO u", Production), ["ddl"]);
        assert!(ids("SELECT 1", Production).is_empty());
        assert!(ids("SET work_mem = '1GB'", Production).is_empty());
        assert!(ids("BEGIN", Production).is_empty());
        let f = review(&["SELECT 1", "DELETE FROM s.orders", "ALTER TABLE t DROP COLUMN email"], None);
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].index, f[0].label.as_str(), f[0].target.as_deref()), (1, "DELETE without WHERE", Some("s.orders")));
        assert_eq!((f[1].index, f[1].label.as_str()), (2, "ALTER TABLE · DROP COLUMN email"));
    }

    #[test]
    fn read_only() {
        assert!(read_only_refusal("SELECT * FROM t").is_none());
        assert!(read_only_refusal("SHOW work_mem").is_none());
        assert!(read_only_refusal("BEGIN").is_none());
        assert!(read_only_refusal("SET search_path = a").is_none());
        assert_eq!(read_only_refusal("insert into t values (1)").as_deref(), Some("read-only connection: INSERT is not run"));
        assert!(read_only_refusal("WITH x AS (UPDATE t SET a = 1 WHERE b RETURNING *) SELECT * FROM x").is_some());
        assert!(read_only_refusal("CREATE TABLE x (a int)").is_some());
        assert!(read_only_refusal("TRUNCATE t").is_some());
        assert!(read_only_refusal("SELECT 1 INTO t2").is_some());
        assert!(read_only_refusal("EXPLAIN ANALYZE INSERT INTO t VALUES (1)").is_some());
        assert!(read_only_refusal("EXPLAIN INSERT INTO t VALUES (1)").is_none());
    }

    #[test]
    fn tsql() {
        let d = Dialect::TSql;
        let c = |sql: &str| classify_in(sql, d);
        let f = c("EXEC dbo.cleanup @days = 30");
        assert_eq!((f.verb.as_str(), f.effect, f.target.as_deref()), ("EXEC", Effect::Write, Some("dbo.cleanup")));
        assert_eq!(c("EXECUTE @rc = [dbo].[p]").target.as_deref(), Some("[dbo].[p]"));
        assert_eq!(c("EXEC sp_helptext 'dbo.v'").effect, Effect::Read, "a system procedure that only reports");
        assert_eq!(c("EXEC ('DELETE FROM t')").effect, Effect::Write);
        assert_eq!(c("SELECT * INTO #t FROM orders").effect, Effect::Ddl);
        assert_eq!(c("MERGE TOP (10) INTO dbo.t AS d USING s ON d.id = s.id WHEN MATCHED THEN DELETE;").target.as_deref(), Some("dbo.t"));
        let f = c("DELETE TOP (5) FROM [dbo].[log]");
        assert_eq!((f.target.as_deref(), f.no_where), (Some("[dbo].[log]"), true));
        assert_eq!(c("BULK INSERT dbo.t FROM '/x.csv'").effect, Effect::Write);
        assert_eq!(c("TRUNCATE TABLE dbo.t").verb, "TRUNCATE");
        assert_eq!(c("BEGIN TRAN").tx, Some(TxCmd::Begin));
        assert_eq!(c("BEGIN TRY SELECT 1 END TRY BEGIN CATCH SELECT 2 END CATCH").tx, None);
        assert_eq!(c("SAVE TRANSACTION sp").tx, Some(TxCmd::Savepoint));
        assert_eq!(c("ROLLBACK TRANSACTION sp").tx, Some(TxCmd::RollbackTo));
        assert_eq!(c("ROLLBACK TRAN").tx, Some(TxCmd::Rollback));
        assert_eq!(c("UPDATE STATISTICS dbo.t").effect, Effect::Other);
        let f = c("DECLARE @n int = 3\nSELECT @n\nUPDATE dbo.t SET a = @n\nSELECT 1");
        assert_eq!((f.verb.as_str(), f.no_where, f.effect), ("UPDATE", true, Effect::Write));
        let f = c("SET NOCOUNT ON\nIF @x > 0 DELETE FROM t WHERE id = 1 ELSE DROP TABLE t");
        assert_eq!((f.verb.as_str(), f.target.as_deref()), ("DROP TABLE", Some("t")));
        let f = c("BEGIN TRANSACTION\nUPDATE t SET a = 1 WHERE id = 2");
        assert_eq!((f.effect, f.tx), (Effect::Write, Some(TxCmd::Begin)));
        assert_eq!(c("ALTER TABLE t DROP COLUMN email").alter_drop.as_deref(), Some("DROP COLUMN email"));
        assert_eq!(c("ALTER TABLE t ALTER COLUMN a int NOT NULL").verb, "ALTER TABLE");
        assert_eq!(c("ALTER TABLE t ADD CONSTRAINT f FOREIGN KEY (a) REFERENCES u (id) ON DELETE CASCADE").effect, Effect::Ddl);
        assert!(!c("ALTER TABLE t ADD CONSTRAINT f FOREIGN KEY (a) REFERENCES u (id) ON DELETE CASCADE").no_where);
        assert_eq!(c("INSERT INTO t (a) SELECT a FROM u UNION ALL SELECT 2").verb, "INSERT");
        assert_eq!(c("INSERT INTO t EXEC dbo.p").verb, "INSERT");
        assert_eq!(c("MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a WHEN NOT MATCHED THEN INSERT (a) VALUES (s.a);").verb, "MERGE");
        assert!(!c("MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a;").no_where);
        assert_eq!(c("DECLARE k CURSOR FOR SELECT a FROM t FOR UPDATE").effect, Effect::Read);
        assert_eq!(c("GRANT SELECT, UPDATE ON t TO u").verb, "GRANT");
        assert_eq!(c("WITH c AS (SELECT 1 AS a) DELETE FROM t WHERE a IN (SELECT a FROM c)").verb, "DELETE");
        assert_eq!(c("INSERT INTO t VALUES (1) SELECT 1 DELETE FROM t").no_where, true);
        let f = c("CREATE PROCEDURE p AS BEGIN DELETE FROM t; DROP TABLE u; END");
        assert_eq!((f.verb.as_str(), f.no_where), ("CREATE PROCEDURE", false));
        assert_eq!(c("CREATE OR ALTER VIEW v AS SELECT * FROM t").verb, "CREATE VIEW");
        assert_eq!(c("SELECT N'DROP TABLE t' -- DELETE FROM x\n/* UPDATE y */").effect, Effect::Read);
        assert!(read_only_refusal_in("EXEC dbo.p", d).is_some());
        assert!(read_only_refusal_in("SELECT 1 UPDATE t SET a = 1 WHERE b = 2", d).is_some());
        assert!(read_only_refusal_in("SELECT * FROM t WITH (NOLOCK)", d).is_none());
        assert_eq!(agent_read_refusal_in("SELECT TOP (5) * FROM [dbo].[t] WITH (NOLOCK) ORDER BY 1", d), None);
        for (bad, why) in [
            ("SELECT * FROM t WITH (UPDLOCK)", "lock hints"),
            ("SELECT NEXT VALUE FOR dbo.seq", "advances"),
            ("SELECT * FROM OPENROWSET('SQLNCLI', 's', 'q')", "effects"),
            ("EXEC sp_help", "not a read"),
            ("SELECT 1 DELETE FROM t", "not a read"),
            ("SELECT * INTO #x FROM t", "not a read"),
            ("BEGIN TRAN", "transaction control"),
        ] {
            let r = agent_read_refusal_in(bad, d).unwrap_or_else(|| panic!("{bad} must be refused"));
            assert!(r.contains(why), "{bad}: {r}");
        }
        let f = review_in(&["SELECT 1", "UPDATE t SET a = 1\nGO", "EXEC dbo.p"], Environment::Production, d);
        assert_eq!(f.iter().map(|f| f.rule).collect::<Vec<_>>(), ["no-where", "write"]);
    }

    #[test]
    fn agent_write_corpus() {
        #[derive(Debug)]
        enum W {
            R,
            S,
            D(&'static str),
        }
        use Dialect::{MySql as My, Postgres as Pg, TSql as Ms};
        use W::*;
        let corpus: &[(Dialect, &str, W)] = &[
            (Pg, "SELECT * FROM t WHERE a = 'DELETE FROM x'", R),
            (Pg, "with a as (select 1) select * from a", R),
            (Pg, "EXPLAIN DELETE FROM t", R),
            (Pg, "SELECT lar_history('t')", R),
            (Pg, "-- only a comment", R),
            (My, "SELECT SLEEP(1)", R),
            (My, "DESCRIBE t", R),
            (Ms, "SELECT TOP (5) * FROM [dbo].[t] WITH (NOLOCK)", R),
            (Ms, "DECLARE @n int = 3\nSELECT @n", R),
            (Pg, "INSERT INTO t (a) VALUES (1)", S),
            (Pg, "INSERT INTO t SELECT * FROM u", S),
            (Pg, "INSERT INTO t VALUES (1) ON CONFLICT (id) DO UPDATE SET x = 1", S),
            (Pg, "INSERT INTO t (id) VALUES (nextval('s'))", S),
            (Pg, "UPDATE t SET a = 1 WHERE id = 2", S),
            (Pg, "update t set a = 1 where (id = 2)", S),
            (Pg, "UPDATE t SET a = 1 WHERE id = $1 RETURNING *", S),
            (Pg, "DELETE FROM t WHERE true AND id = 3", S),
            (Pg, "DELETE FROM t WHERE id = 3 -- OR 1=1", S),
            (Pg, "DELETE FROM t WHERE id = 3 /* OR 1=1 */", S),
            (Pg, "DELETE FROM t WHERE created < DATE '2020-01-01'", S),
            (Pg, "DELETE FROM t WHERE id BETWEEN 1 AND 5", S),
            (Pg, "DELETE FROM t WHERE CURRENT OF c", S),
            (Pg, "WITH x AS (UPDATE t SET a = 1 WHERE id = 2 RETURNING *) SELECT * FROM x", S),
            (Pg, "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE", S),
            (Pg, "SELECT * FROM t WHERE id = 1 FOR UPDATE", S),
            (Pg, "SELECT nextval('s')", S),
            (Pg, "INSERT INTO a VALUES (1); UPDATE b SET x = 2 WHERE id = 3", S),
            (My, "UPDATE t1 JOIN t2 ON t1.id = t2.id SET t1.a = 1 WHERE t2.b = 2", S),
            (My, "DELETE t1 FROM t1 JOIN t2 ON t1.id = t2.id WHERE t2.gone", S),
            (My, "REPLACE INTO t VALUES (1)", S),
            (My, "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 2", S),
            (My, "SET @x = 3; UPDATE t SET a = @x WHERE id = 1", S),
            (My, "UPDATE t SET date = NOW() WHERE user = 'bob'", S),
            (Ms, "SET NOCOUNT ON; INSERT INTO t VALUES (1)", S),
            (Ms, "DECLARE @n int = 3\nUPDATE t SET a = @n WHERE id = 1", S),
            (Ms, "MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a;", S),
            (Ms, "UPDATE TOP (10) dbo.t SET a = 1 WHERE b = 2", S),
            (Ms, "SELECT * FROM t WITH (UPDLOCK) WHERE id = 1", S),
            (Ms, "DELETE FROM t OUTPUT deleted.* WHERE id = 7", S),
            (Pg, "DELETE FROM t", D("DELETE without WHERE")),
            (Pg, "UPDATE t SET x = 'WHERE id = 1'", D("UPDATE without WHERE")),
            (Pg, "DELETE FROM t WHERE 1=1", D("always true")),
            (Pg, "DELETE FROM t WHERE 1 = 1 RETURNING *", D("always true")),
            (Pg, "UPDATE t SET a = 1 WHERE true", D("always true")),
            (Pg, "UPDATE t SET a = 1 WHERE id = id", D("always true")),
            (Pg, "DELETE FROM t WHERE t.id = t.id", D("always true")),
            (Pg, "DELETE FROM t WHERE \"id\" = id", D("always true")),
            (Pg, "DELETE FROM t WHERE id = 5 OR 1=1", D("always true")),
            (Pg, "DELETE FROM t WHERE (id = 5) OR ('a' = 'a')", D("always true")),
            (Pg, "DELETE FROM t WHERE NULL IS NULL", D("always true")),
            (Pg, "DELETE FROM t WHERE NOT false", D("always true")),
            (Pg, "DELETE FROM t WHERE now() > '2000-01-01'", D("always true")),
            (Pg, "DELETE FROM t WHERE $1 = $1", D("always true")),
            (Pg, "DELETE FROM t WHERE 1 BETWEEN 0 AND 2", D("always true")),
            (Pg, "WITH d AS (DELETE FROM t RETURNING *) SELECT count(*) FROM d", D("DELETE without WHERE")),
            (Pg, "WITH x AS (UPDATE t SET a = 1 WHERE 1 = 1 RETURNING *) SELECT * FROM x", D("always true")),
            (Pg, "EXPLAIN ANALYZE DELETE FROM t", D("DELETE without WHERE")),
            (Pg, "MERGE INTO t USING s ON true WHEN MATCHED THEN DELETE", D("MERGE")),
            (My, "DELETE FROM t WHERE 1", D("always true")),
            (My, "DELETE FROM t ORDER BY id LIMIT 10", D("DELETE without WHERE")),
            (My, "UPDATE t1 JOIN t2 ON t1.id = t2.id SET t1.a = 1", D("UPDATE without WHERE")),
            (My, "DELETE t1, t2 FROM t1 INNER JOIN t2 ON t1.id = t2.id", D("DELETE without WHERE")),
            (Ms, "DELETE TOP (10) FROM t", D("DELETE without WHERE")),
            (Ms, "UPDATE t SET a = 1 WHERE @x = 1", D("always true")),
            (Ms, "DELETE FROM t OUTPUT deleted.* WHERE 1=1", D("always true")),
            (Ms, "MERGE t USING s ON 1=1 WHEN MATCHED THEN DELETE;", D("MERGE")),
            (Ms, "MERGE t USING s ON t.id = s.id WHEN NOT MATCHED BY SOURCE THEN DELETE;", D("MERGE")),
            (Pg, "TRUNCATE t", D("TRUNCATE")),
            (Pg, "DROP TABLE t", D("DROP TABLE")),
            (Pg, "dRoP tAbLe t", D("DROP TABLE")),
            (Pg, "/* harmless */ DROP TABLE t", D("DROP TABLE")),
            (Pg, "ALTER TABLE t ADD COLUMN c int", D("ALTER TABLE")),
            (Pg, "CREATE TEMP TABLE x (a int)", D("CREATE TABLE")),
            (Pg, "CREATE INDEX CONCURRENTLY i ON t (x)", D("CREATE INDEX")),
            (Pg, "GRANT ALL ON t TO public", D("GRANT")),
            (Pg, "REVOKE SELECT ON t FROM u", D("REVOKE")),
            (Pg, "SELECT * INTO new_t FROM t", D("SELECT INTO")),
            (My, "RENAME TABLE a TO b", D("RENAME")),
            (My, "SELECT * FROM t INTO OUTFILE '/tmp/x'", D("SELECT INTO")),
            (Ms, "SELECT * INTO #x FROM t", D("SELECT INTO")),
            (Ms, "ALTER DATABASE x SET SINGLE_USER", D("ALTER DATABASE")),
            (Ms, "DENY SELECT ON t TO u", D("DENY")),
            (Pg, "DELETE FROM t WHERE id = 3; DROP TABLE t", D("DROP TABLE")),
            (Pg, "INSERT INTO a VALUES (1);DELETE FROM b", D("DELETE without WHERE")),
            (Pg, "DELETE/**/FROM t", D("DELETE without WHERE")),
            (Pg, "DEL/**/ETE FROM t WHERE id = 1", D("DEL")),
            (Ms, "INSERT INTO t VALUES (1)\nGO\nDROP TABLE t", D("DROP TABLE")),
            (Ms, "INSERT INTO t VALUES (1) DELETE FROM t", D("DELETE without WHERE")),
            (Ms, "IF 1 = 1 DELETE FROM t WHERE id = 1", D("IF")),
            (Ms, "WHILE 1 = 1 DELETE TOP (1) FROM t WHERE id > 0", D("WHILE")),
            (Ms, "BEGIN TRY DELETE FROM t WHERE id = 1 END TRY BEGIN CATCH END CATCH", D("BEGIN")),
            (Ms, "WAITFOR DELAY '00:00:10'", D("WAITFOR")),
            (Pg, "CALL p()", D("CALL")),
            (Pg, "DO $$ BEGIN DELETE FROM t; END $$", D("DO")),
            (Pg, "PREPARE q AS DELETE FROM t; EXECUTE q", D("PREPARE")),
            (My, "CALL purge()", D("CALL")),
            (Ms, "EXEC('DROP TABLE x')", D("EXEC")),
            (Ms, "EXEC sp_executesql N'DELETE FROM t'", D("EXEC")),
            (Ms, "sp_executesql N'DELETE FROM t'", D("SP_EXECUTESQL")),
            (Ms, "EXEC dbo.purge @days = 30", D("EXEC")),
            (Ms, "INSERT INTO t EXEC dbo.p", D("INSERT … EXEC")),
            (Pg, "SELECT pg_terminate_backend(pid) FROM pg_stat_activity", D("pg_terminate_backend()")),
            (Pg, "INSERT INTO t VALUES (pg_cancel_backend(1))", D("pg_cancel_backend()")),
            (Pg, "SELECT dblink_exec('host=x', 'DROP TABLE t')", D("dblink_exec()")),
            (Pg, "UPDATE t SET a = lo_import('/etc/passwd') WHERE id = 1", D("lo_import()")),
            (Pg, "SELECT setval('s', 1)", D("setval()")),
            (Pg, "SELECT pg_advisory_lock(1)", D("pg_advisory_lock()")),
            (Pg, "SELECT lar_backup('/tmp/b')", D("lar_backup()")),
            (My, "SELECT GET_LOCK('x', 10)", D("get_lock()")),
            (Ms, "SELECT * FROM OPENROWSET('SQLNCLI', 's', 'q')", D("openrowset()")),
            (Pg, "COPY t FROM '/etc/passwd'", D("COPY")),
            (Pg, "COPY t TO STDOUT", D("COPY")),
            (My, "LOAD DATA INFILE 'x' INTO TABLE t", D("LOAD DATA")),
            (Ms, "BULK INSERT dbo.t FROM '/x.csv'", D("BULK INSERT")),
            (Pg, "VACUUM t", D("VACUUM")),
            (Pg, "LOCK TABLE t", D("LOCK")),
            (Pg, "SET ROLE postgres", D("SET")),
            (Pg, "SET search_path = evil; INSERT INTO t VALUES (1)", D("SET")),
            (My, "SET @@global.read_only = 0", D("SET")),
            (Ms, "SET IDENTITY_INSERT t ON", D("SET")),
            (Pg, "EXPLAIN (ANALYZE, COSTS OFF) DELETE FROM t", D("DELETE without WHERE")),
            (Pg, "EXPLAIN (ANALYZE TRUE, TIMING FALSE) UPDATE t SET a = 1 WHERE id = 1", S),
            (My, "SET @a = 1, sql_mode = 'NO_BACKSLASH_ESCAPES'; INSERT INTO t VALUES (1)", D("SET")),
            (My, "SET @a = 1, @@session.sql_mode = 'ANSI'", D("SET")),
            (My, "SET @a = 1, @b = (SELECT MAX(x) FROM t); INSERT INTO t VALUES (@a)", S),
            (Ms, "SET NOCOUNT, IDENTITY_INSERT t ON", D("SET")),
            (Ms, "SELECT 1 DISABLE TRIGGER ALL ON t", D("DISABLE")),
            (Ms, "SELECT 1 SET IMPLICIT_TRANSACTIONS ON", D("SET")),
            (Ms, "SELECT 1 SET CONTEXT_INFO 0x01", D("SET")),
            (Ms, "SELECT 1 SETUSER 'x'", D("SETUSER")),
            (Ms, "SELECT 1 UPDATETEXT t.c @p 0 NULL 'x'", D("UPDATETEXT")),
            (Ms, "SELECT 1 OPEN SYMMETRIC KEY k DECRYPTION BY PASSWORD = 'x'", D("OPEN")),
            (Ms, "SELECT * FROM t RECEIVE TOP (1) * FROM q", D("RECEIVE")),
            (Ms, "SELECT 1 SEND ON CONVERSATION @h", D("SEND")),
            (Ms, "UPDATE t SET a = 1 WHERE id = 1 SET IMPLICIT_TRANSACTIONS ON", D("SET")),
            (Ms, "SELECT disable, receive, [open] FROM t", R),
            (Ms, "SELECT * FROM t ORDER BY a OFFSET 5 ROWS FETCH NEXT 5 ROWS ONLY", R),
            (Ms, "DECLARE @x int\nSET @x = 1\nUPDATE t SET a = @x WHERE id = 2", S),
            (Ms, "CREATE TABLE c (id int REFERENCES p ON DELETE SET NULL ON UPDATE SET DEFAULT)", D("CREATE")),
            (Ms, "SET ANSI_NULLS, QUOTED_IDENTIFIER ON; INSERT INTO t VALUES (1)", S),
            (Pg, "SELECT set_config('role', 'admin', true); INSERT INTO t VALUES (1)", D("set_config()")),
            (Pg, "BEGIN; INSERT INTO t VALUES (1); COMMIT", D("transaction control")),
            (Ms, "UPDATE STATISTICS dbo.t", D("UPDATE")),
            (Pg, "DECLARE c CURSOR FOR SELECT 1", D("DECLARE")),
        ];
        let mut failed = Vec::new();
        for (d, sql, want) in corpus {
            let got = agent_write_class(sql, *d);
            let ok = match (want, &got) {
                (R, WriteClass::Read) | (S, WriteClass::SafeWrite) => true,
                (D(x), WriteClass::Destructive { reasons }) => reasons.iter().any(|r| r.contains(x)),
                _ => false,
            };
            if !ok {
                failed.push(format!("{d:?} {sql:?}: want {want:?}, got {got:?}"));
            }
        }
        assert!(failed.is_empty(), "{} of {} wrong:\n{}", failed.len(), corpus.len(), failed.join("\n"));
        assert!(corpus.len() >= 100, "{}", corpus.len());
    }

    #[test]
    fn agent_previews() {
        use Dialect::{MySql as My, Postgres as Pg, TSql as Ms};
        for (d, sql, want) in [
            (Pg, "UPDATE t SET a = 1 WHERE id = 2", Preview::Held),
            (Pg, "DELETE FROM t", Preview::Held),
            (Pg, "INSERT INTO a VALUES (1); DELETE FROM b WHERE x RETURNING *", Preview::Held),
            (Pg, "DROP TABLE t", Preview::AskFirst),
            (Pg, "TRUNCATE t", Preview::AskFirst),
            (Pg, "CALL p()", Preview::AskFirst),
            (Pg, "DO $$ BEGIN END $$", Preview::AskFirst),
            (Pg, "SELECT dblink_exec('x', 'y')", Preview::AskFirst),
            (Pg, "INSERT INTO t VALUES (1); ALTER TABLE t ADD c int", Preview::AskFirst),
            (Pg, "VACUUM t", Preview::Outside),
            (Pg, "CREATE INDEX CONCURRENTLY i ON t (x)", Preview::Outside),
            (Pg, "CREATE DATABASE x", Preview::Outside),
            (My, "DELETE FROM t", Preview::Held),
            (My, "ALTER TABLE t ADD c int", Preview::AskFirst),
            (My, "TRUNCATE t", Preview::AskFirst),
            (Ms, "DELETE TOP (5) FROM t", Preview::Held),
            (Ms, "EXEC dbo.p", Preview::AskFirst),
            (Ms, "INSERT INTO t EXEC dbo.p", Preview::AskFirst),
            (Ms, "ALTER DATABASE x SET RECOVERY SIMPLE", Preview::Outside),
            (Ms, "SET NOCOUNT ON; UPDATE t SET a = 1", Preview::Held),
        ] {
            assert_eq!(agent_preview(sql, d), want, "{d:?} {sql}");
        }
    }
}
