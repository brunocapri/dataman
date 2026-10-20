use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PgType {
    Int8,
    Numeric,
    Float8,
    Bool,
    Date,
    Timestamp,
    TimestampTz,
    Time,
    Uuid,
    Jsonb,
    Bytea,
    Text,
}

impl PgType {
    pub const ALL: [PgType; 12] = [
        PgType::Int8,
        PgType::Numeric,
        PgType::Float8,
        PgType::Bool,
        PgType::Date,
        PgType::Timestamp,
        PgType::TimestampTz,
        PgType::Time,
        PgType::Uuid,
        PgType::Jsonb,
        PgType::Bytea,
        PgType::Text,
    ];

    pub fn sql(self) -> &'static str {
        match self {
            PgType::Int8 => "int8",
            PgType::Numeric => "numeric",
            PgType::Float8 => "float8",
            PgType::Bool => "boolean",
            PgType::Date => "date",
            PgType::Timestamp => "timestamp",
            PgType::TimestampTz => "timestamptz",
            PgType::Time => "time",
            PgType::Uuid => "uuid",
            PgType::Jsonb => "jsonb",
            PgType::Bytea => "bytea",
            PgType::Text => "text",
        }
    }
}

const INT: u16 = 1;
const NUM: u16 = 1 << 1;
const FLT: u16 = 1 << 2;
const BOOL: u16 = 1 << 3;
const DATE: u16 = 1 << 4;
const TS: u16 = 1 << 5;
const UUID: u16 = 1 << 6;
const JSON: u16 = 1 << 7;
const ALL: u16 = INT | NUM | FLT | BOOL | DATE | TS | UUID | JSON;

#[derive(Clone, Copy, Debug)]
pub struct Infer {
    mask: u16,
    seen: usize,
}

impl Default for Infer {
    fn default() -> Self {
        Infer { mask: ALL, seen: 0 }
    }
}

impl Infer {
    pub fn observe(&mut self, s: &str) {
        let s = s.trim();
        if s.is_empty() || self.mask == 0 {
            if !s.is_empty() {
                self.seen += 1;
            }
            return;
        }
        self.seen += 1;
        self.mask &= kinds_of(s);
    }

    pub fn observe_json(&mut self, v: &Value) {
        match v {
            Value::Null => {}
            Value::Bool(_) => {
                self.seen += 1;
                self.mask &= BOOL;
            }
            Value::Number(n) => {
                self.seen += 1;
                self.mask &= if n.is_i64() {
                    INT | NUM | FLT
                } else if n.is_u64() || !n.to_string().contains(['e', 'E']) {
                    NUM | FLT
                } else {
                    FLT
                };
            }
            Value::String(s) => self.observe(s),
            Value::Array(_) | Value::Object(_) => {
                self.seen += 1;
                self.mask &= JSON;
            }
        }
    }

    pub fn accepts(&self, ty: PgType) -> bool {
        let bit = match ty {
            PgType::Int8 => INT,
            PgType::Numeric => NUM,
            PgType::Float8 => FLT,
            PgType::Bool => BOOL,
            PgType::Date => DATE,
            PgType::Timestamp | PgType::TimestampTz => TS,
            PgType::Uuid => UUID,
            PgType::Jsonb => JSON,
            PgType::Text => return true,
            PgType::Time | PgType::Bytea => return false,
        };
        self.mask & bit != 0
    }

    pub fn result(&self) -> PgType {
        if self.seen == 0 {
            return PgType::Text;
        }
        for (bit, ty) in [
            (BOOL, PgType::Bool),
            (INT, PgType::Int8),
            (NUM, PgType::Numeric),
            (FLT, PgType::Float8),
            (DATE, PgType::Date),
            (TS, PgType::TimestampTz),
            (UUID, PgType::Uuid),
            (JSON, PgType::Jsonb),
        ] {
            if self.mask & bit != 0 {
                return ty;
            }
        }
        PgType::Text
    }
}

fn kinds_of(s: &str) -> u16 {
    let mut m = 0;
    match s.to_ascii_lowercase().as_str() {
        "true" | "false" | "t" | "f" | "yes" | "no" | "y" | "n" => m |= BOOL,
        "nan" | "infinity" | "-infinity" | "inf" | "-inf" => m |= FLT,
        _ => {}
    }
    match number(s) {
        Some(Num::Int) => m |= INT | NUM | FLT,
        Some(Num::Big) | Some(Num::Decimal) => m |= NUM | FLT,
        Some(Num::Exp) => m |= FLT,
        None => {}
    }
    if m & (INT | NUM | FLT | BOOL) != 0 {
        return m;
    }
    match datetime(s) {
        Some(true) => m |= DATE | TS,
        Some(false) => m |= TS,
        None => {}
    }
    if is_uuid(s) {
        m |= UUID;
    }
    if (s.starts_with('{') && s.ends_with('}') || s.starts_with('[') && s.ends_with(']'))
        && serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
    {
        m |= JSON;
    }
    m
}

enum Num {
    Int,
    Big,
    Decimal,
    Exp,
}

fn number(s: &str) -> Option<Num> {
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mant, exp) = match digits.find(['e', 'E']) {
        Some(i) => (&digits[..i], Some(&digits[i + 1..])),
        None => (digits, None),
    };
    let (int, frac) = match mant.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mant, None),
    };
    let all = |p: &str| p.bytes().all(|c| c.is_ascii_digit());
    if int.is_empty() && frac.is_none_or(str::is_empty) || !all(int) || !frac.is_none_or(all) {
        return None;
    }
    // `007`: an identifier that a number column would turn into 7.
    if int.len() > 1 && int.starts_with('0') {
        return None;
    }
    if let Some(e) = exp {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        return (!e.is_empty() && all(e)).then_some(Num::Exp);
    }
    if frac.is_some() {
        return Some(Num::Decimal);
    }
    Some(if s.parse::<i64>().is_ok() { Num::Int } else { Num::Big })
}

