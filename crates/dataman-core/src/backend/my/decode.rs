//! MySQL values into the grid's chunks: each column's [`Kind`] from its type, flags and
//! character set, and the cells of both protocols (text: every value as bytes; binary: typed).

use std::fmt::Write as _;

use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column, Value};

use crate::result::{ChunkBuilder, ColumnInfo};
use crate::value::{self, Kind};

/// The `binary` character set: BINARY, VARBINARY and BLOB columns (and BIT, GEOMETRY…).
const BINARY_CHARSET: u16 = 63;

/// How one column's cells are decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dec {
    pub kind: Kind,
    /// BIT: big-endian bytes, as an integer.
    pub bits: bool,
}

/// What the grid shows for a column, and the type's name as MySQL writes it.
pub(crate) fn kind_of(ty: ColumnType, flags: ColumnFlags, charset: u16, length: u32) -> (Kind, String) {
    use ColumnType::*;
    let unsigned = flags.contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = charset == BINARY_CHARSET;
    let u = |s: &str| if unsigned { format!("{s} unsigned") } else { s.to_string() };
    match ty {
        MYSQL_TYPE_TINY => (Kind::Int, u("tinyint")),
        MYSQL_TYPE_SHORT => (Kind::Int, u("smallint")),
        MYSQL_TYPE_INT24 => (Kind::Int, u("mediumint")),
        MYSQL_TYPE_LONG => (Kind::Int, u("int")),
        // Unsigned BIGINT goes past i64: kept exact as a decimal.
        MYSQL_TYPE_LONGLONG if unsigned => (Kind::Numeric, "bigint unsigned".into()),
        MYSQL_TYPE_LONGLONG => (Kind::Int, "bigint".into()),
        MYSQL_TYPE_YEAR => (Kind::Int, "year".into()),
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => (Kind::Numeric, u("decimal")),
        MYSQL_TYPE_FLOAT => (Kind::Float4, u("float")),
        MYSQL_TYPE_DOUBLE => (Kind::Float, u("double")),
        MYSQL_TYPE_BIT if length < 64 => (Kind::Int, "bit".into()),
        MYSQL_TYPE_BIT => (Kind::Numeric, "bit".into()),
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => (Kind::Date, "date".into()),
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => (Kind::Timestamp, "datetime".into()),
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => (Kind::Timestamp, "timestamp".into()),
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => (Kind::Time, "time".into()),
        MYSQL_TYPE_JSON => (Kind::Json, "json".into()),
        MYSQL_TYPE_ENUM => (Kind::Text, "enum".into()),
        MYSQL_TYPE_SET => (Kind::Text, "set".into()),
        MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
            if flags.contains(ColumnFlags::ENUM_FLAG) {
                (Kind::Text, "enum".into())
            } else if flags.contains(ColumnFlags::SET_FLAG) {
                (Kind::Text, "set".into())
            } else if binary {
                (Kind::Bytes, if ty == MYSQL_TYPE_STRING { "binary" } else { "varbinary" }.into())
            } else {
                (Kind::Text, if ty == MYSQL_TYPE_STRING { "char" } else { "varchar" }.into())
            }
        }
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                (Kind::Bytes, "blob".into())
            } else {
                (Kind::Text, "text".into())
            }
        }
        MYSQL_TYPE_GEOMETRY => (Kind::Bytes, "geometry".into()),
        MYSQL_TYPE_VECTOR => (Kind::Bytes, "vector".into()),
        MYSQL_TYPE_NULL => (Kind::Text, "null".into()),
        _ => (if binary { Kind::Bytes } else { Kind::Other }, format!("{ty:?}").trim_start_matches("MYSQL_TYPE_").to_ascii_lowercase()),
    }
}

pub(crate) fn column_info(c: &Column) -> (ColumnInfo, Dec) {
    let (kind, type_name) = kind_of(c.column_type(), c.flags(), c.character_set(), c.column_length());
    let dec = Dec { kind, bits: c.column_type() == ColumnType::MYSQL_TYPE_BIT };
    (ColumnInfo { name: c.name_str().into_owned(), type_name, type_oid: 0, kind }, dec)
}

pub(crate) fn column_infos(cols: &[Column]) -> (Vec<ColumnInfo>, Vec<Dec>) {
    cols.iter().map(column_info).unzip()
}

