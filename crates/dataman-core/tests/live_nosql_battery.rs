//! A large battery of real-world statements against each document engine, on rich data
//! (dates, geo points, arrays, nested documents, decimals, binaries): aggregations, date and
//! geo filters, text search, every Redis type. Every statement runs; the failures are listed
//! together at the end (one test per engine). Skips an engine whose server does not answer
//! (the servers of `live_mongo`, `live_search` and `live_redis`).

mod common;

use common::*;
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::nosql::{Body, Failed, Output, Page, Ran};

struct Battery<'a> {
    e: &'a Env,
    c: Arc<Connection>,
    db: String,
    failures: Vec<String>,
    ran: usize,
}

impl Battery<'_> {
    fn run(&mut self, text: &str, page: Page) -> Result<Ran, Failed> {
        self.ran += 1;
        let r = self.c.run_nosql(&self.db, text, page);
        match self.e.core.block_on(async { tokio::time::timeout(Duration::from_secs(60), r).await }) {
            Ok(Ok(ran)) => Ok(ran),
            Ok(Err(err)) => Err(err.downcast_ref::<Failed>().cloned().unwrap_or_else(|| Failed { message: format!("{err:#}"), at: None })),
            Err(_) => Err(Failed { message: "timed out".into(), at: None }),
        }
    }

    /// Runs `text`; `check` says whether its output is right (`Err`: why not).
    fn expect(&mut self, what: &str, text: &str, check: impl FnOnce(&Output) -> Result<(), String>) {
        match self.run(text, Page::default()) {
            Ok(r) => {
                if let Err(why) = check(&r.output) {
                    self.failures.push(format!("{what}: {why}\n    {text}"));
                }
            }
            Err(f) => self.failures.push(format!("{what}: {} (at {:?})\n    {text}", f.message, f.at)),
        }
    }

    /// As [`Battery::expect`], with the whole run (a script's printed lines, its final cursor).
    fn expect_ran(&mut self, what: &str, text: &str, check: impl FnOnce(&Ran) -> Result<(), String>) {
        match self.run(text, Page::default()) {
            Ok(r) => {
                if let Err(why) = check(&r) {
                    self.failures.push(format!("{what}: {why}\n    {text}"));
                }
            }
            Err(f) => self.failures.push(format!("{what}: {} (at {:?})\n    {text}", f.message, f.at)),
        }
    }

    /// A script whose value must be `true`.
    fn yes(&mut self, what: &str, text: &str) {
        self.expect(what, text, |o| match o {
            Output::Value(b) if b.text() == "true" => Ok(()),
            o => Err(format!("not true: {}", short(o))),
        });
    }

    fn ok(&mut self, what: &str, text: &str) {
        self.expect(what, text, |_| Ok(()));
    }

    /// Must be refused or fail (unsupported, bad input): `Ok` is a failure.
    fn refused(&mut self, what: &str, text: &str, says: &str) {
        match self.run(text, Page::default()) {
            Ok(r) => self.failures.push(format!("{what}: ran, expected a refusal: {:?}\n    {text}", short(&r.output))),
            Err(f) if !f.message.to_lowercase().contains(&says.to_lowercase()) => self.failures.push(format!("{what}: refused, but said {:?}\n    {text}", f.message)),
            Err(_) => {}
        }
    }

    fn finish(self, engine: &str) {
        eprintln!("{engine}: {} statements, {} failures", self.ran, self.failures.len());
        assert!(self.failures.is_empty(), "{engine}:\n  {}", self.failures.join("\n  "));
    }
}

fn short(o: &Output) -> String {
    let s = format!("{o:?}");
    s.chars().take(300).collect()
}

fn docs(o: &Output) -> Result<&dataman_core::nosql::Docs, String> {
    match o {
        Output::Docs(d) => Ok(d),
        o => Err(format!("not documents: {}", short(o))),
    }
}

fn n_docs(o: &Output, n: usize) -> Result<(), String> {
    let d = docs(o)?;
    if d.docs.len() == n { Ok(()) } else { Err(format!("{} documents, expected {n}: {}", d.docs.len(), d.docs.first().map(|d| d.body.text()).unwrap_or_default())) }
}

fn json_of(o: &Output) -> Result<serde_json::Value, String> {
    match o {
        Output::Value(b) => Ok(b.to_json()),
        Output::Written { detail: Some(b), .. } => Ok(b.to_json()),
        Output::Docs(d) => Ok(serde_json::Value::Array(d.docs.iter().map(|d| d.body.to_json()).collect())),
        o => Err(format!("no value: {}", short(o))),
    }
}

fn text_has(o: &Output, needle: &str) -> Result<(), String> {
    let t = match o {
        Output::Docs(d) => d.docs.iter().map(|d| d.body.text()).collect::<Vec<_>>().join("\n"),
        Output::Value(b) => b.text(),
        Output::Written { summary, detail, .. } => format!("{summary} {}", detail.as_ref().map(Body::text).unwrap_or_default()),
        Output::Use(s) => s.clone(),
    };
    if t.contains(needle) { Ok(()) } else { Err(format!("{needle:?} not in {}", t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect::<String>())) }
}

// ---------------------------------------------------------------------------------------------
// MongoDB

