//! Find and replace, without the UI: the matcher behind the editor's and the grid's find bars,
//! replacement with capture groups, next/previous with wraparound, and the search over the
//! loaded rows of a result.
//!
//! A query is a literal (case-insensitive unless asked) or a regular expression (the `regex`
//! crate's syntax, `^`/`$` per line); "whole word" rejects a match with a word character
//! (alphanumeric or `_`) right before or after it. Empty matches are never reported.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use regex::{Regex, RegexBuilder};

use crate::result::{Cell, Snapshot, format_cell_into};

/// The bar's toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FindOptions {
    /// Aa: match case (off = ignore case).
    pub case: bool,
    /// ab: whole words only.
    pub word: bool,
    /// .*: the query is a regular expression.
    pub regex: bool,
}

/// A compiled query.
#[derive(Clone, Debug)]
pub struct Matcher {
    re: Regex,
    opts: FindOptions,
    /// A literal `NULL` query also finds NULL cells (they read `NULL` in the grid).
    null: bool,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The match `s..e` of `text` stands alone: no word character touches it on either side.
fn whole_word(text: &str, s: usize, e: usize) -> bool {
    !text[..s].chars().next_back().is_some_and(is_word) && !text[e..].chars().next().is_some_and(is_word)
}

/// The offset of the character after `at`, or `None` at the end.
fn after_char(text: &str, at: usize) -> Option<usize> {
    text[at..].chars().next().map(|c| at + c.len_utf8())
}

impl Matcher {
    /// `Ok(None)` for an empty query; `Err` with the parser's message for a bad expression.
    pub fn new(query: &str, opts: FindOptions) -> Result<Option<Matcher>, String> {
        if query.is_empty() {
            return Ok(None);
        }
        let pattern = if opts.regex { query.to_string() } else { regex::escape(query) };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(!opts.case)
            .multi_line(true)
            .size_limit(1 << 22)
            .build()
            .map_err(|e| e.to_string())?;
        let null = !opts.regex && if opts.case { query == "NULL" } else { query.eq_ignore_ascii_case("null") };
        Ok(Some(Matcher { re, opts, null }))
    }

    pub fn options(&self) -> FindOptions {
        self.opts
    }

    /// Every match of `text`, in order and disjoint.
    pub fn find_iter<'a>(&'a self, text: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
        let mut pos = Some(0);
        std::iter::from_fn(move || {
            while let Some(at) = pos {
                let m = self.re.find_at(text, at)?;
                let (s, e) = (m.start(), m.end());
                if s == e || (self.opts.word && !whole_word(text, s, e)) {
                    // Try again one character later: a whole word may start inside this match.
                    pos = after_char(text, s);
                    continue;
                }
                pos = Some(e);
                return Some(s..e);
            }
            None
        })
    }

    /// At most `cap` matches of `text`, and whether there were more.
    pub fn find_all(&self, text: &str, cap: usize) -> (Vec<Range<usize>>, bool) {
        let mut out = Vec::new();
        for m in self.find_iter(text) {
            if out.len() == cap {
                return (out, true);
            }
            out.push(m);
        }
        (out, false)
    }

    pub fn is_match(&self, text: &str) -> bool {
        if !self.opts.word && !self.opts.regex {
            // A non-empty literal never matches empty.
            return self.re.is_match(text);
        }
        self.find_iter(text).next().is_some()
    }

    /// Whether NULL cells count as matches.
    pub fn matches_null(&self) -> bool {
        self.null
    }

    /// What replaces the match `m` of `text`: `rep` as typed for a literal query; for a regular
    /// expression, with `$1`, `${name}` and `$$` expanded from the match's groups.
    pub fn expand(&self, text: &str, m: Range<usize>, rep: &str) -> String {
        if !self.opts.regex {
            return rep.to_string();
        }
        self.expand_with(text, m, &group_digits(rep))
    }

    fn expand_with(&self, text: &str, m: Range<usize>, rep: &str) -> String {
        let mut out = String::new();
        match self.re.captures_at(text, m.start) {
            Some(caps) if caps.get(0).is_some_and(|g| g.range() == m) => caps.expand(rep, &mut out),
            _ => out.push_str(rep),
        }
        out
    }

