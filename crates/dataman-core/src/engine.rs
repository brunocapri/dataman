//! What Dataman knows of each engine, in one table (as `Dialect`/`LexRules` hold the SQL
//! dialects'): `Engine`'s methods, `Caps`, the MCP schema's text and the connection form read
//! it. A new engine is one more entry here.

use crate::backend::Caps;
use crate::config::{Engine, Family};
use crate::sql::Dialect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineSpec {
    pub engine: Engine,
    /// Stored in the file and used by the MCP tools and the logos: must not change.
    pub slug: &'static str,
    pub label: &'static str,
    pub aliases: &'static [&'static str],
    /// Words the engine picker matches (aliases, vendors, uses).
    pub search: &'static str,
    pub default_port: u16,
    pub default_user: &'static str,
    pub family: Family,
    /// What the connection form calls its family (Elasticsearch is a document store that the
    /// form shows under Search).
    pub family_label: &'static str,
    pub file_exts: &'static [&'static str],
    pub dialect: Option<Dialect>,
    pub caps: Caps,
    pub badge: &'static str,
    pub logo: &'static str,
    /// What a connection opens when the form leaves the database empty ("" = the login's own).
    pub default_database: &'static str,
    /// Where an unqualified table goes when nothing else says (an import's target).
    pub default_schema: Option<&'static str>,
    /// Database names match whatever their case (a read-only override names one).
    pub db_names_ignore_case: bool,
    pub system_dbs: &'static [&'static str],
    /// The schemas the engine keeps for itself (a trailing `*` matches a prefix), left out of
    /// the lists of tables. MySQL's schemas are its databases: its system ones are both.
    pub system_schemas: &'static [&'static str],
    /// Databases are numbered (Redis' 0–15), shown as `db{n}`.
    pub numbered_dbs: Option<u32>,
    pub nouns: Nouns,
    pub form: Form,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Form {
    /// A file instead of a server (host, port, user, password).
    pub file: bool,
    pub user_required: bool,
    pub database: bool,
    pub database_placeholder: &'static str,
    /// Options as `k=v&k=v` (MongoDB's URI options).
    pub options: bool,
    /// The database field's label ("DB" for Redis' numbered ones).
    pub database_label: &'static str,
    pub scheme: &'static str,
}

/// What the engine calls a table, one and many ("collection", "collections").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nouns {
    pub one: &'static str,
    pub many: &'static str,
}

const TABLES: Nouns = Nouns { one: "table", many: "tables" };

const SERVER: Form = Form { file: false, user_required: true, database: true, database_placeholder: "database", options: false, database_label: "Database", scheme: "postgres" };
const FILE: Form = Form { file: true, user_required: false, database: false, database_placeholder: "", options: false, database_label: "Database", scheme: "" };
const NO_FILES: &[&str] = &[];

