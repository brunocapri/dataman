use std::fmt::Write as _;

use mongodb::bson::{self, Bson, Document};

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Null,
    Undefined,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Val>),
    // In the order written: a sort's keys depend on it.
    Obj(Vec<(String, Val)>),
    Call(String, Vec<Val>),
    Regex(String, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub at: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(text: &str) -> Result<Val, ParseError> {
    let mut p = Parser::new(text);
    let v = p.value()?;
    p.skip_ws();
    if !p.at_end() {
        return Err(p.error("unexpected text after the value"));
    }
    Ok(v)
}

pub(crate) struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

pub(crate) fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

pub(crate) fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

impl<'a> Parser<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Parser { src, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    pub(crate) fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError { message: message.into(), at: self.pos.min(self.src.len()) }
    }

    pub(crate) fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek2(&self) -> Option<char> {
        let mut c = self.rest().chars();
        c.next();
        c.next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    pub(crate) fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.peek2() == Some('/') => {
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                Some('/') if self.peek2() == Some('*') => {
                    self.pos += 2;
                    match self.rest().find("*/") {
                        Some(i) => self.pos += i + 2,
                        None => self.pos = self.src.len(),
                    }
                }
                _ => return,
            }
        }
    }

    pub(crate) fn eat(&mut self, c: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(c) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect(&mut self, c: char) -> Result<(), ParseError> {
        if self.eat(c) { Ok(()) } else { Err(self.error(format!("expected `{c}`"))) }
    }

    pub(crate) fn ident(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        if !self.peek().is_some_and(is_ident_start) {
            return None;
        }
        while self.peek().is_some_and(is_ident_char) {
            self.bump();
        }
        Some(&self.src[start..self.pos])
    }

    pub(crate) fn list(&mut self, close: char) -> Result<Vec<Val>, ParseError> {
        let mut out = Vec::new();
        loop {
            if self.eat(close) {
                return Ok(out);
            }
            out.push(self.value()?);
            if !self.eat(',') {
                self.skip_ws();
                if self.eat(close) {
                    return Ok(out);
                }
                return Err(self.error(format!("expected `,` or `{close}`")));
            }
        }
    }

    pub(crate) fn value(&mut self) -> Result<Val, ParseError> {
        let v = self.primary()?;
        if !matches!(v, Val::Int(_) | Val::Float(_)) {
            return Ok(v);
        }
        let mut acc = self.product(v)?;
        loop {
            let save = self.pos;
            self.skip_ws();
            let op = match self.peek() {
                Some(c @ ('+' | '-')) => c,
                _ => {
                    self.pos = save;
                    return Ok(acc);
                }
            };
            self.bump();
            let rhs = self.numeric()?;
            let rhs = self.product(rhs)?;
            acc = arith(op, acc, rhs);
        }
    }

    fn numeric(&mut self) -> Result<Val, ParseError> {
        let at = self.pos;
        match self.primary()? {
            v @ (Val::Int(_) | Val::Float(_)) => Ok(v),
            _ => Err(ParseError { message: "arithmetic takes numbers".into(), at }),
        }
    }

    fn product(&mut self, mut acc: Val) -> Result<Val, ParseError> {
        loop {
            let save = self.pos;
            self.skip_ws();
            let op = match self.peek() {
                Some(c @ ('*' | '/' | '%')) => c,
                _ => {
                    self.pos = save;
                    return Ok(acc);
                }
            };
            self.bump();
            let rhs = self.numeric()?;
            acc = arith(op, acc, rhs);
        }
    }

    fn primary(&mut self) -> Result<Val, ParseError> {
        self.skip_ws();
        let Some(c) = self.peek() else { return Err(self.error("expected a value")) };
        match c {
            '{' => {
                self.bump();
                self.object()
            }
            '[' => {
                self.bump();
                Ok(Val::Arr(self.list(']')?))
            }
            '"' | '\'' => Ok(Val::Str(self.string()?)),
            '/' => self.regex(),
            '-' | '+' | '.' | '0'..='9' => self.number(),
            c if is_ident_start(c) => self.word(),
            _ => Err(self.error(format!("unexpected `{c}`"))),
        }
    }

    fn object(&mut self) -> Result<Val, ParseError> {
        let mut fields = Vec::new();
        loop {
            if self.eat('}') {
                return Ok(Val::Obj(fields));
            }
            self.skip_ws();
            let key = match self.peek() {
                Some('"' | '\'') => self.string()?,
                Some(c) if is_ident_start(c) => {
                    let start = self.pos;
                    while self.peek().is_some_and(|c| is_ident_char(c) || c == '.') {
                        self.bump();
                    }
                    self.src[start..self.pos].to_string()
                }
                Some(c) if c.is_ascii_digit() => {
                    let start = self.pos;
                    while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == '.') {
                        self.bump();
                    }
                    self.src[start..self.pos].to_string()
                }
                _ => return Err(self.error("expected a field name")),
            };
            self.expect(':')?;
            let v = self.value()?;
            fields.push((key, v));
            if !self.eat(',') {
                if self.eat('}') {
                    return Ok(Val::Obj(fields));
                }
                return Err(self.error("expected `,` or `}`"));
            }
        }
    }

    pub(crate) fn string(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        let q = self.bump().unwrap_or('"');
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(ParseError { message: "a string is not closed".into(), at: start });
            };
            match c {
                c if c == q => return Ok(out),
                '\\' => {
                    let Some(e) = self.bump() else { continue };
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let hex: String = self.rest().chars().take(4).collect();
                            let Ok(mut code) = u32::from_str_radix(&hex, 16) else {
                                return Err(self.error("a \\u escape needs four hex digits"));
                            };
                            self.pos += 4;
                            if (0xD800..0xDC00).contains(&code)
                                && self.rest().starts_with("\\u")
                                && let Ok(lo) = u32::from_str_radix(&self.rest()[2..].chars().take(4).collect::<String>(), 16)
                                && (0xDC00..0xE000).contains(&lo)
                            {
                                self.pos += 6;
                                code = 0x10000 + ((code - 0xD800) << 10) + (lo - 0xDC00);
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        '\n' => {}
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn regex(&mut self) -> Result<Val, ParseError> {
        let start = self.pos;
        self.bump();
        let mut pattern = String::new();
        let mut class = false;
        loop {
            let Some(c) = self.bump() else {
                return Err(ParseError { message: "a regular expression is not closed".into(), at: start });
            };
            match c {
                '\n' => return Err(ParseError { message: "a regular expression is not closed".into(), at: start }),
                '\\' => {
                    pattern.push('\\');
                    if let Some(n) = self.bump() {
                        pattern.push(n);
                    }
                }
                '[' => {
                    class = true;
                    pattern.push(c);
                }
                ']' => {
                    class = false;
                    pattern.push(c);
                }
                '/' if !class => break,
                c => pattern.push(c),
            }
        }
        let fstart = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.bump();
        }
        let mut flags: Vec<char> = self.src[fstart..self.pos].chars().collect();
        flags.sort_unstable();
        Ok(Val::Regex(pattern, flags.into_iter().collect()))
    }

    fn number(&mut self) -> Result<Val, ParseError> {
        let start = self.pos;
        if matches!(self.peek(), Some('-' | '+')) {
            self.bump();
        }
        let neg = self.src[start..self.pos].starts_with('-');
        if self.rest().starts_with("Infinity") {
            self.pos += "Infinity".len();
            return Ok(Val::Float(if neg { f64::NEG_INFINITY } else { f64::INFINITY }));
        }
        if self.rest().starts_with("0x") || self.rest().starts_with("0X") {
            self.pos += 2;
            let hs = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.bump();
            }
            let n = i64::from_str_radix(&self.src[hs..self.pos], 16).map_err(|_| self.error("not a number"))?;
            return Ok(Val::Int(if neg { -n } else { n }));
        }
        let mut float = false;
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' | '_' => {}
                '.' => float = true,
                'e' | 'E' => {
                    float = true;
                    self.bump();
                    if matches!(self.peek(), Some('-' | '+')) {
                        self.bump();
                    }
                    continue;
                }
                _ => break,
            }
            self.bump();
        }
        let raw: String = self.src[start..self.pos].chars().filter(|c| *c != '_' && *c != '+').collect();
        if raw.is_empty() || raw == "-" || raw == "." {
            return Err(ParseError { message: "not a number".into(), at: start });
        }
        if !float && let Ok(n) = raw.parse::<i64>() {
            return Ok(Val::Int(n));
        }
        raw.parse::<f64>().map(Val::Float).map_err(|_| ParseError { message: format!("`{raw}` is not a number"), at: start })
    }

    fn word(&mut self) -> Result<Val, ParseError> {
        let start = self.pos;
        let mut name = self.ident().unwrap_or_default();
        if name == "new" {
            name = self.ident().ok_or_else(|| self.error("expected a name after `new`"))?;
        }
        match name {
            "true" => return Ok(Val::Bool(true)),
            "false" => return Ok(Val::Bool(false)),
            "null" => return Ok(Val::Null),
            "undefined" => return Ok(Val::Undefined),
            "NaN" => return Ok(Val::Float(f64::NAN)),
            "Infinity" => return Ok(Val::Float(f64::INFINITY)),
            _ => {}
        }
        if self.eat('(') {
            let args = self.list(')')?;
            return Ok(Val::Call(name.to_string(), args));
        }
        Err(ParseError { message: format!("`{name}` is not a value (strings need quotes)"), at: start })
    }
}

