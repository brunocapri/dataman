//! The engines without SQL: MongoDB, Elasticsearch, OpenSearch and Redis.
//!
//! Their connections are [`Connection`]s like the others (a profile, server info, the SSH
//! tunnel), holding a [`Client`] instead of SQL sessions. Tabs on them show documents and run the
//! engine's own language ([`mql`] for MongoDB); the explorer lists what [`Connection::nosql_tree`]
//! returns; documents are edited through [`Connection::save_document`] and its siblings. The
//! drivers are in [`crate::backend`].
//!
//! Every value is a [`Body`]: BSON for MongoDB (its types survive an edit), JSON for the search
//! engines. Statements are classified ([`crate::guard::WriteClass`]) before they run, as SQL is:
//! a read-only connection refuses writes, production asks first, the MCP tiers apply.

pub mod console;
pub(crate) mod js;
pub mod lang;
pub mod mql;
pub mod redis;
pub mod shell;

/// BSON's types, for what reads MongoDB's documents.
pub use mongodb::bson;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use mongodb::bson::{Bson, Document};

use crate::backend::{mongo, redis as rds, search};
use crate::config::{ConnectionProfile, Engine};
use crate::conn::Connection;
use crate::guard::WriteClass;
use crate::server::ServerInfo;
use crate::ssh::Tunnel;
use crate::{Task, spawn};

/// A connection's client, one variant per driver.
#[derive(Clone)]
pub(crate) enum Client {
    Mongo(Arc<mongo::Client>),
    /// Elasticsearch and OpenSearch.
    Search(Arc<search::Client>),
    Redis(Arc<rds::Client>),
}

impl Client {
    pub(crate) fn tls_active(&self) -> bool {
        match self {
            Client::Mongo(c) => c.tls_active,
            Client::Search(c) => c.tls_active,
            Client::Redis(c) => c.tls_active,
        }
    }
}

/// Connects (through the tunnel, if any) and reads what the server is.
pub(crate) async fn connect(p: &ConnectionProfile, password: Option<&str>, tunnel: Option<&Tunnel>) -> Result<(Client, ServerInfo)> {
    match p.engine {
        Engine::MongoDb => {
            let c = mongo::connect(p, password, tunnel).await?;
            let info = ServerInfo {
                version: mongo::version(&c).await.unwrap_or_else(|_| "MongoDB".into()),
                database: c.default_db.clone(),
                user: p.user.clone(),
                ..Default::default()
            };
            Ok((Client::Mongo(Arc::new(c)), info))
        }
        Engine::Elasticsearch | Engine::OpenSearch => {
            let c = search::connect(p, password, tunnel).await?;
            let info = ServerInfo { version: c.version.clone(), user: p.user.clone(), ..Default::default() };
            Ok((Client::Search(Arc::new(c)), info))
        }
        Engine::Redis => {
            let c = rds::connect(p, password, tunnel).await?;
            let info = ServerInfo { version: c.version.clone(), database: c.default_db.to_string(), user: p.user.clone(), ..Default::default() };
            Ok((Client::Redis(Arc::new(c)), info))
        }
        e => Err(anyhow!("{} has no documents", e.label())),
    }
}

// ---------------------------------------------------------------------------------------------
// Values

/// A document or a value of one: BSON (MongoDB) or JSON (the search engines).
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Bson(Bson),
    Json(serde_json::Value),
}

impl Body {
    pub fn doc(d: Document) -> Body {
        Body::Bson(Bson::Document(d))
    }

    /// The editable text: the shell's form for BSON ([`shell::print_bson`]), indented JSON.
    pub fn text(&self) -> String {
        match self {
            Body::Bson(b) => shell::print_bson(b),
            // A reply that is text (INFO, `_cat`): as it is, not as a JSON string.
            Body::Json(serde_json::Value::String(s)) => s.clone(),
            Body::Json(j) => serde_json::to_string_pretty(j).unwrap_or_default(),
        }
    }

    /// One line (a scalar as the shell writes it; a container summarized).
    pub fn inline(&self) -> String {
        match self {
            Body::Bson(b) => shell::scalar_text(b),
            Body::Json(serde_json::Value::Object(m)) => format!("{{{}}}", m.len()),
            Body::Json(serde_json::Value::Array(a)) => format!("[{}]", a.len()),
            Body::Json(j) => j.to_string(),
        }
    }

