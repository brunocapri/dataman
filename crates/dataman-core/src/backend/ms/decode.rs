use std::fmt::Write as _;

use tiberius::ColumnData;
use tiberius::ColumnType;

use crate::result::{ChunkBuilder, ColumnInfo};
use crate::value::{self, Kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dec {
    pub kind: Kind,
    pub money: bool,
}

pub(crate) fn kind_of(ty: ColumnType) -> (Kind, &'static str) {
    use ColumnType::*;
    match ty {
        Bit | Bitn => (Kind::Bool, "bit"),
        Int1 => (Kind::Int, "tinyint"),
        Int2 => (Kind::Int, "smallint"),
        Int4 | Intn => (Kind::Int, "int"),
        Int8 => (Kind::Int, "bigint"),
        Float4 => (Kind::Float4, "real"),
        Float8 | Floatn => (Kind::Float, "float"),
        Money | Money4 => (Kind::Numeric, if ty == Money4 { "smallmoney" } else { "money" }),
        Decimaln => (Kind::Numeric, "decimal"),
        Numericn => (Kind::Numeric, "numeric"),
        Datetime4 => (Kind::Timestamp, "smalldatetime"),
        Datetime | Datetimen => (Kind::Timestamp, "datetime"),
        Datetime2 => (Kind::Timestamp, "datetime2"),
        Daten => (Kind::Date, "date"),
        Timen => (Kind::Time, "time"),
        DatetimeOffsetn => (Kind::Text, "datetimeoffset"),
        Guid => (Kind::Other, "uniqueidentifier"),
        BigVarChar => (Kind::Text, "varchar"),
        BigChar => (Kind::Text, "char"),
        NVarchar => (Kind::Text, "nvarchar"),
        NChar => (Kind::Text, "nchar"),
        Text => (Kind::Text, "text"),
        NText => (Kind::Text, "ntext"),
        Xml => (Kind::Text, "xml"),
        BigVarBin => (Kind::Bytes, "varbinary"),
        BigBinary => (Kind::Bytes, "binary"),
        Image => (Kind::Bytes, "image"),
        Udt => (Kind::Bytes, "udt"),
        SSVariant => (Kind::Other, "sql_variant"),
        Null => (Kind::Text, "null"),
    }
}

pub(crate) fn column_info(c: &tiberius::Column) -> (ColumnInfo, Dec) {
    let ty = c.column_type();
    let (kind, type_name) = kind_of(ty);
    let dec = Dec { kind, money: matches!(ty, ColumnType::Money | ColumnType::Money4) };
    (ColumnInfo { name: c.name().to_string(), type_name: type_name.into(), type_oid: 0, kind }, dec)
}

pub(crate) fn column_infos(cols: &[tiberius::Column]) -> (Vec<ColumnInfo>, Vec<Dec>) {
    cols.iter().map(column_info).unzip()
}

const DAY_1900: i64 = -36_524;
const DAY_0001: i64 = -730_119;
const DAY_US: i64 = 86_400_000_000;

fn date_days(d: tiberius::time::Date) -> i64 {
    d.days() as i64 + DAY_0001
}

fn time_us(t: tiberius::time::Time) -> i64 {
    let (inc, scale) = (t.increments() as i64, t.scale() as u32);
    if scale >= 6 { inc / 10i64.pow(scale - 6) } else { inc * 10i64.pow(6 - scale) }
}

fn tenth_micro(t: tiberius::time::Time) -> u8 {
    let (inc, scale) = (t.increments(), t.scale() as u32);
    if scale >= 7 { (inc % 10) as u8 } else { 0 }
}

fn add_tenth(out: &mut String, start: usize, tenth: u8) {
    if tenth == 0 {
        return;
    }
    match out[start..].rfind('.') {
        Some(dot) => {
            let frac = out.len() - start - dot - 1;
            out.extend(std::iter::repeat_n('0', 6 - frac));
        }
        None => out.push_str(".000000"),
    }
    out.push((b'0' + tenth) as char);
}

fn timestamp_text(us: i64, tenth: u8, out: &mut String) {
    let start = out.len();
    value::format_timestamp(us, out);
    add_tenth(out, start, tenth);
}

fn datetime2_us(dt: tiberius::time::DateTime2) -> i64 {
    date_days(dt.date()) * DAY_US + time_us(dt.time())
}

/// 1/300 s ticks, rounded to the milliseconds SQL Server shows (`.003`, `.007`).
fn datetime_us(dt: tiberius::time::DateTime) -> i64 {
    let ms = (dt.seconds_fragments() as i64 * 10 + 1) / 3;
    (dt.days() as i64 + DAY_1900) * DAY_US + ms * 1000
}

fn smalldatetime_us(dt: tiberius::time::SmallDateTime) -> i64 {
    (dt.days() as i64 + DAY_1900) * DAY_US + dt.seconds_fragments() as i64 * 60_000_000
}

pub(crate) fn decimal(value: i128, scale: u8, out: &mut String) {
    if scale == 0 {
        let _ = write!(out, "{value}");
        return;
    }
    let p = 10i128.pow(scale as u32);
    if value < 0 {
        out.push('-');
    }
    let a = value.unsigned_abs();
    let p = p as u128;
    let _ = write!(out, "{}.{:0width$}", a / p, a % p, width = scale as usize);
}

/// tiberius reads `money` as an `f64` (the scaled integer divided by 10⁴): the integer back.
fn money(v: f64, out: &mut String) {
    decimal((v * 1e4).round() as i128, 4, out);
}

fn real(x: f32) -> f64 {
    let mut s = String::new();
    let _ = write!(s, "{x}");
    s.parse().unwrap_or(x as f64)
}

fn hex(b: &[u8], out: &mut String) {
    out.push_str("\\x");
    for x in b {
        let _ = write!(out, "{x:02x}");
    }
}

fn guid(u: &tiberius::Uuid, out: &mut String) {
    let _ = write!(out, "{}", u.hyphenated().encode_upper(&mut uuid::Uuid::encode_buffer()));
}

pub(crate) fn text_into(v: &ColumnData<'_>, money_col: bool, out: &mut String) -> bool {
    match v {
        ColumnData::U8(Some(x)) => {
            let _ = write!(out, "{x}");
        }
        ColumnData::I16(Some(x)) => {
            let _ = write!(out, "{x}");
        }
        ColumnData::I32(Some(x)) => {
            let _ = write!(out, "{x}");
        }
        ColumnData::I64(Some(x)) => {
            let _ = write!(out, "{x}");
        }
        ColumnData::F32(Some(x)) => value::format_float(real(*x), true, out),
        ColumnData::F64(Some(x)) if money_col => money(*x, out),
        ColumnData::F64(Some(x)) => value::format_float(*x, false, out),
        ColumnData::Bit(Some(b)) => out.push_str(if *b { "1" } else { "0" }),
        ColumnData::String(Some(s)) => out.push_str(s),
        ColumnData::Guid(Some(u)) => guid(u, out),
        ColumnData::Binary(Some(b)) => hex(b, out),
        ColumnData::Numeric(Some(n)) => decimal(n.value(), n.scale(), out),
        ColumnData::Xml(Some(x)) => out.push_str(x.as_ref().as_ref()),
        ColumnData::DateTime(Some(dt)) => value::format_timestamp(datetime_us(*dt), out),
        ColumnData::SmallDateTime(Some(dt)) => value::format_timestamp(smalldatetime_us(*dt), out),
        ColumnData::Time(Some(t)) => {
            let start = out.len();
            value::format_time(time_us(*t), out);
            add_tenth(out, start, tenth_micro(*t));
        }
        ColumnData::Date(Some(d)) => value::format_date(date_days(*d) as i32, out),
        ColumnData::DateTime2(Some(dt)) => timestamp_text(datetime2_us(*dt), tenth_micro(dt.time()), out),
        ColumnData::DateTimeOffset(Some(dto)) => {
            let off = dto.offset() as i64;
            timestamp_text(datetime2_us(dto.datetime2()) + off * 60_000_000, tenth_micro(dto.datetime2().time()), out);
            let a = off.unsigned_abs();
            let _ = write!(out, " {}{:02}:{:02}", if off < 0 { '-' } else { '+' }, a / 60, a % 60);
        }
        _ => return false,
    }
    true
}

pub(crate) fn value_text(v: &ColumnData<'_>, ty: ColumnType) -> Option<String> {
    let mut s = String::new();
    text_into(v, matches!(ty, ColumnType::Money | ColumnType::Money4), &mut s).then_some(s)
}

pub(crate) fn push_value(b: &mut ChunkBuilder, i: usize, d: Dec, v: &ColumnData<'_>) {
    let int = |b: &mut ChunkBuilder, x: i64| match d.kind {
        Kind::Int => b.push_i64(i, x),
        Kind::Float | Kind::Float4 => b.push_f64(i, x as f64),
        Kind::Bool => b.push_bool(i, x != 0),
        _ => b.push_text_with(i, |s| {
            let _ = write!(s, "{x}");
        }),
    };
    match v {
        ColumnData::U8(Some(x)) => int(b, *x as i64),
        ColumnData::I16(Some(x)) => int(b, *x as i64),
        ColumnData::I32(Some(x)) => int(b, *x as i64),
        ColumnData::I64(Some(x)) => int(b, *x),
        ColumnData::Bit(Some(x)) if d.kind == Kind::Bool => b.push_bool(i, *x),
        ColumnData::F32(Some(x)) if matches!(d.kind, Kind::Float | Kind::Float4) => b.push_f64(i, real(*x)),
        ColumnData::F64(Some(x)) if matches!(d.kind, Kind::Float | Kind::Float4) && !d.money => b.push_f64(i, *x),
        ColumnData::DateTime(Some(dt)) if d.kind == Kind::Timestamp => b.push_i64(i, datetime_us(*dt)),
        ColumnData::SmallDateTime(Some(dt)) if d.kind == Kind::Timestamp => b.push_i64(i, smalldatetime_us(*dt)),
        ColumnData::DateTime2(Some(dt)) if d.kind == Kind::Timestamp => b.push_i64(i, datetime2_us(*dt)),
        ColumnData::Date(Some(x)) if d.kind == Kind::Date => b.push_i64(i, date_days(*x)),
        ColumnData::Time(Some(t)) if d.kind == Kind::Time => b.push_i64(i, time_us(*t)),
        ColumnData::DateTimeOffset(Some(dto)) if d.kind == Kind::TimestampTz => b.push_i64(i, datetime2_us(dto.datetime2())),
        v if is_null(v) => b.push_null(i),
        v => match d.kind.storage() {
            value::Storage::Text => b.push_text_with(i, |s| {
                text_into(v, d.money, s);
            }),
            _ => b.push_null(i),
        },
    }
}

fn is_null(v: &ColumnData<'_>) -> bool {
    matches!(
        v,
        ColumnData::U8(None)
            | ColumnData::I16(None)
            | ColumnData::I32(None)
            | ColumnData::I64(None)
            | ColumnData::F32(None)
            | ColumnData::F64(None)
            | ColumnData::Bit(None)
            | ColumnData::String(None)
            | ColumnData::Guid(None)
            | ColumnData::Binary(None)
            | ColumnData::Numeric(None)
            | ColumnData::Xml(None)
            | ColumnData::DateTime(None)
            | ColumnData::SmallDateTime(None)
            | ColumnData::Time(None)
            | ColumnData::Date(None)
            | ColumnData::DateTime2(None)
            | ColumnData::DateTimeOffset(None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::format_cell;
    use tiberius::numeric::Numeric;
    use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};

    fn col(kind: Kind) -> ColumnInfo {
        ColumnInfo { name: "c".into(), type_name: String::new(), type_oid: 0, kind }
    }

    fn shown(ty: ColumnType, v: ColumnData<'static>) -> Option<String> {
        let (kind, _) = kind_of(ty);
        let mut b = ChunkBuilder::new(&[col(kind)]);
        push_value(&mut b, 0, Dec { kind, money: matches!(ty, ColumnType::Money | ColumnType::Money4) }, &v);
        b.end_row();
        let chunk = b.finish();
        format_cell(kind, chunk.cell(0, 0)).map(|s| s.into_owned())
    }

    #[test]
    fn kinds_from_metadata() {
        use ColumnType::*;
        assert_eq!(kind_of(Bitn).0, Kind::Bool);
        for t in [Int1, Int2, Int4, Int8, Intn] {
            assert_eq!(kind_of(t).0, Kind::Int);
        }
        assert_eq!(kind_of(Float4), (Kind::Float4, "real"));
        assert_eq!(kind_of(Floatn).0, Kind::Float);
        for t in [Money, Money4, Decimaln, Numericn] {
            assert_eq!(kind_of(t).0, Kind::Numeric);
        }
        assert_eq!(kind_of(Datetime2).0, Kind::Timestamp);
        assert_eq!(kind_of(DatetimeOffsetn).0, Kind::Text);
        assert_eq!(kind_of(Daten).0, Kind::Date);
        assert_eq!(kind_of(Timen).0, Kind::Time);
        assert_eq!(kind_of(Guid), (Kind::Other, "uniqueidentifier"));
        assert_eq!(kind_of(NVarchar).0, Kind::Text);
        assert_eq!(kind_of(Xml).0, Kind::Text);
        assert_eq!(kind_of(BigVarBin).0, Kind::Bytes);
        assert_eq!(kind_of(Udt).0, Kind::Bytes);
        assert_eq!(kind_of(SSVariant).0, Kind::Other);
    }

    #[test]
    fn cells() {
        use ColumnType as T;
        assert_eq!(shown(T::Bitn, ColumnData::Bit(Some(true))).as_deref(), Some("true"));
        assert_eq!(shown(T::Int1, ColumnData::U8(Some(255))).as_deref(), Some("255"));
        assert_eq!(shown(T::Intn, ColumnData::I32(Some(-7))).as_deref(), Some("-7"));
        assert_eq!(shown(T::Intn, ColumnData::I64(Some(i64::MIN))).as_deref(), Some("-9223372036854775808"));
        assert_eq!(shown(T::Intn, ColumnData::I32(None)), None);
        assert_eq!(shown(T::Float4, ColumnData::F32(Some(0.1))).as_deref(), Some("0.1"));
        assert_eq!(shown(T::Floatn, ColumnData::F32(Some(0.1))).as_deref(), Some("0.1"), "a nullable real");
        assert_eq!(shown(T::Floatn, ColumnData::F64(Some(1e300))).as_deref(), Some("1e+300"));
        assert_eq!(shown(T::Money, ColumnData::F64(Some(1520.5))).as_deref(), Some("1520.5000"));
        assert_eq!(shown(T::Money, ColumnData::F64(Some(-0.0001))).as_deref(), Some("-0.0001"));
        assert_eq!(shown(T::Money, ColumnData::F64(Some(99_999_999_999.9999))).as_deref(), Some("99999999999.9999"));
        assert_eq!(shown(T::Money, ColumnData::F64(None)), None);
        assert_eq!(shown(T::Decimaln, ColumnData::Numeric(Some(Numeric::new_with_scale(152050, 2)))).as_deref(), Some("1520.50"));
        assert_eq!(shown(T::Decimaln, ColumnData::Numeric(Some(Numeric::new_with_scale(-5, 3)))).as_deref(), Some("-0.005"));
        assert_eq!(
            shown(T::Decimaln, ColumnData::Numeric(Some(Numeric::new_with_scale(12345678901234567890123456789012345678, 0)))).as_deref(),
            Some("12345678901234567890123456789012345678"),
            "38 digits stay exact"
        );
        let u = tiberius::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff").unwrap();
        assert_eq!(shown(T::Guid, ColumnData::Guid(Some(u))).as_deref(), Some("6F9619FF-8B86-D011-B42D-00C04FC964FF"));
        assert_eq!(shown(T::BigVarBin, ColumnData::Binary(Some(vec![0xde, 0xad].into()))).as_deref(), Some("\\xdead"));
        assert_eq!(shown(T::NVarchar, ColumnData::String(Some("café 東京".into()))).as_deref(), Some("café 東京"));
        assert_eq!(shown(T::NVarchar, ColumnData::String(None)), None);
        assert_eq!(shown(T::NVarchar, ColumnData::String(Some("".into()))).as_deref(), Some(""));
        assert_eq!(shown(T::SSVariant, ColumnData::I32(Some(42))).as_deref(), Some("42"));
        assert_eq!(shown(T::SSVariant, ColumnData::String(Some("x".into()))).as_deref(), Some("x"));
        assert_eq!(shown(T::SSVariant, ColumnData::String(None)), None);
    }

    #[test]
    fn dates_and_times() {
        use ColumnType as T;
        let day = Date::new(739_901);
        assert_eq!(shown(T::Daten, ColumnData::Date(Some(day))).as_deref(), Some("2026-10-13"));
        assert_eq!(shown(T::Daten, ColumnData::Date(Some(Date::new(0)))).as_deref(), Some("0001-01-01"));
        assert_eq!(shown(T::Daten, ColumnData::Date(Some(Date::new(3_652_058)))).as_deref(), Some("9999-12-31"));
        let t = Time::new(((9 * 3600 + 30 * 60) as u64) * 10_000_000 + 1_234_567, 7);
        assert_eq!(shown(T::Timen, ColumnData::Time(Some(t))).as_deref(), Some("09:30:00.123456"));
        assert_eq!(shown(T::Timen, ColumnData::Time(Some(Time::new(5, 1)))).as_deref(), Some("00:00:00.5"));
        let dt2 = DateTime2::new(day, Time::new(((23 * 3600 + 59 * 60 + 59) as u64) * 1000 + 250, 3));
        assert_eq!(shown(T::Datetime2, ColumnData::DateTime2(Some(dt2))).as_deref(), Some("2026-10-13 23:59:59.25"));
        let days_1900 = 46_306;
        assert_eq!(shown(T::Datetimen, ColumnData::DateTime(Some(DateTime::new(days_1900, 300 * 3600 + 1)))).as_deref(), Some("2026-10-13 01:00:00.003"));
        assert_eq!(shown(T::Datetimen, ColumnData::DateTime(Some(DateTime::new(days_1900, 2)))).as_deref(), Some("2026-10-13 00:00:00.007"));
        assert_eq!(shown(T::Datetimen, ColumnData::DateTime(Some(DateTime::new(-53_690, 0)))).as_deref(), Some("1753-01-01 00:00:00"));
        assert_eq!(shown(T::Datetime4, ColumnData::SmallDateTime(Some(SmallDateTime::new(46_306, 61)))).as_deref(), Some("2026-10-13 01:01:00"));
        let dto = DateTimeOffset::new(DateTime2::new(day, Time::new(12 * 3600, 0)), -180);
        let mut b = ChunkBuilder::new(&[col(Kind::TimestampTz)]);
        push_value(&mut b, 0, Dec { kind: Kind::TimestampTz, money: false }, &ColumnData::DateTimeOffset(Some(dto)));
        b.end_row();
        let mut utc = String::new();
        value::format_timestamp(match b.finish().cell(0, 0) {
            crate::result::Cell::Int(v) => v,
            c => panic!("{c:?}"),
        }, &mut utc);
        assert_eq!(utc, "2026-10-13 12:00:00");
        assert_eq!(value_text(&ColumnData::DateTimeOffset(Some(dto)), T::DatetimeOffsetn).as_deref(), Some("2026-10-13 09:00:00 -03:00"));
    }

    #[test]
    fn full_precision_text() {
        use ColumnType as T;
        let day = Date::new(739_901);
        let t7 = |inc: u64| Time::new(((9 * 3600 + 30 * 60) as u64) * 10_000_000 + inc, 7);
        assert_eq!(value_text(&ColumnData::Time(Some(t7(1_234_567))), T::Timen).as_deref(), Some("09:30:00.1234567"));
        assert_eq!(value_text(&ColumnData::Time(Some(t7(5))), T::Timen).as_deref(), Some("09:30:00.0000005"));
        assert_eq!(value_text(&ColumnData::Time(Some(t7(1_234_560))), T::Timen).as_deref(), Some("09:30:00.123456"));
        assert_eq!(value_text(&ColumnData::DateTime2(Some(DateTime2::new(day, t7(1_234_567)))), T::Datetime2).as_deref(), Some("2026-10-13 09:30:00.1234567"));
        let dto = DateTimeOffset::new(DateTime2::new(day, t7(1_234_567)), -330);
        assert_eq!(value_text(&ColumnData::DateTimeOffset(Some(dto)), T::DatetimeOffsetn).as_deref(), Some("2026-10-13 04:00:00.1234567 -05:30"));
    }

    #[test]
    fn texts() {
        assert_eq!(value_text(&ColumnData::Bit(Some(false)), ColumnType::Bitn).as_deref(), Some("0"));
        assert_eq!(value_text(&ColumnData::F64(Some(12.5)), ColumnType::Money).as_deref(), Some("12.5000"));
        assert_eq!(value_text(&ColumnData::F64(Some(12.5)), ColumnType::Floatn).as_deref(), Some("12.5"));
        assert_eq!(value_text(&ColumnData::I32(None), ColumnType::Intn), None);
        let mut s = String::new();
        decimal(-123456, 3, &mut s);
        assert_eq!(s, "-123.456");
        assert_eq!(real(0.3), 0.3);
    }
}