fn arith(op: char, a: Val, b: Val) -> Val {
    let f = |v: &Val| match v {
        Val::Int(n) => *n as f64,
        Val::Float(x) => *x,
        _ => 0.,
    };
    if let (Val::Int(x), Val::Int(y)) = (&a, &b) {
        let r = match op {
            '+' => x.checked_add(*y),
            '-' => x.checked_sub(*y),
            '*' => x.checked_mul(*y),
            '/' if *y != 0 && x % y == 0 => Some(x / y),
            '%' if *y != 0 => Some(x % y),
            _ => None,
        };
        if let Some(r) = r {
            return Val::Int(r);
        }
    }
    let (x, y) = (f(&a), f(&b));
    Val::Float(match op {
        '+' => x + y,
        '-' => x - y,
        '*' => x * y,
        '/' => x / y,
        _ => x % y,
    })
}

const EXT_KEYS: &[&str] = &[
    "$oid", "$date", "$numberLong", "$numberInt", "$numberDouble", "$numberDecimal", "$binary", "$uuid", "$timestamp",
    "$minKey", "$maxKey", "$regularExpression", "$symbol", "$code",
];

pub fn to_bson(v: &Val) -> Result<Bson, String> {
    Ok(match v {
        Val::Null => Bson::Null,
        Val::Undefined => Bson::Undefined,
        Val::Bool(b) => Bson::Boolean(*b),
        Val::Int(n) => match i32::try_from(*n) {
            Ok(n) => Bson::Int32(n),
            Err(_) => Bson::Int64(*n),
        },
        Val::Float(f) => Bson::Double(*f),
        Val::Str(s) => Bson::String(s.clone()),
        Val::Arr(a) => Bson::Array(a.iter().map(to_bson).collect::<Result<_, _>>()?),
        Val::Obj(fields) => {
            if fields.len() <= 2
                && fields.first().is_some_and(|(k, _)| EXT_KEYS.contains(&k.as_str()))
                && let Ok(b) = Bson::try_from(to_json_lenient(v))
                && !matches!(b, Bson::Document(_))
            {
                return Ok(b);
            }
            let mut d = Document::new();
            for (k, v) in fields {
                d.insert(k.clone(), to_bson(v)?);
            }
            Bson::Document(d)
        }
        Val::Regex(p, f) => Bson::RegularExpression(bson::Regex { pattern: p.clone(), options: f.clone() }),
        Val::Call(name, args) => call_to_bson(name, args)?,
    })
}