    /// As JSON (relaxed extended JSON for BSON: `{"$oid": "…"}`), for agents and exports.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Body::Bson(b) => b.clone().into_relaxed_extjson(),
            Body::Json(j) => j.clone(),
        }
    }

    /// The tree the document view draws.
    pub fn node(&self) -> Node {
        match self {
            Body::Bson(b) => Node::of_bson(String::new(), b),
            Body::Json(j) => Node::of_json(String::new(), j),
        }
    }
}

/// What a value is, for its color in the document view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
    /// An ObjectId, a UUID: an identity.
    Id,
    Date,
    /// Everything else (binary, regex, timestamps, keys).
    Other,
}

/// A field of a document as the document view shows it: its key, its value in one line (a
/// container's count) and its own fields.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub key: String,
    pub kind: NodeKind,
    pub text: String,
    /// The value's type name (`objectId`, `long`): shown beside it where types matter.
    pub type_name: &'static str,
    pub children: Vec<Node>,
}

impl Node {
    pub fn of_bson(key: String, b: &Bson) -> Node {
        let kind = match b {
            Bson::Document(_) => NodeKind::Object,
            Bson::Array(_) => NodeKind::Array,
            Bson::String(_) | Bson::Symbol(_) => NodeKind::String,
            Bson::Double(_) | Bson::Int32(_) | Bson::Int64(_) | Bson::Decimal128(_) => NodeKind::Number,
            Bson::Boolean(_) => NodeKind::Bool,
            Bson::Null | Bson::Undefined => NodeKind::Null,
            Bson::ObjectId(_) => NodeKind::Id,
            Bson::Binary(bin) if bin.subtype == mongodb::bson::spec::BinarySubtype::Uuid => NodeKind::Id,
            Bson::DateTime(_) => NodeKind::Date,
            _ => NodeKind::Other,
        };
        let children = match b {
            Bson::Document(d) => d.iter().map(|(k, v)| Node::of_bson(k.clone(), v)).collect(),
            Bson::Array(a) => a.iter().enumerate().map(|(i, v)| Node::of_bson(i.to_string(), v)).collect(),
            _ => Vec::new(),
        };
        Node { key, kind, text: shell::scalar_text(b), type_name: shell::type_name(b), children }
    }

    pub fn of_json(key: String, j: &serde_json::Value) -> Node {
        use serde_json::Value as J;
        let (kind, type_name, text, children) = match j {
            J::Object(m) => (NodeKind::Object, "object", format!("{{{}}}", m.len()), m.iter().map(|(k, v)| Node::of_json(k.clone(), v)).collect()),
            J::Array(a) => (NodeKind::Array, "array", format!("[{}]", a.len()), a.iter().enumerate().map(|(i, v)| Node::of_json(i.to_string(), v)).collect()),
            J::String(s) => (NodeKind::String, "string", shell::quote(s), Vec::new()),
            J::Number(n) => (NodeKind::Number, "number", n.to_string(), Vec::new()),
            J::Bool(b) => (NodeKind::Bool, "boolean", b.to_string(), Vec::new()),
            J::Null => (NodeKind::Null, "null", "null".into(), Vec::new()),
        };
        Node { key, kind, text, type_name, children }
    }

    /// Fields and items below it, all levels.
    pub fn count(&self) -> usize {
        self.children.iter().map(|c| 1 + c.count()).sum()
    }
}

// ---------------------------------------------------------------------------------------------
// Results

/// Where a page of documents lives, so each can be saved back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Mongo { db: String, coll: String },
    /// An index (empty: the documents' own, each saved to its `_index`).
    Search { index: String },
    /// A Redis key's value, its entries (a hash's fields, a list's items…): `kind` is its type.
    Redis { db: String, key: String, kind: String },
}

/// One document of a result.
#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    /// Its identity (`_id`): saving replaces the document that has it.
    pub id: Option<Body>,
    /// The index it lives in (a search's hits: an index pattern finds several).
    pub index: Option<String>,
    pub body: Body,
}

/// A page of documents.
#[derive(Clone, Debug, PartialEq)]
pub struct Docs {
    pub docs: Vec<Doc>,
    /// Where they can be saved (none for computed ones: an aggregation, a projection).
    pub source: Option<Source>,
    /// How many there are in all, when known.
    pub total: Option<u64>,
    /// The page's first document, counted from the query's own first.
    pub skip: u64,
    /// More follow this page.
    pub more: bool,
}

