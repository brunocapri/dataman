//! Decoding PostgreSQL binary values and formatting them for display.
//!
//! Every column gets a [`Kind`] (how to display and sort it) and a storage class: integers,
//! floats and booleans stay native (timestamps and dates as integers, formatted only when
//! a cell is drawn); everything else is formatted once, at decode time, into text.

use std::fmt::Write as _;

use chrono::{DateTime, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use postgres_protocol::types as pt;
use tokio_postgres::types::{FromSql, Kind as PgKind, Type};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Bool,
    Int,
    Float,
    /// `real`: stored as f64, printed with single precision (`0.1`, not `0.10000000149011612`).
    Float4,
    /// Exact decimal kept as text; sorts numerically.
    Numeric,
    Text,
    Bytes,
    /// Days since 2000-01-01.
    Date,
    /// Microseconds since midnight.
    Time,
    /// Microseconds since 2000-01-01 00:00:00.
    Timestamp,
    /// Microseconds since 2000-01-01 00:00:00 UTC; shown in local time.
    TimestampTz,
    Json,
    /// Formatted text of anything else (intervals, uuids, arrays, network types…).
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    Bool,
    I64,
    F64,
    Text,
}

impl Kind {
    pub fn storage(self) -> Storage {
        match self {
            Kind::Bool => Storage::Bool,
            Kind::Int | Kind::Date | Kind::Time | Kind::Timestamp | Kind::TimestampTz => Storage::I64,
            Kind::Float | Kind::Float4 => Storage::F64,
            _ => Storage::Text,
        }
    }

    /// Numbers align right in the grid.
    pub fn is_numeric(self) -> bool {
        matches!(self, Kind::Int | Kind::Float | Kind::Float4 | Kind::Numeric)
    }

    pub fn of(ty: &Type) -> Kind {
        match *ty {
            Type::BOOL => Kind::Bool,
            Type::INT2 | Type::INT4 | Type::INT8 | Type::OID | Type::XID | Type::CID => Kind::Int,
            Type::FLOAT4 => Kind::Float4,
            Type::FLOAT8 => Kind::Float,
            Type::NUMERIC => Kind::Numeric,
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::CHAR | Type::UNKNOWN => Kind::Text,
            Type::BYTEA => Kind::Bytes,
            Type::DATE => Kind::Date,
            Type::TIME => Kind::Time,
            Type::TIMESTAMP => Kind::Timestamp,
            Type::TIMESTAMPTZ => Kind::TimestampTz,
            Type::JSON | Type::JSONB => Kind::Json,
            _ => match ty.kind() {
                PgKind::Enum(_) => Kind::Text,
                PgKind::Domain(base) => Kind::of(base),
                _ => Kind::Other,
            },
        }
    }
}

/// A cell as received: the raw binary value, `None` for NULL. Accepts every type.
pub struct RawCell<'a>(pub Option<&'a [u8]>);

impl<'a> FromSql<'a> for RawCell<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawCell(Some(raw)))
    }
    fn from_sql_null(_: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawCell(None))
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

type BoxErr = Box<dyn std::error::Error + Sync + Send>;

/// Native decoders for the integer-stored kinds.
pub fn decode_i64(ty: &Type, raw: &[u8]) -> Result<i64, BoxErr> {
    Ok(match *ty {
        Type::INT2 => pt::int2_from_sql(raw)? as i64,
        Type::INT4 => pt::int4_from_sql(raw)? as i64,
        Type::INT8 => pt::int8_from_sql(raw)?,
        Type::OID | Type::XID | Type::CID => pt::oid_from_sql(raw)? as i64,
        Type::DATE => pt::date_from_sql(raw)? as i64,
        Type::TIME => pt::time_from_sql(raw)?,
        Type::TIMESTAMP | Type::TIMESTAMPTZ => pt::timestamp_from_sql(raw)?,
        _ => match ty.kind() {
            PgKind::Domain(base) => decode_i64(base, raw)?,
            _ => return Err("not an integer type".into()),
        },
    })
}