/// The table column behind a result column (`None` for an expression).
pub(crate) fn origin(c: &Column) -> Option<crate::result::NamedOrigin> {
    let (schema, table, column) = (c.schema_str(), c.org_table_str(), c.org_name_str());
    (!schema.is_empty() && !table.is_empty() && !column.is_empty()).then(|| crate::result::NamedOrigin {
        schema: schema.into_owned(),
        table: table.into_owned(),
        column: column.into_owned(),
    })
}

/// A decimal integer (`-12`, `+7`); `None` for anything else.
fn parse_i64(b: &[u8]) -> Option<i64> {
    let (neg, digits) = match b.first()? {
        b'-' => (true, &b[1..]),
        b'+' => (false, &b[1..]),
        _ => (false, b),
    };
    if digits.is_empty() || digits.len() > 19 {
        return None;
    }
    let mut v: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_sub((c - b'0') as i64)?;
    }
    if neg { Some(v) } else { v.checked_neg() }
}

fn num<T: std::str::FromStr>(b: &[u8]) -> Option<T> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

/// `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` → (y, m, d, h, mi, s, µs).
fn parse_datetime(b: &[u8]) -> Option<(u16, u8, u8, u8, u8, u8, u32)> {
    let s = std::str::from_utf8(b).ok()?;
    let (date, time) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut d = date.splitn(3, '-');
    let (y, m, dd) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (h, mi, sec, us) = match time {
        Some(t) => {
            let (hms, us) = parse_hms(t)?;
            if hms.0 < 0 || hms.0 > 23 {
                return None;
            }
            (hms.0 as u8, hms.1, hms.2, us)
        }
        None => (0, 0, 0, 0),
    };
    Some((y, m, dd, h, mi, sec, us))
}

/// `[-]H+:MM:SS[.ffffff]` → ((h, m, s), µs), the sign on the hours.
fn parse_hms(t: &str) -> Option<((i64, u8, u8), u32)> {
    let (main, frac) = match t.split_once('.') {
        Some((a, f)) => (a, Some(f)),
        None => (t, None),
    };
    let mut p = main.splitn(3, ':');
    let h: i64 = p.next()?.parse().ok()?;
    let m: u8 = p.next()?.parse().ok()?;
    let s: u8 = p.next()?.parse().ok()?;
    let us = match frac {
        Some(f) if !f.is_empty() && f.len() <= 6 && f.bytes().all(|c| c.is_ascii_digit()) => {
            f.parse::<u32>().ok()? * 10u32.pow(6 - f.len() as u32)
        }
        Some(_) => return None,
        None => 0,
    };
    Some(((h, m, s), us))
}

/// TIME text (`-838:59:59.5`) → signed microseconds.
fn parse_time(b: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(b).ok()?;
    let (neg, t) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let ((h, m, sec), us) = parse_hms(t)?;
    let v = ((h * 60 + m as i64) * 60 + sec as i64) * 1_000_000 + us as i64;
    Some(if neg { -v } else { v })
}

fn hex(b: &[u8], out: &mut String) {
    out.push_str("\\x");
    for x in b {
        let _ = write!(out, "{x:02x}");
    }
}

fn bits(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |v, x| (v << 8) | *x as u64)
}