/// What a statement returned.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Docs(Docs),
    /// A command's answer, a count, a list of names.
    Value(Body),
    /// A write: how many documents it changed, in words, and the server's answer.
    Written { affected: u64, summary: String, detail: Option<Body> },
    /// `use name`: the tab moves to that database.
    Use(String),
}

/// A statement that ran: its output, and how long it took.
#[derive(Clone, Debug, PartialEq)]
pub struct Ran {
    pub output: Output,
    pub elapsed: Duration,
    /// What it was, as classified before it ran.
    pub class: WriteClass,
    /// A MongoDB script's (`js`): the lines it printed (`print`, `printjson`)…
    pub printed: Vec<String>,
    /// …what each of its writes changed, in order…
    pub writes: Vec<String>,
    /// …the database `db` ended on, when it moved there (`use`, `db = db.getSiblingDB(…)`)…
    pub moved: Option<String>,
    /// …and its last value's cursor, for its other pages ([`Connection::rerun_nosql`]): the
    /// script itself does not run again.
    pub rerun: Option<Rerun>,
}

impl Ran {
    fn of(output: Output, elapsed: Duration, class: WriteClass) -> Ran {
        Ran { output, elapsed, class, printed: Vec::new(), writes: Vec::new(), moved: None, rerun: None }
    }
}

/// A script's final cursor: its query, read again for a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Rerun {
    db: String,
    op: mql::Op,
}

/// Why a statement did not run or failed: the message, and where in its text (parse errors).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    pub message: String,
    pub at: Option<usize>,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failed {}

impl From<shell::ParseError> for Failed {
    fn from(e: shell::ParseError) -> Failed {
        Failed { message: e.message, at: Some(e.at) }
    }
}

impl From<anyhow::Error> for Failed {
    fn from(e: anyhow::Error) -> Failed {
        Failed { message: crate::backend::mongo::message(&e), at: None }
    }
}

/// The documents a page asks for: from `skip` (past the query's own skip), at most `limit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub skip: u64,
    pub limit: usize,
}

impl Default for Page {
    fn default() -> Self {
        Page { skip: 0, limit: PAGE }
    }
}

/// The documents one page shows.
pub const PAGE: usize = 50;

/// A statement on its way; drop it or [`Running::cancel`] to stop it. A failure is a
/// [`Failed`] (`downcast_ref`), with where in the text when it could not be read.
pub struct Running {
    task: Task<Ran>,
    abort: tokio::task::AbortHandle,
}

impl std::future::Future for Running {
    type Output = Result<Ran>;
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<Ran>> {
        std::pin::Pin::new(&mut self.task).poll(cx)
    }
}