pub fn to_document(v: &Val) -> Result<Document, String> {
    match to_bson(v)? {
        Bson::Document(d) => Ok(d),
        other => Err(format!("expected a document {{…}}, not {}", type_name(&other))),
    }
}

fn str_arg(name: &str, args: &[Val]) -> Result<Option<String>, String> {
    match args.first() {
        None => Ok(None),
        Some(Val::Str(s)) => Ok(Some(s.clone())),
        Some(Val::Int(n)) => Ok(Some(n.to_string())),
        Some(Val::Float(f)) => Ok(Some(f.to_string())),
        Some(_) => Err(format!("{name}() takes a string")),
    }
}

fn call_to_bson(name: &str, args: &[Val]) -> Result<Bson, String> {
    Ok(match name {
        "ObjectId" | "ObjectID" => match str_arg(name, args)? {
            None => Bson::ObjectId(bson::oid::ObjectId::new()),
            Some(hex) => Bson::ObjectId(bson::oid::ObjectId::parse_str(&hex).map_err(|_| format!("ObjectId(\"{hex}\"): not 24 hex digits"))?),
        },
        "ISODate" | "Date" => match args.first() {
            None => Bson::DateTime(bson::DateTime::now()),
            Some(Val::Int(ms)) => Bson::DateTime(bson::DateTime::from_millis(*ms)),
            Some(Val::Float(ms)) => Bson::DateTime(bson::DateTime::from_millis(*ms as i64)),
            Some(Val::Str(s)) => Bson::DateTime(parse_date(s).ok_or_else(|| format!("{name}(\"{s}\"): not a date"))?),
            Some(_) => return Err(format!("{name}() takes a date string or milliseconds")),
        },
        "NumberLong" | "Long" => {
            let s = str_arg(name, args)?.unwrap_or_else(|| "0".into());
            Bson::Int64(s.trim().parse().map_err(|_| format!("{name}({s}): not an integer"))?)
        }
        "NumberInt" | "Int32" => {
            let s = str_arg(name, args)?.unwrap_or_else(|| "0".into());
            Bson::Int32(s.trim().parse().map_err(|_| format!("{name}({s}): not a 32-bit integer"))?)
        }
        "Double" => match args.first() {
            Some(Val::Int(n)) => Bson::Double(*n as f64),
            Some(Val::Float(f)) => Bson::Double(*f),
            Some(Val::Str(s)) => Bson::Double(s.trim().parse().map_err(|_| format!("Double(\"{s}\"): not a number"))?),
            _ => return Err("Double() takes a number".into()),
        },
        "NumberDecimal" | "Decimal128" => {
            let s = str_arg(name, args)?.unwrap_or_else(|| "0".into());
            Bson::Decimal128(s.trim().parse().map_err(|_| format!("{name}(\"{s}\"): not a decimal"))?)
        }
        "UUID" => {
            let s = str_arg(name, args)?.ok_or("UUID() takes its hex string")?;
            let hex: String = s.chars().filter(|c| *c != '-').collect();
            let bytes = data_encoding::HEXLOWER_PERMISSIVE.decode(hex.as_bytes()).map_err(|_| format!("UUID(\"{s}\"): not a UUID"))?;
            if bytes.len() != 16 {
                return Err(format!("UUID(\"{s}\"): not a UUID"));
            }
            Bson::Binary(bson::Binary { subtype: bson::spec::BinarySubtype::Uuid, bytes })
        }
        "BinData" => {
            let (Some(Val::Int(sub)), Some(Val::Str(b64))) = (args.first(), args.get(1)) else {
                return Err("BinData() takes a subtype and a base64 string".into());
            };
            let bytes = data_encoding::BASE64.decode(b64.as_bytes()).map_err(|_| "BinData(): not base64".to_string())?;
            Bson::Binary(bson::Binary { subtype: bson::spec::BinarySubtype::from(*sub as u8), bytes })
        }
        "Timestamp" => {
            let (t, i) = match args {
                [Val::Int(t), Val::Int(i)] => (*t, *i),
                [Val::Obj(f)] => {
                    let get = |k: &str| f.iter().find(|(n, _)| n == k).and_then(|(_, v)| if let Val::Int(n) = v { Some(*n) } else { None });
                    (get("t").unwrap_or(0), get("i").unwrap_or(0))
                }
                [] => (0, 0),
                _ => return Err("Timestamp() takes (seconds, increment)".into()),
            };
            Bson::Timestamp(bson::Timestamp { time: t as u32, increment: i as u32 })
        }
        "MinKey" => Bson::MinKey,
        "MaxKey" => Bson::MaxKey,
        "RegExp" => {
            let p = str_arg(name, args)?.unwrap_or_default();
            let f = match args.get(1) {
                Some(Val::Str(f)) => f.clone(),
                _ => String::new(),
            };
            Bson::RegularExpression(bson::Regex { pattern: p, options: f })
        }
        "Code" => Bson::JavaScriptCode(str_arg(name, args)?.unwrap_or_default()),
        other => return Err(format!("{other}() is not a value the shell knows")),
    })
}

