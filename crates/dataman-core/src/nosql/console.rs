use std::ops::Range;

use super::shell::{self, ParseError};
use crate::guard::WriteClass;

pub const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    None,
    Json(serde_json::Value),
    Lines(Vec<serde_json::Value>),
}

impl Request {
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('?').next().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect()
    }

    pub fn index(&self) -> Option<&str> {
        self.segments().first().copied().filter(|s| !s.starts_with('_'))
    }

    pub fn endpoint(&self) -> Option<&str> {
        self.segments().into_iter().find(|s| s.starts_with('_'))
    }

    pub fn is_search(&self) -> bool {
        matches!(self.method.as_str(), "GET" | "POST") && self.endpoint() == Some("_search") && !self.path.contains("scroll")
    }

    fn reads(&self) -> bool {
        let segs = self.segments();
        let has = |s: &str| segs.contains(&s);
        let sql_delete = || match &self.body {
            Body::Json(b) => b["query"].as_str().is_some_and(|q| q.trim_start().to_ascii_uppercase().starts_with("DELETE")),
            _ => false,
        };
        (has("_sql") || has("_ppl")) && !sql_delete()
            || ["_query", "_eql", "_pit", "_async_search", "_knn_search", "_terms_enum", "_msearch", "_rollup_search", "_graph"].iter().any(|s| has(s))
            || has("_search") && (has("scroll") || has("template"))
    }

    pub fn class(&self) -> WriteClass {
        let destructive = |why: &str| WriteClass::Destructive { reasons: vec![why.to_string()] };
        let ep = self.endpoint().unwrap_or("");
        let segs = self.segments();
        if matches!(self.method.as_str(), "GET" | "POST") && self.reads() {
            return WriteClass::Read;
        }
        match self.method.as_str() {
            "GET" | "HEAD" => WriteClass::Read,
            "POST" if matches!(ep, "_search" | "_count" | "_msearch" | "_mget" | "_validate" | "_explain" | "_field_caps" | "_analyze" | "_termvectors" | "_mtermvectors" | "_rank_eval" | "_render") => {
                WriteClass::Read
            }
            "POST" | "PUT" if ep == "_doc" || ep == "_create" => WriteClass::SafeWrite,
            "POST" if ep == "_update" => WriteClass::SafeWrite,
            "DELETE" if ep == "_doc" => WriteClass::SafeWrite,
            "POST" if ep == "_delete_by_query" || ep == "_update_by_query" => match &self.body {
                Body::Json(b) if b.get("query").is_some_and(|q| q.get("match_all").is_none()) => WriteClass::SafeWrite,
                _ => destructive(if ep == "_delete_by_query" { "_delete_by_query without a query" } else { "_update_by_query without a query" }),
            },
            "POST" | "PUT" if ep == "_bulk" => match &self.body {
                Body::Lines(lines) if lines.iter().any(|l| l.get("delete").is_some()) => destructive("_bulk deletes"),
                _ => WriteClass::SafeWrite,
            },
            "DELETE" if segs.len() == 1 && !ep.starts_with('_') => destructive("deletes an index"),
            "DELETE" => destructive("deletes"),
            "PUT" if segs.len() == 1 => destructive("creates an index"),
            "POST" if matches!(ep, "_refresh" | "_flush") => WriteClass::SafeWrite,
            _ => destructive(&format!("{} {}", self.method, self.path)),
        }
    }
}

fn method_at(line: &str) -> Option<&'static str> {
    let word = line.split_whitespace().next()?;
    METHODS.into_iter().find(|m| m.eq_ignore_ascii_case(word) && line.starts_with(word))
}

pub fn split(text: &str) -> Vec<Range<usize>> {
    let mut starts = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if method_at(line).is_some() {
            starts.push(at);
        }
        at += line.len();
    }
    let mut out = Vec::new();
    for (i, s) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(text.len());
        let t = &text[*s..end];
        out.push(*s..*s + t.trim_end().len());
    }
    out
}

pub fn statement_at(text: &str, at: usize) -> Option<Range<usize>> {
    let all = split(text);
    all.iter().rev().find(|r| r.start <= at).or(all.first()).cloned()
}

