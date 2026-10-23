use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
    /// A scalar JSON cannot spell (MongoDB's ObjectId, ISODate, NumberLong…), shown as written.
    Other { text: String, type_name: &'static str },
}

pub type Path = Vec<usize>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}, col {}: {}", self.line, self.col, self.message)
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: impl Into<String>) -> ParseError {
        let upto = &self.text[..self.i.min(self.text.len())];
        let line = upto.matches('\n').count() + 1;
        let col = upto.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        ParseError { line, col, message: msg.into() }
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err(format!("expected '{}'", c as char)))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Node, ParseError> {
        if depth > 512 {
            return Err(self.err("nested too deep"));
        }
        self.ws();
        match self.peek() {
            None => Err(self.err("unexpected end")),
            Some(b'{') => {
                self.i += 1;
                let mut members = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(Node::Object(members));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return Err(self.err("expected a key"));
                    }
                    let k = self.string()?;
                    self.ws();
                    self.expect(b':')?;
                    let v = self.value(depth + 1)?;
                    members.push((k, v));
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Node::Object(members));
                        }
                        _ => return Err(self.err("expected ',' or '}'")),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(Node::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Node::Array(items));
                        }
                        _ => return Err(self.err("expected ',' or ']'")),
                    }
                }
            }
            Some(b'"') => Ok(Node::String(self.string()?)),
            Some(b't') => self.word("true", Node::Bool(true)),
            Some(b'f') => self.word("false", Node::Bool(false)),
            Some(b'n') => self.word("null", Node::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn word(&mut self, w: &str, n: Node) -> Result<Node, ParseError> {
        if self.s[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(n)
        } else {
            Err(self.err("unexpected word"))
        }
    }

    fn number(&mut self) -> Result<Node, ParseError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.i += 1;
            }
            p.i > s
        };
        if self.peek() == Some(b'0') {
            self.i += 1;
        } else if !digits(self) {
            return Err(self.err("expected a digit"));
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !digits(self) {
                return Err(self.err("expected a digit"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return Err(self.err("expected a digit"));
            }
        }
        Ok(Node::Number(self.text[start..self.i].to_string()))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else { return Err(self.err("unterminated string")) };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(e) = self.peek() else { return Err(self.err("unterminated string")) };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&hi) {
                                if !self.s[self.i..].starts_with(b"\\u") {
                                    return Err(self.err("invalid surrogate pair"));
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF))
                            } else {
                                char::from_u32(hi)
                            };
                            out.push(ch.ok_or_else(|| self.err("invalid escape"))?);
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                c if c < 0x20 => return Err(self.err("control character in string")),
                _ => {
                    let ch = self.text[self.i..].chars().next().unwrap_or('\u{fffd}');
                    out.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let h = self.text.get(self.i..self.i + 4).ok_or_else(|| self.err("invalid escape"))?;
        let v = u32::from_str_radix(h, 16).map_err(|_| self.err("invalid escape"))?;
        self.i += 4;
        Ok(v)
    }
}

pub fn parse(text: &str) -> Result<Node, ParseError> {
    let mut p = Parser { s: text.as_bytes(), i: 0, text };
    let v = p.value(0)?;
    p.ws();
    if p.i < p.s.len() {
        return Err(p.err("unexpected text after the value"));
    }
    Ok(v)
}

fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