impl Running {
    pub fn cancel(&self) {
        self.abort.abort();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

fn tree_key(parent: Option<&str>) -> (&'static str, String, String) {
    ("nosql-tree", parent.unwrap_or("").to_string(), if parent.is_some() { "db".into() } else { String::new() })
}

fn running<F>(rt: &tokio::runtime::Handle, fut: F) -> Running
where
    F: std::future::Future<Output = Result<Ran, Failed>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let h = rt.spawn(async move {
        let _ = tx.send(fut.await.map_err(anyhow::Error::from));
    });
    Running { task: Task(rx), abort: h.abort_handle() }
}

/// What a statement is, read without running it: `Err` when it cannot be read.
pub fn classify(engine: Engine, text: &str) -> Result<WriteClass, Failed> {
    match engine {
        Engine::MongoDb => match mongo_text(text)? {
            MongoText::Statement(p) => Ok(p.op.class()),
            MongoText::Script => js::class(text),
        },
        Engine::Elasticsearch | Engine::OpenSearch => Ok(console::parse(text)?.class()),
        Engine::Redis => {
            let args = redis::parse(text)?;
            if let Some(why) = redis::refusal(&args) {
                return Err(Failed { message: why, at: None });
            }
            Ok(redis::class(&args))
        }
        e => Err(Failed { message: format!("{} has no statements here", e.label()), at: None }),
    }
}

/// A MongoDB text: one statement of the shell, or a script ([`js`]).
enum MongoText {
    Statement(mql::Parsed),
    Script,
}

/// What the shell does with `text`: a statement it reads; else JavaScript when it is one (several
/// statements, a declaration, a loop, a call chain the statement form has not: `forEach`), the
/// statement's own error (where) otherwise.
fn mongo_text(text: &str) -> Result<MongoText, Failed> {
    let e = match mql::parse(text) {
        Ok(p) => return Ok(MongoText::Statement(p)),
        Err(e) => e,
    };
    let parts = mql::statements(text);
    if parts.len() > 1 || parts.first().is_some_and(|r| js::is_script_start(&text[r.clone()])) {
        return Ok(MongoText::Script);
    }
    let first = shell::Parser::new(text.trim_start()).ident().unwrap_or("").to_string();
    if first == "db" && js::check(text).is_ok() {
        return Ok(MongoText::Script);
    }
    Err(e.into())
}

/// A script on its own thread (Boa's runtime is not `Send`), its calls to the server through
/// the connection's runtime; stopped at its next call when the run is dropped or cancelled.
async fn run_script(conn: Arc<Connection>, client: Arc<mongo::Client>, db: String, text: String, class: WriteClass, page: Page) -> Result<js::Script, Failed> {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Server {
        rt: tokio::runtime::Handle,
        client: Arc<mongo::Client>,
        conn: Arc<Connection>,
        stop: Arc<AtomicBool>,
    }
    impl Server {
        fn block<T>(&self, f: impl std::future::Future<Output = Result<T>>) -> Result<T, String> {
            let stop = self.stop.clone();
            self.rt.block_on(async move {
                tokio::select! {
                    r = f => r.map_err(|e| mongo::message(&e)),
                    _ = async { while !stop.load(Ordering::Relaxed) { tokio::time::sleep(Duration::from_millis(50)).await } } => Err("the script was stopped".into()),
                }
            })
        }
    }
    impl js::Server for Server {
        fn run(&mut self, db: &str, op: &mql::Op, page: Page) -> Result<Output, String> {
            self.block(mongo::run(&self.client, db, op, page))
        }
        fn command(&mut self, db: &str, cmd: Document, admin: bool) -> Result<Document, String> {
            self.block(mongo::command(&self.client, db, cmd, admin))
        }
        fn writable(&self, db: &str) -> bool {
            !self.conn.read_only_for(db)
        }
        fn cancelled(&self) -> bool {
            self.stop.load(Ordering::Relaxed)
        }
        fn sleep(&mut self, ms: u64) {
            let until = Instant::now() + Duration::from_millis(ms);
            while !self.cancelled() && Instant::now() < until {
                std::thread::sleep(until.saturating_duration_since(Instant::now()).min(Duration::from_millis(50)));
            }
        }
    }
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let _stop = StopOnDrop(stop.clone());
    let server = Server { rt: conn.handle().clone(), client, conn, stop };
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("dataman-js".into())
        .stack_size(64 << 20)
        .spawn(move || {
            let _ = tx.send(js::run(&text, &db, &class, Box::new(server), page));
        })
        .map_err(|e| Failed { message: format!("the script did not start: {e}"), at: None })?;
    rx.await.unwrap_or_else(|_| Err(Failed { message: "the script stopped".into(), at: None }))
}

/// Whether `text` runs as a MongoDB script (JavaScript, [`js`]) rather than one statement: what
/// it writes is known only as it runs ([`Connection::nosql_affects`] cannot count it first).
pub fn is_script(engine: Engine, text: &str) -> bool {
    engine == Engine::MongoDb && matches!(mongo_text(text), Ok(MongoText::Script))
}

/// The statements of an editor's text (byte ranges), in the engine's language.
pub fn statements(engine: Engine, text: &str) -> Vec<std::ops::Range<usize>> {
    match engine {
        Engine::MongoDb => mql::split(text),
        Engine::Elasticsearch | Engine::OpenSearch => console::split(text),
        Engine::Redis => redis::split(text),
        _ => {
            let t = text.trim();
            let r = text.len() - text.trim_start().len()..text.trim_end().len();
            if t.is_empty() { Vec::new() } else { std::iter::once(r).collect() }
        }
    }
}

/// The statement a tab runs first: a collection's documents (or a key's value).
pub fn open_statement(engine: Engine, collection: &str) -> String {
    match engine {
        Engine::MongoDb => {
            if collection.chars().all(shell::is_ident_char) && collection.chars().next().is_some_and(shell::is_ident_start) {
                format!("db.{collection}.find({{}})")
            } else {
                format!("db.getCollection({}).find({{}})", shell::quote(collection))
            }
        }
        Engine::Elasticsearch | Engine::OpenSearch => format!("GET /{collection}/_search\n{{\n  \"query\": {{ \"match_all\": {{}} }}\n}}"),
        Engine::Redis => redis::view_command(collection, ""),
        _ => String::new(),
    }
}

/// The statement a tab opens a tree row with: [`open_statement`], a Redis key's by its type
/// (as the explorer last read it).
pub fn open_statement_for(conn: &Connection, db: &str, name: &str) -> String {
    if conn.engine() == Engine::Redis {
        let kind = conn.cached_tree(Some(db)).and_then(|v| v.iter().find(|i| i.name == name).and_then(|i| i.note.clone())).unwrap_or_default();
        return redis::view_command(name, &kind);
    }
    open_statement(conn.engine(), name)
}

/// A new document's text in the document view: an entry of a Redis value's type, else an
/// empty object.
pub fn new_template(source: &Source) -> String {
    match source {
        Source::Redis { kind, .. } => match kind.as_str() {
            "hash" => "{\n  \"field\": \"\",\n  \"value\": \"\"\n}",
            "list" => "{\n  \"value\": \"\"\n}",
            "set" => "{\n  \"member\": \"\"\n}",
            "zset" => "{\n  \"member\": \"\",\n  \"score\": 0\n}",
            _ => "{\n  \n}",
        }
        .to_string(),
        _ => "{\n  \n}".to_string(),
    }
}

/// A collection's (a key's) count statement, by what the explorer last read (a Redis key's
/// type).
pub fn count_statement_for(conn: &Connection, db: &str, name: &str) -> String {
    if conn.engine() == Engine::Redis {
        let kind = conn.cached_tree(Some(db)).and_then(|v| v.iter().find(|i| i.name == name).and_then(|i| i.note.clone())).unwrap_or_default();
        return redis::len_command(name, &kind);
    }
    count_statement(conn.engine(), name)
}

/// The statement that counts a collection's documents.
pub fn count_statement(engine: Engine, collection: &str) -> String {
    match engine {
        Engine::MongoDb => open_statement(engine, collection).replace(".find({})", ".countDocuments({})"),
        Engine::Elasticsearch | Engine::OpenSearch => format!("GET /{collection}/_count"),
        Engine::Redis => format!("TYPE {}", redis::quote(collection)),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// The explorer

/// One entry of a document connection's tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub kind: ItemKind,
    /// Documents (keys), when the server says cheaply.
    pub count: Option<u64>,
    /// Bytes on disk, when the server says.
    pub size: Option<u64>,
    /// What else the row says: a Redis key's type.
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// Holds collections (a MongoDB database).
    Database,
    Collection,
    View,
}

// ---------------------------------------------------------------------------------------------
// On a connection

impl Connection {
    pub(crate) fn client(&self) -> Result<&Client, Failed> {
        self.nosql.as_ref().ok_or_else(|| Failed { message: format!("{} has no documents", self.engine().label()), at: None })
    }

    /// Runs `text` (one statement of the engine's language) on `db`, the page `page` of its
    /// documents. Refused before it reaches the server when it writes on a read-only database.
    pub fn run_nosql(self: &Arc<Self>, db: &str, text: &str, page: Page) -> Running {
        let (this, db, text) = (self.clone(), db.to_string(), text.to_string());
        running(self.handle(), async move {
            let started = Instant::now();
            let client = this.client()?.clone();
            match client {
                Client::Mongo(c) => {
                    let parsed = match mongo_text(&text)? {
                        MongoText::Statement(p) => p,
                        MongoText::Script => {
                            let class = js::class(&text)?;
                            if class.is_write() && this.read_only_for(&db) {
                                return Err(Failed { message: format!("{db} is read-only: writes are refused"), at: None });
                            }
                            let s = run_script(this.clone(), c, db.clone(), text, class.clone(), page).await?;
                            let mut ran = Ran::of(s.output, started.elapsed(), class);
                            ran.printed = s.printed;
                            ran.writes = s.writes;
                            ran.moved = (s.db != db).then_some(s.db);
                            ran.rerun = s.rerun.map(|(db, op)| Rerun { db, op });
                            return Ok(ran);
                        }
                    };
                    let db = parsed.db.clone().unwrap_or(db);
                    let class = parsed.op.class();
                    if class.is_write() && this.read_only_for(&db) {
                        return Err(Failed { message: format!("{db} is read-only: writes are refused"), at: None });
                    }
                    let output = mongo::run(&c, &db, &parsed.op, page).await?;
                    Ok(Ran::of(output, started.elapsed(), class))
                }
                Client::Search(c) => {
                    let req = console::parse(&text)?;
                    let class = req.class();
                    if class.is_write() && this.read_only_for(&db) {
                        return Err(Failed { message: "this connection is read-only: writes are refused".into(), at: None });
                    }
                    let output = search::run(&c, &req, page).await?;
                    Ok(Ran::of(output, started.elapsed(), class))
                }
                Client::Redis(c) => {
                    let args = redis::parse(&text)?;
                    let raw = redis::parse_bytes(&text)?;
                    let class = redis::class(&args);
                    if class.is_write() && this.read_only_for(&db) {
                        return Err(Failed { message: format!("database {db} is read-only: writes are refused"), at: None });
                    }
                    let output = rds::run(&c, &db, &args, &raw, page).await?;
                    Ok(Ran::of(output, started.elapsed(), class))
                }
            }
        })
    }

    /// Another page of a script's final cursor ([`Ran::rerun`]): its query alone, read again.
    pub fn rerun_nosql(self: &Arc<Self>, r: &Rerun, page: Page) -> Running {
        let (this, r) = (self.clone(), r.clone());
        running(self.handle(), async move {
            let started = Instant::now();
            let Client::Mongo(c) = this.client()?.clone() else { return Err(Failed { message: "not a MongoDB connection".into(), at: None }) };
            let output = mongo::run(&c, &r.db, &r.op, page).await?;
            let mut ran = Ran::of(output, started.elapsed(), r.op.class());
            ran.rerun = Some(r);
            Ok(ran)
        })
    }

    /// The tree under `parent` (`None`: the connection's top level), read from the server
    /// (and kept: [`Connection::cached_tree`]).
    pub fn nosql_tree(self: &Arc<Self>, parent: Option<&str>) -> Task<Vec<Item>> {
        let (this, parent) = (self.clone(), parent.map(str::to_string));
        spawn(self.handle(), async move {
            let items = match this.client()?.clone() {
                Client::Mongo(c) => match &parent {
                    None => mongo::databases(&c).await?,
                    Some(db) => mongo::collections(&c, db).await?,
                },
                // No databases: the indices at the top, and for any database asked about.
                Client::Search(c) => search::indices(&c).await?,
                Client::Redis(c) => match &parent {
                    None => rds::databases(&c).await?,
                    Some(db) => rds::keys(&c, db).await?,
                },
            };
            this.caches.lock().insert(tree_key(parent.as_deref()), Arc::new(items.clone()));
            Ok(items)
        })
    }

    /// The tree under `parent` as last read ([`Connection::nosql_tree`]).
    pub fn cached_tree(&self, parent: Option<&str>) -> Option<Arc<Vec<Item>>> {
        self.caches.lock().get(&tree_key(parent)).cloned().and_then(|a| a.downcast::<Vec<Item>>().ok())
    }

    /// Forgets the tree read so far (the explorer's reload).
    pub fn forget_tree(&self) {
        self.caches.lock().retain(|k, _| k.0 != "nosql-tree");
    }

    /// Saves `text` (the document as edited) over the document `id` of `source`; `id: None`
    /// inserts it. Returns the saved document's id.
    pub fn save_document(self: &Arc<Self>, source: &Source, id: Option<Body>, text: &str) -> Task<Body> {
        let (this, source, text) = (self.clone(), source.clone(), text.to_string());
        spawn(self.handle(), async move {
            let db = match &source {
                Source::Mongo { db, .. } | Source::Redis { db, .. } => db.clone(),
                Source::Search { .. } => this.database().to_string(),
            };
            this.refuse_read_only(&db)?;
            let val = shell::parse(&text).map_err(Failed::from)?;
            match (this.client()?.clone(), &source) {
                (Client::Mongo(c), Source::Mongo { db, coll }) => {
                    let doc = shell::to_document(&val).map_err(|m| Failed { message: m, at: None })?;
                    let id = match id {
                        Some(Body::Bson(id)) => Some(id),
                        Some(Body::Json(_)) => return Err(anyhow!("not a MongoDB document")),
                        None => None,
                    };
                    Ok(Body::Bson(mongo::save(&c, db, coll, id, doc).await?))
                }
                (Client::Search(c), Source::Search { index }) => {
                    if index.is_empty() {
                        return Err(anyhow!("which index? search one index (not a pattern) to add documents"));
                    }
                    let doc = shell::to_json(&val).map_err(|m| Failed { message: m, at: None })?;
                    let id = match &id {
                        Some(Body::Json(serde_json::Value::String(s))) => Some(s.as_str()),
                        _ => None,
                    };
                    Ok(Body::Json(serde_json::Value::String(search::save(&c, index, id, doc).await?)))
                }
                (Client::Redis(c), Source::Redis { db, key, kind }) => {
                    let entry = shell::to_json(&val).map_err(|m| Failed { message: m, at: None })?;
                    let id = match id {
                        Some(Body::Json(v)) => Some(v),
                        _ => None,
                    };
                    Ok(Body::Json(rds::save(&c, db, key, kind, id, entry).await?))
                }
                _ => Err(anyhow!("these documents cannot be saved here")),
            }
        })
    }

    /// Deletes the document `id` of `source`.
    pub fn delete_document(self: &Arc<Self>, source: &Source, id: Body) -> Task<u64> {
        let (this, source) = (self.clone(), source.clone());
        spawn(self.handle(), async move {
            match (this.client()?.clone(), &source, id) {
                (Client::Mongo(c), Source::Mongo { db, coll }, Body::Bson(id)) => {
                    this.refuse_read_only(db)?;
                    mongo::delete(&c, db, coll, id).await
                }
                (Client::Search(c), Source::Search { index }, Body::Json(serde_json::Value::String(id))) => {
                    this.refuse_read_only(this.database())?;
                    search::delete(&c, index, &id).await
                }
                (Client::Redis(c), Source::Redis { db, key, kind }, Body::Json(id)) => {
                    this.refuse_read_only(db)?;
                    rds::delete(&c, db, key, kind, id).await
                }
                _ => Err(anyhow!("this document cannot be deleted here")),
            }
        })
    }

    /// The field paths of a collection (from a sample of its documents) and their types: the
    /// editor's completions, an agent's `describe_table`.
    pub fn sample_fields(self: &Arc<Self>, db: &str, collection: &str) -> Task<Vec<(String, Vec<&'static str>)>> {
        let (this, db, coll) = (self.clone(), db.to_string(), collection.to_string());
        spawn(self.handle(), async move {
            match this.client()?.clone() {
                Client::Mongo(c) => mongo::sample_fields(&c, &db, &coll, 100).await,
                Client::Search(c) => search::fields(&c, &coll).await,
                Client::Redis(_) => Ok(Vec::new()),
            }
        })
    }

    /// How many documents a write would change, counted before it runs (its filter's matches;
    /// an insert's documents); `None` for what changes no documents (a read, a drop).
    pub fn nosql_affects(self: &Arc<Self>, db: &str, text: &str) -> Task<Option<u64>> {
        let (this, db, text) = (self.clone(), db.to_string(), text.to_string());
        spawn(self.handle(), async move {
            match this.client()?.clone() {
                Client::Mongo(c) => match mongo_text(&text)? {
                    MongoText::Statement(parsed) => {
                        let db = parsed.db.clone().unwrap_or(db);
                        mongo::affects(&c, &db, &parsed.op).await
                    }
                    // A script's writes are known only as it runs.
                    MongoText::Script => Ok(None),
                },
                Client::Search(c) => search::affects(&c, &console::parse(&text).map_err(Failed::from)?).await,
                Client::Redis(_) => Ok(rds::affects(&redis::parse(&text).map_err(Failed::from)?)),
            }
        })
    }

    fn refuse_read_only(&self, db: &str) -> Result<()> {
        if self.read_only_for(db) {
            return Err(anyhow!("{db} is read-only: writes are refused"));
        }
        Ok(())
    }
}