pub fn parse(text: &str) -> Result<Request, ParseError> {
    let lead = text.len() - text.trim_start().len();
    let text_t = text.trim_start();
    let (first, rest) = match text_t.find('\n') {
        Some(i) => (&text_t[..i], &text_t[i + 1..]),
        None => (text_t, ""),
    };
    let Some(m) = method_at(first) else {
        return Err(ParseError { message: "a request starts with GET, POST, PUT, DELETE or HEAD".into(), at: lead });
    };
    let path = first[m.len()..].trim();
    if path.is_empty() {
        return Err(ParseError { message: "the method needs a path (GET /index/_search)".into(), at: lead + first.len() });
    }
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    let body_at = lead + first.len() + 1;
    let mut req = Request { method: m.to_string(), path, body: Body::None };
    let ndjson = matches!(req.endpoint(), Some("_bulk" | "_msearch" | "_msearch/template"));
    if rest.trim().is_empty() {
        return Ok(req);
    }
    let json = |v: &shell::Val, at: usize| shell::to_json(v).map_err(|m| ParseError { message: m, at });
    if ndjson {
        let mut lines = Vec::new();
        let mut off = 0;
        for line in rest.split_inclusive('\n') {
            if !line.trim().is_empty() {
                let v = shell::parse(line).map_err(|e| ParseError { message: e.message, at: body_at + off + e.at })?;
                lines.push(json(&v, body_at + off)?);
            }
            off += line.len();
        }
        req.body = Body::Lines(lines);
    } else {
        let v = shell::parse(rest).map_err(|e| ParseError { message: e.message, at: body_at + e.at })?;
        req.body = Body::Json(json(&v, body_at)?);
    }
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_split_at_method_lines() {
        let t = "# notes\nGET /a/_search\n{\n  query: {match_all: {}},\n}\n\nPOST _bulk\n{\"index\": {\"_index\": \"a\"}}\n{\"x\": 1}\n\nGET _cat/indices";
        let parts: Vec<&str> = split(t).into_iter().map(|r| &t[r]).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].starts_with("GET /a/_search") && parts[0].ends_with('}'));
        assert_eq!(parts[2], "GET _cat/indices");
        assert_eq!(statement_at(t, t.find("match_all").unwrap()).map(|r| &t[r]), Some(parts[0]));
        let r = parse(parts[0]).unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str(), r.index(), r.endpoint(), r.is_search()), ("GET", "/a/_search", Some("a"), Some("_search"), true));
        assert_eq!(r.body, Body::Json(serde_json::json!({"query": {"match_all": {}}})));
        let b = parse(parts[1]).unwrap();
        assert!(matches!(&b.body, Body::Lines(l) if l.len() == 2));
        assert_eq!(parse(parts[2]).unwrap().path, "/_cat/indices");
        let e = parse("GET /a/_search\n{query: }").unwrap_err();
        assert_eq!(e.at, 23, "{e:?}");
        assert!(parse("SELECT 1").is_err());
    }

    #[test]
    fn what_each_request_does() {
        let c = |s: &str| parse(s).unwrap().class();
        assert_eq!(c("GET /a/_search"), WriteClass::Read);
        assert_eq!(c("POST /a/_search\n{}"), WriteClass::Read);
        assert_eq!(c("POST /a/_count\n{}"), WriteClass::Read);
        assert_eq!(c("PUT /a/_doc/1\n{\"x\": 1}"), WriteClass::SafeWrite);
        assert_eq!(c("DELETE /a/_doc/1"), WriteClass::SafeWrite);
        assert_eq!(c("POST /a/_delete_by_query\n{\"query\": {\"term\": {\"x\": 1}}}"), WriteClass::SafeWrite);
        assert!(c("POST /a/_delete_by_query\n{\"query\": {\"match_all\": {}}}").is_destructive());
        assert!(c("DELETE /a").is_destructive());
        assert!(c("PUT /a").is_destructive());
        assert!(c("PUT /_cluster/settings\n{}").is_destructive());
        assert!(c("POST _bulk\n{\"delete\": {\"_index\": \"a\", \"_id\": \"1\"}}").is_destructive());
        assert_eq!(c("POST _sql?format=json\n{\"query\": \"SELECT 1\"}"), WriteClass::Read);
        assert_eq!(c("POST _plugins/_sql\n{\"query\": \"SELECT * FROM a\"}"), WriteClass::Read);
        assert_eq!(c("POST _plugins/_ppl\n{\"query\": \"source=a | head 5\"}"), WriteClass::Read);
        assert_eq!(c("POST _query\n{\"query\": \"FROM a | LIMIT 1\"}"), WriteClass::Read);
        assert_eq!(c("POST /a/_pit?keep_alive=1m"), WriteClass::Read);
        assert_eq!(c("POST _search/scroll\n{\"scroll_id\": \"x\"}"), WriteClass::Read);
        assert!(c("POST _plugins/_sql\n{\"query\": \"DELETE FROM a\"}").is_destructive());
    }
}
