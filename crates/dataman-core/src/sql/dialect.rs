//! What differs between the SQL of the engines Dataman speaks to, in one place.
//!
//! The lexer, the splitter and the SQL Dataman writes (data views, grid edits, transaction
//! control) ask a [`Dialect`] for every engine-specific bit: identifier quoting, which literal and
//! comment forms exist ([`LexRules`]), how a script splits ([`ScriptRules`]), paging, parameter
//! placeholders, how a write hands back its rows ([`Returning`]), transaction control.
//!
//! PostgreSQL (and LAR) is the dialect everything was written for. MySQL's own forms (`#`
//! comments, backticks, backslash escapes, `"…"` strings, `?` placeholders, `DELIMITER`) and
//! T-SQL's (`[brackets]`, `@variables`, `#temp` tables, `GO` batches, `BEGIN … END` blocks) are
//! lexed and split in their dialect.

use crate::config::Engine;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Dialect {
    /// PostgreSQL and LAR.
    #[default]
    Postgres,
    MySql,
    /// SQL Server's Transact-SQL.
    TSql,
}

/// The lexical forms a dialect has beyond the common core (`'…'` strings with doubled quotes,
/// `"…"` identifiers, `--` and `/* */` comments, `N'…'`, `X'…'` and `B'…'` literals).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LexRules {
    /// `$tag$ … $tag$` bodies and `$1` parameters (PostgreSQL).
    pub dollar: bool,
    /// `E'…'` strings with backslash escapes and `U&'…'` / `U&"…"` (PostgreSQL).
    pub escape_strings: bool,
    /// Block comments nest (PostgreSQL, T-SQL).
    pub nested_comments: bool,
    /// `::` casts (PostgreSQL); elsewhere `:` is punctuation only.
    pub double_colon: bool,
    /// MySQL: `#` starts a line comment; `--` only does when a space (or the line's end)
    /// follows (`5--3` is arithmetic).
    pub hash_comments: bool,
    /// MySQL: `` `identifiers` `` (a doubled backtick inside), `"…"` is a string (the default
    /// `sql_mode`, without `ANSI_QUOTES`), a backslash escapes the next character inside
    /// strings, `?` is a placeholder and `@var` / `@@var` are variables.
    pub mysql_quotes: bool,
    /// T-SQL: `[identifiers]` (a doubled `]` inside), `@var` / `@@global` variables and
    /// `#temp` / `##temp` table names are one name each.
    pub tsql_names: bool,
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
    };
    pub const MYSQL: LexRules = LexRules {
        dollar: false,
        escape_strings: false,
        nested_comments: false,
        double_colon: false,
        hash_comments: true,
        mysql_quotes: true,
        tsql_names: false,
    };
    pub const TSQL: LexRules = LexRules {
        dollar: false,
        escape_strings: false,
        nested_comments: true,
        double_colon: false,
        hash_comments: false,
        mysql_quotes: false,
        tsql_names: true,
    };
}

/// How a script splits into statements beyond top-level `;`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptRules {
    /// SQL-standard bodies (`BEGIN ATOMIC … END`) keep their `;` (PostgreSQL 14+).
    pub begin_atomic: bool,
    /// A line holding only `GO` (and an optional repeat count) ends a batch (SQL Server tools);
    /// `BEGIN … END` blocks keep their `;`, and the body of `CREATE PROCEDURE | FUNCTION |
    /// TRIGGER` runs to the end of its batch.
    pub go_batches: bool,
    /// `DELIMITER //` on a line of its own changes the terminator (MySQL clients), so bodies
    /// of procedures and triggers keep their `;`.
    pub delimiter_command: bool,
    /// `CREATE PROCEDURE | FUNCTION | TRIGGER | EVENT` keeps its `BEGIN … END` body (MySQL), with
    /// or without a `DELIMITER` line: clients like Workbench and DataGrip send it whole.
    pub routine_bodies: bool,
}

/// How the rows a write touched come back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Returning {
    /// `INSERT … RETURNING *` (PostgreSQL).
    Clause,
    /// `INSERT … OUTPUT inserted.* VALUES …` (T-SQL).
    Output,
    /// Nothing: read them again by key (MySQL's `LAST_INSERT_ID()`).
    Reselect,
}

/// PostgreSQL's reserved words that the lexer's keyword list (made for highlighting) lacks.
const PG_RESERVED_EXTRA: &[&str] = &[
    "analyse", "asymmetric", "authorization", "binary", "collation", "current_catalog", "current_role", "current_schema",
    "current_time", "deferrable", "freeze", "initially", "localtime", "localtimestamp", "overlaps", "placing",
    "session_user", "symmetric", "tablesample", "user", "variadic", "verbose",
];

