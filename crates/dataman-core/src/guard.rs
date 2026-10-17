//! What a statement does, read with the lexer (comments and strings never count), and the
//! rules that ask before running the dangerous ones.
//!
//! [`classify`] gives a statement's [`Facts`]: its command, whether it reads, writes or changes
//! the schema, its target, an `UPDATE`/`DELETE` without `WHERE` (in the statement or in one of
//! its `WITH` queries), `ALTER … DROP`, transaction control, and whether it may run inside a
//! transaction block. [`RULES`] is the table the confirmation sheet follows; [`review`] applies
//! it to a script for a connection's environment, and [`read_only_refusal`] is the client-side
//! refusal of a read-only connection.

use crate::config::Environment;
use crate::sql::{Dialect, Token, TokenKind, tokenize_in};

/// What a statement does to the database, from least to most.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    /// Reads (`SELECT`, `SHOW`, `EXPLAIN`), or nothing at all.
    #[default]
    Read,
    /// Session and maintenance commands: `SET`, `VACUUM`, `LISTEN`, `PREPARE`.
    Other,
    /// Transaction control: `BEGIN`, `COMMIT`, `SAVEPOINT`.
    Tx,
    /// Changes rows: `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `TRUNCATE`, `COPY … FROM`, `CALL`.
    Write,
    /// Changes the schema or the privileges: `CREATE`, `ALTER`, `DROP`, `GRANT`.
    Ddl,
}

/// A transaction-control command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxCmd {
    Begin,
    Commit,
    Rollback,
    Savepoint,
    Release,
    RollbackTo,
    /// `PREPARE TRANSACTION`, `COMMIT PREPARED`, `ROLLBACK PREPARED`.
    Prepared,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// `UPDATE`, `DROP TABLE`, `ALTER TABLE`: the command, upper case.
    pub verb: String,
    /// The strongest effect of the statement and its `WITH` queries.
    pub effect: Effect,
    /// The table or object acted on, as written (`public.orders`).
    pub target: Option<String>,
    /// An `UPDATE` or `DELETE` (the statement or one in a `WITH`) without a `WHERE`.
    pub no_where: bool,
    /// An `UPDATE` or `DELETE` (the statement or one in a `WITH`) whose `WHERE` is always true
    /// (`WHERE 1=1`, `WHERE true`, `WHERE id = id`, `… OR 1=1`): every row, as without one.
    pub trivial_where: bool,
    /// A `MERGE` that may touch rows its source does not bound: `ON` always true, or `WHEN NOT
    /// MATCHED BY SOURCE`.
    pub merge_unbounded: bool,
    /// `ALTER … DROP`: what it drops, as written (`DROP COLUMN email`).
    pub alter_drop: Option<String>,
    pub tx: Option<TxCmd>,
    /// Cannot run inside a transaction block (`VACUUM`, `CREATE DATABASE`, `… CONCURRENTLY`).
    pub outside_tx: bool,
}

impl Facts {
    /// Writes rows or changes the schema.
    pub fn is_write(&self) -> bool {
        matches!(self.effect, Effect::Write | Effect::Ddl)
    }

    /// Valid in a failed transaction (everything else is refused until it ends).
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

    /// The index of the `)` closing the `(` at `open` (the end when unbalanced).
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

    /// Indexes of the tokens outside any parentheses.
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

    /// A possibly qualified name at `i` (`schema.table`, `"Quoted"`), as written, and the index
    /// after it.
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

