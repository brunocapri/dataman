//! MongoDB's editor language: mongosh's statements, read into an [`Op`].
//!
//! ```text
//! db.users.find({ age: { $gt: 30 } }, { name: 1 }).sort({ name: 1 }).limit(20)
//! db.orders.aggregate([{ $match: { paid: true } }, { $group: { _id: "$user", n: { $sum: 1 } } }])
//! db.users.updateOne({ _id: ObjectId("…") }, { $set: { vip: true } })
//! db.getCollection("odd name").countDocuments({})
//! db.runCommand({ dbStats: 1 })      show dbs      show collections      use shop
//! ```
//!
//! Statements end at `;` or at a line break outside brackets, unless the next line goes on with
//! `.method(…)`. [`Op::class`] says what a statement does, for the read-only switch, production's
//! confirmations and the MCP tiers.

use std::ops::Range;

use mongodb::bson::{Bson, Document, doc};

use super::shell::{self, ParseError, Parser, Val};
use crate::guard::WriteClass;

/// One statement, read.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    /// `db.getSiblingDB("other")…`: the database it runs in instead of the tab's.
    pub db: Option<String>,
    pub op: Op,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    ShowDbs,
    ShowCollections,
    /// `use name`: the tab's database changes.
    Use(String),
    /// `options`: what else the `find` command takes (`collation`, `hint`, `maxTimeMS`…).
    Find { coll: String, filter: Document, projection: Option<Document>, sort: Option<Document>, skip: u64, limit: Option<i64>, one: bool, options: Document },
    /// `coll: None`: `db.aggregate(…)` (`$currentOp`, `$documents`).
    Aggregate { coll: Option<String>, pipeline: Vec<Document> },
    /// `filter: None`: the collection's estimated count (its metadata).
    Count { coll: String, filter: Option<Document> },
    Distinct { coll: String, field: String, filter: Document },
    Insert { coll: String, docs: Vec<Document> },
    /// `update` is an update document (`{$set: …}`) or a pipeline (an array of stages);
    /// `options` the statement's others (`arrayFilters`, `collation`, `hint`).
    Update { coll: String, filter: Document, update: Bson, many: bool, upsert: bool, options: Document },
    Replace { coll: String, filter: Document, doc: Document, upsert: bool, options: Document },
    Delete { coll: String, filter: Document, many: bool, options: Document },
    /// `findOneAndUpdate` / `…Replace` / `…Delete`: one document changed, returned (`update`:
    /// an update or a replacement; `None`: deleted). `options`: `findAndModify`'s own.
    FindAndModify { coll: String, filter: Document, update: Option<Bson>, options: Document },
    /// `bulkWrite([{insertOne: …}, {updateMany: …}, …])`, run in order.
    BulkWrite { coll: String, ops: Vec<Document> },
    CreateIndex { coll: String, keys: Document, options: Document },
    DropIndex { coll: String, index: Bson },
    Indexes { coll: String },
    DropCollection { coll: String },
    CreateCollection { name: String, options: Document },
    RenameCollection { coll: String, to: String },
    DropDatabase,
    /// `db.stats()` (`coll: None`) and `db.coll.stats()`.
    Stats { coll: Option<String> },
    /// `db.runCommand(…)`, `db.adminCommand(…)` (`admin`), and the helpers that are one command.
    Command { doc: Document, admin: bool },
}

/// Commands that only read (`runCommand`'s first key, in lower case).
const READ_COMMANDS: &[&str] = &[
    "ping", "buildinfo", "hello", "ismaster", "serverstatus", "dbstats", "collstats", "datasize", "listcollections",
    "listdatabases", "listindexes", "count", "distinct", "find", "explain", "hostinfo", "getlog", "getparameter",
    "connectionstatus", "replsetgetstatus", "currentop", "top", "validate", "usersinfo", "rolesinfo", "getcmdlineopts",
    "listcommands", "features", "whatsmyuri", "getmore", "aggregate", "connpoolstats", "shardingstate", "lockinfo",
];

