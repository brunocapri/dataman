//! A streaming CSV / TSV reader over bytes, and the dialect detection of a file's first bytes.
//!
//! The reader keeps the raw bytes of one record at a time (fields as ranges into them, with
//! whether each was quoted, so `""` and an empty field differ like in `COPY … CSV`) and the
//! physical line each record starts on, for error reports. Decoding (UTF-8, Latin-1,
//! Windows-1252) and the TSV escapes happen per field, in [`decode_field`].

use std::io::{self, BufRead};

use super::{CsvDialect, Encoding};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub start: usize,
    pub end: usize,
    pub quoted: bool,
}

/// One record: its bytes (quotes removed, doubled quotes collapsed) and its fields.
#[derive(Clone, Debug, Default)]
pub struct Record {
    pub buf: Vec<u8>,
    pub fields: Vec<Field>,
    /// The physical line (1-based) the record starts on.
    pub line: u64,
}

impl Record {
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<(&[u8], bool)> {
        self.fields.get(i).map(|f| (&self.buf[f.start..f.end], f.quoted))
    }

    /// A line with nothing on it (one empty, unquoted field).
    pub fn is_blank(&self) -> bool {
        self.fields.len() == 1 && self.fields[0].start == self.fields[0].end && !self.fields[0].quoted
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    Unquoted,
    Quoted,
    /// A quote inside a quoted field: the next byte says whether it was doubled.
    QuoteSeen,
}

pub struct Reader<R> {
    inner: R,
    delim: u8,
    quote: Option<u8>,
    /// The line the next record starts on.
    line: u64,
    /// A CR at the end of the previous record: skip one LF.
    skip_lf: bool,
}

impl<R: BufRead> Reader<R> {
    pub fn new(inner: R, delim: u8, quote: Option<u8>) -> Self {
        Reader { inner, delim, quote, line: 1, skip_lf: false }
    }