    /// Names separated by commas from `i` (`TRUNCATE a, b`), joined.
    fn names(&self, mut i: usize) -> Option<String> {
        let mut out = Vec::new();
        while let Some((n, next)) = self.name(i) {
            out.push(n);
            i = next;
            // `TRUNCATE t *`: the star of the old inheritance syntax.
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

/// What `sql` (one statement) does, read as PostgreSQL.
pub fn classify(sql: &str) -> Facts {
    classify_in(sql, Dialect::Postgres)
}

/// [`classify`] for a statement in `dialect` (its comments and strings lexed as the dialect's).
/// A T-SQL batch may hold several statements without `;` between them: its facts are those of
/// the statement the rules would ask about first, with the strongest effect of them all.
pub fn classify_in(sql: &str, dialect: Dialect) -> Facts {
    let all: Vec<Token> = tokenize_in(sql, dialect).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let mut t = Toks { src: sql, t: &all, dialect };
    while t.t.last().is_some_and(|x| x.kind == TokenKind::Punct && x.text(sql) == ";") {
        t.t = &t.t[..t.t.len() - 1];
    }
    if dialect != Dialect::TSql {
        return with_queries(&t);
    }
    let pieces: Vec<Facts> = tsql_pieces(&t).into_iter().map(|r| with_queries(&t.sub(r))).filter(|f| !f.verb.is_empty()).collect();
    combine(pieces)
}

/// The facts of a batch's statements together: those of the first one a rule asks about (the
/// rules' order), else of the one with the strongest effect, else the first; the strongest
/// effect, any transaction control (so a batch with `BEGIN TRAN` gets no implicit BEGIN), and
/// whether one cannot run in a transaction.
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

/// The statements of a T-SQL batch (which need no `;` between them), as byte ranges of `sql`.
pub fn tsql_statements(sql: &str) -> Vec<std::ops::Range<usize>> {
    let all: Vec<Token> = tokenize_in(sql, Dialect::TSql).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let t = Toks { src: sql, t: &all, dialect: Dialect::TSql };
    tsql_pieces(&t)
        .into_iter()
        .filter_map(|r| {
            let mut toks = &all[r];
            while toks.last().is_some_and(|x| x.kind == TokenKind::Punct && x.text(sql) == ";") {
                toks = &toks[..toks.len() - 1];
            }
            Some(toks.first()?.start..toks.last()?.end)
        })
        .collect()
}

/// Whether a T-SQL statement returns rows: a SELECT (not `SELECT … INTO`), a procedure, or a
/// write with an `OUTPUT` clause (not `OUTPUT … INTO`).
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

/// Words that start a T-SQL statement (the ones that matter to the rules, and those that
/// separate them).
const TSQL_HEADS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "DROP", "CREATE", "ALTER", "EXEC", "EXECUTE", "BULK", "GRANT",
    "REVOKE", "DENY", "BEGIN", "COMMIT", "ROLLBACK", "SAVE", "DECLARE", "USE", "PRINT", "IF", "WHILE", "RETURN", "WAITFOR",
    "RAISERROR", "THROW", "DBCC", "BACKUP", "RESTORE", "KILL",
];

/// The statements of a T-SQL batch, which need no `;` between them: one starts at a statement
/// word outside parentheses unless the words before make it part of the one going on (`ALTER
/// TABLE t DROP COLUMN`, `ON DELETE CASCADE`, `THEN UPDATE` in a MERGE, `INSERT … SELECT`,
/// `UNION SELECT`, `CURSOR FOR SELECT`, a CTE's statement). The body of a procedure, function,
/// trigger or view is part of its CREATE (it does not run).
fn tsql_pieces(t: &Toks) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    // The current statement's first word, and whether a CTE or an INSERT still waits for its
    // statement or its rows.
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
        // An INSERT with its rows.
        if head == "INSERT" && matches!(w.as_str(), "VALUES" | "DEFAULT") {
            waiting = false;
        }
        if !TSQL_HEADS.contains(&w.as_str()) {
            continue;
        }
        let prev = t.word(i - 1).unwrap_or_default();
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

/// A statement with its leading `WITH` queries: the main statement's facts, with the strongest
/// effect of all, and the no-WHERE of any of them (that one's command and target shown).
fn with_queries(t: &Toks) -> Facts {
    // A parenthesized query (`(SELECT 1) UNION (SELECT 2)`, `(DELETE … RETURNING *)`) is read as
    // its first part.
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
        // name [(columns)] AS [NOT] [MATERIALIZED] ( body )
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

/// Adds what a statement nested in `f` (a CTE's body) does to `f`'s facts.
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

/// Object words of `CREATE`/`DROP`/`ALTER` (`MATERIALIZED VIEW`, `FOREIGN TABLE`) and the
/// modifiers before them.
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

/// `DROP [MATERIALIZED] VIEW [IF EXISTS] [CONCURRENTLY] name`: the command (with its object
/// words), the target, and the index after the target.
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
    // ALTER/DROP … IF EXISTS, CREATE … IF NOT EXISTS, CONCURRENTLY, ONLY.
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

/// One statement, without a leading `WITH`.
fn statement(t: &Toks) -> Facts {
    let Some(v) = t.word(0) else { return Facts::default() };
    let mut f = Facts { verb: v.clone(), ..Default::default() };
    match v.as_str() {
        "SELECT" => {
            // `SELECT … INTO new_table`: creates a table.
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
        // MySQL's own reads and commands.
        "DESCRIBE" | "DESC" | "CHECK" | "CHECKSUM" | "HANDLER" if t.dialect == Dialect::MySql => {}
        "DO" if t.dialect == Dialect::MySql => {}
        "REPLACE" if t.dialect == Dialect::MySql => {
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
            // `EXEC @rc = proc`.
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
            // `ROLLBACK TRANSACTION savepoint` goes back to it.
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
                analyze = (i..close).any(|j| t.is(j, "ANALYZE") || t.is(j, "ANALYSE")) && !(i..close).any(|j| t.is(j, "FALSE") || t.is(j, "OFF"));
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
            // PostgreSQL's ONLY; MySQL's LOW_PRIORITY and IGNORE.
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

/// T-SQL's `TOP (n) [PERCENT]` after `UPDATE`, `DELETE`, `MERGE`: the index after it.
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

/// SQL Server's system procedures that only report (an `EXEC` of anything else may write).
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

/// `ALTER TABLE t DROP [COLUMN] x`, `… DROP CONSTRAINT c`: what is dropped. Not the column
/// properties (`ALTER COLUMN x DROP DEFAULT`, `DROP NOT NULL`, `DROP IDENTITY`, `DROP
/// EXPRESSION`), which lose no data.
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
        // DROP [COLUMN|CONSTRAINT|…] [IF EXISTS] name
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

// ---------------------------------------------------------------------------------------------
// Predicates that are always true

/// Words that end a `WHERE` (the clauses after it, T-SQL's `ELSE` of an `IF`).
const WHERE_END: &[&str] = &["RETURNING", "ORDER", "LIMIT", "OFFSET", "OPTION", "FOR", "GROUP", "HAVING", "WINDOW", "UNION", "EXCEPT", "INTERSECT", "ELSE"];

/// Words of a predicate that are not column references (reserved words only: a column may be
/// named `date` or `user`).
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

    /// The ranges between the top-level `word`s (`a OR b OR c`), or the whole when there is none.
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

    /// A token as compared between the two sides of `=`: names without their quotes and case.
    fn norm(&self, i: usize) -> String {
        let t = &self.t[i];
        let s = t.text(self.src);
        match t.kind {
            TokenKind::QuotedIdent => s.trim_matches(['"', '`', '[', ']']).to_ascii_lowercase(),
            TokenKind::Ident | TokenKind::Keyword => s.to_ascii_lowercase(),
            _ => s.to_string(),
        }
    }

    /// Whether the tokens name a column (anything but a predicate word, a function's name, a
    /// type after `::` or `AS`, a literal's prefix, a parameter or a variable).
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
            // `DATE '2020-01-01'`, `INTERVAL '1 day'`: a literal's type.
            let literal = self.t.get(i + 1).is_some_and(|n| n.kind == TokenKind::String);
            !(self.punct(i + 1, "(") || after_cast || literal)
        })
    }
}

/// Whether a predicate is always true (or holds no column at all, so either all rows or none):
/// no column in it (`1=1`, `true`, `'a' = 'a'`, `now() > '2000-01-01'`, `@flag = 1`), `x = x`,
/// a branch of a top-level `OR` that is, or every branch of a top-level `AND`. A heuristic: what
/// it misses (`id > 0`, `id IS NOT NULL`) is what the agents' row limit is for.
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
    // `a = a` (and `>=`, `<=`, `LIKE`): the same tokens on both sides.
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

/// An `UPDATE`'s or `DELETE`'s `WHERE` is always true ([`trivial`]); false without one, or for
/// `WHERE CURRENT OF cursor`.
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

/// A `MERGE` whose `ON` is always true, or with a `WHEN NOT MATCHED BY SOURCE` (it changes the
/// target's rows the source does not have: all of them, for an empty source).
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

// ---------------------------------------------------------------------------------------------
// Rules

/// Where a rule applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every connection.
    Always,
    /// Connections tagged production.
    Production,
}

/// What a rule looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Test {
    /// An `UPDATE` or `DELETE` without `WHERE`.
    NoWhere,
    /// The statement's first word.
    Command(&'static str),
    /// `ALTER … DROP` (a column, a constraint).
    AlterDrop,
    /// The statement's effect.
    Effect(Effect),
}

#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub id: &'static str,
    pub scope: Scope,
    pub test: Test,
}

/// The confirmation rules, the most specific first (the first that matches names the
/// statement on the sheet).
pub const RULES: &[Rule] = &[
    Rule { id: "no-where", scope: Scope::Always, test: Test::NoWhere },
    Rule { id: "truncate", scope: Scope::Always, test: Test::Command("TRUNCATE") },
    Rule { id: "drop", scope: Scope::Always, test: Test::Command("DROP") },
    Rule { id: "alter-drop", scope: Scope::Always, test: Test::AlterDrop },
    Rule { id: "write", scope: Scope::Production, test: Test::Effect(Effect::Write) },
    Rule { id: "ddl", scope: Scope::Production, test: Test::Effect(Effect::Ddl) },
];

/// A statement the sheet asks about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Its index in the script.
    pub index: usize,
    pub rule: &'static str,
    /// `DELETE without WHERE`, `DROP TABLE`, `ALTER TABLE · DROP COLUMN email`, `UPDATE`.
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

/// The first rule that asks about `sql` on a connection of `env`, if any.
pub fn check(sql: &str, env: Environment) -> Option<(&'static Rule, Facts)> {
    check_in(sql, env, Dialect::Postgres)
}

/// [`check`] for a statement in `dialect`.
pub fn check_in(sql: &str, env: Environment, dialect: Dialect) -> Option<(&'static Rule, Facts)> {
    let f = classify_in(sql, dialect);
    RULES
        .iter()
        .filter(|r| r.scope == Scope::Always || env.is_production())
        .find(|r| matches(r.test, &f))
        .map(|r| (r, f))
}

/// The statements of a script the sheet asks about, in order.
pub fn review<S: AsRef<str>>(stmts: &[S], env: Environment) -> Vec<Finding> {
    review_in(stmts, env, Dialect::Postgres)
}

/// [`review`] for a script in `dialect`.
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

/// Why a read-only connection refuses `sql` before it reaches the server, if it does.
pub fn read_only_refusal(sql: &str) -> Option<String> {
    read_only_refusal_in(sql, Dialect::Postgres)
}

/// [`read_only_refusal`] for a statement in `dialect`.
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
    "txid_current", "pg_current_xact_id",
    // MySQL: named locks held past the transaction, server files, waits on replication.
    "get_lock", "release_lock", "release_all_locks", "load_file", "master_pos_wait", "source_pos_wait", "wait_for_executed_gtid_set",
    // SQL Server: other servers and files (which may run anything there).
    "openrowset", "openquery", "opendatasource",
];

