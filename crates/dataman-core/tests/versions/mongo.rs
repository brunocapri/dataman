//! See live_versions.rs. MongoDB and FerretDB: each release's aggregation stages and operators
//! answer with the right values or fail with the server's own message, odd BSON values show as
//! the shell writes them and save back unchanged, and the tree tells what each collection is.

use super::*;
use dataman_core::nosql::bson::Bson;
use dataman_core::nosql::{Body, ItemKind, Source};

/// The server, on an empty database of the test's own (the tests run at once). FerretDB 1 (past
/// its end of life) is only told apart: it refuses dotted keys and drops the connection on a
/// document of odd values, its own failures.
fn mongo(db: &str) -> Option<Target> {
    let t = target(Engine::MongoDb)?;
    if t.c.info.version.starts_with("FerretDB 1.") {
        eprintln!("skipping: {}", t.c.info.version);
        return None;
    }
    t.nosql_ok(db, "db.dropDatabase()");
    Some(t)
}

fn since(t: &Target, release: (u32, u32)) -> bool {
    t.c.info.release >= (release.0, release.1, 0)
}

fn is_mongodb(t: &Target) -> bool {
    t.c.info.flavor.is_standard()
}

fn docs(o: Output) -> dataman_core::nosql::Docs {
    match o {
        Output::Docs(d) => d,
        o => panic!("not documents: {o:?}"),
    }
}

/// The documents as Dataman prints them, without the whitespace (the values here have none).
fn shown(o: &Output) -> String {
    let all = match o {
        Output::Docs(d) => d.docs.iter().map(|d| d.body.text()).collect::<Vec<_>>().join(","),
        Output::Value(b) => b.text(),
        o => panic!("{o:?}"),
    };
    format!("[{}]", all.split_whitespace().collect::<String>())
}

