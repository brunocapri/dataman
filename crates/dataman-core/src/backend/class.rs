//! What an engine's error code means, per backend: callers match on [`ErrorClass`], never on
//! the code (whose numbers collide across engines: 1205 is MySQL's lock wait and SQL Server's
//! deadlock) or the message (in the server's language).

use crate::config::Engine;
use crate::result::ErrorClass;

pub fn of(engine: Engine, code: &str) -> ErrorClass {
    match engine {
        Engine::Postgres => pg(code),
        Engine::MySql => my(code),
        Engine::SqlServer => ms(code),
        Engine::Sqlite => lite(code),
        Engine::DuckDb => duck(code),
        Engine::MongoDb => mongo(code),
        Engine::Redis => redis(code),
        Engine::Elasticsearch | Engine::OpenSearch => search(code),
    }
}

/// SQLSTATE.
fn pg(code: &str) -> ErrorClass {
    use ErrorClass::*;
    match code {
        "23505" | "23P01" => Unique,
        "23502" => NotNull,
        "23503" => Fk,
        "23514" => Check,
        "40P01" => Deadlock,
        "40001" => Serialization,
        "55P03" => LockTimeout,
        "25006" => ReadOnly,
        "25001" => TxBlock,
        "57014" => Cancelled,
        "0A000" => Unsupported,
        "42601" => Syntax,
        "42P01" | "42703" | "42883" | "42704" | "3D000" | "3F000" => Undefined,
        "42501" => Permission,
        c if c.starts_with("28") => Auth,
        c if c.starts_with("22") => Data,
        c if c.starts_with("23") => Check,
        c if c.starts_with("08") => Connection,
        _ => Other,
    }
}

fn my(code: &str) -> ErrorClass {
    use ErrorClass::*;
    match code {
        "1045" | "1698" | "1251" | "2049" => Auth,
        "1044" | "1142" | "1143" | "1227" | "1370" => Permission,
        "1062" | "1586" => Unique,
        "1048" | "1364" => NotNull,
        "1451" | "1452" | "1216" | "1217" => Fk,
        "3819" => Check,
        "1213" => Deadlock,
        "1205" => LockTimeout,
        "1292" | "1366" | "1265" | "1406" | "1264" | "1690" | "1367" | "1411" => Data,
        "1792" | "1290" | "1836" => ReadOnly,
        "1317" => Cancelled,
        "3024" | "1969" => Timeout,
        "1064" | "1149" => Syntax,
        "1146" | "1054" | "1049" | "1305" => Undefined,
        "1235" => Unsupported,
        "2002" | "2003" | "2006" | "2013" => Connection,
        _ => Other,
    }
}

fn ms(code: &str) -> ErrorClass {
    use ErrorClass::*;
    match code {
        "18456" | "18452" | "18486" | "18487" | "18488" => Auth,
        "229" | "230" | "262" | "297" | "300" | "916" | "4060" => Permission,
        "2627" | "2601" => Unique,
        "515" => NotNull,
        // 547 is a FOREIGN KEY or a CHECK constraint: the row points nowhere more often.
        "547" => Fk,
        "1205" => Deadlock,
        "1222" => LockTimeout,
        "3906" | "3908" => ReadOnly,
        "3902" | "3903" | "3930" => TxBlock,
        "245" | "8114" | "241" | "242" | "8115" | "8152" | "2628" | "220" | "232" | "8134" | "248" | "295" => Data,
        "102" | "156" | "170" | "105" => Syntax,
        "208" | "207" | "2812" | "911" | "4121" => Undefined,
        "40613" | "10054" | "10060" => Connection,
        _ => Other,
    }
}