/// In `Engine`'s declaration order: `Engine::spec` indexes it.
pub static SPECS: [EngineSpec; 9] = [
    EngineSpec {
        engine: Engine::Postgres,
        slug: "postgres",
        label: "PostgreSQL",
        aliases: &["pg", "postgresql"],
        search: "postgresql postgres pg",
        default_port: 5432,
        default_user: "postgres",
        family: Family::Sql,
        family_label: "Relational",
        file_exts: NO_FILES,
        dialect: Some(Dialect::Postgres),
        caps: Caps::POSTGRES,
        badge: "PG",
        logo: "logos/postgres.svg",
        default_database: "postgres",
        default_schema: Some("public"),
        db_names_ignore_case: false,
        system_dbs: &[],
        system_schemas: &["pg_catalog", "information_schema", "pg_toast*", "pg_temp*"],
        numbered_dbs: None,
        nouns: TABLES,
        form: SERVER,
    },
    EngineSpec {
        engine: Engine::MySql,
        slug: "mysql",
        label: "MySQL",
        aliases: &["mariadb"],
        search: "mysql mariadb",
        default_port: 3306,
        default_user: "root",
        family: Family::Sql,
        family_label: "Relational",
        file_exts: NO_FILES,
        dialect: Some(Dialect::MySql),
        caps: Caps::MYSQL,
        badge: "MY",
        logo: "logos/mysql.svg",
        default_database: "",
        default_schema: None,
        db_names_ignore_case: false,
        system_dbs: &["mysql", "information_schema", "performance_schema", "sys"],
        system_schemas: &["mysql", "information_schema", "performance_schema", "sys"],
        numbered_dbs: None,
        nouns: TABLES,
        form: Form { scheme: "mysql", ..SERVER },
    },
    EngineSpec {
        engine: Engine::SqlServer,
        slug: "sql-server",
        label: "SQL Server",
        aliases: &["mssql", "sqlserver", "sql_server"],
        search: "sql server mssql azure microsoft",
        default_port: 1433,
        default_user: "sa",
        family: Family::Sql,
        family_label: "Relational",
        file_exts: NO_FILES,
        dialect: Some(Dialect::TSql),
        caps: Caps::SQL_SERVER,
        badge: "MS",
        logo: "logos/sql-server.svg",
        default_database: "",
        default_schema: Some("dbo"),
        db_names_ignore_case: true,
        system_dbs: &["master", "model", "msdb", "tempdb"],
        system_schemas: &["sys", "INFORMATION_SCHEMA"],
        numbered_dbs: None,
        nouns: TABLES,
        form: Form { scheme: "sqlserver", ..SERVER },
    },
    EngineSpec {
        engine: Engine::MongoDb,
        slug: "mongodb",
        label: "MongoDB",
        aliases: &["mongo"],
        search: "mongodb mongo atlas",
        default_port: 27017,
        default_user: "",
        family: Family::Document,
        family_label: "Document",
        file_exts: NO_FILES,
        dialect: None,
        caps: Caps { field_values: true, ..Caps::NONE },
        badge: "MDB",
        logo: "logos/mongodb.svg",
        default_database: "",
        default_schema: None,
        db_names_ignore_case: false,
        system_dbs: &["admin", "local", "config"],
        system_schemas: &[],
        numbered_dbs: None,
        nouns: Nouns { one: "collection", many: "collections" },
        form: Form { user_required: false, database_placeholder: "test", options: true, scheme: "mongodb", ..SERVER },
    },
    EngineSpec {
        engine: Engine::Elasticsearch,
        slug: "elasticsearch",
        label: "Elasticsearch",
        aliases: &["es", "elastic"],
        search: "elasticsearch elastic es",
        default_port: 9200,
        default_user: "elastic",
        family: Family::Document,
        family_label: "Search",
        file_exts: NO_FILES,
        dialect: None,
        caps: Caps::NONE,
        badge: "ES",
        logo: "logos/elasticsearch.svg",
        default_database: "",
        default_schema: None,
        db_names_ignore_case: false,
        system_dbs: &[],
        system_schemas: &[],
        numbered_dbs: None,
        nouns: Nouns { one: "index", many: "indices" },
        form: Form { user_required: false, database: false, database_placeholder: "", scheme: "https", ..SERVER },
    },
    EngineSpec {
        engine: Engine::OpenSearch,
        slug: "opensearch",
        label: "OpenSearch",
        aliases: &["os"],
        search: "opensearch aws os",
        default_port: 9200,
        default_user: "admin",
        family: Family::Document,
        family_label: "Search",
        file_exts: NO_FILES,
        dialect: None,
        caps: Caps::NONE,
        badge: "OS",
        logo: "logos/opensearch.svg",
        default_database: "",
        default_schema: None,
        db_names_ignore_case: false,
        system_dbs: &[],
        system_schemas: &[],
        numbered_dbs: None,
        nouns: Nouns { one: "index", many: "indices" },
        form: Form { user_required: false, database: false, database_placeholder: "", scheme: "https", ..SERVER },
    },
    EngineSpec {
        engine: Engine::Redis,
        slug: "redis",
        label: "Redis",
        aliases: &[],
        search: "redis valkey cache",
        default_port: 6379,
        default_user: "",
        family: Family::KeyValue,
        family_label: "Key-value",
        file_exts: NO_FILES,
        dialect: None,
        caps: Caps::NONE,
        badge: "RDS",
        logo: "logos/redis.svg",
        default_database: "",
        default_schema: None,
        db_names_ignore_case: false,
        system_dbs: &[],
        system_schemas: &[],
        numbered_dbs: Some(16),
        nouns: Nouns { one: "key", many: "keys" },
        form: Form { user_required: false, database_placeholder: "0", database_label: "DB", scheme: "redis", ..SERVER },
    },
    EngineSpec {
        engine: Engine::Sqlite,
        slug: "sqlite",
        label: "SQLite",
        aliases: &["sqlite3", "lite"],
        search: "sqlite sqlite3 db file local embedded",
        default_port: 0,
        default_user: "",
        family: Family::Sql,
        family_label: "Relational",
        file_exts: &["sqlite", "sqlite3", "db", "db3"],
        dialect: Some(Dialect::Sqlite),
        caps: Caps::SQLITE,
        badge: "LITE",
        logo: "logos/sqlite.svg",
        default_database: "",
        default_schema: Some("main"),
        db_names_ignore_case: false,
        system_dbs: &[],
        system_schemas: &[],
        numbered_dbs: None,
        nouns: TABLES,
        form: FILE,
    },
    EngineSpec {
        engine: Engine::DuckDb,
        slug: "duckdb",
        label: "DuckDB",
        aliases: &["duck"],
        search: "duckdb duck analytics olap parquet file local",
        default_port: 0,
        default_user: "",
        family: Family::Sql,
        family_label: "Relational",
        file_exts: &["duckdb", "ddb"],
        dialect: Some(Dialect::DuckDb),
        caps: Caps::DUCKDB,
        badge: "DUCK",
        logo: "logos/duckdb.svg",
        default_database: "",
        default_schema: Some("main"),
        db_names_ignore_case: false,
        system_dbs: &[],
        system_schemas: &["information_schema", "pg_catalog"],
        numbered_dbs: None,
        nouns: TABLES,
        form: FILE,
    },
];

