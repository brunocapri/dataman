//! The agent SQL analyzer reads one text in several places: the splitters (`bounded`, the guard's
//! pieces, `split_in`), the read gate, the write classes, the preview, the allowlist and the
//! privacy rewrite. Where two of them read a statement differently, one layer lets through what
//! another would have stopped: these properties hold them to one reading.

mod sql_corpus;

use std::collections::HashSet;

use dataman_core::agent_sql::{self, Context, Table};
use dataman_core::bounded::{self, Mode};
use dataman_core::guard::{self, Effect, Preview, WriteClass};
use dataman_core::privacy::{AgentRules, AgentView, ItemKind, SensitiveCol, Visible, VisibleItem, VisibleMode, Mask};
use dataman_core::sql::{Dialect, TokenKind, split_in, tokenize_in};
use sql_corpus::*;

fn rank(c: &WriteClass) -> u8 {
    match c {
        WriteClass::Read => 0,
        WriteClass::SafeWrite => 1,
        WriteClass::Destructive { .. } => 2,
    }
}

/// What a whole text is to the agent layers, to compare readings of it.
#[derive(Debug, PartialEq, Eq)]
struct Reading {
    statements: usize,
    pieces: usize,
    class: u8,
    reads: bool,
    preview: Preview,
}

fn reading(sql: &str, d: Dialect) -> Reading {
    let stmts = bounded::statements_in(sql, d);
    Reading {
        statements: stmts.len(),
        pieces: guard::agent_pieces(sql, d).len(),
        class: rank(&guard::agent_write_class(sql, d)),
        reads: bounded::refusal_in(&stmts, 0, Mode::Read, false, d).is_none(),
        preview: guard::agent_preview(sql, d),
    }
}

fn effect(sql: &str, d: Dialect) -> Effect {
    guard::classify_in(sql, d).effect
}

/// Every check of one text; the failures, not a panic, so a run lists them all.
fn check(sql: &str, d: Dialect, out: &mut Vec<String>) {
    let mut fail = |what: &str| out.push(format!("[{d:?}] {sql:?}: {what}"));
    let stmts = bounded::statements_in(sql, d);
    let pieces = guard::agent_pieces(sql, d);

    // Every splitter counts the same statements, with the same bounds.
    let ranges: Vec<String> = split_in(sql, d).into_iter().map(|s| sql[s.range].trim().to_string()).filter(|s| !s.is_empty()).collect();
    if ranges != stmts {
        fail(&format!("split_in {ranges:?} vs statements_in {stmts:?}"));
    }
    let expected: Vec<String> = if d == Dialect::TSql {
        stmts.iter().flat_map(|s| guard::tsql_statements(s).into_iter().map(move |r| s[r].to_string())).collect()
    } else {
        stmts.clone()
    };
    let trimmed: Vec<String> = pieces.iter().map(|p| p.trim().to_string()).collect();
    if trimmed != expected {
        fail(&format!("agent_pieces {trimmed:?} vs statements {expected:?}"));
    }
    for p in &pieces {
        let again = guard::agent_pieces(p, d);
        if again.len() != 1 || again[0].trim() != p.trim() {
            fail(&format!("piece {p:?} splits again into {again:?}"));
        }
    }
    if guard::pieces_class(&pieces, d) != guard::agent_write_class(sql, d) {
        fail("pieces_class of its pieces is not its agent_write_class");
    }

    // The read gate and the write classes agree.
    let class = guard::agent_write_class(sql, d);
    let reads = bounded::refusal_in(&stmts, 0, Mode::Read, false, d).is_none();
    if reads && class != WriteClass::Read {
        fail(&format!("the read gate takes it, agent_write_class says {class:?}"));
    }
    for s in &stmts {
        if guard::agent_read_refusal_in(s, d).is_none() && guard::agent_write_class(s, d) != WriteClass::Read {
            fail(&format!("statement {s:?}: read gate takes it, agent_write_class says {:?}", guard::agent_write_class(s, d)));
        }
    }
    for p in &pieces {
        let f = guard::classify_in(p, d);
        let neutral = matches!(f.verb.as_str(), "SET" | "DECLARE" | "PRINT");
        if guard::agent_write_class(p, d) == WriteClass::Read && !neutral && guard::agent_read_refusal_in(p, d).is_some() {
            fail(&format!("piece {p:?}: agent_write_class Read, the read gate refuses it"));
        }
    }

    // A whole text is its strongest statement.
    let strongest = pieces.iter().map(|p| rank(&guard::agent_write_class(p, d))).max().unwrap_or(0);
    if rank(&class) != strongest {
        fail(&format!("agent_write_class {class:?}, strongest piece rank {strongest}"));
    }
    let preview = pieces.iter().map(|p| guard::agent_preview(p, d)).max().unwrap_or(Preview::Held);
    if guard::agent_preview(sql, d) != preview {
        fail(&format!("agent_preview {:?}, strongest piece {preview:?}", guard::agent_preview(sql, d)));
    }
    let strongest = stmts.iter().map(|s| effect(s, d)).max().unwrap_or_default();
    if effect(sql, d) != strongest {
        fail(&format!("classify_in effect {:?}, strongest statement {strongest:?}", effect(sql, d)));
    }
    for s in &stmts {
        let parts: Vec<String> = guard::agent_pieces(s, d);
        let strongest = parts.iter().map(|p| effect(p, d)).max().unwrap_or_default();
        if effect(s, d) != strongest {
            fail(&format!("statement {s:?}: classify_in effect {:?}, strongest piece {strongest:?}", effect(s, d)));
        }
        if guard::agent_read_refusal_in(s, d).is_none() && parts.iter().any(|p| guard::agent_read_refusal_in(p, d).is_some()) {
            fail(&format!("statement {s:?}: the read gate takes it, not each of its pieces"));
        }
    }

    // MySQL runs what `/*! … */` holds: the text with those comments opened reads the same.
    if d == Dialect::MySql {
        let open = dataman_core::sql::expand_exec_comments(sql, d).into_owned();
        if open != sql && !gated(sql) && reading(&open, d) != reading(sql, d) {
            fail(&format!("reads {:?}, with its /*! */ opened {:?}", reading(sql, d), reading(&open, d)));
        }
    }
}

