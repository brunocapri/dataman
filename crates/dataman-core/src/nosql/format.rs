//! Format Query for the document engines. Only whitespace moves (and a command's or method's
//! case): every token comes out as written, which `same_tokens` checks before anything is
//! handed back.

use crate::config::Engine;
use crate::format::{FormatOptions, KeywordCase};

use super::{console, js, mql, redis, shell};

const WIDTH: usize = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Formatted {
    pub text: String,
    /// What was left as written, when only part of the text could be formatted.
    pub left: Option<String>,
}

/// `Err` leaves the text as it is: it says why.
pub fn format(engine: Engine, text: &str, opts: &FormatOptions) -> Result<Formatted, String> {
    let body = text.trim();
    if body.is_empty() {
        return Ok(Formatted { text: text.to_string(), left: None });
    }
    let lead = &text[..text.len() - text.trim_start().len()];
    let trail = &text[lead.len() + body.len()..];
    let mut f = match engine {
        Engine::MongoDb => mongo(body, opts)?,
        Engine::Elasticsearch | Engine::OpenSearch => search(body, opts)?,
        Engine::Redis => redis_text(body, opts)?,
        e => return Err(format!("{} queries are SQL", e.label())),
    };
    f.text = format!("{lead}{}{trail}", f.text);
    Ok(f)
}

fn mongo(text: &str, opts: &FormatOptions) -> Result<Formatted, String> {
    if mql::parse(text).is_err()
        && let Err(e) = js::check(text)
        && js::check(&format!("({text})")).is_err()
    {
        return Err(e.message);
    }
    let text = code(text, Lang::Js, false, false, opts.indent)?;
    Ok(Formatted { text, left: None })
}

fn search(text: &str, opts: &FormatOptions) -> Result<Formatted, String> {
    let reqs = console::split(text);
    if reqs.is_empty() {
        shell::parse(text).map_err(|e| e.message)?;
        let text = code(text, Lang::Json, true, false, opts.indent)?;
        return Ok(Formatted { text, left: None });
    }
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    let (mut at, mut left, mut why) = (0, 0, None);
    for r in &reqs {
        let gap = &text[at..r.start];
        let prelude = gap.trim_end();
        out.push_str(prelude);
        if at > 0 || !prelude.is_empty() {
            out.push_str(if gap[prelude.len()..].matches('\n').count() >= 2 { "\n\n" } else { "\n" });
        }
        match request(&text[r.clone()], opts) {
            Ok(s) => out.push_str(&s),
            Err(e) => {
                out.push_str(&text[r.clone()]);
                left += 1;
                why.get_or_insert(e);
            }
        }
        at = r.end;
    }
    out.push_str(&text[at..]);
    match why {
        Some(e) if left == reqs.len() => Err(e),
        Some(e) => Ok(Formatted { text: out, left: Some(format!("{left} of {} requests left as written: {e}", reqs.len())) }),
        None => Ok(Formatted { text: out, left: None }),
    }
}

fn request(text: &str, opts: &FormatOptions) -> Result<String, String> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let mut lines: Vec<&str> = rest.lines().collect();
    let mut tail = Vec::new();
    while let Some(l) = lines.last().map(|l| l.trim()) {
        if !l.is_empty() && !l.starts_with('#') {
            break;
        }
        tail.push(l);
        lines.pop();
    }
    let body = lines.join("\n");
    let req = console::parse(&format!("{first}\n{body}")).map_err(|e| e.message)?;
    let word = first.split_whitespace().next().unwrap_or("");
    let mut out = format!("{} {}", req.method, first.trim_start()[word.len()..].trim());
    match req.body {
        console::Body::None => {}
        console::Body::Json(_) => {
            out.push('\n');
            out.push_str(&code(body.trim(), Lang::Json, true, false, opts.indent)?);
        }
        console::Body::Lines(_) => {
            for l in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
                out.push('\n');
                out.push_str(&code(l, Lang::Json, false, true, opts.indent).unwrap_or_else(|_| l.to_string()));
            }
        }
    }
    let mut blank = false;
    for l in tail.into_iter().rev() {
        if l.is_empty() {
            blank = true;
            continue;
        }
        out.push_str(if blank { "\n\n" } else { "\n" });
        out.push_str(l);
        blank = false;
    }
    Ok(out)
}

