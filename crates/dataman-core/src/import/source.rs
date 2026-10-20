use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::Value;

use super::csv::{self, Reader, Record, decode_field};
use super::infer::{Infer, PgType};
use super::{CsvDialect, ImportFormat, PREVIEW_ROWS, Preview, SAMPLE_BYTES, SAMPLE_ROWS, SourceColumn};

#[derive(Debug, Default)]
pub struct Row {
    vals: Vec<String>,
    null: Vec<bool>,
}

impl Row {
    pub fn new(n: usize) -> Row {
        Row { vals: vec![String::new(); n], null: vec![true; n] }
    }

    pub fn get(&self, i: usize) -> Option<&str> {
        match self.null.get(i) {
            Some(false) => Some(&self.vals[i]),
            _ => None,
        }
    }

    fn clear(&mut self) {
        self.null.iter_mut().for_each(|n| *n = true);
    }

    fn slot(&mut self, i: usize) -> &mut String {
        self.null[i] = false;
        let s = &mut self.vals[i];
        s.clear();
        s
    }

    fn set_null(&mut self, i: usize) {
        self.null[i] = true;
    }

    #[cfg(test)]
    pub fn from_values(vals: &[Option<&str>]) -> Row {
        let mut r = Row::new(vals.len());
        for (i, v) in vals.iter().enumerate() {
            if let Some(v) = v {
                r.slot(i).push_str(v);
            }
        }
        r
    }

    pub fn to_vec(&self) -> Vec<Option<String>> {
        (0..self.vals.len()).map(|i| self.get(i).map(str::to_string)).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Go,
    Stop,
}

struct Counting<R> {
    inner: R,
    n: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.n.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

pub struct Spec<'a> {
    pub format: ImportFormat,
    pub dialect: &'a CsvDialect,
    pub names: &'a [String],
}

pub fn read_rows(
    path: &Path,
    spec: &Spec<'_>,
    bytes: Arc<AtomicU64>,
    on_row: &mut dyn FnMut(&Row, u64) -> Result<Flow>,
    on_bad: &mut dyn FnMut(u64, String) -> Result<Flow>,
) -> Result<()> {
    if spec.format == ImportFormat::Parquet {
        return read_parquet(path, spec.names.len(), None, bytes, on_row);
    }
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut input = BufReader::with_capacity(256 * 1024, Counting { inner: file, n: bytes });
    skip_bom(&mut input)?;
    match spec.format {
        ImportFormat::Csv | ImportFormat::Tsv => read_delimited(input, spec.dialect, spec.names.len(), on_row, on_bad),
        ImportFormat::Ndjson => read_ndjson(input, &key_index(spec.names), on_row, on_bad),
        ImportFormat::Json => read_json_array(input, &key_index(spec.names), on_row, on_bad),
        ImportFormat::Parquet => unreachable!(),
    }
}

fn skip_bom<R: BufRead>(r: &mut R) -> io::Result<()> {
    let b = r.fill_buf()?;
    if b.starts_with(&[0xef, 0xbb, 0xbf]) {
        r.consume(3);
    }
    Ok(())
}

fn key_index(names: &[String]) -> HashMap<String, usize> {
    names.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect()
}

fn fill_from_record(rec: &Record, d: &CsvDialect, row: &mut Row, n: usize) -> Result<(), String> {
    if rec.len() != n {
        return Err(format!("{} fields, expected {n}", rec.len()));
    }
    for i in 0..n {
        let (raw, quoted) = rec.get(i).unwrap();
        let slot = row.slot(i);
        match decode_field(raw, quoted, d, slot) {
            Ok(true) => {}
            Ok(false) => row.set_null(i),
            Err(()) => return Err(format!("not valid {} (choose another encoding)", d.encoding.label())),
        }
    }
    Ok(())
}

fn read_delimited<R: BufRead>(
    input: R,
    d: &CsvDialect,
    n: usize,
    on_row: &mut dyn FnMut(&Row, u64) -> Result<Flow>,
    on_bad: &mut dyn FnMut(u64, String) -> Result<Flow>,
) -> Result<()> {
    let mut r = Reader::new(input, d.delimiter, d.quote);
    let mut rec = Record::default();
    let mut row = Row::new(n);
    let mut first = true;
    while r.read(&mut rec).with_context(|| "reading the file")? {
        if first {
            first = false;
            if d.header {
                continue;
            }
        }
        if rec.is_blank() {
            continue;
        }
        let flow = match fill_from_record(&rec, d, &mut row, n) {
            Ok(()) => on_row(&row, rec.line)?,
            Err(why) => on_bad(rec.line, why)?,
        };
        if flow == Flow::Stop {
            break;
        }
    }
    Ok(())
}

pub fn column_names(raw: &[Option<String>]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    raw.iter()
        .enumerate()
        .map(|(i, n)| {
            let base = n.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| format!("column_{}", i + 1));
            let k = seen.entry(base.clone()).or_insert(0);
            *k += 1;
            if *k == 1 { base } else { format!("{base}_{k}") }
        })
        .collect()
}