#[test]
fn mongodb_battery() {
    let e = env();
    let v = std::env::var("DATAMAN_MONGO").unwrap_or_else(|_| "127.0.0.1:27027".into());
    let (host, port) = v.rsplit_once(':').unwrap();
    let p = ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: "root".into(), database: "dataman_battery".into(), sslmode: SslMode::Disable, engine: Engine::MongoDb, ..ConnectionProfile::new("mongo") };
    let pw = std::env::var("DATAMAN_MONGO_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let Ok(c) = e.core.block_on(e.core.connect(p, Some(pw))) else {
        eprintln!("skipping: no MongoDB");
        return;
    };
    let mut b = Battery { e: &e, c, db: "dataman_battery".into(), failures: Vec::new(), ran: 0 };
    b.ok("drop", "db.dropDatabase()");

    // Rich data: places with points, people with nested addresses and arrays, orders with
    // decimals and dates, a big document, binaries.
    b.ok("places", r#"db.places.insertMany([
      {_id: 1, name: "Belém Tower", city: "Lisbon", loc: {type: "Point", coordinates: [-9.2160, 38.6916]}, tags: ["monument", "river"]},
      {_id: 2, name: "Praça do Comércio", city: "Lisbon", loc: {type: "Point", coordinates: [-9.1366, 38.7075]}, tags: ["square"]},
      {_id: 3, name: "Torre dos Clérigos", city: "Porto", loc: {type: "Point", coordinates: [-8.6146, 41.1456]}, tags: ["monument"]},
      {_id: 4, name: "Big Ben", city: "London", loc: {type: "Point", coordinates: [-0.1246, 51.5007]}, tags: ["monument", "clock"]},
      {_id: 5, name: "Empire State", city: "New York", loc: {type: "Point", coordinates: [-73.9857, 40.7484]}, tags: ["tower"]}
    ])"#);
    b.ok("2dsphere", "db.places.createIndex({loc: '2dsphere'})");
    let mut people = Vec::new();
    for i in 0..40 {
        people.push(format!(
            "{{_id: ObjectId('{:024x}'), name: '{} {}', age: {}, born: ISODate('19{:02}-0{}-1{}T08:00:00Z'), address: {{city: '{}', zip: '{:05}', geo: [{}, {}]}}, skills: [{}], scores: [{{subject: 'math', v: {}}}, {{subject: 'art', v: {}}}], vip: {}, balance: NumberDecimal('{}.{:02}'), bio: '{}'}}",
            0x650000000000000000000000u128 + i,
            ["Ada", "Grace", "Alan", "Linus", "Margaret"][i as usize % 5],
            ["Lovelace", "Hopper", "Turing", "Torvalds", "Hamilton"][i as usize % 5],
            18 + i,
            50 + i,
            1 + i % 9,
            i % 9,
            ["Lisbon", "Porto", "London", "New York"][i as usize % 4],
            1000 + i * 7,
            -9.0 - i as f64 / 100.0,
            38.0 + i as f64 / 100.0,
            ["'rust'", "'go', 'sql'", "'swift', 'rust', 'sql'"][i as usize % 3],
            50 + i % 50,
            90 - i % 40,
            i % 3 == 0,
            i * 13,
            i % 100,
            ["loves compilers and coffee", "writes databases at night", "rides bikes in the rain", "plays chess with robots"][i as usize % 4]
        ));
    }
    b.ok("people", &format!("db.people.insertMany([{}])", people.join(",")));
    b.ok("text index", "db.people.createIndex({bio: 'text', name: 'text'})");
    let mut orders = Vec::new();
    for i in 0..200 {
        orders.push(format!(
            "{{n: {i}, person: ObjectId('{:024x}'), total: NumberDecimal('{}.{:02}'), qty: NumberLong({}), status: '{}', created: ISODate('2024-{:02}-{:02}T{:02}:30:00Z'), items: [{{sku: 'A{}', price: {}.5}}, {{sku: 'B{}', price: {}}}]}}",
            0x650000000000000000000000u128 + i % 40,
            i * 3 % 997,
            i % 100,
            1 + i % 7,
            ["paid", "shipped", "new", "refunded"][i as usize % 4],
            1 + i % 12,
            1 + i % 28,
            i % 24,
            i % 9,
            i % 50,
            i % 7,
            i % 30
        ));
    }
    b.ok("orders", &format!("db.orders.insertMany([{}])", orders.join(",")));
    b.ok("big document", &format!("db.big.insertOne({{_id: 'big', text: '{}', arr: [{}]}})", "x".repeat(200_000), (0..5000).map(|i| i.to_string()).collect::<Vec<_>>().join(",")));
    b.ok("binaries and odd types", "db.odd.insertOne({_id: 'odd', bin: BinData(0, 'AAECAwQF/w=='), uuid: UUID('123e4567-e89b-12d3-a456-426614174000'), ts: Timestamp(1700000000, 1), min: MinKey(), max: MaxKey(), re: /ab+c/i, nan: NaN, inf: Infinity, neg: -0.0, old: ISODate('1900-01-01T00:00:00Z'), far: ISODate('2999-12-31T23:59:59.999Z'), big: NumberLong('9223372036854775807'), dec: NumberDecimal('12345678901234567890.123456789'), nested: {a: {b: {c: {d: [1, [2, [3]]]}}}}})");

    // Filters: dates, regex, arrays, nested, $expr, $in with ObjectIds, $exists, $type.
    b.expect("date range", "db.orders.find({created: {$gte: ISODate('2024-03-01'), $lt: new Date('2024-04-01')}})", |o| docs(o).map(|_| ()));
    b.expect("date range count", "db.orders.countDocuments({created: {$gte: ISODate('2024-03-01T00:00:00Z'), $lt: ISODate('2024-04-01T00:00:00Z')}})", |o| text_has(o, "NumberLong(17)"));
    b.expect("regex literal", "db.people.find({name: /^ada/i}, {name: 1})", |o| n_docs(o, 8));
    b.expect("regex operator", "db.people.find({name: {$regex: 'TORV', $options: 'i'}}).count()", |o| text_has(o, "NumberLong(8)"));
    b.expect("$in ObjectIds", "db.people.find({_id: {$in: [ObjectId('650000000000000000000000'), ObjectId('650000000000000000000001')]}})", |o| n_docs(o, 2));
    b.expect("array contains", "db.people.find({skills: 'rust'}).count()", |o| text_has(o, "NumberLong("));
    b.expect("$all", "db.people.find({skills: {$all: ['rust', 'sql']}}).count()", |o| text_has(o, "NumberLong(13)"));
    b.expect("$elemMatch", "db.people.find({scores: {$elemMatch: {subject: 'math', v: {$gte: 80}}}}).count()", |o| text_has(o, "NumberLong("));
    b.expect("$size", "db.people.find({skills: {$size: 3}}).count()", |o| text_has(o, "NumberLong(13)"));
    b.expect("nested path", "db.people.find({'address.city': 'Porto'}).count()", |o| text_has(o, "NumberLong(10)"));
    b.expect("$expr", "db.people.find({$expr: {$gt: [{$arrayElemAt: ['$scores.v', 0]}, {$arrayElemAt: ['$scores.v', 1]}]}}).count()", |o| text_has(o, "NumberLong("));
    b.expect("$exists $type", "db.odd.find({bin: {$exists: true}, dec: {$type: 'decimal'}}).count()", |o| text_has(o, "NumberLong(1)"));
    b.expect("$or $and $nor $not", "db.people.find({$or: [{age: {$lt: 20}}, {$and: [{vip: true}, {age: {$not: {$gte: 50}}}]}], $nor: [{name: /Turing/}]}).count()", |o| text_has(o, "NumberLong("));
    b.expect("decimal compare", "db.people.find({balance: {$gt: NumberDecimal('400')}}).count()", |o| text_has(o, "NumberLong("));
    b.expect("projection $slice $elemMatch", "db.people.findOne({}, {skills: {$slice: 1}, scores: {$elemMatch: {subject: 'art'}}})", |o| n_docs(o, 1));
    b.expect("sort skip limit chain", "db.orders.find({status: 'paid'}).sort({total: -1, n: 1}).skip(3).limit(5)", |o| n_docs(o, 5));
    b.expect("negative limit", "db.orders.find().limit(-3)", |o| n_docs(o, 3));
    b.expect("collation", "db.people.find({name: 'ada lovelace'}).collation({locale: 'en', strength: 2})", |o| n_docs(o, 8));
    b.expect("hint", "db.orders.find({status: 'paid'}).hint({_id: 1}).limit(2)", |o| n_docs(o, 2));
    b.expect("text search", "db.people.find({$text: {$search: 'compilers'}}, {score: {$meta: 'textScore'}, name: 1}).sort({score: {$meta: 'textScore'}})", |o| n_docs(o, 10));
    b.expect("$near", "db.places.find({loc: {$near: {$geometry: {type: 'Point', coordinates: [-9.14, 38.71]}, $maxDistance: 10000}}})", |o| n_docs(o, 2));
    b.expect("$geoWithin $centerSphere", "db.places.find({loc: {$geoWithin: {$centerSphere: [[-9.14, 38.71], 400 / 6378.1]}}}, {name: 1})", |o| n_docs(o, 3));
    b.expect("$geoWithin polygon", "db.places.find({loc: {$geoWithin: {$geometry: {type: 'Polygon', coordinates: [[[-10, 38], [-8, 38], [-8, 42], [-10, 42], [-10, 38]]]}}}}).count()", |o| text_has(o, "NumberLong(3)"));
    b.expect("big document", "db.big.find()", |o| text_has(o, "xxxxxxxx"));
    b.expect("odd types", "db.odd.findOne()", |o| {
        for s in ["BinData(0, ", "UUID(\"123e4567-e89b-12d3-a456-426614174000\")", "Timestamp(1700000000, 1)", "MinKey()", "MaxKey()", "/ab+c/i", "NaN", "Infinity", "ISODate(\"1900-01-01T00:00:00.000Z\")", "ISODate(\"2999-12-31T23:59:59.999Z\")", "NumberLong(9223372036854775807)", "NumberDecimal(\"12345678901234567890.123456789\")"] {
            text_has(o, s)?;
        }
        Ok(())
    });

    // Aggregations.
    b.expect("$group by month", "db.orders.aggregate([{$group: {_id: {$dateToString: {format: '%Y-%m', date: '$created'}}, n: {$sum: 1}, total: {$sum: '$total'}, avg: {$avg: '$qty'}}}, {$sort: {_id: 1}}])", |o| n_docs(o, 12));
    b.expect("$lookup $unwind $project", "db.orders.aggregate([{$match: {status: 'paid'}}, {$lookup: {from: 'people', localField: 'person', foreignField: '_id', as: 'who'}}, {$unwind: '$who'}, {$project: {n: 1, 'who.name': 1, 'who.address.city': 1}}, {$limit: 5}])", |o| { n_docs(o, 5)?; text_has(o, "who") });
    b.expect("$lookup pipeline", "db.people.aggregate([{$lookup: {from: 'orders', let: {p: '$_id'}, pipeline: [{$match: {$expr: {$eq: ['$person', '$$p']}}}, {$group: {_id: null, spent: {$sum: '$total'}}}], as: 'spent'}}, {$project: {name: 1, spent: {$first: '$spent.spent'}}}, {$sort: {spent: -1}}, {$limit: 3}])", |o| n_docs(o, 3));
    b.expect("$facet $bucket $bucketAuto $count", "db.people.aggregate([{$facet: {byAge: [{$bucket: {groupBy: '$age', boundaries: [0, 20, 30, 40, 60], default: 'other', output: {n: {$sum: 1}}}}], auto: [{$bucketAuto: {groupBy: '$balance', buckets: 4}}], total: [{$count: 'n'}], cities: [{$sortByCount: '$address.city'}]}}])", |o| { n_docs(o, 1)?; text_has(o, "byAge") });
    b.expect("$unwind arrays + $group $addToSet", "db.people.aggregate([{$unwind: '$skills'}, {$group: {_id: '$skills', people: {$addToSet: '$name'}, n: {$sum: 1}}}, {$sort: {n: -1}}])", |o| n_docs(o, 4));
    b.expect("$geoNear", "db.places.aggregate([{$geoNear: {near: {type: 'Point', coordinates: [-9.14, 38.71]}, distanceField: 'dist', spherical: true}}, {$project: {name: 1, km: {$round: [{$divide: ['$dist', 1000]}, 1]}}}])", |o| n_docs(o, 5));
    b.expect("$setWindowFields", "db.orders.aggregate([{$setWindowFields: {partitionBy: '$status', sortBy: {created: 1}, output: {running: {$sum: '$qty', window: {documents: ['unbounded', 'current']}}}}}, {$limit: 3}])", |o| n_docs(o, 3));
    b.expect("$dateTrunc $dateDiff", "db.orders.aggregate([{$project: {week: {$dateTrunc: {date: '$created', unit: 'week'}}, age: {$dateDiff: {startDate: '$created', endDate: ISODate('2025-01-01'), unit: 'day'}}}}, {$limit: 2}])", |o| n_docs(o, 2));
    b.expect("$replaceRoot $mergeObjects", "db.people.aggregate([{$replaceRoot: {newRoot: {$mergeObjects: ['$address', {who: '$name'}]}}}, {$limit: 2}])", |o| n_docs(o, 2));
    b.expect("$unionWith", "db.places.aggregate([{$project: {name: 1}}, {$unionWith: {coll: 'people', pipeline: [{$project: {name: 1}}, {$limit: 2}]}}])", |o| n_docs(o, 7));
    b.expect("aggregate with options", "db.orders.aggregate([{$sort: {total: -1}}, {$limit: 1}], {allowDiskUse: true})", |o| n_docs(o, 1));
    b.expect("aggregate stages as arguments", "db.orders.aggregate({$match: {n: 1}}, {$project: {n: 1}})", |o| n_docs(o, 1));
    b.expect("paging an aggregation", "db.orders.aggregate([{$sort: {n: 1}}])", |o| { let d = docs(o)?; if d.more && d.docs.len() == 50 { Ok(()) } else { Err(format!("{} more {}", d.docs.len(), d.more)) } });
    b.expect("db.aggregate $documents", "db.aggregate([{$documents: [{x: 1}, {x: 2}]}])", |o| n_docs(o, 2));
    b.expect("distinct nested", "db.people.distinct('address.city')", |o| { let v = json_of(o)?; if v.as_array().map(Vec::len) == Some(4) { Ok(()) } else { Err(v.to_string()) } });
    b.expect("distinct with filter", "db.people.distinct('skills', {vip: true})", |o| json_of(o).map(|_| ()));
    b.expect("explain", "db.orders.find({status: 'paid'}).explain('executionStats')", |o| text_has(o, "executionStats"));
    b.expect("aggregate explain", "db.orders.aggregate([{$match: {n: 1}}]).explain()", |o| text_has(o, "queryPlanner"));

    // Writes with options and the shell's helpers.
    b.expect("updateOne arrayFilters", "db.people.updateOne({_id: ObjectId('650000000000000000000000')}, {$set: {'scores.$[s].v': 100}}, {arrayFilters: [{'s.subject': 'math'}]})", |o| text_has(o, "1 modified"));
    b.expect("update pipeline", "db.orders.updateMany({status: 'refunded'}, [{$set: {refundedTotal: {$multiply: ['$total', -1]}}}])", |o| text_has(o, "50 modified"));
    b.expect("upsert", "db.orders.updateOne({n: 9999}, {$setOnInsert: {status: 'new'}, $inc: {qty: 1}}, {upsert: true})", |o| text_has(o, "upserted"));
    b.expect("$push $each $slice $sort", "db.people.updateOne({_id: ObjectId('650000000000000000000001')}, {$push: {skills: {$each: ['zig', 'ada'], $sort: 1, $slice: 3}}})", |o| text_has(o, "1 modified"));
    b.expect("$currentDate $unset $rename", "db.people.updateMany({vip: true}, {$currentDate: {touched: true}, $unset: {bio: ''}, $rename: {zip: 'postcode'}})", |o| text_has(o, "modified"));
    b.expect("findOneAndUpdate", "db.orders.findOneAndUpdate({n: 1}, {$set: {status: 'shipped'}}, {returnDocument: 'after'})", |o| text_has(o, "shipped"));
    b.expect("findOneAndReplace", "db.orders.findOneAndReplace({n: 2}, {n: 2, status: 'replaced'})", |o| text_has(o, "\"n\": 2"));
    b.expect("findOneAndDelete", "db.orders.findOneAndDelete({n: 3})", |o| text_has(o, "\"n\": 3"));
    b.expect("bulkWrite", "db.orders.bulkWrite([{insertOne: {document: {n: 5000}}}, {updateOne: {filter: {n: 5000}, update: {$set: {status: 'bulk'}}}}, {deleteOne: {filter: {n: 4}}}])", |o| text_has(o, "insert"));
    b.expect("insertMany with dates and decimals", "db.events.insertMany([{at: new Date(), d: NumberDecimal('0.1')}, {at: ISODate('2024-02-29'), d: NumberDecimal('0.2')}])", |o| text_has(o, "2 documents inserted"));
    b.expect("createIndex options", "db.orders.createIndex({status: 1, created: -1}, {name: 'status_created', partialFilterExpression: {status: 'paid'}})", |o| text_has(o, "status_created"));
    b.expect("getIndexes", "db.orders.getIndexes()", |o| text_has(o, "status_created"));
    b.ok("dropIndex", "db.orders.dropIndex('status_created')");
    b.ok("collection stats", "db.orders.stats()");
    b.ok("db stats", "db.stats()");
    b.ok("server status", "db.serverStatus()");
    b.ok("version", "db.version()");
    b.ok("list collections", "show collections");
    b.ok("list databases", "show dbs");
    b.ok("runCommand", "db.runCommand({collStats: 'orders', scale: 1024})");
    b.ok("adminCommand", "db.adminCommand({listDatabases: 1, nameOnly: true})");
    b.ok("getSiblingDB", "db.getSiblingDB('admin').runCommand({ping: 1})");
    b.ok("currentOp", "db.currentOp()");
    b.ok("rename", "db.events.renameCollection('events2')");
    b.ok("createCollection capped", "db.createCollection('capped', {capped: true, size: 100000})");
    b.ok("createView", "db.createView('paid_orders', 'orders', [{$match: {status: 'paid'}}])");
    b.expect("a view's documents", "db.paid_orders.find().limit(3)", |o| n_docs(o, 3));
    b.ok("time series", "db.createCollection('ticks', {timeseries: {timeField: 't', metaField: 'sym'}})");
    b.ok("time series insert", "db.ticks.insertOne({t: new Date(), sym: 'ACME', px: 1.5})");

    // The shell's JavaScript: variables, dates, loops, functions, cursors, print.
    b.yes("const + new Date + Date.UTC", "const since = new Date(Date.UTC(2024, 5, 1))\ndb.orders.countDocuments({created: {$gte: since}}) === db.orders.countDocuments({created: {$gte: ISODate('2024-06-01')}})");
    b.yes("Date.now() arithmetic", "const weekAgo = new Date(Date.now() - 7 * 864e5)\ndb.events2.countDocuments({at: {$gte: weekAgo}}) >= 1");
    b.yes("date methods", "const d = new Date('2024-02-29T00:00:00Z'); d.setUTCFullYear(2025); d.toISOString() === '2025-03-01T00:00:00.000Z'");
    b.expect("forEach + updateOne in a loop", "let n = 0\ndb.orders.find({status: 'new'}).limit(5).forEach(o => {\n  db.orders.updateOne({_id: o._id}, {$set: {touched: true}})\n  n++\n})\nn", |o| text_has(o, "5"));
    b.expect("…the loop's writes are there", "db.orders.countDocuments({touched: true})", |o| text_has(o, "5"));
    b.expect_ran("function + print + a final cursor", "function sorted(field) { return {[field]: 1} }\nprint('looking')\ndb.orders.find({}).sort(sorted('n'))", |r| {
        let d = docs(&r.output)?;
        if r.printed != ["looking"] || d.docs.len() != 50 || !d.more || d.source.is_none() || r.rerun.is_none() {
            return Err(format!("printed {:?}, {} docs, more {}, source {:?}, rerun {}", r.printed, d.docs.len(), d.more, d.source, r.rerun.is_some()));
        }
        Ok(())
    });
    b.yes("toArray + map + reduce = $sum", "const js = db.orders.find({status: 'paid'}).toArray().map(o => Number(o.qty)).reduce((a, b) => a + b, 0)\nconst agg = db.orders.aggregate([{$match: {status: 'paid'}}, {$group: {_id: null, q: {$sum: '$qty'}}}]).next()\njs === Number(agg.q)");
    b.yes("ObjectId methods on an aggregation's _id", "const top = db.orders.aggregate([{$group: {_id: '$person', n: {$sum: 1}}}, {$sort: {n: -1}}, {$limit: 1}]).next()\ntop._id instanceof ObjectId && top._id.toHexString().length === 24 && top._id.getTimestamp() instanceof Date");
    b.yes("types survive a round trip", "const id = ObjectId()\ndb.events.insertOne({_id: id, at: new Date(), n: NumberLong(5), d: NumberDecimal('0.3'), u: UUID('123e4567-e89b-12d3-a456-426614174000')})\nconst e = db.events.findOne({_id: id})\ne.n instanceof NumberLong && e.d.toString() === '0.3' && e.at instanceof Date && e._id.equals(id) && e.u.toString() === '123e4567-e89b-12d3-a456-426614174000'");
    b.expect("try / catch a server error", "try { db.orders.aggregate([{$bogus: 1}]).toArray(); 'no' } catch (e) { 'caught: ' + e.message }", |o| text_has(o, "caught: Unrecognized pipeline stage"));
    b.expect_ran("use in a script", "use admin\ndb.runCommand({ping: 1}).ok", |r| if r.moved.as_deref() == Some("admin") { Ok(()) } else { Err(format!("moved {:?}", r.moved)) });
    b.expect("JSON.stringify of a document", "JSON.stringify(db.people.findOne({}, {_id: 1}))", |o| text_has(o, "$oid"));
    b.expect("a loop's generated documents, then a date filter", "const docs = []\nfor (let i = 0; i < 100; i++) docs.push({i, sq: i * i, at: new Date(Date.UTC(2024, 0, 1 + i))})\ndb.gen.insertMany(docs)\ndb.gen.countDocuments({at: {$gte: ISODate('2024-03-01')}})", |o| text_has(o, "40"));
    b.expect("geo + map + join", "db.places.find({loc: {$near: {$geometry: {type: 'Point', coordinates: [-9.14, 38.71]}, $maxDistance: 10000}}}).toArray().map(p => p.name).join(', ')", |o| text_has(o, "Praça do Comércio"));
    b.yes("cursor next / hasNext / count", "const c = db.orders.find({status: 'paid'})\nconst first = c.next()\nc.hasNext() && c.count() === db.orders.countDocuments({status: 'paid'}) && first.status === 'paid'");
    b.expect("for…of a cursor, break", "let k = 0\nfor (const o of db.orders.find()) { if (++k >= 3) break }\nk", |o| text_has(o, "3"));
    b.expect("bulkWrite's result", "db.gen.bulkWrite([{insertOne: {document: {i: 1000}}}, {deleteOne: {filter: {i: 1000}}}]).deletedCount", |o| text_has(o, "1"));
    b.expect_ran("forEach(printjson)", "db.places.find({_id: 1}).forEach(printjson)", |r| text_has(&r.output, "Belém Tower"));
    b.expect("destructuring + template", "const {name, city} = db.places.findOne({_id: 3});\n`${name} @ ${city}`", |o| text_has(o, "Torre dos Clérigos @ Porto"));
    b.yes("getCollectionNames", "db.getCollectionNames().includes('orders')");
    b.yes("explain from a script", "db.orders.find({status: 'paid'}).explain('executionStats').executionStats.nReturned === db.orders.countDocuments({status: 'paid'})");
    b.expect("distinct + sort + join", "db.people.distinct('address.city').sort().join('|')", |o| text_has(o, "Lisbon|London|New York|Porto"));
    b.yes("a write per document", "db.orders.find().forEach(o => db.orders.updateOne({_id: o._id}, {$set: {qty2: Number(o.qty) * 2}}))\ndb.orders.countDocuments({qty2: {$exists: true}}) === db.orders.countDocuments({})");
    b.expect_ran("an aggregation's cursor as the result", "const stage = {$match: {status: 'paid'}}\ndb.orders.aggregate([stage, {$sort: {n: 1}}])", |r| {
        let d = docs(&r.output)?;
        if d.docs.is_empty() || r.rerun.is_none() { Err(format!("{} docs, rerun {}", d.docs.len(), r.rerun.is_some())) } else { Ok(()) }
    });
    b.expect("several statements: the last one's result", "db.gen.insertOne({_id: 'multi'}); db.gen.findOne({_id: 'multi'})", |o| text_has(o, "multi"));
    // The page after a script's final cursor: its query again, not the script.
    let ran = b.run("db.gen.insertOne({mark: 1})\ndb.orders.find({}).sort({n: 1})", Page::default());
    match ran.as_ref().ok().and_then(|r| r.rerun.clone()) {
        Some(rerun) => {
            let next = b.e.core.block_on(b.c.rerun_nosql(&rerun, Page { skip: 50, limit: 50 }));
            let marks = b.run("db.gen.countDocuments({mark: 1})", Page::default());
            match (next, marks) {
                (Ok(n), Ok(m)) if docs(&n.output).is_ok_and(|d| d.docs.len() == 50 && d.skip == 50) && text_has(&m.output, "1").is_ok() => {}
                (n, m) => b.failures.push(format!("rerun: {:?} / {:?}", n.map(|r| short(&r.output)), m.map(|r| short(&r.output)))),
            }
        }
        None => b.failures.push(format!("rerun: no cursor: {ran:?}")),
    }
    // What a script does is read before it runs, and held to while it runs.
    let class = |t: &str| dataman_core::nosql::classify(Engine::MongoDb, t);
    for (t, want) in [
        ("let n = db.orders.countDocuments({}); n", "read"),
        ("db.orders.find().forEach(o => db.orders.updateOne({_id: o._id}, {$set: {x: 1}}))", "safe"),
        ("db.gen.deleteMany({}); 1", "destructive"),
        ("const f = {i: 1}; db.gen.deleteMany(f)", "destructive"),
        ("db.gen.drop(); 1", "destructive"),
    ] {
        let got = match class(t) {
            Ok(dataman_core::guard::WriteClass::Read) => "read",
            Ok(dataman_core::guard::WriteClass::SafeWrite) => "safe",
            Ok(_) => "destructive",
            Err(_) => "error",
        };
        if got != want {
            b.failures.push(format!("class: {t}: {got}, expected {want}"));
        }
    }
    b.refused("a write the reading did not see", "const m = 'delete' + 'Many'\ndb.gen[m]({i: 1})", "refused: this call writes");
    b.yes("…and nothing was deleted", "db.gen.countDocuments({i: 1}) === 1");
    b.refused("a syntax error, where", "let x = 1\nlet = = 2", "unexpected token");
    match b.run("let x = 1\nlet = = 2", Page::default()) {
        Err(f) if f.at == Some(16) => {}
        r => b.failures.push(format!("syntax error's place: {r:?}")),
    }
    b.refused("a thrown error", "throw new Error('stop here')", "stop here");
    b.refused("mapReduce is not here", "db.orders.mapReduce(function () {}, function () {})", "mapReduce");
    b.finish("MongoDB");
}