fn takes_batch(sql: &str, d: Dialect) -> bool {
    let w = sig(sql, d);
    d == Dialect::TSql
        && w.first().is_some_and(|x| x.eq_ignore_ascii_case("CREATE") || x.eq_ignore_ascii_case("ALTER"))
        && w.get(1).is_some_and(|x| ["PROC", "PROCEDURE", "FUNCTION", "TRIGGER", "VIEW"].iter().any(|k| x.eq_ignore_ascii_case(k)))
}

/// A `/*!NNNNN … */` or `/*M! … */` comment: some servers run it, others skip it.
fn gated(sql: &str) -> bool {
    tokenize_in(sql, Dialect::MySql).iter().any(|t| {
        let s = t.text(sql);
        t.kind == TokenKind::BlockComment && (s.starts_with("/*M!") || s.starts_with("/*!") && s[3..].starts_with(|c: char| c.is_ascii_digit()))
    })
}

fn report(fails: Vec<String>) {
    let mut seen = HashSet::new();
    let fails: Vec<String> = fails.into_iter().filter(|f| seen.insert(f.clone())).collect();
    assert!(fails.is_empty(), "{} divergences:\n{}", fails.len(), fails.iter().take(60).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn every_layer_reads_the_corpus_alike() {
    let mut fails = Vec::new();
    for d in DIALECTS {
        for t in texts(d) {
            check(&t, d, &mut fails);
            for v in variations(&t, d) {
                check(&v, d, &mut fails);
            }
        }
    }
    report(fails);
}

#[test]
fn variations_read_the_same() {
    let mut fails = Vec::new();
    for d in DIALECTS {
        for t in texts(d) {
            let base = reading(&t, d);
            for v in variations(&t, d) {
                let r = reading(&v, d);
                if r != base {
                    fails.push(format!("[{d:?}] {t:?} reads {base:?}; as {v:?} it reads {r:?}"));
                }
            }
        }
    }
    report(fails);
}

#[test]
fn joined_statements_read_as_both() {
    let mut fails = Vec::new();
    for d in DIALECTS {
        let all = corpus(d);
        for a in all {
            for b in all {
                let (ra, rb) = (reading(a, d), reading(b, d));
                for j in joins(a, b, d) {
                    check(&j, d, &mut fails);
                    let r = reading(&j, d);
                    let both = Reading {
                        statements: ra.statements + rb.statements,
                        pieces: ra.pieces + rb.pieces,
                        class: ra.class.max(rb.class),
                        reads: ra.reads && rb.reads,
                        preview: ra.preview.max(rb.preview),
                    };
                    // A T-SQL routine's body runs to the end of its batch, on the server too.
                    if takes_batch(a, d) {
                        if r.class < both.class || r.reads {
                            fails.push(format!("[{d:?}] {j:?} reads {r:?}; its parts {both:?}"));
                        }
                    } else if r != both {
                        fails.push(format!("[{d:?}] {j:?} reads {r:?}; its parts {both:?}"));
                    }
                }
                // T-SQL needs no `;`: what follows a statement can never make it weaker.
                if d == Dialect::TSql {
                    let j = format!("{a} {b}");
                    let r = reading(&j, d);
                    if r.class < ra.class.max(rb.class) || r.reads && !(ra.reads && rb.reads) {
                        fails.push(format!("[{d:?}] {j:?} reads {r:?}; its parts {ra:?} and {rb:?}"));
                    }
                }
            }
        }
    }
    report(fails);
}

/// A comment some servers run and others skip: Dataman reads the stronger of both texts.
#[test]
fn mysql_versioned_comments_read_as_the_stronger_text() {
    let mut fails = Vec::new();
    let d = Dialect::MySql;
    for t in MYSQL {
        let toks: Vec<_> = tokenize_in(t, d).into_iter().filter(|x| !x.kind.is_trivia()).collect();
        if t.contains("/*") || t.contains("--") || t.contains('#') {
            continue;
        }
        for k in 1..toks.len() {
            let (head, tail) = (&t[..toks[k].start], &t[toks[k].start..]);
            for open in ["/*!", "/*!99999", "/*!50000", "/*M!"] {
                let wrapped = format!("{head}{open} {tail} */");
                check(&wrapped, d, &mut fails);
                let w = reading(&wrapped, d);
                let (run, skip) = (reading(t, d), reading(head, d));
                if open == "/*!" {
                    if w != run {
                        fails.push(format!("{wrapped:?} reads {w:?}; MySQL runs {t:?}, which reads {run:?}"));
                    }
                } else if w.class < run.class.max(skip.class) || w.reads && !(run.reads && skip.reads) {
                    fails.push(format!("{wrapped:?} reads {w:?}; run {run:?}, skipped {skip:?}"));
                }
            }
        }
    }
    report(fails);
}

#[test]
fn allowlist_entries_take_one_value_per_mark() {
    let mut fails = Vec::new();
    for d in DIALECTS {
        for s in corpus(d) {
            if guard::agent_write_class(s, d) == WriteClass::Read || guard::agent_pieces(s, d).len() != 1 {
                continue;
            }
            let Ok(entry) = guard::allowlist_entry(&entry_of(s, d), d) else { continue };
            let marks = entry.matches('?').count();
            let filled = fill(&entry, "1");
            let class = rank(&guard::agent_write_class(&filled, d));
            let verb = guard::classify_in(&filled, d).verb;
            let opened = |x: &str| dataman_core::sql::expand_exec_comments(x, d).into_owned();
            let mut candidates: Vec<String> = vec![filled.clone(), fill(&entry, "-1"), fill(&entry, "'x'")];
            candidates.extend(variations(&filled, d));
            candidates.extend(impostors(&entry, d));
            candidates.extend(corpus(d).iter().map(|x| x.to_string()));
            for c in &candidates {
                if !guard::allowlist_match(&entry, c, d) {
                    if c == &filled {
                        fails.push(format!("[{d:?}] entry {entry:?} does not take {c:?}"));
                    }
                    continue;
                }
                let (n, e) = (sig(opened(c).trim_end().trim_end_matches(';'), d).len(), sig(&entry, d).len());
                if n < e || n > e + marks || gated(c) && d == Dialect::MySql {
                    fails.push(format!("[{d:?}] entry {entry:?} takes {c:?}: {n} tokens for {e}"));
                }
                if guard::classify_in(c, d).verb != verb {
                    fails.push(format!("[{d:?}] entry {entry:?} takes {c:?}, a {}", guard::classify_in(c, d).verb));
                }
                if rank(&guard::agent_write_class(c, d)) > class {
                    fails.push(format!("[{d:?}] entry {entry:?} takes {c:?}, which is {:?}", guard::agent_write_class(c, d)));
                }
            }
            for v in variations(&filled, d) {
                if !guard::allowlist_match(&entry, &v, d) && sig(&v, d).len() == sig(&filled, d).len() {
                    fails.push(format!("[{d:?}] entry {entry:?} takes {filled:?}, not {v:?}"));
                }
            }
        }
    }
    report(fails);
}

fn rules(view: AgentView) -> AgentRules {
    AgentRules {
        view,
        visible: Visible { mode: VisibleMode::Except, items: vec![VisibleItem { name: "*.secret".into(), kind: ItemKind::Column }] },
        sensitive: vec![SensitiveCol { col: "ssn".into(), how: Mask::Partial, on: true }],
        ..Default::default()
    }
}

/// The privacy rewrite keeps the statements it was given, and gives the read gate nothing new to
/// refuse.
#[test]
fn privacy_rewrite_keeps_statements() {
    let mut fails = Vec::new();
    let routines = HashSet::new();
    let tables = |schema: Option<&str>, name: &str| {
        Some(Table { schema: schema.unwrap_or("public").to_string(), name: name.to_string(), columns: ["id", "a", "name", "email", "ssn", "secret"].map(String::from).to_vec() })
    };
    for view in [AgentView::Data, AgentView::Masked] {
        let r = rules(view);
        for d in DIALECTS {
            let cx = Context { rules: &r, connection: "c", dialect: d, tables: &tables, routines: &routines };
            let mut texts = texts(d);
            texts.extend(["SELECT * FROM users", "SELECT u.* FROM users u JOIN t ON t.id = u.id; SELECT 2", "SELECT ssn FROM users WHERE id = 1"].map(String::from));
            for t in texts {
                // Reads are checked as reads once the read gate took them; everything else as a write.
                let read = bounded::refusal_in(&bounded::statements_in(&t, d), 0, Mode::Read, false, d).is_none();
                for write in [false, true] {
                    if !write && !read {
                        continue;
                    }
                    let Ok(c) = agent_sql::check(&t, write, &cx) else { continue };
                    let (before, after) = (bounded::statements_in(&t, d), bounded::statements_in(&c.sql, d));
                    if before.len() != after.len() || guard::agent_pieces(&t, d).len() != guard::agent_pieces(&c.sql, d).len() {
                        fails.push(format!("[{d:?} {view:?}] {t:?} became {:?}: {} statements, then {}", c.sql, before.len(), after.len()));
                    }
                    let reads = |s: &[String]| bounded::refusal_in(s, 0, Mode::Read, false, d).is_none();
                    if !write && reads(&before) && !reads(&after) {
                        fails.push(format!("[{d:?} {view:?}] {t:?} became {:?}, which the read gate refuses: {:?}", c.sql, bounded::refusal_in(&after, 0, Mode::Read, false, d)));
                    }
                    if rank(&guard::agent_write_class(&t, d)) != rank(&guard::agent_write_class(&c.sql, d)) {
                        fails.push(format!("[{d:?} {view:?}] {t:?} became {:?}: {:?}, then {:?}", c.sql, guard::agent_write_class(&t, d), guard::agent_write_class(&c.sql, d)));
                    }
                }
            }
        }
    }
    report(fails);
}

/// What each server was seen to do (PostgreSQL 18, MySQL 8.4, SQL Server 2022), and the cases
/// the properties above found: the class and the read gate's answer they must keep.
const REGRESSIONS: &[(Dialect, &str, u8, bool)] = &[
    // A text is its strongest statement.
    (Dialect::Postgres, "SELECT 1; DELETE FROM t", 2, false),
    // MySQL runs what `/*! … */` holds; versioned ones are read run and skipped.
    (Dialect::MySql, "SELECT 1 /*!; DELETE FROM t */", 2, false),
    (Dialect::MySql, "SELECT 1 /*! FOR UPDATE */", 1, false),
    (Dialect::MySql, "SELECT /*! GET_LOCK('x', 1) */", 2, false),
    (Dialect::MySql, "DELETE FROM t WHERE id = 1 /*! OR 1=1 */", 2, false),
    (Dialect::MySql, "DELETE FROM t /*!99999 WHERE id = 1 */", 2, false),
    (Dialect::MySql, "/*!50000 DROP TABLE t */", 2, false),
    (Dialect::MySql, "SELECT /*!40001 SQL_NO_CACHE */ 1", 0, true),
    // What follows a read in a T-SQL batch is read on its own.
    (Dialect::TSql, "SELECT 1 EXEC sp_help 't'", 2, false),
    (Dialect::TSql, "SELECT 1 DECLARE @x int", 0, false),
    // SQL Server's spaces: every C0 control but NUL, NEL, NBSP and Unicode's spaces.
    (Dialect::TSql, "SELECT 1\u{a0}DELETE FROM t", 2, false),
    (Dialect::TSql, "UPDATE t SET a = 1 WHERE id = 2\u{2003}DROP\u{2003}TABLE\u{2003}t", 2, false),
    (Dialect::TSql, "SELECT 1\u{200b}DELETE\u{3000}FROM t", 2, false),
    (Dialect::TSql, "SELECT 1\u{1c}DELETE\u{85}FROM\u{2028}t", 2, false),
    (Dialect::TSql, "SELECT\u{a0}1", 0, true),
    // `\v` is a space everywhere.
    (Dialect::Postgres, "EXPLAIN\u{b}ANALYZE DELETE FROM t", 2, false),
    (Dialect::MySql, "SELECT\u{b}1", 0, true),
    // A bare `\r` ends a `--` comment on PostgreSQL and SQL Server, not on MySQL.
    (Dialect::Postgres, "DELETE FROM t WHERE id = 5 --\rOR true\n AND true", 2, false),
    (Dialect::TSql, "SELECT 1 --\rDELETE FROM t\n", 2, false),
    (Dialect::MySql, "SELECT 1 --\rDELETE FROM t\n", 0, true),
    (Dialect::MySql, "SELECT 1 #\rDELETE FROM t\n", 0, true),
];

#[test]
fn regressions_keep_their_reading() {
    let mut fails = Vec::new();
    for &(d, sql, class, reads) in REGRESSIONS {
        check(sql, d, &mut fails);
        let r = reading(sql, d);
        if (r.class, r.reads) != (class, reads) {
            fails.push(format!("[{d:?}] {sql:?} reads {r:?}; expected class {class}, reads {reads}"));
        }
    }
    for (d, entry, stmt) in [
        (Dialect::MySql, "DELETE FROM t WHERE id = ?", "DELETE FROM t WHERE id = 1 /*! OR 1=1 */"),
        (Dialect::MySql, "DELETE FROM t WHERE id = ?", "DELETE FROM t /*!99999 WHERE id = 1 */"),
        (Dialect::Postgres, "DELETE FROM t WHERE id = ? AND a = ?", "DELETE FROM t WHERE id = 5 --\rOR true\n AND a = 1"),
        (Dialect::TSql, "UPDATE t SET a = ? WHERE id = ?", "UPDATE t SET a = 1 WHERE id = 2\u{a0}DROP TABLE t"),
    ] {
        if guard::allowlist_match(entry, stmt, d) {
            fails.push(format!("entry {entry:?} takes {stmt:?}"));
        }
    }
    let d = Dialect::MySql;
    // Masked data: a sensitive column renamed inside `/*! … */` is still renamed.
    let r = rules(AgentView::Masked);
    let routines = HashSet::new();
    let tables = |_: Option<&str>, name: &str| Some(Table { schema: "app".into(), name: name.into(), columns: ["id", "ssn", "secret"].map(String::from).to_vec() });
    let cx = Context { rules: &r, connection: "c", dialect: d, tables: &tables, routines: &routines };
    for sql in ["SELECT /*! ssn AS x */ FROM users", "SELECT id /*!99999 , ssn AS x */ FROM users", "SELECT id /*! , secret */ FROM users"] {
        if let Ok(c) = agent_sql::check(sql, false, &cx) {
            fails.push(format!("privacy takes {sql:?} as {:?}", c.sql));
        }
    }
    report(fails);
}
