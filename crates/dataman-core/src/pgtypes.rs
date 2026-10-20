use std::fmt::Write;

use tokio_postgres::types::{Kind as PgKind, Type};

use crate::value::{format_binary, format_float};

type BoxErr = Box<dyn std::error::Error + Sync + Send>;

struct Cur<'a>(&'a [u8]);

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BoxErr> {
        if self.0.len() < n {
            return Err("short value".into());
        }
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        Ok(h)
    }
    fn u8(&mut self) -> Result<u8, BoxErr> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, BoxErr> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn i16(&mut self) -> Result<i16, BoxErr> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn i32(&mut self) -> Result<i32, BoxErr> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32, BoxErr> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64, BoxErr> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn f32(&mut self) -> Result<f32, BoxErr> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn f64(&mut self) -> Result<f64, BoxErr> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn cstr(&mut self) -> Result<&'a str, BoxErr> {
        let n = self.0.iter().position(|&b| b == 0).ok_or("unterminated string")?;
        let s = std::str::from_utf8(&self.0[..n])?;
        self.0 = &self.0[n + 1..];
        Ok(s)
    }
}

// By name: extension types have no fixed OID.
pub(crate) fn format_special(ty: &Type, raw: &[u8], out: &mut String) -> Option<Result<(), BoxErr>> {
    match ty.kind() {
        PgKind::Range(elem) => return Some(range(elem, raw, out)),
        PgKind::Multirange(elem) => return Some(multirange(elem, raw, out)),
        PgKind::Simple => {}
        _ => return None,
    }
    Some(match ty.name() {
        "bit" | "varbit" => bits(raw, out),
        "point" => (|| {
            let mut c = Cur(raw);
            point(&mut c, out)
        })(),
        "line" => (|| {
            let mut c = Cur(raw);
            out.push('{');
            for i in 0..3 {
                if i > 0 {
                    out.push(',');
                }
                float8(c.f64()?, out);
            }
            out.push('}');
            Ok(())
        })(),
        "lseg" => (|| {
            let mut c = Cur(raw);
            out.push('[');
            point(&mut c, out)?;
            out.push(',');
            point(&mut c, out)?;
            out.push(']');
            Ok(())
        })(),
        "box" => (|| {
            let mut c = Cur(raw);
            point(&mut c, out)?;
            out.push(',');
            point(&mut c, out)
        })(),
        "path" => path(raw, out),
        "polygon" => polygon(raw, out),
        "circle" => (|| {
            let mut c = Cur(raw);
            out.push_str("<(");
            float8(c.f64()?, out);
            out.push(',');
            float8(c.f64()?, out);
            out.push_str("),");
            float8(c.f64()?, out);
            out.push('>');
            Ok(())
        })(),
        "macaddr8" if raw.len() == 8 => {
            let parts: Vec<String> = raw.iter().map(|b| format!("{b:02x}")).collect();
            out.push_str(&parts.join(":"));
            Ok(())
        }
        "tid" => (|| {
            let mut c = Cur(raw);
            let (block, off) = (c.u32()?, c.u16()?);
            let _ = write!(out, "({block},{off})");
            Ok(())
        })(),
        "xid8" => (|| {
            let _ = write!(out, "{}", Cur(raw).u64()?);
            Ok(())
        })(),
        "pg_snapshot" | "txid_snapshot" => snapshot(raw, out),
        "tsvector" => tsvector(raw, out),
        "tsquery" => tsquery(raw, out),
        "jsonpath" => (|| {
            let mut c = Cur(raw);
            if c.u8()? != 1 {
                return Err("jsonpath version".into());
            }
            out.push_str(std::str::from_utf8(c.0)?);
            Ok(())
        })(),
        "regtype" => (|| {
            let oid = Cur(raw).u32()?;
            match Type::from_oid(oid) {
                Some(t) => out.push_str(builtin_type_name(&t)),
                None => {
                    let _ = write!(out, "{oid}");
                }
            }
            Ok(())
        })(),
        "regclass" | "regproc" | "regprocedure" | "regoper" | "regoperator" | "regnamespace" | "regrole" | "regconfig" | "regdictionary"
        | "regcollation" => (|| {
            let _ = write!(out, "{}", Cur(raw).u32()?);
            Ok(())
        })(),
        "ltree" | "lquery" | "ltxtquery" => (|| {
            let mut c = Cur(raw);
            if c.u8()? != 1 {
                return Err("ltree version".into());
            }
            out.push_str(std::str::from_utf8(c.0)?);
            Ok(())
        })(),
        "hstore" => hstore(raw, out),
        "vector" => vector(raw, out),
        "halfvec" => halfvec(raw, out),
        "sparsevec" => sparsevec(raw, out),
        _ => return None,
    })
}