    /// The edits of "Replace All" over `text`: sorted, disjoint, every match (no cap).
    pub fn replace_all(&self, text: &str, rep: &str) -> Vec<(Range<usize>, String)> {
        if !self.opts.regex {
            return self.find_iter(text).map(|m| (m, rep.to_string())).collect();
        }
        let rep = group_digits(rep);
        self.find_iter(text).map(|m| (m.clone(), self.expand_with(text, m, &rep))).collect()
    }
}

/// `$1_x` means group 1 then `_x` (as in most editors), not the `regex` crate's group named
/// `1_x`: a `$` followed by digits becomes `${digits}`. `$$`, `${…}` and `$name` are unchanged.
fn group_digits(rep: &str) -> String {
    let mut out = String::with_capacity(rep.len() + 4);
    let mut it = rep.char_indices().peekable();
    while let Some((_, c)) = it.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match it.peek().map(|&(_, n)| n) {
            Some('$') => {
                it.next();
                out.push_str("$$");
            }
            Some(d) if d.is_ascii_digit() => {
                out.push_str("${");
                while let Some(&(_, d)) = it.peek().filter(|(_, d)| d.is_ascii_digit()) {
                    out.push(d);
                    it.next();
                }
                out.push('}');
            }
            _ => out.push('$'),
        }
    }
    out
}

/// `text` with sorted, disjoint `edits` applied in one pass.
pub fn apply_edits(text: &str, edits: &[(Range<usize>, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for (r, s) in edits {
        out.push_str(&text[pos..r.start]);
        out.push_str(s);
        pos = r.end;
    }
    out.push_str(&text[pos..]);
    out
}

/// The item after `at` in sorted `items` (by `key`), wrapping to the first: "find next" from the
/// current selection or cell. An item equal to `at` is the current one and is skipped.
pub fn next_index<T, K: Ord>(items: &[T], key: impl Fn(&T) -> K, at: K) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let i = items.partition_point(|x| key(x) <= at);
    Some(if i == items.len() { 0 } else { i })
}

/// The item before `at`, wrapping to the last: "find previous".
pub fn prev_index<T, K: Ord>(items: &[T], key: impl Fn(&T) -> K, at: K) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let i = items.partition_point(|x| key(x) < at);
    Some(if i == 0 { items.len() - 1 } else { i - 1 })
}

/// The first item at or after `at`, wrapping to the first: where an incremental search lands.
pub fn nearest_index<T, K: Ord>(items: &[T], key: impl Fn(&T) -> K, at: K) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let i = items.partition_point(|x| key(x) < at);
    Some(if i == items.len() { 0 } else { i })
}

/// Matching cells of a result: (display row, column), row by row, left to right.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellHits {
    pub cells: Vec<(u32, u32)>,
    /// Stopped at the cap: there are more.
    pub capped: bool,
}

