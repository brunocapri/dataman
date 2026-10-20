use std::cell::RefCell;
use std::fmt::Write as _;

use boa_engine::{Context, JsArgs, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source, js_string};
use mongodb::bson::{Bson, Document};

use super::mql::{self, Op};
use super::shell::{self, Parser, Val};
use super::{Body, Doc, Docs, Failed, Output, Page};
use crate::guard::WriteClass;

const PRELUDE: &str = include_str!("js_prelude.js");

const LOOP_LIMIT: u64 = if cfg!(test) { 200_000 } else { 20_000_000 };
const RECURSION_LIMIT: usize = 400;
const PRINT_LINES: usize = 5_000;
const PRINT_BYTES: usize = 1 << 20;
const SHOWN_DOCS: usize = 1_000;

pub(crate) trait Server {
    fn run(&mut self, db: &str, op: &Op, page: Page) -> Result<Output, String>;
    fn command(&mut self, db: &str, cmd: Document, admin: bool) -> Result<Document, String>;
    fn writable(&self, db: &str) -> bool;
    fn cancelled(&self) -> bool;
    fn sleep(&mut self, ms: u64);
}

pub(crate) struct Script {
    pub output: Output,
    pub rerun: Option<(String, Op)>,
    pub printed: Vec<String>,
    pub writes: Vec<String>,
    pub db: String,
}

struct Session {
    server: Box<dyn Server>,
    ceiling: u8,
    cleared: String,
    printed: Vec<String>,
    printed_bytes: usize,
    writes: Vec<String>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

fn rank(c: &WriteClass) -> u8 {
    match c {
        WriteClass::Read => 0,
        WriteClass::SafeWrite => 1,
        WriteClass::Destructive { .. } => 2,
    }
}

fn what(c: &WriteClass) -> String {
    match c {
        WriteClass::Read => "reads".into(),
        WriteClass::SafeWrite => "writes".into(),
        WriteClass::Destructive { reasons } => reasons.join(", "),
    }
}

fn js_err(message: impl Into<String>) -> boa_engine::JsError {
    JsNativeError::error().with_message(message.into()).into()
}

fn with_session<T>(f: impl FnOnce(&mut Session) -> Result<T, String>) -> JsResult<T> {
    SESSION.with(|s| match s.borrow_mut().as_mut() {
        Some(s) => f(s).map_err(js_err),
        None => Err(js_err("no session")),
    })
}

fn arg_str(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<Option<String>> {
    let v = args.get_or_undefined(i);
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    Ok(Some(v.to_string(ctx)?.to_std_string_escaped()))
}

fn js_str(s: &str) -> JsValue {
    JsValue::from(JsString::from(s))
}

impl Session {
    fn call(&mut self, db: &str, coll: Option<&str>, calls: &str, page: Option<Page>) -> Result<String, String> {
        if self.server.cancelled() {
            return Err("the script was stopped".into());
        }
        let op = mql::op_of(coll, decode_calls(calls)?).map_err(|e| e.message)?;
        self.allow(db, &op)?;
        if let (Op::Command { doc, admin }, None) = (&op, page) {
            let r = self.server.command(db, doc.clone(), *admin)?;
            return Ok(format!("{{k: \"value\", v: {}}}", js_text(&Bson::Document(r))));
        }
        let out = self.server.run(db, &op, page.unwrap_or_default())?;
        if let Output::Written { summary, .. } = &out {
            self.writes.push(summary.clone());
        }
        Ok(describe(&out))
    }

    fn allow(&self, db: &str, op: &Op) -> Result<(), String> {
        let class = op.class();
        if class.is_write() && !self.server.writable(db) {
            return Err(format!("{db} is read-only: writes are refused"));
        }
        if rank(&class) > self.ceiling {
            return Err(format!(
                "refused: this call {} — more than the script was read as doing before it ran ({}). Write the call out plainly (the method and its filter, e.g. db.orders.deleteMany({{status: 'old'}})) so it is checked first.",
                what(&class),
                self.cleared
            ));
        }
        Ok(())
    }

    fn print(&mut self, line: String) {
        if self.printed.len() >= PRINT_LINES || self.printed_bytes >= PRINT_BYTES {
            if self.printed.last().is_none_or(|l| !l.starts_with("… more printed")) {
                self.printed.push("… more printed than is kept".into());
            }
            return;
        }
        self.printed_bytes += line.len();
        self.printed.push(line);
    }
}

fn n_op(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let db = arg_str(args, 0, ctx)?.unwrap_or_default();
    let coll = arg_str(args, 1, ctx)?;
    let calls = arg_str(args, 2, ctx)?.unwrap_or_default();
    with_session(|s| s.call(&db, coll.as_deref(), &calls, None)).map(|t| js_str(&t))
}

fn n_cursor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let db = arg_str(args, 0, ctx)?.unwrap_or_default();
    let coll = arg_str(args, 1, ctx)?;
    let calls = arg_str(args, 2, ctx)?.unwrap_or_default();
    let skip = args.get_or_undefined(3).to_number(ctx)?.max(0.) as u64;
    let limit = (args.get_or_undefined(4).to_number(ctx)?.max(1.) as usize).min(10_000);
    with_session(|s| s.call(&db, coll.as_deref(), &calls, Some(Page { skip, limit }))).map(|t| js_str(&t))
}

fn n_print(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let line = arg_str(args, 0, ctx)?.unwrap_or_default();
    with_session(|s| {
        s.print(line);
        Ok(())
    })?;
    Ok(JsValue::undefined())
}

fn n_pretty(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = arg_str(args, 0, ctx)?.unwrap_or_default();
    let out = shell::parse(&text).ok().and_then(|v| shell::to_bson(&v).ok()).map(|b| shell::print_bson(&b)).unwrap_or(text);
    Ok(js_str(&out))
}

fn n_oid(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(js_str(&mongodb::bson::oid::ObjectId::new().to_hex()))
}

fn n_uuid(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(js_str(&mongodb::bson::Uuid::new().to_string()))
}

fn n_date(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = arg_str(args, 0, ctx)?.unwrap_or_default();
    match shell::parse_date(&s) {
        Some(d) => Ok(JsValue::from(d.timestamp_millis() as f64)),
        None => Err(JsNativeError::typ().with_message(format!("ISODate(\"{s}\"): not a date")).into()),
    }
}

fn n_sleep(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ms = args.get_or_undefined(0).to_number(ctx)?.clamp(0., 600_000.) as u64;
    with_session(|s| {
        s.server.sleep(ms);
        if s.server.cancelled() { Err("the script was stopped".into()) } else { Ok(()) }
    })?;
    Ok(JsValue::undefined())
}

fn decode_calls(text: &str) -> Result<Vec<(String, Vec<Val>)>, String> {
    let Val::Arr(calls) = shell::parse(text).map_err(|e| format!("a value the shell cannot store: {}", e.message))? else {
        return Err("no calls".into());
    };
    calls
        .into_iter()
        .map(|c| match c {
            Val::Arr(mut pair) if pair.len() == 2 => {
                let args = pair.pop();
                match (pair.pop(), args) {
                    (Some(Val::Str(name)), Some(Val::Arr(args))) => Ok((name, args)),
                    _ => Err("a call the shell cannot read".to_string()),
                }
            }
            _ => Err("a call the shell cannot read".to_string()),
        })
        .collect()
}

fn describe(out: &Output) -> String {
    let body = |b: &Body| match b {
        Body::Bson(b) => js_text(b),
        Body::Json(j) => j.to_string(),
    };
    match out {
        Output::Docs(d) => {
            let mut s = String::from("{k: \"docs\", docs: [");
            for (i, doc) in d.docs.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&body(&doc.body));
            }
            let _ = write!(s, "], more: {}, total: {}}}", d.more, d.total.map_or("null".into(), |t| t.to_string()));
            s
        }
        Output::Value(v) => format!("{{k: \"value\", v: {}}}", body(v)),
        Output::Written { affected, summary, detail } => {
            format!("{{k: \"written\", affected: {affected}, summary: {}, detail: {}}}", shell::quote(summary), detail.as_ref().map_or("null".into(), body))
        }
        Output::Use(db) => format!("{{k: \"use\", db: {}}}", shell::quote(db)),
    }
}

