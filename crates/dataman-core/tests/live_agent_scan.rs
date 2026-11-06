//! Scan again on real engines: what it suggests from the names in each engine's catalog and the
//! values in the columns, and that an entry already there stands whatever its spelling.

mod common;

use common::*;
use dataman_core::agent_scan::Scan;
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::nosql::Page;
use dataman_core::privacy::{AgentRules, AgentView, Mask, Names, SensitiveCol, TypeSettings, Types};

fn scan_with(e: &Env, c: &Arc<Connection>, db: &str, rules: &AgentRules, types: Types) -> Scan {
    e.core.block_on(c.scan_sensitive(db, rules.clone(), types)).unwrap()
}

fn scan(e: &Env, c: &Arc<Connection>, db: &str, rules: &AgentRules) -> Scan {
    scan_with(e, c, db, rules, Types::new(&TypeSettings::default()))
}

/// `table.column`, in the catalog's spelling, sorted.
fn listed(s: &Scan) -> Vec<String> {
    let mut v: Vec<String> = s.found.iter().map(|f| format!("{}.{}", f.table, f.column)).collect();
    v.sort();
    v
}

fn mask_of(s: &Scan, table: &str, column: &str) -> Option<Mask> {
    s.found.iter().find(|f| f.table == table && f.column == column).map(|f| f.how)
}

fn entry(col: &str, on: bool) -> SensitiveCol {
    SensitiveCol { col: col.into(), how: Mask::Hidden, on }
}

const SCHEMA_MS: &[&str] = &[
    // No rows at all.
    "CREATE TABLE dbo.Blank (Id int PRIMARY KEY, Phone nvarchar(40), Token nvarchar(200), TokenHash varbinary(64), ApiKey nvarchar(200), Password nvarchar(200), ClientSecret nvarchar(200), UserPassword nvarchar(200), CustomerCPF char(11), AccessToken nvarchar(200), Notes nvarchar(400))",
    // Rows, but nothing in these columns.
    "CREATE TABLE dbo.Nulls (Id int PRIMARY KEY, Phone nvarchar(40) NOT NULL DEFAULT '', Token nvarchar(200), Address nvarchar(200))",
    "INSERT INTO dbo.Nulls (Id) VALUES (1), (2), (3)",
    "CREATE TABLE dbo.Filled (Id int PRIMARY KEY, Phone nvarchar(40), Mobile nvarchar(40), Token nvarchar(200), JwtToken nvarchar(400), TokenHash varbinary(64), mfa_enabled bit, Notes nvarchar(400), [Zip] int)",
    "INSERT INTO dbo.Filled VALUES (1, N'(11) 91234-5678', N'415-555-0132', N'abc123deadbeefcafe', N'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc', 0xDEADBEEF, 1, N'sk_live_abcdefghij012345', 90210)",
];