impl Op {
    /// The collection it works on, if one.
    pub fn collection(&self) -> Option<&str> {
        match self {
            Op::Find { coll, .. }
            | Op::Count { coll, .. }
            | Op::Distinct { coll, .. }
            | Op::Insert { coll, .. }
            | Op::Update { coll, .. }
            | Op::Replace { coll, .. }
            | Op::Delete { coll, .. }
            | Op::FindAndModify { coll, .. }
            | Op::BulkWrite { coll, .. }
            | Op::CreateIndex { coll, .. }
            | Op::DropIndex { coll, .. }
            | Op::Indexes { coll }
            | Op::DropCollection { coll }
            | Op::RenameCollection { coll, .. } => Some(coll),
            Op::Aggregate { coll, .. } | Op::Stats { coll } => coll.as_deref(),
            Op::CreateCollection { name, .. } => Some(name),
            _ => None,
        }
    }

    /// What it does: reads, bounded writes (a filter that is not empty, one document), or
    /// everything else (no filter on a `Many`, drops, `$out`, commands that are not reads).
    pub fn class(&self) -> WriteClass {
        let destructive = |why: &str| WriteClass::Destructive { reasons: vec![why.to_string()] };
        match self {
            Op::ShowDbs | Op::ShowCollections | Op::Use(_) | Op::Find { .. } | Op::Count { .. } | Op::Distinct { .. } | Op::Indexes { .. } | Op::Stats { .. } => {
                WriteClass::Read
            }
            Op::Aggregate { pipeline, .. } => {
                if pipeline.iter().any(|s| s.contains_key("$out") || s.contains_key("$merge")) {
                    destructive("$out / $merge writes a collection")
                } else {
                    WriteClass::Read
                }
            }
            Op::Insert { .. } | Op::Replace { .. } | Op::FindAndModify { .. } => WriteClass::SafeWrite,
            Op::BulkWrite { ops, .. } => {
                let unbounded = ops.iter().any(|o| {
                    ["updateMany", "deleteMany"].iter().any(|k| o.get_document(k).is_ok_and(|x| x.get_document("filter").map_or(true, |f| f.is_empty())))
                });
                if unbounded { destructive("bulkWrite: updateMany or deleteMany without a filter") } else { WriteClass::SafeWrite }
            }
            Op::Update { filter, many, .. } | Op::Delete { filter, many, .. } => {
                if *many && filter.is_empty() {
                    destructive(if matches!(self, Op::Delete { .. }) { "deleteMany without a filter" } else { "updateMany without a filter" })
                } else {
                    WriteClass::SafeWrite
                }
            }
            Op::CreateIndex { .. } => destructive("creates an index"),
            Op::DropIndex { .. } => destructive("drops an index"),
            Op::DropCollection { .. } => destructive("drops a collection"),
            Op::CreateCollection { .. } => destructive("creates a collection"),
            Op::RenameCollection { .. } => destructive("renames a collection"),
            Op::DropDatabase => destructive("drops the database"),
            Op::Command { doc, .. } => {
                let name = doc.keys().next().map(|k| k.to_ascii_lowercase()).unwrap_or_default();
                let reads = READ_COMMANDS.contains(&name.as_str())
                    && !(name == "aggregate" && doc.get_array("pipeline").is_ok_and(|p| {
                        p.iter().any(|s| s.as_document().is_some_and(|s| s.contains_key("$out") || s.contains_key("$merge")))
                    }));
                if reads { WriteClass::Read } else { destructive(&format!("runs the {name} command")) }
            }
        }
    }

    /// Writes documents (an insert, update, replace or delete): what the row limit counts.
    pub fn changes_documents(&self) -> bool {
        matches!(self, Op::Insert { .. } | Op::Update { .. } | Op::Replace { .. } | Op::Delete { .. } | Op::FindAndModify { .. } | Op::BulkWrite { .. })
    }
}

// ---------------------------------------------------------------------------------------------
// Splitting

