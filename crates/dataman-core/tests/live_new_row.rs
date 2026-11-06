//! What a new row's cells say, read from each engine: nullability, what the server fills, and the
//! UUID a key is offered. Each insert is read back from the table.

mod common;

use common::*;
use dataman_core::config::{ConnectionProfile, Engine};
use dataman_core::edit::{Blank, CellValue, ChangeSet, Editability, EditTarget, Outcome};

struct Case<'a> {
    e: &'a Env,
    c: &'a Arc<Connection>,
    db: &'a str,
}

impl Case<'_> {
    fn target(&self, table: &str) -> (Arc<dataman_core::ResultSet>, EditTarget) {
        let rs = query(self.e, self.c, &format!("SELECT * FROM {table}"));
        match self.e.core.block_on(self.c.edit_target(self.db, &rs, None)).unwrap() {
            Editability::Editable(t) => (rs, t),
            Editability::ReadOnly(why) => panic!("{table}: {why}"),
        }
    }

    fn blanks(&self, table: &str) -> Vec<Blank> {
        let (_, t) = self.target(table);
        (0..t.columns.len()).map(|c| t.blank(c)).collect()
    }

    fn offered(&self, table: &str) -> Option<usize> {
        self.target(table).1.new_row_uuid().map(|(c, _)| c)
    }

    /// Inserts a row with these columns set, the others left to the server. Whether it committed.
    fn insert(&self, table: &str, values: &[(usize, CellValue)]) -> bool {
        let (rs, t) = self.target(table);
        let mut cs = ChangeSet::new();
        cs.insert_row(t.columns.len());
        for (c, v) in values {
            cs.set_inserted(0, *c, Some(v.clone()));
        }
        let plan = cs.plan(&t, &rs.snapshot()).unwrap();
        let r = self.e.core.block_on(self.c.apply_changes(Some(self.db), plan)).unwrap();
        r.committed && r.outcomes.iter().all(|o| matches!(o.outcome, Outcome::Applied { rows: 1 }))
    }

    /// Every cell of the table's only row.
    fn row(&self, table: &str) -> Vec<Option<String>> {
        let rs = query(self.e, self.c, &format!("SELECT * FROM {table}"));
        let snap = rs.snapshot();
        assert_eq!(snap.rows(), 1, "{table}");
        (0..snap.columns.len()).map(|c| snap.text(0, c).map(|v| v.into_owned())).collect()
    }

    /// A UUID key, nothing filling it: it is offered, and the row goes in with it.
    fn uuid_rules(&self, uuid: &str, generator: &str, upper: bool) {
        exec(
            self.e,
            self.c,
            &[
                &format!("CREATE TABLE nr_items (id {uuid} NOT NULL PRIMARY KEY, name varchar(20) NOT NULL, note varchar(20), qty int NOT NULL DEFAULT 5)"),
                &format!("CREATE TABLE nr_served (id {uuid} NOT NULL PRIMARY KEY DEFAULT {generator}, name varchar(20) NOT NULL)"),
                &format!("CREATE TABLE nr_owned (n int NOT NULL PRIMARY KEY, owner {uuid} NOT NULL)"),
                &format!("CREATE TABLE nr_pair (a {uuid} NOT NULL, b {uuid} NOT NULL, PRIMARY KEY (a, b))"),
                &format!("CREATE TABLE nr_profile (id {uuid} NOT NULL PRIMARY KEY REFERENCES nr_items (id))"),
            ],
        );
        assert_eq!(self.blanks("nr_items")[1..], [Blank::Required, Blank::Null, Blank::Default], "a NOT NULL column with no default is required, not a default");
        assert_eq!(self.offered("nr_items"), Some(0));
        assert_eq!(self.offered("nr_served"), None, "the server's generator stays");
        assert_eq!(self.blanks("nr_served")[0], Blank::Default);
        assert_eq!(self.offered("nr_owned"), None, "a UUID that is not the key is someone else's row");
        assert_eq!(self.offered("nr_pair"), None, "a composite key's parts are other rows' too");
        assert_eq!(self.offered("nr_profile"), None, "a key that is a foreign key takes the other row's value");

        let (_, t) = self.target("nr_items");
        let (col, id) = t.new_row_uuid().unwrap();
        let text = id.as_deref().unwrap().to_string();
        assert_eq!(text.parse::<uuid::Uuid>().map(|u| u.get_version_num()), Ok(4), "{text}");
        assert_eq!(text == text.to_uppercase(), upper, "{text}");
        assert!(!self.insert("nr_items", &[(col, id.clone())]), "name is required, and the server says so");
        assert!(self.insert("nr_items", &[(col, id), (1, CellValue::text("a"))]));
        let row = self.row("nr_items");
        assert_eq!(row, [Some(text), Some("a".into()), None, Some("5".into())], "the offered id, NULL and the default as the table holds them");

        assert!(self.insert("nr_served", &[(1, CellValue::text("b"))]));
        let row = self.row("nr_served");
        assert!(row[0].as_deref().is_some_and(|v| !v.is_empty()), "the server generated it: {row:?}");
    }
}