/// T-SQL table hints that take locks a read must not hold (`WITH (UPDLOCK)`).
const TSQL_LOCK_HINTS: &[&str] = &["UPDLOCK", "XLOCK", "HOLDLOCK", "TABLOCK", "TABLOCKX", "PAGLOCK", "ROWLOCK", "SERIALIZABLE", "REPEATABLEREAD"];

/// Words that make a LAR built-in (`lar_*`) an administrative action rather than a read.
const LAR_ACTION_WORDS: &[&str] = &[
    "backup", "restore", "create", "drop", "alter", "grant", "revoke", "user", "password", "warm", "compact", "checkpoint",
    "vacuum", "promote", "demote", "kill", "cancel", "terminate", "reset", "set", "flush", "import", "export", "write", "delete",
    "truncate", "reload", "shutdown", "failover", "join", "leave", "evict", "rotate",
];

/// Why an agent's read-only statement is refused before it reaches the server, if it is. This
/// is stricter than [`read_only_refusal`]: only reads run (`SELECT`, `WITH … SELECT`, `TABLE`,
/// `VALUES`, `SHOW`, `EXPLAIN` without ANALYZE of a write), no row locks (`FOR UPDATE`), no
/// transaction control, and no function with effects a read-only transaction would let through
/// ([`SIDE_EFFECT_FUNCTIONS`], LAR's administrative `lar_*` built-ins).
pub fn agent_read_refusal(sql: &str) -> Option<String> {
    agent_read_refusal_in(sql, Dialect::Postgres)
}