pub fn preview_delimited(head: &[u8], complete: bool, format: ImportFormat, given: Option<CsvDialect>) -> Result<Preview> {
    let body = head.strip_prefix(&[0xef, 0xbb, 0xbf][..]).unwrap_or(head);
    let sample = if complete {
        body
    } else {
        match body.iter().rposition(|&c| c == b'\n') {
            Some(i) => &body[..=i],
            None => body,
        }
    };
    let dialect = match given {
        Some(d) => d,
        None => detect_dialect(sample, complete, format),
    };
    let recs = csv::records(sample, dialect.delimiter, dialect.quote, SAMPLE_ROWS + 1);
    let decoded: Vec<Vec<Option<String>>> = recs
        .iter()
        .map(|r| {
            (0..r.len())
                .map(|i| {
                    let (raw, q) = r.get(i).unwrap();
                    let mut s = String::new();
                    match decode_field(raw, q, &dialect, &mut s) {
                        Ok(true) => Some(s),
                        Ok(false) => None,
                        Err(()) => Some(String::from_utf8_lossy(raw).into_owned()),
                    }
                })
                .collect()
        })
        .collect();
    let ncols = decoded.first().map(Vec::len).unwrap_or(0);
    let (names, data) = match decoded.split_first() {
        Some((h, rest)) if dialect.header => (column_names(h), rest),
        _ => (column_names(&vec![None; ncols]), &decoded[..]),
    };
    let mut inf = vec![Infer::default(); names.len()];
    for r in data {
        for (c, v) in r.iter().enumerate().take(names.len()) {
            if let Some(v) = v {
                inf[c].observe(v);
            }
        }
    }
    let rows: Vec<Vec<Option<String>>> =
        data.iter().take(PREVIEW_ROWS).map(|r| (0..names.len()).map(|i| r.get(i).cloned().flatten()).collect()).collect();
    let exact = complete && recs.len() <= SAMPLE_ROWS;
    Ok(Preview {
        path: Default::default(),
        format,
        dialect,
        columns: names.into_iter().zip(&inf).map(|(name, i)| SourceColumn { name, ty: i.result() }).collect(),
        rows,
        file_bytes: 0,
        total_rows: exact.then_some(data.len() as u64),
        rows_exact: exact,
        sampled: data.len(),
    })
}

pub fn detect_dialect(sample: &[u8], complete: bool, format: ImportFormat) -> CsvDialect {
    let encoding = csv::sniff_encoding(sample, complete);
    let mut d = if format == ImportFormat::Tsv {
        CsvDialect::tsv()
    } else {
        let delimiter = csv::sniff_delimiter(sample, Some(b'"'));
        CsvDialect { delimiter, quote: csv::sniff_quote(sample, delimiter), ..CsvDialect::default() }
    };
    d.encoding = encoding;
    let recs = csv::records(sample, d.delimiter, d.quote, 50);
    let mut probe = d.clone();
    probe.null_text = "\u{0}".into();
    let decoded: Vec<Vec<Option<String>>> = recs
        .iter()
        .map(|r| {
            (0..r.len())
                .map(|i| {
                    let (raw, q) = r.get(i).unwrap();
                    let mut s = String::new();
                    decode_field(raw, q, &probe, &mut s).ok().map(|_| s)
                })
                .collect()
        })
        .collect();
    d.header = csv::sniff_header(&decoded);
    d
}

fn json_text(v: &Value, out: &mut String) -> bool {
    match v {
        Value::Null => return false,
        Value::String(s) => out.push_str(s),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            use std::fmt::Write;
            let _ = write!(out, "{n}");
        }
        Value::Array(_) | Value::Object(_) => out.push_str(&serde_json::to_string(v).unwrap_or_default()),
    }
    true
}

fn fill_from_object(obj: &serde_json::Map<String, Value>, keys: &HashMap<String, usize>, row: &mut Row) {
    row.clear();
    for (k, v) in obj {
        if let Some(&i) = keys.get(k) {
            let filled = json_text(v, row.slot(i));
            if !filled {
                row.set_null(i);
            }
        }
    }
}