/// Searches display rows `rows` of `snap` (cells as the grid shows them: formatted numbers and
/// times, the whole value), appending to `hits` up to `cap` cells. Checks `cancel` every 256
/// rows: `false` when it was set (the query changed), with `hits` partly filled.
pub fn search_rows(snap: &Snapshot, rows: Range<usize>, m: &Matcher, cap: usize, cancel: &AtomicBool, hits: &mut CellHits) -> bool {
    let kinds: Vec<_> = snap.columns.iter().map(|c| c.kind).collect();
    let mut buf = String::new();
    let end = rows.end.min(snap.rows());
    for r in rows.start..end {
        if r % 256 == 0 && cancel.load(Ordering::Relaxed) {
            return false;
        }
        let (chunk, i) = snap.locate(snap.source_row(r));
        for (c, &kind) in kinds.iter().enumerate() {
            let hit = match chunk.cell(i, c) {
                Cell::Null => m.null,
                Cell::Text(s) => m.is_match(s),
                cell => format_cell_into(kind, cell, &mut buf) && m.is_match(&buf),
            };
            if hit {
                if hits.cells.len() == cap {
                    hits.capped = true;
                    return true;
                }
                hits.cells.push((r as u32, c as u32));
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::{ChunkBuilder, ColumnInfo, ResultSet, SortDir};
    use crate::value::Kind;

    fn m(q: &str, case: bool, word: bool, regex: bool) -> Matcher {
        Matcher::new(q, FindOptions { case, word, regex }).unwrap().unwrap()
    }

    fn all(q: &Matcher, text: &str) -> Vec<Range<usize>> {
        q.find_iter(text).collect()
    }

    #[test]
    fn case_and_literals() {
        let t = "Select sel, SEL.x FROM t; -- a.b a+b";
        assert_eq!(all(&m("sel", false, false, false), t), vec![0..3, 7..10, 12..15]);
        assert_eq!(all(&m("sel", true, false, false), t), vec![7..10]);
        // Literal: regex metacharacters are plain text.
        assert_eq!(all(&m("a.b", false, false, false), t), vec![29..32]);
        assert_eq!(all(&m("a+b", false, false, false), t), vec![33..36]);
        // Unicode case folding.
        assert_eq!(all(&m("CAFÉ", false, false, false), "café"), vec![0..5]);
        assert!(Matcher::new("", FindOptions::default()).unwrap().is_none());
    }

    #[test]
    fn whole_words() {
        let t = "id, p.id, pid, id_x, (id)";
        assert_eq!(all(&m("id", false, true, false), t), vec![0..2, 6..8, 22..24]);
        // A whole word starting inside a rejected match is still found.
        assert_eq!(all(&m("aa", false, true, false), "aaa aa"), vec![4..6]);
        assert_eq!(all(&m("a-a", false, true, false), "xa-a-a"), vec![3..6]);
        assert!(m("id", false, true, false).is_match("x id"));
        assert!(!m("id", false, true, false).is_match("pid"));
    }

    #[test]
    fn regexes() {
        let t = "select 1;\nselect 22;\nupdate x";
        assert_eq!(all(&m(r"\d+", false, false, true), t), vec![7..8, 17..19]);
        // ^ and $ are per line.
        assert_eq!(all(&m("^select", false, false, true), t), vec![0..6, 10..16]);
        assert_eq!(all(&m(";$", false, false, true), t), vec![8..9, 19..20]);
        // Empty matches are skipped (they would select nothing).
        assert_eq!(all(&m("x*", false, false, true), t), vec![28..29]);
        assert!(!m("q*", false, false, true).is_match("abc"));
        assert_eq!(all(&m("SELECT", true, false, true), t), Vec::<Range<usize>>::new());
        assert_eq!(all(&m(r"sel\w*", false, true, true), "select selected"), vec![0..6, 7..15]);
        assert!(Matcher::new("(", FindOptions { regex: true, ..Default::default() }).is_err());
    }

    #[test]
    fn replacement_with_groups() {
        let q = m(r"(\w+)\.(\w+)", false, false, true);
        let t = "p.id, c.name";
        let edits = q.replace_all(t, "$2 of ${1}, $$");
        assert_eq!(edits, vec![(0..4, "id of p, $".to_string()), (6..12, "name of c, $".to_string())]);
        assert_eq!(apply_edits(t, &edits), "id of p, $, name of c, $");
        // A literal query replaces with the text as typed, `$1` included.
        let lit = m("p.", false, false, false);
        assert_eq!(apply_edits("p.id p.x", &lit.replace_all("p.id p.x", "$1")), "$1id $1x");
        // Case-insensitive regex groups.
        let ci = m("(sel)ect", false, false, true);
        assert_eq!(apply_edits("SELECT", &ci.replace_all("SELECT", "<$1>")), "<SEL>");
        // `$1_p` is group 1 then `_p`; `$10` is group 10 (absent: empty); names still work.
        assert_eq!(apply_edits(t, &q.replace_all(t, "$1_$2")), "p_id, c_name");
        let named = m(r"(?P<t>\w+)\.\w+", false, false, true);
        assert_eq!(apply_edits(t, &named.replace_all(t, "$t!")), "p!, c!");
        assert_eq!(group_digits("$1_p ${2}x $$ $10 $name $"), "${1}_p ${2}x $$ ${10} $name $");
    }

    #[test]
    fn next_previous_wrap() {
        let ms = vec![2..4, 10..12, 20..25];
        let key = |r: &Range<usize>| (r.start, r.end);
        // From a caret: the first match at or after it.
        assert_eq!(next_index(&ms, key, (0, 0)), Some(0));
        assert_eq!(next_index(&ms, key, (10, 10)), Some(1));
        // From the current match: the next one, then around.
        assert_eq!(next_index(&ms, key, (10, 12)), Some(2));
        assert_eq!(next_index(&ms, key, (20, 25)), Some(0));
        assert_eq!(next_index(&ms, key, (30, 30)), Some(0));
        assert_eq!(prev_index(&ms, key, (10, 12)), Some(0));
        assert_eq!(prev_index(&ms, key, (2, 4)), Some(2));
        assert_eq!(prev_index(&ms, key, (10, 10)), Some(0));
        assert_eq!(prev_index(&ms, key, (0, 0)), Some(2));
        assert_eq!(nearest_index(&ms, key, (11, 0)), Some(2));
        assert_eq!(nearest_index(&ms, key, (26, 0)), Some(0));
        assert_eq!(next_index(&Vec::<Range<usize>>::new(), key, (0, 0)), None);
        // Grid cells, row-major.
        let cells = vec![(0u32, 2u32), (3, 0), (3, 1)];
        assert_eq!(next_index(&cells, |c| *c, (3, 0)), Some(2));
        assert_eq!(next_index(&cells, |c| *c, (3, 1)), Some(0));
        assert_eq!(prev_index(&cells, |c| *c, (0, 2)), Some(2));
        assert_eq!(prev_index(&cells, |c| *c, (2, 9)), Some(0));
    }

    fn result(rows: &[(i64, Option<&str>)], chunk_every: &[usize]) -> std::sync::Arc<ResultSet> {
        let cols = vec![
            ColumnInfo { name: "id".into(), type_name: "int8".into(), type_oid: 20, kind: Kind::Int },
            ColumnInfo { name: "name".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text },
        ];
        let rs = ResultSet::new("x");
        rs.set_columns(cols.clone());
        let mut b = ChunkBuilder::new(&cols);
        for (i, (id, name)) in rows.iter().enumerate() {
            b.push_i64(0, *id);
            match name {
                Some(n) => b.push_text(1, n),
                None => b.push_null(1),
            }
            b.end_row();
            if chunk_every.contains(&i) {
                rs.push_chunk(b.finish());
            }
        }
        rs.push_chunk(b.finish());
        rs
    }

    #[test]
    fn grid_hits_across_chunks_and_sort() {
        let rows: Vec<(i64, Option<&str>)> =
            vec![(10, Some("ana")), (11, None), (12, Some("Bruno")), (110, Some("")), (13, Some("analu")), (14, Some("x11"))];
        // Chunks of 1, 3 and 2 rows.
        let rs = result(&rows, &[0, 3]);
        let snap = rs.snapshot();
        let no = AtomicBool::new(false);
        let run = |q: &Matcher, snap: &Snapshot, rows: Range<usize>| {
            let mut h = CellHits::default();
            assert!(search_rows(snap, rows, q, 100, &no, &mut h));
            h.cells
        };
        // Numbers match as shown; text in any chunk; "" and NULL never for text.
        assert_eq!(run(&m("11", false, false, false), &snap, 0..6), vec![(1, 0), (3, 0), (5, 1)]);
        assert_eq!(run(&m("an", false, false, false), &snap, 0..6), vec![(0, 1), (4, 1)]);
        assert_eq!(run(&m("ana", false, true, false), &snap, 0..6), vec![(0, 1)]);
        assert_eq!(run(&m("BRUNO", true, false, false), &snap, 0..6), Vec::<(u32, u32)>::new());
        // A literal NULL finds the NULL cells (and text reading "null").
        assert_eq!(run(&m("null", false, false, false), &snap, 0..6), vec![(1, 1)]);
        assert_eq!(run(&m("nul", false, false, false), &snap, 0..6), Vec::<(u32, u32)>::new());
        assert_eq!(run(&m("NULL", false, false, true), &snap, 0..6), Vec::<(u32, u32)>::new());
        // Incremental: a range of rows only.
        assert_eq!(run(&m("1", false, false, false), &snap, 4..6), vec![(4, 0), (5, 0), (5, 1)]);
        // Sorted (110, 14, 13, 12, 11, 10): display rows, not storage rows.
        rs.sort(Some((0, SortDir::Desc)));
        let snap = rs.snapshot();
        assert_eq!(run(&m("11", false, false, false), &snap, 0..6), vec![(0, 0), (1, 1), (4, 0)]);
        // The cap stops the search and says so.
        let mut h = CellHits::default();
        assert!(search_rows(&snap, 0..6, &m("1", false, false, false), 2, &no, &mut h));
        assert_eq!((h.cells.len(), h.capped), (2, true));
        // Cancelled before the first row.
        let yes = AtomicBool::new(true);
        assert!(!search_rows(&snap, 0..6, &m("1", false, false, false), 10, &yes, &mut CellHits::default()));
    }
}