pub fn decode_f64(ty: &Type, raw: &[u8]) -> Result<f64, BoxErr> {
    Ok(match *ty {
        Type::FLOAT4 => pt::float4_from_sql(raw)? as f64,
        Type::FLOAT8 => pt::float8_from_sql(raw)?,
        _ => match ty.kind() {
            PgKind::Domain(base) => decode_f64(base, raw)?,
            _ => return Err("not a float type".into()),
        },
    })
}

/// Formats any binary value as PostgreSQL would print it (close to `psql`), appending to `out`.
/// Never fails: undecodable values come out as `\x…` hex.
pub fn format_binary(ty: &Type, raw: &[u8], out: &mut String) {
    if format_inner(ty, raw, out).is_err() {
        hex(raw, out);
    }
}

fn hex(raw: &[u8], out: &mut String) {
    out.push_str("\\x");
    for b in raw {
        let _ = write!(out, "{b:02x}");
    }
}

fn format_inner(ty: &Type, raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mark = out.len();
    let r = format_value(ty, raw, out);
    if r.is_err() {
        out.truncate(mark);
    }
    r
}

fn format_value(ty: &Type, raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    match *ty {
        Type::BOOL => out.push_str(if pt::bool_from_sql(raw)? { "true" } else { "false" }),
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID | Type::XID | Type::CID => {
            let v = match *ty {
                Type::INT2 => pt::int2_from_sql(raw)? as i64,
                Type::INT4 => pt::int4_from_sql(raw)? as i64,
                Type::INT8 => pt::int8_from_sql(raw)?,
                _ => pt::oid_from_sql(raw)? as i64,
            };
            let _ = write!(out, "{v}");
        }
        Type::FLOAT4 => format_float(pt::float4_from_sql(raw)? as f64, true, out),
        Type::FLOAT8 => format_float(pt::float8_from_sql(raw)?, false, out),
        Type::NUMERIC => format_numeric(raw, out)?,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN | Type::XML
        | Type::JSON => out.push_str(pt::text_from_sql(raw)?),
        Type::JSONB => {
            let (&ver, rest) = raw.split_first().ok_or("empty jsonb")?;
            if ver != 1 {
                return Err("jsonb version".into());
            }
            out.push_str(std::str::from_utf8(rest)?);
        }
        Type::CHAR => {
            let c = pt::char_from_sql(raw)? as u8;
            out.push(c as char);
        }
        Type::BYTEA => hex(raw, out),
        Type::DATE => format_date(pt::date_from_sql(raw)?, out),
        Type::TIME => format_time(pt::time_from_sql(raw)?, out),
        Type::TIMESTAMP => format_timestamp(pt::timestamp_from_sql(raw)?, out),
        Type::TIMESTAMPTZ => format_timestamptz(pt::timestamp_from_sql(raw)?, out),
        Type::TIMETZ => {
            if raw.len() != 12 {
                return Err("timetz".into());
            }
            let micros = i64::from_be_bytes(raw[0..8].try_into()?);
            let zone = -i32::from_be_bytes(raw[8..12].try_into()?);
            format_time(micros, out);
            format_offset(zone, out);
        }
        Type::INTERVAL => {
            if raw.len() != 16 {
                return Err("interval".into());
            }
            let micros = i64::from_be_bytes(raw[0..8].try_into()?);
            let days = i32::from_be_bytes(raw[8..12].try_into()?);
            let months = i32::from_be_bytes(raw[12..16].try_into()?);
            format_interval(months, days, micros, out);
        }
        Type::UUID => {
            let u = pt::uuid_from_sql(raw)?;
            let _ = write!(out, "{}", uuid::Uuid::from_bytes(u).hyphenated());
        }
        Type::INET | Type::CIDR => {
            let v = pt::inet_from_sql(raw)?;
            let full = if v.addr().is_ipv4() { 32 } else { 128 };
            let _ = write!(out, "{}", v.addr());
            if *ty == Type::CIDR || v.netmask() != full {
                let _ = write!(out, "/{}", v.netmask());
            }
        }
        Type::MACADDR => {
            let m = pt::macaddr_from_sql(raw)?;
            let _ = write!(out, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
        }
        Type::MONEY => {
            let cents = pt::int8_from_sql(raw)?;
            let sign = if cents < 0 { "-" } else { "" };
            let c = cents.unsigned_abs();
            let _ = write!(out, "{sign}{}.{:02}", c / 100, c % 100);
        }
        Type::PG_LSN => {
            let v = pt::lsn_from_sql(raw)?;
            let _ = write!(out, "{:X}/{:X}", v >> 32, v & 0xffff_ffff);
        }
        Type::VOID => {}
        Type::INT2_VECTOR | Type::OID_VECTOR => format_array(ty, raw, out, " ", false)?,
        _ => match ty.kind() {
            PgKind::Array(_) => format_array(ty, raw, out, ",", true)?,
            PgKind::Enum(_) => out.push_str(std::str::from_utf8(raw)?),
            PgKind::Domain(base) => format_value(base, raw, out)?,
            PgKind::Composite(fields) => format_record(raw, out, Some(fields))?,
            PgKind::Pseudo if *ty == Type::RECORD => format_record(raw, out, None)?,
            _ => match crate::pgtypes::format_special(ty, raw, out) {
                Some(r) => r?,
                // Text-like unknown types (citext…) usually send UTF-8; anything else is hex.
                None => match std::str::from_utf8(raw) {
                    Ok(s) if s.chars().all(|c| !c.is_control() || c == '\n' || c == '\t') => out.push_str(s),
                    _ => return Err("unknown".into()),
                },
            },
        },
    }
    Ok(())
}

fn type_by_oid(oid: u32) -> Type {
    Type::from_oid(oid).unwrap_or(Type::UNKNOWN)
}

fn format_array(ty: &Type, raw: &[u8], out: &mut String, sep: &str, braces: bool) -> Result<(), BoxErr> {
    use fallible_iterator::FallibleIterator;
    let arr = pt::array_from_sql(raw)?;
    let elem = match ty.kind() {
        PgKind::Array(e) => e.clone(),
        _ => type_by_oid(arr.element_type()),
    };
    let bounds: Vec<(i32, i32)> = arr.dimensions().map(|d| Ok((d.len, d.lower_bound))).collect()?;
    let dims: Vec<i32> = bounds.iter().map(|b| b.0).collect();
    let mut values = arr.values();
    if dims.is_empty() {
        if braces {
            out.push_str("{}");
        }
        return Ok(());
    }
    // `[0:2]={1,2,3}`: dimensions that do not start at 1 are spelled out.
    if braces && bounds.iter().any(|b| b.1 != 1) {
        for (len, lo) in &bounds {
            let _ = write!(out, "[{lo}:{}]", lo + len - 1);
        }
        out.push('=');
    }
    fn level(
        dims: &[i32],
        values: &mut postgres_protocol::types::ArrayValues<'_>,
        elem: &Type,
        out: &mut String,
        sep: &str,
        braces: bool,
    ) -> Result<(), BoxErr> {
        if braces {
            out.push('{');
        }
        for i in 0..dims[0] {
            if i > 0 {
                out.push_str(sep);
            }
            if dims.len() > 1 {
                level(&dims[1..], values, elem, out, sep, braces)?;
            } else {
                match values.next()? {
                    None => return Err("short array".into()),
                    Some(None) => out.push_str("NULL"),
                    Some(Some(v)) => {
                        let mut s = String::new();
                        format_binary(elem, v, &mut s);
                        if braces && *elem == Type::BOOL {
                            s = if s == "true" { "t".into() } else { "f".into() };
                        }
                        if braces && needs_array_quotes(&s) {
                            out.push('"');
                            for c in s.chars() {
                                if c == '"' || c == '\\' {
                                    out.push('\\');
                                }
                                out.push(c);
                            }
                            out.push('"');
                        } else {
                            out.push_str(&s);
                        }
                    }
                }
            }
        }
        if braces {
            out.push('}');
        }
        Ok(())
    }
    level(&dims, &mut values, &elem, out, sep, braces)
}

fn needs_array_quotes(s: &str) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\' | ' ' | '\t' | '\n'))
}