    /// The next record, `false` at the end of the input. CR, LF and CRLF all end a record
    /// (outside quotes); a quote that closes in the middle of a field keeps what follows.
    pub fn read(&mut self, rec: &mut Record) -> io::Result<bool> {
        rec.buf.clear();
        rec.fields.clear();
        let (delim, quote) = (self.delim, self.quote);
        let mut state = State::Start;
        let mut field_start = 0usize;
        let mut quoted = false;
        let mut any = false;
        if self.skip_lf {
            self.skip_lf = false;
            let buf = self.inner.fill_buf()?;
            if buf.first() == Some(&b'\n') {
                self.inner.consume(1);
            }
        }
        rec.line = self.line;
        loop {
            let buf = self.inner.fill_buf()?;
            if buf.is_empty() {
                if !any {
                    return Ok(false);
                }
                // The last line had no line break (an unclosed quote ends here too).
                rec.fields.push(Field { start: field_start, end: rec.buf.len(), quoted });
                return Ok(true);
            }
            any = true;
            let mut i = 0;
            let n = buf.len();
            while i < n {
                match state {
                    State::Start => {
                        if Some(buf[i]) == quote {
                            state = State::Quoted;
                            quoted = true;
                            i += 1;
                        } else {
                            state = State::Unquoted;
                        }
                    }
                    State::Unquoted => {
                        // Copy the run up to the next special byte at once.
                        let run = buf[i..].iter().position(|&c| c == delim || c == b'\n' || c == b'\r').unwrap_or(n - i);
                        rec.buf.extend_from_slice(&buf[i..i + run]);
                        i += run;
                        if i == n {
                            break;
                        }
                        let c = buf[i];
                        rec.fields.push(Field { start: field_start, end: rec.buf.len(), quoted });
                        field_start = rec.buf.len();
                        quoted = false;
                        i += 1;
                        if c == delim {
                            state = State::Start;
                        } else if c == b'\n' {
                            self.line += 1;
                            self.inner.consume(i);
                            return Ok(true);
                        } else {
                            // CR: an LF may follow, possibly in the next buffer.
                            self.line += 1;
                            if i < n {
                                if buf[i] == b'\n' {
                                    i += 1;
                                }
                            } else {
                                self.skip_lf = true;
                            }
                            self.inner.consume(i);
                            return Ok(true);
                        }
                    }
                    State::Quoted => {
                        let q = quote.unwrap_or(0);
                        let run = buf[i..].iter().position(|&c| c == q).unwrap_or(n - i);
                        let part = &buf[i..i + run];
                        self.line += part.iter().filter(|&&c| c == b'\n').count() as u64;
                        rec.buf.extend_from_slice(part);
                        i += run;
                        if i < n {
                            state = State::QuoteSeen;
                            i += 1;
                        }
                    }
                    State::QuoteSeen => {
                        let c = buf[i];
                        if Some(c) == quote {
                            rec.buf.push(c);
                            state = State::Quoted;
                            i += 1;
                        } else if c == delim || c == b'\n' || c == b'\r' {
                            state = State::Unquoted;
                        } else {
                            // Text after a closing quote: kept, like most readers do.
                            rec.buf.push(c);
                            state = State::Unquoted;
                            i += 1;
                        }
                    }
                }
            }
            self.inner.consume(n);
        }
    }
}

/// Decodes one field into `out` (cleared first). `Ok(false)`: the field is NULL. `Err`: the
/// bytes are not valid in the encoding.
pub fn decode_field(raw: &[u8], quoted: bool, d: &CsvDialect, out: &mut String) -> Result<bool, ()> {
    out.clear();
    if !quoted && raw == d.null_text.as_bytes() {
        return Ok(false);
    }
    if d.escapes {
        if raw.contains(&b'\\') {
            let mut tmp = Vec::with_capacity(raw.len());
            let mut it = raw.iter().copied();
            while let Some(c) = it.next() {
                if c != b'\\' {
                    tmp.push(c);
                    continue;
                }
                match it.next() {
                    Some(b'n') => tmp.push(b'\n'),
                    Some(b't') => tmp.push(b'\t'),
                    Some(b'r') => tmp.push(b'\r'),
                    Some(b'b') => tmp.push(8),
                    Some(b'f') => tmp.push(12),
                    Some(b'v') => tmp.push(11),
                    Some(o) => tmp.push(o),
                    None => tmp.push(b'\\'),
                }
            }
            decode(&tmp, d.encoding, out)?;
            return Ok(true);
        }
    }
    decode(raw, d.encoding, out)?;
    Ok(true)
}

/// Windows-1252's 0x80–0x9F (the rest of its upper half is Latin-1).
const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—',
    '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

pub fn decode(raw: &[u8], enc: Encoding, out: &mut String) -> Result<(), ()> {
    match enc {
        Encoding::Utf8 => out.push_str(std::str::from_utf8(raw).map_err(|_| ())?),
        Encoding::Latin1 => out.extend(raw.iter().map(|&b| b as char)),
        Encoding::Windows1252 => out.extend(raw.iter().map(|&b| if (0x80..0xa0).contains(&b) { CP1252[(b - 0x80) as usize] } else { b as char })),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Detection

/// The encoding a sample suggests: UTF-8 when it decodes (a character cut at the sample's end
/// does not count), else Windows-1252 when it has bytes in 0x80–0x9F (unused in Latin-1), else
/// Latin-1.
pub fn sniff_encoding(sample: &[u8], complete: bool) -> Encoding {
    match std::str::from_utf8(sample) {
        Ok(_) => Encoding::Utf8,
        Err(e) if !complete && e.error_len().is_none() => Encoding::Utf8,
        Err(_) if sample.iter().any(|b| (0x80..0xa0).contains(b)) => Encoding::Windows1252,
        Err(_) => Encoding::Latin1,
    }
}

/// Records of a sample with a dialect (at most `limit`).
pub fn records(sample: &[u8], delim: u8, quote: Option<u8>, limit: usize) -> Vec<Record> {
    let mut r = Reader::new(sample, delim, quote);
    let mut out = Vec::new();
    let mut rec = Record::default();
    while out.len() < limit {
        match r.read(&mut rec) {
            Ok(true) => {
                if !rec.is_blank() {
                    out.push(rec.clone());
                }
            }
            _ => break,
        }
    }
    out
}

/// The delimiter that splits the sample's lines most consistently into more than one field:
/// for each candidate, the share of records with the most common field count, then that
/// count. One column (no candidate splits anything) keeps the comma.
pub fn sniff_delimiter(sample: &[u8], quote: Option<u8>) -> u8 {
    let mut best = (b',', 0.0f64, 0usize);
    for d in [b',', b';', b'\t', b'|'] {
        let recs = records(sample, d, quote, 200);
        if recs.is_empty() {
            continue;
        }
        let mut counts: std::collections::HashMap<usize, usize> = Default::default();
        for r in &recs {
            *counts.entry(r.len()).or_default() += 1;
        }
        let (&mode, &hits) = counts.iter().max_by_key(|(n, h)| (**h, **n)).unwrap();
        if mode < 2 {
            continue;
        }
        let share = hits as f64 / recs.len() as f64;
        if share > best.1 + 1e-9 || (share > best.1 - 1e-9 && mode > best.2) {
            best = (d, share, mode);
        }
    }
    best.0
}

/// The quote character: `"` unless the sample has none and quotes fields with `'`.
pub fn sniff_quote(sample: &[u8], delim: u8) -> Option<u8> {
    let starts = |q: u8| {
        sample.windows(2).filter(|w| (w[0] == delim || w[0] == b'\n') && w[1] == q).count() + usize::from(sample.first() == Some(&q))
    };
    if starts(b'"') == 0 && starts(b'\'') > 0 { Some(b'\'') } else { Some(b'"') }
}

/// Whether the first record is a header: in each column whose other values share a type, a
/// first value that parses as nothing votes yes (one that parses votes no); with only text columns, a
/// first row of distinct, non-empty names that do not reappear below is a header.
pub fn sniff_header(recs: &[Vec<Option<String>>]) -> bool {
    use super::infer::{Infer, PgType};
    let Some((first, rest)) = recs.split_first() else { return true };
    if rest.is_empty() {
        return true;
    }
    let mut votes = 0i32;
    for (c, head) in first.iter().enumerate() {
        let mut inf = Infer::default();
        for r in rest {
            if let Some(Some(v)) = r.get(c) {
                inf.observe(v);
            }
        }
        let ty = inf.result();
        if ty == PgType::Text {
            continue;
        }
        let mut one = Infer::default();
        match head {
            Some(h) if !h.is_empty() => one.observe(h),
            _ => continue,
        }
        // A name parses as nothing; a value (even of a wider type: 2.5 over integers) does.
        if one.result() == PgType::Text { votes += 1 } else { votes -= 1 }
    }
    if votes != 0 {
        return votes > 0;
    }
    let names: Vec<&str> = first.iter().map(|v| v.as_deref().unwrap_or("")).collect();
    let distinct = names.iter().collect::<std::collections::HashSet<_>>().len() == names.len();
    let reappear = rest.iter().any(|r| r.iter().zip(&names).any(|(v, n)| v.as_deref() == Some(*n)));
    names.iter().all(|n| !n.trim().is_empty()) && distinct && !reappear
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &[u8], delim: u8, quote: Option<u8>) -> Vec<(u64, Vec<(String, bool)>)> {
        // A tiny buffer exercises every split across refills.
        let mut r = Reader::new(io::BufReader::with_capacity(3, input), delim, quote);
        let mut rec = Record::default();
        let mut out = Vec::new();
        while r.read(&mut rec).unwrap() {
            let fields = (0..rec.len()).map(|i| {
                let (b, q) = rec.get(i).unwrap();
                (String::from_utf8(b.to_vec()).unwrap(), q)
            });
            out.push((rec.line, fields.collect()));
        }
        out
    }

    fn texts(v: &[(u64, Vec<(String, bool)>)]) -> Vec<Vec<&str>> {
        v.iter().map(|(_, f)| f.iter().map(|(s, _)| s.as_str()).collect()).collect()
    }

    #[test]
    fn quotes_line_breaks_and_lines() {
        let input = b"id,name,note\r\n1,\"a,b\",\"say \"\"hi\"\"\"\n2,\"two\nlines\",\r3,,\"\"\n4,x";
        let recs = parse(input, b',', Some(b'"'));
        assert_eq!(texts(&recs), vec![vec!["id", "name", "note"], vec!["1", "a,b", "say \"hi\""], vec!["2", "two\nlines", ""], vec!["3", "", ""], vec!["4", "x"]]);
        // The physical line each record starts on (the quoted line break counts).
        assert_eq!(recs.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1, 2, 3, 5, 6]);
        // Quoted empty vs empty.
        assert_eq!(recs[3].1[1], ("".into(), false));
        assert_eq!(recs[3].1[2], ("".into(), true));
    }

    #[test]
    fn trailing_newline_and_blank_lines() {
        let recs = parse(b"a;b\n\n1;2\n", b';', Some(b'"'));
        assert_eq!(texts(&recs), vec![vec!["a", "b"], vec![""], vec!["1", "2"]]);
        assert_eq!(parse(b"", b',', None).len(), 0);
        assert_eq!(texts(&parse(b"x,\n", b',', None)), vec![vec!["x", ""]]);
        assert_eq!(texts(&parse(b"x,", b',', None)), vec![vec!["x", ""]]);
        // CRLF split across buffer refills.
        assert_eq!(texts(&parse(b"ab\r\ncd\r\n", b',', None)), vec![vec!["ab"], vec!["cd"]]);
    }

    #[test]
    fn no_quoting_and_escapes() {
        let d = CsvDialect::tsv();
        let recs = parse(b"a\\tb\t\\N\tc\\\\d\\ne\n", b'\t', None);
        let mut s = String::new();
        let (raw, q) = (recs[0].1[0].0.as_bytes(), recs[0].1[0].1);
        assert!(decode_field(raw, q, &d, &mut s).unwrap());
        assert_eq!(s, "a\tb");
        assert!(!decode_field(recs[0].1[1].0.as_bytes(), false, &d, &mut s).unwrap(), "\\N is NULL");
        assert!(decode_field(recs[0].1[2].0.as_bytes(), false, &d, &mut s).unwrap());
        assert_eq!(s, "c\\d\ne");
        // A quote is an ordinary byte without quoting.
        assert_eq!(texts(&parse(b"\"a\"\tb\n", b'\t', None)), vec![vec!["\"a\"", "b"]]);
    }

    #[test]
    fn encodings() {
        let mut s = String::new();
        decode(&[0x6e, 0x61, 0xef, 0x76, 0x65], Encoding::Latin1, &mut s).unwrap();
        assert_eq!(s, "naïve");
        s.clear();
        decode(&[0x80, 0x93, 0x94], Encoding::Windows1252, &mut s).unwrap();
        assert_eq!(s, "€“”");
        s.clear();
        assert!(decode(&[0xe7], Encoding::Utf8, &mut s).is_err());
        assert_eq!(sniff_encoding("Zürich".as_bytes(), true), Encoding::Utf8);
        // A UTF-8 character cut at the sample's end is still UTF-8.
        assert_eq!(sniff_encoding(&"Zürich".as_bytes()[..2], false), Encoding::Utf8);
        assert_eq!(sniff_encoding(&[b'a', 0xe7, b'b'], true), Encoding::Latin1);
        assert_eq!(sniff_encoding(&[b'a', 0x93, b'b', 0x94], true), Encoding::Windows1252);
    }

    #[test]
    fn delimiters_quotes_and_headers() {
        assert_eq!(sniff_delimiter(b"a,b,c\n1,2,3\n4,5,6\n", Some(b'"')), b',');
        assert_eq!(sniff_delimiter(b"name;amount\n\"Smith, J\";1,50\nX;2,75\n", Some(b'"')), b';');
        assert_eq!(sniff_delimiter(b"a\tb\n1\t2\n", Some(b'"')), b'\t');
        assert_eq!(sniff_delimiter(b"a|b|c\n1|2|3\n", Some(b'"')), b'|');
        assert_eq!(sniff_delimiter(b"just one column\nsecond\n", Some(b'"')), b',');
        assert_eq!(sniff_quote(b"a,'b,c'\n", b','), Some(b'\''));
        assert_eq!(sniff_quote(b"a,\"b\"\n", b','), Some(b'"'));
        let rows = |v: &[&[&str]]| v.iter().map(|r| r.iter().map(|s| Some(s.to_string())).collect()).collect::<Vec<_>>();
        assert!(sniff_header(&rows(&[&["id", "price"], &["1", "2.5"], &["2", "3"]])));
        assert!(!sniff_header(&rows(&[&["7", "2.5"], &["1", "2.5"], &["2", "3"]])));
        // Text only: distinct names that do not reappear.
        assert!(sniff_header(&rows(&[&["name", "city"], &["Ann", "Oslo"], &["Bea", "NYC"]])));
        assert!(!sniff_header(&rows(&[&["Ann", "Oslo"], &["Ann", "NYC"], &["Bea", "Oslo"]])));
    }
}
