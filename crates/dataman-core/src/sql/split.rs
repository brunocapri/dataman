use std::ops::Range;

use super::dialect::{Dialect, ScriptRules};
use super::lexer::{Token, TokenKind, tokenize, tokenize_in};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub range: Range<usize>,
    pub full: Range<usize>,
}

pub fn split(src: &str) -> Vec<Statement> {
    split_tokens(src, &tokenize(src))
}

/// MySQL's statements are cut where the server cuts them, inside `/*! … */` too; a statement
/// inside such a comment keeps the comment around it (and runs only where the server runs it).
pub fn split_in(src: &str, dialect: Dialect) -> Vec<Statement> {
    let open = super::expand_exec_comments(src, dialect);
    let mut out = split_tokens_in(&open, &tokenize_in(&open, dialect), dialect.script());
    if open != src {
        for t in tokenize_in(src, dialect).into_iter().filter(|t| t.kind == TokenKind::BlockComment && is_exec_comment(t.text(src))) {
            let mut inside = out.iter_mut().filter(|s| s.range.start < t.end && t.start < s.range.end);
            if let (Some(s), None) = (inside.next(), inside.next()) {
                s.range = s.range.start.min(t.start)..s.range.end.max(t.end);
                s.full = s.full.start.min(t.start)..s.full.end.max(t.end);
            }
        }
    }
    out
}

pub fn split_tokens(src: &str, tokens: &[Token]) -> Vec<Statement> {
    split_tokens_in(src, tokens, Dialect::Postgres.script())
}

fn delimiter_command(src: &str, t: &Token) -> Option<(String, usize)> {
    if !matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) || !t.text(src).eq_ignore_ascii_case("DELIMITER") {
        return None;
    }
    let line_end = src[t.end..].find('\n').map_or(src.len(), |p| t.end + p);
    let delim = src[t.end..line_end].split_whitespace().next()?;
    Some((delim.to_string(), line_end))
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoBatch {
    pub range: Range<usize>,
    pub repeat: u32,
    pub end: usize,
}

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

/// A batch repeated by `GO n` keeps its `GO n` line, which the driver reads back.
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

pub fn may_return_more(src: &str) -> bool {
    let b = go_batches(src);
    b.len() > 1 || b.iter().any(|b| b.repeat > 1) || b.first().is_some_and(|b| crate::guard::tsql_statements(&src[b.range.clone()]).len() > 1)
}

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

fn begins_block(next: Option<&str>) -> bool {
    !next.is_some_and(|w| {
        ["TRAN", "TRANSACTION", "DISTRIBUTED", "DIALOG", "CONVERSATION"].iter().any(|k| w.eq_ignore_ascii_case(k))
    })
}

fn creates_routine(src: &str, sig: &[&Token], i: usize) -> bool {
    for n in sig.iter().skip(i + 1).take(14) {
        if n.kind == TokenKind::Punct && n.text(src) == ";" {
            return false;
        }
        if matches!(n.kind, TokenKind::Keyword | TokenKind::Ident) {
            let w = n.text(src);
            if ["PROCEDURE", "FUNCTION", "TRIGGER", "EVENT"].iter().any(|k| w.eq_ignore_ascii_case(k)) {
                return true;
            }
            if ["TABLE", "VIEW", "INDEX", "DATABASE", "SCHEMA", "USER", "ROLE", "SERVER", "TABLESPACE", "UNIQUE", "FULLTEXT", "SPATIAL", "TEMPORARY", "SEQUENCE", "LOGFILE", "RESOURCE"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
            {
                return false;
            }
        }
    }
    false
}

fn if_is_statement(src: &str, sig: &[&Token], i: usize) -> bool {
    let w = |k: usize, x: &str| sig.get(k).is_some_and(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(src).eq_ignore_ascii_case(x));
    if w(i + 1, "NOT") || w(i + 1, "EXISTS") {
        return false;
    }
    if !sig.get(i + 1).is_some_and(|t| t.text(src) == "(") {
        return true;
    }
    let mut depth = 0;
    for (k, t) in sig.iter().enumerate().skip(i + 1) {
        match t.text(src) {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return w(k + 1, "THEN");
                }
            }
            _ => {}
        }
    }
    false
}

pub fn is_exec_comment(text: &str) -> bool {
    text.starts_with("/*!") || text.starts_with("/*M!")
}