fn read_ndjson<R: BufRead>(
    mut input: R,
    keys: &HashMap<String, usize>,
    on_row: &mut dyn FnMut(&Row, u64) -> Result<Flow>,
    on_bad: &mut dyn FnMut(u64, String) -> Result<Flow>,
) -> Result<()> {
    let mut line = Vec::new();
    let mut row = Row::new(keys.len());
    let mut n = 0u64;
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        n += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let flow = match serde_json::from_slice::<Value>(&line) {
            Ok(Value::Object(o)) => {
                fill_from_object(&o, keys, &mut row);
                on_row(&row, n)?
            }
            Ok(_) => on_bad(n, "not a JSON object".into())?,
            Err(e) => on_bad(n, format!("invalid JSON: {e}"))?,
        };
        if flow == Flow::Stop {
            return Ok(());
        }
    }
}

/// serde_json's map sorts its keys unless a crate in the build turns `preserve_order` on: an
/// object's entries are kept here in the file's order.
enum Ordered {
    Object(Vec<(String, Value)>),
    Other,
}

impl<'de> serde::Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Ordered, D::Error> {
        use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
        struct V;
        macro_rules! other {
            ($($name:ident: $t:ty),*) => {$(
                fn $name<E: serde::de::Error>(self, _: $t) -> Result<Ordered, E> { Ok(Ordered::Other) }
            )*};
        }
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Ordered, A::Error> {
                let mut v = Vec::new();
                while let Some((k, val)) = m.next_entry::<String, Value>()? {
                    v.push((k, val));
                }
                Ok(Ordered::Object(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Ordered, A::Error> {
                while s.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Ordered::Other)
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Ordered, E> {
                Ok(Ordered::Other)
            }
            other!(visit_bool: bool, visit_i64: i64, visit_u64: u64, visit_f64: f64, visit_str: &str);
        }
        d.deserialize_any(V)
    }
}

fn each_element<R: Read, T: serde::de::DeserializeOwned>(input: R, f: &mut dyn FnMut(T) -> Result<bool>) -> Result<()> {
    use serde::de::{Deserializer, SeqAccess, Visitor};
    struct V<'a, T> {
        f: &'a mut dyn FnMut(T) -> Result<bool>,
        stopped: &'a mut bool,
        failure: &'a mut Option<anyhow::Error>,
    }
    impl<'de, T: serde::de::DeserializeOwned> Visitor<'de> for V<'_, T> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an array of objects")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while let Some(v) = seq.next_element::<T>()? {
                match (self.f)(v) {
                    Ok(true) => {}
                    Ok(false) => {
                        *self.stopped = true;
                        return Err(serde::de::Error::custom("stopped"));
                    }
                    Err(e) => {
                        *self.failure = Some(e);
                        return Err(serde::de::Error::custom("failed"));
                    }
                }
            }
            Ok(())
        }
    }
    let (mut stopped, mut failure) = (false, None);
    let mut de = serde_json::Deserializer::from_reader(input);
    let r = de.deserialize_seq(V { f, stopped: &mut stopped, failure: &mut failure });
    if let Some(e) = failure {
        return Err(e);
    }
    if stopped {
        return Ok(());
    }
    r.map_err(|e| anyhow!("invalid JSON: {e}"))
}

fn read_json_array<R: BufRead>(
    input: R,
    keys: &HashMap<String, usize>,
    on_row: &mut dyn FnMut(&Row, u64) -> Result<Flow>,
    on_bad: &mut dyn FnMut(u64, String) -> Result<Flow>,
) -> Result<()> {
    let mut row = Row::new(keys.len());
    let mut n = 0u64;
    each_element(input, &mut |v: Value| {
        n += 1;
        let flow = match v {
            Value::Object(o) => {
                fill_from_object(&o, keys, &mut row);
                on_row(&row, n)?
            }
            _ => on_bad(n, "not a JSON object".into())?,
        };
        Ok(flow == Flow::Go)
    })
}

fn json_sample(path: &Path, format: ImportFormat) -> Result<(Vec<Vec<(String, Value)>>, bool)> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut input = BufReader::new(file);
    skip_bom(&mut input)?;
    let mut out = Vec::new();
    let mut complete = true;
    if format == ImportFormat::Ndjson {
        let mut line = Vec::new();
        let mut used = 0usize;
        loop {
            line.clear();
            let n = input.read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            used += n;
            if let Ok(Ordered::Object(o)) = serde_json::from_slice::<Ordered>(&line) {
                out.push(o);
            }
            if out.len() >= SAMPLE_ROWS || used >= SAMPLE_BYTES {
                complete = input.fill_buf()?.is_empty();
                break;
            }
        }
    } else {
        let mut seen = 0usize;
        let r = each_element(input, &mut |v: Ordered| {
            seen += 1;
            if let Ordered::Object(o) = v {
                out.push(o);
            }
            if seen >= SAMPLE_ROWS {
                complete = false;
                return Ok(false);
            }
            Ok(true)
        });
        if let Err(e) = r
            && out.is_empty()
        {
            return Err(e);
        }
    }
    Ok((out, complete))
}