impl Node {
    pub fn compact(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, None, 0);
        s
    }

    pub fn pretty(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, Some(2), 0);
        s
    }

    fn write(&self, out: &mut String, indent: Option<usize>, level: usize) {
        let nl = |out: &mut String, level: usize| {
            if let Some(n) = indent {
                out.push('\n');
                out.push_str(&" ".repeat(n * level));
            }
        };
        match self {
            Node::Null => out.push_str("null"),
            Node::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Node::Number(n) => out.push_str(n),
            Node::Other { text, .. } => out.push_str(text),
            Node::String(s) => quote(s, out),
            Node::Array(a) if a.is_empty() => out.push_str("[]"),
            Node::Object(o) if o.is_empty() => out.push_str("{}"),
            Node::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                        if indent.is_none() {
                            out.push(' ');
                        }
                    }
                    nl(out, level + 1);
                    v.write(out, indent, level + 1);
                }
                nl(out, level);
                out.push(']');
            }
            Node::Object(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                        if indent.is_none() {
                            out.push(' ');
                        }
                    }
                    nl(out, level + 1);
                    quote(k, out);
                    out.push_str(": ");
                    v.write(out, indent, level + 1);
                }
                nl(out, level);
                out.push('}');
            }
        }
    }

    pub fn is_container(&self) -> bool {
        matches!(self, Node::Array(_) | Node::Object(_))
    }

    pub fn len(&self) -> usize {
        match self {
            Node::Array(a) => a.len(),
            Node::Object(o) => o.len(),
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Node::Null => "null",
            Node::Bool(_) => "bool",
            Node::Number(_) => "number",
            Node::String(_) => "string",
            Node::Array(_) => "array",
            Node::Object(_) => "object",
            Node::Other { type_name, .. } => type_name,
        }
    }

    /// Nodes under this one (its members, theirs, …).
    pub fn count(&self) -> usize {
        match self {
            Node::Array(a) => a.iter().map(|v| 1 + v.count()).sum(),
            Node::Object(o) => o.iter().map(|(_, v)| 1 + v.count()).sum(),
            _ => 0,
        }
    }

    pub fn of_json(j: &serde_json::Value) -> Node {
        use serde_json::Value as J;
        match j {
            J::Null => Node::Null,
            J::Bool(b) => Node::Bool(*b),
            J::Number(n) => Node::Number(n.to_string()),
            J::String(s) => Node::String(s.clone()),
            J::Array(a) => Node::Array(a.iter().map(Node::of_json).collect()),
            J::Object(m) => Node::Object(m.iter().map(|(k, v)| (k.clone(), Node::of_json(v))).collect()),
        }
    }

    /// The shell's notation for what JSON has no word for.
    pub fn of_bson(b: &crate::nosql::bson::Bson) -> Node {
        use crate::nosql::bson::Bson as B;
        match b {
            B::Null => Node::Null,
            B::Boolean(v) => Node::Bool(*v),
            B::Int32(_) | B::Double(_) => Node::Number(crate::nosql::shell::scalar_text(b)),
            B::String(s) | B::Symbol(s) => Node::String(s.clone()),
            B::Array(a) => Node::Array(a.iter().map(Node::of_bson).collect()),
            B::Document(d) => Node::Object(d.iter().map(|(k, v)| (k.clone(), Node::of_bson(v))).collect()),
            other => Node::Other { text: crate::nosql::shell::scalar_text(other), type_name: crate::nosql::shell::type_name(other) },
        }
    }

    pub fn get(&self, path: &[usize]) -> Option<&Node> {
        let Some((&i, rest)) = path.split_first() else { return Some(self) };
        match self {
            Node::Array(a) => a.get(i)?.get(rest),
            Node::Object(o) => o.get(i)?.1.get(rest),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        let Some((&i, rest)) = path.split_first() else { return Some(self) };
        match self {
            Node::Array(a) => a.get_mut(i)?.get_mut(rest),
            Node::Object(o) => o.get_mut(i)?.1.get_mut(rest),
            _ => None,
        }
    }

    pub fn edit_text(&self) -> String {
        match self {
            Node::String(s) => s.clone(),
            n => n.compact(),
        }
    }
}

pub fn leaf_from_text(text: &str) -> Node {
    parse(text.trim()).unwrap_or_else(|_| Node::String(text.to_string()))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub path: Path,
    pub depth: usize,
    pub key: Option<RowKey>,
    pub value: String,
    pub type_name: &'static str,
    pub container: bool,
    pub expanded: bool,
    /// A container's members.
    pub len: usize,
}

impl Row {
    /// What a container holds: "5 keys", "12 items".
    pub fn count_text(&self) -> String {
        let (one, many) = if self.type_name == "array" { ("item", "items") } else { ("key", "keys") };
        format!("{} {}", self.len, if self.len == 1 { one } else { many })
    }