impl Engine {
    pub fn spec(self) -> &'static EngineSpec {
        &SPECS[self as usize]
    }

    /// Its SQL dialect; none for the NoSQL engines (`dialect()` gives them PostgreSQL's).
    pub fn sql_dialect(self) -> Option<Dialect> {
        self.spec().dialect
    }

    pub fn caps(self) -> Caps {
        self.spec().caps
    }

    pub fn badge(self) -> &'static str {
        self.spec().badge
    }

    pub fn logo(self) -> &'static str {
        self.spec().logo
    }

    pub fn family_label(self) -> &'static str {
        self.spec().family_label
    }

    pub fn default_schema(self) -> Option<&'static str> {
        self.spec().default_schema
    }

    /// How a database is shown: Redis' numbered ones as `db3`.
    pub fn db_label(self, db: &str) -> String {
        if self.spec().numbered_dbs.is_some() { format!("db{db}") } else { db.to_string() }
    }

    pub fn is_system_db(self, db: &str) -> bool {
        self.spec().system_dbs.contains(&db)
    }

    pub fn is_system_schema(self, schema: &str) -> bool {
        let s = self.spec();
        s.system_schemas.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => schema.starts_with(prefix),
            None if s.db_names_ignore_case => schema.eq_ignore_ascii_case(p),
            None => schema == *p,
        })
    }

    pub fn nouns(self) -> Nouns {
        self.spec().nouns
    }

    pub fn port_list() -> String {
        let mut ports: Vec<String> = Vec::new();
        for s in &SPECS {
            let p = s.default_port.to_string();
            if s.default_port != 0 && !ports.contains(&p) {
                ports.push(p);
            }
        }
        ports.join(" / ")
    }

    pub fn slugs() -> Vec<&'static str> {
        Engine::ALL.iter().map(|e| e.slug()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_spec_per_engine_in_order() {
        for e in Engine::ALL {
            assert_eq!(e.spec().engine, e);
            assert_eq!(Engine::parse(e.slug()), Some(e));
            for a in e.spec().aliases {
                assert_eq!(Engine::parse(a), Some(e), "{a}");
            }
            assert_eq!(e.spec().caps.file, e.spec().form.file, "{e:?}");
            assert_eq!(e.is_sql(), e.sql_dialect().is_some(), "{e:?}");
        }
        assert_eq!(Engine::port_list(), "5432 / 3306 / 1433 / 27017 / 9200 / 6379");
        assert_eq!((Engine::Redis.db_label("3"), Engine::MySql.db_label("shop")), ("db3".to_string(), "shop".to_string()));
        assert!(Engine::SqlServer.is_system_db("tempdb") && !Engine::Postgres.is_system_db("postgres"));
        assert!(Engine::Postgres.is_system_schema("pg_toast_temp_3") && !Engine::Postgres.is_system_schema("public"));
        assert!(Engine::SqlServer.is_system_schema("information_schema") && !Engine::SqlServer.is_system_schema("dbo"));
        assert!(Engine::MySql.is_system_schema("performance_schema") && !Engine::DuckDb.is_system_schema("sys"));
        assert_eq!((Engine::Redis.nouns().many, Engine::Sqlite.nouns().one), ("keys", "table"));
        assert_eq!((Engine::Redis.spec().form.database_label, Engine::MongoDb.spec().form.database_label), ("DB", "Database"));
    }
}