/// The statements of `text`, as byte ranges (trimmed, never empty). A script (a statement that
/// is JavaScript: `var`, a loop, `print`; see [`super::js`]) is one statement, all of it: its
/// statements depend on each other.
pub fn split(text: &str) -> Vec<Range<usize>> {
    let parts = statements(text);
    if parts.len() > 1 && parts.iter().any(|r| super::js::is_script_start(&text[r.clone()])) {
        return vec![parts[0].start..parts[parts.len() - 1].end];
    }
    parts
}

/// The shell's statements of `text` one by one (`;`, line breaks), script or not.
pub(crate) fn statements(text: &str) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut i = 0;
    // The last significant byte, to tell a regex's `/` from anything else.
    let mut last = b'(';
    let push = |s: usize, e: usize, out: &mut Vec<Range<usize>>| {
        let t = &text[s..e];
        let lead = t.len() - t.trim_start().len();
        let r = s + lead..s + t.trim_end().len();
        if !r.is_empty() && !only_comments(&text[r.clone()]) {
            out.push(r);
        }
    };
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' | b'\'' | b'`' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                last = c;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = text[i + 2..].find("*/").map_or(b.len(), |j| i + 2 + j + 2);
                continue;
            }
            b'/' if matches!(last, b'(' | b',' | b':' | b'[' | b'{' | b'=') => {
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
                last = b'/';
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                last = c;
            }
            b')' | b']' | b'}' => {
                depth -= 1;
                last = c;
            }
            b';' if depth <= 0 => {
                push(start, i, &mut out);
                start = i + 1;
                depth = 0;
                last = b'(';
            }
            b'\n' if depth <= 0 => {
                // A chain may go on on the next line: `.sort(…)`.
                let next = text[i + 1..].trim_start();
                if !next.starts_with('.') && !text[start..i].trim().is_empty() && !text[start..i].trim_end().ends_with('.') {
                    push(start, i, &mut out);
                    start = i + 1;
                    depth = 0;
                    last = b'(';
                }
            }
            c if c.is_ascii_whitespace() => {}
            c => last = c,
        }
        i += 1;
    }
    push(start, b.len().max(start), &mut out);
    out
}

fn only_comments(s: &str) -> bool {
    let mut p = Parser::new(s);
    p.skip_ws();
    p.at_end()
}

/// The statement around byte `at` (or the last one before it).
pub fn statement_at(text: &str, at: usize) -> Option<Range<usize>> {
    let all = split(text);
    all.iter().find(|r| r.start <= at && at <= r.end).or_else(|| all.iter().rev().find(|r| r.end <= at)).or(all.first()).cloned()
}

// ---------------------------------------------------------------------------------------------
// Parsing

/// Reads one statement.
pub fn parse(text: &str) -> Result<Parsed, ParseError> {
    let mut p = Parser::new(text);
    p.skip_ws();
    let start = p.pos();
    let head = p.ident().ok_or_else(|| p.error("a statement starts with db., show or use"))?;
    let parsed = match head {
        "show" => {
            let what = p.ident().unwrap_or_default();
            let op = match what {
                "dbs" | "databases" => Op::ShowDbs,
                "collections" | "tables" => Op::ShowCollections,
                _ => return Err(p.error("show dbs or show collections")),
            };
            Parsed { db: None, op }
        }
        "use" => {
            p.skip_ws();
            let rest = p.rest().trim().trim_end_matches(';').trim();
            if rest.is_empty() || rest.contains(char::is_whitespace) {
                return Err(p.error("use takes a database name"));
            }
            let name = rest.trim_matches(|c| c == '"' || c == '\'').to_string();
            return Ok(Parsed { db: None, op: Op::Use(name) });
        }
        "db" => chain(&mut p)?,
        _ => return Err(shell::ParseError { message: format!("`{head}`: a statement starts with db., show or use"), at: start }),
    };
    p.skip_ws();
    let _ = p.eat(';');
    p.skip_ws();
    if !p.at_end() {
        return Err(p.error("unexpected text after the statement"));
    }
    Ok(parsed)
}

