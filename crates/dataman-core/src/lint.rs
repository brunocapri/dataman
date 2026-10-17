//! Checks before a statement runs, shown in the editor as it is typed (debounced, off the
//! keystroke path): unknown tables and columns against the cached schema, ambiguous columns,
//! `JOIN` without `ON`, unterminated strings and comments, and `UPDATE`/`DELETE` without
//! `WHERE` (the guard's rule, as a warning).
//!
//! False positives are worse than misses: the schema rules only speak when the schema is loaded
//! and the reference is fully understood. They stay quiet on names the script or the session
//! creates (temp tables), CTEs, function sources, anything after `SET search_path`, LAR's
//! catalog gaps, names the reader does not follow, and every statement that is not plain DML.
//! A statement with anything the reader does not model (`Analysis::unsure`: APPLY, PIVOT,
//! MERGE, OUTPUT, control flow, table variables, several statements without `;`…) gets no
//! schema findings at all (fail-open); SQL Server's syntax is checked by the server instead
//! (`SET PARSEONLY`, `Connection::parse_check`).

use std::collections::HashSet;
use std::ops::Range;

use crate::schema::SchemaSnapshot;
use crate::sql::lexer::{Token, TokenKind, tokenize, tokenize_in, unterminated_in};
use crate::sql::scope::{self, Analysis, Clause, Item, SYSTEM_COLUMNS, SourceKind};
use crate::sql::{Dialect, split::split_tokens_in};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lint {
    pub range: Range<usize>,
    pub severity: Severity,
    pub message: String,
}

#[derive(Default)]
pub struct LintContext<'a> {
    /// The cached schema; `None` (or not loaded yet) turns the schema rules off.
    pub schema: Option<&'a SchemaSnapshot>,
    /// A LAR server: its catalog does not list everything, so unknown tables are not reported.
    pub lar: bool,
    /// Relations the session created that the cache may not know (temp tables), lower case.
    pub known: Option<&'a HashSet<String>>,
}