pub(crate) fn parse_date(s: &str) -> Option<bson::DateTime> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(bson::DateTime::from_millis(d.timestamp_millis()));
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), f) {
            return Some(bson::DateTime::from_millis(d.and_utc().timestamp_millis()));
        }
    }
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(bson::DateTime::from_millis(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis()))
}

pub fn to_json(v: &Val) -> Result<serde_json::Value, String> {
    use serde_json::Value as J;
    Ok(match v {
        Val::Null | Val::Undefined => J::Null,
        Val::Bool(b) => J::Bool(*b),
        Val::Int(n) => J::from(*n),
        Val::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).ok_or("NaN and Infinity are not JSON")?,
        Val::Str(s) => J::String(s.clone()),
        Val::Arr(a) => J::Array(a.iter().map(to_json).collect::<Result<_, _>>()?),
        Val::Obj(fields) => {
            let mut m = serde_json::Map::new();
            for (k, v) in fields {
                m.insert(k.clone(), to_json(v)?);
            }
            J::Object(m)
        }
        Val::Regex(..) => return Err("a /regex/ is not JSON: write it as a string".into()),
        Val::Call(name, _) => return Err(format!("{name}() is a MongoDB value, not JSON")),
    })
}

fn to_json_lenient(v: &Val) -> serde_json::Value {
    match v {
        Val::Obj(fields) => serde_json::Value::Object(fields.iter().map(|(k, v)| (k.clone(), to_json_lenient(v))).collect()),
        Val::Arr(a) => serde_json::Value::Array(a.iter().map(to_json_lenient).collect()),
        other => to_json(other).unwrap_or(serde_json::Value::Null),
    }
}