fn lite(code: &str) -> ErrorClass {
    use ErrorClass::*;
    match code {
        "SQLITE_CONSTRAINT_UNIQUE" | "SQLITE_CONSTRAINT_PRIMARYKEY" => Unique,
        "SQLITE_CONSTRAINT_NOTNULL" => NotNull,
        "SQLITE_CONSTRAINT_FOREIGNKEY" => Fk,
        "SQLITE_CONSTRAINT_CHECK" | "SQLITE_CONSTRAINT" | "SQLITE_CONSTRAINT_TRIGGER" => Check,
        "SQLITE_CONSTRAINT_DATATYPE" | "SQLITE_MISMATCH" | "SQLITE_TOOBIG" | "SQLITE_RANGE" => Data,
        "SQLITE_BUSY" | "SQLITE_LOCKED" => LockTimeout,
        "SQLITE_READONLY" => ReadOnly,
        "SQLITE_INTERRUPT" => Cancelled,
        "SQLITE_AUTH" | "SQLITE_PERM" => Permission,
        "SQLITE_NOTADB" | "SQLITE_CANTOPEN" => Connection,
        _ => Other,
    }
}

fn duck(code: &str) -> ErrorClass {
    use ErrorClass::*;
    match code {
        "DUCKDB_CONSTRAINT_UNIQUE" => Unique,
        "DUCKDB_CONSTRAINT_NOTNULL" => NotNull,
        "DUCKDB_CONSTRAINT_FOREIGNKEY" => Fk,
        "DUCKDB_CONSTRAINT_CHECK" | "DUCKDB_CONSTRAINT" => Check,
        "TransactionContext Error" => Serialization,
        "Conversion Error" | "Out of Range Error" | "Invalid Input Error" => Data,
        "Permission Error" => Permission,
        "Interrupt Error" => Cancelled,
        "Parser Error" => Syntax,
        "Catalog Error" | "Binder Error" => Undefined,
        "Not implemented Error" => Unsupported,
        "IO Error" => Connection,
        _ => Other,
    }
}

/// Server error codes (`AuthenticationFailed` is 18, `Unauthorized` 13).
fn mongo(code: &str) -> ErrorClass {
    use ErrorClass::*;
    match code {
        "18" => Auth,
        "13" => Permission,
        "11000" | "11001" => Unique,
        "50" => Timeout,
        "11601" => Cancelled,
        "112" | "251" => Serialization,
        "121" => Check,
        "26" => Undefined,
        _ => Other,
    }
}

/// The reply's first word.
fn redis(code: &str) -> ErrorClass {
    use ErrorClass::*;
    match code {
        "WRONGPASS" | "NOAUTH" => Auth,
        "NOPERM" => Permission,
        "READONLY" => ReadOnly,
        "BUSY" => LockTimeout,
        "WRONGTYPE" => Data,
        _ => Other,
    }
}

/// HTTP status, or the error's `type`.
fn search(code: &str) -> ErrorClass {
    use ErrorClass::*;
    match code {
        "401" | "security_exception" => Auth,
        "403" => Permission,
        "409" | "version_conflict_engine_exception" => Unique,
        "404" | "index_not_found_exception" => Undefined,
        "400" | "mapper_parsing_exception" | "document_parsing_exception" => Data,
        "parsing_exception" => Syntax,
        "408" | "504" => Timeout,
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ErrorClass::*;

    #[test]
    fn codes_mean_what_their_engine_says() {
        // 1205: MySQL's lock wait, SQL Server's deadlock.
        assert_eq!((of(Engine::MySql, "1205"), of(Engine::SqlServer, "1205")), (LockTimeout, Deadlock));
        for (e, c) in [
            (Engine::Postgres, "28P01"),
            (Engine::Postgres, "28000"),
            (Engine::MySql, "1045"),
            (Engine::SqlServer, "18456"),
            (Engine::MongoDb, "18"),
            (Engine::Redis, "WRONGPASS"),
        ] {
            assert_eq!(of(e, c), Auth, "{e:?} {c}");
        }
        assert_eq!(of(Engine::Postgres, "42501"), Permission);
        assert_eq!(of(Engine::Postgres, "22P02"), Data);
        assert_eq!(of(Engine::Sqlite, "SQLITE_CONSTRAINT_NOTNULL"), NotNull);
        assert!(of(Engine::SqlServer, "2627").carries_values() && of(Engine::MySql, "1062").carries_values());
        assert!(!of(Engine::Postgres, "42P01").carries_values());
    }
}