/// `[A-Za-z_][A-Za-z0-9_]*` and not a keyword: what MySQL and T-SQL take unquoted (case kept).
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
        }
    }

    pub fn lex(self) -> LexRules {
        match self {
            Dialect::Postgres => LexRules::POSTGRES,
            Dialect::MySql => LexRules::MYSQL,
            Dialect::TSql => LexRules::TSQL,
        }
    }

    pub fn script(self) -> ScriptRules {
        match self {
            Dialect::Postgres => ScriptRules { begin_atomic: true, go_batches: false, delimiter_command: false, routine_bodies: false },
            Dialect::MySql => ScriptRules { begin_atomic: false, go_batches: false, delimiter_command: true, routine_bodies: true },
            Dialect::TSql => ScriptRules { begin_atomic: false, go_batches: true, delimiter_command: false, routine_bodies: false },
        }
    }

    /// Unquoted identifiers fold to lower case (PostgreSQL); elsewhere they are kept as written
    /// and compared by the server's collation.
    pub fn folds_to_lower(self) -> bool {
        self == Dialect::Postgres
    }

    /// An identifier for generated SQL, quoted when the engine would fold, reject or misread it
    /// unquoted (reserved words included).
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
        }
    }

    /// `schema.table`, each part quoted when needed; no schema → just the table.
    pub fn qualified(self, schema: Option<&str>, name: &str) -> String {
        match schema {
            Some(s) if !s.is_empty() => format!("{}.{}", self.quote_ident(s), self.quote_ident(name)),
            _ => self.quote_ident(name),
        }
    }

    /// `'text'` with quotes doubled. PostgreSQL's `standard_conforming_strings` (the default, and
    /// LAR's only mode) and T-SQL keep backslashes plain; MySQL's default mode escapes them.
    /// T-SQL's are Unicode (`N'…'`): a plain `'…'` goes through the database's code page.
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

    /// The placeholder of the `n`th (1-based) bound parameter.
    pub fn placeholder(self, n: usize) -> String {
        match self {
            Dialect::Postgres => format!("${n}"),
            Dialect::MySql => "?".into(),
            Dialect::TSql => format!("@P{n}"),
        }
    }

    /// The paging clause after `ORDER BY`: `LIMIT 100 OFFSET 200`. T-SQL's form needs an
    /// `ORDER BY` before it.
    pub fn page_clause(self, limit: u64, offset: u64) -> String {
        match self {
            Dialect::Postgres | Dialect::MySql if offset == 0 => format!("LIMIT {limit}"),
            Dialect::Postgres | Dialect::MySql => format!("LIMIT {limit} OFFSET {offset}"),
            Dialect::TSql => format!("OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"),
        }
    }

    pub fn returning(self) -> Returning {
        match self {
            Dialect::Postgres => Returning::Clause,
            Dialect::MySql => Returning::Reselect,
            Dialect::TSql => Returning::Output,
        }
    }

    /// `a` equals `b`, NULL equal to NULL. (SQL Server only has `IS NOT DISTINCT FROM` since
    /// 2022: the long form works on every version.)
    pub fn not_distinct(self, a: &str, b: &str) -> String {
        match self {
            Dialect::Postgres => format!("{a} IS NOT DISTINCT FROM {b}"),
            Dialect::MySql => format!("{a} <=> {b}"),
            Dialect::TSql => format!("({a} = {b} OR ({a} IS NULL AND {b} IS NULL))"),
        }
    }

    /// An expression as text, for pattern matching any type.
    pub fn text_cast(self, expr: &str) -> String {
        match self {
            Dialect::Postgres => format!("{expr}::text"),
            Dialect::MySql => format!("CAST({expr} AS CHAR)"),
            Dialect::TSql => format!("CAST({expr} AS NVARCHAR(MAX))"),
        }
    }

    /// The case-insensitive `LIKE` (MySQL's and SQL Server's default collations already are).
    pub fn ilike(self) -> &'static str {
        match self {
            Dialect::Postgres => "ILIKE",
            Dialect::MySql | Dialect::TSql => "LIKE",
        }
    }

    pub fn begin(self) -> &'static str {
        match self {
            Dialect::Postgres => "BEGIN",
            Dialect::MySql => "START TRANSACTION",
            Dialect::TSql => "BEGIN TRANSACTION",
        }
    }

    /// A transaction that refuses writes; `None` where the engine has none (T-SQL).
    pub fn begin_read_only(self) -> Option<&'static str> {
        match self {
            Dialect::Postgres => Some("BEGIN READ ONLY"),
            Dialect::MySql => Some("START TRANSACTION READ ONLY"),
            Dialect::TSql => None,
        }
    }

    /// Puts a server session's later transactions in read-only mode (`on`) or back; `None`
    /// where the engine has no such session setting (T-SQL). Not valid inside a transaction on
    /// MySQL, and undone by a rollback on PostgreSQL: sent between transactions.
    pub fn session_read_only(self, on: bool) -> Option<&'static str> {
        match (self, on) {
            (Dialect::Postgres, true) => Some("SET default_transaction_read_only = on"),
            (Dialect::Postgres, false) => Some("SET default_transaction_read_only = off"),
            (Dialect::MySql, true) => Some("SET SESSION TRANSACTION READ ONLY"),
            (Dialect::MySql, false) => Some("SET SESSION TRANSACTION READ WRITE"),
            (Dialect::TSql, _) => None,
        }
    }

    pub fn commit(self) -> &'static str {
        match self {
            Dialect::Postgres | Dialect::MySql => "COMMIT",
            Dialect::TSql => "COMMIT TRANSACTION",
        }
    }

    pub fn rollback(self) -> &'static str {
        match self {
            Dialect::Postgres | Dialect::MySql => "ROLLBACK",
            Dialect::TSql => "ROLLBACK TRANSACTION",
        }
    }

    pub fn savepoint(self, name: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::MySql => format!("SAVEPOINT {name}"),
            Dialect::TSql => format!("SAVE TRANSACTION {name}"),
        }
    }

    /// Back to a savepoint and forget it (T-SQL's savepoints cannot be released: back only).
    pub fn rollback_to(self, name: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::MySql => format!("ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"),
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
