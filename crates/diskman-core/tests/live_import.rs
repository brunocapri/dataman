//! Importing files into real servers: CSV, TSV, NDJSON and Parquet into existing and new tables
//! (PostgreSQL, in the scratch schema `diskman_import`, dropped at the end), the line of a bad
//! row, skipping bad rows, cancelling mid-way, truncate first, the read-only refusal, a 1 M-row
//! CSV through COPY (throughput printed) and the `INSERT` fallback on PostgreSQL and on LAR.
//! Skips without servers.
//!
//! `cargo test --release -p diskman-core --test live_import -- --nocapture` prints the rates.

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use common::*;
use diskman_core::config::{ConnectionProfile, SslMode};
use diskman_core::import::{
    self, ColumnMap, ImportOptions, ImportReport, Method, OnError, Outcome, PgType, Preview, auto_map,
};

fn import_into(e: &Env, c: &Arc<Connection>, path: &Path, opts: ImportOptions) -> ImportReport {
    let imp = c.import(None, path, opts);
    e.core.block_on(async { tokio::time::timeout(Duration::from_secs(600), imp.task).await.expect("timeout") }).expect("import task")
}

fn preview(path: &Path) -> Preview {
    import::preview(path, None, None).expect("preview")
}

/// Every column into a new table, with the inferred types.
fn new_table(pv: &Preview, schema: &str, table: &str) -> ImportOptions {
    ImportOptions {
        format: pv.format,
        dialect: pv.dialect.clone(),
        source_names: pv.columns.iter().map(|c| c.name.clone()).collect(),
        schema: schema.into(),
        table: table.into(),
        create: true,
        columns: pv.columns.iter().enumerate().map(|(i, c)| ColumnMap { source: i, target: c.name.clone(), type_name: c.ty.sql().into() }).collect(),
        truncate: false,
        on_error: OnError::Stop,
        one_transaction: true,
        method: Method::Auto,
        insert_rows: 0,
    }
}

/// The file's columns mapped by name onto an existing table's.
fn existing(e: &Env, c: &Arc<Connection>, pv: &Preview, schema: &str, table: &str) -> ImportOptions {
    let cols = e.core.block_on(c.columns(c.database(), schema, table)).expect("columns");
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let map = auto_map(&pv.columns, &names, pv.dialect.header || !pv.format.is_delimited());
    let mut o = new_table(pv, schema, table);
    o.create = false;
    o.columns = map
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.map(|j| ColumnMap { source: i, target: cols[j].name.clone(), type_name: cols[j].type_name.clone() }))
        .collect();
    o
}

fn rows(e: &Env, c: &Arc<Connection>, sql: &str) -> Vec<Vec<Option<String>>> {
    let rs = query(e, c, sql);
    let snap = rs.snapshot();
    (0..snap.rows()).map(|r| (0..rs.columns().unwrap().len()).map(|k| snap.text(r, k).map(|s| s.into_owned())).collect()).collect()
}

fn one(e: &Env, c: &Arc<Connection>, sql: &str) -> String {
    rows(e, c, sql).first().and_then(|r| r[0].clone()).unwrap_or_default()
}

fn write(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// `n` rows of `id,name,price,ok,at,doc` (one bad id at `bad`, if any).
fn big_csv(dir: &Path, n: u64, bad: Option<u64>) -> PathBuf {
    let p = dir.join(format!("big-{n}.csv"));
    let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&p).unwrap());
    writeln!(w, "id,name,price,ok,at,doc").unwrap();
    for i in 1..=n {
        if Some(i) == bad {
            writeln!(w, "x{i},bad,1,true,2024-01-01 00:00:00+00,{{}}").unwrap();
            continue;
        }
        writeln!(w, "{i},\"name {i}, with comma\",{}.{:02},{},2024-01-{:02} 10:{:02}:00+00,\"{{\"\"k\"\": {i}}}\"", i % 1000, i % 100, i % 2 == 0, 1 + i % 28, i % 60).unwrap();
    }
    w.flush().unwrap();
    p
}