pub fn lint(src: &str, cx: &LintContext) -> Vec<Lint> {
    let dialect = cx.schema.map(|s| s.dialect).unwrap_or_default();
    let tokens = tokenize_in(src, dialect);
    let mut out = Vec::new();
    for t in &tokens {
        if let Some(what) = unterminated_in(src, t, dialect) {
            // From the opening quote to the end of its line.
            let end = src[t.start..].find('\n').map_or(src.len(), |n| t.start + n).max(t.start + 1).min(src.len());
            out.push(Lint { range: t.start..end, severity: Severity::Error, message: format!("unterminated {what}") });
        }
    }
    let stmts = split_tokens_in(src, &tokens, dialect.script());
    let created = created_in(src, &tokens, dialect);
    // The script may change where unqualified names resolve.
    let search_path_set = src.to_ascii_lowercase().contains("search_path");
    for s in &stmts {
        let from = tokens.partition_point(|t| t.start < s.range.start);
        let toks: Vec<Token> = tokens[from..].iter().take_while(|t| t.start < s.range.end).filter(|t| !t.kind.is_trivia() && t.end <= s.range.end).copied().collect();
        let toks = if dialect == Dialect::TSql { without_tsql_hints(src, toks) } else { toks };
        let Some(first) = toks.first() else { continue };
        let verb = first.text(src).to_ascii_uppercase();
        let facts = crate::guard::classify_in(&src[s.range.clone()], dialect);
        if facts.no_where
            && let Some(t) = toks.iter().enumerate().find(|(i, t)| {
                let w = t.text(src);
                (w.eq_ignore_ascii_case("UPDATE") || w.eq_ignore_ascii_case("DELETE"))
                    && !i.checked_sub(1).is_some_and(|p| ["FOR", "DO", "KEY"].iter().any(|k| toks[p].text(src).eq_ignore_ascii_case(k)))
            })
        {
            let w = t.1.text(src).to_ascii_uppercase();
            out.push(Lint { range: t.1.range(), severity: Severity::Warning, message: format!("{w} without WHERE") });
        }
        let a = scope::analyze_in(src, &toks, dialect);
        for r in &a.missing_on {
            out.push(Lint { range: r.clone(), severity: Severity::Error, message: "JOIN without ON".into() });
        }
        let dml = ["SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "VALUES"].contains(&verb.as_str());
        // Fail-open: a statement the reader did not fully follow gets no schema findings.
        if let Some(snap) = cx.schema
            && dml
            && !a.unsure
            && !a.scopes.is_empty()
        {
            let known = |n: &str| created.contains(&n.to_lowercase()) || cx.known.is_some_and(|k| k.contains(&n.to_lowercase()));
            if !cx.lar && !search_path_set {
                unknown_tables(&a, snap, &known, &mut out);
            }
            columns(&a, snap, cx.lar, &mut out);
        }
    }
    out.sort_by(|a, b| a.range.start.cmp(&b.range.start).then(a.range.end.cmp(&b.range.end)));
    out.dedup_by(|a, b| a.range == b.range && a.message == b.message);
    out
}

/// A T-SQL statement's tokens without its query and table hints (`OPTION (MAXRECURSION 100)`,
/// `orders p WITH (NOLOCK, INDEX(ix))`, OPENJSON's `WITH (…)` schema): their words are options,
/// not columns. A CTE's `WITH name AS (` is not followed by a parenthesis.
fn without_tsql_hints(src: &str, toks: Vec<Token>) -> Vec<Token> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        let w = toks[i].text(src);
        let hint = w.eq_ignore_ascii_case("OPTION") || (i > 0 && w.eq_ignore_ascii_case("WITH"));
        if hint && toks.get(i + 1).is_some_and(|t| t.text(src) == "(") {
            let mut depth = 0;
            let mut j = i + 1;
            while j < toks.len() {
                match toks[j].text(src) {
                    "(" => depth += 1,
                    ")" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        out.push(toks[i]);
        i += 1;
    }
    out
}

/// Tables and views a script creates (`CREATE [TEMP] TABLE x`, `CREATE VIEW x`,
/// `SELECT … INTO x`, `ALTER … RENAME TO x`), lower case.
pub fn created_names(src: &str) -> HashSet<String> {
    created_in(src, &tokenize(src), Dialect::Postgres)
}

/// [`created_names`] of a script in `dialect` (T-SQL's `#temp` tables, `[brackets]`).
pub fn created_names_in(src: &str, dialect: Dialect) -> HashSet<String> {
    created_in(src, &tokenize_in(src, dialect), dialect)
}

fn created_in(src: &str, tokens: &[Token], dialect: Dialect) -> HashSet<String> {
    let sig: Vec<&Token> = tokens.iter().filter(|t| !t.kind.is_trivia()).collect();
    let w = |i: usize, k: &str| sig.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(src).eq_ignore_ascii_case(k));
    let mut out = HashSet::new();
    let name_at = |mut i: usize| -> Option<String> {
        let t = sig.get(i)?;
        if !matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword) {
            return None;
        }
        // schema.name: the last part.
        while sig.get(i + 1).is_some_and(|d| d.text(src) == ".") && sig.get(i + 2).is_some() {
            i += 2;
        }
        Some(crate::sql::ident_value_in(src, sig[i], dialect).to_lowercase())
    };
    for i in 0..sig.len() {
        let obj = (w(i, "TABLE") || w(i, "VIEW")) && (i > 0 && (w(i - 1, "CREATE") || w(i - 1, "TEMP") || w(i - 1, "TEMPORARY") || w(i - 1, "UNLOGGED") || w(i - 1, "MATERIALIZED") || w(i - 1, "REPLACE") || w(i - 1, "RECURSIVE")));
        if obj {
            let mut k = i + 1;
            if w(k, "IF") {
                k += 3;
            }
            out.extend(name_at(k));
        }
        if w(i, "INTO") && !(i > 0 && w(i - 1, "INSERT")) {
            let mut k = i + 1;
            while w(k, "TEMP") || w(k, "TEMPORARY") || w(k, "UNLOGGED") || w(k, "TABLE") {
                k += 1;
            }
            out.extend(name_at(k));
        }
        if w(i, "RENAME") && w(i + 1, "TO") {
            out.extend(name_at(i + 2));
        }
    }
    out
}