/// After `db`: `.coll.method(…)…`, `.method(…)`, `["coll"]`, `.getCollection("…")`,
/// `.getSiblingDB("…")`.
fn chain(p: &mut Parser) -> Result<Parsed, ParseError> {
    let mut db = None;
    let mut coll: Option<String> = None;
    loop {
        if p.eat('[') {
            let Val::Str(name) = p.value()? else { return Err(p.error("db[…] takes a collection name")) };
            p.expect(']')?;
            coll = Some(join(coll, &name));
            continue;
        }
        if !p.eat('.') {
            return Err(p.error("expected .collection.method(…)"));
        }
        let at = p.pos();
        let name = p.ident().ok_or_else(|| p.error("expected a name"))?.to_string();
        p.skip_ws();
        if p.peek() != Some('(') {
            coll = Some(join(coll, &name));
            continue;
        }
        p.eat('(');
        let args = p.list(')')?;
        match (coll.is_none(), name.as_str()) {
            (true, "getCollection") => {
                let Some(Val::Str(c)) = args.first() else { return Err(err_at(at, "getCollection() takes a name")) };
                coll = Some(c.clone());
            }
            (true, "getSiblingDB") => {
                let Some(Val::Str(d)) = args.first() else { return Err(err_at(at, "getSiblingDB() takes a name")) };
                db = Some(d.clone());
            }
            (true, _) => return Ok(Parsed { db, op: db_method(&name, &args, at)? }),
            (false, _) => {
                let mut calls = vec![(name, args, at)];
                while p.eat('.') {
                    let at = p.pos();
                    let m = p.ident().ok_or_else(|| p.error("expected a method"))?.to_string();
                    p.expect('(')?;
                    let a = p.list(')')?;
                    calls.push((m, a, at));
                }
                return Ok(Parsed { db, op: coll_method(coll.unwrap_or_default(), calls)? });
            }
        }
    }
}

/// The operation a script's call names (`js`): a database method (`coll: None`, one call) or a
/// collection's with the cursor's methods after it, from arguments already evaluated.
pub(crate) fn op_of(coll: Option<&str>, calls: Vec<(String, Vec<Val>)>) -> Result<Op, ParseError> {
    match coll {
        None => {
            let mut calls = calls.into_iter();
            let (name, args) = calls.next().ok_or_else(|| err_at(0, "no method"))?;
            if calls.next().is_some() {
                return Err(err_at(0, format!("db.{name}() takes no cursor methods")));
            }
            db_method(&name, &args, 0)
        }
        Some(coll) => {
            if calls.is_empty() {
                return Err(err_at(0, "no method"));
            }
            coll_method(coll.to_string(), calls.into_iter().map(|(n, a)| (n, a, 0)).collect())
        }
    }
}

fn join(coll: Option<String>, name: &str) -> String {
    match coll {
        Some(c) => format!("{c}.{name}"),
        None => name.to_string(),
    }
}

fn err_at(at: usize, message: impl Into<String>) -> ParseError {
    ParseError { message: message.into(), at }
}

fn document(v: Option<&Val>, at: usize, what: &str) -> Result<Document, ParseError> {
    match v {
        None => Ok(Document::new()),
        Some(v) => shell::to_document(v).map_err(|e| err_at(at, format!("{what}: {e}"))),
    }
}

fn opt_document(v: Option<&Val>, at: usize, what: &str) -> Result<Option<Document>, ParseError> {
    match v {
        None | Some(Val::Null) | Some(Val::Undefined) => Ok(None),
        some => document(some, at, what).map(Some),
    }
}

fn int(v: Option<&Val>, at: usize, what: &str) -> Result<i64, ParseError> {
    match v {
        Some(Val::Int(n)) => Ok(*n),
        Some(Val::Float(f)) if f.fract() == 0. => Ok(*f as i64),
        _ => Err(err_at(at, format!("{what} takes a number"))),
    }
}