/// [`agent_read_refusal`] for a statement in `dialect` (MySQL's `DESCRIBE` reads too; its
/// `LOCK IN SHARE MODE` is a row lock).
pub fn agent_read_refusal_in(sql: &str, dialect: Dialect) -> Option<String> {
    let f = classify_in(sql, dialect);
    if f.verb.is_empty() {
        return Some("empty statement".into());
    }
    if f.tx.is_some() {
        return Some(format!("transaction control ({}) is not allowed", f.verb));
    }
    let reads = matches!(f.verb.as_str(), "SELECT" | "TABLE" | "VALUES" | "SHOW" | "EXPLAIN")
        || (dialect == Dialect::MySql && matches!(f.verb.as_str(), "DESCRIBE" | "DESC"));
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
    match effect_call(&t, &[]) {
        Some((name, false)) => Some(format!("{name}() has effects outside a read-only transaction")),
        Some((name, true)) => Some(format!("{name}() is an administrative action")),
        None => None,
    }
}

/// The first call to a function with effects outside the transaction ([`SIDE_EFFECT_FUNCTIONS`]
/// but those in `allowed`) or to one of LAR's administrative built-ins (`true`).
fn effect_call(t: &Toks, allowed: &[&str]) -> Option<(String, bool)> {
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

// ---------------------------------------------------------------------------------------------
// Agents' writes (the MCP permission tiers)

/// What an agent's script does, for the MCP tiers (`write` runs `SafeWrite`; `full` asks a
/// human about `Destructive`). Whatever is not understood is destructive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteClass {
    /// Reads only.
    Read,
    /// Rows changed within bounds: `INSERT`, `UPDATE` / `DELETE` with a `WHERE` that is not
    /// always true, a `MERGE` its source bounds, row locks, a sequence's next value.
    SafeWrite,
    /// Everything else that changes something: `UPDATE` / `DELETE` without a real `WHERE`,
    /// `TRUNCATE`, DDL, privileges, procedures, `SELECT … INTO`, `COPY`, functions with outside
    /// effects, session and server commands, control flow; the reasons, terse, in order.
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

/// How an agent's write can be shown to a human before it commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Preview {
    /// Run in a transaction held open for the answer: its rows are counted and its RETURNING /
    /// OUTPUT read, then it commits or rolls back.
    Held,
    /// Asked about before it runs, then run in a transaction: DDL and `TRUNCATE` (a preview
    /// would hold their exclusive locks while waiting, and MySQL and LAR commit them anyway),
    /// procedures and functions with outside effects (which would act before the answer).
    AskFirst,
    /// Asked about before it runs, then run alone outside a transaction: `VACUUM`, `CREATE
    /// INDEX CONCURRENTLY`, `CREATE DATABASE`, SQL Server's `ALTER DATABASE`.
    Outside,
}