fn unknown_tables(a: &Analysis, snap: &SchemaSnapshot, known: &dyn Fn(&str) -> bool, out: &mut Vec<Lint>) {
    for (si, s) in a.scopes.iter().enumerate() {
        for src in &s.sources {
            let SourceKind::Table { schema, name } = &src.kind else { continue };
            // T-SQL's temporary tables live in tempdb, out of the cache.
            if name.starts_with("pg_") || name.starts_with('#') || known(name) || (name.eq_ignore_ascii_case("dual") && schema.is_none()) {
                continue;
            }
            match schema {
                Some(sc) => {
                    if sc.starts_with("pg_") || sc.eq_ignore_ascii_case("information_schema") || !snap.loaded.contains(sc) || known(&format!("{sc}.{name}")) {
                        continue;
                    }
                    if snap.resolve(Some(sc), name).is_none() {
                        out.push(Lint { range: src.name_range.clone(), severity: Severity::Error, message: format!("relation \"{sc}.{name}\" does not exist") });
                    }
                }
                None => {
                    if a.visible_cte(si, name).is_some() || !snap.search_path_loaded() {
                        continue;
                    }
                    if snap.resolve(None, name).is_none() {
                        out.push(Lint { range: src.name_range.clone(), severity: Severity::Error, message: format!("relation \"{name}\" does not exist") });
                    }
                }
            }
        }
    }
}

fn columns(a: &Analysis, snap: &SchemaSnapshot, lar: bool, out: &mut Vec<Lint>) {
    // Output names (ORDER BY / GROUP BY may use them; a set operation's ORDER BY reads the
    // first arm's) and every source name (a whole-row reference) are never unknown columns.
    let mut names: HashSet<&str> = HashSet::new();
    for s in &a.scopes {
        for (ix, i) in s.items.iter().enumerate() {
            // An alias, not a plain column (`total` names itself and must still exist).
            if let Item::Named(n) = i
                && s.item_refs.get(ix).cloned().flatten().is_none_or(|(_, c)| c != *n)
            {
                names.insert(n);
            }
        }
        for src in &s.sources {
            if let Some(n) = src.visible_name() {
                names.insert(n);
            }
            if let SourceKind::Table { name, .. } = &src.kind {
                names.insert(name);
            }
        }
    }
    for c in &a.ctes {
        names.insert(&c.name);
    }
    for r in &a.refs {
        if SYSTEM_COLUMNS.contains(&r.name.as_str()) {
            continue;
        }
        match &r.qualifier {
            Some(q) => {
                let Some((s, i)) = a.find_source(r.scope, q) else { continue };
                let Some(cols) = a.source_columns(s, &a.scopes[s].sources[i], snap) else { continue };
                if !cols.is_empty() && !cols.iter().any(|(n, _)| a.col_eq(n, &r.name)) {
                    out.push(Lint { range: r.name_range.clone(), severity: Severity::Error, message: format!("column {q}.{} does not exist", r.name) });
                }
            }
            None => {
                if lar || names.contains(r.name.as_str()) {
                    continue;
                }
                if let Some(l) = unqualified(a, r.scope, &r.name, r.clause, snap) {
                    out.push(Lint { range: r.name_range.clone(), ..l });
                }
            }
        }
    }
    for t in &a.targets {
        let src = &a.scopes[t.scope].sources[t.source];
        let Some(cols) = a.source_columns(t.scope, src, snap) else { continue };
        if !cols.is_empty() && !cols.iter().any(|(n, _)| a.col_eq(n, &t.name)) && !SYSTEM_COLUMNS.contains(&t.name.as_str()) {
            let rel = src.visible_name().unwrap_or("");
            out.push(Lint { range: t.range.clone(), severity: Severity::Error, message: format!("column \"{}\" of relation \"{rel}\" does not exist", t.name) });
        }
    }
}

