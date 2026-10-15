//! Statement boundaries in a script.

use std::ops::Range;

use super::dialect::{Dialect, ScriptRules};
use super::lexer::{Token, TokenKind, tokenize, tokenize_in};

/// One statement of a script: `range` spans its first to last significant token (without the
/// terminating `;`), `full` extends to the `;` when there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub range: Range<usize>,
    pub full: Range<usize>,
}

/// Splits a script into statements at top-level `;` (not inside parentheses, strings,
/// dollar-quoted bodies or comments). Empty statements are dropped.
pub fn split(src: &str) -> Vec<Statement> {
    split_tokens(src, &tokenize(src))
}

/// [`split`] in `dialect`.
pub fn split_in(src: &str, dialect: Dialect) -> Vec<Statement> {
    split_tokens_in(src, &tokenize_in(src, dialect), dialect.script())
}

pub fn split_tokens(src: &str, tokens: &[Token]) -> Vec<Statement> {
    split_tokens_in(src, tokens, Dialect::Postgres.script())
}

/// `DELIMITER x` on a line of its own (only spaces before it): the new terminator, and where
/// the command's line ends.
fn delimiter_command(src: &str, t: &Token) -> Option<(String, usize)> {
    if !matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) || !t.text(src).eq_ignore_ascii_case("DELIMITER") {
        return None;
    }
    let line_start = src[..t.start].rfind('\n').map_or(0, |p| p + 1);
    if !src[line_start..t.start].trim().is_empty() {
        return None;
    }
    let line_end = src[t.end..].find('\n').map_or(src.len(), |p| t.end + p);
    let delim = src[t.end..line_end].split_whitespace().next()?;
    Some((delim.to_string(), line_end))
}

/// `GO` on a line of its own (only spaces before it; after it an optional repeat count and a
/// `--` comment): how many times the batch before it runs, and where the line ends.
fn go_line(src: &str, t: &Token) -> Option<(u32, usize)> {
    if !matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) || !t.text(src).eq_ignore_ascii_case("GO") {
        return None;
    }
    let line_start = src[..t.start].rfind('\n').map_or(0, |p| p + 1);
    if !src[line_start..t.start].trim().is_empty() {
        return None;
    }
    let line_end = src[t.end..].find('\n').map_or(src.len(), |p| t.end + p);
    let rest = src[t.end..line_end].trim();
    let rest = rest.split_once("--").map_or(rest, |(a, _)| a).trim();
    let n = if rest.is_empty() { 1 } else { rest.parse().ok()? };
    Some((n, line_end))
}

/// One batch of a T-SQL script: the text between `GO` lines (first to last significant
/// token), run `repeat` times (`GO 5`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoBatch {
    pub range: Range<usize>,
    pub repeat: u32,
    /// Where its `GO` line ends (the end of the script for the last batch).
    pub end: usize,
}

/// The batches of a T-SQL script, split at `GO` lines (never inside strings or comments).
/// Batches without a statement (only comments) are left out; `GO 0` runs its batch no time.
pub fn go_batches(src: &str) -> Vec<GoBatch> {
    let tokens = tokenize_in(src, Dialect::TSql);
    let mut out = Vec::new();
    let mut first: Option<usize> = None;
    let mut last = 0;
    let mut skip_until = 0;
    for t in tokens.iter().filter(|t| !t.kind.is_trivia()) {
        if t.start < skip_until {
            continue;
        }
        if let Some((n, line_end)) = go_line(src, t) {
            if let Some(f) = first.take() {
                out.push(GoBatch { range: f..last, repeat: n, end: line_end });
            }
            skip_until = line_end;
            continue;
        }
        first.get_or_insert(t.start);
        last = t.end;
    }
    if let Some(f) = first {
        out.push(GoBatch { range: f..last, repeat: 1, end: src.len() });
    }
    out
}