pub fn type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

pub fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn float_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0. { "Infinity".into() } else { "-Infinity".into() }
    } else if f.fract() == 0. && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

pub fn date_text(d: &bson::DateTime) -> String {
    match chrono::DateTime::from_timestamp_millis(d.timestamp_millis()) {
        Some(t) => t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        None => d.timestamp_millis().to_string(),
    }
}

pub fn scalar_text(b: &Bson) -> String {
    match b {
        Bson::Double(f) => float_text(*f),
        Bson::String(s) => quote(s),
        Bson::Array(a) => format!("[{}]", a.len()),
        Bson::Document(d) => format!("{{{}}}", d.len()),
        Bson::Boolean(v) => v.to_string(),
        Bson::Null => "null".into(),
        Bson::Undefined => "undefined".into(),
        Bson::RegularExpression(r) => format!("/{}/{}", r.pattern, r.options),
        Bson::JavaScriptCode(c) => format!("Code({})", quote(c)),
        Bson::JavaScriptCodeWithScope(c) => format!("Code({})", quote(&c.code)),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => format!("NumberLong({n})"),
        Bson::Timestamp(t) => format!("Timestamp({}, {})", t.time, t.increment),
        Bson::Binary(bin) if bin.subtype == bson::spec::BinarySubtype::Uuid && bin.bytes.len() == 16 => {
            let h = data_encoding::HEXLOWER.encode(&bin.bytes);
            format!("UUID(\"{}-{}-{}-{}-{}\")", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
        }
        Bson::Binary(bin) => format!("BinData({}, \"{}\")", u8::from(bin.subtype), data_encoding::BASE64.encode(&bin.bytes)),
        Bson::ObjectId(o) => format!("ObjectId(\"{o}\")"),
        Bson::DateTime(d) => format!("ISODate(\"{}\")", date_text(d)),
        Bson::Symbol(s) => quote(s),
        Bson::Decimal128(d) => format!("NumberDecimal(\"{d}\")"),
        Bson::MaxKey => "MaxKey()".into(),
        Bson::MinKey => "MinKey()".into(),
        Bson::DbPointer(_) => "DBPointer()".into(),
    }
}

const INLINE_MAX: usize = 72;

pub fn print_bson(b: &Bson) -> String {
    let mut out = String::new();
    write_bson(&mut out, b, 0);
    out
}

#[cfg(test)]
pub fn print_document(d: &Document) -> String {
    let mut out = String::new();
    write_doc(&mut out, d, 0);
    out
}

fn pad(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_doc(out: &mut String, d: &Document, level: usize) {
    if d.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push_str("{\n");
    let n = d.len();
    for (i, (k, v)) in d.iter().enumerate() {
        pad(out, level + 1);
        out.push_str(&quote(k));
        out.push_str(": ");
        write_bson(out, v, level + 1);
        if i + 1 < n {
            out.push(',');
        }
        out.push('\n');
    }
    pad(out, level);
    out.push('}');
}

fn write_bson(out: &mut String, b: &Bson, level: usize) {
    match b {
        Bson::Document(d) => write_doc(out, d, level),
        Bson::Array(a) if a.is_empty() => out.push_str("[]"),
        Bson::Array(a) => {
            let scalars = a.iter().all(|v| !matches!(v, Bson::Document(_) | Bson::Array(_)));
            if scalars {
                let line: Vec<String> = a.iter().map(scalar_text).collect();
                let len: usize = line.iter().map(|s| s.len() + 2).sum();
                if len <= INLINE_MAX {
                    let _ = write!(out, "[{}]", line.join(", "));
                    return;
                }
            }
            out.push_str("[\n");
            for (i, v) in a.iter().enumerate() {
                pad(out, level + 1);
                write_bson(out, v, level + 1);
                if i + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, level);
            out.push(']');
        }
        other => out.push_str(&scalar_text(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_people_type_in_a_shell() {
        let v = parse("{ name: 'Ana', age: {$gt: 30,}, 'a.b': 1, tags: [\"x\", 2.5, -3], ok: true, n: null, // note\n }").unwrap();
        let Val::Obj(f) = &v else { panic!() };
        let keys: Vec<&str> = f.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["name", "age", "a.b", "tags", "ok", "n"]);
        assert_eq!(f[3].1, Val::Arr(vec![Val::Str("x".into()), Val::Float(2.5), Val::Int(-3)]));
        assert_eq!(parse("/^a.*[/]b/ig").unwrap(), Val::Regex("^a.*[/]b".into(), "gi".into()));
        assert_eq!(parse("new Date(0)").unwrap(), Val::Call("Date".into(), vec![Val::Int(0)]));
        assert_eq!(parse("\"\\u00e9\\n\"").unwrap(), Val::Str("é\n".into()));
        let e = parse("{a: 1 b: 2}").unwrap_err();
        assert_eq!(e.at, 6, "{e:?}");
        assert!(parse("{a: foo}").unwrap_err().message.contains("quotes"));
        assert!(parse("'open").is_err());
        assert_eq!(parse("400 / 6378.1").unwrap(), Val::Float(400. / 6378.1));
        assert_eq!(parse("[7 * 24 * 3600, 10 - 2 * 3, 9 / 3, 7 / 2, -1 + 1]").unwrap(), Val::Arr(vec![Val::Int(604800), Val::Int(4), Val::Int(3), Val::Float(3.5), Val::Int(0)]));
        assert_eq!(parse("{a: 1 /* note */, b: 2 // end\n}").unwrap(), Val::Obj(vec![("a".into(), Val::Int(1)), ("b".into(), Val::Int(2))]));
        assert!(parse("1 + 'a'").is_err());
    }

    #[test]
    fn constructors_become_their_types() {
        let d = to_document(&parse(
            r#"{_id: ObjectId("65a1b2c3d4e5f60718293a4b"), at: ISODate("2024-01-02T03:04:05Z"), day: ISODate("2024-01-02"),
               n: 5, big: 9007199254740993, l: NumberLong(5), i: NumberInt("7"), f: 5.0, dec: NumberDecimal("1.25"),
               u: UUID("123e4567-e89b-12d3-a456-426614174000"), ts: Timestamp(10, 2), x: {"$oid": "65a1b2c3d4e5f60718293a4c"},
               q: {$regex: "a", $options: "i"}}"#,
        ).unwrap())
        .unwrap();
        assert!(matches!(d.get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(d.get_datetime("at").unwrap().timestamp_millis(), 1704164645000);
        assert_eq!(d.get_datetime("day").unwrap().timestamp_millis(), 1704153600000);
        assert_eq!(d.get("n"), Some(&Bson::Int32(5)));
        assert_eq!(d.get("big"), Some(&Bson::Int64(9007199254740993)));
        assert_eq!(d.get("l"), Some(&Bson::Int64(5)));
        assert_eq!(d.get("i"), Some(&Bson::Int32(7)));
        assert_eq!(d.get("f"), Some(&Bson::Double(5.0)));
        assert!(matches!(d.get("dec"), Some(Bson::Decimal128(_))));
        assert!(matches!(d.get("u"), Some(Bson::Binary(b)) if b.bytes.len() == 16));
        assert!(matches!(d.get("ts"), Some(Bson::Timestamp(t)) if t.time == 10 && t.increment == 2));
        assert!(matches!(d.get("x"), Some(Bson::ObjectId(_))));
        assert!(matches!(d.get("q"), Some(Bson::Document(_))));
        assert!(to_document(&parse("[1]").unwrap()).unwrap_err().contains("array"));
        assert!(to_bson(&parse("ObjectId('xyz')").unwrap()).is_err());
    }

    #[test]
    fn printed_documents_read_back_the_same() {
        let src = r#"{_id: ObjectId("65a1b2c3d4e5f60718293a4b"), name: "Ana \"A\"", n: 5, l: NumberLong(5), f: 5.0, g: 0.1,
            at: ISODate("2024-01-02T03:04:05.123Z"), tags: ["a", "b"], nested: {deep: [ {x: 1}, [] ], e: {}},
            u: UUID("123e4567-e89b-12d3-a456-426614174000"), r: /a+/i, dec: NumberDecimal("1.25"), nil: null}"#;
        let d = to_document(&parse(src).unwrap()).unwrap();
        let text = print_document(&d);
        assert!(text.starts_with("{\n  \"_id\": ObjectId(\"65a1b2c3d4e5f60718293a4b\"),\n"), "{text}");
        assert!(text.contains("\"l\": NumberLong(5)") && text.contains("\"f\": 5.0") && text.contains("\"tags\": [\"a\", \"b\"]"), "{text}");
        let back = to_document(&parse(&text).unwrap()).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn json_for_the_search_engines() {
        let v = to_json(&parse("{query: {match_all: {}}, size: 10, 'sort': [{at: 'desc'}]}").unwrap()).unwrap();
        assert_eq!(v.to_string(), r#"{"query":{"match_all":{}},"size":10,"sort":[{"at":"desc"}]}"#);
        assert!(to_json(&parse("ObjectId()").unwrap()).unwrap_err().contains("MongoDB"));
    }
}
