mod common;

use common::*;
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::nosql::{bson, Body, Failed, ItemKind, Output, Page, Ran, Source};

fn mongo_profile(database: &str) -> ConnectionProfile {
    let v = std::env::var("DATAMAN_MONGO").unwrap_or_else(|_| "127.0.0.1:27027".into());
    let (host, port) = v.rsplit_once(':').unwrap();
    ConnectionProfile {
        host: host.into(),
        port: port.parse().unwrap(),
        user: "root".into(),
        database: database.into(),
        sslmode: SslMode::Disable,
        engine: Engine::MongoDb,
        ..ConnectionProfile::new("mongo")
    }
}

fn password() -> String {
    std::env::var("DATAMAN_MONGO_PASSWORD").unwrap_or_else(|_| "dataman".into())
}

fn mongo(e: &Env, p: ConnectionProfile) -> Option<Arc<Connection>> {
    match e.core.block_on(e.core.connect(p.clone(), Some(password()))) {
        Ok(c) => Some(c),
        Err(err) => {
            eprintln!("skipping: cannot reach {} ({err:#})", p.summary());
            None
        }
    }
}

fn run(e: &Env, c: &Arc<Connection>, db: &str, text: &str, page: Page) -> Result<Ran, Failed> {
    let r = c.run_nosql(db, text, page);
    match e.core.block_on(async { tokio::time::timeout(Duration::from_secs(30), r).await.expect("timeout") }) {
        Ok(ran) => Ok(ran),
        Err(err) => Err(err.downcast_ref::<Failed>().cloned().unwrap_or_else(|| Failed { message: format!("{err:#}"), at: None })),
    }
}

fn ok(e: &Env, c: &Arc<Connection>, db: &str, text: &str) -> Output {
    run(e, c, db, text, Page::default()).unwrap_or_else(|f| panic!("{text}: {f:?}")).output
}

struct Scratch<'a> {
    e: &'a Env,
    c: Arc<Connection>,
    db: String,
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        let _ = run(self.e, &self.c, &self.db, "db.dropDatabase()", Page::default());
    }
}

fn scratch<'a>(e: &'a Env, name: &str) -> Option<Scratch<'a>> {
    let c = mongo(e, mongo_profile(name))?;
    let s = Scratch { e, c, db: name.to_string() };
    let _ = run(e, &s.c, name, "db.dropDatabase()", Page::default());
    Some(s)
}

#[test]
fn connects_lists_and_pages() {
    let e = env();
    let Some(s) = scratch(&e, "dataman_t_pages") else { return };
    let (c, db) = (&s.c, s.db.as_str());
    assert_eq!(c.engine(), Engine::MongoDb);
    assert!(c.info.version.starts_with("MongoDB "), "{}", c.info.version);
    assert_eq!(c.database(), db);
    assert!(!c.caps().schemas && !c.caps().explain);

    let docs: Vec<String> = (0..120).map(|i| format!("{{n: {i}, name: 'user {i}', even: {}}}", i % 2 == 0)).collect();
    let Output::Written { affected, .. } = ok(&e, c, db, &format!("db.users.insertMany([{}])", docs.join(","))) else { panic!() };
    assert_eq!(affected, 120);
    ok(&e, c, db, "db.createCollection('empty')");

    let dbs = e.core.block_on(c.nosql_tree(None)).unwrap();
    assert!(dbs.iter().any(|d| d.name == db && d.kind == ItemKind::Database), "{dbs:?}");
    let colls = e.core.block_on(c.nosql_tree(Some(db))).unwrap();
    let users = colls.iter().find(|i| i.name == "users").unwrap();
    assert_eq!((users.kind, users.count), (ItemKind::Collection, Some(120)));
    assert!(colls.iter().any(|i| i.name == "empty"));

    let page = |skip| match run(&e, c, db, "db.users.find({}).sort({n: 1})", Page { skip, limit: 50 }).unwrap().output {
        Output::Docs(d) => d,
        o => panic!("{o:?}"),
    };
    let p1 = page(0);
    assert_eq!((p1.docs.len(), p1.total, p1.more, p1.skip), (50, Some(120), true, 0));
    assert_eq!(p1.source, Some(Source::Mongo { db: db.into(), coll: "users".into() }));
    assert!(matches!(&p1.docs[0].id, Some(Body::Bson(bson::Bson::ObjectId(_)))));
    let p3 = page(100);
    assert_eq!((p3.docs.len(), p3.more, p3.total), (20, false, None));
    assert!(p3.docs[0].body.text().contains("\"n\": 100"), "{}", p3.docs[0].body.text());

    let Output::Docs(d) = run(&e, c, db, "db.users.find({even: true}).skip(2).limit(55)", Page { skip: 50, limit: 50 }).unwrap().output else { panic!() };
    assert_eq!((d.docs.len(), d.more), (5, false));
    let Output::Docs(d) = ok(&e, c, db, "db.users.find({even: true}).limit(55)") else { panic!() };
    assert_eq!((d.docs.len(), d.more, d.total), (50, true, Some(55)));
    let Output::Docs(d) = ok(&e, c, db, "db.users.find({}, {name: 1})") else { panic!() };
    assert_eq!(d.source, None);

    let Output::Docs(d) = ok(&e, c, db, "db.users.aggregate([{$group: {_id: '$even', n: {$sum: 1}}}, {$sort: {_id: 1}}])") else { panic!() };
    assert_eq!(d.docs.len(), 2);
    assert!(d.docs[1].body.text().contains("\"n\": 60"), "{}", d.docs[1].body.text());
    assert_eq!(ok(&e, c, db, "db.users.countDocuments({even: false})"), Output::Value(Body::Bson(bson::Bson::Int64(60))));
    let Output::Value(Body::Bson(bson::Bson::Array(v))) = ok(&e, c, db, "db.users.distinct('even')") else { panic!() };
    assert_eq!(v.len(), 2);
    let Output::Value(Body::Bson(bson::Bson::Array(names))) = ok(&e, c, db, "show collections") else { panic!() };
    assert!(names.iter().any(|n| n.as_str() == Some("users")));
    let Output::Docs(d) = ok(&e, c, db, "db.runCommand({find: 'users', filter: {n: {$lt: 3}}})") else { panic!() };
    assert_eq!(d.docs.len(), 3);
    assert!(matches!(ok(&e, c, db, "db.runCommand({dbStats: 1})"), Output::Value(_)));
    assert_eq!(ok(&e, c, db, "use other"), Output::Use("other".into()));

    let fields = e.core.block_on(c.sample_fields(db, "users")).unwrap();
    assert_eq!(fields[0].0, "_id");
    assert!(fields.iter().any(|(p, t)| p == "even" && t == &vec!["bool"]), "{fields:?}");
}