pub fn preview_json(path: &Path, format: ImportFormat) -> Result<Preview> {
    let (objs, complete) = json_sample(path, format)?;
    if objs.is_empty() {
        bail!("no JSON objects at the start of {}", path.display());
    }
    let mut names: Vec<String> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for o in &objs {
        for (k, _) in o {
            if !index.contains_key(k) {
                index.insert(k.clone(), names.len());
                names.push(k.clone());
            }
        }
    }
    let mut inf = vec![Infer::default(); names.len()];
    for o in &objs {
        for (k, v) in o {
            inf[index[k]].observe_json(v);
        }
    }
    let mut row = Row::new(names.len());
    let rows = objs
        .iter()
        .take(PREVIEW_ROWS)
        .map(|o| {
            row.clear();
            for (k, v) in o {
                let i = index[k];
                if !json_text(v, row.slot(i)) {
                    row.set_null(i);
                }
            }
            row.to_vec()
        })
        .collect();
    let n = objs.len();
    Ok(Preview {
        path: Default::default(),
        format,
        dialect: CsvDialect::default(),
        columns: names.into_iter().zip(&inf).map(|(name, i)| SourceColumn { name, ty: i.result() }).collect(),
        rows,
        file_bytes: 0,
        total_rows: complete.then_some(n as u64),
        rows_exact: complete,
        sampled: n,
    })
}

mod pq {
    use std::fmt::Write as _;

    use arrow_array::cast::AsArray;
    use arrow_array::types::*;
    use arrow_array::{Array, ArrayRef};
    use arrow_schema::{DataType, TimeUnit};

    use super::PgType;