fn pg_profile(read_only: bool) -> ConnectionProfile {
    let v = std::env::var("DISKMAN_PG").unwrap_or_else(|_| "127.0.0.1:55432".into());
    let (host, port) = v.rsplit_once(':').unwrap();
    ConnectionProfile {
        host: host.into(),
        port: port.parse().unwrap(),
        user: "postgres".into(),
        database: "diskman".into(),
        sslmode: SslMode::Disable,
        read_only,
        ..ConnectionProfile::new("imp")
    }
}

#[test]
fn files_into_postgres() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec(&e, &c, &[
        "DROP SCHEMA IF EXISTS diskman_import CASCADE",
        "CREATE SCHEMA diskman_import",
        "CREATE TABLE diskman_import.people (id int PRIMARY KEY, name text NOT NULL, born date, score numeric(6,2), note text, created timestamptz DEFAULT now())",
    ]);
    let dir = tempfile::tempdir().unwrap();

    // CSV into an existing table: `;`, quotes, an embedded line break, Latin-1, a column the
    // table does not have, a quoted empty string vs an empty field.
    let mut csv = b"id;Name;born;score;note;extra\n1;\"Smith; Zo\xeb\";1990-05-01;9.5;\"two\nlines\";x\n2;Ana;;;\"\";y\n".to_vec();
    csv.extend_from_slice(b"3;\"say \"\"hi\"\"\";2000-01-31;10;tab\there;z\n");
    let p = write(dir.path(), "people.csv", &csv);
    let pv = preview(&p);
    assert_eq!(pv.dialect.delimiter, b';');
    assert_eq!(pv.dialect.encoding, import::Encoding::Latin1);
    let o = existing(&e, &c, &pv, "diskman_import", "people");
    assert_eq!(o.columns.iter().map(|m| m.target.as_str()).collect::<Vec<_>>(), ["id", "name", "born", "score", "note"]);
    let r = import_into(&e, &c, &p, o);
    assert_eq!(r.outcome, Outcome::Done, "{r:?}");
    assert_eq!((r.rows, r.method), (3, Method::Copy));
    let got = rows(&e, &c, "SELECT id, name, born, score, note, created IS NOT NULL FROM diskman_import.people ORDER BY id");
    assert_eq!(got[0][1].as_deref(), Some("Smith; Zoë"));
    assert_eq!(got[0][4].as_deref(), Some("two\nlines"));
    assert_eq!(got[1][2], None, "empty date → NULL");
    assert_eq!(got[1][4].as_deref(), Some(""), "quoted empty → empty string");
    assert_eq!(got[2][1].as_deref(), Some("say \"hi\""));
    assert_eq!(got[2][4].as_deref(), Some("tab\there"));
    assert_eq!(got[2][5].as_deref(), Some("true"), "defaults fill the columns left out");

    // Truncate first, from TSV (COPY text escapes, \N).
    let p = write(dir.path(), "people.tsv", b"id\tname\tnote\n7\tSeven\tback\\\\slash\n8\tEight\t\\N\n");
    let pv = preview(&p);
    let mut o = existing(&e, &c, &pv, "diskman_import", "people");
    o.truncate = true;
    let r = import_into(&e, &c, &p, o);
    assert_eq!((r.outcome.clone(), r.rows), (Outcome::Done, 2), "{r:?}");
    assert_eq!(rows(&e, &c, "SELECT id, note FROM diskman_import.people ORDER BY id"), vec![
        vec![Some("7".into()), Some("back\\slash".into())],
        vec![Some("8".into()), None]
    ]);

    // NDJSON into a new table: inferred types, nested values as jsonb.
    let nd = "{\"id\": 1, \"name\": \"a\", \"tags\": [\"x\", \"y\"], \"at\": \"2024-03-01T10:00:00Z\", \"ok\": true, \"u\": \"0f8fad5b-d9cb-469f-a165-70867728950e\"}\n\
              {\"id\": 2, \"name\": \"b\", \"tags\": {\"k\": 1}, \"at\": null, \"ok\": false, \"u\": null, \"price\": 2.5}\n";
    let p = write(dir.path(), "events.ndjson", nd.as_bytes());
    let pv = preview(&p);
    assert_eq!(pv.columns.iter().map(|c| c.ty).collect::<Vec<_>>(), [PgType::Int8, PgType::Text, PgType::Jsonb, PgType::TimestampTz, PgType::Bool, PgType::Uuid, PgType::Numeric]);
    let r = import_into(&e, &c, &p, new_table(&pv, "diskman_import", "events"));
    assert_eq!((r.outcome.clone(), r.rows), (Outcome::Done, 2), "{r:?}");
    assert_eq!(one(&e, &c, "SELECT tags->>1 FROM diskman_import.events WHERE id = 1"), "y");
    assert_eq!(one(&e, &c, "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'diskman_import.events'::regclass AND attname = 'at'"), "timestamp with time zone");
    assert_eq!(one(&e, &c, "SELECT (tags->>'k')::int + price FROM diskman_import.events WHERE id = 2"), "3.5");

    // A JSON array into a new table, through INSERT (forced) and per-chunk commits.
    let p = write(dir.path(), "arr.json", b"[{\"a\": 1, \"b\": \"it's\"}, {\"a\": 2, \"b\": null}]");
    let pv = preview(&p);
    let mut o = new_table(&pv, "diskman_import", "arr");
    o.method = Method::Insert;
    o.one_transaction = false;
    let r = import_into(&e, &c, &p, o);
    assert_eq!((r.outcome.clone(), r.rows, r.method), (Outcome::Done, 2, Method::Insert), "{r:?}");
    assert_eq!(one(&e, &c, "SELECT b FROM diskman_import.arr WHERE a = 1"), "it's");

    // Parquet into a new table: Arrow types → PostgreSQL types.
    let p = parquet_file(dir.path());
    let pv = preview(&p);
    let r = import_into(&e, &c, &p, new_table(&pv, "diskman_import", "pq"));
    assert_eq!((r.outcome.clone(), r.rows), (Outcome::Done, 3), "{r:?}");
    assert_eq!(
        rows(&e, &c, "SELECT id, price, (at = '2000-01-01 00:00:01.5+00')::text, raw, tags FROM diskman_import.pq ORDER BY id")[0],
        vec![Some("1".into()), Some("12.50".into()), Some("true".into()), Some("\\xdead".into()), Some("[1, 2]".into())]
    );

    exec(&e, &c, &["DROP SCHEMA diskman_import CASCADE"]);
}