fn datetime(s: &str) -> Option<bool> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).filter(|p| p.bytes().all(|c| c.is_ascii_digit())).and_then(|p| p.parse::<u32>().ok());
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    chrono::NaiveDate::from_ymd_opt(y as i32, mo, d)?;
    if b.len() == 10 {
        return Some(true);
    }
    if b[10] != b'T' && b[10] != b' ' || b.len() < 16 || b[13] != b':' {
        return None;
    }
    let (h, mi) = (num(11..13)?, num(14..16)?);
    if h > 24 || mi > 59 {
        return None;
    }
    let mut rest = &s[16..];
    if let Some(r) = rest.strip_prefix(':') {
        let sec = r.get(..2).filter(|p| p.bytes().all(|c| c.is_ascii_digit()))?;
        if sec.parse::<u32>().ok()? > 60 {
            return None;
        }
        rest = &r[2..];
        if let Some(f) = rest.strip_prefix('.') {
            let n = f.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 {
                return None;
            }
            rest = &f[n..];
        }
    }
    let rest = rest.trim_start();
    if rest.is_empty() || rest == "Z" || rest == "z" || rest.eq_ignore_ascii_case("utc") {
        return Some(false);
    }
    let z = rest.strip_prefix(['+', '-'])?;
    let ok = match z.len() {
        2 | 4 => z.bytes().all(|c| c.is_ascii_digit()),
        5 => z.as_bytes()[2] == b':' && z.bytes().enumerate().all(|(i, c)| i == 2 || c.is_ascii_digit()),
        _ => false,
    };
    ok.then_some(false)
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infer(vals: &[&str]) -> PgType {
        let mut i = Infer::default();
        for v in vals {
            i.observe(v);
        }
        i.result()
    }

    #[test]
    fn numbers() {
        assert_eq!(infer(&["1", "-2", "+30", ""]), PgType::Int8);
        assert_eq!(infer(&["1", "2.50"]), PgType::Numeric);
        assert_eq!(infer(&["99999999999999999999"]), PgType::Numeric, "too big for int8");
        assert_eq!(infer(&["1", "2.5", "1e10"]), PgType::Float8);
        assert_eq!(infer(&["1.5", "NaN"]), PgType::Float8);
        assert_eq!(infer(&["007", "12"]), PgType::Text, "leading zeros stay text");
        assert_eq!(infer(&["0", "0.5", "10"]), PgType::Numeric);
        assert_eq!(infer(&["1,5"]), PgType::Text);
        assert_eq!(infer(&[".5", "-.25"]), PgType::Numeric);
        assert_eq!(infer(&["1.", "."]), PgType::Text);
    }

    #[test]
    fn booleans_dates_and_times() {
        assert_eq!(infer(&["true", "False", "t", "NO"]), PgType::Bool);
        assert_eq!(infer(&["1", "0"]), PgType::Int8, "0/1 stay numbers");
        assert_eq!(infer(&["2024-02-29", "2023-12-31"]), PgType::Date);
        assert_eq!(infer(&["2023-02-29"]), PgType::Text, "not a real day");
        assert_eq!(infer(&["2024-01-01", "2024-01-02 10:20:30"]), PgType::TimestampTz);
        for ts in ["2024-01-02T10:20", "2024-01-02 10:20:30.123456Z", "2024-01-02T10:20:30+02:00", "2024-01-02 10:20:30-0300", "2024-01-02 10:20:30 UTC"] {
            assert_eq!(infer(&[ts]), PgType::TimestampTz, "{ts}");
        }
        assert_eq!(infer(&["2024-01-02 25:00"]), PgType::Text);
        assert_eq!(infer(&["2024-01-02x"]), PgType::Text);
    }

    #[test]
    fn uuids_json_and_text() {
        assert_eq!(infer(&["0f8fad5b-d9cb-469f-a165-70867728950e", "7C9E6679-7425-40DE-944B-E07FC1F90AE7"]), PgType::Uuid);
        assert_eq!(infer(&["{\"a\": 1}", "[1, 2]"]), PgType::Jsonb);
        assert_eq!(infer(&["{not json}"]), PgType::Text);
        assert_eq!(infer(&["12", "abc"]), PgType::Text);
        assert_eq!(infer(&["", "  "]), PgType::Text, "nothing seen");
        assert_eq!(infer(&[]), PgType::Text);
    }

    #[test]
    fn json_values() {
        let mut i = Infer::default();
        for v in [serde_json::json!(1), serde_json::json!(null), serde_json::json!(-7)] {
            i.observe_json(&v);
        }
        assert_eq!(i.result(), PgType::Int8);
        let mut i = Infer::default();
        i.observe_json(&serde_json::json!(1));
        i.observe_json(&serde_json::json!(2.5));
        assert_eq!(i.result(), PgType::Numeric);
        let mut i = Infer::default();
        i.observe_json(&serde_json::json!({"a": [1]}));
        i.observe_json(&serde_json::json!([]));
        assert_eq!(i.result(), PgType::Jsonb);
        let mut i = Infer::default();
        i.observe_json(&serde_json::json!("2024-01-01"));
        assert_eq!(i.result(), PgType::Date);
        let mut i = Infer::default();
        i.observe_json(&serde_json::json!(true));
        i.observe_json(&serde_json::json!("x"));
        assert_eq!(i.result(), PgType::Text);
        assert!(Infer::default().accepts(PgType::Text));
    }
}