    pub fn pg_type(t: &DataType) -> PgType {
        match t {
            DataType::Boolean => PgType::Bool,
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::UInt8 | DataType::UInt16 | DataType::UInt32 => {
                PgType::Int8
            }
            DataType::UInt64 | DataType::Decimal128(..) | DataType::Decimal256(..) => PgType::Numeric,
            DataType::Float16 | DataType::Float32 | DataType::Float64 => PgType::Float8,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => PgType::Text,
            DataType::Binary | DataType::LargeBinary | DataType::BinaryView | DataType::FixedSizeBinary(_) => PgType::Bytea,
            DataType::Date32 | DataType::Date64 => PgType::Date,
            DataType::Time32(_) | DataType::Time64(_) => PgType::Time,
            DataType::Timestamp(_, Some(_)) => PgType::TimestampTz,
            DataType::Timestamp(_, None) => PgType::Timestamp,
            DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) | DataType::Struct(_) | DataType::Map(..) => PgType::Jsonb,
            DataType::Dictionary(_, v) => pg_type(v),
            _ => PgType::Text,
        }
    }

    fn micros(unit: &TimeUnit, v: i64) -> (i64, u32) {
        let (div, mul) = match unit {
            TimeUnit::Second => (1, 1_000_000_000),
            TimeUnit::Millisecond => (1_000, 1_000_000),
            TimeUnit::Microsecond => (1_000_000, 1_000),
            TimeUnit::Nanosecond => (1_000_000_000, 1),
        };
        (v.div_euclid(div), (v.rem_euclid(div) * mul) as u32)
    }

    fn push_ts(out: &mut String, unit: &TimeUnit, v: i64, tz: bool) -> bool {
        let (s, ns) = micros(unit, v);
        let Some(dt) = chrono::DateTime::from_timestamp(s, ns) else { return false };
        let _ = write!(out, "{}", dt.naive_utc().format("%Y-%m-%d %H:%M:%S%.f"));
        if tz {
            out.push_str("+00");
        }
        true
    }

    fn push_time(out: &mut String, nanos: i64) -> bool {
        let Some(t) = chrono::NaiveTime::from_num_seconds_from_midnight_opt((nanos / 1_000_000_000) as u32, (nanos % 1_000_000_000) as u32)
        else {
            return false;
        };
        let _ = write!(out, "{}", t.format("%H:%M:%S%.f"));
        true
    }

    fn push_hex(out: &mut String, b: &[u8]) {
        out.push_str("\\x");
        for x in b {
            let _ = write!(out, "{x:02x}");
        }
    }

    fn push_float(out: &mut String, f: f64) {
        if f.is_nan() {
            out.push_str("NaN");
        } else if f.is_infinite() {
            out.push_str(if f > 0. { "Infinity" } else { "-Infinity" });
        } else {
            let _ = write!(out, "{f}");
        }
    }

    pub fn text(a: &dyn Array, i: usize, out: &mut String) -> bool {
        if a.is_null(i) {
            return false;
        }
        macro_rules! prim {
            ($t:ty) => {{
                let _ = write!(out, "{}", a.as_primitive::<$t>().value(i));
                true
            }};
        }
        match a.data_type() {
            DataType::Boolean => {
                out.push_str(if a.as_boolean().value(i) { "true" } else { "false" });
                true
            }
            DataType::Int8 => prim!(Int8Type),
            DataType::Int16 => prim!(Int16Type),
            DataType::Int32 => prim!(Int32Type),
            DataType::Int64 => prim!(Int64Type),
            DataType::UInt8 => prim!(UInt8Type),
            DataType::UInt16 => prim!(UInt16Type),
            DataType::UInt32 => prim!(UInt32Type),
            DataType::UInt64 => prim!(UInt64Type),
            DataType::Float16 => {
                push_float(out, a.as_primitive::<Float16Type>().value(i).to_f64());
                true
            }
            DataType::Float32 => {
                push_float(out, a.as_primitive::<Float32Type>().value(i) as f64);
                true
            }
            DataType::Float64 => {
                push_float(out, a.as_primitive::<Float64Type>().value(i));
                true
            }
            DataType::Decimal128(..) => {
                out.push_str(&a.as_primitive::<Decimal128Type>().value_as_string(i));
                true
            }
            DataType::Decimal256(..) => {
                out.push_str(&a.as_primitive::<Decimal256Type>().value_as_string(i));
                true
            }
            DataType::Utf8 => {
                out.push_str(a.as_string::<i32>().value(i));
                true
            }
            DataType::LargeUtf8 => {
                out.push_str(a.as_string::<i64>().value(i));
                true
            }
            DataType::Utf8View => {
                out.push_str(a.as_string_view().value(i));
                true
            }
            DataType::Binary => {
                push_hex(out, a.as_binary::<i32>().value(i));
                true
            }
            DataType::LargeBinary => {
                push_hex(out, a.as_binary::<i64>().value(i));
                true
            }
            DataType::BinaryView => {
                push_hex(out, a.as_binary_view().value(i));
                true
            }
            DataType::FixedSizeBinary(_) => {
                push_hex(out, a.as_fixed_size_binary().value(i));
                true
            }
            DataType::Date32 => {
                let d = a.as_primitive::<Date32Type>().value(i);
                match chrono::NaiveDate::from_num_days_from_ce_opt(d + 719_163) {
                    Some(d) => {
                        let _ = write!(out, "{}", d.format("%Y-%m-%d"));
                        true
                    }
                    None => false,
                }
            }
            DataType::Date64 => {
                let ms = a.as_primitive::<Date64Type>().value(i);
                match chrono::DateTime::from_timestamp_millis(ms) {
                    Some(d) => {
                        let _ = write!(out, "{}", d.format("%Y-%m-%d"));
                        true
                    }
                    None => false,
                }
            }
            DataType::Time32(TimeUnit::Second) => push_time(out, a.as_primitive::<Time32SecondType>().value(i) as i64 * 1_000_000_000),
            DataType::Time32(_) => push_time(out, a.as_primitive::<Time32MillisecondType>().value(i) as i64 * 1_000_000),
            DataType::Time64(TimeUnit::Microsecond) => push_time(out, a.as_primitive::<Time64MicrosecondType>().value(i) * 1_000),
            DataType::Time64(_) => push_time(out, a.as_primitive::<Time64NanosecondType>().value(i)),
            DataType::Timestamp(unit, tz) => {
                let v = match unit {
                    TimeUnit::Second => a.as_primitive::<TimestampSecondType>().value(i),
                    TimeUnit::Millisecond => a.as_primitive::<TimestampMillisecondType>().value(i),
                    TimeUnit::Microsecond => a.as_primitive::<TimestampMicrosecondType>().value(i),
                    TimeUnit::Nanosecond => a.as_primitive::<TimestampNanosecondType>().value(i),
                };
                push_ts(out, unit, v, tz.is_some())
            }
            DataType::Dictionary(..) => {
                let d = a.as_any_dictionary();
                let keys = d.normalized_keys();
                text(d.values().as_ref(), keys[i], out)
            }
            DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) | DataType::Struct(_) | DataType::Map(..) => {
                json(a, i, out);
                true
            }
            _ => false,
        }
    }

    pub fn json(a: &dyn Array, i: usize, out: &mut String) {
        if a.is_null(i) {
            out.push_str("null");
            return;
        }
        let list = |out: &mut String, items: ArrayRef| {
            out.push('[');
            for j in 0..items.len() {
                if j > 0 {
                    out.push(',');
                }
                json(items.as_ref(), j, out);
            }
            out.push(']');
        };
        match a.data_type() {
            DataType::List(_) => list(out, a.as_list::<i32>().value(i)),
            DataType::LargeList(_) => list(out, a.as_list::<i64>().value(i)),
            DataType::FixedSizeList(..) => list(out, a.as_fixed_size_list().value(i)),
            DataType::Struct(fields) => {
                let s = a.as_struct();
                out.push('{');
                for (j, f) in fields.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(f.name()).unwrap_or_default());
                    out.push(':');
                    json(s.column(j).as_ref(), i, out);
                }
                out.push('}');
            }
            DataType::Map(..) => {
                let m = a.as_map();
                let entries = m.value(i);
                let (k, v) = (entries.column(0), entries.column(1));
                out.push('{');
                for j in 0..entries.len() {
                    if j > 0 {
                        out.push(',');
                    }
                    let mut key = String::new();
                    text(k.as_ref(), j, &mut key);
                    out.push_str(&serde_json::to_string(&key).unwrap_or_default());
                    out.push(':');
                    json(v.as_ref(), j, out);
                }
                out.push('}');
            }
            DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64 => {
                text(a, i, out);
            }
            DataType::Float16 | DataType::Float32 | DataType::Float64 | DataType::Decimal128(..) | DataType::Decimal256(..) => {
                let mut s = String::new();
                text(a, i, &mut s);
                if s.parse::<f64>().is_ok_and(f64::is_finite) {
                    out.push_str(&s);
                } else {
                    out.push_str(&serde_json::to_string(&s).unwrap_or_default());
                }
            }
            _ => {
                let mut s = String::new();
                if text(a, i, &mut s) {
                    out.push_str(&serde_json::to_string(&s).unwrap_or_default());
                } else {
                    out.push_str("null");
                }
            }
        }
    }
}