#[test]
fn sql_server_capitalized_names() {
    let e = env();
    let db = "dat36_scan".to_string();
    let Some(m) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, &db);
    exec(&e, &m, &[&format!("CREATE DATABASE [{db}]")]);
    let c = mssql_on(&e, mssql_profile(&db)).unwrap();
    exec(&e, &c, SCHEMA_MS);
    let none = AgentRules::default();
    let s = scan(&e, &c, &db, &none);
    assert_eq!(
        listed(&s),
        [
            // The names say it all: suggested though the table has no rows.
            "Blank.ApiKey",
            "Blank.ClientSecret",
            "Blank.CustomerCPF",
            "Blank.Password",
            "Blank.UserPassword",
            // Values the Masks don't know.
            "Filled.Phone",
            "Filled.Token",
            "Filled.Zip",
        ],
        "not Blank.Phone, Token, TokenHash, AccessToken (empty), Nulls.* (NULL or blank), Filled.TokenHash (a digest), Filled.mfa_enabled (a flag), Filled.Mobile and JwtToken (the Masks have them), Filled.Notes (no name hit)"
    );
    assert_eq!(mask_of(&s, "Filled", "Phone"), Some(Mask::PARTIAL));
    assert_eq!(mask_of(&s, "Filled", "Token"), Some(Mask::Hidden));
    assert_eq!(mask_of(&s, "Blank", "ApiKey"), Some(Mask::Hidden));

    // The Phone mask off: nothing masks Mobile's value any more.
    let mut off = TypeSettings::default();
    off.set_builtin("phone", Some(false), None);
    assert!(listed(&scan_with(&e, &c, &db, &none, Types::new(&off))).contains(&"Filled.Mobile".to_string()));

    // Scanning again adds nothing, whatever the spelling of what is there.
    let q = Names::of(Engine::SqlServer);
    let mut rules = AgentRules::default();
    assert_eq!(s.add_to(&mut rules, q), 8);
    assert_eq!(rules.sensitive.iter().find(|r| r.col == "Filled.Token").map(|r| r.how), Some(Mask::Hidden), "the catalog's spelling");
    assert_eq!(scan(&e, &c, &db, &rules).add_to(&mut rules.clone(), q), 0);
    let spelled = AgentRules {
        sensitive: vec![entry("FILLED.PHONE", true), entry("dbo.Filled.Token", true), entry("dbo.filled.zip", true), entry("*secret*", true), entry("Blank.APIKEY", false), entry("blank.password", true), entry("BLANK.USERPASSWORD", true), entry("DBO.BLANK.CUSTOMERCPF", true)],
        ..Default::default()
    };
    let mut again = spelled.clone();
    assert_eq!(scan(&e, &c, &db, &spelled).add_to(&mut again, q), 0, "{:?}", again.sensitive);
    assert_eq!(again, spelled, "an entry switched off is a decision too");

    // A rule on any spelling covers every spelling of the column, as the server folds them.
    let masked = AgentRules { view: AgentView::Masked, sensitive: vec![entry("Filled.Token", true)], ..Default::default() };
    for name in ["Token", "token", "TOKEN", "tOkEn"] {
        let read = query(&e, &c, &format!("SELECT {name} FROM dbo.Filled"));
        assert_eq!(read.phase(), Phase::Done, "the server takes {name} as the column");
        assert_eq!(masked.sensitive_mask(Some("dbo"), "Filled", name, q), Some(Mask::Hidden), "{name}");
        assert_eq!(masked.sensitive_mask(Some("DBO"), "FILLED", name, q), Some(Mask::Hidden), "{name}");
    }

    // Another session changes the catalog: the scan reads it as it is now.
    let other = mssql_on(&e, mssql_profile(&db)).unwrap();
    exec(&e, &other, &["ALTER TABLE dbo.Blank ADD ApiSecret nvarchar(200)", "ALTER TABLE dbo.Blank DROP COLUMN ClientSecret"]);
    other.close();
    let now = listed(&scan(&e, &c, &db, &none));
    assert!(now.contains(&"Blank.ApiSecret".to_string()) && !now.contains(&"Blank.ClientSecret".to_string()), "{now:?}");

    // A scan never drops what is there: a rule on an empty column stays.
    let kept = AgentRules { sensitive: vec![entry("Blank.Phone", true)], ..Default::default() };
    let mut after = kept.clone();
    scan(&e, &c, &db, &kept).add_to(&mut after, q);
    assert!(after.sensitive.iter().any(|r| r.col == "Blank.Phone" && r.on), "{:?}", after.sensitive);

    c.close();
    m.close();
    mssql_drop(&e, &db);
}

