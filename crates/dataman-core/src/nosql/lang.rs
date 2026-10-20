// The editor re-lexes incrementally: never look behind a token's start, and never let a token's
// extent depend on text past the whitespace after it (its kind may look at the next char only).

use std::collections::HashMap;

use crate::complete::{Candidate, CandidateKind, Completion, MAX_ITEMS};
use crate::config::Engine;
use crate::sql::{Token, TokenKind};

use super::shell::{is_ident_char, is_ident_start};

#[derive(Clone, Debug, Default)]
pub struct Hints {
    pub collections: Vec<String>,
    pub fields: HashMap<String, Vec<(String, String)>>,
}

pub fn tokenize(engine: Engine, text: &str) -> Vec<Token> {
    match engine {
        Engine::Elasticsearch | Engine::OpenSearch => console_tokens(text),
        Engine::Redis => redis_tokens(text),
        _ => shell_tokens(text),
    }
}

fn redis_tokens(text: &str) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = match c {
            c if c.is_ascii_whitespace() => {
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                TokenKind::Whitespace
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                TokenKind::LineComment
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                TokenKind::LineComment
            }
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                TokenKind::String
            }
            _ => {
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += text[i..].chars().next().map_or(1, char::len_utf8);
                }
                let w = &text[start..i];
                if super::redis::is_command(w) {
                    TokenKind::Keyword
                } else if w.parse::<f64>().is_ok() {
                    TokenKind::Number
                } else {
                    TokenKind::Ident
                }
            }
        };
        out.push(Token { kind, start, end: i });
    }
    out
}

fn console_tokens(text: &str) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let path_char = |x: u8| !x.is_ascii_whitespace() && x != b'{' && x != b'[';
        let kind = match c {
            b'/' if !matches!(b.get(i + 1), Some(b'/' | b'*')) => {
                while i < b.len() && path_char(b[i]) {
                    i += 1;
                }
                TokenKind::Ident
            }
            b'_' if b.get(i + 1).is_some_and(u8::is_ascii_alphabetic) => {
                let mut j = i;
                while j < b.len() && path_char(b[j]) && b[j] != b'"' && b[j] != b':' && b[j] != b',' && b[j] != b'}' {
                    j += 1;
                }
                if b.get(j) == Some(&b':') {
                    i = j;
                    TokenKind::QuotedIdent
                } else {
                    i = j;
                    TokenKind::Ident
                }
            }
            c if c.is_ascii_uppercase() => {
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if super::console::METHODS.contains(&&text[start..i]) {
                    TokenKind::Keyword
                } else {
                    let t = shell_tokens(&text[start..]);
                    let first = t.first().copied().unwrap_or(Token { kind: TokenKind::Ident, start: 0, end: 1 });
                    i = start + first.end;
                    first.kind
                }
            }
            _ => {
                let line_end = text[i..].find('\n').map_or(b.len(), |n| i + n + 1);
                let t = shell_tokens(&text[i..line_end]);
                let first = t.first().copied().unwrap_or(Token { kind: TokenKind::Unknown, start: 0, end: 1 });
                i += first.end.max(1);
                first.kind
            }
        };
        out.push(Token { kind, start, end: i });
    }
    out
}

const WORDS: &[&str] = &[
    "db", "true", "false", "null", "undefined", "new", "show", "use", "NaN", "Infinity",
    "var", "let", "const", "function", "return", "if", "else", "for", "of", "in", "while", "do", "break", "continue", "try",
    "catch", "finally", "throw", "typeof", "instanceof", "this", "class", "switch", "case", "default", "delete", "void",
];