pub fn split_tokens_in(src: &str, tokens: &[Token], rules: ScriptRules) -> Vec<Statement> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    // CASE … END pairs are tracked too, so their END does not close the body.
    let mut blocks: Vec<bool> = Vec::new();
    let mut to_batch_end = false;
    let mut routine = false;
    let mut first: Option<usize> = None;
    let mut last = 0;
    let mut delimiter: Option<String> = None;
    let mut skip_until = 0;
    let exec_comment = |t: &Token| rules.exec_comments && t.kind == TokenKind::BlockComment && is_exec_comment(t.text(src));
    let sig: Vec<&Token> = tokens.iter().filter(|t| !t.kind.is_trivia() || exec_comment(t)).collect();
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
            if p > 0 {
                first.get_or_insert(t.start);
                last = t.start + p;
            }
            if let Some(f) = first.take() {
                out.push(Statement { range: f..last, full: f..t.start + p + d.len() });
            }
            skip_until = t.start + p + d.len();
            routine = false;
            blocks.clear();
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
        if rules.routine_bodies && first.is_none() && word(t, "CREATE") {
            routine = creates_routine(src, &sig, i);
        }
        if routine && delimiter.is_none() {
            let after_end = i.checked_sub(1).is_some_and(|p| word(sig[p], "END"));
            let opens = word(t, "BEGIN")
                || word(t, "CASE")
                || word(t, "WHILE")
                || word(t, "LOOP")
                || (word(t, "IF") && if_is_statement(src, &sig, i))
                || (word(t, "REPEAT") && !sig.get(i + 1).is_some_and(|n| n.text(src) == "("));
            if !after_end && opens {
                blocks.push(true);
            } else if word(t, "END") && !blocks.is_empty() {
                blocks.pop();
            }
        } else if rules.begin_atomic && word(t, "BEGIN") && sig.get(i + 1).is_some_and(|n| word(n, "ATOMIC")) {
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
                    routine = false;
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
        let src = "delimiter $$\nCREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; END$$\nSELECT 1$$\nDELIMITER ;;\nSELECT 2;; SELECT 3;;";
        let got: Vec<&str> = split_in(src, Dialect::MySql).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(got, vec!["CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; END", "SELECT 1", "SELECT 2", "SELECT 3"]);
        let s = split_in("DELIMITER //\nSELECT 1 //", Dialect::MySql);
        assert_eq!((s.len(), &"DELIMITER //\nSELECT 1 //"[s[0].full.clone()]), (1, "SELECT 1 //"));
        assert_eq!(split_in("SELECT delimiter FROM t; SELECT 2", Dialect::MySql).len(), 2);
        assert_eq!(split("DELIMITER //\nSELECT 1; SELECT 2").len(), 2);
    }

    #[test]
    fn mysql_executable_comments_are_statements() {
        let src = "/*!40101 SET NAMES utf8mb4 */;\n/* plain */;\n-- note\n/*!40014 SET FOREIGN_KEY_CHECKS=0 */;\nSELECT /*!40001 SQL_NO_CACHE */ 1;\n/*M!100000 SET x = 1 */";
        let got: Vec<&str> = split_in(src, Dialect::MySql).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(got, vec!["/*!40101 SET NAMES utf8mb4 */", "/*!40014 SET FOREIGN_KEY_CHECKS=0 */", "SELECT /*!40001 SQL_NO_CACHE */ 1", "/*M!100000 SET x = 1 */"]);
        assert_eq!(split_in("/*!40101 SET x */;", Dialect::Postgres).len(), 0);
        let f = crate::guard::classify_in("/*!50000 DROP TABLE t */", Dialect::MySql);
        assert_eq!((f.verb.as_str(), f.effect), ("DROP TABLE", crate::guard::Effect::Ddl));
        assert!(crate::guard::read_only_refusal_in("/*!50000 DELETE FROM t */", Dialect::MySql).is_some());
        assert!(crate::guard::read_only_refusal_in("SELECT /*!40001 SQL_NO_CACHE */ 1", Dialect::MySql).is_none());
    }

    #[test]
    fn mysql_bare_compound_bodies() {
        let src = "CREATE PROCEDURE a(x INT) IF x > 1 THEN SELECT 1; SELECT 2; ELSE SELECT 3; END IF;\n\
            CREATE PROCEDURE b(x INT) IF (x > 1) THEN SELECT IF(x, 1, 2); END IF;\n\
            CREATE PROCEDURE c() l: LOOP SELECT 1; LEAVE l; END LOOP l;\n\
            CREATE PROCEDURE d(x INT) WHILE x < 3 DO SET x = x + 1; SELECT REPEAT('a', 2); END WHILE;\n\
            CREATE PROCEDURE e(x INT) REPEAT SET x = x - 1; UNTIL x < 0 END REPEAT;\n\
            CREATE PROCEDURE f(x INT) CASE x WHEN 1 THEN SELECT 1; ELSE SELECT 2; END CASE;\n\
            CREATE FUNCTION g(x INT) RETURNS INT RETURN IF(x > 1, 1, 2);\n\
            SELECT 9";
        let got: Vec<&str> = split_in(src, Dialect::MySql).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(got.len(), 8, "{got:#?}");
        assert!(got[0].ends_with("END IF") && got[2].ends_with("END LOOP l") && got[5].ends_with("END CASE") && got[6].ends_with("IF(x > 1, 1, 2)"));
    }

    #[test]
    fn mysql_routine_bodies_without_delimiter() {
        let src = "CREATE PROCEDURE p() BEGIN DECLARE i INT DEFAULT 0; w: WHILE i < 3 DO SET i = i + 1; END WHILE w; REPEAT SET i = i - 1; UNTIL i <= 0 END REPEAT; \
            CASE WHEN i > 0 THEN SET i = 1; ELSE SET i = 2; END CASE; IF i THEN SELECT CASE WHEN i THEN 1 END; END IF; l: LOOP LEAVE l; END LOOP l; END;\n\
            CREATE FUNCTION f() RETURNS INT RETURN CASE WHEN 1 THEN 1 ELSE 2 END;\n\
            CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; SET NEW.b = 2; END;\n\
            CREATE DEFINER=`root`@`%` EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN DELETE FROM x; END;\n\
            CREATE TABLE trigger_log (event INT, begin_at INT); BEGIN; SELECT 1; COMMIT; CREATE VIEW v AS SELECT CASE WHEN 1 THEN 2 END AS c; SELECT 9";
        let got: Vec<&str> = split_in(src, Dialect::MySql).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(got.len(), 10, "{got:#?}");
        assert!(got[0].ends_with("END LOOP l; END") && got[1].ends_with("END") && got[2].ends_with("SET NEW.b = 2; END") && got[3].ends_with("DELETE FROM x; END"));
        assert_eq!(&got[4..], ["CREATE TABLE trigger_log (event INT, begin_at INT)", "BEGIN", "SELECT 1", "COMMIT", "CREATE VIEW v AS SELECT CASE WHEN 1 THEN 2 END AS c", "SELECT 9"]);
        let src = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; END//\nSELECT 2//\nDELIMITER ;\nSELECT 3; SELECT 4";
        assert_eq!(split_in(src, Dialect::MySql).len(), 4);
    }

    #[test]
    fn tsql_batches_and_statements() {
        let d = Dialect::TSql;
        let src = "CREATE TABLE #t (a int)\nGO\n-- GO inside a comment\nINSERT #t VALUES (1); /* GO\n*/ SELECT 'GO\nGO'\n  go  \nSELECT 1\nGO 3\nSELECT [GO] FROM x\nGO -- the end";
        let b = go_batches(src);
        let texts: Vec<&str> = b.iter().map(|b| &src[b.range.clone()]).collect();
        assert_eq!(texts, vec!["CREATE TABLE #t (a int)", "INSERT #t VALUES (1); /* GO\n*/ SELECT 'GO\nGO'", "SELECT 1", "SELECT [GO] FROM x"]);
        assert_eq!(b.iter().map(|b| b.repeat).collect::<Vec<_>>(), vec![1, 1, 3, 1]);
        let units: Vec<&str> = batches_in(src, d).into_iter().map(|s| &src[s.range]).collect();
        assert_eq!(units[2], "SELECT 1\nGO 3");
        assert_eq!(go_batches(&units[2])[0].repeat, 3);
        assert_eq!(batches_in("select 1; select 2", Dialect::Postgres).len(), 2);
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
        assert_eq!(go_batches("SELECT 1 GO\nSELECT 2").len(), 1);
        assert_eq!(go_batches("SELECT 1\nGO x\nSELECT 2").len(), 1);
        assert_eq!(go_batches("SELECT 1\r\nGO\r\nSELECT 2").len(), 2);
        assert!(go_batches("-- only\nGO\n/* comments */").is_empty());
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
        assert_eq!(at(9), Some("select 1"));
        assert_eq!(at(10), Some("select 2"));
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
        assert_eq!(at("SELECT *"), "DECLARE @d date = '2026-01-01';\nSET @d = DATEADD(day, 1, @d);\nSELECT * FROM t WHERE c > @d AND id = @id");
        assert_eq!(at("SELECT 2"), "SELECT 2");
        let src = "SELECT * FROM t WHERE id = @id";
        assert_eq!(&src[tsql_run_range(src, &split_in(src, Dialect::TSql)[0])], src);
    }
}