fn parquet_file(dir: &Path) -> PathBuf {
    use arrow_array::builder::{Int32Builder, ListBuilder, TimestampMicrosecondBuilder};
    use arrow_array::{ArrayRef, BinaryArray, Decimal128Array, Int64Array, RecordBatch};
    let p = dir.join("x.parquet");
    let mut ts = TimestampMicrosecondBuilder::new().with_timezone("UTC");
    let mut tags = ListBuilder::new(Int32Builder::new());
    for i in 0..3 {
        ts.append_value(946_684_801_500_000 + i);
        tags.values().append_value(1);
        tags.values().append_value(2);
        tags.append(true);
    }
    let cols: Vec<(&str, ArrayRef)> = vec![
        ("id", Arc::new(Int64Array::from(vec![1, 2, 3]))),
        ("price", Arc::new(Decimal128Array::from(vec![1250, 5, -1]).with_precision_and_scale(10, 2).unwrap())),
        ("at", Arc::new(ts.finish())),
        ("raw", Arc::new(BinaryArray::from(vec![&b"\xde\xad"[..], b"", b"\x00"]))),
        ("tags", Arc::new(tags.finish())),
    ];
    let batch = RecordBatch::try_from_iter(cols).unwrap();
    let mut w = parquet::arrow::ArrowWriter::try_new(std::fs::File::create(&p).unwrap(), batch.schema(), None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    p
}

#[test]
fn bad_rows_cancel_and_read_only_on_postgres() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec(&e, &c, &["DROP SCHEMA IF EXISTS diskman_import_err CASCADE", "CREATE SCHEMA diskman_import_err", "CREATE TABLE diskman_import_err.t (id int PRIMARY KEY, v text)"]);
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "bad.csv", b"id,v\n1,a\n2,b\nthree,c\n4,d\n2,dup\n6,f\n");
    let pv = preview(&p);
    let base = existing(&e, &c, &pv, "diskman_import_err", "t");

    // Stop, one transaction: the server's error and the file's line; nothing stays.
    let r = import_into(&e, &c, &p, base.clone());
    let Outcome::Failed(f) = &r.outcome else { panic!("{r:?}") };
    assert_eq!(f.line, Some(4), "{f:?}");
    assert_eq!(f.error.code.as_deref(), Some("22P02"));
    assert!(f.error.message.contains("three"), "{f:?}");
    assert!(r.rolled_back && r.rows == 0);
    assert_eq!(one(&e, &c, "SELECT count(*) FROM diskman_import_err.t"), "0");

    // Through INSERT the line comes from bisecting the batch.
    let mut o = base.clone();
    o.method = Method::Insert;
    let r = import_into(&e, &c, &p, o);
    let Outcome::Failed(f) = &r.outcome else { panic!("{r:?}") };
    assert_eq!(f.line, Some(4), "{f:?}");

    // Skip: the bad rows are left out, counted, with their lines; the rest goes.
    for method in [Method::Copy, Method::Insert] {
        for one_tx in [true, false] {
            exec(&e, &c, &["TRUNCATE diskman_import_err.t"]);
            let mut o = base.clone();
            o.on_error = OnError::Skip;
            o.method = method;
            o.one_transaction = one_tx;
            let r = import_into(&e, &c, &p, o);
            assert_eq!(r.outcome, Outcome::Done, "{method:?} {one_tx}: {r:?}");
            assert_eq!((r.rows, r.skipped), (4, 2), "{method:?} {one_tx}");
            assert_eq!(r.issues.iter().map(|i| i.line).collect::<Vec<_>>(), vec![4, 6], "{method:?} {one_tx}: {:?}", r.issues);
            assert_eq!(one(&e, &c, "SELECT string_agg(id::text, ',' ORDER BY id) FROM diskman_import_err.t"), "1,2,4,6");
        }
    }

    // A row the file gets wrong (a field missing): its line, before the server sees it.
    let p2 = write(dir.path(), "short.csv", b"id,v\n1,a\n2\n3,c\n");
    exec(&e, &c, &["TRUNCATE diskman_import_err.t"]);
    let r = import_into(&e, &c, &p2, base.clone());
    let Outcome::Failed(f) = &r.outcome else { panic!("{r:?}") };
    assert_eq!(f.line, Some(3));
    assert!(f.error.message.contains("1 fields, expected 2"), "{f:?}");

    // Not one transaction: what committed before the failing chunk stays.
    let big = big_csv(dir.path(), 120_000, Some(110_000));
    exec(&e, &c, &["CREATE TABLE diskman_import_err.big (id int, name text, price numeric, ok boolean, at timestamptz, doc jsonb)"]);
    let pvb = preview(&big);
    let mut o = existing(&e, &c, &pvb, "diskman_import_err", "big");
    o.one_transaction = false;
    let r = import_into(&e, &c, &big, o);
    let Outcome::Failed(f) = &r.outcome else { panic!("{r:?}") };
    assert_eq!(f.line, Some(110_001), "the header is line 1");
    assert_eq!(r.rows, 100_000, "two chunks of 50,000 committed");
    assert_eq!(one(&e, &c, "SELECT count(*) FROM diskman_import_err.big"), "100000");

    // Cancel mid-way: one transaction rolls back everything.
    exec(&e, &c, &["TRUNCATE diskman_import_err.big"]);
    let big2 = big_csv(dir.path(), 1_500_000, None);
    let pvb = preview(&big2);
    let imp = c.import(None, &big2, existing(&e, &c, &pvb, "diskman_import_err", "big"));
    let p = imp.progress.clone();
    let t0 = Instant::now();
    while p.rows() < 100_000 && t0.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = p.rows();
    p.cancel();
    let t_cancel = Instant::now();
    let r = e.core.block_on(imp.task).unwrap();
    eprintln!("cancel after {before} rows answered in {:?}", t_cancel.elapsed());
    assert_eq!(r.outcome, Outcome::Cancelled);
    assert!(before > 0 && r.rolled_back && r.rows == 0, "{r:?}");
    assert!(t_cancel.elapsed() < Duration::from_secs(5));
    assert_eq!(one(&e, &c, "SELECT count(*) FROM diskman_import_err.big"), "0");

    // A read-only connection refuses before connecting for the import.
    let ro = e.core.block_on(e.core.connect(pg_profile(true), Some(pg_password()))).unwrap();
    let r = import_into(&e, &ro, &p2, base.clone());
    let Outcome::Failed(f) = &r.outcome else { panic!("{r:?}") };
    assert_eq!(f.error.code.as_deref(), Some("25006"));
    assert_eq!(one(&e, &c, "SELECT count(*) FROM diskman_import_err.t"), "0");

    exec(&e, &c, &["DROP SCHEMA diskman_import_err CASCADE"]);
}

