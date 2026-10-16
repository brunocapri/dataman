//! Live tests of export, grid editing and the relationship graph (see `common` for the servers).

mod common;
use common::*;

// ---------------------------------------------------------------------------------------------
// Export

mod export {
    use super::*;
    use dataman_core::export::{ExportFormat, ExportOptions, export_result};

    #[test]
    fn pg_query_export_10m_rows_flat_memory() {
        let e = env();
        let Some(c) = pg(&e) else { return };
        let dir = tempfile::tempdir().unwrap();
        let sql = "SELECT g AS id, md5(g::text) AS h, g * 0.5 AS half, (g % 3 = 0) AS three, \
                   timestamptz '2026-01-01' + g * interval '1 second' AS at FROM generate_series(1, 10000000) g";
        let rows: u64 = std::env::var("DATAMAN_EXPORT_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000_000);
        let sql = sql.replace("10000000", &rows.to_string());
        for fmt in [ExportFormat::Csv, ExportFormat::Parquet] {
            let path = dir.path().join(format!("big.{}", fmt.extension()));
            let before = rss_kb();
            let t = Instant::now();
            let x = c.export_query(None, sql.clone(), &path, ExportOptions::new(fmt), None);
            let mut peak = before;
            let mut task = x.task;
            let s = loop {
                if let Some(r) = task.try_take() {
                    break r.unwrap();
                }
                peak = peak.max(rss_kb());
                std::thread::sleep(Duration::from_millis(200));
            };
            assert_eq!(s.rows, rows, "{fmt:?}");
            assert!(s.complete && !s.cancelled);
            let size = std::fs::metadata(&path).unwrap().len();
            eprintln!(
                "export {fmt:?}: {rows} rows, {:.1} MB in {:.1?} ({:.0} rows/s); RSS {} MB → peak {} MB",
                size as f64 / 1e6,
                t.elapsed(),
                rows as f64 / t.elapsed().as_secs_f64(),
                before / 1024,
                peak / 1024
            );
            // Flat memory: far below the size of the data.
            assert!(peak.saturating_sub(before) < 400 * 1024, "RSS grew {} MB", (peak - before) / 1024);
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn pg_query_export_cancel_and_errors() {
        let e = env();
        let Some(c) = pg(&e) else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ndjson");
        let x = c.export_query(None, "SELECT g FROM generate_series(1, 50000000) g", &path, ExportOptions::new(ExportFormat::Ndjson), None);
        while x.progress.rows() < 100_000 {
            std::thread::sleep(Duration::from_millis(20));
        }
        x.progress.cancel();
        let s = e.core.block_on(x.task).unwrap();
        assert!(s.cancelled);
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // A runtime error midway leaves nothing behind either.
        let x = c.export_query(None, "SELECT 1 / (g - 5000) FROM generate_series(1, 10000) g", &path, ExportOptions::new(ExportFormat::Csv), None);
        let err = e.core.block_on(x.task).unwrap_err().to_string();
        assert!(err.contains("22012"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let x = c.export_query(None, "DELETE FROM orders", &path, ExportOptions::new(ExportFormat::Csv), None);
        assert!(e.core.block_on(x.task).is_err());
    }

    #[test]
    fn pg_query_export_cancel_before_the_first_row() {
        // A slow statement: the cancel must not wait for its first row.
        let e = env();
        let Some(c) = pg(&e) else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slow.csv");
        let x = c.export_query(None, "SELECT pg_sleep(4), 1", &path, ExportOptions::new(ExportFormat::Csv), None);
        std::thread::sleep(Duration::from_millis(400));
        let t = Instant::now();
        x.progress.cancel();
        let s = e.core.block_on(x.task).unwrap();
        eprintln!("cancel of a sleeping export: {:?}", t.elapsed());
        assert!(s.cancelled);
        assert!(t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // The CancelRequest reached the server: nothing is left sleeping there.
        std::thread::sleep(Duration::from_millis(300));
        let rs = query(&e, &c, "SELECT count(*) FROM pg_stat_activity WHERE query = 'SELECT pg_sleep(4), 1' AND state = 'active'");
        assert_eq!(rs.snapshot().text(0, 0).as_deref(), Some("0"));
    }

    #[test]
    fn pg_result_export_fetches_the_rest_on_demand() {
        let e = env();
        let Some(c) = pg(&e) else { return };
        let s = c.session(None);
        let rs = s.execute("SELECT g, 'x' || g AS t, NULL::text AS n, now()::date AS d FROM generate_series(1, 200000) g");
        e.core.block_on(async {
            let mut rx = rs.subscribe();
            while rs.phase() != Phase::Paused {
                rx.changed().await.unwrap();
            }
        });
        assert!(rs.rows() < 200_000);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.sql");
        let mut o = ExportOptions::new(ExportFormat::SqlInsert);
        o.table = "copied".into();
        o.batch_size = 1000;
        let x = export_result(c.handle(), rs.clone(), &path, o);
        let sum = e.core.block_on(x.task).unwrap();
        assert_eq!(sum.rows, 200_000);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("INSERT INTO copied (g, t, n, d) VALUES").count(), 200);
        assert!(text.contains("(200000, 'x200000', NULL, '"));
        // The file loads back into PostgreSQL.
        let schema = "dataman_export_t";
        super::exec(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &format!("CREATE SCHEMA {schema}")]);
        let load = format!("SET search_path = {schema}; CREATE TABLE copied (g int, t text, n text, d date); {text}");
        super::exec(&e, &c, &[&load]);
        let rs = super::query(&e, &c, &format!("SELECT count(*), sum(g) FROM {schema}.copied"));
        assert_eq!(rs.snapshot().text(0, 0).as_deref(), Some("200000"));
        super::exec(&e, &c, &[&format!("DROP SCHEMA {schema} CASCADE")]);
    }

    #[test]
    fn lar_export() {
        let e = env();
        let Some(c) = lar(&e) else { return };
        let dir = tempfile::tempdir().unwrap();
        super::exec_quiet(&e, &c, &["DROP TABLE dataman_exp"]);
        super::exec(
            &e,
            &c,
            &[
                "CREATE TABLE dataman_exp (id BIGINT PRIMARY KEY, name TEXT, price DOUBLE PRECISION, ok BOOLEAN, at TIMESTAMPTZ, due DATE, b BYTEA, emb VECTOR(3))",
                "INSERT INTO dataman_exp VALUES (1, 'a''b', 1.5, true, '2026-01-02T03:04:05Z', '2026-01-02', '\\xdead', '[1,0,0]'), (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
            ],
        );
        let path = dir.path().join("l.parquet");
        let x = c.export_query(None, "SELECT * FROM dataman_exp ORDER BY id", &path, ExportOptions::new(ExportFormat::Parquet), None);
        assert_eq!(e.core.block_on(x.task).unwrap().rows, 2);
        let path = dir.path().join("l.json");
        let x = c.export_query(None, "SELECT * FROM dataman_exp ORDER BY id", &path, ExportOptions::new(ExportFormat::Json), None);
        e.core.block_on(x.task).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        eprintln!("LAR json export: {v}");
        assert_eq!(v[0]["name"], "a'b");
        assert_eq!(v[0]["at"], "2026-01-02T03:04:05Z");
        assert_eq!(v[1]["price"], serde_json::Value::Null);
        super::exec(&e, &c, &["DROP TABLE dataman_exp"]);
    }
}

// ---------------------------------------------------------------------------------------------
// Grid editing

mod edit {
    use super::*;
    use dataman_core::edit::{CellValue, ChangeSet, ConflictCheck, ConflictKind, EditTarget, Editability, KeySource, Outcome, preview};

    fn target(e: &Env, c: &Arc<Connection>, rs: &Arc<dataman_core::ResultSet>, key: Option<Vec<String>>) -> Editability {
        e.core.block_on(c.edit_target(c.database(), rs, key)).unwrap()
    }

    fn editable(x: Editability) -> EditTarget {
        match x {
            Editability::Editable(t) => t,
            Editability::ReadOnly(why) => panic!("read-only: {why}"),
        }
    }

    fn read_only(x: Editability) -> String {
        match x {
            Editability::ReadOnly(why) => why,
            Editability::Editable(t) => panic!("editable: {t:?}"),
        }
    }

    #[test]
    fn postgres_edit_apply_and_conflicts() {
        let e = env();
        let Some(c) = pg(&e) else { return };
        let s = "dataman_edit_t";
        exec(&e, &c, &[
            &format!("DROP SCHEMA IF EXISTS {s} CASCADE"),
            &format!("CREATE SCHEMA {s}"),
            &format!("CREATE TABLE {s}.p (id int PRIMARY KEY, name text, total numeric(10,2), f real, j json, at timestamptz)"),
            &format!("INSERT INTO {s}.p VALUES (1, 'one', 1.50, 0.1, '{{\"a\":1}}', '2026-01-01 10:00+00'), (2, NULL, 2, 0.2, NULL, NULL), (3, 'three', 3, 0.3, NULL, now())"),
            &format!("CREATE TABLE {s}.nokey (a int, b text)"),
            &format!("CREATE TABLE {s}.u (code text NOT NULL UNIQUE, v int)"),
        ]);
        let rs = query(&e, &c, &format!("SELECT * FROM {s}.p ORDER BY id"));
        let t = editable(target(&e, &c, &rs, None));
        assert_eq!(t.key_source, KeySource::PrimaryKey);
        assert_eq!(t.check, ConflictCheck::Predicate);
        assert_eq!(t.table.name, "p");
        assert!(t.columns.iter().all(|c| c.is_some()), "{:?}", t.columns);
        let snap = rs.snapshot();
        let mut cs = ChangeSet::new();
        cs.edit(0, 1, CellValue::text("one"), CellValue::text("ONE"));
        cs.edit(0, 3, CellValue::text("0.1"), CellValue::text("0.75"));
        cs.edit(1, 1, CellValue::Null, CellValue::text("two"));
        let i = cs.insert_row(6);
        cs.set_inserted(i, 0, Some(CellValue::text("4")));
        cs.set_inserted(i, 1, Some(CellValue::text("four")));
        cs.delete_row(2);
        let plan = cs.plan(&t, &snap).unwrap();
        eprintln!("{}", preview(&plan));
        let r = e.core.block_on(c.apply_changes(None, plan)).unwrap();
        assert!(r.committed, "{r:?}");
        assert!(r.outcomes.iter().all(|o| matches!(o.outcome, Outcome::Applied { rows: 1 })), "{r:?}");
        let after = query(&e, &c, &format!("SELECT id, name, f FROM {s}.p ORDER BY id"));
        let a = after.snapshot();
        assert_eq!(a.rows(), 3);
        assert_eq!(a.text(0, 1).as_deref(), Some("ONE"));
        assert_eq!(a.text(0, 2).as_deref(), Some("0.75"));
        assert_eq!(a.text(1, 1).as_deref(), Some("two"));
        assert_eq!(a.text(2, 0).as_deref(), Some("4"));

        // Conflicts: someone else changes row 1 and deletes row 2 after we read them.
        let rs = query(&e, &c, &format!("SELECT * FROM {s}.p ORDER BY id"));
        let t = editable(target(&e, &c, &rs, None));
        exec(&e, &c, &[&format!("UPDATE {s}.p SET name = 'other' WHERE id = 1"), &format!("DELETE FROM {s}.p WHERE id = 2")]);
        let mut cs = ChangeSet::new();
        cs.edit(0, 1, CellValue::text("ONE"), CellValue::text("mine"));
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(!r.committed);
        assert!(matches!(r.outcomes[0].outcome, Outcome::Conflict { kind: ConflictKind::Changed { .. }, .. }), "{r:?}");
        let mut cs = ChangeSet::new();
        cs.edit(2, 1, CellValue::text("four"), CellValue::text("4!"));
        cs.edit(1, 1, CellValue::text("two"), CellValue::text("2!"));
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(!r.committed);
        assert!(matches!(r.outcomes[0].outcome, Outcome::Conflict { kind: ConflictKind::RowGone, .. }), "{r:?}");
        assert_eq!(r.outcomes[1].outcome, Outcome::NotRun);
        eprintln!("conflict: {:?}", r.error);

        // A failing statement rolls back the ones before it.
        let mut cs = ChangeSet::new();
        cs.edit(2, 1, CellValue::text("four"), CellValue::text("FOUR"));
        let i = cs.insert_row(6);
        cs.set_inserted(i, 0, Some(CellValue::text("1")));
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(!r.committed);
        assert!(matches!(r.outcomes[0].outcome, Outcome::RolledBack { rows: 1 }), "{r:?}");
        match &r.outcomes[1].outcome {
            Outcome::Failed { error, message } => {
                assert_eq!(error.code.as_deref(), Some("23505"));
                assert!(message.starts_with("Duplicate value"), "{message}");
            }
            o => panic!("{o:?}"),
        }
        let chk = query(&e, &c, &format!("SELECT name FROM {s}.p WHERE id = 4"));
        assert_eq!(chk.snapshot().text(0, 0).as_deref(), Some("four"));

        // Read-only cases, with reasons.
        let why = read_only(target(&e, &c, &query(&e, &c, &format!("SELECT a.id, b.id FROM {s}.p a JOIN {s}.p b ON a.id = b.id")), None));
        assert!(why.contains("several tables"), "{why}");
        let why = read_only(target(&e, &c, &query(&e, &c, &format!("SELECT * FROM {s}.nokey")), None));
        assert!(why.contains("no primary key"), "{why}");
        let why = read_only(target(&e, &c, &query(&e, &c, &format!("SELECT name FROM {s}.p")), None));
        assert!(why.contains("add it to the SELECT"), "{why}");
        let why = read_only(target(&e, &c, &query(&e, &c, &format!("SELECT count(*) FROM {s}.p")), None));
        eprintln!("count(*): {why}");
        // A unique NOT NULL index works as the key; an explicit key overrides.
        let t = editable(target(&e, &c, &query(&e, &c, &format!("SELECT * FROM {s}.u")), None));
        assert_eq!(t.key_source, KeySource::UniqueIndex);
        let t = editable(target(&e, &c, &query(&e, &c, &format!("SELECT * FROM {s}.nokey")), Some(vec!["a".into()])));
        assert_eq!(t.key_source, KeySource::Explicit);
        // Computed columns stay read-only.
        let t = editable(target(&e, &c, &query(&e, &c, &format!("SELECT id, upper(name) AS u, name AS n FROM {s}.p")), None));
        assert_eq!(t.columns, vec![Some("id".into()), None, Some("name".into())]);
        exec(&e, &c, &[&format!("DROP SCHEMA {s} CASCADE")]);
    }

    #[test]
    fn postgres_edit_guards() {
        let e = env();
        let Some(c) = pg(&e) else { return };
        let s = "dataman_edit_g";
        exec(&e, &c, &[
            &format!("DROP SCHEMA IF EXISTS {s} CASCADE"),
            &format!("CREATE SCHEMA {s}"),
            &format!("CREATE TABLE {s}.emp (id int PRIMARY KEY, name text, manager int, email text)"),
            &format!("INSERT INTO {s}.emp VALUES (1, 'boss', NULL, 'boss@x'), (2, 'worker', 1, 'worker@x')"),
            &format!("CREATE TABLE {s}.fk (k real PRIMARY KEY, v text)"),
            &format!("INSERT INTO {s}.fk VALUES (0.1, 'a'), (0.3, 'b')"),
            &format!("CREATE DOMAIN {s}.dj AS json"),
            &format!("CREATE DOMAIN {s}.dpa AS point[]"),
            &format!("CREATE TABLE {s}.odd (id int PRIMARY KEY, v text, j json[], x xml[], pa point[], dj {s}.dj, dpa {s}.dpa)"),
            &format!("INSERT INTO {s}.odd VALUES (1, 'a', ARRAY['{{}}'::json], ARRAY['<a/>'::xml], ARRAY[point(1,2)], '{{}}', ARRAY[point(3,4)]), (2, 'b', NULL, NULL, NULL, NULL, NULL)"),
        ]);
        // A self-join behind a subquery or a CTE: one origin table for every column, still
        // read-only (an edit keyed by the worker's id would write the boss's email into it).
        for sql in [
            format!("SELECT * FROM (SELECT w.id, w.name, m.email FROM {s}.emp w JOIN {s}.emp m ON m.id = w.manager) x"),
            format!("WITH x AS (SELECT w.id, w.name, m.email FROM {s}.emp w JOIN {s}.emp m ON m.id = w.manager) SELECT * FROM x"),
        ] {
            let rs = query(&e, &c, &sql);
            let why = read_only(target(&e, &c, &rs, None));
            eprintln!("{sql} → read-only: {why}");
        }
        // `TABLE t` is editable.
        let t = editable(target(&e, &c, &query(&e, &c, &format!("TABLE {s}.emp")), None));
        assert_eq!(t.table.name, "emp");

        // A real key: the WHERE must match the stored float4.
        let rs = query(&e, &c, &format!("SELECT * FROM {s}.fk ORDER BY k"));
        let t = editable(target(&e, &c, &rs, None));
        let mut cs = ChangeSet::new();
        cs.edit(0, 1, CellValue::text("a"), CellValue::text("A"));
        cs.delete_row(1);
        let plan = cs.plan(&t, &rs.snapshot()).unwrap();
        eprintln!("{}", preview(&plan));
        let r = e.core.block_on(c.apply_changes(None, plan)).unwrap();
        assert!(r.committed, "{r:#?}");
        let a = query(&e, &c, &format!("SELECT v FROM {s}.fk"));
        assert_eq!(a.snapshot().rows(), 1);
        assert_eq!(a.snapshot().text(0, 0).as_deref(), Some("A"));

        // A DELETE checks the row first: changed → conflict, gone → conflict.
        let rs = query(&e, &c, &format!("SELECT * FROM {s}.emp ORDER BY id"));
        let t = editable(target(&e, &c, &rs, None));
        exec(&e, &c, &[&format!("UPDATE {s}.emp SET email = 'new@x' WHERE id = 2")]);
        let mut cs = ChangeSet::new();
        cs.delete_row(1);
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(!r.committed);
        assert!(matches!(r.outcomes[0].outcome, Outcome::Conflict { kind: ConflictKind::Changed { .. }, .. }), "{r:?}");
        exec(&e, &c, &[&format!("DELETE FROM {s}.emp WHERE id = 2")]);
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(matches!(r.outcomes[0].outcome, Outcome::Conflict { kind: ConflictKind::RowGone, .. }), "{r:?}");
        // Unchanged: deleted.
        let mut cs = ChangeSet::new();
        cs.delete_row(0);
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(r.committed, "{r:?}");

        // Arrays and domains of types without equality: left out of the checks, not failing
        // the whole save with "could not identify an equality operator".
        let rs = query(&e, &c, &format!("SELECT * FROM {s}.odd ORDER BY id"));
        let t = editable(target(&e, &c, &rs, None));
        eprintln!("odd type names: {:?}", t.type_names);
        let mut cs = ChangeSet::new();
        cs.edit(0, 1, CellValue::text("a"), CellValue::text("A"));
        cs.edit(0, 2, CellValue::text("x"), CellValue::text("{\"[1]\"}"));
        cs.edit(0, 5, CellValue::text("x"), CellValue::text("[2]"));
        cs.edit(0, 6, CellValue::text("x"), CellValue::text("{\"(5,6)\"}"));
        cs.delete_row(1);
        let plan = cs.plan(&t, &rs.snapshot()).unwrap();
        eprintln!("{}", preview(&plan));
        let r = e.core.block_on(c.apply_changes(None, plan)).unwrap();
        assert!(r.committed, "{r:#?}");
        exec(&e, &c, &[&format!("DROP SCHEMA {s} CASCADE")]);
    }

    #[test]
    fn lar_edit_apply_and_conflicts() {
        let e = env();
        let Some(c) = lar(&e) else { return };
        exec_quiet(&e, &c, &["DROP TABLE dataman_ed", "DROP TABLE dataman_edf"]);
        // (Created before the first edit_target, which caches the table list.)
        exec(&e, &c, &["CREATE TABLE dataman_edf (k REAL, v TEXT)", "INSERT INTO dataman_edf VALUES (0.1, 'a'), (0.3, 'b')"]);
        exec(&e, &c, &[
            "CREATE TABLE dataman_ed (id BIGINT PRIMARY KEY, name TEXT, total DOUBLE PRECISION, at TIMESTAMPTZ)",
            "INSERT INTO dataman_ed VALUES (1, 'one', 1.5, '2026-01-01T10:00:00Z'), (2, NULL, 2, NULL), (3, 'three', 3, NULL)",
        ]);
        let rs = query(&e, &c, "SELECT * FROM dataman_ed ORDER BY id");
        eprintln!("LAR origins: {:?}", rs.origins());
        let t = editable(target(&e, &c, &rs, None));
        assert_eq!(t.key_source, KeySource::Guessed);
        assert_eq!(t.check, ConflictCheck::Reread);
        let mut cs = ChangeSet::new();
        cs.edit(0, 1, CellValue::text("one"), CellValue::text("ONE"));
        cs.edit(0, 3, CellValue::text("x"), CellValue::text("2026-02-02T00:00:00Z"));
        cs.edit(1, 2, CellValue::text("2"), CellValue::text("2.25"));
        let i = cs.insert_row(4);
        cs.set_inserted(i, 0, Some(CellValue::text("4")));
        cs.set_inserted(i, 1, Some(CellValue::text("four")));
        cs.delete_row(2);
        let plan = cs.plan(&t, &rs.snapshot()).unwrap();
        eprintln!("{}", preview(&plan));
        let r = e.core.block_on(c.apply_changes(None, plan)).unwrap();
        assert!(r.committed, "{r:#?}");
        let a = query(&e, &c, "SELECT id, name, total FROM dataman_ed ORDER BY id");
        let a = a.snapshot();
        assert_eq!(a.rows(), 3);
        assert_eq!(a.text(0, 1).as_deref(), Some("ONE"));
        assert_eq!(a.text(1, 2).as_deref(), Some("2.25"));
        assert_eq!(a.text(2, 1).as_deref(), Some("four"));

        // Conflicts, detected by re-reading by key.
        let rs = query(&e, &c, "SELECT * FROM dataman_ed ORDER BY id");
        exec(&e, &c, &["UPDATE dataman_ed SET name = 'other' WHERE id = 1", "DELETE FROM dataman_ed WHERE id = 2"]);
        let mut cs = ChangeSet::new();
        cs.edit(0, 1, CellValue::text("ONE"), CellValue::text("mine"));
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(!r.committed);
        match &r.outcomes[0].outcome {
            Outcome::Conflict { kind: ConflictKind::Changed { column, found, .. }, message } => {
                assert_eq!(column.as_deref(), Some("name"));
                assert_eq!(found.as_deref(), Some("other"));
                eprintln!("LAR conflict: {message}");
            }
            o => panic!("{o:?}"),
        }
        let mut cs = ChangeSet::new();
        cs.delete_row(1);
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(matches!(r.outcomes[0].outcome, Outcome::Conflict { kind: ConflictKind::RowGone, .. }), "{r:?}");
        // A DELETE of a row changed since it was read: re-read, conflict.
        let mut cs = ChangeSet::new();
        cs.delete_row(0);
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(!r.committed);
        assert!(matches!(&r.outcomes[0].outcome, Outcome::Conflict { kind: ConflictKind::Changed { column: Some(c), .. }, .. } if c == "name"), "{r:?}");
        // Duplicate key.
        let mut cs = ChangeSet::new();
        let i = cs.insert_row(4);
        cs.set_inserted(i, 0, Some(CellValue::text("1")));
        let r = e.core.block_on(c.apply_changes(None, cs.plan(&t, &rs.snapshot()).unwrap())).unwrap();
        assert!(!r.committed);
        eprintln!("LAR duplicate insert: {:?}", r.outcomes[0].outcome);
        // Historical results are read-only.
        let rs = query(&e, &c, "SELECT * FROM dataman_ed FOR SYSTEM_TIME AS OF 1");
        assert!(read_only(target(&e, &c, &rs, None)).contains("AS OF"));
        exec(&e, &c, &["DROP TABLE dataman_ed"]);

        // A REAL key chosen by the caller: the cast literal matches the stored value.
        let rs = query(&e, &c, "SELECT * FROM dataman_edf ORDER BY k");
        let t = editable(target(&e, &c, &rs, Some(vec!["k".into()])));
        let mut cs = ChangeSet::new();
        cs.edit(0, 1, CellValue::text("a"), CellValue::text("A"));
        let plan = cs.plan(&t, &rs.snapshot()).unwrap();
        eprintln!("LAR real key: {}", preview(&plan));
        let r = e.core.block_on(c.apply_changes(None, plan)).unwrap();
        eprintln!("LAR real key: {r:?}");
        assert!(r.committed, "{r:#?}");
        exec(&e, &c, &["DROP TABLE dataman_edf"]);
    }
}

// ---------------------------------------------------------------------------------------------
// Relationships (the links foreign-key navigation follows)

mod relations {
    use super::*;
    use dataman_core::relations::Link;

    fn label(l: &Link) -> String {
        format!("{}.{} → {}", l.from_table, l.from_columns.join(","), l.to_table)
    }

    #[test]
    fn postgres_catalog() {
        let e = env();
        let Some(c) = pg(&e) else { return };
        let s = "dataman_rel_t";
        exec(&e, &c, &[
            &format!("DROP SCHEMA IF EXISTS {s} CASCADE"),
            &format!("CREATE SCHEMA {s}"),
            &format!("CREATE TABLE {s}.customer (id int PRIMARY KEY, name text NOT NULL)"),
            &format!("CREATE TABLE {s}.profile (customer_id int PRIMARY KEY REFERENCES {s}.customer(id), bio text)"),
            &format!("CREATE TABLE {s}.orders (id int PRIMARY KEY, customer int NOT NULL REFERENCES {s}.customer(id), \"Total\" numeric)"),
            &format!("CREATE TABLE {s}.batch (a int, b int, PRIMARY KEY (a, b))"),
            &format!("CREATE TABLE {s}.item (id serial PRIMARY KEY, order_id int REFERENCES {s}.orders, la int, lb int, FOREIGN KEY (la, lb) REFERENCES {s}.batch (a, b))"),
            &format!("ANALYZE {s}.customer"),
        ]);
        let g = e.core.block_on(c.relationships(c.database(), s)).unwrap();
        assert!(g.links.iter().all(|l| !l.inferred && l.from_schema == s && l.to_schema == s));
        let l = g.links.iter().find(|l| l.from_table == "item" && l.to_table == "batch").unwrap();
        assert_eq!((l.from_columns.clone(), l.to_columns.clone()), (vec!["la".to_string(), "lb".into()], vec!["a".to_string(), "b".into()]));
        let l = g.links.iter().find(|l| l.from_table == "orders").unwrap();
        assert_eq!((l.from_columns.clone(), l.to_columns.clone()), (vec!["customer".to_string()], vec!["id".to_string()]));
        assert_eq!(g.links.iter().filter(|l| l.to_table == "customer").count(), 2);
        assert_eq!(g.links.len(), 4);
        // Cached.
        let t = Instant::now();
        let again = e.core.block_on(c.relationships(c.database(), s)).unwrap();
        assert!(Arc::ptr_eq(&g, &again) && t.elapsed() < Duration::from_millis(50));
        exec(&e, &c, &[&format!("DROP SCHEMA {s} CASCADE")]);
    }

    #[test]
    fn lar_inferred_links() {
        let e = env();
        let Some(c) = lar(&e) else { return };
        exec_quiet(&e, &c, &["DROP TABLE strel_items", "DROP TABLE strel_orders", "DROP TABLE strel_products", "DROP TABLE strel_customers"]);
        exec(&e, &c, &[
            "CREATE TABLE strel_customers (id BIGINT PRIMARY KEY, name TEXT)",
            "CREATE TABLE strel_products (id BIGINT PRIMARY KEY, title TEXT)",
            "CREATE TABLE strel_orders (id BIGINT PRIMARY KEY, strel_customer BIGINT REFERENCES strel_customers(id), total DOUBLE PRECISION)",
            // strel_product looks like a link by its name, but is a plain integer.
            "CREATE TABLE strel_items (id BIGINT PRIMARY KEY, strel_order_id BIGINT REFERENCES strel_orders(id), strel_product BIGINT, qty BIGINT)",
            "INSERT INTO strel_customers VALUES (1, 'Ana')",
            "INSERT INTO strel_orders VALUES (10, 1, 5.0), (11, 1, 6.0)",
        ]);
        let t = Instant::now();
        let g = e.core.block_on(c.relationships(c.database(), "public")).unwrap();
        eprintln!("LAR graph: {} links in {:?}", g.links.len(), t.elapsed());
        let mine: Vec<&Link> = g.links.iter().filter(|l| l.from_table.starts_with("strel_")).collect();
        let labels: Vec<String> = mine.iter().map(|l| label(l)).collect();
        assert_eq!(mine.len(), 2, "{labels:?}");
        assert!(labels.contains(&"strel_orders.strel_customer → strel_customers".to_string()), "{labels:?}");
        assert!(labels.contains(&"strel_items.strel_order_id → strel_orders".to_string()), "{labels:?}");
        // The target's key is inferred (an integer `id`).
        assert!(mine.iter().all(|l| l.inferred && l.to_columns == ["id"]));
        eprintln!("other links: {:?}", g.links.iter().filter(|l| !l.from_table.starts_with("strel_")).map(label).collect::<Vec<_>>());
        exec(&e, &c, &["DROP TABLE strel_items", "DROP TABLE strel_orders", "DROP TABLE strel_products", "DROP TABLE strel_customers"]);
    }
}