fn builtin_type_name(t: &Type) -> &str {
    match t.name() {
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "bool" => "boolean",
        "bpchar" => "character",
        "varchar" => "character varying",
        "timestamp" => "timestamp without time zone",
        "timestamptz" => "timestamp with time zone",
        "time" => "time without time zone",
        "timetz" => "time with time zone",
        "varbit" => "bit varying",
        "_int2" => "smallint[]",
        "_int4" => "integer[]",
        "_int8" => "bigint[]",
        "_text" => "text[]",
        n => n,
    }
}

fn float8(v: f64, out: &mut String) {
    format_float(v, false, out);
}

fn point(c: &mut Cur, out: &mut String) -> Result<(), BoxErr> {
    out.push('(');
    float8(c.f64()?, out);
    out.push(',');
    float8(c.f64()?, out);
    out.push(')');
    Ok(())
}

fn points(c: &mut Cur, out: &mut String) -> Result<(), BoxErr> {
    let n = c.i32()?;
    for i in 0..n.max(0) {
        if i > 0 {
            out.push(',');
        }
        point(c, out)?;
    }
    Ok(())
}

fn path(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let closed = c.u8()? != 0;
    out.push(if closed { '(' } else { '[' });
    points(&mut c, out)?;
    out.push(if closed { ')' } else { ']' });
    Ok(())
}

fn polygon(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    out.push('(');
    points(&mut c, out)?;
    out.push(')');
    Ok(())
}

fn bits(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let n = c.i32()?.max(0) as usize;
    let data = c.take(n.div_ceil(8))?;
    for i in 0..n {
        out.push(if data[i / 8] & (0x80 >> (i % 8)) != 0 { '1' } else { '0' });
    }
    Ok(())
}

fn snapshot(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let n = c.i32()?;
    let (xmin, xmax) = (c.u64()?, c.u64()?);
    let _ = write!(out, "{xmin}:{xmax}:");
    for i in 0..n.max(0) {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", c.u64()?);
    }
    Ok(())
}

fn range_bound(elem: &Type, raw: &[u8], out: &mut String) {
    let mut s = String::new();
    format_binary(elem, raw, &mut s);
    if s.is_empty() || s.chars().any(|c| matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_whitespace()) {
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push(c);
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(&s);
    }
}

fn range_in(elem: &Type, c: &mut Cur, out: &mut String) -> Result<(), BoxErr> {
    const EMPTY: u8 = 0x01;
    const LB_INC: u8 = 0x02;
    const UB_INC: u8 = 0x04;
    const LB_INF: u8 = 0x08;
    const UB_INF: u8 = 0x10;
    let flags = c.u8()?;
    if flags & EMPTY != 0 {
        out.push_str("empty");
        return Ok(());
    }
    out.push(if flags & LB_INC != 0 { '[' } else { '(' });
    if flags & LB_INF == 0 {
        let n = c.i32()?;
        range_bound(elem, c.take(n.max(0) as usize)?, out);
    }
    out.push(',');
    if flags & UB_INF == 0 {
        let n = c.i32()?;
        range_bound(elem, c.take(n.max(0) as usize)?, out);
    }
    out.push(if flags & UB_INC != 0 { ']' } else { ')' });
    Ok(())
}

fn range(elem: &Type, raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    range_in(elem, &mut Cur(raw), out)
}

fn multirange(elem: &Type, raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let n = c.i32()?;
    out.push('{');
    for i in 0..n.max(0) {
        if i > 0 {
            out.push(',');
        }
        let len = c.i32()?;
        range_in(elem, &mut Cur(c.take(len.max(0) as usize)?), out)?;
    }
    out.push('}');
    Ok(())
}

fn quoted_lexeme(s: &str, out: &mut String) {
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
}

fn tsvector(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let n = c.i32()?;
    for i in 0..n.max(0) {
        if i > 0 {
            out.push(' ');
        }
        quoted_lexeme(c.cstr()?, out);
        let np = c.u16()?;
        for j in 0..np {
            out.push(if j == 0 { ':' } else { ',' });
            let p = c.u16()?;
            let _ = write!(out, "{}", p & 0x3fff);
            match p >> 14 {
                3 => out.push('A'),
                2 => out.push('B'),
                1 => out.push('C'),
                _ => {}
            }
        }
    }
    Ok(())
}

enum Q {
    Val { text: String, weight: u8, prefix: bool },
    Not(Box<Q>),
    Op { op: u8, distance: i16, left: Box<Q>, right: Box<Q> },
}