    /// A collapsed container's line: `{…} 5 keys`, `[…] 12 items`; an empty one is `{}` or `[]`.
    pub fn preview(&self) -> String {
        let (open, close) = if self.type_name == "array" { ('[', ']') } else { ('{', '}') };
        if self.len == 0 { format!("{open}{close}") } else { format!("{open}…{close} {}", self.count_text()) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowKey {
    Key(String),
    Index(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub root: Node,
    pub collapsed: HashSet<Path>,
}

impl Doc {
    pub fn new(root: Node) -> Doc {
        Doc { root, collapsed: HashSet::new() }
    }

    pub fn parse(text: &str) -> Result<Doc, ParseError> {
        parse(text).map(Doc::new)
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        self.walk(&self.root, &mut Vec::new(), 0, None, &mut out);
        out
    }

    fn walk(&self, n: &Node, path: &mut Path, depth: usize, key: Option<RowKey>, out: &mut Vec<Row>) {
        let expanded = n.is_container() && !self.collapsed.contains(path);
        let value = match n {
            Node::Array(a) => format!("[{}]", a.len()),
            Node::Object(o) => format!("{{{}}}", o.len()),
            leaf => leaf.compact(),
        };
        out.push(Row { path: path.clone(), depth, key, value, type_name: n.type_name(), container: n.is_container(), expanded, len: n.len() });
        if !expanded {
            return;
        }
        match n {
            Node::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    path.push(i);
                    self.walk(v, path, depth + 1, Some(RowKey::Index(i)), out);
                    path.pop();
                }
            }
            Node::Object(o) => {
                for (i, (k, v)) in o.iter().enumerate() {
                    path.push(i);
                    self.walk(v, path, depth + 1, Some(RowKey::Key(k.clone())), out);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    pub fn toggle(&mut self, path: &[usize]) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_vec());
        }
    }

    pub fn set_text(&mut self, path: &[usize], text: &str) -> bool {
        match self.root.get_mut(path) {
            Some(n) => {
                *n = leaf_from_text(text);
                true
            }
            None => false,
        }
    }

    pub fn rename(&mut self, path: &[usize], key: &str) -> Result<(), String> {
        let Some((&i, parent)) = path.split_last() else { return Err("the root has no key".into()) };
        match self.root.get_mut(parent) {
            Some(Node::Object(o)) => {
                if o.iter().enumerate().any(|(j, (k, _))| j != i && k == key) {
                    return Err(format!("\"{key}\" is already a key"));
                }
                match o.get_mut(i) {
                    Some(m) => {
                        m.0 = key.to_string();
                        Ok(())
                    }
                    None => Err("no such member".into()),
                }
            }
            _ => Err("not an object member".into()),
        }
    }

    pub fn remove(&mut self, path: &[usize]) {
        let Some((&i, parent)) = path.split_last() else {
            self.root = Node::Null;
            return;
        };
        match self.root.get_mut(parent) {
            Some(Node::Array(a)) if i < a.len() => {
                a.remove(i);
            }
            Some(Node::Object(o)) if i < o.len() => {
                o.remove(i);
            }
            _ => {}
        }
        self.collapsed.retain(|p| !p.starts_with(parent) || p.len() <= parent.len());
    }

    pub fn add(&mut self, path: &[usize]) -> Option<Path> {
        let n = self.root.get_mut(path)?;
        let mut out = path.to_vec();
        match n {
            Node::Array(a) => {
                a.push(Node::Null);
                out.push(a.len() - 1);
            }
            Node::Object(o) => {
                let mut k = "key".to_string();
                let mut i = 2;
                while o.iter().any(|(x, _)| *x == k) {
                    k = format!("key{i}");
                    i += 1;
                }
                o.push((k, Node::Null));
                out.push(o.len() - 1);
            }
            _ => {
                let (&i, parent) = path.split_last()?;
                match self.root.get_mut(parent)? {
                    Node::Array(a) => {
                        a.insert(i + 1, Node::Null);
                    }
                    Node::Object(o) => {
                        let mut k = "key".to_string();
                        let mut j = 2;
                        while o.iter().any(|(x, _)| *x == k) {
                            k = format!("key{j}");
                            j += 1;
                        }
                        o.insert(i + 1, (k, Node::Null));
                    }
                    _ => return None,
                }
                out = parent.to_vec();
                out.push(i + 1);
            }
        }
        self.collapsed.remove(path);
        Some(out)
    }

    pub fn expand_all(&mut self) {
        self.collapsed.clear();
    }

    pub fn collapse_all(&mut self) {
        self.collapse_to_level(1);
    }

    /// Members `n` levels down from the root stay in sight: every container at depth `n` or
    /// deeper is collapsed (1 shows the root's members, each folded).
    pub fn collapse_to_level(&mut self, n: usize) {
        let mut out = HashSet::new();
        Self::containers(&self.root, &mut Vec::new(), &mut |p| {
            if p.len() >= n.max(1) {
                out.insert(p.to_vec());
            }
        });
        self.collapsed = out;
    }

    /// The container at `path` and every one under it, open or folded together.
    pub fn set_subtree(&mut self, path: &[usize], open: bool) {
        if open {
            self.collapsed.retain(|p| !p.starts_with(path));
            return;
        }
        let Some(n) = self.root.get(path) else { return };
        let mut at = path.to_vec();
        let mut out = Vec::new();
        Self::containers(n, &mut at, &mut |p| out.push(p.to_vec()));
        // The root is never folded: its rows are the document.
        self.collapsed.extend(out.into_iter().filter(|p| !p.is_empty()));
    }

    /// A fresh document's view: all of it, or two levels when that would be a long read.
    pub fn open_default(&mut self) {
        if self.root.count() > LARGE {
            self.collapse_to_level(2);
        } else {
            self.collapsed.clear();
        }
    }

    /// Every non-empty container at or under `n` (whose path is `path`).
    fn containers(n: &Node, path: &mut Path, f: &mut impl FnMut(&[usize])) {
        let kids: Vec<&Node> = match n {
            Node::Array(a) => a.iter().collect(),
            Node::Object(o) => o.iter().map(|(_, v)| v).collect(),
            _ => return,
        };
        if kids.is_empty() {
            return;
        }
        f(path);
        for (i, k) in kids.into_iter().enumerate() {
            path.push(i);
            Self::containers(k, path, f);
            path.pop();
        }
    }
}

/// Past this many nodes a document opens two levels deep.
pub const LARGE: usize = 200;

pub fn from_pg_array(text: &str) -> Option<Node> {
    let mut t = text.trim();
    if t.starts_with('[') {
        t = t[t.find('=')? + 1..].trim_start();
    }
    let s: Vec<char> = t.chars().collect();
    let mut i = 0;
    let node = pg_level(&s, &mut i)?;
    while i < s.len() && s[i].is_whitespace() {
        i += 1;
    }
    (i == s.len()).then_some(node)
}

fn pg_level(s: &[char], i: &mut usize) -> Option<Node> {
    let ws = |i: &mut usize| {
        while *i < s.len() && s[*i].is_whitespace() {
            *i += 1;
        }
    };
    ws(i);
    if s.get(*i) != Some(&'{') {
        return None;
    }
    *i += 1;
    let mut items = Vec::new();
    ws(i);
    if s.get(*i) == Some(&'}') {
        *i += 1;
        return Some(Node::Array(items));
    }
    loop {
        ws(i);
        match s.get(*i)? {
            '{' => items.push(pg_level(s, i)?),
            '"' => {
                *i += 1;
                let mut out = String::new();
                loop {
                    match s.get(*i)? {
                        '\\' => {
                            *i += 1;
                            out.push(*s.get(*i)?);
                        }
                        '"' => break,
                        c => out.push(*c),
                    }
                    *i += 1;
                }
                *i += 1;
                items.push(Node::String(out));
            }
            _ => {
                let start = *i;
                while *i < s.len() && !matches!(s[*i], ',' | '}') {
                    *i += 1;
                }
                let word = s[start..*i].iter().collect::<String>().trim_end().to_string();
                items.push(match parse(&word) {
                    _ if word.eq_ignore_ascii_case("null") => Node::Null,
                    Ok(n @ Node::Number(_)) => n,
                    _ => Node::String(word),
                });
            }
        }
        ws(i);
        match s.get(*i)? {
            ',' => *i += 1,
            '}' => {
                *i += 1;
                return Some(Node::Array(items));
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_keeps_order_and_numbers() {
        let text = r#"{"z": 1.50, "a": [true, null, "x\ny"], "e": {}, "u": "é😀"}"#;
        let n = parse(text).unwrap();
        assert_eq!(n.compact(), r#"{"z": 1.50, "a": [true, null, "x\ny"], "e": {}, "u": "é😀"}"#);
        assert_eq!(parse(&n.pretty()).unwrap(), n, "pretty text parses back to the same tree");
        assert_eq!(n.pretty().lines().count(), 10);
        assert_eq!(parse("  -0.5e+3 ").unwrap(), Node::Number("-0.5e+3".into()));
    }

    #[test]
    fn errors_say_where() {
        let e = parse("{\n  \"a\": 1,\n  \"b\" 2\n}").unwrap_err();
        assert_eq!((e.line, e.col), (3, 7));
        assert_eq!(e.to_string(), "line 3, col 7: expected ':'");
        assert!(parse("[1, 2").is_err());
        assert!(parse("{\"a\": 1} x").unwrap_err().message.contains("after the value"));
        assert!(parse("01").is_err());
        assert!(parse("\"a\tb\"").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn rows_follow_expansion() {
        let mut d = Doc::parse(r#"{"a": 1, "b": {"c": [1, 2]}}"#).unwrap();
        let rows = d.rows();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].value, "{2}");
        assert_eq!(rows[2].key, Some(RowKey::Key("b".into())));
        assert_eq!(rows[4].path, vec![1, 0, 0]);
        assert_eq!(rows[4].key, Some(RowKey::Index(0)));
        assert_eq!(rows[4].depth, 3);
        d.toggle(&[1]);
        assert_eq!(d.rows().len(), 3);
        assert!(!d.rows()[2].expanded);
        d.toggle(&[1]);
        assert_eq!(d.rows().len(), 6);
    }

    #[test]
    fn edits_round_trip() {
        let mut d = Doc::parse(r#"{"name": "ana", "tags": ["a"], "n": 1}"#).unwrap();
        assert!(d.set_text(&[0], "Ana Lima"));
        assert!(d.set_text(&[2], "42"));
        assert!(d.set_text(&[1, 0], "\"7\""));
        assert_eq!(d.root.compact(), r#"{"name": "Ana Lima", "tags": ["7"], "n": 42}"#);
        d.rename(&[2], "count").unwrap();
        assert!(d.rename(&[2], "name").is_err(), "duplicate key");
        let p = d.add(&[1]).unwrap();
        assert_eq!(p, vec![1, 1]);
        d.set_text(&p, "true");
        let p = d.add(&[]).unwrap();
        assert_eq!(p, vec![3]);
        d.set_text(&p, "{}");
        let q = d.add(&[0]).unwrap();
        assert_eq!(q, vec![1], "on a leaf: a sibling after it");
        assert_eq!(d.root.compact(), r#"{"name": "Ana Lima", "key2": null, "tags": ["7", true], "count": 42, "key": {}}"#);
        d.remove(&[1]);
        d.remove(&[1, 0]);
        assert_eq!(d.root.compact(), r#"{"name": "Ana Lima", "tags": [true], "count": 42, "key": {}}"#);
        assert_eq!(parse(&d.root.compact()).unwrap(), d.root);
        assert_eq!(leaf_from_text("abc"), Node::String("abc".into()));
        assert_eq!(leaf_from_text(" null "), Node::Null);
        assert_eq!(Node::String("x".into()).edit_text(), "x");
        assert_eq!(Node::Number("3".into()).edit_text(), "3");
    }

    #[test]
    fn pg_arrays() {
        let n = from_pg_array(r#"{1,2.5,"a b",NULL,"NULL","q\"x\\y",plain}"#).unwrap();
        assert_eq!(
            n,
            Node::Array(vec![
                Node::Number("1".into()),
                Node::Number("2.5".into()),
                Node::String("a b".into()),
                Node::Null,
                Node::String("NULL".into()),
                Node::String(r#"q"x\y"#.into()),
                Node::String("plain".into()),
            ])
        );
        let nested = from_pg_array("{{1,2},{3,4}}").unwrap();
        assert_eq!(nested.len(), 2);
        assert_eq!(nested.get(&[1, 0]), Some(&Node::Number("3".into())));
        assert_eq!(from_pg_array("{}"), Some(Node::Array(vec![])));
        assert_eq!(from_pg_array("[0:1]={a,b}").map(|n| n.len()), Some(2));
        assert_eq!(from_pg_array(r#"{"{\"a\": 1}"}"#), Some(Node::Array(vec![Node::String(r#"{"a": 1}"#.into())])));
        for bad in ["", "1,2", "{1,2", "{1,2} x", r#"{"a}"#] {
            assert_eq!(from_pg_array(bad), None, "{bad}");
        }
    }

    #[test]
    fn expand_and_collapse_all() {
        let mut d = Doc::parse(r#"{"a": {"b": [1, {"c": 2}]}, "d": 3}"#).unwrap();
        let all = d.rows().len();
        d.collapse_all();
        assert_eq!(d.rows().len(), 3, "the root and its members");
        d.expand_all();
        assert_eq!(d.rows().len(), all);
    }

    #[test]
    fn levels_subtrees_and_previews() {
        let mut d = Doc::parse(r#"{"a": {"b": [1, {"c": 2}], "e": []}, "d": 3, "f": [{"g": 1}]}"#).unwrap();
        let keys = |d: &Doc| d.rows().iter().skip(1).map(|r| format!("{}{}", "-".repeat(r.depth - 1), r.path.iter().map(|i| i.to_string()).collect::<String>())).collect::<Vec<_>>();
        d.collapse_to_level(1);
        assert_eq!(keys(&d), ["0", "1", "2"]);
        d.collapse_to_level(2);
        assert_eq!(keys(&d), ["0", "-00", "-01", "1", "2", "-20"]);
        assert!(!d.collapsed.contains(&vec![0, 1]), "an empty array has nothing to fold");
        d.collapse_to_level(3);
        assert_eq!(keys(&d), ["0", "-00", "--000", "--001", "-01", "1", "2", "-20", "--200"]);
        let three = d.collapsed.clone();
        d.collapse_to_level(0);
        let zero = d.collapsed.clone();
        d.collapse_to_level(1);
        assert_eq!(zero, d.collapsed, "the root never folds");
        assert!(three.len() < zero.len());

        d.expand_all();
        d.set_subtree(&[0], false);
        assert_eq!(keys(&d), ["0", "1", "2", "-20", "--200"]);
        d.toggle(&[0]);
        assert_eq!(keys(&d), ["0", "-00", "-01", "1", "2", "-20", "--200"], "its members stay folded under it");
        d.set_subtree(&[0], true);
        assert_eq!(d.rows().len(), d.root.count() + 1);
        d.set_subtree(&[], false);
        assert_eq!(keys(&d), ["0", "1", "2"], "the whole document: down to its members");
        d.set_subtree(&[], true);
        assert!(d.collapsed.is_empty());

        let rows = d.rows();
        let by = |p: &[usize]| rows.iter().find(|r| r.path == p).unwrap().clone();
        assert_eq!(by(&[0]).preview(), "{…} 2 keys");
        assert_eq!(by(&[0, 0]).preview(), "[…] 2 items");
        assert_eq!(by(&[2]).preview(), "[…] 1 item");
        assert_eq!(by(&[0, 0, 1]).count_text(), "1 key");
        assert_eq!(by(&[0, 1]).preview(), "[]");
        assert_eq!(d.root.count(), 10);
    }

    #[test]
    fn large_documents_open_two_levels_deep() {
        let mut small = Doc::parse(r#"{"a": {"b": {"c": 1}}}"#).unwrap();
        small.open_default();
        assert!(small.collapsed.is_empty());
        let items: Vec<String> = (0..LARGE).map(|i| format!(r#"{{"n": {i}, "tags": ["x"]}}"#)).collect();
        let mut big = Doc::parse(&format!(r#"{{"items": [{}], "meta": {{"k": 1}}}}"#, items.join(","))).unwrap();
        big.open_default();
        let rows = big.rows();
        assert_eq!(rows.iter().map(|r| r.depth).max(), Some(2), "the items in sight, each folded");
        assert_eq!(rows.len(), 1 + 1 + LARGE + 1 + 1);
    }

    #[test]
    fn bson_and_json_trees() {
        use crate::nosql::bson::{self, Bson};
        let oid = bson::oid::ObjectId::parse_str("6ad671d80bb7c7ed855e5c57").unwrap();
        let b = Bson::Document(bson::doc! { "_id": oid, "n": 7, "big": Bson::Int64(3), "x": 1.5, "tags": ["a", Bson::Null], "ok": true });
        let n = Node::of_bson(&b);
        assert_eq!(n.compact(), r#"{"_id": ObjectId("6ad671d80bb7c7ed855e5c57"), "n": 7, "big": NumberLong(3), "x": 1.5, "tags": ["a", null], "ok": true}"#);
        assert_eq!(n.get(&[0]).map(Node::type_name), Some("objectId"));
        assert!(!n.get(&[2]).unwrap().is_container());
        let j = Node::of_json(&serde_json::json!({"a": [1, "b", null, {"c": false}]}));
        assert_eq!(j.compact(), r#"{"a": [1, "b", null, {"c": false}]}"#);
    }
}
