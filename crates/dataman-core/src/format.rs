use serde::{Deserialize, Serialize};

use crate::sql::Dialect;
use crate::sql::lexer::{Token, TokenKind, tokenize_in};
use crate::sql::split::split_tokens_in;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeywordCase {
    #[default]
    Upper,
    Lower,
    Preserve,
}

impl KeywordCase {
    pub const ALL: [KeywordCase; 3] = [KeywordCase::Upper, KeywordCase::Lower, KeywordCase::Preserve];

    pub fn label(self) -> &'static str {
        match self {
            KeywordCase::Upper => "UPPER",
            KeywordCase::Lower => "lower",
            KeywordCase::Preserve => "Preserve",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatOptions {
    pub keyword_case: KeywordCase,
    pub indent: usize,
}

impl Default for FormatOptions {
    fn default() -> Self {
        FormatOptions { keyword_case: KeywordCase::Upper, indent: 2 }
    }
}

pub fn format(src: &str, opts: &FormatOptions) -> String {
    format_in(src, opts, Dialect::Postgres)
}

pub fn format_in(src: &str, opts: &FormatOptions, dialect: Dialect) -> String {
    let tokens = tokenize_in(src, dialect);
    let stmts = split_tokens_in(src, &tokens, dialect.script());
    let lead = &src[..src.len() - src.trim_start().len()];
    let trail = &src[src.trim_end().len().max(lead.len())..];
    let mut out = String::with_capacity(src.len() + src.len() / 4);
    let mut next = stmts.iter().peekable();
    let mut newlines = 0usize;
    let mut first = true;
    let mut i = 0;
    let sep = |out: &mut String, newlines: usize, first: bool, statement: bool| {
        if first {
            return;
        }
        out.push_str(match newlines {
            0 if statement => "\n",
            0 => " ",
            1 => "\n",
            _ => "\n\n",
        });
    };
    while i < tokens.len() {
        let t = tokens[i];
        if let Some(s) = next.peek()
            && t.start == s.range.start
        {
            let end = tokens[i..].iter().position(|x| x.start >= s.full.end).map_or(tokens.len(), |n| i + n);
            sep(&mut out, newlines, first, true);
            out.push_str(&statement(src, &tokens[i..end], opts));
            next.next();
            i = end;
            newlines = 0;
            first = false;
            continue;
        }
        match t.kind {
            TokenKind::Whitespace => newlines += t.text(src).matches('\n').count(),
            TokenKind::Punct if t.text(src) == ";" => {
                out.push(';');
                first = false;
                newlines = 0;
            }
            _ => {
                sep(&mut out, newlines, first, false);
                out.push_str(t.text(src).trim_end_matches([' ', '\t']));
                newlines = 0;
                first = false;
            }
        }
        i += 1;
    }
    format!("{lead}{out}{trail}")
}

fn statement(src: &str, toks: &[Token], opts: &FormatOptions) -> String {
    let sig: Vec<usize> = (0..toks.len()).filter(|&k| !toks[k].kind.is_trivia()).collect();
    let word = |si: usize| sig.get(si).map_or(String::new(), |&k| toks[k].text(src).to_ascii_uppercase());
    let first = word(0);
    let laid = match first.as_str() {
        "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "VALUES" | "TABLE" | "EXPLAIN" => true,
        "CREATE" => create_as(src, toks, &sig),
        _ => false,
    };
    if !laid {
        return verbatim(src, toks, &sig, opts);
    }
    let mut f = Fmt::new(src, toks, sig, *opts);
    f.run();
    f.out
}

fn create_as(src: &str, toks: &[Token], sig: &[usize]) -> bool {
    let text = |si: usize| sig.get(si).map_or("", |&k| toks[k].text(src));
    let mut depth = 0i32;
    let mut object = false;
    for si in 0..sig.len() {
        let w = text(si);
        match w {
            "(" => depth += 1,
            ")" => depth -= 1,
            _ => {}
        }
        if w.eq_ignore_ascii_case("VIEW") || w.eq_ignore_ascii_case("TABLE") {
            object = true;
        }
        if depth == 0 && w.eq_ignore_ascii_case("AS") {
            let n = text(si + 1);
            return object && ["SELECT", "WITH", "VALUES", "("].iter().any(|q| n.eq_ignore_ascii_case(q));
        }
    }
    false
}

fn cased(text: &str, case: KeywordCase) -> String {
    match case {
        KeywordCase::Upper => text.to_ascii_uppercase(),
        KeywordCase::Lower => text.to_ascii_lowercase(),
        KeywordCase::Preserve => text.to_string(),
    }
}

fn keyword_as_name(src: &str, toks: &[Token], sig: &[usize], si: usize) -> bool {
    let at = |i: Option<usize>| i.and_then(|i| sig.get(i)).map(|&k| toks[k].text(src));
    at(si.checked_sub(1)) == Some(".") || at(Some(si + 1)) == Some(".")
}

fn verbatim(src: &str, toks: &[Token], sig: &[usize], opts: &FormatOptions) -> String {
    let mut out = String::with_capacity(toks.iter().map(|t| t.end - t.start).sum());
    let mut si = 0;
    for (k, t) in toks.iter().enumerate() {
        let is_sig = sig.get(si) == Some(&k);
        if t.kind == TokenKind::Keyword && is_sig && !keyword_as_name(src, toks, sig, si) {
            out.push_str(&cased(t.text(src), opts.keyword_case));
        } else {
            out.push_str(t.text(src));
        }
        if is_sig {
            si += 1;
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Query,
    Paren,
    Case,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum C {
    Select,
    From,
    On,
    Where,
    Having,
    Set,
    Values,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    kind: Kind,
    base: usize,
    open: usize,
    clause: C,
    list_break: bool,
    item_at: Option<usize>,
    between: bool,
    first: bool,
}

impl Frame {
    fn new(kind: Kind, base: usize, open: usize) -> Frame {
        Frame { kind, base, open, clause: C::Other, list_break: false, item_at: None, between: false, first: true }
    }
}

const NO_SPACE_KW: &[&str] = &[
    "CAST", "ANY", "SOME", "ALL", "ARRAY", "ROW", "NUMERIC", "DECIMAL", "VARCHAR", "CHAR", "CHARACTER", "TIMESTAMP",
    "TIMESTAMPTZ", "TIME", "LEFT", "RIGHT", "REPLACE",
];
const SELECT_END: &[&str] = &[
    "FROM", "INTO", "WHERE", "GROUP", "HAVING", "WINDOW", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR", "UNION", "INTERSECT",
    "EXCEPT",
];
const SET_END: &[&str] = &["WHERE", "FROM", "RETURNING"];
const VALUES_END: &[&str] = &["ON", "RETURNING", "ORDER", "LIMIT", "OFFSET", "UNION", "INTERSECT", "EXCEPT"];
const CLAUSES: &[&str] = &[
    "SELECT", "FROM", "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "FETCH", "WINDOW", "UNION", "INTERSECT",
    "EXCEPT", "RETURNING", "VALUES", "SET", "JOIN", "LEFT", "RIGHT", "FULL", "INNER", "CROSS", "NATURAL", "UPDATE",
    "DELETE", "INSERT",
];

struct Fmt<'a> {
    src: &'a str,
    toks: &'a [Token],
    sig: Vec<usize>,
    close: Vec<usize>,
    opts: FormatOptions,
    out: String,
    line_level: usize,
    line_start: bool,
    pending: Option<usize>,
    prev: Option<usize>,
    unary: bool,
    frames: Vec<Frame>,
}

impl<'a> Fmt<'a> {
    fn new(src: &'a str, toks: &'a [Token], sig: Vec<usize>, opts: FormatOptions) -> Self {
        let n = sig.len();
        let mut close = vec![n; n];
        let mut stack = Vec::new();
        for si in 0..n {
            match toks[sig[si]].text(src) {
                "(" | "[" if toks[sig[si]].kind == TokenKind::Punct => stack.push(si),
                ")" | "]" if toks[sig[si]].kind == TokenKind::Punct => {
                    if let Some(o) = stack.pop() {
                        close[o] = si;
                    }
                }
                _ => {}
            }
        }
        Fmt {
            src,
            toks,
            sig,
            close,
            opts,
            out: String::new(),
            line_level: 0,
            line_start: true,
            pending: None,
            prev: None,
            unary: false,
            frames: vec![Frame::new(Kind::Query, 0, 0)],
        }
    }

    fn tok(&self, si: usize) -> Option<&Token> {
        self.sig.get(si).map(|&k| &self.toks[k])
    }

    fn text(&self, si: usize) -> &'a str {
        self.sig.get(si).map_or("", |&k| self.toks[k].text(self.src))
    }

    fn word(&self, si: usize, w: &str) -> bool {
        self.tok(si).is_some_and(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(self.src).eq_ignore_ascii_case(w))
    }

    fn kw(&self, si: usize, w: &str) -> bool {
        self.tok(si).is_some_and(|t| t.kind == TokenKind::Keyword && t.text(self.src).eq_ignore_ascii_case(w))
    }

    fn punct(&self, si: usize, p: &str) -> bool {
        self.tok(si).is_some_and(|t| t.kind == TokenKind::Punct && t.text(self.src) == p)
    }

    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("the statement frame")
    }

    fn starts_query(&self, si: usize) -> bool {
        ["SELECT", "WITH", "VALUES", "TABLE"].iter().any(|w| self.kw(si, w)) || (self.punct(si, "(") && self.starts_query(si + 1))
    }

    fn list(&self, mut si: usize, ends: &[&str]) -> (usize, usize, bool) {
        let (mut n, mut len, mut block) = (0, 0, false);
        while si < self.sig.len() {
            if self.punct(si, "(") || self.punct(si, "[") {
                let c = self.close[si].min(self.sig.len());
                block |= self.starts_query(si + 1);
                for x in si..c.min(self.sig.len()) {
                    len += self.text(x).len() + 1;
                    block |= self.kw(x, "CASE");
                }
                si = c + 1;
                continue;
            }
            if self.punct(si, ")") || self.punct(si, ";") {
                break;
            }
            if ends.iter().any(|w| self.kw(si, w)) && !(self.kw(si, "FROM") && self.word(si.wrapping_sub(1), "DISTINCT")) {
                break;
            }
            if self.punct(si, ",") {
                n += 1;
            }
            block |= self.kw(si, "CASE");
            len += self.text(si).len() + 1;
            si += 1;
        }
        (n, len, block)
    }

    fn breaks(&self, si: usize, ends: &[&str]) -> bool {
        let (n, len, block) = self.list(si, ends);
        n >= 2 || (n >= 1 && (len > 60 || block))
    }

    fn nl(&mut self, level: usize) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        for _ in 0..level * self.opts.indent {
            self.out.push(' ');
        }
        self.line_level = level;
        self.line_start = true;
        self.pending = None;
    }

    fn put(&mut self, text: &str, space: bool) {
        if space && !self.line_start {
            self.out.push(' ');
        }
        self.out.push_str(text);
        self.line_start = false;
    }

    fn cont_level(&self) -> usize {
        let f = self.frames.last().expect("frame");
        match f.kind {
            Kind::Query => f.base + 1,
            Kind::Paren => f.open + 1,
            Kind::Case => f.base,
        }
    }

    fn run(&mut self) {
        let mut si = 0;
        for k in 0..self.toks.len() {
            let t = self.toks[k];
            match t.kind {
                TokenKind::Whitespace => {}
                TokenKind::LineComment | TokenKind::BlockComment => self.comment(k),
                _ => {
                    self.token(si);
                    si += 1;
                }
            }
        }
        while self.out.ends_with(' ') || self.out.ends_with('\n') {
            self.out.pop();
        }
    }

    fn comment(&mut self, k: usize) {
        let t = self.toks[k];
        let own = k == 0 || self.toks[k - 1].kind == TokenKind::LineComment || (self.toks[k - 1].kind == TokenKind::Whitespace && self.toks[k - 1].text(self.src).contains('\n'));
        let text = t.text(self.src).trim_end_matches([' ', '\t', '\r']);
        let line = t.kind == TokenKind::LineComment;
        if own {
            let level = self.pending.take().unwrap_or_else(|| {
                let next = self.sig.iter().position(|&x| x > k);
                let f = self.frames.last().expect("frame");
                match next {
                    Some(n) if f.kind == Kind::Query && CLAUSES.iter().any(|w| self.kw(n, w)) => f.base,
                    _ => self.cont_level(),
                }
            });
            self.nl(level);
            self.put(text, false);
            self.pending = Some(level);
        } else {
            self.put(text, true);
            if line {
                self.pending = Some(self.pending.unwrap_or_else(|| self.cont_level()));
            }
        }
        self.unary = false;
    }

    fn token(&mut self, si: usize) {
        let t = *self.tok(si).expect("token");
        let text = t.text(self.src);
        let w = if matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) { text.to_ascii_uppercase() } else { String::new() };
        let mut newline: Option<usize> = None;
        let mut push: Option<Frame> = None;
        let mut pop_after = false;

        if t.kind == TokenKind::Punct && (text == ")" || text == "]") {
            while self.frames.len() > 1 && self.frames.last().is_some_and(|f| f.kind == Kind::Case) {
                self.frames.pop();
            }
            if self.frames.len() > 1 {
                let f = self.frames.pop().expect("frame");
                if f.kind == Kind::Query {
                    newline = Some(f.open);
                }
            }
        }
        let f = *self.frames.last().expect("frame");
        if f.item_at == Some(si) {
            self.top().item_at = None;
            if f.list_break {
                newline = Some(f.base + 1);
            }
        }
        let is_kw = t.kind == TokenKind::Keyword;
        match f.kind {
            Kind::Query if is_kw => {
                let prev_word = |w: &str| self.word(si.wrapping_sub(1), w);
                match w.as_str() {
                    "SELECT" => {
                        if !f.first {
                            newline = Some(f.base);
                        }
                        let mut j = si + 1;
                        if self.kw(j, "ALL") {
                            j += 1;
                        } else if self.kw(j, "DISTINCT") {
                            j += 1;
                            if self.kw(j, "ON") && self.punct(j + 1, "(") {
                                j = self.close[j + 1] + 1;
                            }
                        }
                        let brk = self.breaks(j, SELECT_END);
                        let fr = self.top();
                        fr.clause = C::Select;
                        fr.list_break = brk;
                        fr.item_at = brk.then_some(j);
                    }
                    "FROM" if !prev_word("DISTINCT") && !prev_word("DELETE") => {
                        newline = Some(f.base);
                        let fr = self.top();
                        fr.clause = C::From;
                        fr.list_break = false;
                    }
                    "FROM" => {}
                    "LEFT" | "RIGHT" | "FULL" | "INNER" | "CROSS" | "NATURAL" | "JOIN" if !self.punct(si + 1, "(") => {
                        let joined = ["LEFT", "RIGHT", "FULL", "INNER", "OUTER", "CROSS", "NATURAL"].iter().any(|x| self.kw(si.wrapping_sub(1), x));
                        if !joined {
                            newline = Some(f.base);
                        }
                        self.top().clause = C::From;
                    }
                    "ON" => {
                        if self.word(si + 1, "CONFLICT") {
                            newline = Some(f.base);
                            self.top().clause = C::Other;
                        } else if f.clause == C::From {
                            self.top().clause = C::On;
                        }
                    }
                    "WHERE" => {
                        newline = Some(f.base);
                        self.top().clause = C::Where;
                    }
                    "HAVING" => {
                        newline = Some(f.base);
                        self.top().clause = C::Having;
                    }
                    "GROUP" | "ORDER" | "LIMIT" | "OFFSET" | "FETCH" | "WINDOW" | "UNION" | "INTERSECT" | "EXCEPT" | "RETURNING" => {
                        newline = Some(f.base);
                        self.top().clause = C::Other;
                    }
                    "FOR" if !self.word(si + 1, "SYSTEM_TIME") => {
                        newline = Some(f.base);
                        self.top().clause = C::Other;
                    }
                    "VALUES" if !prev_word("DEFAULT") => {
                        if !f.first {
                            newline = Some(f.base);
                        }
                        let brk = self.list(si + 1, VALUES_END).0 > 0;
                        let fr = self.top();
                        fr.clause = C::Values;
                        fr.list_break = brk;
                        fr.item_at = brk.then_some(si + 1);
                    }
                    "SET" => {
                        newline = Some(f.base);
                        let brk = self.list(si + 1, SET_END).0 > 0;
                        let fr = self.top();
                        fr.clause = C::Set;
                        fr.list_break = brk;
                        fr.item_at = brk.then_some(si + 1);
                    }
                    "INSERT" | "UPDATE" | "DELETE" if !f.first && !prev_word("DO") && !prev_word("FOR") && !prev_word("KEY") => {
                        newline = Some(f.base);
                        self.top().clause = C::Other;
                    }
                    "AND" | "OR" => {
                        if f.between && w == "AND" {
                            self.top().between = false;
                        } else if matches!(f.clause, C::Where | C::Having | C::On) {
                            newline = Some(f.base + 1);
                        }
                    }
                    "BETWEEN" => self.top().between = true,
                    "CASE" => push = Some(Frame::new(Kind::Case, 0, 0)),
                    _ => {}
                }
            }
            Kind::Case if is_kw => match w.as_str() {
                "WHEN" | "ELSE" => newline = Some(f.base),
                "END" => {
                    newline = Some(f.open);
                    pop_after = true;
                }
                "CASE" => push = Some(Frame::new(Kind::Case, 0, 0)),
                _ => {}
            },
            Kind::Paren if is_kw && w == "CASE" => push = Some(Frame::new(Kind::Case, 0, 0)),
            _ => {}
        }
        if t.kind == TokenKind::Punct && text == "(" {
            push = Some(if self.starts_query(si + 1) { Frame::new(Kind::Query, 0, 0) } else { Frame::new(Kind::Paren, 0, 0) });
        } else if t.kind == TokenKind::Punct && text == "[" {
            push = Some(Frame::new(Kind::Paren, 0, 0));
        }

        match newline {
            Some(l) => self.nl(l),
            None => {
                if let Some(l) = self.pending.take() {
                    self.nl(l);
                }
            }
        }
        let space = self.space_before(si);
        let shown = if t.kind == TokenKind::Keyword && !keyword_as_name(self.src, self.toks, &self.sig, si) {
            cased(text, self.opts.keyword_case)
        } else {
            text.to_string()
        };
        self.put(&shown, space);
        self.unary = matches!(text, "-" | "+") && t.kind == TokenKind::Operator && self.prev.is_none_or(|p| self.opens_operand(p));
        self.prev = Some(si);
        self.top().first = false;

        if pop_after && self.frames.len() > 1 {
            self.frames.pop();
        }
        if t.kind == TokenKind::Punct && text == "," {
            let f = *self.frames.last().expect("frame");
            if f.kind == Kind::Query {
                let brk = match f.clause {
                    C::Select | C::Set | C::Values => f.list_break,
                    C::From => true,
                    _ => false,
                };
                if brk {
                    self.pending = Some(f.base + 1);
                }
            }
        }
        if let Some(mut fr) = push {
            let level = self.line_level;
            fr.open = level;
            fr.base = level + 1;
            let query = fr.kind == Kind::Query;
            self.frames.push(fr);
            if query {
                self.pending = Some(level + 1);
            }
        }
    }

