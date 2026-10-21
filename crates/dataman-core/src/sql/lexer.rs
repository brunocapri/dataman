use std::ops::Range;

use super::dialect::{Dialect, LexRules};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    LineComment,
    BlockComment,
    String,
    DollarString,
    QuotedIdent,
    Number,
    Keyword,
    Ident,
    Param,
    Operator,
    Punct,
    Unknown,
}

impl TokenKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

pub fn tokenize(src: &str) -> Vec<Token> {
    tokenize_in(src, Dialect::Postgres)
}

pub fn tokenize_in(src: &str, dialect: Dialect) -> Vec<Token> {
    let rules: LexRules = dialect.lex();
    let mut out = Vec::with_capacity(src.len() / 4 + 1);
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = match c {
            _ if space_len(src, i, &rules) > 0 => {
                while i < b.len() && space_len(src, i, &rules) > 0 {
                    i += space_len(src, i, &rules);
                }
                TokenKind::Whitespace
            }
            b'-' if dash_comment(b, i, &rules) => {
                while i < b.len() && b[i] != b'\n' && !(rules.cr_ends_comment && b[i] == b'\r') {
                    i += 1;
                }
                TokenKind::LineComment
            }
            b'#' if rules.hash_comments => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                TokenKind::LineComment
            }
            b'\'' | b'"' if rules.mysql_quotes => {
                i = quoted(b, i + 1, c, true);
                TokenKind::String
            }
            b'`' if rules.mysql_quotes => {
                i = quoted(b, i + 1, b'`', false);
                TokenKind::QuotedIdent
            }
            b'?' if rules.mysql_quotes => {
                i += 1;
                TokenKind::Param
            }
            b'@' if rules.mysql_quotes && b.get(i + 1).is_some_and(|&n| is_ident_start(n) || matches!(n, b'@' | b'`' | b'\'' | b'"')) => {
                i += 1;
                if b[i] == b'@' {
                    i += 1;
                }
                match b.get(i) {
                    Some(&q @ (b'`' | b'\'' | b'"')) => i = quoted(b, i + 1, q, q != b'`'),
                    _ => {
                        while i < b.len() && is_ident_cont(b[i]) && space_len(src, i, &rules) == 0 {
                            i += 1;
                        }
                    }
                }
                TokenKind::Ident
            }
            b'[' if rules.tsql_names => {
                i = quoted(b, i + 1, b']', false);
                TokenKind::QuotedIdent
            }
            b'@' | b'#' if rules.tsql_names && b.get(i + 1).is_some_and(|&n| is_ident_start(n) || n == c) => {
                i += 1;
                if b[i] == c {
                    i += 1;
                }
                while i < b.len() && is_ident_cont(b[i]) && space_len(src, i, &rules) == 0 {
                    i += 1;
                }
                TokenKind::Ident
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                let mut depth = 1;
                while i < b.len() && depth > 0 {
                    if rules.nested_comments && b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                TokenKind::BlockComment
            }
            b'\'' => {
                i = quoted(b, i + 1, b'\'', false);
                TokenKind::String
            }
            b'"' => {
                i = quoted(b, i + 1, b'"', false);
                TokenKind::QuotedIdent
            }
            b'e' | b'E' if rules.escape_strings && b.get(i + 1) == Some(&b'\'') => {
                i = quoted(b, i + 2, b'\'', true);
                TokenKind::String
            }
            b'b' | b'B' | b'x' | b'X' | b'n' | b'N' if b.get(i + 1) == Some(&b'\'') => {
                i = quoted(b, i + 2, b'\'', false);
                TokenKind::String
            }
            b'u' | b'U' if rules.escape_strings && b.get(i + 1) == Some(&b'&') && matches!(b.get(i + 2), Some(b'\'' | b'"')) => {
                let q = b[i + 2];
                i = quoted(b, i + 3, q, false);
                if q == b'\'' { TokenKind::String } else { TokenKind::QuotedIdent }
            }
            b'$' if rules.dollar => {
                if b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    TokenKind::Param
                } else if let Some(tag_end) = dollar_tag(b, i) {
                    let tag = &b[i..tag_end];
                    i = tag_end;
                    loop {
                        if i >= b.len() {
                            break;
                        }
                        if b[i] == b'$' && b[i..].starts_with(tag) {
                            i += tag.len();
                            break;
                        }
                        i += 1;
                    }
                    TokenKind::DollarString
                } else {
                    i += 1;
                    TokenKind::Unknown
                }
            }
            b'0'..=b'9' => {
                i = number(b, i);
                TokenKind::Number
            }
            b'.' if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i = number(b, i);
                TokenKind::Number
            }
            b'(' | b')' | b'[' | b']' | b',' | b';' | b'.' => {
                i += 1;
                TokenKind::Punct
            }
            b':' if rules.double_colon && b.get(i + 1) == Some(&b':') => {
                i += 2;
                TokenKind::Operator
            }
            b':' => {
                i += 1;
                TokenKind::Punct
            }
            c if is_ident_start(c) => {
                i += 1;
                while i < b.len() && is_ident_cont(b[i]) && space_len(src, i, &rules) == 0 {
                    i += 1;
                }
                if is_keyword_in(&src[start..i], dialect) { TokenKind::Keyword } else { TokenKind::Ident }
            }
            c if is_op_char(c) => {
                while i < b.len() && is_op_char(b[i]) {
                    if dash_comment(b, i, &rules)
                        || (b[i] == b'/' && b.get(i + 1) == Some(&b'*'))
                        || (rules.mysql_quotes && i > start && matches!(b[i], b'#' | b'`' | b'?' | b'@'))
                        || (rules.tsql_names && i > start && matches!(b[i], b'#' | b'@'))
                    {
                        break;
                    }
                    i += 1;
                }
                if i == start {
                    i += 1;
                }
                TokenKind::Operator
            }
            _ => {
                i += utf8_len(c);
                TokenKind::Unknown
            }
        };
        out.push(Token { kind, start, end: i.min(b.len()) });
    }
    out
}