fn shell_tokens(text: &str) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let push = |out: &mut Vec<Token>, kind, start, end| out.push(Token { kind, start, end });
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = match c {
            c if c.is_ascii_whitespace() => {
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                TokenKind::Whitespace
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                TokenKind::LineComment
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                TokenKind::LineComment
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = text[i + 2..].find("*/").map_or(b.len(), |j| i + 2 + j + 2);
                TokenKind::BlockComment
            }
            b'/' if out.iter().rev().find(|t: &&Token| !matches!(t.kind, TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment)).is_some_and(|t| {
                matches!(t.kind, TokenKind::Ident | TokenKind::Number) || (t.kind == TokenKind::Punct && matches!(b[t.start], b')' | b']'))
            }) => {
                i += 1;
                TokenKind::Operator
            }
            b'/' => {
                i += 1;
                let mut class = false;
                while i < b.len() && b[i] != b'\n' && (b[i] != b'/' || class) {
                    match b[i] {
                        b'\\' => i += 1,
                        b'[' => class = true,
                        b']' => class = false,
                        _ => {}
                    }
                    i += 1;
                }
                if i < b.len() && b[i] == b'/' {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                }
                TokenKind::String
            }
            b'`' => {
                i += 1;
                while i < b.len() && b[i] != b'`' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                TokenKind::String
            }
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                if b.get(i) == Some(&b':') { TokenKind::QuotedIdent } else { TokenKind::String }
            }
            b'0'..=b'9' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                    i += 1;
                }
                TokenKind::Number
            }
            b'-' if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                TokenKind::Number
            }
            c if is_ident_start(c as char) => {
                while i < b.len() && is_ident_char(b[i] as char) {
                    i += 1;
                }
                let word = &text[start..i];
                if word.starts_with('$') {
                    TokenKind::Keyword
                } else if b.get(i) == Some(&b':') {
                    TokenKind::QuotedIdent
                } else if WORDS.contains(&word) {
                    TokenKind::Keyword
                } else {
                    TokenKind::Ident
                }
            }
            b'(' | b')' | b'[' | b']' | b'{' | b'}' | b',' | b';' | b'.' | b':' => {
                i += 1;
                TokenKind::Punct
            }
            _ => {
                // One whole character (never inside a multi-byte one).
                i += text[i..].chars().next().map_or(1, char::len_utf8);
                TokenKind::Operator
            }
        };
        push(&mut out, kind, start, i);
    }
    out
}

const DB_METHODS: &[(&str, &str)] = &[
    ("getCollection", "(name)"),
    ("getSiblingDB", "(name)"),
    ("runCommand", "(command)"),
    ("adminCommand", "(command)"),
    ("aggregate", "(pipeline)"),
    ("getCollectionNames", "()"),
    ("createCollection", "(name)"),
    ("dropDatabase", "()"),
    ("stats", "()"),
    ("serverStatus", "()"),
    ("version", "()"),
    ("currentOp", "()"),
];

const COLL_METHODS: &[(&str, &str)] = &[
    ("find", "(filter, projection)"),
    ("findOne", "(filter, projection)"),
    ("aggregate", "(pipeline)"),
    ("countDocuments", "(filter)"),
    ("estimatedDocumentCount", "()"),
    ("distinct", "(field, filter)"),
    ("insertOne", "(document)"),
    ("insertMany", "(documents)"),
    ("updateOne", "(filter, update)"),
    ("updateMany", "(filter, update)"),
    ("replaceOne", "(filter, document)"),
    ("deleteOne", "(filter)"),
    ("deleteMany", "(filter)"),
    ("createIndex", "(keys, options)"),
    ("dropIndex", "(name)"),
    ("getIndexes", "()"),
    ("stats", "()"),
    ("renameCollection", "(name)"),
    ("drop", "()"),
];

const CURSOR_METHODS: &[(&str, &str)] =
    &[("sort", "(spec)"), ("limit", "(n)"), ("skip", "(n)"), ("projection", "(spec)"), ("count", "()"), ("explain", "(verbosity)"), ("toArray", "()")];

const OPERATORS: &[&str] = &[
    "$eq", "$ne", "$gt", "$gte", "$lt", "$lte", "$in", "$nin", "$and", "$or", "$nor", "$not", "$exists", "$type", "$regex",
    "$options", "$elemMatch", "$size", "$all", "$expr", "$text", "$search", "$mod", "$where", "$set", "$unset", "$inc",
    "$push", "$pull", "$addToSet", "$pop", "$rename", "$min", "$max", "$mul", "$currentDate", "$setOnInsert", "$each",
];