/// Pushes one cell of either protocol into column `i`.
pub(crate) fn push_value(b: &mut ChunkBuilder, i: usize, d: Dec, v: &Value) {
    match v {
        Value::NULL => b.push_null(i),
        Value::Bytes(raw) => push_bytes(b, i, d, raw),
        Value::Int(x) => match d.kind {
            Kind::Int | Kind::Date | Kind::Time | Kind::Timestamp => b.push_i64(i, *x),
            Kind::Float | Kind::Float4 => b.push_f64(i, *x as f64),
            _ => b.push_text_with(i, |s| {
                let _ = write!(s, "{x}");
            }),
        },
        Value::UInt(x) => match d.kind {
            Kind::Int if *x <= i64::MAX as u64 => b.push_i64(i, *x as i64),
            Kind::Float | Kind::Float4 => b.push_f64(i, *x as f64),
            _ => b.push_text_with(i, |s| {
                let _ = write!(s, "{x}");
            }),
        },
        Value::Float(x) => match d.kind {
            Kind::Float | Kind::Float4 => b.push_f64(i, *x as f64),
            _ => b.push_text_with(i, |s| value::format_float(*x as f64, true, s)),
        },
        Value::Double(x) => match d.kind {
            Kind::Float | Kind::Float4 => b.push_f64(i, *x),
            _ => b.push_text_with(i, |s| value::format_float(*x, false, s)),
        },
        Value::Date(y, m, dd, h, mi, s, us) => match d.kind {
            Kind::Date => b.push_i64(i, value::date_cell(*y, *m, *dd)),
            Kind::Timestamp => b.push_i64(i, value::timestamp_cell(*y, *m, *dd, *h, *mi, *s, *us)),
            _ => b.push_text_with(i, |out| {
                let _ = write!(out, "{y:04}-{m:02}-{dd:02} {h:02}:{mi:02}:{s:02}");
            }),
        },
        Value::Time(neg, days, h, m, s, us) => {
            let v = (((*days as i64 * 24 + *h as i64) * 60 + *m as i64) * 60 + *s as i64) * 1_000_000 + *us as i64;
            let v = if *neg { -v } else { v };
            match d.kind {
                Kind::Time => b.push_i64(i, v),
                _ => b.push_text_with(i, |out| value::format_time(v, out)),
            }
        }
    }
}

/// A value the server sent as bytes: every value of the text protocol, strings and decimals of
/// the binary one. What does not parse as the column's kind is kept: NULL for native storage
/// (never in practice: the server formats these), text otherwise.
fn push_bytes(b: &mut ChunkBuilder, i: usize, d: Dec, raw: &[u8]) {
    if d.bits {
        let v = bits(raw);
        return match d.kind {
            Kind::Int => b.push_i64(i, v as i64),
            _ => b.push_text_with(i, |s| {
                let _ = write!(s, "{v}");
            }),
        };
    }
    match d.kind {
        Kind::Int => match parse_i64(raw) {
            Some(v) => b.push_i64(i, v),
            None => b.push_null(i),
        },
        Kind::Float | Kind::Float4 => match num::<f64>(raw) {
            Some(v) => b.push_f64(i, v),
            None => b.push_null(i),
        },
        Kind::Date => match parse_datetime(raw) {
            Some((y, m, dd, ..)) => b.push_i64(i, value::date_cell(y, m, dd)),
            None => b.push_null(i),
        },
        Kind::Timestamp => match parse_datetime(raw) {
            Some((y, m, dd, h, mi, s, us)) => b.push_i64(i, value::timestamp_cell(y, m, dd, h, mi, s, us)),
            None => b.push_null(i),
        },
        Kind::Time => match parse_time(raw) {
            Some(v) => b.push_i64(i, v),
            None => b.push_null(i),
        },
        Kind::Bytes => b.push_text_with(i, |s| hex(raw, s)),
        _ => b.push_text_with(i, |s| s.push_str(&String::from_utf8_lossy(raw))),
    }
}