/// Functions an agent's write may call: a sequence's next value, a notification, the
/// transaction's id. The rest of [`SIDE_EFFECT_FUNCTIONS`] are destructive in writes too.
pub const WRITE_OK_FUNCTIONS: &[&str] = &["nextval", "pg_notify", "txid_current", "pg_current_xact_id"];

/// T-SQL session settings a write may change for its own batch.
const TSQL_SESSION_SETTINGS: &[&str] = &[
    "NOCOUNT", "XACT_ABORT", "ANSI_NULLS", "ANSI_WARNINGS", "ANSI_PADDING", "ANSI_NULL_DFLT_ON", "QUOTED_IDENTIFIER", "ARITHABORT",
    "CONCAT_NULL_YIELDS_NULL", "NUMERIC_ROUNDABORT", "DATEFORMAT", "DATEFIRST", "LANGUAGE", "LOCK_TIMEOUT", "DEADLOCK_PRIORITY", "TEXTSIZE",
    "ROWCOUNT", "STATISTICS",
];

/// The statements of a script as the tiers read them: split at `;` (and at T-SQL's `GO`
/// lines), a T-SQL batch into its statements.
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

/// One statement's part in [`agent_write_class`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    /// Changes nothing and reads nothing: a T-SQL variable, a batch's session setting, PRINT.
    Neutral,
    Read,
    Safe,
    Destructive(String),
}

/// A T-SQL (or MySQL) variable, a T-SQL batch setting or a PRINT: nothing outside the batch.
fn neutral(t: &Toks, f: &Facts) -> bool {
    let var = |i: usize| t.t.get(i).is_some_and(|x| x.kind == TokenKind::Ident && x.text(t.src).starts_with('@') && !x.text(t.src).starts_with("@@"));
    match (t.dialect, f.verb.as_str()) {
        (Dialect::TSql, "DECLARE") => var(1),
        (Dialect::TSql, "SET") => var(1) || t.word(1).is_some_and(|w| TSQL_SESSION_SETTINGS.contains(&w.as_str()) || w.starts_with("ANSI_")),
        (Dialect::TSql, "PRINT") => true,
        (Dialect::MySql, "SET") => var(1),
        _ => false,
    }
}