const STAGES: &[&str] = &[
    "$match", "$group", "$project", "$sort", "$limit", "$skip", "$unwind", "$lookup", "$addFields", "$set", "$unset",
    "$count", "$facet", "$bucket", "$sample", "$replaceRoot", "$sortByCount", "$out", "$merge", "$unionWith",
    "$sum", "$avg", "$first", "$last", "$push", "$addToSet", "$concat", "$cond", "$ifNull", "$dateToString", "$toString",
    "$size", "$map", "$filter", "$arrayElemAt",
];

const CONSTRUCTORS: &[(&str, &str)] = &[
    ("ObjectId", "(\"\")"),
    ("ISODate", "(\"\")"),
    ("NumberLong", "()"),
    ("NumberInt", "()"),
    ("NumberDecimal", "(\"\")"),
    ("UUID", "(\"\")"),
];

fn call(name: &str, args: &str, kind: CandidateKind) -> Candidate {
    let caret = if args.starts_with("(\"") { name.len() + 2 } else { name.len() + 1 };
    let insert = if args.starts_with("(\"") { format!("{name}(\"\")") } else { format!("{name}()") };
    Candidate { label: name.to_string(), kind, detail: args.to_string(), insert, caret: Some(caret) }
}

fn word(label: &str, kind: CandidateKind, detail: &str) -> Candidate {
    Candidate { label: label.to_string(), kind, detail: detail.to_string(), insert: label.to_string(), caret: None }
}

fn statement_collection(src: &str, at: usize) -> Option<String> {
    let range = super::mql::statement_at(src, at)?;
    let text = &src[range];
    let rest = text.trim_start().strip_prefix("db")?;
    if let Some(r) = rest.strip_prefix(".getCollection(") {
        let q = r.chars().next()?;
        let r = &r[1..];
        return Some(r[..r.find(q)?].to_string());
    }
    let r = rest.strip_prefix('.')?;
    let name: String = r.chars().take_while(|c| is_ident_char(*c) || *c == '.').collect();
    let name = name.rsplit_once('.').map(|(c, _)| c.to_string()).unwrap_or(name);
    (!name.is_empty()).then_some(name)
}

#[derive(Debug, PartialEq)]
enum Spot {
    Top,
    Key,
    Value,
}

fn spot(tokens: &[Token], src: &str) -> Spot {
    let mut stack: Vec<(u8, bool)> = Vec::new();
    for t in tokens {
        if t.kind != TokenKind::Punct {
            if let Some(top) = stack.last_mut()
                && top.0 == b'{'
                && !t.kind.is_trivia()
            {
                top.1 = false;
            }
            continue;
        }
        match src.as_bytes()[t.start] {
            b'{' => stack.push((b'{', true)),
            b'[' | b'(' => stack.push((src.as_bytes()[t.start], false)),
            b'}' | b']' | b')' => {
                stack.pop();
            }
            b',' => {
                if let Some(top) = stack.last_mut() {
                    top.1 = top.0 == b'{';
                }
            }
            b':' => {
                if let Some(top) = stack.last_mut() {
                    top.1 = false;
                }
            }
            b';' => stack.clear(),
            _ => {}
        }
    }
    match stack.last() {
        None => Spot::Top,
        Some((b'{', true)) => Spot::Key,
        Some(_) => Spot::Value,
    }
}