pub(crate) fn js_text(b: &Bson) -> String {
    let mut out = String::new();
    write_js(&mut out, b);
    out
}

fn write_js(out: &mut String, b: &Bson) {
    match b {
        Bson::Document(d) => {
            out.push('{');
            for (i, (k, v)) in d.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                // A literal `"__proto__": …` would set the prototype: a computed key does not.
                if k == "__proto__" {
                    let _ = write!(out, "[{}]: ", shell::quote(k));
                } else {
                    let _ = write!(out, "{}: ", shell::quote(k));
                }
                write_js(out, v);
            }
            out.push('}');
        }
        Bson::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_js(out, v);
            }
            out.push(']');
        }
        Bson::Double(f) if f.is_nan() => out.push_str("NaN"),
        Bson::Double(f) if f.is_infinite() => out.push_str(if *f > 0. { "Infinity" } else { "-Infinity" }),
        Bson::Double(f) => {
            let _ = write!(out, "{f}");
        }
        Bson::Int32(n) => {
            let _ = write!(out, "{n}");
        }
        Bson::Int64(n) => {
            let _ = write!(out, "__B.long(\"{n}\")");
        }
        Bson::String(s) | Bson::Symbol(s) => out.push_str(&shell::quote(s)),
        Bson::Boolean(v) => out.push_str(if *v { "true" } else { "false" }),
        Bson::Null | Bson::DbPointer(_) => out.push_str("null"),
        Bson::Undefined => out.push_str("undefined"),
        Bson::ObjectId(o) => {
            let _ = write!(out, "__B.oid(\"{o}\")");
        }
        Bson::DateTime(d) => {
            let _ = write!(out, "__B.date({})", d.timestamp_millis());
        }
        Bson::Decimal128(d) => {
            let _ = write!(out, "__B.dec(\"{d}\")");
        }
        Bson::Timestamp(t) => {
            let _ = write!(out, "__B.ts({}, {})", t.time, t.increment);
        }
        Bson::Binary(bin) if bin.subtype == mongodb::bson::spec::BinarySubtype::Uuid && bin.bytes.len() == 16 => {
            let _ = write!(out, "__B.uuid(\"{}\")", data_encoding::HEXLOWER.encode(&bin.bytes));
        }
        Bson::Binary(bin) => {
            let _ = write!(out, "__B.bin({}, \"{}\")", u8::from(bin.subtype), data_encoding::BASE64.encode(&bin.bytes));
        }
        Bson::RegularExpression(r) => {
            let _ = write!(out, "__B.re({}, {})", shell::quote(&r.pattern), shell::quote(&r.options));
        }
        Bson::JavaScriptCode(c) => {
            let _ = write!(out, "__B.code({})", shell::quote(c));
        }
        Bson::JavaScriptCodeWithScope(c) => {
            let _ = write!(out, "__B.code({})", shell::quote(&c.code));
        }
        Bson::MinKey => out.push_str("__B.min"),
        Bson::MaxKey => out.push_str("__B.max"),
    }
}