/// 1 M rows of CSV through COPY into PostgreSQL, and 200 k through the INSERT fallback.
#[test]
fn throughput_on_postgres() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec(&e, &c, &["DROP SCHEMA IF EXISTS diskman_import_big CASCADE", "CREATE SCHEMA diskman_import_big"]);
    let dir = tempfile::tempdir().unwrap();
    let p = big_csv(dir.path(), 1_000_000, None);
    let mb = std::fs::metadata(&p).unwrap().len() as f64 / 1e6;
    let pv = preview(&p);
    assert_eq!(pv.columns.iter().map(|c| c.ty).collect::<Vec<_>>(), [PgType::Int8, PgType::Text, PgType::Numeric, PgType::Bool, PgType::TimestampTz, PgType::Jsonb]);
    let r = import_into(&e, &c, &p, new_table(&pv, "diskman_import_big", "copy1m"));
    assert_eq!((r.outcome.clone(), r.rows), (Outcome::Done, 1_000_000), "{r:?}");
    let s = r.elapsed.as_secs_f64();
    eprintln!("PG COPY: 1,000,000 rows ({mb:.1} MB) in {s:.2} s = {:.0} rows/s, {:.1} MB/s", 1e6 / s, mb / s);
    assert_eq!(one(&e, &c, "SELECT sum(id) FROM diskman_import_big.copy1m"), "500000500000");

    let p2 = big_csv(dir.path(), 200_000, None);
    let mut o = new_table(&preview(&p2), "diskman_import_big", "ins");
    o.method = Method::Insert;
    for (rows_per, one_tx) in [(0usize, true), (0, false), (1000, true)] {
        o.insert_rows = rows_per;
        o.one_transaction = one_tx;
        o.table = format!("ins_{rows_per}_{one_tx}");
        let r = import_into(&e, &c, &p2, o.clone());
        assert_eq!((r.outcome.clone(), r.rows), (Outcome::Done, 200_000), "{r:?}");
        let s = r.elapsed.as_secs_f64();
        let per = if rows_per == 0 { "auto".to_string() } else { rows_per.to_string() };
        eprintln!("PG INSERT ({per} rows/stmt, one tx {one_tx}): 200,000 rows in {s:.2} s = {:.0} rows/s", 2e5 / s);
    }
    exec(&e, &c, &["DROP SCHEMA diskman_import_big CASCADE"]);
}