pub fn complete(engine: Engine, src: &str, cursor: usize, hints: &Hints) -> Completion {
    let cursor = cursor.min(src.len());
    if matches!(engine, Engine::Elasticsearch | Engine::OpenSearch) {
        return complete_console(src, cursor, hints);
    }
    if engine == Engine::Redis {
        let line_start = src[..cursor].rfind('\n').map_or(0, |i| i + 1);
        let start = src[..cursor].char_indices().rev().take_while(|(_, c)| !c.is_whitespace()).last().map_or(cursor, |(i, _)| i);
        let typed = &src[start..cursor];
        let first = src[line_start..start].trim().is_empty();
        let up = typed.to_ascii_uppercase();
        let mut items: Vec<Candidate> = if first {
            let mut v: Vec<&str> = super::redis::commands().filter(|c| c.starts_with(&up)).collect();
            v.sort_unstable();
            v.dedup();
            v.into_iter().map(|c| word(c, CandidateKind::Keyword, "")).collect()
        } else {
            hints.collections.iter().filter(|k| k.starts_with(typed)).map(|k| word(&super::redis::quote(k), CandidateKind::Table, "key")).collect()
        };
        items.truncate(MAX_ITEMS);
        return Completion { replace: start..cursor, items };
    }
    if engine != Engine::MongoDb {
        return Completion { replace: cursor..cursor, items: Vec::new() };
    }
    let before = &src[..cursor];
    let start = before.char_indices().rev().take_while(|(_, c)| is_ident_char(*c)).last().map_or(cursor, |(i, _)| i);
    let partial = &src[start..cursor];
    let tokens = shell_tokens(&src[..start]);
    if let Some(last) = tokens.last()
        && last.kind == TokenKind::String
        && last.end == start
        && {
            let t = &src[last.start..last.end];
            t.len() == 1 || !t[1..].ends_with(&t[..1])
        }
    {
        let inside = &src[last.start + 1..cursor];
        if let Some(path) = inside.strip_prefix('$') {
            let coll = statement_collection(src, cursor).unwrap_or_default();
            let at = last.start + 2;
            let items = fields(hints, &coll).filter(|(f, _)| f.starts_with(path)).map(|(f, t)| word(f, CandidateKind::Column, t)).take(MAX_ITEMS).collect();
            return Completion { replace: at..cursor, items };
        }
        return Completion { replace: cursor..cursor, items: Vec::new() };
    }
    let significant: Vec<&Token> = tokens.iter().filter(|t| !t.kind.is_trivia()).collect();
    let text_of = |t: &Token| &src[t.start..t.end];
    let prev = significant.last().map(|t| text_of(t));
    let mut items: Vec<Candidate> = Vec::new();
    if prev == Some(".") {
        let n = significant.len();
        let before_dot = significant.get(n.wrapping_sub(2)).map(|t| text_of(t));
        let two_back = significant.get(n.wrapping_sub(3)).map(|t| text_of(t));
        if before_dot == Some("db") {
            items.extend(hints.collections.iter().map(|c| word(c, CandidateKind::Table, "collection")));
            items.extend(DB_METHODS.iter().map(|(m, a)| call(m, a, CandidateKind::Function)));
        } else if before_dot == Some(")") {
            let mut depth = 0;
            let mut name = None;
            for j in (0..n - 1).rev() {
                match text_of(significant[j]) {
                    ")" | "]" | "}" => depth += 1,
                    "(" | "[" | "{" => {
                        depth -= 1;
                        if depth == 0 {
                            name = j.checked_sub(1).map(|k| text_of(significant[k]));
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if name == Some("getCollection") {
                items.extend(COLL_METHODS.iter().map(|(m, a)| call(m, a, CandidateKind::Function)));
            } else {
                items.extend(CURSOR_METHODS.iter().map(|(m, a)| call(m, a, CandidateKind::Function)));
            }
        } else if two_back == Some(".") || two_back == Some("db") || before_dot.is_some_and(|w| w.chars().all(is_ident_char)) {
            items.extend(COLL_METHODS.iter().map(|(m, a)| call(m, a, CandidateKind::Function)));
        }
    } else {
        match spot(&tokens, src) {
            Spot::Top => {
                items.extend(["db", "show dbs", "show collections", "use "].iter().map(|w| word(w.trim_end(), CandidateKind::Keyword, "")));
                if let Some(c) = items.iter_mut().find(|c| c.label == "use") {
                    c.insert = "use ".into();
                }
            }
            Spot::Key => {
                let coll = statement_collection(src, cursor).unwrap_or_default();
                let in_pipeline = src[..cursor].rfind("aggregate(").is_some_and(|i| super::mql::statement_at(src, cursor).is_some_and(|r| r.start <= i));
                if partial.starts_with('$') || partial.is_empty() {
                    let ops: &[&str] = if in_pipeline { STAGES } else { OPERATORS };
                    items.extend(ops.iter().map(|o| word(o, CandidateKind::Keyword, if in_pipeline { "stage" } else { "operator" })));
                }
                if !partial.starts_with('$') {
                    items.extend(fields(hints, &coll).map(|(f, t)| word(f, CandidateKind::Column, t)));
                }
            }
            Spot::Value => {
                items.extend(CONSTRUCTORS.iter().map(|(m, a)| call(m, a, CandidateKind::Function)));
                items.extend(["true", "false", "null"].iter().map(|w| word(w, CandidateKind::Keyword, "")));
            }
        }
    }
    let lower = partial.to_ascii_lowercase();
    let mut items: Vec<Candidate> = items.into_iter().filter(|c| c.label.to_ascii_lowercase().starts_with(&lower) || (lower.len() >= 3 && c.label.to_ascii_lowercase().contains(&lower))).collect();
    items.sort_by_key(|c| !c.label.to_ascii_lowercase().starts_with(&lower));
    items.truncate(MAX_ITEMS);
    Completion { replace: start..cursor, items }
}

const ENDPOINTS: &[&str] = &[
    "_search", "_count", "_doc/", "_mapping", "_settings", "_bulk", "_update_by_query", "_delete_by_query", "_cat/indices?v",
    "_cat/health?v", "_cat/shards?v", "_cluster/health", "_cluster/stats", "_nodes/stats", "_aliases", "_refresh",
];

const DSL: &[&str] = &[
    "query", "match", "match_all", "match_phrase", "multi_match", "term", "terms", "range", "bool", "must", "should", "filter",
    "must_not", "exists", "prefix", "wildcard", "regexp", "fuzzy", "ids", "nested", "query_string", "simple_query_string",
    "size", "from", "sort", "_source", "aggs", "highlight", "track_total_hits", "gte", "lte", "gt", "lt", "order", "fields",
    "minimum_should_match", "boost",
];

fn complete_console(src: &str, cursor: usize, hints: &Hints) -> Completion {
    let line_start = src[..cursor].rfind('\n').map_or(0, |i| i + 1);
    let line = &src[line_start..cursor];
    let word_start = |stop: &dyn Fn(char) -> bool| src[..cursor].char_indices().rev().take_while(|(_, c)| !stop(*c)).last().map_or(cursor, |(i, _)| i);
    if let Some(m) = super::console::METHODS.iter().find(|m| line.starts_with(*m)) {
        let rest = &line[m.len()..];
        if rest.starts_with(' ') {
            let start = word_start(&|c: char| c == '/' || c == ' ');
            let typed = &src[start..cursor];
            let in_index = !rest.trim_start().trim_start_matches('/').contains('/');
            let mut items: Vec<Candidate> = Vec::new();
            if in_index && !typed.starts_with('_') {
                items.extend(hints.collections.iter().filter(|c| !c.starts_with('.') || typed.starts_with('.')).map(|c| word(c, CandidateKind::Table, "index")));
            }
            items.extend(ENDPOINTS.iter().map(|e| word(e, CandidateKind::Keyword, "")));
            items.retain(|c| c.label.starts_with(typed));
            items.truncate(MAX_ITEMS);
            return Completion { replace: start..cursor, items };
        }
        return Completion { replace: cursor..cursor, items: Vec::new() };
    }
    let start = word_start(&|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '@'));
    let typed = &src[start..cursor];
    let req = super::console::statement_at(src, cursor).map(|r| &src[r]).unwrap_or("");
    let index = req.split_whitespace().nth(1).map(|p| p.trim_start_matches('/').split('/').next().unwrap_or("").to_string()).unwrap_or_default();
    let body_at = super::console::statement_at(src, cursor).map_or(0, |r| r.start + req.find('\n').unwrap_or(req.len()));
    let quoted = start > 0 && matches!(src.as_bytes()[start - 1], b'"' | b'\'');
    let upto = if quoted { start - 1 } else { start };
    let tokens = shell_tokens(&src[body_at.min(upto)..upto]);
    let mut items: Vec<Candidate> = Vec::new();
    match spot(&tokens, &src[body_at.min(upto)..upto]) {
        Spot::Key | Spot::Top => {
            items.extend(DSL.iter().map(|k| word(k, CandidateKind::Keyword, "")));
            items.extend(fields(hints, &index).filter(|(f, _)| *f != "_id").map(|(f, t)| word(f, CandidateKind::Column, t)));
        }
        Spot::Value if quoted => items.extend(fields(hints, &index).map(|(f, t)| word(f, CandidateKind::Column, t))),
        Spot::Value => items.extend(["true", "false", "null"].iter().map(|w| word(w, CandidateKind::Keyword, ""))),
    }
    items.retain(|c| c.label.starts_with(typed));
    items.truncate(MAX_ITEMS);
    Completion { replace: start..cursor, items }
}

fn fields<'a>(hints: &'a Hints, coll: &str) -> impl Iterator<Item = (&'a str, &'a str)> {
    hints.fields.get(coll).into_iter().flatten().map(|(f, t)| (f.as_str(), t.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<(&str, TokenKind)> {
        tokenize(Engine::MongoDb, s).into_iter().filter(|t| t.kind != TokenKind::Whitespace).map(|t| (&s[t.start..t.end], t.kind)).collect()
    }

    #[test]
    fn colors_the_shell() {
        use TokenKind::*;
        assert_eq!(
            kinds("db.users.find({name: 'a', \"x\": /b+/i, n: {$gt: -1.5}}) // hi"),
            vec![
                ("db", Keyword), (".", Punct), ("users", Ident), (".", Punct), ("find", Ident), ("(", Punct), ("{", Punct),
                ("name", QuotedIdent), (":", Punct), ("'a'", String), (",", Punct), ("\"x\"", QuotedIdent), (":", Punct),
                ("/b+/i", String), (",", Punct), ("n", QuotedIdent), (":", Punct), ("{", Punct), ("$gt", Keyword), (":", Punct),
                ("-1.5", Number), ("}", Punct), ("}", Punct), (")", Punct), ("// hi", LineComment),
            ]
        );
        let k = kinds("const n = total / 2; let r = /a/ \nprint(`${n}`)");
        assert!(k.contains(&("const", Keyword)) && k.contains(&("let", Keyword)) && k.contains(&("/", Operator)) && k.contains(&("/a/", String)) && k.contains(&("`${n}`", String)), "{k:?}");
        let s = "db.é.find({a: \"open";
        let t = tokenize(Engine::MongoDb, s);
        assert_eq!(t.first().unwrap().start, 0);
        assert_eq!(t.last().unwrap().end, s.len());
        assert!(t.windows(2).all(|w| w[0].end == w[1].start));
    }

    fn labels(s: &str, hints: &Hints) -> Vec<String> {
        let at = s.find('|').unwrap();
        let src = s.replace('|', "");
        complete(Engine::MongoDb, &src, at, hints).items.into_iter().map(|c| c.label).collect()
    }

    #[test]
    fn completes_names_methods_operators_and_fields() {
        let mut h = Hints { collections: vec!["users".into(), "orders".into()], ..Default::default() };
        h.fields.insert("users".into(), vec![("_id".into(), "objectId".into()), ("name".into(), "string".into()), ("address.city".into(), "string".into())]);
        assert_eq!(labels("db.us|", &h), ["users"]);
        assert!(labels("db.|", &h).starts_with(&["users".to_string(), "orders".to_string()]));
        assert!(labels("db.users.f|", &h).starts_with(&["find".to_string(), "findOne".to_string()]));
        assert_eq!(labels("db.users.find({}).li|", &h), ["limit"]);
        assert!(labels("db.getCollection('users').cou|", &h).contains(&"countDocuments".to_string()));
        assert_eq!(labels("db.users.find({na|", &h), ["name"]);
        assert_eq!(labels("db.users.find({name: 'x', $o|", &h), ["$or", "$options"]);
        assert!(labels("db.users.find({a: {$|", &h).contains(&"$gt".to_string()));
        assert!(labels("db.users.aggregate([{$m|", &h).contains(&"$match".to_string()));
        assert_eq!(labels("db.users.aggregate([{$group: {_id: \"$add|", &h), ["address.city"]);
        assert!(labels("db.users.find({_id: Obj|", &h).contains(&"ObjectId".to_string()));
        assert!(labels("sh|", &h).contains(&"show dbs".to_string()));
        let c = complete(Engine::MongoDb, "db.users.fin", 12, &h);
        assert_eq!((c.replace, c.items[0].insert.as_str(), c.items[0].caret), (9..12, "find()", Some(5)));
    }

    #[test]
    fn the_console_colors_and_completes() {
        use TokenKind::*;
        let s = "GET /orders/_search\n{\"query\": {match: {status: 'paid'}}, size: 10}";
        let k: Vec<(&str, TokenKind)> = tokenize(Engine::Elasticsearch, s).into_iter().filter(|t| t.kind != Whitespace).map(|t| (&s[t.start..t.end], t.kind)).collect();
        assert_eq!(&k[..2], &[("GET", Keyword), ("/orders/_search", Ident)]);
        assert!(k.contains(&("\"query\"", QuotedIdent)) && k.contains(&("match", QuotedIdent)) && k.contains(&("'paid'", String)) && k.contains(&("10", Number)), "{k:?}");
        let t = tokenize(Engine::Elasticsearch, "POST _bulk\n{\"index\": {}}\nDELETE x");
        assert!(t.windows(2).all(|w| w[0].end == w[1].start));
        let mut h = Hints { collections: vec!["orders".into(), "logs".into()], ..Default::default() };
        h.fields.insert("orders".into(), vec![("status".into(), "keyword".into()), ("total".into(), "double".into())]);
        let labels = |s: &str| {
            let at = s.find('|').unwrap();
            complete(Engine::Elasticsearch, &s.replace('|', ""), at, &h).items.into_iter().map(|c| c.label).collect::<Vec<_>>()
        };
        assert_eq!(labels("GET /or|"), ["orders"]);
        assert_eq!(labels("GET /orders/_sea|"), ["_search"]);
        assert!(labels("GET /orders/_search\n{\"query\": {\"ma|").starts_with(&["match".to_string(), "match_all".to_string()]));
        assert_eq!(labels("GET /orders/_search\n{\"query\": {\"term\": {\"sta|"), ["status"]);
    }

    #[test]
    fn redis_colors_and_completes() {
        use TokenKind::*;
        let s = "HSET user:1 name \"Ada L\" age 36 # note";
        let k: Vec<(&str, TokenKind)> = tokenize(Engine::Redis, s).into_iter().filter(|t| t.kind != Whitespace).map(|t| (&s[t.start..t.end], t.kind)).collect();
        assert_eq!(k, [("HSET", Keyword), ("user:1", Ident), ("name", Ident), ("\"Ada L\"", String), ("age", Ident), ("36", Number), ("# note", LineComment)]);
        let h = Hints { collections: vec!["user:1".into(), "user:2".into(), "queue".into()], ..Default::default() };
        let labels = |s: &str| complete(Engine::Redis, s, s.len(), &h).items.into_iter().map(|c| c.label).collect::<Vec<_>>();
        assert!(labels("hge").starts_with(&["HGET".to_string(), "HGETALL".to_string()]));
        assert_eq!(labels("HGETALL us"), ["user:1", "user:2"]);
    }
}