/// A read that can't finish (a writer holds the rows) is no evidence the column is empty.
#[test]
fn sql_server_blocked_read_keeps_the_suggestion() {
    let e = env();
    let db = "dat36_lock".to_string();
    let Some(m) = mssql_on(&e, mssql_profile("master")) else { return };
    mssql_drop(&e, &db);
    exec(&e, &m, &[&format!("CREATE DATABASE [{db}]")]);
    let c = mssql_on(&e, mssql_profile(&db)).unwrap();
    exec(&e, &c, &["CREATE TABLE dbo.Held (Id int PRIMARY KEY, Token nvarchar(200))"]);
    assert_eq!(listed(&scan(&e, &c, &db, &AgentRules::default())), Vec::<String>::new(), "empty, and readable");
    let writer = c.session(None);
    for sql in ["BEGIN TRANSACTION", "INSERT INTO dbo.Held VALUES (1, N'abc')"] {
        let rs = writer.execute(sql);
        assert_eq!(e.core.block_on(rs.wait_finished()), Phase::Done, "{sql}: {:?}", rs.status().error);
    }
    let t = Instant::now();
    let s = scan(&e, &c, &db, &AgentRules::default());
    assert_eq!(listed(&s), ["Held.Token"]);
    assert!(t.elapsed() < Duration::from_secs(15), "bounded: {:?}", t.elapsed());
    let rs = writer.execute("ROLLBACK");
    e.core.block_on(rs.wait_finished());
    c.close();
    m.close();
    mssql_drop(&e, &db);
}