fn piece(sql: &str, dialect: Dialect) -> Piece {
    let all: Vec<Token> = tokenize_in(sql, dialect).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let t = Toks { src: sql, t: &all, dialect };
    if all.iter().all(|x| x.kind == TokenKind::Punct && x.text(sql) == ";") {
        return Piece::Neutral;
    }
    let f = classify_in(sql, dialect);
    if let Some((name, _)) = effect_call(&t, WRITE_OK_FUNCTIONS) {
        return Piece::Destructive(format!("{name}()"));
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
        // Row locks, a sequence's next value, T-SQL's lock hints: held only by its transaction.
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

/// What an agent's script does ([`WriteClass`]), every statement of it read with the lexer of
/// `dialect` (comments and strings never count; T-SQL's batches statement by statement).
pub fn agent_write_class(sql: &str, dialect: Dialect) -> WriteClass {
    let mut safe = false;
    let mut reasons: Vec<String> = Vec::new();
    for p in agent_pieces(sql, dialect) {
        match piece(&p, dialect) {
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

/// How an agent's script can be previewed before it commits ([`Preview`]): the most cautious
/// its statements need.
pub fn agent_preview(sql: &str, dialect: Dialect) -> Preview {
    let mut out = Preview::Held;
    for p in agent_pieces(sql, dialect) {
        let all: Vec<Token> = tokenize_in(&p, dialect).into_iter().filter(|t| !t.kind.is_trivia()).collect();
        let t = Toks { src: &p, t: &all, dialect };
        let f = classify_in(&p, dialect);
        let this = if f.outside_tx {
            Preview::Outside
        } else if f.effect == Effect::Ddl
            || matches!(f.effect, Effect::Other | Effect::Tx) && !neutral(&t, &f)
            || matches!(f.verb.as_str(), "TRUNCATE" | "CALL" | "DO" | "EXEC" | "EXECUTE")
            || effect_call(&t, WRITE_OK_FUNCTIONS).is_some()
            || matches!(piece(&p, dialect), Piece::Destructive(r) if r == "INSERT … EXEC")
        {
            Preview::AskFirst
        } else {
            Preview::Held
        };
        out = out.max(this);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Agents' allowed queries (a connection's MCP › Advanced)

/// One token of a statement as the allowed queries compare it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AllowTok {
    /// A `?` of an entry: one value.
    Value,
    /// A keyword or plain name (upper case), or anything else as written.
    Text(TokenKind, String),
}

/// A value [`AllowTok::Value`] stands for: a string, a number, a `$n` (or MySQL `?`) parameter.
fn is_value(k: TokenKind) -> bool {
    matches!(k, TokenKind::String | TokenKind::DollarString | TokenKind::Number | TokenKind::Param)
}

/// The tokens of one statement for the comparison: comments and whitespace go, a trailing `;`
/// goes, keywords and plain names are upper case; quoted names, strings and numbers stay as
/// written. In an entry (`entry`) a `?` is a value (PostgreSQL lexes `=?` as one operator: its
/// `?` is split off).
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

/// Whether `stmt` (one statement) is the one an allowed-queries `entry` names: the same tokens
/// in the same order, case, spacing, comments and a trailing `;` aside; a `?` of the entry
/// stands for exactly one value (a string, a number or a `$n` parameter, signed or not), never
/// for a name, an expression or more SQL. So `DELETE FROM sessions WHERE expires < ?` allows
/// `delete from sessions where expires < '2026-01-01'` and not `… < now() OR true`.
pub fn allowlist_match(entry: &str, stmt: &str, dialect: Dialect) -> bool {
    let e = allow_tokens(entry, dialect, true);
    let s = allow_tokens(stmt, dialect, false);
    if e.is_empty() {
        return false;
    }
    let mut j = 0;
    for t in &e {
        match t {
            AllowTok::Value => {
                // `-5`, `+1`: a sign and its number are one value.
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

/// An allowed-queries entry as typed, checked: exactly one statement (as written, the trailing
/// `;` dropped), not transaction control (every agent call is its own transaction).
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
    Ok(stmt)
}

/// A script against the allowed queries: the entries its statements matched (each once, in
/// the list's order), and the statements no entry allows.
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

/// The statements of a script an "Always allow" adds to the allowed queries: those that are not
/// plain reads (reads run in every tier already), each as written.
pub fn allowlist_candidates(sql: &str, dialect: Dialect) -> Vec<String> {
    agent_pieces(sql, dialect)
        .into_iter()
        .filter(|p| agent_write_class(p, dialect).is_write())
        .filter_map(|p| allowlist_entry(&p, dialect).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allowed queries: token for token, case, spacing, comments and the `;` aside; `?` is one
    /// value and nothing else; quoted names and strings as written.
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
        // `=?` lexes as one operator: its `?` is still a value.
        assert!(allowlist_match("UPDATE t SET a=? WHERE id=?", "update t set a = 'x' where id = 7", Pg));
        // Exact entries; strings and quoted names keep their case.
        assert!(allowlist_match("TRUNCATE audit_tmp;", "truncate AUDIT_TMP", Pg));
        assert!(!allowlist_match("UPDATE t SET s = 'Paid' WHERE id = 1", "UPDATE t SET s = 'paid' WHERE id = 1", Pg));
        assert!(!allowlist_match("DELETE FROM \"Users\" WHERE id = 1", "DELETE FROM \"users\" WHERE id = 1", Pg));
        assert!(!allowlist_match("", "SELECT 1", Pg) && !allowlist_match("  ;", "", Pg));
        // Entries: one statement, never transaction control.
        assert_eq!(allowlist_entry("  TRUNCATE audit_tmp ; ", Pg).as_deref(), Ok("TRUNCATE audit_tmp"));
        assert!(allowlist_entry("DELETE FROM a; DELETE FROM b", Pg).unwrap_err().contains("one statement"));
        assert!(allowlist_entry("COMMIT", Pg).unwrap_err().contains("transaction control"));
        assert!(allowlist_entry(" -- nothing", Pg).is_err());
        // Scripts: each statement on its own; reads are not candidates.
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
            "SELECT lar_history('t')",
            "SELECT pg_sleep(0.1)",
            "SELECT format('for update')",
            "SELECT * FROM t FOR_x",
        ] {
            assert_eq!(agent_read_refusal(ok), None, "{ok}");
        }
        for (bad, why) in [
            ("", "empty"),
            ("INSERT INTO t VALUES (1)", "not a read"),
            ("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x", "not a read"),
            ("EXPLAIN ANALYZE DELETE FROM t", "not a read"),
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
        // Comments and strings are not code.
        assert!(nw("DELETE FROM orders -- WHERE id = 1"));
        assert!(nw("DELETE FROM orders /* WHERE id = 1 */"));
        assert!(nw("UPDATE t SET note = 'see WHERE clause'"));
        assert!(nw("UPDATE t SET note = $$ WHERE $$"));
        assert!(!nw("-- clean up\nDELETE FROM t\n WHERE done"));
        // A WHERE in a subquery is not the statement's.
        assert!(nw("UPDATE t SET x = (SELECT max(y) FROM u WHERE u.id = 1)"));
        assert!(!nw("UPDATE t SET x = (SELECT 1) WHERE id IN (SELECT id FROM u WHERE z)"));
        // DELETE … USING, UPDATE … FROM.
        assert!(nw("DELETE FROM t USING u"));
        assert!(!nw("DELETE FROM t USING u WHERE t.id = u.id"));
        assert!(nw("UPDATE t SET x = u.x FROM u"));
        assert!(!nw("UPDATE t SET x = u.x FROM u WHERE t.id = u.id"));
        assert!(!nw("DELETE FROM t WHERE CURRENT OF c"));
        // RETURNING does not count.
        assert!(nw("DELETE FROM t RETURNING *"));
        // Not an UPDATE: INSERT … ON CONFLICT DO UPDATE.
        assert!(!nw("INSERT INTO t VALUES (1) ON CONFLICT (id) DO UPDATE SET x = 1"));
        assert!(!nw("SELECT * FROM t"));
    }

    #[test]
    fn no_where_through_with() {
        // The main statement after a CTE.
        assert!(nw("WITH old AS (SELECT id FROM t WHERE x) DELETE FROM u"));
        assert!(!nw("WITH old AS (SELECT id FROM t) DELETE FROM u WHERE id IN (SELECT id FROM old)"));
        // A data-modifying CTE.
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
        // What cannot run inside a transaction block.
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

    /// T-SQL: batches of statements without `;`, EXEC, SELECT INTO, TOP, transaction words.
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
        // A batch: the statement the rules ask about first, the strongest effect, any
        // transaction control.
        let f = c("DECLARE @n int = 3\nSELECT @n\nUPDATE dbo.t SET a = @n\nSELECT 1");
        assert_eq!((f.verb.as_str(), f.no_where, f.effect), ("UPDATE", true, Effect::Write));
        let f = c("SET NOCOUNT ON\nIF @x > 0 DELETE FROM t WHERE id = 1 ELSE DROP TABLE t");
        assert_eq!((f.verb.as_str(), f.target.as_deref()), ("DROP TABLE", Some("t")));
        let f = c("BEGIN TRANSACTION\nUPDATE t SET a = 1 WHERE id = 2");
        assert_eq!((f.effect, f.tx), (Effect::Write, Some(TxCmd::Begin)));
        // What is part of a statement does not start one.
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
        // A body does not run when it is created.
        let f = c("CREATE PROCEDURE p AS BEGIN DELETE FROM t; DROP TABLE u; END");
        assert_eq!((f.verb.as_str(), f.no_where), ("CREATE PROCEDURE", false));
        assert_eq!(c("CREATE OR ALTER VIEW v AS SELECT * FROM t").verb, "CREATE VIEW");
        // Strings and comments never count.
        assert_eq!(c("SELECT N'DROP TABLE t' -- DELETE FROM x\n/* UPDATE y */").effect, Effect::Read);
        // Read-only connections and agents.
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
        // The rules in a script of batches.
        let f = review_in(&["SELECT 1", "UPDATE t SET a = 1\nGO", "EXEC dbo.p"], Environment::Production, d);
        assert_eq!(f.iter().map(|f| f.rule).collect::<Vec<_>>(), ["no-where", "write"]);
    }

    /// The agents' tiers: what is a read, a safe write or destructive, in every dialect, with
    /// the evasions an agent (or a prompt injection) would try. `D(x)`: destructive, a reason
    /// containing `x`.
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
            // Reads.
            (Pg, "SELECT * FROM t WHERE a = 'DELETE FROM x'", R),
            (Pg, "with a as (select 1) select * from a", R),
            (Pg, "EXPLAIN DELETE FROM t", R),
            (Pg, "SELECT lar_history('t')", R),
            (Pg, "-- only a comment", R),
            (My, "SELECT SLEEP(1)", R),
            (My, "DESCRIBE t", R),
            (Ms, "SELECT TOP (5) * FROM [dbo].[t] WITH (NOLOCK)", R),
            (Ms, "DECLARE @n int = 3\nSELECT @n", R),
            // Safe writes.
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
            // No real WHERE.
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
            // DDL, privileges, TRUNCATE.
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
            // Chained, batched, hidden.
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
            // Procedures and dynamic SQL.
            (Pg, "CALL p()", D("CALL")),
            (Pg, "DO $$ BEGIN DELETE FROM t; END $$", D("DO")),
            (Pg, "PREPARE q AS DELETE FROM t; EXECUTE q", D("PREPARE")),
            (My, "CALL purge()", D("CALL")),
            (Ms, "EXEC('DROP TABLE x')", D("EXEC")),
            (Ms, "EXEC sp_executesql N'DELETE FROM t'", D("EXEC")),
            (Ms, "sp_executesql N'DELETE FROM t'", D("SP_EXECUTESQL")),
            (Ms, "EXEC dbo.purge @days = 30", D("EXEC")),
            (Ms, "INSERT INTO t EXEC dbo.p", D("INSERT … EXEC")),
            // Functions with effects outside the transaction.
            (Pg, "SELECT pg_terminate_backend(pid) FROM pg_stat_activity", D("pg_terminate_backend()")),
            (Pg, "INSERT INTO t VALUES (pg_cancel_backend(1))", D("pg_cancel_backend()")),
            (Pg, "SELECT dblink_exec('host=x', 'DROP TABLE t')", D("dblink_exec()")),
            (Pg, "UPDATE t SET a = lo_import('/etc/passwd') WHERE id = 1", D("lo_import()")),
            (Pg, "SELECT setval('s', 1)", D("setval()")),
            (Pg, "SELECT pg_advisory_lock(1)", D("pg_advisory_lock()")),
            (Pg, "SELECT lar_backup('/tmp/b')", D("lar_backup()")),
            (My, "SELECT GET_LOCK('x', 10)", D("get_lock()")),
            (Ms, "SELECT * FROM OPENROWSET('SQLNCLI', 's', 'q')", D("openrowset()")),
            // Files, session and server commands, transaction control.
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