fn format_record(raw: &[u8], out: &mut String, fields: Option<&[tokio_postgres::types::Field]>) -> Result<(), BoxErr> {
    let mut b = raw;
    let take = |b: &mut &[u8], n: usize| -> Result<Vec<u8>, BoxErr> {
        if b.len() < n {
            return Err("short record".into());
        }
        let (h, t) = b.split_at(n);
        *b = t;
        Ok(h.to_vec())
    };
    let n = i32::from_be_bytes(take(&mut b, 4)?.try_into().unwrap());
    out.push('(');
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        let oid = u32::from_be_bytes(take(&mut b, 4)?.try_into().unwrap());
        let len = i32::from_be_bytes(take(&mut b, 4)?.try_into().unwrap());
        if len < 0 {
            continue;
        }
        let v = take(&mut b, len as usize)?;
        let mut s = String::new();
        match fields.and_then(|f| f.get(i as usize)) {
            Some(f) => format_binary(f.type_(), &v, &mut s),
            None => format_binary(&type_by_oid(oid), &v, &mut s),
        }
        if s.is_empty() || s.chars().any(|c| matches!(c, ',' | '(' | ')' | '"' | '\\' | ' ')) {
            out.push('"');
            out.push_str(&s.replace('\\', "\\\\").replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(&s);
        }
    }
    out.push(')');
    Ok(())
}