// ---------------------------------------------------------------------------------------------
// Elasticsearch and OpenSearch

fn search_battery(engine: Engine) {
    let e = env();
    let (var, default, user, pw) = match engine {
        Engine::Elasticsearch => ("DATAMAN_ES", "127.0.0.1:9201", "elastic", Some(std::env::var("DATAMAN_ES_PASSWORD").unwrap_or_else(|_| "dataman".into()))),
        _ => ("DATAMAN_OPENSEARCH", "127.0.0.1:9200", "", None),
    };
    let v = std::env::var(var).unwrap_or_else(|_| default.into());
    let (host, port) = v.rsplit_once(':').unwrap();
    let p = ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: user.into(), database: String::new(), sslmode: SslMode::Disable, engine, ..ConnectionProfile::new("search") };
    let Ok(c) = e.core.block_on(e.core.connect(p, pw)) else {
        eprintln!("skipping: no {}", engine.label());
        return;
    };
    let os = engine == Engine::OpenSearch;
    let mut b = Battery { e: &e, c, db: String::new(), failures: Vec::new(), ran: 0 };
    let ix = "dataman_battery";
    let _ = b.run(&format!("DELETE /{ix}"), Page::default());
    b.ok("mapping", &format!(r#"PUT /{ix}
{{
  "settings": {{"number_of_shards": 1}},
  "mappings": {{"properties": {{
    "name": {{"type": "text", "fields": {{"raw": {{"type": "keyword"}}}}}},
    "status": {{"type": "keyword"}},
    "total": {{"type": "double"}},
    "created": {{"type": "date"}},
    "loc": {{"type": "geo_point"}},
    "tags": {{"type": "keyword"}},
    "items": {{"type": "nested", "properties": {{"sku": {{"type": "keyword"}}, "qty": {{"type": "integer"}}}}}}
  }}}}
}}"#));
    let mut bulk = format!("POST /{ix}/_bulk?refresh=true\n");
    let now = chrono::Utc::now();
    for i in 0..300 {
        let when = now - chrono::Duration::hours(i * 7);
        bulk.push_str(&format!(
            "{{\"index\": {{\"_id\": \"{i}\"}}}}\n{{\"name\": \"order {i} for {}\", \"status\": \"{}\", \"total\": {}.{}, \"created\": \"{}\", \"loc\": {{\"lat\": {}, \"lon\": {}}}, \"tags\": [\"t{}\", \"t{}\"], \"items\": [{{\"sku\": \"A{}\", \"qty\": {}}}, {{\"sku\": \"B{}\", \"qty\": {}}}]}}\n",
            ["Ada Lovelace", "Grace Hopper", "Alan Turing"][i as usize % 3],
            ["paid", "shipped", "new", "refunded"][i as usize % 4],
            i * 3 % 500,
            i % 10,
            when.format("%Y-%m-%dT%H:%M:%SZ"),
            38.7 + (i % 20) as f64 / 10.0,
            -9.1 + (i % 15) as f64 / 10.0,
            i % 5,
            i % 7,
            i % 9,
            1 + i % 4,
            i % 4,
            1 + i % 3
        ));
    }
    b.expect("bulk", &bulk, |o| text_has(o, "300 bulk actions done"));

    let s = |body: &str| format!("GET /{ix}/_search\n{body}");
    b.expect("match_all paged", &s("{}"), |o| { let d = docs(o)?; if d.docs.len() == 50 && d.total == Some(300) && d.more { Ok(()) } else { Err(format!("{} {:?} {}", d.docs.len(), d.total, d.more)) } });
    b.expect("bool must/filter/should/must_not", &s(r#"{"query": {"bool": {"must": [{"match": {"name": "ada"}}], "filter": [{"term": {"status": "paid"}}], "should": [{"range": {"total": {"gte": 100}}}], "must_not": [{"term": {"tags": "t4"}}]}}}"#), |o| docs(o).map(|_| ()));
    b.expect("range with date math", &s(r#"{"query": {"range": {"created": {"gte": "now-7d/d", "lt": "now/d"}}}, "sort": [{"created": "desc"}]}"#), |o| { let d = docs(o)?; if !d.docs.is_empty() { Ok(()) } else { Err("none in the last week".into()) } });
    b.expect("range with a date format", &s(r#"{"query": {"range": {"created": {"gte": "01/01/2000", "format": "dd/MM/yyyy"}}}}"#), |o| docs(o).map(|_| ()));
    b.expect("geo_distance", &s(r#"{"query": {"bool": {"filter": {"geo_distance": {"distance": "30km", "loc": {"lat": 38.71, "lon": -9.14}}}}}, "sort": [{"_geo_distance": {"loc": {"lat": 38.71, "lon": -9.14}, "order": "asc", "unit": "km"}}]}"#), |o| { let d = docs(o)?; if !d.docs.is_empty() { Ok(()) } else { Err("no hit near Lisbon".into()) } });
    b.expect("geo_bounding_box", &s(r#"{"query": {"geo_bounding_box": {"loc": {"top_left": {"lat": 40, "lon": -10}, "bottom_right": {"lat": 38, "lon": -8}}}}}"#), |o| docs(o).map(|_| ()));
    b.expect("nested query", &s(r#"{"query": {"nested": {"path": "items", "query": {"bool": {"must": [{"term": {"items.sku": "A1"}}, {"range": {"items.qty": {"gte": 2}}}]}}}}}"#), |o| docs(o).map(|_| ()));
    b.expect("highlight", &s(r#"{"query": {"match": {"name": "grace"}}, "highlight": {"fields": {"name": {}}}}"#), |o| text_has(o, "<em>Grace</em>"));
    b.expect("script_fields", &s(r#"{"query": {"term": {"status": "paid"}}, "script_fields": {"double": {"script": {"source": "doc['total'].value * 2"}}}, "size": 3}"#), |o| { text_has(o, "\"_fields\"")?; text_has(o, "double") });
    b.expect("_source filtering", &s(r#"{"_source": ["name", "total"], "size": 2}"#), |o| { let d = docs(o)?; if d.source.is_none() && d.docs.len() == 2 { Ok(()) } else { Err("editable or wrong size".into()) } });
    b.expect("search_after", &s(r#"{"size": 5, "sort": [{"total": "asc"}, {"created": "asc"}], "search_after": [10.0, 0]}"#), |o| { n_docs(o, 5)?; text_has(o, "_sort") });
    b.expect("query string in the URL", &format!("GET /{ix}/_search?q=status:paid&size=4"), |o| n_docs(o, 4));
    b.expect("multi_match fuzzy", &s(r#"{"query": {"multi_match": {"query": "lovelac", "fields": ["name"], "fuzziness": "AUTO"}}}"#), |o| docs(o).map(|_| ()));
    b.expect("sort by script", &s(r#"{"size": 2, "sort": {"_script": {"type": "number", "script": "doc['total'].value % 7", "order": "desc"}}}"#), |o| n_docs(o, 2));
    b.expect("aggs: terms + avg + date_histogram + percentiles", &s(r#"{"size": 0, "aggs": {"by_status": {"terms": {"field": "status"}, "aggs": {"avg_total": {"avg": {"field": "total"}}, "per_day": {"date_histogram": {"field": "created", "calendar_interval": "day"}}}}, "p": {"percentiles": {"field": "total", "percents": [50, 95, 99]}}}}"#), |o| { let v = json_of(o)?; if v["aggregations"]["by_status"]["buckets"].as_array().map(Vec::len) == Some(4) && v.as_object().and_then(|m| m.keys().next()).map(String::as_str) == Some("aggregations") { Ok(()) } else { Err(v.to_string().chars().take(300).collect()) } });
    b.expect("aggs: composite paging", &s(r#"{"size": 0, "aggs": {"c": {"composite": {"size": 3, "sources": [{"s": {"terms": {"field": "status"}}}, {"t": {"terms": {"field": "tags"}}}]}}}}"#), |o| { let v = json_of(o)?; if v["aggregations"]["c"]["after_key"].is_object() { Ok(()) } else { Err(v.to_string()) } });
    b.expect("aggs: nested + reverse_nested", &s(r#"{"size": 0, "aggs": {"it": {"nested": {"path": "items"}, "aggs": {"skus": {"terms": {"field": "items.sku"}, "aggs": {"back": {"reverse_nested": {}, "aggs": {"st": {"terms": {"field": "status"}}}}}}}}}}"#), |o| text_has(o, "skus"));
    b.expect("aggs: geohash_grid + geo_bounds", &s(r#"{"size": 0, "aggs": {"grid": {"geohash_grid": {"field": "loc", "precision": 3}}, "box": {"geo_bounds": {"field": "loc"}}}}"#), |o| text_has(o, "bounds"));
    b.expect("aggs with hits", &s(r#"{"size": 2, "aggs": {"s": {"terms": {"field": "status"}}}}"#), |o| text_has(o, "buckets"));
    b.expect("_count", &format!("GET /{ix}/_count\n{{\"query\": {{\"term\": {{\"status\": \"paid\"}}}}}}"), |o| { let v = json_of(o)?; if v["count"] == 75 { Ok(()) } else { Err(v.to_string()) } });
    b.expect("_mapping", &format!("GET /{ix}/_mapping"), |o| text_has(o, "geo_point"));
    b.expect("_cat as text", "GET _cat/indices?v", |o| text_has(o, ix));
    b.expect("_cat as JSON", "GET _cat/indices?format=json", |o| text_has(o, ix));
    b.ok("cluster health", "GET _cluster/health");
    b.ok("_msearch", &format!("GET /{ix}/_msearch\n{{}}\n{{\"query\": {{\"term\": {{\"status\": \"new\"}}}}, \"size\": 1}}\n{{}}\n{{\"size\": 0}}"));
    b.ok("_mget", &format!("GET /{ix}/_mget\n{{\"ids\": [\"1\", \"2\"]}}"));
    b.ok("validate", &format!("GET /{ix}/_validate/query?explain=true\n{{\"query\": {{\"match\": {{\"name\": \"ada\"}}}}}}"));
    b.ok("explain a hit", &format!("GET /{ix}/_explain/1\n{{\"query\": {{\"match\": {{\"name\": \"ada\"}}}}}}"));
    b.ok("analyze", "GET _analyze\n{\"analyzer\": \"standard\", \"text\": \"Ada Lovelace\"}");
    b.ok("field caps", &format!("GET /{ix}/_field_caps?fields=*"));
    let sql = if os { "POST _plugins/_sql\n{\"query\": \"SELECT status, count(*) FROM dataman_battery GROUP BY status\"}" } else { "POST _sql?format=json\n{\"query\": \"SELECT status, count(*) FROM dataman_battery GROUP BY status\"}" };
    match b.c.engine() {
        _ => {
            let class = dataman_core::nosql::classify(engine, sql);
            if !matches!(class, Ok(dataman_core::guard::WriteClass::Read)) {
                b.failures.push(format!("SQL is a read: classified {class:?}"));
            }
        }
    }
    b.expect("SQL", sql, |o| text_has(o, "paid"));
    if os {
        b.expect("PPL", &format!("POST _plugins/_ppl\n{{\"query\": \"source={ix} | stats count() by status\"}}"), |o| text_has(o, "paid"));
    } else {
        b.expect("ES|QL", &format!("POST _query?format=json\n{{\"query\": \"FROM {ix} | STATS n = COUNT(*) BY status\"}}"), |o| text_has(o, "paid"));
        b.ok("point in time", &format!("POST /{ix}/_pit?keep_alive=1m"));
    }
    // Writes.
    b.expect("update a document", &format!("POST /{ix}/_update/1?refresh=true\n{{\"doc\": {{\"status\": \"returned\"}}}}"), |o| text_has(o, "updated"));
    b.expect("update by query with a script", &format!("POST /{ix}/_update_by_query?refresh=true\n{{\"query\": {{\"term\": {{\"status\": \"new\"}}}}, \"script\": {{\"source\": \"ctx._source.total += 1\"}}}}"), |o| text_has(o, "75 documents updated"));
    b.expect("delete by query", &format!("POST /{ix}/_delete_by_query?refresh=true\n{{\"query\": {{\"term\": {{\"status\": \"refunded\"}}}}}}"), |o| text_has(o, "deleted"));
    b.expect("index a document with an id", &format!("PUT /{ix}/_doc/new-1?refresh=true\n{{\"name\": \"new\", \"status\": \"new\"}}"), |o| text_has(o, "created"));
    b.expect("delete a document", &format!("DELETE /{ix}/_doc/new-1"), |o| text_has(o, "deleted"));
    b.ok("aliases", &format!("POST _aliases\n{{\"actions\": [{{\"add\": {{\"index\": \"{ix}\", \"alias\": \"{ix}_alias\"}}}}]}}"));
    b.expect("search through an alias", &format!("GET /{ix}_alias/_search\n{{\"size\": 1}}"), |o| n_docs(o, 1));
    b.expect("search every index", "GET /_search\n{\"size\": 1, \"query\": {\"term\": {\"status\": \"paid\"}}}", |o| n_docs(o, 1));
    b.expect("a missing index says so", "GET /dataman_nope/_search", |_| Err("it ran".into()));
    b.failures.retain(|f| !f.starts_with("a missing index says so: no such index") && !f.contains("index_not_found_exception"));
    b.ok("cleanup", &format!("DELETE /{ix}"));
    b.finish(engine.label());
}

#[test]
fn elasticsearch_battery() {
    search_battery(Engine::Elasticsearch);
}

#[test]
fn opensearch_battery() {
    search_battery(Engine::OpenSearch);
}

// ---------------------------------------------------------------------------------------------
// Redis

#[test]
fn redis_battery() {
    let e = env();
    let v = std::env::var("DATAMAN_REDIS").unwrap_or_else(|_| "127.0.0.1:63790".into());
    let (host, port) = v.rsplit_once(':').unwrap();
    let p = ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: String::new(), database: "10".into(), sslmode: SslMode::Disable, engine: Engine::Redis, ..ConnectionProfile::new("redis") };
    let pw = std::env::var("DATAMAN_REDIS_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let Ok(c) = e.core.block_on(e.core.connect(p, Some(pw))) else {
        eprintln!("skipping: no Redis");
        return;
    };
    let mut b = Battery { e: &e, c, db: "10".into(), failures: Vec::new(), ran: 0 };
    b.ok("flush", "FLUSHDB");

    // Strings, counters, expiry, binary.
    b.expect("SET EX", "SET session:1 \"{\\\"user\\\": 1}\" EX 3600", |o| text_has(o, "OK"));
    b.expect("TTL", "TTL session:1", |o| { let v = json_of(o)?; if v.as_i64().is_some_and(|t| t > 3500) { Ok(()) } else { Err(v.to_string()) } });
    b.ok("PERSIST", "PERSIST session:1");
    b.expect("INCRBY", "INCRBY counter 41", |o| text_has(o, "41"));
    b.expect("INCRBYFLOAT", "INCRBYFLOAT price 1.5", |o| text_has(o, "1.5"));
    b.ok("binary value", "SET bin \"\\x00\\x01\\xff\\xfehello\"");
    b.expect("a binary value reads as redis-cli shows it, not editable", "GET bin", |o| { text_has(o, r"\\x00\\x01\\xff\\xfehello")?; if docs(o)?.source.is_none() { Ok(()) } else { Err("editable".into()) } });
    b.expect("the bytes went as typed", "STRLEN bin", |o| text_has(o, "9"));
    b.ok("unicode", "SET greeting \"olá 🙂 日本\"");
    b.expect("unicode reads back", "GET greeting", |o| text_has(o, "olá 🙂 日本"));
    b.ok("big string", &format!("SET big {}", "x".repeat(500_000)));
    b.expect("STRLEN", "STRLEN big", |o| text_has(o, "500000"));
    b.expect("GETRANGE", "GETRANGE big 0 9", |o| text_has(o, "xxxxxxxxxx"));
    b.ok("MSET", "MSET a 1 b 2 c 3");
    b.expect("MGET", "MGET a b c nope", |o| { let v = json_of(o)?; if v == serde_json::json!(["1", "2", "3", null]) { Ok(()) } else { Err(v.to_string()) } });
    // A huge hash, list, set, sorted set: paged entries.
    let mut h = String::from("HSET huge");
    for i in 0..3000 {
        h.push_str(&format!(" f{i} v{i}"));
    }
    b.expect("a 3000-field hash", &h, |o| text_has(o, "3000"));
    b.expect("its entries page", "HGETALL huge", |o| { let d = docs(o)?; if d.docs.len() == 50 && d.total == Some(3000) && d.more { Ok(()) } else { Err(format!("{} {:?}", d.docs.len(), d.total)) } });
    b.ok("list", &format!("RPUSH q {}", (0..500).map(|i| format!("job{i}")).collect::<Vec<_>>().join(" ")));
    b.expect("LRANGE view", "LRANGE q 0 -1", |o| { let d = docs(o)?; if d.total == Some(500) { Ok(()) } else { Err(format!("{:?}", d.total)) } });
    b.expect("LRANGE part", "LRANGE q 10 12", |o| { let v = json_of(o)?; if v == serde_json::json!(["job10", "job11", "job12"]) { Ok(()) } else { Err(v.to_string()) } });
    b.ok("LPOS", "LPOS q job42");
    b.ok("set", "SADD s a b c d e");
    b.ok("SINTER", "SINTER s s");
    b.ok("SISMEMBER", "SISMEMBER s a");
    b.ok("zset", "ZADD board 10 ada 20 grace 15 alan 5 linus");
    b.expect("ZRANGEBYSCORE", "ZRANGEBYSCORE board 10 20 WITHSCORES", |o| text_has(o, "grace"));
    b.expect("ZREVRANGE", "ZREVRANGE board 0 1 WITHSCORES", |o| text_has(o, "grace"));
    b.expect("ZRANGE BYSCORE REV LIMIT", "ZRANGE board +inf -inf BYSCORE REV LIMIT 0 2", |o| text_has(o, "grace"));
    b.expect("ZRANK", "ZRANK board alan", |o| text_has(o, "2"));
    b.expect("ZINCRBY", "ZINCRBY board 100 linus", |o| text_has(o, "105"));
    // Streams.
    b.expect("XADD", "XADD events * type login user 1", |o| text_has(o, "-"));
    b.ok("XADD more", "XADD events * type logout user 1");
    b.expect("XRANGE", "XRANGE events - + COUNT 10", |o| text_has(o, "login"));
    b.expect("XLEN", "XLEN events", |o| text_has(o, "2"));
    b.ok("XINFO", "XINFO STREAM events");
    b.ok("XGROUP", "XGROUP CREATE events g1 0");
    b.ok("XINFO GROUPS", "XINFO GROUPS events");
    // Geo.
    b.ok("GEOADD", "GEOADD places -9.2160 38.6916 belem -9.1366 38.7075 comercio -8.6146 41.1456 clerigos");
    b.expect("GEOSEARCH", "GEOSEARCH places FROMLONLAT -9.14 38.71 BYRADIUS 20 km ASC WITHDIST", |o| { text_has(o, "comercio")?; text_has(o, "belem") });
    b.expect("GEODIST", "GEODIST places belem clerigos km", |o| text_has(o, "27"));
    b.ok("GEOPOS", "GEOPOS places belem");
    b.ok("GEOHASH", "GEOHASH places belem");
    // HyperLogLog, bitmaps.
    b.ok("PFADD", "PFADD visitors a b c a");
    b.expect("PFCOUNT", "PFCOUNT visitors", |o| text_has(o, "3"));
    b.ok("SETBIT", "SETBIT bits 7 1");
    b.expect("BITCOUNT", "BITCOUNT bits", |o| text_has(o, "1"));
    // Keys: patterns, types, memory, encoding.
    for i in 0..300 {
        b.ok("many keys", &format!("SET user:{i}:name n{i}"));
    }
    b.expect("SCAN MATCH COUNT", "SCAN 0 MATCH user:1* COUNT 1000", |o| text_has(o, "user:1"));
    b.expect("KEYS pattern", "KEYS user:2?:name", |o| text_has(o, "user:2"));
    b.expect("TYPE", "TYPE huge", |o| text_has(o, "hash"));
    b.ok("OBJECT ENCODING", "OBJECT ENCODING huge");
    b.ok("MEMORY USAGE", "MEMORY USAGE huge");
    b.ok("DBSIZE", "DBSIZE");
    b.expect("INFO reads as text", "INFO keyspace", |o| text_has(o, "# Keyspace\r\n"));
    b.ok("CONFIG GET", "CONFIG GET maxmemory");
    b.ok("CLIENT LIST", "CLIENT LIST");
    b.ok("SLOWLOG", "SLOWLOG GET 5");
    b.ok("TIME", "TIME");
    b.ok("lowercase", "hgetall huge");
    b.ok("EXPIRE", "EXPIRE big 100");
    b.ok("RENAME", "RENAME big big2");
    b.ok("COPY", "COPY counter counter2");
    b.expect("DEL several", "DEL a b c", |o| text_has(o, "3"));
    b.expect("the explorer lists keys with types", "SCAN 0", |_| Ok(()));
    let keys = b.e.core.block_on(b.c.nosql_tree(Some("10"))).unwrap_or_default();
    if !keys.iter().any(|k| k.name == "events" && k.note.as_deref() == Some("stream")) {
        b.failures.push(format!("the stream is not listed: {} keys", keys.len()));
    }
    // Not here: modules, blocking, stateful commands.
    b.refused("JSON without the module", "JSON.GET doc", "unknown command");
    b.refused("blocking", "BLPOP q 1", "not supported");
    b.refused("MULTI", "MULTI", "not supported");
    b.refused("SUBSCRIBE", "SUBSCRIBE ch", "not supported");
    b.ok("flush", "FLUSHDB");
    b.finish("Redis");
}