/// 10 M rows through COPY: memory stays flat (run under `/usr/bin/time -l` for the peak RSS).
/// `cargo test --release -p diskman-core --test live_import ten_million -- --ignored --nocapture`
#[test]
#[ignore]
fn ten_million_rows() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    exec(&e, &c, &["DROP SCHEMA IF EXISTS diskman_import_10m CASCADE", "CREATE SCHEMA diskman_import_10m"]);
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("10m.csv");
    {
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&p).unwrap());
        writeln!(w, "id,name,price").unwrap();
        for i in 1..=10_000_000u64 {
            writeln!(w, "{i},n{},{}.{:02}", i % 9973, i % 1000, i % 100).unwrap();
        }
        w.flush().unwrap();
    }
    let mb = std::fs::metadata(&p).unwrap().len() as f64 / 1e6;
    let pv = preview(&p);
    let imp = c.import(None, &p, new_table(&pv, "diskman_import_10m", "t"));
    let progress = imp.progress.clone();
    let r = e.core.block_on(imp.task).unwrap();
    assert_eq!((r.outcome.clone(), r.rows), (Outcome::Done, 10_000_000), "{r:?}");
    assert!(progress.is_finished());
    let s = r.elapsed.as_secs_f64();
    eprintln!("PG COPY: 10,000,000 rows ({mb:.0} MB) in {s:.1} s = {:.0} rows/s", 1e7 / s);
    exec(&e, &c, &["DROP SCHEMA diskman_import_10m CASCADE"]);
}