/// The length of the space at `i`, as the server reads it: `\v` is one everywhere (PostgreSQL
/// 16+, MySQL, SQL Server); SQL Server's list was probed on 2022.
fn space_len(src: &str, i: usize, rules: &LexRules) -> usize {
    match src.as_bytes()[i] {
        b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => 1,
        0x01..=0x1f if rules.unicode_spaces => 1,
        0x80.. if rules.unicode_spaces && src.is_char_boundary(i) => match src[i..].chars().next() {
            Some(c @ ('\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200b}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')) => c.len_utf8(),
            _ => 0,
        },
        _ => 0,
    }
}

fn dash_comment(b: &[u8], i: usize, rules: &LexRules) -> bool {
    b[i] == b'-'
        && b.get(i + 1) == Some(&b'-')
        && (!rules.hash_comments || b.get(i + 2).is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()))
}

fn utf8_len(c: u8) -> usize {
    match c {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xff => 4,
        _ => 1,
    }
}

fn quoted(b: &[u8], i: usize, q: u8, backslash: bool) -> usize {
    quoted_end(b, i, q, backslash).unwrap_or(b.len())
}

fn quoted_end(b: &[u8], mut i: usize, q: u8, backslash: bool) -> Option<usize> {
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

pub fn unterminated(src: &str, t: &Token) -> Option<&'static str> {
    unterminated_in(src, t, Dialect::Postgres)
}

pub fn unterminated_in(src: &str, t: &Token, dialect: Dialect) -> Option<&'static str> {
    let b = t.text(src).as_bytes();
    if dialect.lex().mysql_quotes {
        return match (t.kind, b.first()) {
            (TokenKind::String, Some(&q @ (b'\'' | b'"'))) => quoted_end(b, 1, q, true).is_none().then_some("string"),
            (TokenKind::String, _) => quoted_end(b, 2, b'\'', false).is_none().then_some("string"),
            (TokenKind::QuotedIdent, _) => quoted_end(b, 1, b'`', false).is_none().then_some("quoted identifier"),
            (TokenKind::BlockComment, _) => (b.len() < 4 || !b.ends_with(b"*/")).then_some("comment"),
            _ => None,
        };
    }
    match t.kind {
        TokenKind::String => {
            let (open, backslash) = match b {
                [b'u' | b'U', b'&', b'\'', ..] => (3, false),
                [b'e' | b'E', b'\'', ..] => (2, true),
                [b'\'', ..] => (1, false),
                [_, b'\'', ..] => (2, false),
                _ => return None,
            };
            quoted_end(b, open, b'\'', backslash).is_none().then_some("string")
        }
        TokenKind::QuotedIdent => {
            let (open, q) = match b.first() {
                Some(b'"') => (1, b'"'),
                Some(b'[') => (1, b']'),
                _ => (3, b'"'),
            };
            quoted_end(b, open, q, false).is_none().then_some("quoted identifier")
        }
        TokenKind::BlockComment => {
            let (mut i, mut depth) = (2, 1);
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            (depth > 0).then_some("comment")
        }
        TokenKind::DollarString => {
            let tag = dollar_tag(b, 0).unwrap_or(b.len());
            (b.len() < 2 * tag || !b.ends_with(&b[..tag])).then_some("dollar-quoted string")
        }
        _ => None,
    }
}

fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j) == Some(&b'$') {
        return Some(j + 1);
    }
    if !b.get(j).is_some_and(|&c| is_ident_start(c)) {
        return None;
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

fn number(b: &[u8], mut i: usize) -> usize {
    if b[i..].starts_with(b"0x") || b[i..].starts_with(b"0X") {
        i += 2;
        while i < b.len() && (b[i].is_ascii_hexdigit() || b[i] == b'_') {
            i += 1;
        }
        return i;
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' && b.get(i + 1) != Some(&b'.') {
        i += 1;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    i
}

pub fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

pub fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_op_char(c: u8) -> bool {
    matches!(c, b'+' | b'-' | b'*' | b'/' | b'<' | b'>' | b'=' | b'~' | b'!' | b'@' | b'#' | b'%' | b'^' | b'&' | b'|' | b'`' | b'?')
}

pub const KEYWORDS: &[&str] = &[
    "ABORT", "ADD", "ALL", "ALTER", "ANALYZE", "AND", "ANY", "ARRAY", "AS", "ASC", "BEGIN",
    "BETWEEN", "BIGINT", "BOOLEAN", "BOTH", "BY", "CALL", "CASCADE", "CASE", "CAST", "CHAR",
    "CHECK", "CHECKPOINT", "COLLATE", "COLUMN", "COMMENT", "COMMIT", "CONCURRENTLY", "CONFLICT",
    "CONSTRAINT", "COPY", "CREATE", "CROSS", "CURRENT", "CURRENT_DATE", "CURRENT_TIMESTAMP",
    "CURRENT_USER", "CURSOR", "DATABASE", "DATE", "DEALLOCATE", "DECLARE", "DEFAULT", "DELETE",
    "DESC", "DISCARD", "DISTINCT", "DO", "DOUBLE", "DROP", "ELSE", "END", "EXCEPT", "EXECUTE",
    "EXISTS", "EXPLAIN", "EXTENSION", "FALSE", "FETCH", "FILTER", "FIRST", "FOLLOWING", "FOR",
    "FOREIGN", "FROM", "FULL", "FUNCTION", "GRANT", "GROUP", "HAVING", "IF", "ILIKE", "IN",
    "INDEX", "INNER", "INSERT", "INTEGER", "INTERSECT", "INTERVAL", "INTO", "IS", "ISNULL",
    "JOIN", "JSON", "JSONB", "KEY", "LAST", "LATERAL", "LEADING", "LEFT", "LIKE", "LIMIT",
    "LISTEN", "LOCAL", "LOCK", "MATERIALIZED", "NATURAL", "NOT", "NOTHING", "NOTIFY", "NOTNULL",
    "NULL", "NULLS", "NUMERIC", "OF", "OFF", "OFFSET", "ON", "ONLY", "OR", "ORDER", "OUTER",
    "OVER", "PARTITION", "PRECEDING", "PREPARE", "PRIMARY", "PROCEDURE", "RANGE", "REAL",
    "RECURSIVE", "REFERENCES", "REFRESH", "REINDEX", "RELEASE", "RENAME", "REPLACE", "RESET",
    "RESTRICT", "RETURNING", "RETURNS", "REVOKE", "RIGHT", "ROLE", "ROLLBACK", "ROW", "ROWS",
    "SAVEPOINT", "SCHEMA", "SELECT", "SEQUENCE", "SESSION", "SET", "SHOW", "SIMILAR", "SMALLINT",
    "SOME", "START", "SUBSCRIBE", "SYSTEM_TIME", "TABLE", "TEMP", "TEMPORARY", "TEXT", "THEN",
    "TIME", "TIMESTAMP", "TIMESTAMPTZ", "TO", "TRAILING", "TRANSACTION", "TRIGGER", "TRUE",
    "TRUNCATE", "TYPE", "UNBOUNDED", "UNION", "UNIQUE", "UNLISTEN", "UPDATE", "USING", "UUID",
    "VACUUM", "VALUES", "VARCHAR", "VIEW", "WHEN", "WHERE", "WINDOW", "WITH", "WITHOUT",
];

pub const MYSQL_KEYWORDS: &[&str] = &[
    "AUTO_INCREMENT", "BINARY", "BLOB", "CHANGE", "CHARSET", "DATETIME", "DELAYED", "DESCRIBE", "DISTINCTROW", "DIV", "DUPLICATE", "ENGINE", "ENUM",
    "HIGH_PRIORITY", "IGNORE", "LONGBLOB", "LONGTEXT", "LOW_PRIORITY", "MEDIUMINT", "MEDIUMTEXT", "MOD", "MODIFY", "QUICK", "REGEXP",
    "RLIKE", "SQL_BIG_RESULT", "SQL_BUFFER_RESULT", "SQL_CALC_FOUND_ROWS", "SQL_NO_CACHE", "SQL_SMALL_RESULT", "STRAIGHT_JOIN", "TINYINT", "TINYTEXT", "UNSIGNED", "USE", "XOR", "ZEROFILL",
];

pub const TSQL_KEYWORDS: &[&str] = &[
    "BIT", "BREAK", "CATCH", "CLUSTERED", "CONTINUE", "DATETIME", "DATETIME2", "DATETIMEOFFSET", "DBCC", "DENY", "EXEC",
    "GO", "GOTO", "IDENTITY", "IIF", "IMAGE", "INCLUDE", "MERGE", "MONEY", "NCHAR", "NEXT", "NOCOUNT", "NOLOCK", "NONCLUSTERED",
    "NTEXT", "NVARCHAR", "OUTPUT", "PERCENT", "PIVOT", "PRINT", "PROC", "RAISERROR", "RETURN", "ROWVERSION",
    "SMALLDATETIME", "SMALLMONEY", "SYNONYM", "THROW", "TINYINT", "TOP", "TRAN", "TRY", "UNIQUEIDENTIFIER", "UNPIVOT",
    "USE", "VARBINARY", "WAITFOR", "WHILE", "XML",
];

pub fn is_keyword(word: &str) -> bool {
    in_list(KEYWORDS, word)
}

pub fn is_keyword_in(word: &str, dialect: Dialect) -> bool {
    is_keyword(word)
        || match dialect {
            Dialect::MySql => in_list(MYSQL_KEYWORDS, word),
            Dialect::TSql => in_list(TSQL_KEYWORDS, word),
            Dialect::Postgres => false,
        }
}

fn in_list(list: &[&str], word: &str) -> bool {
    if word.len() > 17 || word.len() < 2 {
        return false;
    }
    let mut buf = [0u8; 17];
    for (d, s) in buf.iter_mut().zip(word.bytes()) {
        *d = s.to_ascii_uppercase();
    }
    let up = &buf[..word.len()];
    list.binary_search_by(|k| k.as_bytes().cmp(up)).is_ok()
}

pub fn eq_kw(word: &str, kw: &str) -> bool {
    word.eq_ignore_ascii_case(kw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(TokenKind, &str)> {
        tokenize(src).into_iter().filter(|t| !t.kind.is_trivia()).map(|t| (t.kind, t.text(src))).collect()
    }

    #[test]
    fn keywords_sorted() {
        let mut v = KEYWORDS.to_vec();
        v.sort();
        assert_eq!(v, KEYWORDS);
    }

    #[test]
    fn basic() {
        use TokenKind::*;
        assert_eq!(
            kinds("SELECT a.b, 'x''y', \"Q\"\"\" FROM t WHERE n >= 1.5e3 -- c\n;"),
            vec![
                (Keyword, "SELECT"),
                (Ident, "a"),
                (Punct, "."),
                (Ident, "b"),
                (Punct, ","),
                (String, "'x''y'"),
                (Punct, ","),
                (QuotedIdent, "\"Q\"\"\""),
                (Keyword, "FROM"),
                (Ident, "t"),
                (Keyword, "WHERE"),
                (Ident, "n"),
                (Operator, ">="),
                (Number, "1.5e3"),
                (Punct, ";"),
            ]
        );
    }

    #[test]
    fn dollar_and_nested_comments() {
        use TokenKind::*;
        let src = "$f$ a; 'b' $f$ /* x /* y */ ; */ $$;$$ $1 E'\\'' x::int";
        assert_eq!(
            kinds(src),
            vec![
                (DollarString, "$f$ a; 'b' $f$"),
                (DollarString, "$$;$$"),
                (Param, "$1"),
                (String, "E'\\''"),
                (Ident, "x"),
                (Operator, "::"),
                (Ident, "int"),
            ]
        );
        let toks = tokenize(src);
        assert!(toks.iter().any(|t| t.kind == BlockComment && t.text(src) == "/* x /* y */ ; */"));
    }

    #[test]
    fn unterminated_runs_to_end() {
        let src = "select 'abc";
        let t = tokenize(src);
        assert_eq!(t.last().unwrap().kind, TokenKind::String);
        assert_eq!(t.last().unwrap().end, src.len());
        let t = tokenize("/* open");
        assert_eq!(t[0].kind, TokenKind::BlockComment);
        let t = tokenize("été ü");
        assert_eq!(t[0].kind, TokenKind::Ident);
        assert_eq!(t[0].end, "été".len());
    }

    #[test]
    fn open_constructs() {
        let open = |src: &str| {
            let toks = tokenize(src);
            toks.iter().find_map(|t| unterminated(src, t))
        };
        assert_eq!(open("select 'a''b', E'\\'', $$x$$, $f$ y $f$, \"q\" /* a /* b */ */"), None);
        assert_eq!(open("select 'abc"), Some("string"));
        assert_eq!(open("select E'ab\\'"), Some("string"));
        assert_eq!(open("select \"abc"), Some("quoted identifier"));
        assert_eq!(open("select 1 /* a /* b */"), Some("comment"));
        assert_eq!(open("select $f$ body"), Some("dollar-quoted string"));
        assert_eq!(open("select $$"), Some("dollar-quoted string"));
        assert_eq!(open("select ''"), None);
    }

    #[test]
    fn dialect_rules() {
        use TokenKind::*;
        let src = "$$ x $$ E'a' /* a /* b */ c */ d::int";
        let pg: Vec<(TokenKind, &str)> = tokenize(src).into_iter().filter(|t| t.kind != Whitespace).map(|t| (t.kind, t.text(src))).collect();
        assert_eq!(
            pg,
            vec![(DollarString, "$$ x $$"), (String, "E'a'"), (BlockComment, "/* a /* b */ c */"), (Ident, "d"), (Operator, "::"), (Ident, "int")]
        );
        let my: Vec<(TokenKind, &str)> = tokenize_in(src, Dialect::MySql).into_iter().filter(|t| t.kind != Whitespace).map(|t| (t.kind, t.text(src))).collect();
        assert_eq!(my[0], (Unknown, "$"));
        assert!(my.contains(&(Ident, "E")) && my.contains(&(String, "'a'")));
        assert!(my.contains(&(BlockComment, "/* a /* b */")) && my.contains(&(Ident, "c")), "{my:?}");
        assert!(my.contains(&(Punct, ":")));
    }

    #[test]
    fn mysql_forms() {
        use TokenKind::*;
        let my = |src: &'static str| -> Vec<(TokenKind, &'static str)> {
            tokenize_in(src, Dialect::MySql).into_iter().filter(|t| t.kind != Whitespace).map(|t| (t.kind, t.text(src))).collect()
        };
        assert_eq!(my("a # c ; d\nb"), vec![(Ident, "a"), (LineComment, "# c ; d"), (Ident, "b")]);
        assert_eq!(my("5--3 -- x"), vec![(Number, "5"), (Operator, "--"), (Number, "3"), (LineComment, "-- x")]);
        assert_eq!(my("1 --\n2"), vec![(Number, "1"), (LineComment, "--"), (Number, "2")]);
        assert_eq!(my("`a``b` \"it\\\"s\" 'x\\'y' 'q''r'"), vec![(QuotedIdent, "`a``b`"), (String, "\"it\\\"s\""), (String, "'x\\'y'"), (String, "'q''r'")]);
        assert_eq!(my("'a\\\\' ; 'b'"), vec![(String, "'a\\\\'"), (Punct, ";"), (String, "'b'")], "an escaped backslash ends the string");
        assert_eq!(my("/*!50100 PARTITION */ x /* a /* b */ c"), vec![(BlockComment, "/*!50100 PARTITION */"), (Ident, "x"), (BlockComment, "/* a /* b */"), (Ident, "c")]);
        assert_eq!(
            my("x = ? AND @v := @@session.sql_mode"),
            vec![
                (Ident, "x"),
                (Operator, "="),
                (Param, "?"),
                (Keyword, "AND"),
                (Ident, "@v"),
                (Punct, ":"),
                (Operator, "="),
                (Ident, "@@session"),
                (Punct, "."),
                (Ident, "sql_mode")
            ]
        );
        assert!(my("SELECT x FROM t WHERE y >= ? ORDER BY 1").contains(&(Param, "?")));
        assert_eq!(my("ENGINE")[0].0, Keyword);
        assert_eq!(tokenize("ENGINE")[0].kind, Ident);
        let open = |src: &str| tokenize_in(src, Dialect::MySql).iter().find_map(|t| unterminated_in(src, t, Dialect::MySql));
        assert_eq!(open("select 'a\\'"), Some("string"));
        assert_eq!(open("select \"a"), Some("string"));
        assert_eq!(open("select `a"), Some("quoted identifier"));
        assert_eq!(open("select 1 /* x"), Some("comment"));
        assert_eq!(open("select 'a\\\\', \"b\", `c`, /* d */ 1"), None);
        let mut v = MYSQL_KEYWORDS.to_vec();
        v.sort();
        assert_eq!(v, MYSQL_KEYWORDS);
    }

    #[test]
    fn tsql_forms() {
        use TokenKind::*;
        let ms = |src: &'static str| -> Vec<(TokenKind, &'static str)> {
            tokenize_in(src, Dialect::TSql).into_iter().filter(|t| t.kind != Whitespace).map(|t| (t.kind, t.text(src))).collect()
        };
        assert_eq!(
            ms("[a]]b c].[x] \"q\" N'it''s é' 'x'"),
            vec![(QuotedIdent, "[a]]b c]"), (Punct, "."), (QuotedIdent, "[x]"), (QuotedIdent, "\"q\""), (String, "N'it''s é'"), (String, "'x'")]
        );
        assert_eq!(
            ms("SELECT @v=@@ROWCOUNT FROM #t JOIN ##g ON 1=1"),
            vec![
                (Keyword, "SELECT"),
                (Ident, "@v"),
                (Operator, "="),
                (Ident, "@@ROWCOUNT"),
                (Keyword, "FROM"),
                (Ident, "#t"),
                (Keyword, "JOIN"),
                (Ident, "##g"),
                (Keyword, "ON"),
                (Number, "1"),
                (Operator, "="),
                (Number, "1")
            ]
        );
        assert_eq!(ms("/* a /* b */ c */ x --y"), vec![(BlockComment, "/* a /* b */ c */"), (Ident, "x"), (LineComment, "--y")]);
        assert_eq!(ms("0x0A1B")[0], (Number, "0x0A1B"));
        assert_eq!(ms("TOP")[0].0, Keyword);
        assert_eq!(tokenize("TOP")[0].kind, Ident);
        let open = |src: &str| tokenize_in(src, Dialect::TSql).iter().find_map(|t| unterminated_in(src, t, Dialect::TSql));
        assert_eq!(open("select [a"), Some("quoted identifier"));
        assert_eq!(open("select N'a"), Some("string"));
        assert_eq!(open("select [a]]], N'b''c', /* x /* y */ */ 1"), None);
        let mut v = TSQL_KEYWORDS.to_vec();
        v.sort();
        assert_eq!(v, TSQL_KEYWORDS);
    }

    #[test]
    fn covers_input() {
        let src = "select 1 + -2 /* c */ <> @@ ~* 'x' -- end";
        let toks = tokenize(src);
        let mut at = 0;
        for t in toks {
            assert_eq!(t.start, at);
            at = t.end;
        }
        assert_eq!(at, src.len());
    }
}