#[test]
fn postgres() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let schema = "nr_new_row";
    exec(&e, &c, &[&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &format!("CREATE SCHEMA {schema}")]);
    let c2 = c.clone();
    let case = Case { e: &e, c: &c2, db: "dataman" };
    let session = format!("{schema}.nr_ident");
    exec(
        &e,
        &c,
        &[
            &format!("CREATE DOMAIN {schema}.must AS int NOT NULL"),
            &format!("CREATE DOMAIN {schema}.five AS int DEFAULT 5"),
            &format!("CREATE TABLE {session} (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, s serial, calc int GENERATED ALWAYS AS (1) STORED, a {schema}.must, b {schema}.five, c int NOT NULL, d int)"),
            &format!("CREATE TABLE {schema}.nr_uuid (id uuid PRIMARY KEY, n int NOT NULL)"),
            &format!("CREATE TABLE {schema}.nr_gen (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), n int NOT NULL)"),
            &format!("CREATE DOMAIN {schema}.guid AS uuid"),
            &format!("CREATE TABLE {schema}.nr_dom (id {schema}.guid PRIMARY KEY)"),
            &format!("CREATE TABLE {schema}.nr_ref (id uuid PRIMARY KEY REFERENCES {schema}.nr_uuid (id))"),
        ],
    );
    assert_eq!(
        case.blanks(&session),
        [Blank::Default, Blank::Default, Blank::Default, Blank::Required, Blank::Default, Blank::Required, Blank::Null],
        "identity, serial, generated, a NOT NULL domain, a domain's default, NOT NULL, nullable"
    );
    let table = format!("{schema}.nr_uuid");
    let (col, id) = case.target(&table).1.new_row_uuid().expect("a uuid key");
    let text = id.as_deref().unwrap().to_string();
    assert!(case.insert(&table, &[(col, id), (1, CellValue::text("1"))]));
    assert_eq!(case.row(&table)[0].as_deref(), Some(text.as_str()));
    assert_eq!(case.offered(&format!("{schema}.nr_gen")), None, "gen_random_uuid() stays");
    assert_eq!(case.offered(&format!("{schema}.nr_dom")), Some(0), "a domain over uuid is a uuid");
    assert_eq!(case.offered(&format!("{schema}.nr_ref")), None);
    exec(&e, &c, &[&format!("DROP SCHEMA {schema} CASCADE")]);
}