/// PostgreSQL keeps quoted names apart: `"Token"` and `token` are two columns, and a rule on one
/// (matched as every rule is, ignoring case) masks both, so one entry is enough for both.
#[test]
fn postgres_quoted_names() {
    let e = env();
    let Some(c) = pg(&e) else { return };
    let (blank, both) = ("Dat36Blank", "Dat36Both");
    let db = c.database().to_string();
    let drop = format!(r#"DROP TABLE IF EXISTS public."{blank}", public."{both}""#);
    exec_quiet(&e, &c, &[&drop]);
    exec(&e, &c, &[
        &format!(r#"CREATE TABLE public."{blank}" (id int, "Phone" text, "Token" text, "TokenHash" text, "ApiKey" text, "Password" text)"#),
        &format!(r#"CREATE TABLE public."{both}" (id int, "Token" text, token text)"#),
        &format!(r#"INSERT INTO public."{both}" VALUES (1, 'abc123deadbeefcafe', 'zzz123deadbeefcafe')"#),
    ]);
    c.schema.invalidate(None);
    let found = scan(&e, &c, &db, &AgentRules::default());
    let mine: Vec<String> = listed(&found).into_iter().filter(|f| f.starts_with(blank) || f.starts_with(both)).collect();
    assert_eq!(mine, [format!("{blank}.ApiKey"), format!("{blank}.Password"), format!("{both}.Token"), format!("{both}.token")], "only the sure names in the empty table");
    let q = Names::of(Engine::Postgres);
    let rule = AgentRules { view: AgentView::Masked, sensitive: vec![entry(&format!("{both}.Token"), true)], ..Default::default() };
    for name in ["Token", "token"] {
        assert_eq!(rule.sensitive_mask(Some("public"), both, name, q), Some(Mask::Hidden), "{name}: a rule never misses a column by its case");
        assert!(rule.names_column(Some("public"), both, name, q), "{name}");
    }
    exec_quiet(&e, &c, &[&drop]);
    c.close();
}

#[test]
fn mysql_capitalized_names() {
    let e = env();
    let Some(m) = mysql_on(&e, mysql_profile("")) else { return };
    let db = "dat36_scan".to_string();
    exec_quiet(&e, &m, &[&format!("DROP DATABASE IF EXISTS `{db}`")]);
    exec(&e, &m, &[&format!("CREATE DATABASE `{db}`")]);
    let c = mysql_on(&e, mysql_profile(&db)).unwrap();
    exec(&e, &c, &[
        "CREATE TABLE Blank (Id int PRIMARY KEY, Phone varchar(40), Token varchar(200), TokenHash varbinary(64), ApiKey varchar(200))",
        "CREATE TABLE Filled (Id int PRIMARY KEY, Phone varchar(40), Token varchar(200))",
        "INSERT INTO Filled VALUES (1, '415-555-0132', 'abc123deadbeefcafe')",
    ]);
    assert_eq!(listed(&scan(&e, &c, &db, &AgentRules::default())), ["Blank.ApiKey", "Filled.Token"]);
    c.close();
    exec_quiet(&e, &m, &[&format!("DROP DATABASE IF EXISTS `{db}`")]);
    m.close();
}

fn nosql_profile(engine: Engine, var: &str, at: &str, user: &str, database: &str) -> ConnectionProfile {
    let v = std::env::var(var).unwrap_or_else(|_| at.into());
    let (host, port) = v.rsplit_once(':').unwrap();
    ConnectionProfile { host: host.into(), port: port.parse().unwrap(), user: user.into(), database: database.into(), sslmode: SslMode::Disable, engine, ..ConnectionProfile::new("scan") }
}

/// A field that only ever holds null has nothing to mask; nested ones count by their last part.
#[test]
fn mongodb_fields_are_read() {
    let e = env();
    let db = "dat36_scan".to_string();
    let pw = std::env::var("DATAMAN_MONGO_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let Ok(c) = e.core.block_on(e.core.connect(nosql_profile(Engine::MongoDb, "DATAMAN_MONGO", "127.0.0.1:27027", "root", &db), Some(pw))) else {
        eprintln!("skipping: no MongoDB");
        return;
    };
    assert!(Engine::MongoDb.caps().field_values);
    let run = |text: &str| e.core.block_on(c.run_nosql(&db, text, Page::default())).unwrap_or_else(|err| panic!("{text}: {err:#}"));
    run("db.dropDatabase()");
    run(r#"db.accounts.insertOne({Phone: null, Token: "abc123deadbeefcafe", TokenHash: "abc", ApiKey: null, Mobile: "415-555-0132", Contact: {Token: null}})"#);
    let s = scan(&e, &c, &db, &AgentRules::default());
    assert_eq!(listed(&s), ["accounts.ApiKey", "accounts.Token"]);
    let q = Names::of(Engine::MongoDb);
    let mut rules = AgentRules { sensitive: vec![entry("ACCOUNTS.TOKEN", true)], ..Default::default() };
    assert_eq!(s.add_to(&mut rules, q), 1, "{:?}", rules.sensitive);
    assert_eq!(scan(&e, &c, &db, &rules).add_to(&mut rules.clone(), q), 0);
    run("db.dropDatabase()");
    c.close();
}

/// No field's values can be read on a search engine: a name hit stays, on what is mapped.
#[test]
fn elasticsearch_keeps_names_it_cannot_check() {
    let e = env();
    let pw = std::env::var("DATAMAN_ES_PASSWORD").unwrap_or_else(|_| "dataman".into());
    let Ok(c) = e.core.block_on(e.core.connect(nosql_profile(Engine::Elasticsearch, "DATAMAN_ES", "127.0.0.1:9201", "elastic", ""), Some(pw))) else {
        eprintln!("skipping: no Elasticsearch");
        return;
    };
    assert!(!Engine::Elasticsearch.caps().field_values);
    let idx = "dat36-scan";
    let db = c.database().to_string();
    let run = |text: String| e.core.block_on(c.run_nosql(&db, &text, Page::default()));
    let _ = run(format!("DELETE /{idx}"));
    run(format!(r#"PUT /{idx}
{{"mappings": {{"properties": {{"phone": {{"type": "keyword"}}, "token_hash": {{"type": "keyword"}}, "api_key": {{"type": "keyword"}}}}}}}}"#)).unwrap();
    let s = scan(&e, &c, &db, &AgentRules::default());
    let found: Vec<String> = listed(&s).into_iter().filter(|f| f.starts_with(idx)).collect();
    run(format!("DELETE /{idx}")).unwrap();
    c.close();
    assert_eq!(found, [format!("{idx}.api_key"), format!("{idx}.phone")]);
}