/// What ⌘↩ sends for the T-SQL statement `stmt` of `src`: the statement, or, when it uses
/// variables that its `GO` batch declares before it, the batch from its start through the
/// statement, in one round trip (a variable lives only in the batch that declares it).
pub fn tsql_run_range(src: &str, stmt: &Statement) -> Range<usize> {
    let own = stmt.range.clone();
    let Some(used) = crate::params::detect_tsql(&src[own.clone()]) else { return own };
    let Some(batch) = go_batches(src).into_iter().find(|b| b.range.start <= own.start && own.start <= b.range.end) else {
        return own;
    };
    if batch.range.start >= own.start {
        return own;
    }
    let wide = batch.range.start..own.end;
    let left = crate::params::detect_tsql(&src[wide.clone()]).map(|p| p.params.len()).unwrap_or(0);
    if left < used.params.len() { wide } else { own }
}

/// The units a script runs as, one round trip each: its statements, but in T-SQL its `GO`
/// batches (variables, temporary tables and control flow live in a batch). A batch repeated by
/// `GO n` keeps its `GO n` line, which the driver reads back.
pub fn batches_in(src: &str, dialect: Dialect) -> Vec<Statement> {
    if !dialect.script().go_batches {
        return split_in(src, dialect);
    }
    go_batches(src)
        .into_iter()
        .filter(|b| b.repeat > 0)
        .map(|b| {
            let range = if b.repeat > 1 { b.range.start..src[..b.end].trim_end().len() } else { b.range };
            Statement { full: range.start..b.end.max(range.end), range }
        })
        .collect()
}

/// T-SQL: whether a text may return more than one result set: several statements, several
/// batches, or a batch run again (`GO n`).
pub fn may_return_more(src: &str) -> bool {
    let b = go_batches(src);
    b.len() > 1 || b.iter().any(|b| b.repeat > 1) || b.first().is_some_and(|b| crate::guard::tsql_statements(&src[b.range.clone()]).len() > 1)
}

/// The statement of a T-SQL text that most likely returned its `k`-th result set (0-based): the
/// `k`-th statement that returns rows, across its batches. `None` when there is no such one
/// (a procedure may return several).
pub fn result_statement(src: &str, k: usize) -> Option<&str> {
    let mut n = 0;
    for b in go_batches(src) {
        for _ in 0..b.repeat {
            for r in crate::guard::tsql_statements(&src[b.range.clone()]) {
                let s = &src[b.range.start + r.start..b.range.start + r.end];
                if crate::guard::tsql_returns_rows(s) {
                    if n == k {
                        return Some(s);
                    }
                    n += 1;
                }
            }
        }
    }
    None
}

/// T-SQL words after BEGIN that do not open a block (`BEGIN TRAN`, `BEGIN DIALOG`…).
fn begins_block(next: Option<&str>) -> bool {
    !next.is_some_and(|w| {
        ["TRAN", "TRANSACTION", "DISTRIBUTED", "DIALOG", "CONVERSATION"].iter().any(|k| w.eq_ignore_ascii_case(k))
    })
}