#[test]
fn sql_server() {
    let e = env();
    let Some(c0) = mssql_on(&e, mssql_profile("master")) else { return };
    let db = "dataman_new_row";
    mssql_drop(&e, db);
    exec(&e, &c0, &[&format!("CREATE DATABASE {db}")]);
    let Some(c) = mssql_on(&e, mssql_profile(db)) else { return };
    let case = Case { e: &e, c: &c, db };
    case.uuid_rules("uniqueidentifier", "NEWID()", true);
    exec(
        &e,
        &c,
        &[
            "CREATE TYPE dbo.must FROM int NOT NULL",
            "CREATE TABLE nr_cols (id int IDENTITY(1,1) PRIMARY KEY, calc AS (id + 1), rv rowversion, a dbo.must, b bit NOT NULL, c bit NOT NULL DEFAULT 1, d datetime2 NOT NULL DEFAULT SYSDATETIME(), e nvarchar(5) NULL, f nvarchar(5) NOT NULL)",
            "CREATE TABLE nr_seq (id uniqueidentifier NOT NULL PRIMARY KEY DEFAULT NEWSEQUENTIALID())",
        ],
    );
    assert_eq!(
        case.blanks("nr_cols"),
        [Blank::Computed, Blank::Computed, Blank::Computed, Blank::Required, Blank::Required, Blank::Default, Blank::Default, Blank::Null, Blank::Required],
    );
    assert_eq!(case.offered("nr_seq"), None);
    drop(case);
    drop(c);
    mssql_drop(&e, db);
}

#[test]
fn mysql() {
    let e = env();
    let Some(c0) = mysql_on(&e, mysql_profile("mysql")) else { return };
    let db = "dataman_new_row";
    exec(&e, &c0, &[&format!("DROP DATABASE IF EXISTS {db}"), &format!("CREATE DATABASE {db}")]);
    let Some(c) = mysql_on(&e, mysql_profile(db)) else { return };
    let case = Case { e: &e, c: &c, db };
    exec(
        &e,
        &c,
        &[
            "CREATE TABLE nr_cols (id bigint AUTO_INCREMENT PRIMARY KEY, calc int AS (c + 1), a varchar(5) NOT NULL, b varchar(5), c int NOT NULL DEFAULT 1, d timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP)",
            "CREATE TABLE nr_char (id char(36) NOT NULL PRIMARY KEY, n int NOT NULL)",
            "CREATE TABLE nr_gen (id char(36) NOT NULL PRIMARY KEY DEFAULT (uuid()))",
        ],
    );
    assert_eq!(case.blanks("nr_cols"), [Blank::Default, Blank::Default, Blank::Required, Blank::Null, Blank::Default, Blank::Default]);
    assert_eq!(case.offered("nr_char"), None, "MySQL has no UUID type: a char(36) is only text");
    assert_eq!(case.blanks("nr_char"), [Blank::Required, Blank::Required]);
    assert_eq!(case.blanks("nr_gen"), [Blank::Default]);
    exec(&e, &c0, &[&format!("DROP DATABASE {db}")]);
}

#[test]
fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.sqlite");
    dataman_core::import::create_database(Engine::Sqlite, &path).unwrap();
    let e = env();
    let c = e.core.block_on(e.core.connect(ConnectionProfile::new_file(Engine::Sqlite, path.to_string_lossy()), None)).unwrap();
    let case = Case { e: &e, c: &c, db: "main" };
    case.uuid_rules("UUID", "(lower(hex(randomblob(16))))", false);
    exec(&e, &c, &["CREATE TABLE nr_rowid (id INTEGER PRIMARY KEY, a TEXT NOT NULL, b TEXT)"]);
    assert_eq!(case.blanks("nr_rowid"), [Blank::Default, Blank::Required, Blank::Null], "the rowid fills an INTEGER PRIMARY KEY");
}

#[test]
fn duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.duckdb");
    dataman_core::import::create_database(Engine::DuckDb, &path).unwrap();
    let e = env();
    let c = e.core.block_on(e.core.connect(ConnectionProfile::new_file(Engine::DuckDb, path.to_string_lossy()), None)).unwrap();
    let case = Case { e: &e, c: &c, db: "t" };
    case.uuid_rules("UUID", "uuid()", false);
}