/// Reads one expression of the prefix-ordered item list (an operator is followed by its right
/// operand, then its left one).
fn read_q(c: &mut Cur, depth: usize) -> Result<Q, BoxErr> {
    if depth > 4000 {
        return Err("tsquery too deep".into());
    }
    match c.u8()? {
        1 => {
            let weight = c.u8()?;
            let prefix = c.u8()? != 0;
            Ok(Q::Val { text: c.cstr()?.to_string(), weight, prefix })
        }
        2 => match c.u8()? {
            1 => Ok(Q::Not(Box::new(read_q(c, depth + 1)?))),
            op @ (2..=4) => {
                let distance = if op == 4 { c.i16()? } else { 0 };
                let right = Box::new(read_q(c, depth + 1)?);
                let left = Box::new(read_q(c, depth + 1)?);
                Ok(Q::Op { op, distance, left, right })
            }
            _ => Err("tsquery operator".into()),
        },
        _ => Err("tsquery item".into()),
    }
}

fn q_out(q: &Q, parent: u8, right_of_phrase: bool, out: &mut String) {
    fn priority(op: u8) -> u8 {
        match op {
            3 => 1,
            2 => 2,
            4 => 3,
            _ => 4,
        }
    }
    match q {
        Q::Val { text, weight, prefix } => {
            quoted_lexeme(text, out);
            if *weight != 0 || *prefix {
                out.push(':');
                if *prefix {
                    out.push('*');
                }
                for (bit, l) in [(8, 'A'), (4, 'B'), (2, 'C'), (1, 'D')] {
                    if weight & bit != 0 {
                        out.push(l);
                    }
                }
            }
        }
        Q::Not(inner) => {
            let p = priority(1);
            let paren = p < parent;
            if paren {
                out.push_str("( ");
            }
            out.push('!');
            q_out(inner, p, false, out);
            if paren {
                out.push_str(" )");
            }
        }
        Q::Op { op, distance, left, right } => {
            let p = priority(*op);
            let paren = p < parent || (*op == 4 && right_of_phrase);
            if paren {
                out.push_str("( ");
            }
            q_out(left, p, false, out);
            match op {
                2 => out.push_str(" & "),
                3 => out.push_str(" | "),
                _ if *distance == 1 => out.push_str(" <-> "),
                _ => {
                    let _ = write!(out, " <{distance}> ");
                }
            }
            q_out(right, p, *op == 4, out);
            if paren {
                out.push_str(" )");
            }
        }
    }
}

fn tsquery(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    if c.i32()? == 0 {
        return Ok(());
    }
    let q = read_q(&mut c, 0)?;
    q_out(&q, 0, false, out);
    Ok(())
}

fn hstore_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

fn hstore(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let n = c.i32()?;
    for i in 0..n.max(0) {
        if i > 0 {
            out.push_str(", ");
        }
        let kl = c.i32()?;
        hstore_string(std::str::from_utf8(c.take(kl.max(0) as usize)?)?, out);
        out.push_str("=>");
        let vl = c.i32()?;
        if vl < 0 {
            out.push_str("NULL");
        } else {
            hstore_string(std::str::from_utf8(c.take(vl as usize)?)?, out);
        }
    }
    Ok(())
}

fn float4(v: f32, out: &mut String) {
    format_float(v as f64, true, out);
}

fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let man = (h & 0x3ff) as f32;
    match exp {
        0 => sign * man * 2f32.powi(-24),
        31 if man == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + man / 1024.0) * 2f32.powi(exp - 15),
    }
}

fn vector(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let dim = c.i16()?;
    c.i16()?;
    out.push('[');
    for i in 0..dim.max(0) {
        if i > 0 {
            out.push(',');
        }
        float4(c.f32()?, out);
    }
    out.push(']');
    Ok(())
}

fn halfvec(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let dim = c.i16()?;
    c.i16()?;
    out.push('[');
    for i in 0..dim.max(0) {
        if i > 0 {
            out.push(',');
        }
        float4(half_to_f32(c.u16()?), out);
    }
    out.push(']');
    Ok(())
}