    fn opens_operand(&self, p: usize) -> bool {
        let Some(t) = self.tok(p) else { return true };
        match t.kind {
            TokenKind::Operator => true,
            TokenKind::Punct => matches!(t.text(self.src), "(" | "," | "["),
            TokenKind::Keyword => !["END", "NULL", "TRUE", "FALSE"].iter().any(|w| t.text(self.src).eq_ignore_ascii_case(w)),
            _ => false,
        }
    }

    fn space_before(&self, si: usize) -> bool {
        let Some(p) = self.prev else { return false };
        if self.unary {
            return false;
        }
        let (pt, ct) = (self.tok(p).expect("prev"), self.tok(si).expect("cur"));
        let (ps, cs) = (pt.text(self.src), ct.text(self.src));
        if ct.kind == TokenKind::Punct && matches!(cs, "," | ";" | ")" | "]" | ".") {
            return false;
        }
        if (pt.kind == TokenKind::Punct && ps == ".") || cs == "::" || ps == "::" {
            return false;
        }
        if pt.kind == TokenKind::Punct && matches!(ps, "(" | "[") {
            return false;
        }
        if ct.kind == TokenKind::Punct && cs == ":" {
            return matches!(pt.kind, TokenKind::Operator | TokenKind::Keyword) || ps == ",";
        }
        if pt.kind == TokenKind::Punct && ps == ":" {
            return false;
        }
        if ct.kind == TokenKind::Punct && cs == "(" {
            return match pt.kind {
                TokenKind::Ident | TokenKind::QuotedIdent => {
                    let after = self.close[si] + 1;
                    ["INTO", "WITH", "RECURSIVE", "VIEW", "TABLE"].iter().any(|w| self.word(p.wrapping_sub(1), w))
                        || (self.punct(p.wrapping_sub(1), ",") && self.kw(after, "AS") && (self.punct(after + 1, "(") || self.kw(after + 1, "NOT") || self.kw(after + 1, "MATERIALIZED")))
                }
                TokenKind::Keyword => !NO_SPACE_KW.iter().any(|w| ps.eq_ignore_ascii_case(w)),
                _ => true,
            };
        }
        if ct.kind == TokenKind::Punct && cs == "[" {
            return !(matches!(pt.kind, TokenKind::Ident | TokenKind::QuotedIdent) || matches!(ps, ")" | "]") || ps.eq_ignore_ascii_case("ARRAY"));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::tokenize;

    fn fmt(src: &str) -> String {
        format(src, &FormatOptions::default())
    }

    fn idempotent(src: &str, opts: &FormatOptions) {
        let once = format(src, opts);
        let twice = format(&once, opts);
        assert_eq!(once, twice, "not idempotent for:\n{src}\n--- once:\n{once}\n--- twice:\n{twice}");
    }

    #[test]
    fn golden_select() {
        let src = "select p.id, p.status, c.name from orders p join customer c on c.id = p.customer and c.country = 'DE' left join item i on i.order_id = p.id where p.status = 'open' and p.total between 10 and 20 or p.id in (1, 2) group by p.id, c.name having count(*) > 1 order by p.total desc limit 50";
        assert_eq!(
            fmt(src),
            "SELECT
  p.id,
  p.status,
  c.name
FROM orders p
JOIN customer c ON c.id = p.customer
  AND c.country = 'DE'
LEFT JOIN item i ON i.order_id = p.id
WHERE p.status = 'open'
  AND p.total BETWEEN 10 AND 20
  OR p.id IN (1, 2)
GROUP BY p.id, c.name
HAVING count(*) > 1
ORDER BY p.total DESC
LIMIT 50"
        );
    }

    #[test]
    fn golden_ctes_subqueries_case() {
        let src = "with big as (select id, total from orders where total > 100), n as (select count(*) as c from big) select b.id, case when b.total > 500 then 'high' else 'low' end as band, (select c from n) from big b where exists (select 1 from customer c where c.id = b.id);";
        assert_eq!(
            fmt(src),
            "WITH big AS (
  SELECT id, total
  FROM orders
  WHERE total > 100
), n AS (
  SELECT count(*) AS c
  FROM big
)
SELECT
  b.id,
  CASE
    WHEN b.total > 500 THEN 'high'
    ELSE 'low'
  END AS band,
  (
    SELECT c
    FROM n
  )
FROM big b
WHERE EXISTS (
  SELECT 1
  FROM customer c
  WHERE c.id = b.id
);"
        );
    }

    #[test]
    fn golden_dml() {
        let o = FormatOptions { keyword_case: KeywordCase::Lower, indent: 4 };
        assert_eq!(
            format("INSERT INTO orders (id, status) VALUES (1, 'a'), (2, 'b') ON CONFLICT (id) DO UPDATE SET status = excluded.status RETURNING id", &o),
            "insert into orders (id, status)
values
    (1, 'a'),
    (2, 'b')
on conflict (id) do update
set status = excluded.status
returning id"
        );
        assert_eq!(
            format("update orders set status = 'paid', total = total + 1 from customer c where c.id = orders.customer", &o),
            "update orders
set
    status = 'paid',
    total = total + 1
from customer c
where c.id = orders.customer"
        );
        assert_eq!(format("delete from orders where id = $1", &o), "delete from orders\nwhere id = $1");
        assert_eq!(format("insert into t select * from u", &o), "insert into t\nselect *\nfrom u");
    }

    #[test]
    fn keeps_comments_strings_bodies_and_names() {
        let src = "-- head\nSelect \"Odd Column\", 'it''s  spaced', E'a\\'b', MyCol -- trailing\nFROM t /* inline */ where x = $tag$ keep   this $tag$;\n\n/* own\n   lines */\ncreate function f() returns int as $$ select   1 $$ language sql;\n";
        let out = format(src, &FormatOptions { keyword_case: KeywordCase::Preserve, indent: 2 });
        assert_eq!(
            out,
            "-- head\nSelect\n  \"Odd Column\",\n  'it''s  spaced',\n  E'a\\'b',\n  MyCol -- trailing\nFROM t /* inline */\nwhere x = $tag$ keep   this $tag$;\n\n/* own\n   lines */\ncreate function f() returns int as $$ select   1 $$ language sql;\n"
        );
        let up = fmt("select MyCol, count(*) from t; create table x (id int primary key)");
        assert_eq!(up, "SELECT MyCol, count(*)\nFROM t;\nCREATE TABLE x (id INT PRIMARY KEY)".replace("INT PRIMARY", "int PRIMARY"));
    }

    #[test]
    fn spacing() {
        assert_eq!(fmt("select a::int,-1,b[1:2],array[1,2],x->>'k',count(*),cast(x as text),:name,:'v' from t"), "SELECT\n  a::int,\n  -1,\n  b[1:2],\n  ARRAY[1, 2],\n  x ->> 'k',\n  count(*),\n  CAST(x AS TEXT),\n  :name,\n  :'v'\nFROM t");
        assert_eq!(fmt("select t.type, x . key from t"), "SELECT t.type, x.key\nFROM t");
        assert_eq!(fmt("select * from a join b using(id) where x is distinct from y"), "SELECT *\nFROM a\nJOIN b USING (id)\nWHERE x IS DISTINCT FROM y");
        assert_eq!(fmt("select sum(x) over (partition by a order by b) from t"), "SELECT sum(x) OVER (PARTITION BY a ORDER BY b)\nFROM t");
    }

    #[test]
    fn idempotent_everywhere() {
        let samples = [
            "select p.id, p.status from orders p join customer c on c.id = p.customer where a = 1 and (b = 2 or c = 3) order by 1",
            "with recursive r(n) as (select 1 union all select n + 1 from r where n < 10) select * from r",
            "select a, -- one\n b /* two */ from t -- end\n where x = 1;\n-- after\nselect 2;",
            "select case x when 1 then 'a' when 2 then case when y then 'b' end else 'c' end from t",
            "select * from (select * from (select 1) a) b, lateral (select 2) c",
            "insert into t (a) values (1) returning *; update t set a = 2; delete from t where a in (select a from u)",
            "select 'unterminated",
            "select (1",
            "create view v as select a, b from t where x",
            "explain (analyze, buffers) select 1",
            "select 1;;select 2;",
            "  \n select 1 \n\n\n select 2  \n\n",
            "do $$ begin perform 1; end $$; vacuum analyze t;",
            "select a from t\n-- tail comment\n",
            "select a\n  -- own line\n  , b from t",
            "select x from t for update skip locked",
            "select * from t where a = any(array[1,2]) and b not in (select c from d) limit all",
            "select 1 union select 2 except select 3 intersect select 4 order by 1",
        ];
        for o in [FormatOptions::default(), FormatOptions { keyword_case: KeywordCase::Lower, indent: 4 }, FormatOptions { keyword_case: KeywordCase::Preserve, indent: 2 }] {
            for s in samples {
                idempotent(s, &o);
            }
        }
    }

    #[test]
    fn only_whitespace_and_keyword_case_change() {
        let squash = |s: &str| {
            let toks = tokenize(s);
            toks.iter().filter(|t| t.kind != TokenKind::Whitespace).map(|t| if t.kind == TokenKind::Keyword { t.text(s).to_ascii_uppercase() } else { t.text(s).trim_end().to_string() }).collect::<String>()
        };
        for s in ["select a, b from t where x = 'a  b' -- c\n and y = $$ z $$", "with x as (select 1) insert into t select * from x returning *"] {
            assert_eq!(squash(s), squash(&fmt(s)));
        }
    }
}