/// An unqualified column resolved like the server does, innermost scope first: ambiguous when
/// two known sources of the first scope that has it both have it, unknown when no scope has
/// it and every source on the way is known. `None` when fine or unsure.
fn unqualified(a: &Analysis, scope: usize, name: &str, _clause: Clause, snap: &SchemaSnapshot) -> Option<Lint> {
    for s in a.chain(scope) {
        let sc = &a.scopes[s];
        if sc.open || sc.using.iter().any(|u| u == name) {
            return None;
        }
        let mut hits = 0;
        let mut unsure = false;
        // `excluded` (ON CONFLICT) must be qualified: it never makes a name ambiguous.
        for src in sc.sources.iter().filter(|x| x.alias.as_deref() != Some("excluded")) {
            match a.source_columns(s, src, snap) {
                Some(cols) if cols.is_empty() && matches!(src.kind, SourceKind::Table { .. }) => unsure = true,
                Some(cols) => hits += usize::from(cols.iter().any(|(n, _)| a.col_eq(n, name))),
                None => unsure = true,
            }
        }
        if hits >= 2 && !sc.natural {
            return Some(Lint { range: 0..0, severity: Severity::Error, message: format!("column reference \"{name}\" is ambiguous") });
        }
        if hits >= 1 || unsure || sc.natural {
            return None;
        }
    }
    Some(Lint { range: 0..0, severity: Severity::Error, message: format!("column \"{name}\" does not exist") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, RelKind, Relation};
    use std::collections::HashMap;

    fn snap() -> SchemaSnapshot {
        let col = |n: &str| Column { name: n.into(), type_name: "bigint".into(), nullable: None, primary_key: false, default: None };
        let rel = |s: &str, n: &str| Relation { schema: s.into(), name: n.into(), kind: RelKind::Table, oid: None };
        let mut cols = HashMap::new();
        cols.insert(0, vec![col("id"), col("customer"), col("total"), col("status")]);
        cols.insert(1, vec![col("id"), col("name")]);
        cols.insert(2, vec![col("yr")]);
        let mut s = SchemaSnapshot::new(
            vec!["public".into()],
            vec!["public".into(), "sales".into()],
            vec![rel("public", "orders"), rel("public", "customer"), rel("sales", "goal")],
            cols,
        );
        s.complete = true;
        s
    }

    fn msgs(sql: &str) -> Vec<String> {
        let s = snap();
        lint(sql, &LintContext { schema: Some(&s), ..Default::default() }).into_iter().map(|l| format!("{}: {}", &sql[l.range], l.message)).collect()
    }

    fn clean(sql: &str) {
        let m = msgs(sql);
        assert!(m.is_empty(), "{sql}\n  {m:?}");
    }

    #[test]
    fn unknown_tables_and_columns() {
        assert_eq!(msgs("select * from ordrs"), vec!["ordrs: relation \"ordrs\" does not exist"]);
        assert_eq!(msgs("select * from sales.nope"), vec!["sales.nope: relation \"sales.nope\" does not exist"]);
        assert_eq!(msgs("select p.totl from orders p"), vec!["totl: column p.totl does not exist"]);
        assert_eq!(msgs("select totl from orders"), vec!["totl: column \"totl\" does not exist"]);
        assert_eq!(msgs("select id from orders p join customer c on c.id = p.customer"), vec!["id: column reference \"id\" is ambiguous"]);
        assert_eq!(msgs("update orders set sttus = 'x' where id = 1"), vec!["sttus: column \"sttus\" of relation \"orders\" does not exist"]);
        assert_eq!(msgs("insert into customer (id, nme) values (1, 'a')"), vec!["nme: column \"nme\" of relation \"customer\" does not exist"]);
        assert_eq!(msgs("insert customer (id, nme) values (1, 'a')"), vec!["nme: column \"nme\" of relation \"customer\" does not exist"]);
    }

    #[test]
    fn syntax_rules() {
        assert_eq!(msgs("select * from orders p join customer c where c.id = 1"), vec!["join customer c: JOIN without ON"]);
        assert_eq!(msgs("delete from orders"), vec!["delete: DELETE without WHERE"]);
        assert_eq!(msgs("UPDATE orders SET total = 0;"), vec!["UPDATE: UPDATE without WHERE"]);
        assert_eq!(msgs("select 'abc\nfrom x"), vec!["'abc: unterminated string"]);
        assert_eq!(msgs("select 1 /* open"), vec!["/* open: unterminated comment"]);
    }

    #[test]
    fn no_false_positives() {
        for sql in [
            // CTEs, recursion, subqueries, USING, NATURAL, lateral, functions, VALUES.
            "with big as (select id, total as t from orders) select b.t, t from big b",
            "with recursive r(n) as (select 1 union all select n + 1 from r where n < 10) select n from r",
            "select x.who from (select c.name as who from customer c) x",
            "select id, name from orders join customer using (id)",
            "select id from orders natural join customer",
            "select p.id, g.n, n from orders p, lateral generate_series(1, p.total) g(n)",
            "select value from orders p, jsonb_each(p.status) e",
            "select a, b from (values (1, 2)) v(a, b)",
            "select * from unnest(array[1, 2]) as u",
            // Correlated subqueries and outer references.
            "select * from customer c where exists (select 1 from orders p where p.customer = c.id and total > 0)",
            "select id, (select count(*) from orders where customer = c.id) from customer c",
            // Output aliases, set operations, positions.
            "select total * 2 as doubled from orders order by doubled",
            "select id as x from orders union select id from customer order by x",
            "select status, count(*) n from orders group by 1 having count(*) > 1 order by n desc",
            // Expressions that look like names.
            "select extract(year from now()), current_date, now() at time zone 'utc', interval '1' day from orders",
            "select cast(total as double precision), total::numeric(10, 2), total::character varying from orders",
            "select count(*) filter (where status = 'x'), sum(total) over (partition by customer order by id) from orders",
            "select sum(total) over w from orders window w as (order by id)",
            "select * from orders where id is distinct from customer",
            "select * from orders where status = :status and id = :'id' and total > $1",
            "select ctid, xmin, tableoid from orders",
            "select p from orders p",
            "select row_to_json(c) from customer c",
            "select trim(both 'x' from name), substring(name from 1 for 2), position('a' in name) from customer",
            "select * from orders order by status nulls first",
            "select coalesce(x => 1) from orders",
            "select * from orders for update of orders",
            "select * from orders order by id offset 0 rows fetch next 10 rows only",
            "select * from orders order by id fetch first 5 rows with ties",
            "select status from orders group by grouping sets ((status), ())",
            // Temp tables and objects the script creates, unknown schemas, catalogs.
            "create temp table t (a int); select a from t",
            "select * into temp tmp from orders; select * from tmp",
            "select * from pg_class",
            "select * from information_schema.tables",
            "select * from other.thing",
            "set search_path = other; select * from thing",
            // DML shapes.
            "insert into orders (id, status) select id, name from customer on conflict (id) do update set status = excluded.status, total = total + 1",
            "update orders p set total = c.id from customer c where c.id = p.customer",
            "delete from orders p using customer c where c.id = p.customer",
            "insert into customer values (1, 'a') returning *",
            "insert into customer (id, name) values (default, 'x') on conflict on constraint customer_pkey do nothing",
            // Not DML: bodies and DDL are not read.
            "create function f(a int) returns int language sql begin atomic select a + 1; end",
            "do $$ begin perform nope from ghost; end $$",
            "create view v as select nothing from nowhere",
            "explain select * from orders",
            // Unfinished text while typing.
            "select * from orders p where p.",
            "select * from orders p join customer c on",
            "",
        ] {
            clean(sql);
        }
    }

    #[test]
    fn quiet_without_a_loaded_schema() {
        let mut s = snap();
        s.complete = false;
        s.columns.clear();
        let l = lint("select totl from orders p join customer c on c.x = p.y", &LintContext { schema: Some(&s), ..Default::default() });
        assert!(l.is_empty(), "{l:?}");
        let l = lint("select * from nope", &LintContext { schema: None, ..Default::default() });
        assert!(l.is_empty());
        // LAR: no unknown tables, no unqualified columns.
        let s = snap();
        let l = lint("select totl from nope", &LintContext { schema: Some(&s), lar: true, ..Default::default() });
        assert!(l.is_empty(), "{l:?}");
        // Names the session created.
        let known: HashSet<String> = ["sess_tmp".to_string()].into();
        let l = lint("select * from sess_tmp", &LintContext { schema: Some(&s), known: Some(&known), ..Default::default() });
        assert!(l.is_empty());
    }

    #[test]
    fn tsql_shapes() {
        let mut s = snap();
        s.dialect = crate::sql::Dialect::TSql;
        for sql in [
            "CREATE TABLE #chk (a int);\nINSERT #chk VALUES (1);\nSELECT a FROM #chk\nGO",
            "SELECT * FROM orders ORDER BY id\n OFFSET 0 ROWS FETCH NEXT 1000 ROWS ONLY;",
            "SELECT TOP (5) [id], [total] FROM [orders] WHERE [status] = N'x'",
            // The target named by its alias in the FROM that follows.
            "UPDATE p\nSET p.status = 3, -- review\n    p.total = GETUTCDATE()\nFROM orders p\nJOIN customer c ON c.id = p.customer\nWHERE p.status = 1  -- pending\n  AND c.name = 0\n  AND p.total < DATEADD(MINUTE, -@DelayMinutes, GETUTCDATE());",
            "UPDATE p SET status = 3 FROM orders p JOIN customer c ON c.id = p.customer WHERE c.id = 1",
            "UPDATE orders SET orders.total = 1 FROM orders JOIN customer c ON c.id = orders.customer WHERE c.id = 1",
            "DELETE FROM p FROM orders p JOIN customer c ON c.id = p.customer WHERE c.id = 1",
            "DELETE p FROM orders p JOIN customer c ON c.id = p.customer WHERE c.id = 1",
            "UPDATE TOP (10) orders SET total = 0 WHERE id = 1",
            "DELETE TOP (5) FROM orders WHERE id = 1",
            "UPDATE orders SET orders.total = 0 WHERE id = 1",
            // Query and table hints, FOR XML / JSON.
            "WITH r AS (SELECT id FROM orders UNION ALL SELECT p.id FROM orders p JOIN r ON p.customer = r.id)\nSELECT (SELECT COUNT(DISTINCT id) FROM r) AS n\nOPTION (MAXRECURSION 100);",
            "SELECT id FROM orders OPTION (RECOMPILE, MAXDOP 1, OPTIMIZE FOR UNKNOWN)",
            "SELECT p.id FROM orders p WITH (NOLOCK) JOIN customer c WITH (INDEX(ix_c), FORCESEEK) ON c.id = p.customer",
            "SELECT id, total FROM orders FOR JSON PATH, ROOT('orders')",
            "SELECT STUFF((SELECT ',' + name FROM customer FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 1, '')",
            "SELECT CAST(v AS uniqueidentifier) AS v FROM (VALUES ('a'), ('b')) x(v)",
            "SELECT id FROM orders p WHERE p.status LIKE '%' + CONVERT(nvarchar(36), p.id) + '%'",
        ] {
            let l = lint(sql, &LintContext { schema: Some(&s), ..Default::default() });
            assert!(l.is_empty(), "{sql}\n  {l:?}");
        }
        let m: Vec<String> = lint("UPDATE p SET p.sttus = 3 FROM orders p WHERE p.id = 1", &LintContext { schema: Some(&s), ..Default::default() })
            .into_iter()
            .map(|l| l.message)
            .collect();
        assert_eq!(m, ["column \"sttus\" of relation \"p\" does not exist"]);
        // MySQL updates joined tables.
        s.dialect = crate::sql::Dialect::MySql;
        for sql in ["UPDATE orders p JOIN customer c ON c.id = p.customer SET p.total = 1 WHERE c.id = 1", "UPDATE orders p SET p.total = 1 WHERE p.id = 1"] {
            let l = lint(sql, &LintContext { schema: Some(&s), ..Default::default() });
            assert!(l.is_empty(), "{sql}\n  {l:?}");
        }
    }

    #[test]
    fn tsql_recursive_cte_with_hint() {
        let col = |n: &str| Column { name: n.into(), type_name: "int".into(), nullable: None, primary_key: false, default: None };
        let mut cols = HashMap::new();
        cols.insert(0, ["Id", "ParentLevelId", "IsDeleted", "PathToRoot", "IsRoot", "OrganizationId"].map(col).to_vec());
        let rel = Relation { schema: "dbo".into(), name: "HierarchyLevels".into(), kind: RelKind::Table, oid: None };
        let mut s = SchemaSnapshot::new(vec!["dbo".into()], vec!["dbo".into()], vec![rel], cols);
        s.complete = true;
        s.dialect = crate::sql::Dialect::TSql;
        let sql = "WITH Roots AS (\n    SELECT CAST(RootId AS uniqueidentifier) AS RootId FROM (VALUES\n        ('902ebf67-556c-1b71-4858-3a1e21c5bd74'),\n        ('06d0eb4d-6c89-d5f7-1c2e-3a2009634fba')\n    ) v(RootId)\n),\nByParent AS (\n    SELECT h.Id FROM HierarchyLevels h JOIN Roots r ON r.RootId = h.Id WHERE h.IsDeleted = 0\n    UNION ALL\n    SELECT c.Id FROM HierarchyLevels c JOIN ByParent p ON c.ParentLevelId = p.Id WHERE c.IsDeleted = 0\n),\nByPath AS (\n    SELECT h.Id FROM HierarchyLevels h\n    WHERE h.IsDeleted = 0\n      AND EXISTS (SELECT 1 FROM Roots r WHERE h.PathToRoot LIKE '%' + CONVERT(nvarchar(36), r.RootId) + '%')\n)\nSELECT\n    (SELECT COUNT(DISTINCT Id) FROM ByParent) AS ProgramsByParent,\n    (SELECT COUNT(*) FROM ByPath) AS ProgramsByPath,\n    (SELECT COUNT(*) FROM ByPath WHERE Id NOT IN (SELECT Id FROM ByParent)) AS OnlyInPath,\n    (SELECT COUNT(*) FROM ByParent WHERE Id NOT IN (SELECT Id FROM ByPath)) AS OnlyInParent,\n    (SELECT COUNT(*) FROM HierarchyLevels h\n        WHERE h.IsDeleted = 0 AND h.IsRoot = 0\n          AND h.OrganizationId = '74A17135-94CF-62A5-BDE1-3A16C41F541A'\n          AND h.Id NOT IN (SELECT Id FROM ByPath)) AS OrgProgramsOutOfScope\nOPTION (MAXRECURSION 100);";
        let l = lint(sql, &LintContext { schema: Some(&s), ..Default::default() });
        assert!(l.is_empty(), "{l:?}");
    }

    /// Fail-open on T-SQL the reader does not model: a corpus of real shapes against a schema
    /// where every name they use exists gives no finding, while plain typos are still caught.
    #[test]
    fn tsql_corpus_fails_open() {
        let mut s = snap();
        s.dialect = crate::sql::Dialect::TSql;
        let corpus = [
            "SELECT c.name, x.total FROM customer c CROSS APPLY (SELECT TOP (1) p.total FROM orders p WHERE p.customer = c.id ORDER BY p.id DESC) x",
            "SELECT c.name, x.total FROM customer c OUTER APPLY (SELECT SUM(total) AS total FROM orders WHERE customer = c.id) x",
            "SELECT * FROM (SELECT status, total FROM orders) s PIVOT (SUM(total) FOR status IN ([paid], [open])) pv",
            "SELECT id, attr, val FROM (SELECT id, status, customer FROM orders) s UNPIVOT (val FOR attr IN (status, customer)) u",
            "MERGE orders AS t USING (SELECT id, name FROM customer) AS s ON t.customer = s.id WHEN MATCHED THEN UPDATE SET t.total = 0 WHEN NOT MATCHED THEN INSERT (id, customer) VALUES (s.id, s.id) OUTPUT $action, inserted.*;",
            "INSERT INTO orders (id, customer) OUTPUT inserted.id, inserted.customer VALUES (1, 2)",
            "UPDATE orders SET total = 0 OUTPUT deleted.total, inserted.total WHERE id = 1",
            "BEGIN TRY\n  SELECT ghost FROM orders;\nEND TRY\nBEGIN CATCH\n  SELECT ERROR_MESSAGE() AS msg;\nEND CATCH",
            "IF EXISTS (SELECT 1 FROM orders WHERE id = 1)\n  UPDATE orders SET total = 1 WHERE id = 1\nELSE\n  INSERT INTO orders (id) VALUES (1)",
            "WHILE (SELECT COUNT(*) FROM orders) < 10\nBEGIN\n  INSERT INTO orders (id) VALUES (1)\nEND",
            "SELECT id INTO #t FROM orders; SELECT t.id, t.extra FROM #t t",
            "DECLARE @v TABLE (a int, b int); INSERT INTO @v (a, b) VALUES (1, 2); SELECT a, b FROM @v",
            "SELECT a, b FROM @v WHERE a > 1",
            "SELECT j.name, j.age FROM OPENJSON(@json) WITH (name nvarchar(50) '$.name', age int '$.age') j",
            "SELECT customer, STRING_AGG(status, ',') WITHIN GROUP (ORDER BY status) AS sts FROM orders GROUP BY customer",
            "SELECT id, SUM(total) OVER (PARTITION BY customer ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS acc FROM orders",
            "SELECT TOP (5) WITH TIES id, total FROM orders ORDER BY total DESC",
            "SELECT * INTO #snapshot FROM orders WHERE total > 0",
            "EXEC dbo.recalculate @a = 1, @b = 'x'",
            "WITH stale AS (SELECT id FROM orders WHERE total < 0) DELETE FROM orders WHERE id IN (SELECT id FROM stale)",
            "SELECT NEXT VALUE FOR seq_orders AS n",
            "INSERT INTO orders (id, customer) VALUES (NEXT VALUE FOR dbo.seq_orders, 1)",
            "SELECT JSON_VALUE(status, '$.code') AS code, JSON_QUERY(status, '$.items') AS items FROM orders",
            "SELECT id, total FROM orders FOR SYSTEM_TIME AS OF '2026-01-01' WHERE id = 1",
            "SELECT p.id FROM orders FOR SYSTEM_TIME ALL AS p WHERE p.total > 0",
            "SELECT id FROM orders\nSELECT name FROM customer",
            "UPDATE orders SET total = 1 WHERE id = 1\nSELECT @@ROWCOUNT AS n",
            "SELECT c.id FROM customer c WHERE c.id IN (SELECT customer FROM orders EXCEPT SELECT id FROM customer)",
            "SELECT * FROM CHANGETABLE(CHANGES dbo.orders, 0) AS ct",
            "SELECT * FROM orders WHERE CONTAINS(status, 'x')",
            "SELECT IIF(total > 0, 'a', 'b') AS k, CHOOSE(2, 'x', 'y') AS c, TRY_CAST(status AS int) AS s FROM orders",
            "SELECT DATEADD(DAY, -1, GETDATE()) AS d, DATEDIFF(MINUTE, '2026-01-01', GETDATE()) AS m, FORMAT(GETDATE(), 'yyyy') AS y",
            "SELECT o.id FROM OPENROWSET(BULK 'x.csv', FORMAT = 'CSV') AS o",
            "THROW 50000, 'x', 1",
            "PRINT 'x'",
        ];
        for sql in corpus {
            let l = lint(sql, &LintContext { schema: Some(&s), ..Default::default() });
            let schema_findings: Vec<_> = l.iter().filter(|x| x.message.contains("does not exist") || x.message.contains("ambiguous")).collect();
            assert!(schema_findings.is_empty(), "{sql}\n  {l:?}");
        }
        // Simple statements still get their typos.
        for (sql, want) in [
            ("SELECT totl FROM orders", "column \"totl\" does not exist"),
            ("SELECT p.totl FROM orders p WHERE p.id = 1", "column p.totl does not exist"),
            ("SELECT * FROM ordrs", "relation \"ordrs\" does not exist"),
            ("UPDATE orders SET sttus = 1 WHERE id = 1", "column \"sttus\" of relation \"orders\" does not exist"),
            ("SELECT TOP (10) p.nme FROM orders p ORDER BY p.id", "column p.nme does not exist"),
        ] {
            let m: Vec<String> = lint(sql, &LintContext { schema: Some(&s), ..Default::default() }).into_iter().map(|l| l.message).collect();
            assert_eq!(m, [want], "{sql}");
        }
        // PostgreSQL's MERGE is not modelled either; MySQL's IF() is a function, not a block.
        s.dialect = crate::sql::Dialect::Postgres;
        let l = lint("MERGE INTO orders t USING customer c ON t.customer = c.id WHEN MATCHED THEN UPDATE SET total = 0", &LintContext { schema: Some(&s), ..Default::default() });
        assert!(l.iter().all(|x| !x.message.contains("does not exist")), "{l:?}");
        s.dialect = crate::sql::Dialect::MySql;
        let m: Vec<String> = lint("SELECT IF(total > 0, 'a', 'b') AS k FROM orders WHERE totl = 1", &LintContext { schema: Some(&s), ..Default::default() }).into_iter().map(|l| l.message).collect();
        assert_eq!(m, ["column \"totl\" does not exist"]);
        s.dialect = crate::sql::Dialect::TSql;
        // The syntax rules stay on in a statement the reader is unsure of.
        let m: Vec<String> = lint("SELECT c.id FROM customer c CROSS APPLY f(c.id) x JOIN orders p WHERE 'x", &LintContext { schema: Some(&s), ..Default::default() }).into_iter().map(|l| l.message).collect();
        assert!(m.iter().any(|x| x == "unterminated string"), "{m:?}");
    }

    #[test]
    fn fast_enough_for_a_big_script() {
        let s = snap();
        let mut script = String::new();
        for i in 0..2000 {
            script.push_str(&format!("SELECT p.id, p.total FROM orders p JOIN customer c ON c.id = p.customer WHERE p.customer = {i};\n"));
        }
        let t0 = std::time::Instant::now();
        let l = lint(&script, &LintContext { schema: Some(&s), ..Default::default() });
        let took = t0.elapsed();
        eprintln!("lint of 2,000 statements: {took:?}");
        assert!(l.is_empty());
        assert!(took < std::time::Duration::from_millis(250), "{took:?}");
    }
}