/// What each release added to aggregation: (the release, its stage or operator, the pipeline on
/// `n`, what Dataman shows). Before its release the server must say which one it does not know.
const STAGES: &[((u32, u32), &str, &str, &str)] = &[
    ((3, 4), "$bucketAuto", "[{$facet: {b: [{$bucketAuto: {groupBy: '$v', buckets: 2}}, {$sort: {'_id.min': 1}}], n: [{$count: 'n'}]}}]", r#"[{"b":[{"_id":{"min":1,"max":5},"count":2},{"_id":{"min":5,"max":10},"count":2}],"n":[{"n":4}]}]"#),
    ((3, 6), "$lookup", "[{$match: {_id: 1}}, {$lookup: {from: 'n', let: {g: '$g'}, pipeline: [{$match: {$expr: {$eq: ['$g', '$$g']}}}, {$sort: {_id: 1}}, {$project: {_id: 1}}], as: 'same'}}, {$project: {same: 1}}]", r#"[{"_id":1,"same":[{"_id":1},{"_id":2}]}]"#),
    ((3, 4), "$graphLookup", "[{$match: {_id: 4}}, {$graphLookup: {from: 'n', startWith: '$up', connectFromField: 'up', connectToField: '_id', as: 'chain'}}, {$project: {n: {$size: '$chain'}}}]", r#"[{"_id":4,"n":3}]"#),
    ((5, 0), "$setWindowFields", "[{$setWindowFields: {partitionBy: '$g', sortBy: {at: 1}, output: {run: {$sum: '$v', window: {documents: ['unbounded', 'current']}}}}}, {$sort: {_id: 1}}, {$project: {run: 1}}]", r#"[{"_id":1,"run":1},{"_id":2,"run":6},{"_id":3,"run":3},{"_id":4,"run":13}]"#),
    ((5, 0), "$dateTrunc", "[{$match: {_id: 4}}, {$project: {w: {$dateTrunc: {date: '$at', unit: 'week'}}, d: {$dateDiff: {startDate: ISODate('2024-01-01'), endDate: '$at', unit: 'day'}}}}]", r#"[{"_id":4,"w":ISODate("2024-01-07T00:00:00.000Z"),"d":NumberLong(6)}]"#),
    ((5, 0), "$getField", "[{$match: {_id: 1}}, {$project: {dot: {$getField: 'a.b'}, dollar: {$getField: {$literal: '$p'}}}}]", r#"[{"_id":1,"dot":"dotted","dollar":"price"}]"#),
    ((5, 1), "$densify", "[{$densify: {field: 'at', range: {step: 1, unit: 'day', bounds: 'full'}}}, {$count: 'n'}]", r#"[{"n":7}]"#),
    ((5, 2), "$top", "[{$group: {_id: '$g', top: {$top: {sortBy: {v: -1}, output: '$_id'}}, last2: {$lastN: {input: '$v', n: 2}}}}, {$sort: {_id: 1}}]", r#"[{"_id":"a","top":2,"last2":[1,5]},{"_id":"b","top":4,"last2":[3,10]}]"#),
    ((5, 2), "$sortArray", "[{$match: {_id: 1}}, {$project: {s: {$sortArray: {input: [3, 1, 2], sortBy: -1}}}}]", r#"[{"_id":1,"s":[3,2,1]}]"#),
    ((5, 3), "$fill", "[{$fill: {sortBy: {_id: 1}, output: {up: {method: 'locf'}, v: {value: 0}}}}, {$sort: {_id: 1}}, {$project: {up: 1}}]", r#"[{"_id":1,"up":null},{"_id":2,"up":1},{"_id":3,"up":2},{"_id":4,"up":3}]"#),
    ((7, 0), "$median", "[{$match: {_id: {$lte: 3}}}, {$group: {_id: null, m: {$median: {input: '$v', method: 'approximate'}}, p: {$percentile: {input: '$v', p: [0.5, 1], method: 'approximate'}}}}]", r#"[{"_id":null,"m":3.0,"p":[3.0,5.0]}]"#),
];

#[test]
fn each_release_stages_answer_or_fail_clean() {
    const DB: &str = "dataman_v_stages";
    let Some(t) = mongo(DB) else { return };
    // A field named `$p` is stored from 5.0 on.
    let dollar = if since(&t, (5, 0)) { ", $p: 'price'" } else { "" };
    t.nosql_ok(DB, &format!("db.n.insertMany([{{_id: 1, g: 'a', v: 1, at: ISODate('2024-01-01'), 'a.b': 'dotted'{dollar}}}, {{_id: 2, g: 'a', v: 5, at: ISODate('2024-01-03'), up: 1}}, {{_id: 3, g: 'b', v: 3, at: ISODate('2024-01-04'), up: 2}}, {{_id: 4, g: 'b', v: 10, at: ISODate('2024-01-07'), up: 3}}])"));
    let mut wrong = Vec::new();
    for (release, op, pipeline, want) in STAGES {
        let text = format!("db.n.aggregate({pipeline})");
        match t.nosql(DB, &text) {
            Ok(o) if shown(&o) == *want => {}
            Ok(o) => wrong.push(format!("{op}: shows {}, not {want}", shown(&o))),
            // MongoDB itself has it from its release; a flavor that answers as that release may
            // still lack it, and must say so.
            Err(e) if since(&t, *release) && is_mongodb(&t) => wrong.push(format!("{op}: {e}")),
            Err(e) if !e.contains(op) => wrong.push(format!("{op}: fails without naming it: {e}")),
            Err(_) => {}
        }
    }
    assert!(wrong.is_empty(), "{}:\n{}", t.c.info.version, wrong.join("\n"));
}

/// Every BSON value the shell can write, as the server keeps it.
const ODD: &str = r#"{
  _id: 'odd',
  dec: [NumberDecimal('NaN'), NumberDecimal('-Infinity'), NumberDecimal('-0'), NumberDecimal('1E+6111'), NumberDecimal('0.000'), NumberDecimal('-1E-6176')],
  long: [NumberLong('-9223372036854775808'), NumberLong('9223372036854775807')],
  int: [NumberInt(-2147483648), NumberInt(2147483647)],
  dbl: [1e300, 5e-324, -0.0, NaN, -Infinity, 0.1, 9007199254740993.0],
  dates: [ISODate('1969-12-31T23:59:59.999Z'), ISODate('0001-01-01T00:00:00Z'), new Date(253402300800000), new Date(-62198755200000), new Date(8640000000000000)],
  ts: [Timestamp(0, 0), Timestamp(4294967295, 4294967295)],
  bin: [BinData(0, ''), BinData(2, 'AQID'), BinData(3, 'EjRWeJCrze8SNFZ4kKvN7w=='), UUID('00000000-0000-0000-0000-000000000000'), BinData(5, 'EjRWeJCrze8SNFZ4kKvN7w=='), BinData(128, '/w==')],
  re: [/a\/b/imsx, RegExp('a/b'), /\d+$/],
  str: ['', 'tab\tnew\nline', 'nul\u0000inside', 'quote " and \\ back', '日本語 🚀 עברית'],
  keys: {'': 'empty', 'a.b': 'dot', ' ': 'space'},
  min: MinKey(), max: MaxKey(), nil: null, code: Code('function () { return 1 }')
}"#;

#[test]
fn odd_values_show_as_written_and_save_back_unchanged() {
    const DB: &str = "dataman_v_odd";
    let Some(t) = mongo(DB) else { return };
    t.nosql_ok(DB, &format!("db.odd.insertOne({ODD})"));
    let read = || docs(t.nosql_ok(DB, "db.odd.find({_id: 'odd'})")).docs.remove(0);
    let doc = read();
    let Body::Bson(Bson::Document(kept)) = &doc.body else { panic!("{:?}", doc.body) };
    let text = doc.body.text();
    for s in [
        "NumberDecimal(\"NaN\")", "NumberDecimal(\"-Infinity\")", "NumberDecimal(\"-0\")", "NumberDecimal(\"1E+6111\")", "NumberDecimal(\"0.000\")",
        "NumberLong(-9223372036854775808)", "NumberLong(9223372036854775807)", "-2147483648",
        "1e+300", "5e-324", "-0.0", "NaN", "-Infinity", "0.1",
        "ISODate(\"1969-12-31T23:59:59.999Z\")", "ISODate(\"0001-01-01T00:00:00.000Z\")", "ISODate(\"+010000-01-01T00:00:00.000Z\")", "ISODate(\"-000001-01-01T00:00:00.000Z\")", "ISODate(8640000000000000)",
        "RegExp(\"a/b\", \"\")", "/a\\/b/imsx",
        "Timestamp(4294967295, 4294967295)", "BinData(0, \"\")", "BinData(3, \"EjRWeJCrze8SNFZ4kKvN7w==\")", "UUID(\"00000000-0000-0000-0000-000000000000\")", "BinData(128, \"/w==\")",
        "MinKey()", "MaxKey()", "\"nul\\u0000inside\"", "\"日本語 🚀 עברית\"", "\"a.b\": \"dot\"", "\"\": \"empty\"",
    ] {
        assert!(text.contains(s), "{s} not shown in:\n{text}");
    }
    // What the document view shows is what its editor saves: saved back as shown, it is the same
    // document (canonical extended JSON, where NaN equals NaN).
    let source = Source::Mongo { db: DB.into(), coll: "odd".into() };
    t.e.core.block_on(t.c.save_document(&source, doc.id.clone(), &text)).unwrap_or_else(|e| panic!("saving it back: {e:#}\n{text}"));
    let Body::Bson(again) = read().body else { panic!() };
    assert_eq!(again.into_canonical_extjson(), Bson::Document(kept.clone()).into_canonical_extjson(), "changed by a save of what was shown:\n{text}");
}

/// The limits are the server's, said in its words: nothing is cut or shown half.
#[test]
fn deep_wide_and_huge_documents() {
    const DB: &str = "dataman_v_deep";
    let Some(t) = mongo(DB) else { return };
    let deep = |n: usize| format!("{}1{}", "{a: ".repeat(n), "}".repeat(n));
    t.nosql_ok(DB, &format!("db.deep.insertOne({{_id: 1, d: {}}})", deep(150)));
    let d = docs(t.nosql_ok(DB, "db.deep.find()")).docs.remove(0);
    assert_eq!(d.body.node().count(), 2 + 150, "every level is in the tree");
    let source = Source::Mongo { db: DB.into(), coll: "deep".into() };
    t.e.core.block_on(t.c.save_document(&source, d.id.clone(), &d.body.text())).unwrap_or_else(|e| panic!("saving it back: {e:#}"));
    // As deep as Dataman reads, through the shell's JavaScript.
    let e = t.nosql(DB, &format!("const d = {}\ndb.deep.insertOne({{_id: 3, d}})", deep(199))).unwrap_err().to_lowercase();
    assert!(e.contains("nest") || e.contains("depth"), "{e}");
    // Deeper than MongoDB reads: refused in words, never a crash (a stack overflow aborts the
    // whole app).
    for n in [250, 1000] {
        for text in [
            format!("db.deep.insertOne({{_id: 2, d: {}}})", deep(n)),
            format!("db.deep.find({{d: {}}})", deep(n)),
            format!("db.deep.insertOne({{_id: 2, d: {}1{}}})", "[".repeat(n), "]".repeat(n)),
            format!("const d = {}\ndb.deep.insertOne({{_id: 2, d}})", deep(n)),
        ] {
            let e = t.nosql(DB, &text).unwrap_err().to_lowercase();
            assert!(e.contains("nest") || e.contains("depth"), "{n}: {e}");
        }
    }
    let e = t.e.core.block_on(t.c.save_document(&source, None, &format!("{{d: {}}}", deep(1000)))).unwrap_err();
    assert!(format!("{e:#}").contains("nested more than"), "{e:#}");

    t.nosql_ok(DB, &format!("db.wide.insertOne({{_id: 1, a: [{}]}})", (0..100_000).map(|i| i.to_string()).collect::<Vec<_>>().join(",")));
    let a = docs(t.nosql_ok(DB, "db.wide.find()")).docs.remove(0).body.node();
    assert_eq!(a.children[1].children.len(), 100_000);
    assert_eq!(a.children[1].children[99_999].text, "99999");
    let big = t.nosql(DB, &format!("db.wide.insertOne({{_id: 2, s: '{}'}})", "x".repeat(17 << 20))).unwrap_err();
    assert!(big.contains("16777216 bytes") && !big.contains("Kind:"), "{big}");
}

/// A refused write says what the server said, not the driver's dump of its reply.
#[test]
fn refusals_in_the_servers_words() {
    const DB: &str = "dataman_v_refusals";
    let Some(t) = mongo(DB) else { return };
    t.nosql_ok(DB, "db.dup.insertOne({_id: 1})");
    for text in ["db.dup.insertOne({_id: 1})", "db.dup.insertMany([{_id: 2}, {_id: 1}, {_id: 3}])"] {
        let e = t.nosql(DB, text).unwrap_err();
        assert!(e.to_lowercase().contains("duplicate key") && !e.contains("Kind:") && !e.contains("IndexedWriteError"), "{text}: {e}");
    }
    let e = t.nosql(DB, "db.dup.aggregate([{$group: {_id: '$x', n: {$bogus: 1}}}])").unwrap_err();
    assert!(e.contains("$bogus") && !e.contains("Kind:"), "{e}");
}

#[test]
fn pages_of_a_find_and_an_aggregation() {
    const DB: &str = "dataman_v_pages";
    let Some(t) = mongo(DB) else { return };
    t.nosql_ok(DB, &format!("db.p.insertMany([{}])", (0..130).map(|i| format!("{{_id: {i}}}")).collect::<Vec<_>>().join(",")));
    let page = |text: &str, skip: u64| docs(t.nosql_page(DB, text, Page { skip, limit: 50 }).unwrap_or_else(|e| panic!("{text}: {e}")).output);
    let ids = |d: &dataman_core::nosql::Docs| d.docs.iter().map(|d| d.body.text().split_whitespace().collect::<String>()).collect::<Vec<_>>();
    for text in ["db.p.find().sort({_id: 1}).skip(5).limit(70)", "db.p.aggregate([{$sort: {_id: 1}}, {$skip: 5}, {$limit: 70}])"] {
        let (a, b) = (page(text, 0), page(text, 50));
        assert_eq!((a.docs.len(), a.more, b.docs.len(), b.more), (50, true, 20, false), "{text}");
        assert_eq!((ids(&a)[0].as_str(), ids(&b)[19].as_str()), ("{\"_id\":5}", "{\"_id\":74}"), "{text}");
    }
    assert_eq!(page("db.p.find().sort({_id: 1}).skip(5).limit(70)", 0).total, Some(70));
}

#[test]
fn the_tree_tells_what_each_collection_is() {
    const DB: &str = "dataman_v_tree";
    let Some(t) = mongo(DB) else { return };
    t.nosql_ok(DB, "db.plain.insertMany([{a: 1}, {a: 2}, {a: 3}])");
    let mut want = vec![("plain", ItemKind::Collection, Some(3))];
    // FerretDB makes none of these (Dataman refuses them there: `FlavorSpec::ignores`).
    if is_mongodb(&t) {
        t.nosql_ok(DB, "db.createView('plain_view', 'plain', [{$match: {a: {$gt: 1}}}])");
        assert_eq!(docs(t.nosql_ok(DB, "db.plain_view.find()")).docs.len(), 2);
        want.push(("plain_view", ItemKind::View, None));
        t.nosql_ok(DB, "db.createCollection('capped', {capped: true, size: 4096, max: 2})");
        t.nosql_ok(DB, "db.capped.insertMany([{i: 1}, {i: 2}, {i: 3}])");
        want.push(("capped", ItemKind::Collection, Some(2)));
    }
    if is_mongodb(&t) && since(&t, (5, 0)) {
        t.nosql_ok(DB, "db.createCollection('ticks', {timeseries: {timeField: 't', metaField: 'sym'}})");
        t.nosql_ok(DB, "db.ticks.insertMany([{t: ISODate('2024-01-01'), sym: 'A', px: 1}, {t: ISODate('2024-01-02'), sym: 'B', px: 2}])");
        want.push(("ticks", ItemKind::Collection, Some(2)));
    }
    let items = t.e.core.block_on(t.c.nosql_tree(Some(DB))).unwrap();
    for (name, kind, count) in want {
        let i = items.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{name} not in {items:?}"));
        assert_eq!((i.kind, i.count), (kind, count), "{name}: {i:?}");
    }
}