/// A value as plain text (catalog reads); `None` for NULL.
pub(crate) fn value_text(v: &Value) -> Option<String> {
    Some(match v {
        Value::NULL => return None,
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Int(x) => x.to_string(),
        Value::UInt(x) => x.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Double(x) => x.to_string(),
        Value::Date(y, m, d, h, mi, s, us) => {
            let mut out = format!("{y:04}-{m:02}-{d:02}");
            if (*h, *mi, *s, *us) != (0, 0, 0, 0) {
                let _ = write!(out, " {h:02}:{mi:02}:{s:02}");
                if *us > 0 {
                    let _ = write!(out, ".{us:06}");
                }
            }
            out
        }
        Value::Time(neg, days, h, m, s, us) => {
            let v = (((*days as i64 * 24 + *h as i64) * 60 + *m as i64) * 60 + *s as i64) * 1_000_000 + *us as i64;
            let mut out = String::new();
            value::format_time(if *neg { -v } else { v }, &mut out);
            out
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::{Cell, format_cell};

    fn col(kind: Kind) -> ColumnInfo {
        ColumnInfo { name: "c".into(), type_name: String::new(), type_oid: 0, kind }
    }

    /// One cell through the builder, as the grid prints it.
    fn shown(kind: Kind, bits: bool, v: Value) -> Option<String> {
        let mut b = ChunkBuilder::new(&[col(kind)]);
        push_value(&mut b, 0, Dec { kind, bits }, &v);
        b.end_row();
        let chunk = b.finish();
        format_cell(kind, chunk.cell(0, 0)).map(|s| s.into_owned())
    }

    fn text(kind: Kind, s: &str) -> Option<String> {
        shown(kind, false, Value::Bytes(s.as_bytes().to_vec()))
    }

    #[test]
    fn kinds_from_metadata() {
        use ColumnType::*;
        let none = ColumnFlags::empty();
        let k = |t, f, cs| kind_of(t, f, cs, 0).0;
        assert_eq!(k(MYSQL_TYPE_LONG, none, 63), Kind::Int);
        assert_eq!(k(MYSQL_TYPE_LONGLONG, ColumnFlags::UNSIGNED_FLAG, 63), Kind::Numeric);
        assert_eq!(kind_of(MYSQL_TYPE_LONGLONG, ColumnFlags::UNSIGNED_FLAG, 63, 20).1, "bigint unsigned");
        assert_eq!(k(MYSQL_TYPE_LONGLONG, none, 63), Kind::Int);
        assert_eq!(k(MYSQL_TYPE_NEWDECIMAL, none, 63), Kind::Numeric);
        assert_eq!(k(MYSQL_TYPE_FLOAT, none, 63), Kind::Float4);
        assert_eq!(k(MYSQL_TYPE_DOUBLE, none, 63), Kind::Float);
        assert_eq!(k(MYSQL_TYPE_DATETIME, none, 63), Kind::Timestamp);
        assert_eq!(k(MYSQL_TYPE_TIMESTAMP, none, 63), Kind::Timestamp);
        assert_eq!(k(MYSQL_TYPE_DATE, none, 63), Kind::Date);
        assert_eq!(k(MYSQL_TYPE_TIME, none, 63), Kind::Time);
        assert_eq!(k(MYSQL_TYPE_YEAR, none, 63), Kind::Int);
        assert_eq!(k(MYSQL_TYPE_JSON, none, 63), Kind::Json);
        assert_eq!(k(MYSQL_TYPE_BLOB, none, 63), Kind::Bytes);
        assert_eq!(k(MYSQL_TYPE_BLOB, none, 255), Kind::Text, "TEXT is a BLOB with a character set");
        assert_eq!(k(MYSQL_TYPE_VAR_STRING, none, 63), Kind::Bytes);
        assert_eq!(k(MYSQL_TYPE_VAR_STRING, none, 255), Kind::Text);
        assert_eq!(kind_of(MYSQL_TYPE_STRING, ColumnFlags::ENUM_FLAG, 255, 0), (Kind::Text, "enum".into()));
        assert_eq!(kind_of(MYSQL_TYPE_STRING, ColumnFlags::SET_FLAG, 255, 0), (Kind::Text, "set".into()));
        assert_eq!(kind_of(MYSQL_TYPE_BIT, none, 63, 8).0, Kind::Int);
        assert_eq!(kind_of(MYSQL_TYPE_BIT, none, 63, 64).0, Kind::Numeric);
    }

    #[test]
    fn text_protocol_cells() {
        assert_eq!(text(Kind::Int, "-42").as_deref(), Some("-42"));
        assert_eq!(text(Kind::Int, "9223372036854775807").as_deref(), Some("9223372036854775807"));
        assert_eq!(text(Kind::Int, "-9223372036854775808").as_deref(), Some("-9223372036854775808"));
        assert_eq!(text(Kind::Numeric, "18446744073709551615").as_deref(), Some("18446744073709551615"));
        assert_eq!(text(Kind::Numeric, "12.50").as_deref(), Some("12.50"));
        assert_eq!(text(Kind::Float4, "0.1").as_deref(), Some("0.1"));
        assert_eq!(text(Kind::Float, "1.5e-7").as_deref(), Some("1.5e-07"));
        assert_eq!(text(Kind::Date, "2024-02-29").as_deref(), Some("2024-02-29"));
        assert_eq!(text(Kind::Date, "1999-12-31").as_deref(), Some("1999-12-31"));
        assert_eq!(text(Kind::Timestamp, "2024-01-02 03:04:05.5").as_deref(), Some("2024-01-02 03:04:05.5"));
        assert_eq!(text(Kind::Timestamp, "2024-01-02 03:04:05.000120").as_deref(), Some("2024-01-02 03:04:05.00012"));
        assert_eq!(text(Kind::Time, "-838:59:59").as_deref(), Some("-838:59:59"));
        assert_eq!(text(Kind::Time, "12:00:00.250000").as_deref(), Some("12:00:00.25"));
        assert_eq!(text(Kind::Json, "{\"a\": 1}").as_deref(), Some("{\"a\": 1}"));
        assert_eq!(shown(Kind::Bytes, false, Value::Bytes(vec![0xde, 0xad])).as_deref(), Some("\\xdead"));
        assert_eq!(shown(Kind::Int, true, Value::Bytes(vec![0x01, 0x01])).as_deref(), Some("257"), "BIT is big-endian");
        assert_eq!(text(Kind::Text, "paid").as_deref(), Some("paid"));
        assert_eq!(shown(Kind::Int, false, Value::NULL), None);
    }

    /// Zero and partial dates stay as written, and sort before every real date.
    #[test]
    fn zero_dates() {
        assert_eq!(text(Kind::Date, "0000-00-00").as_deref(), Some("0000-00-00"));
        assert_eq!(text(Kind::Timestamp, "0000-00-00 00:00:00").as_deref(), Some("0000-00-00 00:00:00"));
        assert_eq!(text(Kind::Date, "2024-02-00").as_deref(), Some("2024-02-00"));
        assert_eq!(text(Kind::Date, "2023-02-29").as_deref(), Some("2023-02-29"), "off the calendar");
        let zero = value::date_cell(0, 0, 0);
        let part = value::date_cell(2024, 2, 0);
        let real = value::date_cell(1, 1, 1);
        assert!(value::is_odd_date(zero) && value::is_odd_date(part) && !value::is_odd_date(real));
        assert!(zero < part && part < real);
        assert!(!value::is_odd_date(i64::MIN) && !value::is_odd_date(0));
    }

    #[test]
    fn binary_protocol_cells() {
        assert_eq!(shown(Kind::Int, false, Value::Int(-7)).as_deref(), Some("-7"));
        assert_eq!(shown(Kind::Numeric, false, Value::UInt(u64::MAX)).as_deref(), Some("18446744073709551615"));
        assert_eq!(shown(Kind::Int, false, Value::UInt(5)).as_deref(), Some("5"));
        assert_eq!(shown(Kind::Float4, false, Value::Float(0.1)).as_deref(), Some("0.1"));
        assert_eq!(shown(Kind::Float, false, Value::Double(2.5)).as_deref(), Some("2.5"));
        assert_eq!(shown(Kind::Date, false, Value::Date(2026, 10, 13, 0, 0, 0, 0)).as_deref(), Some("2026-10-13"));
        assert_eq!(shown(Kind::Timestamp, false, Value::Date(2026, 10, 13, 9, 30, 0, 500_000)).as_deref(), Some("2026-10-13 09:30:00.5"));
        assert_eq!(shown(Kind::Timestamp, false, Value::Date(0, 0, 0, 0, 0, 0, 0)).as_deref(), Some("0000-00-00 00:00:00"));
        assert_eq!(shown(Kind::Time, false, Value::Time(true, 1, 2, 3, 4, 0)).as_deref(), Some("-26:03:04"));
        let mut b = ChunkBuilder::new(&[col(Kind::Int)]);
        push_value(&mut b, 0, Dec { kind: Kind::Int, bits: false }, &Value::Int(3));
        b.end_row();
        assert_eq!(b.finish().cell(0, 0), Cell::Int(3), "integers stay native");
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_i64(b"+12"), Some(12));
        assert_eq!(parse_i64(b"12a"), None);
        assert_eq!(parse_i64(b"99999999999999999999"), None);
        assert_eq!(parse_time(b"00:00:01.000001"), Some(1_000_001));
        assert_eq!(parse_datetime(b"2024-01-02T03:04:05"), Some((2024, 1, 2, 3, 4, 5, 0)));
        assert_eq!(value_text(&Value::Date(2024, 1, 2, 0, 0, 0, 0)).as_deref(), Some("2024-01-02"));
        assert_eq!(value_text(&Value::NULL), None);
    }
}