fn read_parquet(
    path: &Path,
    ncols: usize,
    limit: Option<usize>,
    bytes: Arc<AtomicU64>,
    on_row: &mut dyn FnMut(&Row, u64) -> Result<Flow>,
) -> Result<()> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let b = ParquetRecordBatchReaderBuilder::try_new(file).context("reading the Parquet metadata")?;
    let total = b.metadata().file_metadata().num_rows().max(1) as u64;
    let reader = b.with_batch_size(8192).build()?;
    let mut row = Row::new(ncols);
    let mut n = 0u64;
    for batch in reader {
        let batch = batch.context("reading Parquet rows")?;
        let cols = batch.num_columns().min(ncols);
        for r in 0..batch.num_rows() {
            for c in 0..cols {
                let filled = pq::text(batch.column(c).as_ref(), r, row.slot(c));
                if !filled {
                    row.set_null(c);
                }
            }
            n += 1;
            if on_row(&row, n)? == Flow::Stop || limit.is_some_and(|l| n as usize >= l) {
                return Ok(());
            }
        }
        bytes.store(size.saturating_mul(n) / total, Ordering::Relaxed);
    }
    Ok(())
}

pub fn preview_parquet(path: &Path) -> Result<Preview> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let b = ParquetRecordBatchReaderBuilder::try_new(file).context("not a Parquet file")?;
    let total = b.metadata().file_metadata().num_rows().max(0) as u64;
    let fields: Vec<(String, PgType)> = b.schema().fields().iter().map(|f| (f.name().clone(), pq::pg_type(f.data_type()))).collect();
    drop(b);
    let names: Vec<String> = column_names(&fields.iter().map(|(n, _)| Some(n.clone())).collect::<Vec<_>>());
    let mut inf = vec![Infer::default(); names.len()];
    let mut rows = Vec::new();
    let mut sampled = 0usize;
    read_parquet(path, names.len(), Some(SAMPLE_ROWS), Arc::new(AtomicU64::new(0)), &mut |row, _| {
        sampled += 1;
        for (c, (_, ty)) in fields.iter().enumerate() {
            if *ty == PgType::Text
                && let Some(v) = row.get(c)
            {
                inf[c].observe(v);
            }
        }
        if rows.len() < PREVIEW_ROWS {
            rows.push(row.to_vec());
        }
        Ok(Flow::Go)
    })?;
    let columns = names
        .into_iter()
        .zip(&fields)
        .zip(&inf)
        .map(|((name, (_, ty)), i)| SourceColumn { name, ty: if *ty == PgType::Text { i.result() } else { *ty } })
        .collect();
    Ok(Preview {
        path: Default::default(),
        format: ImportFormat::Parquet,
        dialect: CsvDialect::default(),
        columns,
        rows,
        file_bytes: 0,
        total_rows: Some(total),
        rows_exact: true,
        sampled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, body: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        (dir, p)
    }

    fn all_rows(path: &Path, format: ImportFormat, d: &CsvDialect, names: &[String]) -> (Vec<(u64, Vec<Option<String>>)>, Vec<(u64, String)>) {
        let (mut good, mut bad) = (Vec::new(), Vec::new());
        let spec = Spec { format, dialect: d, names };
        read_rows(
            path,
            &spec,
            Arc::new(AtomicU64::new(0)),
            &mut |r, l| {
                good.push((l, r.to_vec()));
                Ok(Flow::Go)
            },
            &mut |l, why| {
                bad.push((l, why));
                Ok(Flow::Go)
            },
        )
        .unwrap();
        (good, bad)
    }

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn csv_preview_and_rows() {
        let body = "\u{feff}id;name;amount;at\n1;\"Smith; J\";1.50;2024-01-02\n2;;\"\";2024-01-03\n3;x;2;\n";
        let (_d, p) = tmp("a.csv", body.as_bytes());
        let pv = super::super::preview(&p, None, None).unwrap();
        assert_eq!(pv.format, ImportFormat::Csv);
        assert_eq!(pv.dialect.delimiter, b';');
        assert!(pv.dialect.header);
        assert_eq!(pv.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "amount", "at"]);
        assert_eq!(pv.columns.iter().map(|c| c.ty).collect::<Vec<_>>(), [PgType::Int8, PgType::Text, PgType::Numeric, PgType::Date]);
        assert_eq!(pv.rows.len(), 3);
        assert_eq!(pv.total_rows, Some(3));
        assert!(pv.rows_exact);
        let names: Vec<String> = pv.columns.iter().map(|c| c.name.clone()).collect();
        let (good, bad) = all_rows(&p, pv.format, &pv.dialect, &names);
        assert!(bad.is_empty());
        assert_eq!(good[0], (2, vec![s("1"), s("Smith; J"), s("1.50"), s("2024-01-02")]));
        assert_eq!(good[1].1, vec![s("2"), None, s(""), s("2024-01-03")]);
        assert_eq!(good[2].0, 4);
    }

    #[test]
    fn bad_rows_have_lines() {
        let (_d, p) = tmp("b.csv", b"a,b\n1,2\n3\n4,5,6\n7,8\n");
        let d = CsvDialect::default();
        let (good, bad) = all_rows(&p, ImportFormat::Csv, &d, &["a".into(), "b".into()]);
        assert_eq!(good.len(), 2);
        assert_eq!(bad, vec![(3, "1 fields, expected 2".to_string()), (4, "3 fields, expected 2".to_string())]);
        let (_d2, p2) = tmp("c.csv", b"n\nZ\xfcrich\n");
        let (good, bad) = all_rows(&p2, ImportFormat::Csv, &d, &["n".into()]);
        assert!(good.is_empty() && bad[0].1.contains("UTF-8"));
        let latin = CsvDialect { encoding: super::super::Encoding::Latin1, ..d };
        let (good, _) = all_rows(&p2, ImportFormat::Csv, &latin, &["n".into()]);
        assert_eq!(good[0].1, vec![s("Zürich")]);
        let pv = super::super::preview(&p2, None, None).unwrap();
        assert_eq!(pv.dialect.encoding, super::super::Encoding::Latin1);
    }

    #[test]
    fn headerless_files_and_names() {
        let (_d, p) = tmp("h.csv", b"1,2.5,x\n2,3,y\n");
        let pv = super::super::preview(&p, None, None).unwrap();
        assert!(!pv.dialect.header);
        assert_eq!(pv.columns[0].name, "column_1");
        assert_eq!(pv.rows.len(), 2);
        assert_eq!(column_names(&[s("a"), None, s("a"), s(" ")]), ["a", "column_2", "a_2", "column_4"]);
    }

    #[test]
    fn tsv_escapes() {
        let (_d, p) = tmp("t.tsv", b"id\tnote\n1\tline\\none\\ttab\n2\t\\N\n");
        let pv = super::super::preview(&p, None, None).unwrap();
        assert_eq!(pv.format, ImportFormat::Tsv);
        assert_eq!(pv.rows[0][1], s("line\none\ttab"));
        assert_eq!(pv.rows[1][1], None);
    }

    #[test]
    fn json_and_ndjson() {
        let nd = "{\"id\": 1, \"name\": \"a\", \"tags\": [1, 2]}\n\n{\"id\": 2, \"extra\": {\"k\": true}, \"name\": null}\nnot json\n[1]\n";
        let (_d, p) = tmp("x.ndjson", nd.as_bytes());
        let pv = super::super::preview(&p, None, None).unwrap();
        assert_eq!(pv.format, ImportFormat::Ndjson);
        assert_eq!(pv.columns.iter().map(|c| (c.name.as_str(), c.ty)).collect::<Vec<_>>(), [
            ("id", PgType::Int8),
            ("name", PgType::Text),
            ("tags", PgType::Jsonb),
            ("extra", PgType::Jsonb)
        ]);
        let names: Vec<String> = pv.columns.iter().map(|c| c.name.clone()).collect();
        let (good, bad) = all_rows(&p, pv.format, &pv.dialect, &names);
        assert_eq!(good[0], (1, vec![s("1"), s("a"), s("[1,2]"), None]));
        assert_eq!(good[1], (3, vec![s("2"), None, None, s("{\"k\":true}")]));
        assert_eq!(bad.iter().map(|b| b.0).collect::<Vec<_>>(), vec![4, 5]);
        let (_d2, p2) = tmp("y.json", b"[{\"a\": 1.5, \"b\": \"2024-01-01T00:00:00Z\"}, 7, {\"a\": 2}]");
        let pv = super::super::preview(&p2, None, None).unwrap();
        assert_eq!(pv.format, ImportFormat::Json);
        assert_eq!(pv.columns.iter().map(|c| c.ty).collect::<Vec<_>>(), [PgType::Numeric, PgType::TimestampTz]);
        let (good, bad) = all_rows(&p2, pv.format, &pv.dialect, &["a".into(), "b".into()]);
        assert_eq!(good.len(), 2);
        assert_eq!(good[1], (3, vec![s("2"), None]));
        assert_eq!(bad, vec![(2, "not a JSON object".to_string())]);
        let (_d3, p3) = tmp("z.json", b"{\"a\": 1}\n{\"a\": 2}\n");
        assert_eq!(super::super::preview(&p3, None, None).unwrap().format, ImportFormat::Ndjson);
    }

    #[test]
    fn parquet_types_and_values() {
        use arrow_array::builder::{Int32Builder, ListBuilder, StringBuilder, TimestampMicrosecondBuilder};
        use arrow_array::{ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array, Float64Array, RecordBatch};
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.parquet");
        let mut ts = TimestampMicrosecondBuilder::new().with_timezone("UTC");
        ts.append_value(946_684_801_500_000);
        ts.append_null();
        let mut tags = ListBuilder::new(Int32Builder::new());
        tags.values().append_value(1);
        tags.values().append_value(2);
        tags.append(true);
        tags.append(false);
        let mut name = StringBuilder::new();
        name.append_value("42");
        name.append_value("43");
        let cols: Vec<(&str, ArrayRef)> = vec![
            ("ok", Arc::new(BooleanArray::from(vec![Some(true), None]))),
            ("price", Arc::new(Decimal128Array::from(vec![1250, -5]).with_precision_and_scale(10, 2).unwrap())),
            ("x", Arc::new(Float64Array::from(vec![0.5, f64::NAN]))),
            ("day", Arc::new(Date32Array::from(vec![19_723, 0]))),
            ("at", Arc::new(ts.finish())),
            ("raw", Arc::new(BinaryArray::from(vec![&b"\xde\xad"[..], &b""[..]]))),
            ("tags", Arc::new(tags.finish())),
            ("code", Arc::new(name.finish())),
        ];
        let batch = RecordBatch::try_from_iter(cols).unwrap();
        let mut w = parquet::arrow::ArrowWriter::try_new(std::fs::File::create(&p).unwrap(), batch.schema(), None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        let pv = super::super::preview(&p, None, None).unwrap();
        assert_eq!(pv.format, ImportFormat::Parquet);
        assert_eq!(pv.total_rows, Some(2));
        assert_eq!(pv.columns.iter().map(|c| c.ty).collect::<Vec<_>>(), [
            PgType::Bool,
            PgType::Numeric,
            PgType::Float8,
            PgType::Date,
            PgType::TimestampTz,
            PgType::Bytea,
            PgType::Jsonb,
            PgType::Int8
        ]);
        assert_eq!(pv.rows[0], vec![s("true"), s("12.50"), s("0.5"), s("2024-01-01"), s("2000-01-01 00:00:01.500+00"), s("\\xdead"), s("[1,2]"), s("42")]);
        assert_eq!(pv.rows[1], vec![None, s("-0.05"), s("NaN"), s("1970-01-01"), None, s("\\x"), None, s("43")]);
    }
}