fn sparsevec(raw: &[u8], out: &mut String) -> Result<(), BoxErr> {
    let mut c = Cur(raw);
    let dim = c.i32()?;
    let nnz = c.i32()?.max(0) as usize;
    c.i32()?;
    let mut idx = Vec::with_capacity(nnz.min(1 << 16));
    for _ in 0..nnz {
        idx.push(c.i32()?);
    }
    out.push('{');
    for (i, ix) in idx.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}:", ix + 1);
        float4(c.f32()?, out);
    }
    let _ = write!(out, "}}/{dim}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_postgres::types::Kind;

    fn show(ty: &Type, raw: &[u8]) -> String {
        let mut s = String::new();
        format_special(ty, raw, &mut s).expect("a special type").expect("decodes");
        s
    }

    fn ext(name: &str) -> Type {
        Type::new(name.into(), 99_999, Kind::Simple, "public".into())
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn ranges() {
        assert_eq!(show(&Type::INT4_RANGE, &hex("0200000004000000010000000400000005")), "[1,5)");
        assert_eq!(show(&Type::INT4_RANGE, &hex("01")), "empty");
        assert_eq!(show(&Type::NUM_RANGE, &hex("18")), "(,)");
        assert_eq!(show(&Type::TS_RANGE, &hex("02000000080002b0d5d4e94000000000087fffffffffffffff")), "[\"2024-01-01 00:00:00\",infinity)");
        assert_eq!(show(&Type::INT4MULTI_RANGE, &hex("00000002000000110200000004000000010000000400000003000000110200000004000000050000000400000009")), "{[1,3),[5,9)}");
        assert_eq!(show(&Type::INT4MULTI_RANGE, &hex("00000000")), "{}");
    }

    #[test]
    fn bits_geometry_and_ids() {
        assert_eq!(show(&Type::VARBIT, &hex("00000005a8")), "10101");
        assert_eq!(show(&Type::VARBIT, &hex("00000000")), "");
        assert_eq!(show(&Type::POINT, &hex("3ff00000000000004000000000000000")), "(1,2)");
        assert_eq!(show(&Type::BOX, &hex("3ff00000000000003ff000000000000000000000000000000000000000000000")), "(1,1),(0,0)");
        assert_eq!(show(&Type::PATH, &hex("0100000003000000000000000000000000000000003ff00000000000003ff000000000000040000000000000000000000000000000")), "((0,0),(1,1),(2,0))");
        assert_eq!(show(&Type::CIRCLE, &hex("3ff000000000000040000000000000004008000000000000")), "<(1,2),3>");
        assert_eq!(show(&Type::MACADDR8, &hex("08002b0102030405")), "08:00:2b:01:02:03:04:05");
        assert_eq!(show(&Type::TID, &hex("000000010002")), "(1,2)");
        assert_eq!(show(&Type::JSONPATH, &hex("01242e61")), "$.a");
        assert_eq!(show(&Type::REGTYPE, &hex("00000017")), "integer");
    }

    #[test]
    fn text_search() {
        assert_eq!(show(&Type::TS_VECTOR, &hex("00000001616263000001c001")), "'abc':1A");
        assert_eq!(show(&Type::TS_VECTOR, &hex("0000000261000002c001800262000000")), "'a':1A,2B 'b'");
        assert_eq!(show(&Type::TSQUERY, &hex("000000030202010000666f7800010000717569636b00")), "'quick' & 'fox'");
        assert_eq!(
            show(&Type::TSQUERY, &hex("0000000602020201010000646f670002040001010000666f7800010000717569636b00")),
            "'quick' <-> 'fox' & !'dog'"
        );
        assert_eq!(show(&Type::TSQUERY, &hex("000000060203020400010100006300010000620002010100006100")), "!'a' | 'b' <-> 'c'");
    }

    #[test]
    fn extension_types() {
        assert_eq!(show(&ext("vector"), &hex("000300003f8000004000000040400000")), "[1,2,3]");
        assert_eq!(show(&ext("vector"), &hex("000300003fc00000c000000037fba882")), "[1.5,-2,3e-05]");
        assert_eq!(show(&ext("halfvec"), &hex("000300003c0040004200")), "[1,2,3]");
        assert_eq!(show(&ext("sparsevec"), &hex("00000005000000020000000000000000000000023fc0000040000000")), "{1:1.5,3:2}/5");
        assert_eq!(show(&ext("hstore"), &hex("00000003000000016100000001310000000162ffffffff0000000363206400000003782079")), "\"a\"=>\"1\", \"b\"=>NULL, \"c d\"=>\"x y\"");
        assert_eq!(show(&ext("ltree"), &hex("01612e622e63")), "a.b.c");
        let mut s = String::new();
        assert!(format_special(&ext("vector"), &hex("0003"), &mut s).unwrap().is_err());
        assert!(format_special(&ext("unknown_ext"), &[1], &mut s).is_none());
    }

    #[test]
    fn half_floats() {
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0x7c00), f32::INFINITY);
        assert!(half_to_f32(0x7e00).is_nan());
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
    }
}