/// LAR: COPY FROM STDIN (text) when it has it, the INSERT fallback, a bad row's line found by
/// bisection (its COPY errors carry no line), skip mode.
#[test]
fn files_into_lar() {
    let e = env();
    let Some(c) = lar(&e) else { return };
    exec_quiet(&e, &c, &["DROP TABLE diskman_imp_lar", "DROP TABLE diskman_imp_lar_new", "DROP TABLE diskman_imp_lar_ins"]);
    exec(&e, &c, &["CREATE TABLE diskman_imp_lar (id bigint PRIMARY KEY, name text, price numeric, ok boolean, at timestamp, doc jsonb)"]);
    let dir = tempfile::tempdir().unwrap();
    let p = big_csv(dir.path(), 100_000, None);
    let pv = preview(&p);
    let o = existing(&e, &c, &pv, "public", "diskman_imp_lar");

    let r = import_into(&e, &c, &p, o.clone());
    assert_eq!((r.outcome.clone(), r.rows), (Outcome::Done, 100_000), "{r:?}");
    eprintln!("LAR {:?}: 100,000 rows in {:.2} s = {:.0} rows/s", r.method, r.elapsed.as_secs_f64(), 1e5 / r.elapsed.as_secs_f64());
    assert_eq!(one(&e, &c, "SELECT count(*) FROM diskman_imp_lar"), "100000");

    // The INSERT fallback, forced, into a new table.
    let mut o2 = new_table(&pv, "public", "diskman_imp_lar_ins");
    o2.method = Method::Insert;
    let r = import_into(&e, &c, &p, o2);
    assert_eq!((r.outcome.clone(), r.rows, r.method), (Outcome::Done, 100_000, Method::Insert), "{r:?}");
    eprintln!("LAR INSERT: 100,000 rows in {:.2} s = {:.0} rows/s", r.elapsed.as_secs_f64(), 1e5 / r.elapsed.as_secs_f64());
    assert_eq!(one(&e, &c, "SELECT count(*) FROM diskman_imp_lar_ins"), "100000");
    assert_eq!(rows(&e, &c, "SELECT name, doc FROM diskman_imp_lar_ins WHERE id = 7")[0][0].as_deref(), Some("name 7, with comma"));

    // A bad row: its line, found by bisection; nothing stays.
    let bad = write(dir.path(), "bad.csv", b"id,name\n1,a\n2,b\nx,c\n4,d\n");
    let pvb = preview(&bad);
    for method in [Method::Auto, Method::Insert] {
        let mut o3 = new_table(&pvb, "public", "diskman_imp_lar_new");
        o3.columns[0].type_name = "bigint".into();
        o3.method = method;
        let r = import_into(&e, &c, &bad, o3.clone());
        let Outcome::Failed(f) = &r.outcome else { panic!("{r:?}") };
        assert_eq!(f.line, Some(4), "{method:?}: {f:?}");
        assert!(r.rolled_back);
        // LAR keeps a table created in a transaction that rolled back: the import drops it.
        let s = c.session(None);
        let rs = s.execute("SELECT 1 FROM diskman_imp_lar_new");
        assert_eq!(e.core.block_on(rs.wait_finished()), Phase::Failed, "the new table is gone");
        // Skip mode: three rows.
        o3.on_error = OnError::Skip;
        let r = import_into(&e, &c, &bad, o3);
        assert_eq!((r.outcome.clone(), r.rows, r.skipped), (Outcome::Done, 3, 1), "{method:?}: {r:?}");
        assert_eq!(r.issues[0].line, 4);
        exec(&e, &c, &["DROP TABLE diskman_imp_lar_new"]);
    }
    exec(&e, &c, &["DROP TABLE diskman_imp_lar", "DROP TABLE diskman_imp_lar_ins"]);
}