fn redis_text(text: &str, opts: &FormatOptions) -> Result<Formatted, String> {
    let mut out = String::with_capacity(text.len());
    let (mut commands, mut left, mut why, mut blank) = (0, 0, None, false);
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        if t.starts_with('#') || t.starts_with("//") {
            out.push_str(t);
            continue;
        }
        commands += 1;
        match redis_line(t, opts.keyword_case) {
            Ok(s) => out.push_str(&s),
            Err(e) => {
                out.push_str(t);
                left += 1;
                why.get_or_insert(e);
            }
        }
    }
    match why {
        Some(e) if left == commands => Err(e),
        Some(e) => Ok(Formatted { text: out, left: Some(format!("{left} of {commands} commands left as written: {e}")) }),
        None => Ok(Formatted { text: out, left: None }),
    }
}

/// The arguments as `redis::parse_bytes` reads them, each kept as written.
fn redis_line(line: &str, case: KeywordCase) -> Result<String, String> {
    redis::parse_bytes(line).map_err(|e| e.message)?;
    let mut args: Vec<(&str, bool)> = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        chars.next();
        let mut end = line.len();
        if c == '"' || c == '\'' {
            while let Some((j, x)) = chars.next() {
                if x == c {
                    end = j + 1;
                    break;
                }
                if x == '\\' && (c == '"' || chars.peek().is_some_and(|(_, n)| *n == '\'')) {
                    chars.next();
                }
            }
        } else {
            while let Some(&(j, x)) = chars.peek() {
                if x.is_whitespace() {
                    end = j;
                    break;
                }
                chars.next();
            }
        }
        args.push((&line[i..end], c == '"' || c == '\''));
    }
    let cased = |w: &str| match case {
        KeywordCase::Upper => w.to_ascii_uppercase(),
        KeywordCase::Lower => w.to_ascii_lowercase(),
        KeywordCase::Preserve => w.to_string(),
    };
    let mut out: Vec<String> = args.iter().map(|(a, _)| a.to_string()).collect();
    if let Some((cmd, false)) = args.first().copied()
        && redis::is_command(cmd)
    {
        out[0] = cased(cmd);
        if let Some((sub, false)) = args.get(1).copied()
            && sub.chars().all(|c| c.is_ascii_alphabetic())
            && redis::name(&[cmd.to_string(), sub.to_string()]).contains(' ')
        {
            out[1] = cased(sub);
        }
    }
    Ok(out.join(" "))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lang {
    Js,
    /// A console body: `#` comments and `"""` strings, no regexes.
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Word,
    Str,
    Regex,
    Line,
    Block,
    Open(u8),
    Close(u8),
    Comma,
    Semi,
    Colon,
    Op,
}

#[derive(Clone, Copy, Debug)]
struct Tok<'a> {
    k: K,
    s: &'a str,
    /// Newlines in the whitespace before it.
    nl: usize,
    /// Any whitespace before it.
    sp: bool,
}

fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn regex_after(t: Option<&Tok>) -> bool {
    t.is_none_or(|t| match t.k {
        K::Open(_) | K::Comma | K::Semi | K::Colon | K::Op => true,
        K::Word => matches!(t.s, "return" | "typeof" | "case" | "do" | "else" | "in" | "of" | "new" | "delete" | "void" | "throw" | "instanceof" | "yield" | "await"),
        _ => false,
    })
}