fn string(v: Option<&Val>, at: usize, what: &str) -> Result<String, ParseError> {
    match v {
        Some(Val::Str(s)) => Ok(s.clone()),
        _ => Err(err_at(at, format!("{what} takes a string"))),
    }
}

fn flag(opts: &Document, key: &str) -> bool {
    opts.get_bool(key).unwrap_or(false)
}

fn db_method(name: &str, args: &[Val], at: usize) -> Result<Op, ParseError> {
    Ok(match name {
        "runCommand" | "adminCommand" => {
            let doc = match args.first() {
                Some(Val::Str(s)) => doc! { s.as_str(): 1 },
                v => document(v, at, name)?,
            };
            if doc.is_empty() {
                return Err(err_at(at, format!("{name}() takes a command document")));
            }
            Op::Command { doc, admin: name == "adminCommand" }
        }
        "getCollectionNames" | "getCollectionInfos" => Op::ShowCollections,
        "createCollection" => Op::CreateCollection { name: string(args.first(), at, name)?, options: document(args.get(1), at, name)? },
        "createView" => {
            let mut options = document(args.get(3), at, name)?;
            options.insert("viewOn", string(args.get(1), at, "createView()'s source")?);
            options.insert("pipeline", pipeline(args.get(2).map(std::slice::from_ref).unwrap_or(&[]), at)?);
            Op::CreateCollection { name: string(args.first(), at, name)?, options }
        }
        "dropDatabase" => Op::DropDatabase,
        "stats" => Op::Stats { coll: None },
        "aggregate" => Op::Aggregate { coll: None, pipeline: pipeline(args, at)? },
        "version" | "serverBuildInfo" => Op::Command { doc: doc! { "buildInfo": 1 }, admin: true },
        "serverStatus" => Op::Command { doc: doc! { "serverStatus": 1 }, admin: true },
        "hostInfo" => Op::Command { doc: doc! { "hostInfo": 1 }, admin: true },
        "currentOp" => Op::Aggregate { coll: None, pipeline: vec![doc! { "$currentOp": {} }] },
        "getUsers" => Op::Command { doc: doc! { "usersInfo": 1 }, admin: false },
        "getRoles" => Op::Command { doc: doc! { "rolesInfo": 1 }, admin: false },
        "ping" => Op::Command { doc: doc! { "ping": 1 }, admin: false },
        other => return Err(err_at(at, format!("db.{other}() is not supported here (try db.runCommand)"))),
    })
}

fn pipeline(args: &[Val], at: usize) -> Result<Vec<Document>, ParseError> {
    let stages: Vec<&Val> = match args.first() {
        Some(Val::Arr(a)) => a.iter().collect(),
        // aggregate({…}, {…}): stages as arguments (the old shell's form).
        Some(Val::Obj(_)) => args.iter().collect(),
        None => Vec::new(),
        Some(_) => return Err(err_at(at, "aggregate() takes an array of stages")),
    };
    stages.into_iter().map(|s| document(Some(s), at, "a pipeline stage")).collect()
}

