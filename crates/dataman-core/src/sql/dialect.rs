use crate::config::Engine;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Dialect {
    #[default]
    Postgres,
    MySql,
    TSql,
    Sqlite,
    DuckDb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LexRules {
    pub dollar: bool,
    pub escape_strings: bool,
    pub nested_comments: bool,
    pub double_colon: bool,
    pub hash_comments: bool,
    pub mysql_quotes: bool,
    pub tsql_names: bool,
    /// SQL Server reads every C0 control but NUL and Unicode's spaces as space.
    pub unicode_spaces: bool,
    /// A `--` comment ends at a bare `\r` too (PostgreSQL, SQL Server; MySQL's at `\n` only).
    pub cr_ends_comment: bool,
    /// `?` and `?NNN` are parameters (SQLite, DuckDB).
    pub qmark_params: bool,
    /// `:name`, `@name` and `$name` are parameters (SQLite).
    pub named_params: bool,
    /// `[x]` is a name, with none of T-SQL's `@x`/`#x` (SQLite).
    pub bracket_names: bool,
    /// `` `x` `` is a name while `"x"` stays one too (SQLite; MySQL's quotes are `mysql_quotes`).
    pub backtick_names: bool,
}

impl LexRules {
    pub const POSTGRES: LexRules = LexRules {
        dollar: true,
        escape_strings: true,
        nested_comments: true,
        double_colon: true,
        hash_comments: false,
        mysql_quotes: false,
        tsql_names: false,
        unicode_spaces: false,
        cr_ends_comment: true,
        qmark_params: false,
        named_params: false,
        bracket_names: false,
        backtick_names: false,
    };
    pub const MYSQL: LexRules = LexRules {
        dollar: false,
        escape_strings: false,
        nested_comments: false,
        double_colon: false,
        hash_comments: true,
        mysql_quotes: true,
        tsql_names: false,
        unicode_spaces: false,
        cr_ends_comment: false,
        qmark_params: false,
        named_params: false,
        bracket_names: false,
        backtick_names: false,
    };
    pub const TSQL: LexRules = LexRules {
        dollar: false,
        escape_strings: false,
        nested_comments: true,
        double_colon: false,
        hash_comments: false,
        mysql_quotes: false,
        tsql_names: true,
        unicode_spaces: true,
        cr_ends_comment: true,
        qmark_params: false,
        named_params: false,
        bracket_names: false,
        backtick_names: false,
    };
    /// SQLite's tokenizer: `"x"`, `[x]` and `` `x` `` are names, `X'..'` a blob, `?`, `?N`, `:x`,
    /// `@x` and `$x` parameters; comments do not nest and `--` ends at `\n`.
    pub const SQLITE: LexRules = LexRules {
        dollar: false,
        escape_strings: false,
        nested_comments: false,
        double_colon: false,
        hash_comments: false,
        mysql_quotes: false,
        tsql_names: false,
        unicode_spaces: false,
        cr_ends_comment: false,
        qmark_params: true,
        named_params: true,
        bracket_names: true,
        backtick_names: true,
    };
    /// PostgreSQL's rules (`$$`, `E''`, `::`, `$1`) plus `?` parameters.
    pub const DUCKDB: LexRules = LexRules { qmark_params: true, ..LexRules::POSTGRES };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptRules {
    pub begin_atomic: bool,
    pub go_batches: bool,
    pub delimiter_command: bool,
    pub routine_bodies: bool,
    pub exec_comments: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Returning {
    Clause,
    Output,
    Reselect,
}

const PG_RESERVED_EXTRA: &[&str] = &[
    "analyse", "asymmetric", "authorization", "binary", "collation", "current_catalog", "current_role", "current_schema",
    "current_time", "deferrable", "freeze", "initially", "localtime", "localtimestamp", "overlaps", "placing",
    "session_user", "symmetric", "tablesample", "user", "variadic", "verbose",
];

fn plain_mixed_case(name: &str, d: Dialect) -> bool {
    !name.is_empty()
        && name.bytes().next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        && !super::lexer::is_keyword_in(name, d)
}

impl Dialect {
    pub fn of(engine: Engine) -> Dialect {
        match engine {
            Engine::Postgres => Dialect::Postgres,
            Engine::MySql => Dialect::MySql,
            Engine::SqlServer => Dialect::TSql,
            Engine::Sqlite => Dialect::Sqlite,
            Engine::DuckDb => Dialect::DuckDb,
            Engine::MongoDb | Engine::Elasticsearch | Engine::OpenSearch | Engine::Redis => Dialect::Postgres,
        }
    }

    pub fn lex(self) -> LexRules {
        match self {
            Dialect::Postgres => LexRules::POSTGRES,
            Dialect::MySql => LexRules::MYSQL,
            Dialect::TSql => LexRules::TSQL,
            Dialect::Sqlite => LexRules::SQLITE,
            Dialect::DuckDb => LexRules::DUCKDB,
        }
    }

    pub fn script(self) -> ScriptRules {
        match self {
            Dialect::Postgres => ScriptRules { begin_atomic: true, go_batches: false, delimiter_command: false, routine_bodies: false, exec_comments: false },
            Dialect::MySql => ScriptRules { begin_atomic: false, go_batches: false, delimiter_command: true, routine_bodies: true, exec_comments: true },
            Dialect::TSql => ScriptRules { begin_atomic: false, go_batches: true, delimiter_command: false, routine_bodies: false, exec_comments: false },
            // `CREATE TRIGGER … BEGIN …; …; END;` is one statement.
            Dialect::Sqlite => ScriptRules { begin_atomic: false, go_batches: false, delimiter_command: false, routine_bodies: true, exec_comments: false },
            Dialect::DuckDb => ScriptRules { begin_atomic: false, go_batches: false, delimiter_command: false, routine_bodies: false, exec_comments: false },
        }
    }

    pub fn folds_to_lower(self) -> bool {
        self == Dialect::Postgres
    }

    /// Names match whatever their case, quoted or not (SQLite, DuckDB): the case is kept, not
    /// folded.
    pub fn names_ignore_case(self) -> bool {
        matches!(self, Dialect::Sqlite | Dialect::DuckDb)
    }

    pub fn quote_ident(self, name: &str) -> String {
        match self {
            Dialect::Postgres => {
                if PG_RESERVED_EXTRA.contains(&name) {
                    return format!("\"{name}\"");
                }
                super::quote_ident(name)
            }
            Dialect::MySql => {
                if plain_mixed_case(name, self) { name.to_string() } else { format!("`{}`", name.replace('`', "``")) }
            }
            Dialect::TSql => {
                if plain_mixed_case(name, self) { name.to_string() } else { format!("[{}]", name.replace(']', "]]")) }
            }
            Dialect::Sqlite | Dialect::DuckDb => {
                if plain_mixed_case(name, self) { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
            }
        }
    }

    pub fn qualified(self, schema: Option<&str>, name: &str) -> String {
        match schema {
            Some(s) if !s.is_empty() => format!("{}.{}", self.quote_ident(s), self.quote_ident(name)),
            _ => self.quote_ident(name),
        }
    }

    /// PostgreSQL's `standard_conforming_strings` (the default, and LAR's only mode) and T-SQL keep
    /// backslashes plain; MySQL's default mode escapes them. T-SQL's are Unicode (`N'…'`): a plain
    /// `'…'` goes through the database's code page.
    pub fn quote_literal(self, s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 3);
        if self == Dialect::TSql {
            out.push('N');
        }
        out.push('\'');
        for c in s.chars() {
            if c == '\'' || (c == '\\' && self == Dialect::MySql) {
                out.push(c);
            }
            out.push(c);
        }
        out.push('\'');
        out
    }

    pub fn placeholder(self, n: usize) -> String {
        match self {
            Dialect::Postgres => format!("${n}"),
            Dialect::MySql => "?".into(),
            Dialect::TSql => format!("@P{n}"),
            Dialect::Sqlite => format!("?{n}"),
            Dialect::DuckDb => format!("${n}"),
        }
    }

    pub fn page_clause(self, limit: u64, offset: u64) -> String {
        match self {
            Dialect::Postgres | Dialect::MySql | Dialect::Sqlite | Dialect::DuckDb if offset == 0 => format!("LIMIT {limit}"),
            Dialect::Postgres | Dialect::MySql | Dialect::Sqlite | Dialect::DuckDb => format!("LIMIT {limit} OFFSET {offset}"),
            Dialect::TSql => format!("OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"),
        }
    }

    pub fn returning(self) -> Returning {
        match self {
            Dialect::Postgres | Dialect::Sqlite | Dialect::DuckDb => Returning::Clause,
            Dialect::MySql => Returning::Reselect,
            Dialect::TSql => Returning::Output,
        }
    }

    /// SQL Server only has `IS NOT DISTINCT FROM` since 2022: the long form works on every version.
    pub fn not_distinct(self, a: &str, b: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::DuckDb => format!("{a} IS NOT DISTINCT FROM {b}"),
            Dialect::Sqlite => format!("{a} IS {b}"),
            Dialect::MySql => format!("{a} <=> {b}"),
            Dialect::TSql => format!("({a} = {b} OR ({a} IS NULL AND {b} IS NULL))"),
        }
    }

    pub fn text_cast(self, expr: &str) -> String {
        match self {
            Dialect::Postgres => format!("{expr}::text"),
            Dialect::MySql => format!("CAST({expr} AS CHAR)"),
            Dialect::TSql => format!("CAST({expr} AS NVARCHAR(MAX))"),
            Dialect::Sqlite => format!("CAST({expr} AS TEXT)"),
            Dialect::DuckDb => format!("CAST({expr} AS VARCHAR)"),
        }
    }

    /// SQLite's `LIKE` ignores case already (ASCII letters only).
    pub fn ilike(self) -> &'static str {
        match self {
            Dialect::Postgres | Dialect::DuckDb => "ILIKE",
            Dialect::MySql | Dialect::TSql | Dialect::Sqlite => "LIKE",
        }
    }

    pub fn begin(self) -> &'static str {
        match self {
            Dialect::Postgres | Dialect::Sqlite | Dialect::DuckDb => "BEGIN",
            Dialect::MySql => "START TRANSACTION",
            Dialect::TSql => "BEGIN TRANSACTION",
        }
    }

    pub fn begin_read_only(self) -> Option<&'static str> {
        match self {
            Dialect::Postgres => Some("BEGIN READ ONLY"),
            Dialect::MySql => Some("START TRANSACTION READ ONLY"),
            Dialect::DuckDb => Some("BEGIN TRANSACTION READ ONLY"),
            // Read-only comes from the connection itself (opened read-only, `query_only`).
            Dialect::TSql | Dialect::Sqlite => None,
        }
    }

    /// Not valid inside a transaction on MySQL, and undone by a rollback on PostgreSQL: sent between
    /// transactions.
    pub fn session_read_only(self, on: bool) -> Option<&'static str> {
        match (self, on) {
            (Dialect::Postgres, true) => Some("SET default_transaction_read_only = on"),
            (Dialect::Postgres, false) => Some("SET default_transaction_read_only = off"),
            (Dialect::MySql, true) => Some("SET SESSION TRANSACTION READ ONLY"),
            (Dialect::MySql, false) => Some("SET SESSION TRANSACTION READ WRITE"),
            (Dialect::Sqlite, true) => Some("PRAGMA query_only = ON"),
            (Dialect::Sqlite, false) => Some("PRAGMA query_only = OFF"),
            // DuckDB's is the whole database's (`access_mode`): the connection reopens.
            (Dialect::TSql | Dialect::DuckDb, _) => None,
        }
    }

    pub fn commit(self) -> &'static str {
        match self {
            Dialect::Postgres | Dialect::MySql | Dialect::Sqlite | Dialect::DuckDb => "COMMIT",
            Dialect::TSql => "COMMIT TRANSACTION",
        }
    }

    pub fn rollback(self) -> &'static str {
        match self {
            Dialect::Postgres | Dialect::MySql | Dialect::Sqlite | Dialect::DuckDb => "ROLLBACK",
            Dialect::TSql => "ROLLBACK TRANSACTION",
        }
    }

    pub fn savepoint(self, name: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::MySql | Dialect::Sqlite | Dialect::DuckDb => format!("SAVEPOINT {name}"),
            Dialect::TSql => format!("SAVE TRANSACTION {name}"),
        }
    }

    /// T-SQL's savepoints cannot be released: back only.
    pub fn rollback_to(self, name: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::MySql | Dialect::Sqlite | Dialect::DuckDb => format!("ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"),
            Dialect::TSql => format!("ROLLBACK TRANSACTION {name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        let pg = Dialect::Postgres;
        assert_eq!(pg.quote_ident("orders"), "orders");
        assert_eq!(pg.quote_ident("Orders"), "\"Orders\"");
        assert_eq!(pg.quote_ident("user"), "\"user\"");
        assert_eq!(pg.qualified(Some("public"), "a\"b"), "public.\"a\"\"b\"");
        assert_eq!(Dialect::MySql.quote_ident("Orders"), "Orders");
        assert_eq!(Dialect::MySql.quote_ident("a`b c"), "`a``b c`");
        assert_eq!(Dialect::TSql.qualified(Some("dbo"), "order"), "dbo.[order]");
        assert_eq!(Dialect::TSql.quote_ident("a]b"), "[a]]b]");
        assert_eq!(pg.quote_literal("it's \\n"), "'it''s \\n'");
        assert_eq!(Dialect::MySql.quote_literal("it's \\n"), "'it''s \\\\n'");
        assert_eq!(Dialect::TSql.quote_literal("it's \\n é"), "N'it''s \\n é'");
        assert_eq!(Dialect::TSql.not_distinct("a", "N'x'"), "(a = N'x' OR (a IS NULL AND N'x' IS NULL))");
    }

    #[test]
    fn facts() {
        assert_eq!(Dialect::of(Engine::SqlServer), Dialect::TSql);
        assert_eq!(Engine::default().dialect(), Dialect::Postgres);
        assert_eq!([Dialect::Postgres, Dialect::MySql, Dialect::TSql].map(|d| d.placeholder(2)), ["$2".to_string(), "?".into(), "@P2".into()]);
        assert_eq!(Dialect::Postgres.page_clause(100, 0), "LIMIT 100");
        assert_eq!(Dialect::MySql.page_clause(100, 200), "LIMIT 100 OFFSET 200");
        assert_eq!(Dialect::TSql.page_clause(100, 0), "OFFSET 0 ROWS FETCH NEXT 100 ROWS ONLY");
        assert_eq!(Dialect::Postgres.rollback_to("s"), "ROLLBACK TO SAVEPOINT s; RELEASE SAVEPOINT s");
        assert_eq!(Dialect::MySql.not_distinct("a", "1"), "a <=> 1");
        assert_eq!(Dialect::Postgres.lex(), LexRules::POSTGRES);
        assert!(Dialect::Postgres.script().begin_atomic && !Dialect::TSql.script().begin_atomic);
    }
}