fn lex(text: &str, lang: Lang) -> Result<Vec<Tok<'_>>, String> {
    let b = text.as_bytes();
    let mut out: Vec<Tok> = Vec::new();
    let mut last: Option<usize> = None;
    let (mut i, mut nl, mut sp) = (0, 0, false);
    let line_end = |i: usize| text[i..].find('\n').map_or(b.len(), |n| i + n);
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            nl += usize::from(c == b'\n');
            sp = true;
            i += 1;
            continue;
        }
        let start = i;
        let k = match c {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = line_end(i);
                K::Line
            }
            b'#' if lang == Lang::Json => {
                i = line_end(i);
                K::Line
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = text[i + 2..].find("*/").map(|n| i + 2 + n + 2).ok_or("a /* comment is not closed")?;
                K::Block
            }
            b'"' if lang == Lang::Json && text[i..].starts_with("\"\"\"") => {
                i = text[i + 3..].find("\"\"\"").map(|n| i + 3 + n + 3).ok_or("a \"\"\" string is not closed")?;
                K::Str
            }
            b'"' | b'\'' | b'`' => {
                i += 1;
                loop {
                    match b.get(i) {
                        None => return Err("a string is not closed".into()),
                        Some(b'\\') => i += 2,
                        Some(&x) if x == c => break,
                        Some(b'\n') if c != b'`' => return Err("a string is not closed".into()),
                        _ => i += 1,
                    }
                }
                i += 1;
                K::Str
            }
            b'/' if lang == Lang::Js && regex_after(last.map(|l| &out[l])) => {
                i += 1;
                let mut class = false;
                loop {
                    match b.get(i) {
                        None | Some(b'\n') => return Err("a /regex/ is not closed".into()),
                        Some(b'\\') => {
                            i += 2;
                            continue;
                        }
                        Some(b'[') => class = true,
                        Some(b']') => class = false,
                        Some(b'/') if !class => break,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                K::Regex
            }
            b'{' | b'[' | b'(' => {
                i += 1;
                K::Open(c)
            }
            b'}' | b']' | b')' => {
                i += 1;
                K::Close(c)
            }
            b',' | b';' | b':' => {
                i += 1;
                match c {
                    b',' => K::Comma,
                    b';' => K::Semi,
                    _ => K::Colon,
                }
            }
            c if word_byte(c) => {
                let number = c.is_ascii_digit();
                while i < b.len() {
                    let x = b[i];
                    if word_byte(x)
                        || (number && x == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
                        || (number && matches!(x, b'+' | b'-') && matches!(b[i - 1], b'e' | b'E'))
                    {
                        i += 1;
                    } else {
                        break;
                    }
                }
                K::Word
            }
            _ => {
                while i < b.len() {
                    let x = b[i];
                    if x.is_ascii_whitespace() || word_byte(x) || b"{}[](),;:\"'`".contains(&x) || (x == b'#' && lang == Lang::Json) {
                        break;
                    }
                    if x == b'/' && matches!(b.get(i + 1), Some(b'/' | b'*')) && i > start {
                        break;
                    }
                    i += 1;
                }
                K::Op
            }
        };
        if !matches!(k, K::Line | K::Block) {
            last = Some(out.len());
        }
        out.push(Tok { k, s: &text[start..i], nl, sp });
        (nl, sp) = (0, false);
    }
    Ok(out)
}

fn mates(toks: &[Tok]) -> Result<Vec<usize>, String> {
    let mut mate = vec![usize::MAX; toks.len()];
    let mut open: Vec<usize> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match t.k {
            K::Open(_) => open.push(i),
            K::Close(c) => {
                let Some(o) = open.pop() else { return Err(format!("a `{}` closes nothing", c as char)) };
                let want = match toks[o].s.as_bytes()[0] {
                    b'{' => b'}',
                    b'[' => b']',
                    _ => b')',
                };
                if c != want {
                    return Err(format!("a `{}` is closed by `{}`", toks[o].s, c as char));
                }
                mate[o] = i;
                mate[i] = o;
            }
            _ => {}
        }
    }
    match open.last() {
        Some(&o) => Err(format!("a `{}` is not closed", toks[o].s)),
        None => Ok(mate),
    }
}

fn ternaries(toks: &[Tok]) -> Vec<bool> {
    let mut open = vec![0usize];
    toks.iter()
        .map(|t| {
            match t.k {
                K::Open(_) => open.push(0),
                K::Close(_) => {
                    open.pop();
                }
                K::Op if t.s.starts_with('?') && !t.s.starts_with("?.") && !t.s.starts_with("??") => *open.last_mut().unwrap() += 1,
                K::Colon if open.last().is_some_and(|n| *n > 0) => {
                    *open.last_mut().unwrap() -= 1;
                    return true;
                }
                _ => {}
            }
            false
        })
        .collect()
}

fn text_of<'a>(t: &Tok<'a>) -> &'a str {
    if t.k == K::Line { t.s.trim_end() } else { t.s }
}

fn same_tokens(a: &[Tok], b: &[Tok]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.k == y.k && text_of(x) == text_of(y))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Frame {
    /// One entry per line.
    Lines,
    Flat,
    /// A call's parentheses; `true` when written over several lines (those are kept, indented).
    Paren(bool),
}

struct Open {
    at: usize,
    frame: Frame,
    /// The indent of the line it opens on.
    base: usize,
}

struct Layout<'a> {
    toks: Vec<Tok<'a>>,
    mate: Vec<usize>,
    /// A colon that ends `a ? b :`, spaced like the `?`.
    ternary: Vec<bool>,
    /// Every non-empty object on lines of its own (a console body, as Kibana writes them).
    objects: bool,
    /// All on one line (a bulk body's line).
    flat: bool,
}