fn coll_method(coll: String, calls: Vec<(String, Vec<Val>, usize)>) -> Result<Op, ParseError> {
    let mut calls = calls.into_iter();
    let (name, args, at) = calls.next().expect("a call");
    let mut op = match name.as_str() {
        "find" | "findOne" => {
            let mut options = document(args.get(2), at, "find options")?;
            let sort = options.remove("sort").and_then(|s| s.as_document().cloned());
            let skip = options.remove("skip").and_then(|s| s.as_i64().or(s.as_i32().map(i64::from))).unwrap_or(0).max(0) as u64;
            let limit = options.remove("limit").and_then(|s| s.as_i64().or(s.as_i32().map(i64::from)));
            options.remove("projection");
            Op::Find {
                filter: document(args.first(), at, "the filter")?,
                projection: opt_document(args.get(1), at, "the projection")?,
                sort,
                skip,
                limit: if name == "findOne" { Some(1) } else { limit },
                one: name == "findOne",
                coll: coll.clone(),
                options,
            }
        }
        "aggregate" => Op::Aggregate { coll: Some(coll.clone()), pipeline: pipeline(&args, at)? },
        "countDocuments" | "count" => Op::Count { coll: coll.clone(), filter: Some(document(args.first(), at, "the filter")?) },
        "estimatedDocumentCount" => Op::Count { coll: coll.clone(), filter: None },
        "distinct" => Op::Distinct { coll: coll.clone(), field: string(args.first(), at, "distinct()")?, filter: document(args.get(1), at, "the filter")? },
        "insertOne" => Op::Insert { coll: coll.clone(), docs: vec![document(args.first(), at, "insertOne()")?] },
        "insertMany" | "insert" => {
            let docs = match args.first() {
                Some(Val::Arr(a)) => a.iter().map(|d| document(Some(d), at, "a document")).collect::<Result<_, _>>()?,
                Some(v @ Val::Obj(_)) if name == "insert" => vec![document(Some(v), at, "insert()")?],
                _ => return Err(err_at(at, format!("{name}() takes an array of documents"))),
            };
            Op::Insert { coll: coll.clone(), docs }
        }
        "updateOne" | "updateMany" | "update" => {
            let filter = document(args.first(), at, "the filter")?;
            let update = match args.get(1) {
                Some(Val::Arr(stages)) => Bson::Array(stages.iter().map(|s| document(Some(s), at, "a stage").map(Bson::Document)).collect::<Result<_, _>>()?),
                v => Bson::Document(document(v, at, "the update")?),
            };
            if matches!(&update, Bson::Document(d) if d.is_empty()) {
                return Err(err_at(at, format!("{name}() takes an update ({{$set: …}})")));
            }
            let mut options = document(args.get(2), at, "the options")?;
            let (many, upsert) = (name == "updateMany" || flag(&options, "multi"), flag(&options, "upsert"));
            options.remove("multi");
            options.remove("upsert");
            Op::Update { coll: coll.clone(), filter, update, many, upsert, options }
        }
        "replaceOne" => {
            let mut options = document(args.get(2), at, "the options")?;
            let upsert = flag(&options, "upsert");
            options.remove("upsert");
            Op::Replace { coll: coll.clone(), filter: document(args.first(), at, "the filter")?, doc: document(args.get(1), at, "the replacement")?, upsert, options }
        }
        "findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete" | "findAndModify" => {
            let delete = name == "findOneAndDelete";
            let filter = document(args.first(), at, "the filter")?;
            let update = if delete {
                None
            } else {
                Some(match args.get(1) {
                    Some(Val::Arr(stages)) => Bson::Array(stages.iter().map(|s| document(Some(s), at, "a stage").map(Bson::Document)).collect::<Result<_, _>>()?),
                    v => Bson::Document(document(v, at, if name == "findOneAndReplace" { "the replacement" } else { "the update" })?),
                })
            };
            let options = document(args.get(if delete { 1 } else { 2 }), at, "the options")?;
            Op::FindAndModify { coll: coll.clone(), filter, update, options }
        }
        "bulkWrite" => {
            let Some(Val::Arr(ops)) = args.first() else { return Err(err_at(at, "bulkWrite() takes an array of operations")) };
            let ops: Vec<Document> = ops.iter().map(|o| document(Some(o), at, "an operation")).collect::<Result<_, _>>()?;
            for o in &ops {
                let k = o.keys().next().map(String::as_str).unwrap_or("");
                if !["insertOne", "updateOne", "updateMany", "replaceOne", "deleteOne", "deleteMany"].contains(&k) {
                    return Err(err_at(at, format!("bulkWrite: {k:?} is not an operation (insertOne, updateOne, updateMany, replaceOne, deleteOne, deleteMany)")));
                }
            }
            Op::BulkWrite { coll: coll.clone(), ops }
        }
        "deleteOne" | "deleteMany" | "remove" => {
            if args.is_empty() {
                return Err(err_at(at, format!("{name}() takes a filter ({{}} for every document)")));
            }
            let mut options = document(args.get(1), at, "the options")?;
            let many = name != "deleteOne" && !flag(&options, "justOne");
            options.remove("justOne");
            Op::Delete { coll: coll.clone(), filter: document(args.first(), at, "the filter")?, many, options }
        }
        "createIndex" | "ensureIndex" => Op::CreateIndex { coll: coll.clone(), keys: document(args.first(), at, "the keys")?, options: document(args.get(1), at, "the options")? },
        "dropIndex" => {
            let index = match args.first() {
                Some(v) => shell::to_bson(v).map_err(|e| err_at(at, e))?,
                None => return Err(err_at(at, "dropIndex() takes the index's name or keys")),
            };
            Op::DropIndex { coll: coll.clone(), index }
        }
        "dropIndexes" => Op::DropIndex { coll: coll.clone(), index: Bson::String("*".into()) },
        "getIndexes" | "indexes" => Op::Indexes { coll: coll.clone() },
        "drop" => Op::DropCollection { coll: coll.clone() },
        "renameCollection" => Op::RenameCollection { coll: coll.clone(), to: string(args.first(), at, "renameCollection()")? },
        "stats" => Op::Stats { coll: Some(coll.clone()) },
        other => return Err(err_at(at, format!("{other}() is not a collection method here"))),
    };
    // The cursor's methods after find() / aggregate().
    for (m, a, at) in calls {
        match (&mut op, m.as_str()) {
            (_, "toArray" | "pretty" | "batchSize") => {}
            // The cursor's options: into the find command.
            (Op::Find { options, .. }, "hint" | "collation" | "maxTimeMS" | "comment" | "allowDiskUse" | "min" | "max" | "returnKey" | "showRecordId") => {
                let v = match a.first() {
                    Some(v) => shell::to_bson(v).map_err(|e| err_at(at, e))?,
                    None => Bson::Boolean(true),
                };
                options.insert(m.as_str(), v);
            }
            (_, "maxTimeMS" | "comment" | "allowDiskUse") => {}
            (Op::Find { sort, .. }, "sort") => *sort = Some(document(a.first(), at, "sort()")?),
            (Op::Find { limit, .. }, "limit") => *limit = Some(int(a.first(), at, "limit()")?),
            (Op::Find { skip, .. }, "skip") => *skip = int(a.first(), at, "skip()")?.max(0) as u64,
            (Op::Find { projection, .. }, "projection" | "project") => *projection = Some(document(a.first(), at, "projection()")?),
            (Op::Find { coll, filter, .. }, "count" | "itcount" | "size") => {
                op = Op::Count { coll: coll.clone(), filter: Some(filter.clone()) };
            }
            (Op::Find { coll, filter, projection, sort, skip, limit, .. }, "explain") => {
                let mut find = doc! { "find": coll.clone(), "filter": filter.clone() };
                if let Some(p) = projection {
                    find.insert("projection", p.clone());
                }
                if let Some(s) = sort {
                    find.insert("sort", s.clone());
                }
                if *skip > 0 {
                    find.insert("skip", *skip as i64);
                }
                if let Some(l) = limit {
                    find.insert("limit", *l);
                }
                let verbosity = match a.first() {
                    Some(Val::Str(v)) => v.clone(),
                    Some(Val::Bool(true)) => "allPlansExecution".into(),
                    _ => "queryPlanner".into(),
                };
                op = Op::Command { doc: doc! { "explain": find, "verbosity": verbosity }, admin: false };
            }
            (Op::Aggregate { coll: Some(coll), pipeline }, "explain") => {
                op = Op::Command { doc: doc! { "explain": { "aggregate": coll.clone(), "pipeline": pipeline.clone(), "cursor": {} }, "verbosity": "queryPlanner" }, admin: false };
            }
            (_, other) => return Err(err_at(at, format!(".{other}() does not go here"))),
        }
    }
    Ok(op)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(s: &str) -> Op {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e:?}")).op
    }

    #[test]
    fn reads_the_shells_statements() {
        assert_eq!(
            op("db.users.find({age: {$gt: 30}}, {name: 1}).sort({name: -1}).skip(5).limit(20)"),
            Op::Find {
                coll: "users".into(),
                filter: doc! { "age": { "$gt": 30 } },
                projection: Some(doc! { "name": 1 }),
                sort: Some(doc! { "name": -1 }),
                skip: 5,
                limit: Some(20),
                one: false,
                options: doc! {},
            }
        );
        assert!(matches!(op("db.users.findOne()"), Op::Find { limit: Some(1), one: true, .. }));
        assert_eq!(op("db.system.users.find()").collection(), Some("system.users"));
        assert_eq!(op("db['odd name'].find()").collection(), Some("odd name"));
        assert_eq!(op("db.getCollection('a.b').countDocuments({})"), Op::Count { coll: "a.b".into(), filter: Some(doc! {}) });
        assert_eq!(op("db.c.find({x: 1}).count()"), Op::Count { coll: "c".into(), filter: Some(doc! { "x": 1 }) });
        let p = parse("db.getSiblingDB('admin').runCommand({ping: 1})").unwrap();
        assert_eq!(p.db.as_deref(), Some("admin"));
        assert_eq!(p.op, Op::Command { doc: doc! { "ping": 1 }, admin: false });
        assert_eq!(op("show dbs"), Op::ShowDbs);
        assert_eq!(op("use shop;"), Op::Use("shop".into()));
        assert!(matches!(op("db.o.aggregate([{$match: {a: 1}}, {$count: 'n'}])"), Op::Aggregate { pipeline, .. } if pipeline.len() == 2));
        assert!(matches!(op("db.o.updateMany({}, [{$set: {a: 1}}], {upsert: true})"), Op::Update { many: true, upsert: true, update: Bson::Array(_), .. }));
        assert!(matches!(op("db.c.find().explain('executionStats')"), Op::Command { doc, .. } if doc.get_str("verbosity") == Ok("executionStats")));
        // Errors say where.
        let e = parse("db.users.find({a: 1}).frobnicate()").unwrap_err();
        assert_eq!(e.at, 22, "{e:?}");
        assert!(parse("db.users.deleteMany()").unwrap_err().message.contains("filter"));
        assert!(parse("select 1").is_err());
    }

    #[test]
    fn what_each_statement_does() {
        let class = |s: &str| op(s).class();
        assert_eq!(class("db.a.find()"), WriteClass::Read);
        assert_eq!(class("db.runCommand({dbStats: 1})"), WriteClass::Read);
        assert_eq!(class("db.a.insertOne({x: 1})"), WriteClass::SafeWrite);
        assert_eq!(class("db.a.deleteMany({x: 1})"), WriteClass::SafeWrite);
        assert_eq!(class("db.a.deleteOne({})"), WriteClass::SafeWrite);
        assert!(class("db.a.deleteMany({})").is_destructive());
        assert!(class("db.a.updateMany({}, {$set: {a: 1}})").is_destructive());
        assert!(class("db.a.drop()").is_destructive());
        assert!(class("db.a.aggregate([{$out: 'b'}])").is_destructive());
        assert!(class("db.runCommand({drop: 'a'})").is_destructive());
        assert!(class("db.runCommand({aggregate: 'a', pipeline: [{$merge: 'b'}], cursor: {}})").is_destructive());
    }

    #[test]
    fn statements_split_at_semicolons_and_lines() {
        let text = "db.a.find({\n  x: 1\n})\n  .sort({x: 1})\n\n// a note\ndb.b.find(); db.c.find({r: /;\\//})\nshow dbs";
        let parts: Vec<&str> = split(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(parts, ["db.a.find({\n  x: 1\n})\n  .sort({x: 1})", "db.b.find()", "db.c.find({r: /;\\//})", "show dbs"]);
        assert_eq!(statement_at(text, 3).map(|r| &text[r]), Some(parts[0]));
        assert_eq!(statement_at(text, text.len()).map(|r| &text[r]), Some("show dbs"));
        assert!(split("  // only a note\n").is_empty());
    }
}