fn prepare(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    for line in text.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let t = body.trim_start();
        let lead = &body[..body.len() - t.len()];
        let word = |w: &str| t.strip_prefix(w).filter(|r| r.starts_with(char::is_whitespace));
        let rest_of = |r: &str| -> Option<(String, String)> {
            let r = r.trim_start();
            let end = r.find(|c: char| c == ';' || c.is_whitespace()).unwrap_or(r.len());
            let (name, after) = r.split_at(end);
            let after = after.trim_start().strip_prefix(';').unwrap_or(after);
            (!name.is_empty()).then(|| (name.trim_matches(|c| c == '"' || c == '\'').to_string(), after.to_string()))
        };
        if let Some((name, after)) = word("use").and_then(rest_of) {
            let _ = write!(out, "{lead}db = __B.database({});{after}{nl}", shell::quote(&name));
        } else if let Some((what, after)) = word("show").and_then(rest_of) {
            match what.as_str() {
                "dbs" | "databases" => {
                    let _ = write!(out, "{lead}db.adminCommand({{listDatabases: 1, nameOnly: true}}).databases.map(d => d.name);{after}{nl}");
                }
                "collections" | "tables" => {
                    let _ = write!(out, "{lead}db.getCollectionNames();{after}{nl}");
                }
                _ => out.push_str(line),
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

fn context() -> Result<Context, Failed> {
    let mut ctx = Context::default();
    ctx.runtime_limits_mut().set_loop_iteration_limit(LOOP_LIMIT);
    ctx.runtime_limits_mut().set_recursion_limit(RECURSION_LIMIT);
    let natives: [(&str, usize, boa_engine::native_function::NativeFunctionPointer); 8] = [
        ("__n_op", 3, n_op),
        ("__n_cursor", 5, n_cursor),
        ("__n_print", 1, n_print),
        ("__n_pretty", 1, n_pretty),
        ("__n_oid", 0, n_oid),
        ("__n_uuid", 0, n_uuid),
        ("__n_date", 1, n_date),
        ("__n_sleep", 1, n_sleep),
    ];
    for (name, n, f) in natives {
        ctx.register_global_callable(JsString::from(name), n, NativeFunction::from_fn_ptr(f)).map_err(|e| Failed { message: e.to_string(), at: None })?;
    }
    ctx.eval(Source::from_bytes(PRELUDE)).map_err(|e| Failed { message: format!("the shell did not start: {e}"), at: None })?;
    Ok(ctx)
}

const SCRIPT: &str = "script";

fn failed(e: boa_engine::JsError, ctx: &mut Context, text: &str) -> Failed {
    let m = match e.try_native(ctx) {
        Ok(n) => n.to_string(),
        Err(_) => e.to_string(),
    };
    let mut m = m.strip_prefix("Error: ").map(str::to_string).unwrap_or(m);
    let mut at = None;
    if m.ends_with(')')
        && let Some(i) = m.rfind(" (")
    {
        let inner = &m[i + 2..m.len() - 1];
        let mut parts = inner.rsplit(':');
        let (col, line) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let place = |s: &str| !s.is_empty() && (s == "?" || s.bytes().all(|b| b.is_ascii_digit()));
        if place(col) && place(line) {
            if inner.starts_with(SCRIPT)
                && let (Ok(line), Ok(col)) = (line.parse::<usize>(), col.parse::<usize>())
            {
                at = syntax_at(&format!("line {line}, col {col}"), text);
            }
            m.truncate(i);
        }
    }
    Failed { message: m, at }
}

fn syntax_at(message: &str, text: &str) -> Option<usize> {
    let at = message.rfind("line ")?;
    let mut nums = message[at + 5..].split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty());
    let line: usize = nums.next()?.parse().ok()?;
    let col: usize = nums.next()?.parse().ok()?;
    let start: usize = text.split_inclusive('\n').take(line.saturating_sub(1)).map(str::len).sum();
    let l = text[start..].split('\n').next().unwrap_or("");
    let off: usize = l.chars().take(col.saturating_sub(1)).map(char::len_utf8).sum();
    Some((start + off).min(text.len()))
}

pub(crate) fn check(text: &str) -> Result<(), Failed> {
    let prepared = prepare(text);
    let mut ctx = Context::default();
    boa_engine::Script::parse(Source::from_bytes(&prepared), None, &mut ctx).map(|_| ()).map_err(|e| {
        let m = e.to_string();
        let at = syntax_at(&m, text);
        let m = m.strip_prefix("SyntaxError: ").unwrap_or(&m);
        let m = m.rfind(" at line ").map_or(m, |i| &m[..i]);
        Failed { at, message: format!("SyntaxError: {m}") }
    })
}

pub(crate) fn run(text: &str, db: &str, class: &WriteClass, server: Box<dyn Server>, page: Page) -> Result<Script, Failed> {
    SESSION.with(|s| {
        *s.borrow_mut() = Some(Session { server, ceiling: rank(class), cleared: what(class), printed: Vec::new(), printed_bytes: 0, writes: Vec::new() })
    });
    let r = run_in_session(text, db, page);
    let session = SESSION.with(|s| s.borrow_mut().take());
    let mut script = r?;
    if let Some(s) = session {
        script.printed = s.printed;
        script.writes = s.writes;
    }
    if matches!(script.output, Output::Value(Body::Bson(Bson::Undefined))) && !script.printed.is_empty() {
        script.output = Output::Value(Body::Json(serde_json::Value::String(std::mem::take(&mut script.printed).join("\n"))));
    }
    Ok(script)
}

fn run_in_session(text: &str, db: &str, page: Page) -> Result<Script, Failed> {
    let mut ctx = context()?;
    ctx.eval(Source::from_bytes(&format!("var db = __B.database({});", shell::quote(db)))).map_err(|e| Failed { message: e.to_string(), at: None })?;
    check(text)?;
    let prepared = prepare(text);
    let last = match ctx.eval(Source::from_bytes(&prepared).with_path(std::path::Path::new(SCRIPT))) {
        Ok(v) => v,
        Err(e) => return Err(failed(e, &mut ctx, text)),
    };
    let _ = ctx.run_jobs();
    let global = ctx.global_object();
    global.set(js_string!("__last"), last, false, &mut ctx).map_err(|e| Failed { message: e.to_string(), at: None })?;
    let fin = ctx
        .eval(Source::from_bytes("__B.final(__last)"))
        .and_then(|v| v.to_string(&mut ctx))
        .map(|s| s.to_std_string_escaped())
        .map_err(|e| failed(e, &mut ctx, text))?;
    let end_db = ctx
        .eval(Source::from_bytes("(db && typeof db.getName === 'function') ? db.getName() : ''"))
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| db.to_string());
    let (kind, rest) = fin.split_at(fin.char_indices().nth(1).map_or(fin.len(), |(i, _)| i));
    let mut rerun = None;
    let output = match kind {
        "C" => {
            let (cdb, op) = cursor_op(rest)?;
            let out = SESSION.with(|s| -> Result<Output, String> {
                let mut s = s.borrow_mut();
                let s = s.as_mut().ok_or("no session")?;
                s.allow(&cdb, &op)?;
                s.server.run(&cdb, &op, page)
            });
            let out = out.map_err(|m| Failed { message: m, at: None })?;
            rerun = Some((cdb, op));
            out
        }
        "U" => Output::Value(Body::Bson(Bson::Undefined)),
        "S" => Output::Value(Body::Json(serde_json::Value::String(rest.to_string()))),
        _ => value_output(rest)?,
    };
    Ok(Script { output, rerun, printed: Vec::new(), writes: Vec::new(), db: end_db })
}

fn cursor_op(spec: &str) -> Result<(String, Op), Failed> {
    let fail = |m: String| Failed { message: m, at: None };
    let Val::Obj(fields) = shell::parse(spec).map_err(|e| fail(e.message))? else { return Err(fail("no cursor".into())) };
    let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let Some(Val::Str(db)) = get("db") else { return Err(fail("no cursor".into())) };
    let coll = match get("coll") {
        Some(Val::Str(c)) => Some(c),
        _ => None,
    };
    let calls = match get("calls") {
        Some(v @ Val::Arr(_)) => decode_calls(&print_val(&v)).map_err(fail)?,
        _ => Vec::new(),
    };
    let op = mql::op_of(coll.as_deref(), calls).map_err(|e| fail(e.message))?;
    Ok((db, op))
}

fn print_val(v: &Val) -> String {
    match v {
        Val::Null => "null".into(),
        Val::Undefined => "undefined".into(),
        Val::Bool(b) => b.to_string(),
        Val::Int(n) => n.to_string(),
        Val::Float(f) if f.is_nan() => "NaN".into(),
        Val::Float(f) if f.is_infinite() => if *f > 0. { "Infinity".into() } else { "-Infinity".into() },
        Val::Float(f) => format!("{f:?}"),
        Val::Str(s) => shell::quote(s),
        Val::Arr(a) => format!("[{}]", a.iter().map(print_val).collect::<Vec<_>>().join(", ")),
        Val::Obj(f) => format!("{{{}}}", f.iter().map(|(k, v)| format!("{}: {}", shell::quote(k), print_val(v))).collect::<Vec<_>>().join(", ")),
        Val::Call(n, a) => format!("{n}({})", a.iter().map(print_val).collect::<Vec<_>>().join(", ")),
        Val::Regex(p, f) => format!("RegExp({}, {})", shell::quote(p), shell::quote(f)),
    }
}

fn value_output(text: &str) -> Result<Output, Failed> {
    let fail = |m: String| Failed { message: m, at: None };
    let b = shell::to_bson(&shell::parse(text).map_err(|e| fail(e.message))?).map_err(fail)?;
    Ok(match b {
        Bson::Array(a) if !a.is_empty() && a.iter().all(|v| matches!(v, Bson::Document(_))) => {
            let total = a.len();
            let docs = a
                .into_iter()
                .take(SHOWN_DOCS)
                .filter_map(|v| match v {
                    Bson::Document(d) => Some(Doc { id: d.get("_id").cloned().map(Body::Bson), index: None, body: Body::doc(d) }),
                    _ => None,
                })
                .collect();
            Output::Docs(Docs { docs, source: None, total: Some(total as u64), skip: 0, more: false })
        }
        other => Output::Value(Body::Bson(other)),
    })
}

const BOUNDED: &[&str] = &["insertOne", "insertMany", "insert", "updateOne", "replaceOne", "findOneAndUpdate", "findOneAndReplace", "findOneAndDelete", "findAndModify", "deleteOne", "save"];
const FILTERED: &[&str] = &["updateMany", "deleteMany", "remove", "update"];
const DESTRUCTIVE: &[(&str, &str)] = &[
    ("drop", "drops a collection"),
    ("dropDatabase", "drops the database"),
    ("dropIndex", "drops an index"),
    ("dropIndexes", "drops indexes"),
    ("createIndex", "creates an index"),
    ("createIndexes", "creates indexes"),
    ("ensureIndex", "creates an index"),
    ("createCollection", "creates a collection"),
    ("createView", "creates a view"),
    ("renameCollection", "renames a collection"),
];

pub(crate) fn is_script_start(stmt: &str) -> bool {
    let mut p = Parser::new(stmt);
    p.skip_ws();
    match p.ident() {
        Some("show" | "use") => false,
        Some("db") => {
            p.skip_ws();
            p.peek() == Some('=') && !p.rest().starts_with("==")
        }
        _ => true,
    }
}

// Not proof (a method name the script builds is not seen): `run` refuses any call that does more.
pub(crate) fn class(text: &str) -> Result<WriteClass, Failed> {
    check(text)?;
    let mut reasons: Vec<String> = Vec::new();
    let mut safe = false;
    let push = |r: String, reasons: &mut Vec<String>| {
        if !reasons.contains(&r) {
            reasons.push(r);
        }
    };
    if text.contains("$out") || text.contains("$merge") {
        push("$out / $merge writes a collection".into(), &mut reasons);
    }
    for (name, args) in calls_in(text) {
        if BOUNDED.contains(&name.as_str()) {
            safe = true;
        } else if FILTERED.contains(&name.as_str()) {
            match first_arg(args).and_then(|v| shell::to_document(&v).ok()) {
                Some(f) if !f.is_empty() => safe = true,
                Some(_) => push(format!("{name} without a filter"), &mut reasons),
                None => push(format!("{name} with a filter the script computes"), &mut reasons),
            }
        } else if let Some((_, why)) = DESTRUCTIVE.iter().find(|(n, _)| *n == name) {
            push((*why).to_string(), &mut reasons);
        } else if name == "bulkWrite" {
            let op = first_arg(args).map(|v| mql::op_of(Some("c"), vec![("bulkWrite".into(), vec![v])]));
            match op {
                Some(Ok(op)) => match op.class() {
                    WriteClass::Destructive { reasons: r } => r.into_iter().for_each(|r| push(r, &mut reasons)),
                    _ => safe = true,
                },
                _ => push("bulkWrite with operations the script computes".into(), &mut reasons),
            }
        } else if name == "runCommand" || name == "adminCommand" {
            let op = first_arg(args).map(|v| mql::op_of(None, vec![(name.clone(), vec![v])]));
            match op {
                Some(Ok(op)) => match op.class() {
                    WriteClass::Destructive { reasons: r } => r.into_iter().for_each(|r| push(r, &mut reasons)),
                    WriteClass::SafeWrite => safe = true,
                    WriteClass::Read => {}
                },
                _ => push(format!("{name} with a command the script computes"), &mut reasons),
            }
        }
    }
    Ok(if !reasons.is_empty() {
        WriteClass::Destructive { reasons }
    } else if safe {
        WriteClass::SafeWrite
    } else {
        WriteClass::Read
    })
}

fn first_arg(args: &str) -> Option<Val> {
    let mut p = Parser::new(args);
    p.skip_ws();
    let v = p.value().ok()?;
    p.skip_ws();
    matches!(p.peek(), Some(',' | ')')).then_some(v)
}

fn calls_in(text: &str) -> Vec<(String, &str)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = text[i + 2..].find("*/").map_or(b.len(), |j| i + 2 + j + 1);
            }
            b'.' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$') {
                    j += 1;
                }
                let name = &text[start..j];
                let mut k = j;
                while k < b.len() && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                if !name.is_empty() && b.get(k) == Some(&b'(') {
                    out.push((name.to_string(), &text[k + 1..]));
                }
                i = j.max(i + 1);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    struct Fake {
        log: std::rc::Rc<RefCell<Vec<String>>>,
        read_only: bool,
    }

    impl Server for Fake {
        fn run(&mut self, db: &str, op: &Op, page: Page) -> Result<Output, String> {
            self.log.borrow_mut().push(format!("{db} {op:?} {} {}", page.skip, page.limit));
            Ok(match op {
                Op::Find { one, filter, limit: lim, .. } => {
                    let all: Vec<Doc> = (0..120)
                        .map(|i| doc! { "_id": i, "n": i, "at": mongodb::bson::DateTime::from_millis(1_700_000_000_000 + i as i64), "l": Bson::Int64(i as i64) })
                        .filter(|d| filter.get_i32("n").map_or(true, |n| d.get_i32("n") == Ok(n)))
                        .map(|d| Doc { id: d.get("_id").cloned().map(Body::Bson), index: None, body: Body::doc(d) })
                        .take(lim.map_or(usize::MAX, |l| l.unsigned_abs() as usize))
                        .collect();
                    let skip = page.skip as usize;
                    let limit = if *one { 1 } else { page.limit };
                    let docs: Vec<Doc> = all.iter().skip(skip).take(limit).cloned().collect();
                    let more = skip + docs.len() < all.len() && !*one;
                    Output::Docs(Docs { docs, source: None, total: Some(all.len() as u64), skip: page.skip, more })
                }
                Op::Count { .. } => Output::Value(Body::Bson(Bson::Int64(120))),
                Op::Insert { docs, .. } => Output::Written {
                    affected: docs.len() as u64,
                    summary: format!("{} inserted", docs.len()),
                    detail: Some(Body::doc(doc! { "insertedIds": docs.iter().map(|d| d.get("_id").cloned().unwrap_or(Bson::Int32(0))).collect::<Vec<_>>() })),
                },
                Op::Delete { .. } => Output::Written { affected: 3, summary: "3 deleted".into(), detail: None },
                Op::Update { .. } => Output::Written { affected: 1, summary: "1 matched, 1 modified".into(), detail: Some(Body::doc(doc! { "matchedCount": 1i64, "modifiedCount": 1i64 })) },
                _ => Output::Value(Body::doc(doc! { "ok": 1 })),
            })
        }
        fn command(&mut self, db: &str, cmd: Document, admin: bool) -> Result<Document, String> {
            self.log.borrow_mut().push(format!("{db} command {cmd} {admin}"));
            Ok(doc! { "ok": 1, "version": "8.0.0", "databases": [{ "name": "admin" }, { "name": "shop" }] })
        }
        fn writable(&self, _: &str) -> bool {
            !self.read_only
        }
        fn cancelled(&self) -> bool {
            false
        }
        fn sleep(&mut self, _: u64) {}
    }

    fn script(text: &str) -> (Result<Script, Failed>, Vec<String>) {
        let log = std::rc::Rc::new(RefCell::new(Vec::new()));
        let class = class(text).unwrap_or(WriteClass::Read);
        let r = run(text, "shop", &class, Box::new(Fake { log: log.clone(), read_only: false }), Page::default());
        let l = log.borrow().clone();
        (r, l)
    }

    /// Read as a read, a script never reaches a write, however it names or reaches the call.
    #[test]
    fn indirect_writes_stop_at_the_ceiling() {
        let writes = ["Insert", "Delete", "Update", "Replace", "FindAndModify", "BulkWrite", "Drop", "Rename", "CreateIndex", "CreateCollection", "shutdown", "repair", "$out", "$merge"];
        let mut read = 0;
        for text in [
            "const m = ['delete', 'Many'].join(''); db.users[m]({x: 1})",
            "const d = db; const c = d['us' + 'ers']; c[String.fromCharCode(100, 114, 111, 112)]()",
            "Function('return d' + 'b')().users['dr' + 'op']()",
            "eval('db.users.dr' + 'op()')",
            "globalThis['d' + 'b'].getSiblingDB('admin').runCommand({['shut' + 'down']: 1})",
            "const o = {get x() { return db.users['remove'.slice(0)]({_id: 1}) }}; o.x",
            "db.users.find({}).forEach(d => db.users[['delete', 'One'].join('')]({_id: d._id}))",
            "__n_op('shop', 'users', '[[\"drop\", []]]')",
            "__n_cursor('shop', 'users', '[[\"deleteMany\", [{}]]]', 0, 1)",
            "Object.getPrototypeOf(db.users).find = function () { return this['insert' + 'One']({a: 1}) }; db.users.find({})",
            "db.runCommand({['val' + 'idate']: 'users', repair: true})",
            "db.users.aggregate([{['$' + 'out']: 'copy'}])",
            "const s = db.getSiblingDB('other'); s.getCollection('x')['update' + 'Many']({}, {$set: {a: 1}})",
            "new Proxy({}, {get: () => db.users['drop']()}).anything",
            "[1].map(db.users['delete' + 'Many'].bind(db.users, {}))",
            "Reflect.apply(db.users['insert' + 'Many'], db.users, [[{a: 1}]])",
        ] {
            let c = class(text).unwrap_or(WriteClass::Read);
            if c != WriteClass::Read {
                continue;
            }
            read += 1;
            let (_, log) = script(text);
            let wrote: Vec<&String> = log.iter().filter(|l| writes.iter().any(|w| l.contains(w))).collect();
            assert!(wrote.is_empty(), "{text}: {wrote:?}");
        }
        assert!(read >= 12, "most of these read as reads: {read}");
    }

    fn value(text: &str) -> Bson {
        match script(text).0.unwrap_or_else(|f| panic!("{text}: {}", f.message)).output {
            Output::Value(Body::Bson(b)) => b,
            Output::Value(Body::Json(serde_json::Value::String(s))) => Bson::String(s),
            o => panic!("{text}: {o:?}"),
        }
    }

    #[test]
    fn javascript_as_the_shell_runs_it() {
        assert_eq!(value("var a = [1, 2, 3]; let s = 0; for (const x of a) s += x; s * 2"), Bson::Int32(12));
        assert_eq!(value("function f(n) { return n <= 1 ? 1 : n * f(n - 1) }\nf(5)"), Bson::Int32(120));
        assert_eq!(value("const d = new Date(0); d.toISOString()"), Bson::String("1970-01-01T00:00:00.000Z".into()));
        assert!(matches!(value("ISODate('2024-03-01')"), Bson::DateTime(d) if d.timestamp_millis() == 1_709_251_200_000));
        assert!(matches!(value("ObjectId()"), Bson::ObjectId(_)));
        assert_eq!(value("ObjectId('65a1b2c3d4e5f60718293a4b').getTimestamp().getTime()"), Bson::Int64(1_705_095_875_000));
        assert_eq!(value("NumberLong(5) + 1"), Bson::Int32(6));
        assert_eq!(value("({l: NumberLong('9007199254740993'), d: NumberDecimal('1.10')})"), Bson::Document(doc! { "l": Bson::Int64(9007199254740993), "d": Bson::Decimal128("1.10".parse().unwrap()) }));
        assert_eq!(value("typeof Date.now()"), Bson::String("number".into()));
        assert!(matches!(value("UUID()"), Bson::Binary(b) if b.bytes.len() == 16));
        assert_eq!(value("[/a+b/i, MinKey()]"), Bson::Array(vec![Bson::RegularExpression(mongodb::bson::Regex { pattern: "a+b".into(), options: "i".into() }), Bson::MinKey]));
        assert_eq!(value("print('a', 1); printjson({x: NumberLong(2)})"), Bson::String("a 1\n{\n  \"x\": NumberLong(2)\n}".into()));
        let (r, _) = script("print('hello'); 42");
        let r = r.unwrap();
        assert_eq!((r.output, r.printed), (Output::Value(Body::Bson(Bson::Int32(42))), vec!["hello".to_string()]));
        let f = script("let x = 1\nlet = = 2").0.err().unwrap();
        assert_eq!((f.at, f.message.as_str()), (Some(16), "SyntaxError: unexpected token '=', primary expression"), "{f:?}");
        let f = script("let a = 1\n  throw new Error('boom')").0.err().unwrap();
        assert_eq!((f.message.as_str(), f.at), ("boom", Some(18)));
        assert!(script("throw new TypeError('t')").0.err().unwrap().message.starts_with("TypeError"));
        assert!(script("while (true) {}").0.err().unwrap().message.contains("loop"), "loop limit");
    }

    #[test]
    fn the_database_through_the_prelude() {
        let (r, log) = script("let n = 0\ndb.items.find({}).forEach(d => { n += d.n })\nprint(n)\ndb.items.find().sort({n: -1})");
        let r = r.unwrap();
        assert_eq!(r.printed, ["7140"]);
        assert!(matches!(&r.output, Output::Docs(d) if d.docs.len() == 50 && d.more));
        assert!(matches!(&r.rerun, Some((db, Op::Find { sort: Some(s), .. })) if db == "shop" && s.get_i32("n") == Ok(-1)));
        assert!(log[0].contains("Find") && log[0].ends_with(" 0 1000"), "{log:?}");
        assert_eq!(value("const d = db.items.findOne({n: 3}); [d.l instanceof NumberLong, d.at instanceof Date, d._id]"), Bson::Array(vec![Bson::Boolean(true), Bson::Boolean(true), Bson::Int32(3)]));
        assert_eq!(value("db.items.countDocuments({}) === 120"), Bson::Boolean(true));
        assert_eq!(value("db.items.find().toArray().length"), Bson::Int32(120));
        assert_eq!(value("db.items.find().map(d => d.n).length"), Bson::Int32(120));
        assert_eq!(value("const out = []; for (const d of db.items.find().limit(3)) out.push(d.n); out"), Bson::Array(vec![Bson::Int32(0), Bson::Int32(1), Bson::Int32(2)]));
        assert_eq!(value("db.items.insertOne({_id: 7, at: new Date(5)}).insertedId"), Bson::Int32(7));
        assert_eq!(value("db.items.deleteMany({n: {$gt: 1}}).deletedCount"), Bson::Int32(3));
        let (r, log) = script("use other\ndb.getSiblingDB('x').system.users.findOne()\ndb.version()");
        let r = r.unwrap();
        assert_eq!(r.db, "other");
        assert_eq!(r.output, Output::Value(Body::Json(serde_json::Value::String("8.0.0".into()))));
        assert!(log[0].starts_with("x Find") && log[0].contains("system.users"), "{log:?}");
        assert!(log[1].contains("buildInfo") && log[1].ends_with("true"), "{log:?}");
        assert_eq!(value("show dbs"), Bson::Array(vec![Bson::String("admin".into()), Bson::String("shop".into())]));
        assert!(matches!(script("[{a: 1}, {a: 2}]").0.unwrap().output, Output::Docs(d) if d.docs.len() == 2 && d.source.is_none()));
    }

    #[test]
    fn what_a_script_does_is_read_first_and_held_to() {
        let c = |s: &str| class(s).unwrap();
        assert_eq!(c("var n = db.a.countDocuments({}); db.a.find().forEach(printjson)"), WriteClass::Read);
        assert_eq!(c("for (let i = 0; i < 3; i++) db.a.insertOne({i})"), WriteClass::SafeWrite);
        assert_eq!(c("db.a.find().forEach(d => db.a.updateOne({_id: d._id}, {$set: {x: 1}}))"), WriteClass::SafeWrite);
        assert_eq!(c("db.a.deleteMany({status: 'old'}); print('ok')"), WriteClass::SafeWrite);
        assert!(c("db.a.deleteMany({}); 1").is_destructive());
        assert!(c("const f = {}; db.a.deleteMany(f)").is_destructive());
        assert!(c("db.a.drop(); 1").is_destructive());
        assert!(c("db.a.aggregate([{$out: 'b'}]).toArray()").is_destructive());
        assert!(c("db.runCommand({drop: 'a'}); 1").is_destructive());
        assert_eq!(c("db.runCommand({ping: 1}); 1"), WriteClass::Read);
        assert_eq!(c("// db.a.drop()\nprint('db.a.drop()')"), WriteClass::Read);
        assert!(class("let x = ;").unwrap_err().at.is_some());
        let log = std::rc::Rc::new(RefCell::new(Vec::new()));
        let r = run("const m = 'delete' + 'Many'; db.a[m]({})", "shop", &WriteClass::Read, Box::new(Fake { log: log.clone(), read_only: false }), Page::default());
        assert!(r.err().unwrap().message.starts_with("refused: this call deleteMany without a filter"));
        assert!(log.borrow().is_empty());
        let r = run("const m = 'insert' + 'One'; db.a[m]({x: 1})", "shop", &WriteClass::Read, Box::new(Fake { log: log.clone(), read_only: false }), Page::default());
        assert!(r.err().unwrap().message.starts_with("refused: this call writes"));
        let r = run("db.a.insertOne({x: 1})", "shop", &WriteClass::SafeWrite, Box::new(Fake { log: log.clone(), read_only: true }), Page::default());
        assert!(r.err().unwrap().message.contains("read-only"));
        assert_eq!(value("try { db.a.find({$bad: 1}).sort('x').toArray(); 'no' } catch (e) { 'caught' }"), Bson::String("caught".into()));
    }

    #[test]
    fn scripts_and_statements_apart() {
        assert!(is_script_start("var x = 1"));
        assert!(is_script_start("print(1)"));
        assert!(is_script_start("db = db.getSiblingDB('a')"));
        assert!(!is_script_start("db.a.find()"));
        assert!(!is_script_start("  // note\n db.a.find()"));
        assert!(!is_script_start("show dbs"));
        assert!(!is_script_start("db == 1"));
        assert_eq!(prepare("use shop\n  show collections;\nshow dbs"), "db = __B.database(\"shop\");\n  db.getCollectionNames();\ndb.adminCommand({listDatabases: 1, nameOnly: true}).databases.map(d => d.name);");
    }
}