#[test]
fn edits_keep_types_and_follow_the_switches() {
    let e = env();
    let Some(s) = scratch(&e, "dataman_t_edit") else { return };
    let (c, db) = (&s.c, s.db.as_str());
    ok(&e, c, db, "db.items.insertOne({_id: 1, name: 'pen', qty: NumberLong(3), price: 2.0, at: ISODate('2024-05-06T07:08:09Z')})");
    let Output::Docs(d) = ok(&e, c, db, "db.items.find({_id: 1})") else { panic!() };
    let doc = &d.docs[0];
    let text = doc.body.text();
    assert!(text.contains("\"qty\": NumberLong(3)") && text.contains("\"price\": 2.0") && text.contains("ISODate(\"2024-05-06T07:08:09.000Z\")"), "{text}");
    let source = d.source.clone().unwrap();

    let edited = text.replace("NumberLong(3)", "NumberLong(4)").replace("\"pen\"", "\"pencil\"");
    let id = e.core.block_on(c.save_document(&source, doc.id.clone(), &edited)).unwrap();
    assert_eq!(id, Body::Bson(bson::Bson::Int32(1)));
    let Output::Docs(d) = ok(&e, c, db, "db.items.find({_id: 1})") else { panic!() };
    assert_eq!(d.docs[0].body.text(), edited);
    let err = e.core.block_on(c.save_document(&source, doc.id.clone(), &edited.replace("\"_id\": 1", "\"_id\": 2"))).unwrap_err();
    assert!(format!("{err:#}").contains("_id"), "{err:#}");
    let err = e.core.block_on(c.save_document(&source, doc.id.clone(), "{name: }")).unwrap_err();
    assert_eq!(err.downcast_ref::<Failed>().and_then(|f| f.at), Some(7), "{err:#}");
    let new = e.core.block_on(c.save_document(&source, None, "{name: 'eraser'}")).unwrap();
    assert!(matches!(new, Body::Bson(bson::Bson::ObjectId(_))));
    assert_eq!(e.core.block_on(c.delete_document(&source, new)).unwrap(), 1);

    let Output::Written { affected, summary, .. } = ok(&e, c, db, "db.items.updateMany({_id: 1}, {$inc: {qty: NumberLong(1)}})") else { panic!() };
    assert_eq!((affected, summary.as_str()), (1, "1 matched, 1 modified"));
    let Output::Written { affected, .. } = ok(&e, c, db, "db.items.deleteOne({_id: 1})") else { panic!() };
    assert_eq!(affected, 1);

    let f = run(&e, c, db, "db.items.find({a: })", Page::default()).unwrap_err();
    assert_eq!(f.at, Some(18), "{f:?}");
    let f = run(&e, c, db, "db.items.aggregate([{$nope: 1}])", Page::default()).unwrap_err();
    assert!(f.message.contains("$nope") && f.at.is_none(), "{f:?}");

    let mut p = mongo_profile(db);
    p.read_only = true;
    let ro = mongo(&e, p).unwrap();
    let f = run(&e, &ro, db, "db.items.insertOne({x: 1})", Page::default()).unwrap_err();
    assert!(f.message.contains("read-only"), "{f:?}");
    assert!(e.core.block_on(ro.save_document(&source, None, "{x: 1}")).is_err());
    assert!(run(&e, &ro, db, "db.items.find()", Page::default()).is_ok());
}

#[test]
fn a_wrong_password_says_so() {
    let e = env();
    let p = mongo_profile("admin");
    match e.core.block_on(e.core.connect(p.clone(), Some(password()))) {
        Ok(_) => {}
        Err(_) => return,
    }
    let err = e.core.block_on(e.core.connect(p, Some("wrong".into()))).err().expect("refused");
    assert!(format!("{err:#}").contains("authentication failed"), "{err:#}");
    let (info, _) = e.core.block_on(e.core.test_connection(mongo_profile("admin"), Some(password()), None)).unwrap();
    assert!(info.version.starts_with("MongoDB"), "{}", info.version);
}
