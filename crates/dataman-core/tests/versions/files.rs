//! Embedded engines: their "old versions" are files written by older releases, opened by the
//! engine Dataman carries. No server: these run on every run. See live_versions.rs.
//!
//! files/duckdb-<release>.duckdb were written by that DuckDB release (its Python package):
//! an enum, a table of nested and wide types with a primary key, and a view.

use super::*;
use dataman_core::ConnectionProfile;
use std::path::{Path, PathBuf};

const FILES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/versions/files");

/// A copy of the fixture: opening a file may upgrade or write it.
fn copy(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let to = dir.path().join(name);
    std::fs::copy(Path::new(FILES).join(name), &to).unwrap();
    (dir, to)
}

fn open(e: &Env, engine: Engine, path: &Path) -> anyhow::Result<Arc<Connection>> {
    e.core.block_on(e.core.connect(ConnectionProfile::new_file(engine, path.to_string_lossy()), None))
}

fn texts(e: &Env, c: &Arc<Connection>, sql: &str) -> Vec<Vec<Option<String>>> {
    let rs = c.session(None).execute(sql);
    rs.fetch_all();
    assert_eq!(e.core.block_on(rs.wait_finished()), Phase::Done, "{sql}: {:?}", rs.status().error);
    let n = rs.columns().map_or(0, |c| c.len());
    let s = rs.snapshot();
    (0..s.rows()).map(|r| (0..n).map(|c| s.text(r, c).map(|t| t.into_owned())).collect()).collect()
}

/// From 0.10 on DuckDB reads the files of the releases before it; one written by a release it
/// cannot read (0.8) is refused with the release it was written by, not opened as empty.
#[test]
fn duckdb_files_of_older_releases() {
    let e = env();
    let (_d, old) = copy("duckdb-0.8.1.duckdb");
    let err = format!("{:#}", open(&e, Engine::DuckDb, &old).err().expect("a file DuckDB cannot read is refused"));
    assert!(err.contains("v0.8"), "says which release wrote it: {err}");
    for release in ["0.9.2", "0.10.3", "1.0.0", "1.1.3", "1.2.2"] {
        let (_d, path) = copy(&format!("duckdb-{release}.duckdb"));
        let c = open(&e, Engine::DuckDb, &path).unwrap_or_else(|err| panic!("{release}: {err:#}"));
        let rows = texts(&e, &c, "SELECT id, name, price, \"at\", tags, s, m, feel, big FROM t ORDER BY id");
        let shown: Vec<Vec<&str>> = rows.iter().map(|r| r.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect()).collect();
        assert_eq!(
            shown,
            [
                ["1", "é", "1.50", "2024-01-02 03:04:05", "[\"a\",\"b\"]", "{\"a\":1,\"b\":\"x\"}", "{\"k\":7}", "ok", "170141183460469231731687303715884105727"],
                ["2", "NULL", "-0.01", "NULL", "[]", "NULL", "NULL", "NULL", "-1"],
            ],
            "{release}"
        );
        let db = c.database().to_string();
        let rels = e.core.block_on(c.relations(&db, "main")).unwrap();
        let mut names: Vec<&str> = rels.iter().map(|r| r.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["t", "v"], "{release}");
        let cols = e.core.block_on(c.columns(&db, "main", "t")).unwrap();
        assert!(cols[0].primary_key && cols.len() == 9, "{release}: {cols:?}");
        let ddl = e.core.block_on(c.table_ddl(&db, "main", "t")).unwrap();
        assert!(ddl.contains("id INTEGER PRIMARY KEY") && ddl.contains("feel "), "{release}: {ddl}");
        // Written on: the file takes this release's rows.
        texts(&e, &c, "INSERT INTO t (id, name) VALUES (3, 'new')");
        assert_eq!(texts(&e, &c, "SELECT count(*) FROM v"), [[Some("3".into())]], "{release}");
    }
}

/// SQLite files of every text encoding and an old schema format, with what a file may hold
/// that the catalog has to get through: a view whose table is gone, STRICT and WITHOUT ROWID
/// tables, generated columns, names in other scripts.
#[cfg(feature = "sqlite")]
#[test]
fn sqlite_files_of_every_encoding() {
    let e = env();
    for (encoding, page) in [("UTF-8", 512), ("UTF-16le", 4096), ("UTF-16be", 65536)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("odd.sqlite");
        // Written by another program: the encoding is fixed when the file is first written.
        let w = rusqlite::Connection::open(&path).unwrap();
        w.execute_batch(&format!(
            "PRAGMA encoding = '{encoding}'; PRAGMA page_size = {page};
             CREATE TABLE \"Ünï 表\" (\"ключ\" INTEGER PRIMARY KEY, txt TEXT, blob BLOB, r REAL);
             INSERT INTO \"Ünï 表\" VALUES (1, 'é ☃ 𝄞', x'00ff', 1e308), (2, '', NULL, -0.0);
             CREATE TABLE gone (a);
             CREATE VIEW broken AS SELECT a FROM gone;
             PRAGMA legacy_alter_table = ON;
             DROP TABLE gone;
             CREATE TABLE s (k TEXT PRIMARY KEY, n INTEGER, twice INTEGER GENERATED ALWAYS AS (n * 2) VIRTUAL) STRICT, WITHOUT ROWID;
             INSERT INTO s (k, n) VALUES ('a', 21);"
        ))
        .unwrap();
        drop(w);
        // Schema format 1 (what SQLite 3.0 wrote): the header's byte 44, nothing here needs more.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[44..48].copy_from_slice(&1u32.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();

        let c = open(&e, Engine::Sqlite, &path).unwrap_or_else(|err| panic!("{encoding}: {err:#}"));
        assert_eq!(texts(&e, &c, "PRAGMA encoding"), [[Some(encoding.into())]]);
        let rows = texts(&e, &c, "SELECT \"ключ\", txt, blob, r FROM \"Ünï 表\" ORDER BY 1");
        assert_eq!(rows[0][1].as_deref(), Some("é ☃ 𝄞"), "{encoding}");
        assert_eq!(rows[1][1].as_deref(), Some(""), "{encoding}: an empty text is not NULL");
        assert_eq!(texts(&e, &c, "SELECT twice FROM s"), [[Some("42".into())]], "{encoding}");
        let db = c.database().to_string();
        let rels = e.core.block_on(c.relations(&db, "main")).unwrap();
        let mut names: Vec<&str> = rels.iter().map(|r| r.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["broken", "s", "Ünï 表"], "{encoding}");
        let cols = e.core.block_on(c.columns(&db, "main", "Ünï 表")).unwrap();
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["ключ", "txt", "blob", "r"], "{encoding}");
        let cols = e.core.block_on(c.columns(&db, "main", "s")).unwrap();
        assert_eq!(cols.iter().map(|c| (c.name.as_str(), c.primary_key)).collect::<Vec<_>>(), [("k", true), ("n", false), ("twice", false)], "{encoding}");
        // The broken view: its columns cannot be read, and that is an error, not a view with none.
        let broken = e.core.block_on(c.columns(&db, "main", "broken"));
        assert!(broken.as_ref().is_err_and(|e| format!("{e:#}").contains("gone")), "{encoding}: {broken:?}");
    }
}