pub fn format_float(v: f64, single: bool, out: &mut String) {
    if v.is_nan() {
        out.push_str("NaN");
    } else if v.is_infinite() {
        out.push_str(if v > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        // Shortest round-trip digits, laid out like PostgreSQL's float4out/float8out: plain
        // decimal, or exponent form when the exponent is below -4 or at least 6 (real) / 15
        // (double precision).
        let sci = if single { format!("{:e}", v as f32) } else { format!("{v:e}") };
        let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        if exp < -4 || exp >= if single { 6 } else { 15 } {
            let _ = write!(out, "{mant}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
        } else if single {
            let _ = write!(out, "{}", v as f32);
        } else {
            let _ = write!(out, "{v}");
        }
    }
}

/// PostgreSQL's binary NUMERIC: ndigits, weight, sign, dscale, then base-10000 digits.
pub fn format_numeric(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    if raw.len() < 8 {
        return Err("numeric".into());
    }
    let rd = |i: usize| u16::from_be_bytes([raw[i], raw[i + 1]]);
    let ndigits = rd(0) as usize;
    let weight = rd(2) as i16 as i32;
    let sign = rd(4);
    let dscale = rd(6) as i32;
    match sign {
        0xC000 => {
            out.push_str("NaN");
            return Ok(());
        }
        0xD000 => {
            out.push_str("Infinity");
            return Ok(());
        }
        0xF000 => {
            out.push_str("-Infinity");
            return Ok(());
        }
        0x0000 | 0x4000 => {}
        _ => return Err("numeric sign".into()),
    }
    if raw.len() < 8 + 2 * ndigits {
        return Err("numeric digits".into());
    }
    let digit = |i: i32| -> u16 { if i >= 0 && (i as usize) < ndigits { rd(8 + 2 * i as usize) } else { 0 } };
    if sign == 0x4000 && ndigits > 0 {
        out.push('-');
    }
    // Integer part: groups 0..=weight.
    if weight < 0 {
        out.push('0');
    } else {
        for g in 0..=weight {
            let d = digit(g);
            if g == 0 {
                let _ = write!(out, "{d}");
            } else {
                let _ = write!(out, "{d:04}");
            }
        }
    }
    if dscale > 0 {
        out.push('.');
        let mut written = 0;
        let mut g = weight + 1;
        while written < dscale {
            let d = format!("{:04}", digit(g));
            for ch in d.chars() {
                if written == dscale {
                    break;
                }
                out.push(ch);
                written += 1;
            }
            g += 1;
        }
    }
    Ok(())
}

/// The proleptic Gregorian date of a day number counted from 1970-01-01 (H. Hinnant's
/// `civil_from_days`), as year (astronomical, 0 = 1 BC), month and day.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((yoe + era * 400) + (m <= 2) as i64, m, d)
}

/// `YYYY-MM-DD` of a day number counted from 2000-01-01; whether it is BC (the suffix is
/// written by the caller, after the time).
fn write_ymd(days: i64, out: &mut String) -> bool {
    let (y, m, d) = civil_from_days(days + 10_957);
    let bc = y <= 0;
    let _ = write!(out, "{:04}-{:02}-{:02}", if bc { 1 - y } else { y }, m, d);
    bc
}

pub fn format_date(days: i32, out: &mut String) {
    match days {
        i32::MAX => out.push_str("infinity"),
        i32::MIN => out.push_str("-infinity"),
        _ => {
            if write_ymd(days as i64, out) {
                out.push_str(" BC");
            }
        }
    }
}

fn frac(micros: u32, out: &mut String) {
    if micros > 0 {
        let s = format!("{micros:06}");
        out.push('.');
        out.push_str(s.trim_end_matches('0'));
    }
}

/// `HH:MM:SS[.ffffff]`; past 24 hours and below zero too (MySQL's TIME is a duration:
/// `-838:59:59` … `838:59:59`).
pub fn format_time(micros: i64, out: &mut String) {
    if micros < 0 {
        out.push('-');
    }
    let a = micros.unsigned_abs();
    let secs = a / 1_000_000;
    let us = (a % 1_000_000) as u32;
    let _ = write!(out, "{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60);
    frac(us, out);
}

// MySQL dates outside the calendar (`0000-00-00`, `2024-02-00`, `2024-02-31` where the SQL mode
// allows them) go in `Date` and `Timestamp` columns as `ODD_DATE + packed parts`: far below any
// PostgreSQL date (its minimum, 4713 BC, is about -2.1e17 µs from 2000), so they sort first
// among themselves in calendar order, and print as the server wrote them.

const ODD_DATE: i64 = i64::MIN + 1;
const ODD_SPAN: i64 = 10_000 * 13 * 32 * 24 * 60 * 60 * 1_000_000;

/// A date (and time) that is not on the calendar, as a `Date` / `Timestamp` cell value.
pub fn odd_date(y: u16, m: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> i64 {
    let parts = ((((((y.min(9999) as i64) * 13 + m.min(12) as i64) * 32 + d.min(31) as i64) * 24 + h.min(23) as i64) * 60
        + mi.min(59) as i64)
        * 60
        + s.min(59) as i64)
        * 1_000_000
        + us.min(999_999) as i64;
    ODD_DATE + parts
}

/// Whether a `Date` / `Timestamp` cell value is an [`odd_date`].
pub fn is_odd_date(v: i64) -> bool {
    (ODD_DATE..ODD_DATE + ODD_SPAN).contains(&v)
}

/// Prints an [`odd_date`] (`with_time`: a DATETIME, `0000-00-00 00:00:00`).
pub fn format_odd_date(v: i64, with_time: bool, out: &mut String) {
    let mut p = v - ODD_DATE;
    let us = p % 1_000_000;
    p /= 1_000_000;
    let (s, mi, h) = (p % 60, p / 60 % 60, p / 3600 % 24);
    p /= 86_400;
    let (d, m, y) = (p % 32, p / 32 % 13, p / 32 / 13);
    let _ = write!(out, "{y:04}-{m:02}-{d:02}");
    if with_time {
        let _ = write!(out, " {h:02}:{mi:02}:{s:02}");
        frac(us as u32, out);
    }
}

/// Days from 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 2000-01-01 (the `Date` cell value) of a calendar date; `None` off the calendar.
pub fn date_days(y: u16, m: u8, d: u8) -> Option<i64> {
    let (y, m, d) = (y as i64, m as i64, d as i64);
    if y == 0 || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d) - 10_957)
}

/// The `Date` cell value of a date's parts: its day number, or an [`odd_date`].
pub fn date_cell(y: u16, m: u8, d: u8) -> i64 {
    date_days(y, m, d).unwrap_or_else(|| odd_date(y, m, d, 0, 0, 0, 0))
}

/// The `Timestamp` cell value (µs since 2000-01-01) of a date and time, or an [`odd_date`].
pub fn timestamp_cell(y: u16, m: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> i64 {
    match date_days(y, m, d) {
        Some(days) if h < 24 && mi < 60 && s < 60 && us < 1_000_000 => {
            ((days * 24 + h as i64) * 60 + mi as i64) * 60_000_000 + s as i64 * 1_000_000 + us as i64
        }
        _ => odd_date(y, m, d, h, mi, s, us),
    }
}

pub fn format_timestamp(micros: i64, out: &mut String) {
    match micros {
        i64::MAX => out.push_str("infinity"),
        i64::MIN => out.push_str("-infinity"),
        _ => {
            let bc = write_ymd(micros.div_euclid(86_400_000_000), out);
            out.push(' ');
            format_time(micros.rem_euclid(86_400_000_000), out);
            if bc {
                out.push_str(" BC");
            }
        }
    }
}

/// Shown in the local zone (UTC beyond what chrono can convert).
pub fn format_timestamptz(micros: i64, out: &mut String) {
    if micros == i64::MAX || micros == i64::MIN {
        return format_timestamp(micros, out);
    }
    let utc = NaiveDate::from_ymd_opt(2000, 1, 1)
        .map(|d| d.and_time(NaiveTime::MIN))
        .and_then(|b| b.checked_add_signed(chrono::Duration::microseconds(micros)));
    let Some(utc) = utc else {
        format_timestamp(micros, out);
        out.push_str("+00");
        return;
    };
    let local: DateTime<Local> = Utc.from_utc_datetime(&utc).with_timezone(&Local);
    let naive = local.naive_local();
    use chrono::{Datelike, Timelike};
    let y = naive.year() as i64;
    let bc = y <= 0;
    let _ = write!(out, "{:04}-{:02}-{:02} ", if bc { 1 - y } else { y }, naive.month(), naive.day());
    let t = naive.time();
    let _ = write!(out, "{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second());
    frac(t.nanosecond() / 1000, out);
    format_offset(local.offset().local_minus_utc(), out);
    if bc {
        out.push_str(" BC");
    }
}

fn format_offset(secs: i32, out: &mut String) {
    let sign = if secs < 0 { '-' } else { '+' };
    let s = secs.unsigned_abs();
    let _ = write!(out, "{sign}{:02}", s / 3600);
    if s % 3600 != 0 {
        let _ = write!(out, ":{:02}", (s / 60) % 60);
    }
}

/// PostgreSQL's default `IntervalStyle = postgres` output.
pub fn format_interval(months: i32, days: i32, micros: i64, out: &mut String) {
    // PostgreSQL 17's infinite intervals: every field at its extreme.
    if (months, days, micros) == (i32::MAX, i32::MAX, i64::MAX) {
        return out.push_str("infinity");
    }
    if (months, days, micros) == (i32::MIN, i32::MIN, i64::MIN) {
        return out.push_str("-infinity");
    }
    let mut parts: Vec<String> = Vec::new();
    let (y, m) = (months / 12, months % 12);
    let unit = |n: i64, s: &str, p: &str| format!("{n} {}", if n == 1 { s } else { p });
    if y != 0 {
        parts.push(unit(y as i64, "year", "years"));
    }
    if m != 0 {
        parts.push(unit(m as i64, "mon", "mons"));
    }
    if days != 0 {
        parts.push(unit(days as i64, "day", "days"));
    }
    if micros != 0 || parts.is_empty() {
        let neg = micros < 0;
        let a = micros.unsigned_abs();
        let secs = a / 1_000_000;
        let mut t = String::new();
        if neg {
            t.push('-');
        }
        let _ = write!(t, "{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60);
        frac((a % 1_000_000) as u32, &mut t);
        parts.push(t);
    }
    out.push_str(&parts.join(" "));
}

/// Compares two decimal strings numerically (`NaN` sorts last, like PostgreSQL).
pub fn cmp_decimal(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    fn class(s: &str) -> i8 {
        match s {
            "NaN" => 3,
            "Infinity" => 2,
            "-Infinity" => -2,
            _ => 0,
        }
    }
    let (ca, cb) = (class(a), class(b));
    if ca != 0 || cb != 0 {
        let rank = |c: i8, s: &str| -> (i8, bool) {
            if c != 0 { (c, false) } else { (if s.starts_with('-') { -1 } else { 1 }, true) }
        };
        let (ra, fa) = rank(ca, a);
        let (rb, fb) = rank(cb, b);
        if fa && fb {
            unreachable!()
        }
        // Finite numbers sit between -Infinity (-2) and Infinity (2).
        let ka = if fa { 0 } else { ra };
        let kb = if fb { 0 } else { rb };
        return ka.cmp(&kb);
    }
    let (na, a) = a.strip_prefix('-').map(|r| (true, r)).unwrap_or((false, a));
    let (nb, b) = b.strip_prefix('-').map(|r| (true, r)).unwrap_or((false, b));
    let is_zero = |s: &str| s.bytes().all(|c| c == b'0' || c == b'.');
    let na = na && !is_zero(a);
    let nb = nb && !is_zero(b);
    match (na, nb) {
        (true, false) => return Less,
        (false, true) => return Greater,
        _ => {}
    }
    let (ia, fa) = a.split_once('.').unwrap_or((a, ""));
    let (ib, fb) = b.split_once('.').unwrap_or((b, ""));
    let ia = ia.trim_start_matches('0');
    let ib = ib.trim_start_matches('0');
    let mag = ia.len().cmp(&ib.len()).then_with(|| ia.cmp(ib)).then_with(|| {
        let fa = fa.trim_end_matches('0');
        let fb = fb.trim_end_matches('0');
        fa.cmp(fb)
    });
    if na { mag.reverse() } else { mag }
}

/// The grid's order for text: case-insensitive, a run of digits by its value ("a2" before
/// "a10", "007" with "7"), ties broken by the plain byte order so the order stays total.
pub fn cmp_natural(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (x, y) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        let (c, d) = (x[i], y[j]);
        if c.is_ascii_digit() && d.is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < x.len() && x[i].is_ascii_digit() {
                i += 1;
            }
            while j < y.len() && y[j].is_ascii_digit() {
                j += 1;
            }
            let trim = |s: &[u8]| -> usize { s.iter().position(|&b| b != b'0').unwrap_or(s.len()) };
            let (p, q) = (&x[si..i], &y[sj..j]);
            let (p, q) = (&p[trim(p)..], &q[trim(q)..]);
            let o = p.len().cmp(&q.len()).then_with(|| p.cmp(q));
            if o != Equal {
                return o;
            }
            continue;
        }
        if c < 0x80 && d < 0x80 {
            let o = c.to_ascii_lowercase().cmp(&d.to_ascii_lowercase());
            if o != Equal {
                return o;
            }
            i += 1;
            j += 1;
            continue;
        }
        // Outside ASCII: a character each, by its lowercase.
        let (Some(p), Some(q)) = (a[i..].chars().next(), b[j..].chars().next()) else { break };
        let o = p.to_lowercase().cmp(q.to_lowercase());
        if o != Equal {
            return o;
        }
        i += p.len_utf8();
        j += q.len_utf8();
    }
    let rest = match (i < x.len(), j < y.len()) {
        (false, true) => Less,
        (true, false) => Greater,
        _ => Equal,
    };
    rest.then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_natural("a2", "a10"), Less);
        assert_eq!(cmp_natural("Item 10", "item 9"), Greater);
        assert_eq!(cmp_natural("abc", "ABD"), Less);
        assert_eq!(cmp_natural("b", "Apple"), Greater);
        assert_eq!(cmp_natural("007", "7"), Less, "equal value: the byte order decides");
        assert_eq!(cmp_natural("x", "x"), Equal);
        assert_eq!(cmp_natural("x", "x1"), Less);
        assert_eq!(cmp_natural("Élodie", "élodie"), Less);
        assert_eq!(cmp_natural("élodie", "zebra"), Greater);
        assert_eq!(cmp_natural("file-1", "file_1"), Less);
        let mut v = vec!["v10", "V2", "v1", "a", "B", "v02", "", "10", "9"];
        v.sort_by(|a, b| cmp_natural(a, b));
        assert_eq!(v, ["", "9", "10", "a", "B", "v1", "V2", "v02", "v10"]);
    }

    fn numeric(ndigits: u16, weight: i16, sign: u16, dscale: u16, digits: &[u16]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in [ndigits, weight as u16, sign, dscale] {
            v.extend_from_slice(&x.to_be_bytes());
        }
        for d in digits {
            v.extend_from_slice(&d.to_be_bytes());
        }
        v
    }

    fn num(raw: &[u8]) -> String {
        let mut s = String::new();
        format_numeric(raw, &mut s).unwrap();
        s
    }

    #[test]
    fn numerics() {
        assert_eq!(num(&numeric(2, 1, 0, 2, &[12, 3456])), "123456.00");
        assert_eq!(num(&numeric(2, 0, 0x4000, 3, &[1, 2500])), "-1.250");
        assert_eq!(num(&numeric(1, -1, 0, 4, &[5])), "0.0005");
        assert_eq!(num(&numeric(0, 0, 0, 0, &[])), "0");
        assert_eq!(num(&numeric(0, 0, 0, 2, &[])), "0.00");
        assert_eq!(num(&numeric(1, 2, 0, 0, &[7])), "700000000");
        assert_eq!(num(&numeric(0, 0, 0xC000, 0, &[])), "NaN");
    }

    #[test]
    fn dates_and_times() {
        let mut s = String::new();
        format_date(0, &mut s);
        assert_eq!(s, "2000-01-01");
        s.clear();
        format_date(-1, &mut s);
        assert_eq!(s, "1999-12-31");
        s.clear();
        format_timestamp(86_400_000_000 + 3_723_500_000, &mut s);
        assert_eq!(s, "2000-01-02 01:02:03.5");
        s.clear();
        format_timestamp(i64::MAX, &mut s);
        assert_eq!(s, "infinity");
        s.clear();
        format_interval(14, 3, 4 * 3_600_000_000 + 5 * 60_000_000 + 6_000_000, &mut s);
        assert_eq!(s, "1 year 2 mons 3 days 04:05:06");
        s.clear();
        format_interval(0, 0, 0, &mut s);
        assert_eq!(s, "00:00:00");
    }

    #[test]
    fn floats_like_postgres() {
        let f = |v: f64, single| {
            let mut s = String::new();
            format_float(v, single, &mut s);
            s
        };
        assert_eq!(f(0.1, false), "0.1");
        assert_eq!(f(1.5e-7, false), "1.5e-07");
        assert_eq!(f(1e300, false), "1e+300");
        assert_eq!(f(123456789012345.0, false), "123456789012345");
        assert_eq!(f(1e15, false), "1e+15");
        assert_eq!(f(0.1f32 as f64, true), "0.1");
        assert_eq!(f(1234567.0, true), "1.234567e+06");
        assert_eq!(f(-0.0001, false), "-0.0001");
    }

    #[test]
    fn decimal_order() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_decimal("10", "9.99"), Greater);
        assert_eq!(cmp_decimal("-10", "-9"), Less);
        assert_eq!(cmp_decimal("0.10", "0.1"), Equal);
        assert_eq!(cmp_decimal("-0.00", "0"), Equal);
        assert_eq!(cmp_decimal("NaN", "1e9"), Greater);
        assert_eq!(cmp_decimal("-Infinity", "-5"), Less);
        assert_eq!(cmp_decimal("Infinity", "NaN"), Less);
        assert_eq!(cmp_decimal("007", "7"), Equal);
    }
}