/// Splits tokens lexed in a dialect by its [`ScriptRules`].
pub fn split_tokens_in(src: &str, tokens: &[Token], rules: ScriptRules) -> Vec<Statement> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    // SQL-standard function bodies (`BEGIN ATOMIC … END`, T-SQL's `BEGIN … END`) contain `;`.
    // Inside one, CASE … END pairs are tracked too, so their END does not close the body.
    let mut blocks: Vec<bool> = Vec::new(); // true = BEGIN (ATOMIC), false = CASE
    // T-SQL: a procedure's, function's or trigger's body runs to the end of its batch.
    let mut to_batch_end = false;
    let mut first: Option<usize> = None;
    let mut last = 0;
    // MySQL clients' `DELIMITER //`: the terminator in force when it is not `;`, and the tokens
    // to pass over (the command's line, the rest of a terminator lexed as several tokens).
    let mut delimiter: Option<String> = None;
    let mut skip_until = 0;
    let sig: Vec<&Token> = tokens.iter().filter(|t| !t.kind.is_trivia()).collect();
    for (i, t) in sig.iter().enumerate() {
        if t.start < skip_until {
            continue;
        }
        if rules.delimiter_command
            && first.is_none()
            && let Some((d, line_end)) = delimiter_command(src, t)
        {
            delimiter = (d != ";").then_some(d);
            skip_until = line_end;
            continue;
        }
        if let Some(d) = &delimiter
            && !matches!(t.kind, TokenKind::String | TokenKind::QuotedIdent | TokenKind::DollarString)
            && let Some(p) = t.text(src).find(d.as_str()).or_else(|| src[t.start..].starts_with(d.as_str()).then_some(0))
        {
            // The terminator may be glued to a word (`END$$`): what is before it belongs to the
            // statement.
            if p > 0 {
                first.get_or_insert(t.start);
                last = t.start + p;
            }
            if let Some(f) = first.take() {
                out.push(Statement { range: f..last, full: f..t.start + p + d.len() });
            }
            skip_until = t.start + p + d.len();
            continue;
        }
        if rules.go_batches
            && let Some((_, line_end)) = go_line(src, t)
        {
            if let Some(f) = first.take() {
                out.push(Statement { range: f..last, full: f..last });
            }
            (depth, to_batch_end) = (0, false);
            blocks.clear();
            skip_until = line_end;
            continue;
        }
        let word = |t: &Token, w: &str| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(src).eq_ignore_ascii_case(w);
        if rules.go_batches && first.is_none() && (word(t, "CREATE") || word(t, "ALTER")) {
            let mut j = i + 1;
            if sig.get(j).is_some_and(|n| word(n, "OR")) && sig.get(j + 1).is_some_and(|n| word(n, "ALTER")) {
                j += 2;
            }
            to_batch_end = sig.get(j).is_some_and(|n| ["PROC", "PROCEDURE", "FUNCTION", "TRIGGER"].iter().any(|k| word(n, k)));
        }
        if rules.begin_atomic && word(t, "BEGIN") && sig.get(i + 1).is_some_and(|n| word(n, "ATOMIC")) {
            blocks.push(true);
        } else if rules.go_batches && word(t, "BEGIN") && begins_block(sig.get(i + 1).map(|n| n.text(src))) {
            blocks.push(true);
        } else if !blocks.is_empty() && word(t, "CASE") {
            blocks.push(false);
        } else if !blocks.is_empty() && word(t, "END") {
            blocks.pop();
        }
        if t.kind == TokenKind::Punct {
            match b[t.start] {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth = (depth - 1).max(0),
                b';' if depth == 0 && blocks.is_empty() && delimiter.is_none() && !to_batch_end => {
                    if let Some(f) = first.take() {
                        out.push(Statement { range: f..last, full: f..t.end });
                    }
                    continue;
                }
                _ => {}
            }
        }
        first.get_or_insert(t.start);
        last = t.end;
    }
    if let Some(f) = first {
        out.push(Statement { range: f..last, full: f..last });
    }
    out
}

/// The statement to run for a cursor at byte `cursor`: the one containing it; in the gap
/// between two statements, the previous one if the cursor is on the line where it ends,
/// otherwise the next one (or the previous one when there is no next).
pub fn statement_at(src: &str, cursor: usize) -> Option<Statement> {
    statement_at_in(src, &split(src), cursor)
}