fn code(text: &str, lang: Lang, objects: bool, flat: bool, indent: usize) -> Result<String, String> {
    let toks = lex(text, lang)?;
    let mate = mates(&toks)?;
    let ternary = ternaries(&toks);
    let l = Layout { toks, mate, ternary, objects, flat };
    let out = l.print(indent.max(1));
    match lex(&out, lang) {
        Ok(again) if same_tokens(&l.toks, &again) => Ok(out),
        _ => Err("the text could not be formatted safely".into()),
    }
}

fn cols(s: &str) -> usize {
    s.rsplit('\n').next().unwrap_or(s).chars().count()
}

impl Layout<'_> {
    /// The space between `i - 1` and `i` on one line.
    fn gap(&self, i: usize) -> &'static str {
        let (p, t) = (&self.toks[i - 1], &self.toks[i]);
        let keep = if t.sp { " " } else { "" };
        match (p.k, t.k) {
            (K::Open(b'{'), K::Close(b'}')) => "",
            (K::Open(b'{'), _) | (_, K::Close(b'}')) => " ",
            (K::Open(_), _) | (_, K::Close(_)) => "",
            (_, K::Comma | K::Semi) => "",
            (K::Comma | K::Semi, _) => " ",
            (_, K::Colon) if self.ternary[i] => " ",
            (_, K::Colon) => "",
            (K::Colon, _) => " ",
            (_, K::Line) => " ",
            (K::Block, _) | (_, K::Block) => keep,
            (_, K::Open(b'{')) => " ",
            _ => keep,
        }
    }

    /// An empty pair's closer is drawn next to its opener whatever was between them.
    fn newline_before(&self, j: usize) -> bool {
        self.toks[j].nl > 0 && !self.flat && !(matches!(self.toks[j].k, K::Close(_)) && self.mate[j] + 1 == j)
    }

    /// Original newlines inside, an object the console spreads, or anything on more than one line.
    fn breaks(&self, i: usize) -> bool {
        let m = self.mate[i];
        (i + 1..=m).any(|j| self.newline_before(j))
            || self.toks[i + 1..m].iter().any(|t| t.s.contains('\n'))
            || (self.objects && (i..m).any(|j| self.toks[j].k == K::Open(b'{') && self.mate[j] != j + 1))
    }

    fn flat_width(&self, i: usize, room: usize) -> usize {
        let mut w = 0;
        for j in i..=self.mate[i] {
            if j > i {
                w += self.gap(j).len();
            }
            w += self.toks[j].s.chars().count();
            if w > room {
                break;
            }
        }
        w
    }

    fn direct_newline(&self, i: usize) -> bool {
        let m = self.mate[i];
        let mut j = i + 1;
        while j <= m {
            if self.newline_before(j) {
                return true;
            }
            j = if matches!(self.toks[j].k, K::Open(_)) { self.mate[j] + 1 } else { j + 1 };
        }
        false
    }

    fn indent_for(&self, stack: &[Open], i: usize) -> usize {
        let mut level = stack
            .iter()
            .rev()
            .find(|o| matches!(o.frame, Frame::Lines | Frame::Paren(true)))
            .map_or(0, |o| if self.mate[o.at] == i { o.base } else { o.base + 1 });
        let t = &self.toks[i];
        if t.k == K::Op && matches!(t.s, "." | "?.") {
            level += 1;
        }
        level
    }

    fn print(&self, unit: usize) -> String {
        let toks = &self.toks;
        let mut out = String::with_capacity(toks.iter().map(|t| t.s.len() + 2).sum());
        let mut stack: Vec<Open> = Vec::new();
        let (mut indent, mut col, mut pending) = (0usize, 0usize, false);
        for (i, t) in toks.iter().enumerate() {
            if i > 0 {
                let p = &toks[i - 1];
                let lines = stack.last().filter(|o| o.frame == Frame::Lines);
                let forced = pending
                    || p.k == K::Line
                    || (stack.is_empty() && p.k == K::Semi)
                    || lines.is_some_and(|o| matches!(p.k, K::Comma | K::Semi) || o.at == i - 1 || self.mate[o.at] == i);
                let orig = if self.newline_before(i) { t.nl } else { 0 };
                if forced && orig == 0 && matches!(t.k, K::Line | K::Block) && p.k != K::Line {
                    pending = true;
                    out.push(' ');
                    col += 1;
                } else if forced || orig > 0 {
                    pending = false;
                    out.push('\n');
                    if orig >= 2 && !matches!(p.k, K::Open(_)) && !matches!(t.k, K::Close(_)) {
                        out.push('\n');
                    }
                    indent = self.indent_for(&stack, i);
                    col = indent * unit;
                    out.extend(std::iter::repeat_n(' ', col));
                } else {
                    let g = self.gap(i);
                    out.push_str(g);
                    col += g.len();
                }
            }
            let start = col;
            let s = text_of(t);
            out.push_str(s);
            col = if s.contains('\n') { cols(s) } else { col + s.chars().count() };
            match t.k {
                K::Open(c) => {
                    let frame = if c == b'(' {
                        Frame::Paren(!self.flat && self.direct_newline(i))
                    } else if self.flat || self.mate[i] == i + 1 {
                        Frame::Flat
                    } else if self.breaks(i) || start + self.flat_width(i, WIDTH) > WIDTH {
                        Frame::Lines
                    } else {
                        Frame::Flat
                    };
                    stack.push(Open { at: i, frame, base: indent });
                }
                K::Close(_) => {
                    stack.pop();
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(engine: Engine, text: &str) -> String {
        let out = format(engine, text, &FormatOptions::default()).unwrap_or_else(|e| panic!("{e}: {text}"));
        assert_eq!(out.left, None, "{text}");
        let again = format(engine, &out.text, &FormatOptions::default()).unwrap();
        assert_eq!(again.text, out.text, "formatting twice changes it:\n{}", out.text);
        out.text
    }

    #[test]
    fn mongo_short_queries_stay_on_a_line() {
        assert_eq!(
            f(Engine::MongoDb, "db.users.find({age:{$gt:30},name:\"x\"}).sort({age:-1})"),
            "db.users.find({ age: { $gt: 30 }, name: \"x\" }).sort({ age: -1 })"
        );
        assert_eq!(f(Engine::MongoDb, "db.a.find( {  } ,{x : 1})"), "db.a.find({}, { x: 1 })");
        assert_eq!(f(Engine::MongoDb, "db.a.find(); db.b.find()"), "db.a.find();\ndb.b.find()");
        assert_eq!(f(Engine::MongoDb, "show dbs"), "show dbs");
    }

    #[test]
    fn mongo_objects_indent_one_entry_per_line() {
        let messy = "db.c.insertOne({\n      name: \"a\",\n  tags: [\"x\",   \"y\"],   // the tags\n     nested: {a: 1}\n})";
        assert_eq!(f(Engine::MongoDb, messy), "db.c.insertOne({\n  name: \"a\",\n  tags: [\"x\", \"y\"], // the tags\n  nested: { a: 1 }\n})");
        let long = "db.orders.aggregate([{$match: {status: \"shipped\", total: {$gte: 100}}}, {$group: {_id: \"$customer\", n: {$sum: 1}}}])";
        assert_eq!(
            f(Engine::MongoDb, long),
            "db.orders.aggregate([\n  { $match: { status: \"shipped\", total: { $gte: 100 } } },\n  { $group: { _id: \"$customer\", n: { $sum: 1 } } }\n])"
        );
        let chained = "db.a.find({\nx: 1})\n.sort({x: 1})\n   .limit(5)";
        assert_eq!(f(Engine::MongoDb, chained), "db.a.find({\n  x: 1\n})\n  .sort({ x: 1 })\n  .limit(5)");
        let args = "db.a.updateOne(\n{_id: 1},\n      {$set: {x: 2}}\n)";
        assert_eq!(f(Engine::MongoDb, args), "db.a.updateOne(\n  { _id: 1 },\n  { $set: { x: 2 } }\n)");
        let script = "for (const d of db.a.find()) {\nprint(d)\n      n++\n}";
        assert_eq!(f(Engine::MongoDb, script), "for (const d of db.a.find()) {\n  print(d)\n  n++\n}");
        let four = format(Engine::MongoDb, "db.a.insertOne({\na: 1})", &FormatOptions { indent: 4, ..Default::default() }).unwrap();
        assert_eq!(four.text, "db.a.insertOne({\n    a: 1\n})");
    }

    #[test]
    fn mongo_keeps_strings_and_comments() {
        let src = "// lead\ndb.a.find({a: \"  spaced   {  }  \", b: 'it\\'s',   c: /a  b[/]/i, /* why */ d: `x  ${1}  y`}) // tail";
        let out = f(Engine::MongoDb, src);
        for piece in ["// lead\n", "\"  spaced   {  }  \"", "'it\\'s'", "/a  b[/]/i", "/* why */", "`x  ${1}  y`", "// tail"] {
            assert!(out.contains(piece), "{piece} in {out}");
        }
        assert_eq!(out, "// lead\ndb.a.find({\n  a: \"  spaced   {  }  \",\n  b: 'it\\'s',\n  c: /a  b[/]/i, /* why */\n  d: `x  ${1}  y`\n}) // tail");
        let blank = "db.a.find()\n\n\n\n// next\ndb.b.find({x: 1, // one\ny: 2})";
        assert_eq!(f(Engine::MongoDb, blank), "db.a.find()\n\n// next\ndb.b.find({\n  x: 1, // one\n  y: 2\n})");
        assert_eq!(f(Engine::MongoDb, "  db.a.find({x:1})\n"), "  db.a.find({ x: 1 })\n");
        assert_eq!(f(Engine::MongoDb, "{a:1,b:[1,2]}"), "{ a: 1, b: [1, 2] }");
    }

    #[test]
    fn invalid_text_is_left_alone() {
        for bad in ["db.a.find({x: \"open})", "db.a.find({x: 1)", "db.a.find({x: 1}", "db.a.find({x: })", "db.a.find(/* open"] {
            assert!(format(Engine::MongoDb, bad, &FormatOptions::default()).is_err(), "{bad}");
        }
        assert!(format(Engine::Redis, "SET a \"open", &FormatOptions::default()).is_err());
        assert!(format(Engine::Elasticsearch, "GET /a/_search\n{\"query\": }", &FormatOptions::default()).is_err());
        assert!(format(Engine::Postgres, "select 1", &FormatOptions::default()).is_err());
        assert_eq!(format(Engine::Redis, " \n", &FormatOptions::default()).unwrap().text, " \n");
    }

    #[test]
    fn console_bodies_spread_and_methods_stay() {
        let src = "get /x/_search\n{\"query\":{\"bool\":{\"must\":[{\"match\":{\"t\":\"a  b\"}}],\"filter\":[]}},\"size\":10,\"_source\":[\"a\",\"b\"]}";
        assert_eq!(
            f(Engine::Elasticsearch, src),
            "GET /x/_search\n{\n  \"query\": {\n    \"bool\": {\n      \"must\": [\n        {\n          \"match\": {\n            \"t\": \"a  b\"\n          }\n        }\n      ],\n      \"filter\": []\n    }\n  },\n  \"size\": 10,\n  \"_source\": [\"a\", \"b\"]\n}"
        );
        let many = "# notes\nGET   _cat/indices\n\n\n\nPOST _bulk\n{ \"index\" : { \"_index\" : \"a\" } }\n{\"x\":1}\n\nGET /a/_count\n{query: {match_all: {}}}\n# after";
        assert_eq!(
            f(Engine::OpenSearch, many),
            "# notes\nGET _cat/indices\n\nPOST _bulk\n{ \"index\": { \"_index\": \"a\" } }\n{ \"x\": 1 }\n\nGET /a/_count\n{\n  query: {\n    match_all: {}\n  }\n}\n# after"
        );
        assert_eq!(f(Engine::Elasticsearch, "{\"a\":[1,2]}"), "{\n  \"a\": [1, 2]\n}");
        let part = format(Engine::Elasticsearch, "GET /a/_search\n{\"size\": }\n\nGET /b/_search\n{\"size\":1}", &FormatOptions::default()).unwrap();
        assert_eq!(part.text, "GET /a/_search\n{\"size\": }\n\nGET /b/_search\n{\n  \"size\": 1\n}");
        assert!(part.left.is_some_and(|l| l.starts_with("1 of 2 requests")));
    }

    #[test]
    fn redis_one_command_a_line() {
        assert_eq!(f(Engine::Redis, "set   foo   \"a   b\"  \n\n\n  get foo\n# c\nconfig get maxmemory"), "SET foo \"a   b\"\n\nGET foo\n# c\nCONFIG GET maxmemory");
        assert_eq!(f(Engine::Redis, "hset h 'it\\'s  x' \"say \\\"hi\\\"\""), "HSET h 'it\\'s  x' \"say \\\"hi\\\"\"");
        assert_eq!(f(Engine::Redis, "SET get  set"), "SET get set");
        let lower = format(Engine::Redis, "GET a", &FormatOptions { keyword_case: KeywordCase::Lower, ..Default::default() }).unwrap();
        assert_eq!(lower.text, "get a");
        let part = format(Engine::Redis, "get a\nset b \"open", &FormatOptions::default()).unwrap();
        assert_eq!(part.text, "GET a\nset b \"open");
        assert!(part.left.is_some());
    }
}