pub fn statement_at_in(src: &str, stmts: &[Statement], cursor: usize) -> Option<Statement> {
    for (i, s) in stmts.iter().enumerate() {
        if cursor >= s.range.start && cursor <= s.full.end {
            return Some(s.clone());
        }
        if cursor < s.range.start {
            if i == 0 {
                return Some(s.clone());
            }
            let prev = &stmts[i - 1];
            let between = &src[prev.full.end..cursor];
            return Some(if between.contains('\n') { s.clone() } else { prev.clone() });
        }
    }
    stmts.last().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str) -> Vec<&str> {
        split(src).into_iter().map(|s| &src[s.range]).collect()
    }

    #[test]
    fn splits_top_level_only() {
        let src = "select 1; select ';' ; ;\n create function f() returns int as $$ select 1; $$ language sql;\n-- c;\nselect (1;2);\ncreate function g() returns int language sql begin atomic select case when true then 1 end; select 2; end; select 3";
        assert_eq!(
            texts(src),
            vec![
                "select 1",
                "select ';'",
                "create function f() returns int as $$ select 1; $$ language sql",
                "select (1;2)",
                "create function g() returns int language sql begin atomic select case when true then 1 end; select 2; end",
                "select 3"
            ]
        );
    }

    #[test]
    fn mysql_delimiter_and_comments() {
        let src = "# setup\nSELECT 1; -- a\nDELIMITER //\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 'a;b'; # c;\n  SELECT 2;\nEND //\nDELIMITER ;\nSELECT `x;y` FROM t; SELECT \"q;\\\";\"";
        let got: Vec<&str> = split_in(src, Dialect::MySql).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(got, vec!["SELECT 1", "CREATE PROCEDURE p()\nBEGIN\n  SELECT 'a;b'; # c;\n  SELECT 2;\nEND", "SELECT `x;y` FROM t", "SELECT \"q;\\\";\""]);
        // `$$` (two tokens), glued to END, case-insensitive command, `;;`.
        let src = "delimiter $$\nCREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; END$$\nSELECT 1$$\nDELIMITER ;;\nSELECT 2;; SELECT 3;;";
        let got: Vec<&str> = split_in(src, Dialect::MySql).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(got, vec!["CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; END", "SELECT 1", "SELECT 2", "SELECT 3"]);
        // `full` takes the terminator; DELIMITER only counts at the start of a line and of a statement.
        let s = split_in("DELIMITER //\nSELECT 1 //", Dialect::MySql);
        assert_eq!((s.len(), &"DELIMITER //\nSELECT 1 //"[s[0].full.clone()]), (1, "SELECT 1 //"));
        assert_eq!(split_in("SELECT delimiter FROM t; SELECT 2", Dialect::MySql).len(), 2);
        // PostgreSQL has no DELIMITER command.
        assert_eq!(split("DELIMITER //\nSELECT 1; SELECT 2").len(), 2);
    }

    /// T-SQL: `GO` lines end batches (never inside strings or comments); `;` ends statements
    /// except inside `BEGIN … END` and procedure bodies.
    #[test]
    fn tsql_batches_and_statements() {
        let d = Dialect::TSql;
        let src = "CREATE TABLE #t (a int)\nGO\n-- GO inside a comment\nINSERT #t VALUES (1); /* GO\n*/ SELECT 'GO\nGO'\n  go  \nSELECT 1\nGO 3\nSELECT [GO] FROM x\nGO -- the end";
        let b = go_batches(src);
        let texts: Vec<&str> = b.iter().map(|b| &src[b.range.clone()]).collect();
        assert_eq!(texts, vec!["CREATE TABLE #t (a int)", "INSERT #t VALUES (1); /* GO\n*/ SELECT 'GO\nGO'", "SELECT 1", "SELECT [GO] FROM x"]);
        assert_eq!(b.iter().map(|b| b.repeat).collect::<Vec<_>>(), vec![1, 1, 3, 1]);
        // The units a script runs: its batches; `GO 3` keeps its line (the driver repeats it).
        let units: Vec<&str> = batches_in(src, d).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(units[2], "SELECT 1\nGO 3");
        assert_eq!(go_batches(&units[2])[0].repeat, 3);
        // PostgreSQL's units are its statements.
        assert_eq!(batches_in("select 1; select 2", Dialect::Postgres).len(), 2);
        // The editor's statements: `;` and GO, never inside BEGIN … END or a procedure's body.
        let src = "IF 1 = 1 BEGIN SELECT 1; SELECT 2; END; SELECT 3\nGO\nCREATE PROCEDURE p AS SELECT 1; SELECT 2;\nGO\nBEGIN TRAN; UPDATE t SET a = 1; COMMIT\nBEGIN TRY SELECT 1/0; END TRY BEGIN CATCH SELECT CASE WHEN 1 = 1 THEN 1 END; END CATCH";
        let s: Vec<&str> = split_in(src, d).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(
            s,
            vec![
                "IF 1 = 1 BEGIN SELECT 1; SELECT 2; END",
                "SELECT 3",
                "CREATE PROCEDURE p AS SELECT 1; SELECT 2;",
                "BEGIN TRAN",
                "UPDATE t SET a = 1",
                "COMMIT\nBEGIN TRY SELECT 1/0; END TRY BEGIN CATCH SELECT CASE WHEN 1 = 1 THEN 1 END; END CATCH"
            ]
        );
        // `GO` must be alone on its line, with only a count or a comment after it.
        assert_eq!(go_batches("SELECT 1 GO\nSELECT 2").len(), 1);
        assert_eq!(go_batches("SELECT 1\nGO x\nSELECT 2").len(), 1);
        assert_eq!(go_batches("SELECT 1\r\nGO\r\nSELECT 2").len(), 2);
        assert!(go_batches("-- only\nGO\n/* comments */").is_empty());
        // Several result sets: which statement returned which.
        let src = "DECLARE @n int = 2\nSELECT * FROM customer\nUPDATE t SET a = 1 OUTPUT inserted.id\nSELECT * INTO #x FROM t\nGO\nEXEC dbo.p";
        assert!(may_return_more(src) && may_return_more("SELECT 1 SELECT 2") && may_return_more("SELECT 1\nGO 2"));
        assert!(!may_return_more("SELECT * FROM t WHERE a IN (SELECT b FROM u)") && !may_return_more("EXEC dbo.p"));
        assert_eq!(result_statement(src, 0), Some("SELECT * FROM customer"));
        assert_eq!(result_statement(src, 1), Some("UPDATE t SET a = 1 OUTPUT inserted.id"));
        assert_eq!(result_statement(src, 2), Some("EXEC dbo.p"));
        assert_eq!(result_statement(src, 3), None);
    }

    #[test]
    fn cursor_picks_statement() {
        let src = "select 1;\n\nselect 2; select 3;\n";
        let at = |c| statement_at(src, c).map(|s| &src[s.range]);
        assert_eq!(at(0), Some("select 1"));
        assert_eq!(at(9), Some("select 1")); // right after `;`
        assert_eq!(at(10), Some("select 2")); // next line
        assert_eq!(at(src.find("select 3").unwrap() - 1), Some("select 2"));
        assert_eq!(at(src.len()), Some("select 3"));
        assert_eq!(statement_at("  -- only a comment", 3), None);
    }

    #[test]
    fn tsql_run_takes_the_declarations() {
        let src = "SELECT 0;\nGO\nDECLARE @d date = '2026-01-01';\nSET @d = DATEADD(day, 1, @d);\nSELECT * FROM t WHERE c > @d AND id = @id;\nSELECT 2;";
        let stmts = split_in(src, Dialect::TSql);
        let at = |needle: &str| {
            let s = statement_at_in(src, &stmts, src.find(needle).unwrap()).unwrap();
            &src[tsql_run_range(src, &s)]
        };
        // Uses @d, declared above in its batch: from the batch's start (not the batch before).
        assert_eq!(at("SELECT *"), "DECLARE @d date = '2026-01-01';\nSET @d = DATEADD(day, 1, @d);\nSELECT * FROM t WHERE c > @d AND id = @id");
        // No variables, or only undeclared ones (a parameter): the statement alone.
        assert_eq!(at("SELECT 2"), "SELECT 2");
        let src = "SELECT * FROM t WHERE id = @id";
        assert_eq!(&src[tsql_run_range(src, &split_in(src, Dialect::TSql)[0])], src);
    }
}
