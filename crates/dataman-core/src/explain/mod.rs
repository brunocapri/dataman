pub mod hints;
mod text;

use serde_json::{Map, Value};

use crate::sql::tokenize;

pub use hints::{Hint, Rule};
pub use text::render_text;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub lar: bool,
    pub major: u32,
}

impl Target {
    pub fn of(info: &crate::ServerInfo) -> Target {
        Target { lar: info.is_lar, major: info.major() }
    }

    fn at_least(&self, v: u32) -> bool {
        self.major == 0 || self.major >= v
    }
}

pub fn statement(stmt: &str) -> &str {
    let mut s = stmt.trim();
    while let Some(x) = s.strip_suffix(';') {
        s = x.trim_end();
    }
    s
}

pub fn sql(stmt: &str, analyze: bool, t: Target) -> String {
    let stmt = statement(stmt);
    if is_explain(stmt) {
        return stmt.to_string();
    }
    if t.lar {
        return format!("EXPLAIN {}{stmt}", if analyze { "ANALYZE " } else { "" });
    }
    let mut o: Vec<&str> = Vec::new();
    if analyze {
        o.extend(["ANALYZE", "BUFFERS"]);
    }
    o.extend(["FORMAT JSON", "VERBOSE"]);
    if !analyze {
        o.push("COSTS");
    }
    if t.at_least(12) {
        o.push("SETTINGS");
    }
    if analyze && t.at_least(13) {
        o.push("WAL");
    }
    format!("EXPLAIN ({}) {stmt}", o.join(", "))
}

pub fn text_sql(stmt: &str, t: Target) -> String {
    let stmt = statement(stmt);
    if t.lar {
        return format!("EXPLAIN {stmt}");
    }
    let settings = if t.at_least(12) { ", SETTINGS" } else { "" };
    format!("EXPLAIN (FORMAT TEXT, VERBOSE, COSTS{settings}) {stmt}")
}

fn words(sql: &str) -> Vec<crate::sql::Token> {
    tokenize(sql).into_iter().filter(|t| !t.kind.is_trivia()).collect()
}

pub fn is_explain(sql: &str) -> bool {
    words(sql).first().is_some_and(|t| t.text(sql).eq_ignore_ascii_case("EXPLAIN"))
}

fn options(sql: &str) -> Option<(Vec<String>, usize)> {
    let t = words(sql);
    if !t.first().is_some_and(|x| x.text(sql).eq_ignore_ascii_case("EXPLAIN")) {
        return None;
    }
    let mut opts = Vec::new();
    let mut i = 1;
    if t.get(1).is_some_and(|x| x.text(sql) == "(") {
        let mut cur: Vec<String> = Vec::new();
        i = 2;
        while let Some(x) = t.get(i) {
            let s = x.text(sql);
            i += 1;
            if s == ")" || s == "," {
                let off = cur.len() > 1 && matches!(cur[1].as_str(), "FALSE" | "OFF" | "0");
                let on = cur.len() > 1 && matches!(cur[1].as_str(), "TRUE" | "ON" | "1");
                if !cur.is_empty() && !off {
                    opts.push(if on { cur[0].clone() } else { cur.join(" ") });
                }
                cur.clear();
                if s == ")" {
                    break;
                }
            } else {
                cur.push(s.trim_matches('\'').to_ascii_uppercase());
            }
        }
    } else {
        while let Some(x) = t.get(i) {
            let w = x.text(sql).to_ascii_uppercase();
            if !matches!(w.as_str(), "ANALYZE" | "ANALYSE" | "VERBOSE") {
                break;
            }
            opts.push(if w == "ANALYSE" { "ANALYZE".into() } else { w });
            i += 1;
        }
    }
    Some((opts, t.get(i).map_or(sql.len(), |x| x.start)))
}

pub fn inner(sql: &str) -> Option<&str> {
    options(sql).map(|(_, at)| sql[at..].trim())
}

pub fn analyzes(sql: &str) -> bool {
    options(sql).is_some_and(|(o, _)| o.iter().any(|x| x == "ANALYZE" || x == "ANALYSE"))
}

pub fn fmt_ms(ms: f64) -> String {
    match ms {
        m if m < 10. => format!("{m:.2} ms"),
        m if m < 100. => format!("{m:.1} ms"),
        m if m < 1000. => format!("{m:.0} ms"),
        m if m < 60_000. => format!("{:.2} s", m / 1000.),
        m => {
            let s = (m / 1000.).round() as u64;
            format!("{}m {:02}s", s / 60, s % 60)
        }
    }
}

pub fn fmt_count(n: f64) -> String {
    let short = |x: f64, unit: &str| {
        let s = if x < 10. { format!("{x:.1}") } else { format!("{x:.0}") };
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    match n.abs() {
        a if a < 10. && n.fract() != 0. => format!("{n:.2}"),
        a if a < 1000. => format!("{n:.0}"),
        a if a < 999_500. => short(n / 1e3, "k"),
        a if a < 999_500_000. => short(n / 1e6, "M"),
        _ => short(n / 1e9, "G"),
    }
}

pub fn fmt_kb(kb: f64) -> String {
    match kb {
        k if k < 1024. => format!("{k:.0} kB"),
        k if k < 1024. * 1024. => format!("{:.1} MB", k / 1024.),
        k => format!("{:.1} GB", k / 1024. / 1024.),
    }
}

pub fn fmt_cost(c: f64) -> String {
    match c {
        c if c < 100. => format!("{c:.2}"),
        c if c < 10_000. => format!("{c:.0}"),
        c => fmt_count(c),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Text,
    Raw,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Buffers {
    pub shared_hit: f64,
    pub shared_read: f64,
    pub shared_dirtied: f64,
    pub shared_written: f64,
    pub local_hit: f64,
    pub local_read: f64,
    pub temp_read: f64,
    pub temp_written: f64,
}

impl Buffers {
    pub fn is_empty(&self) -> bool {
        *self == Buffers::default()
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub depth: u16,
    pub op: String,
    pub title: String,
    pub raw: Map<String, Value>,
    pub procs: f64,
    pub incl_ms: f64,
    pub excl_ms: f64,
    pub excl_cost: f64,
    pub share: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Misestimate {
    pub factor: f64,
    pub under: bool,
}

impl Node {
    pub fn str(&self, k: &str) -> Option<&str> {
        self.raw.get(k).and_then(Value::as_str)
    }

    pub fn num(&self, k: &str) -> Option<f64> {
        self.raw.get(k).and_then(Value::as_f64)
    }

    pub fn est_rows(&self) -> Option<f64> {
        self.num("Plan Rows")
    }

    pub fn act_rows(&self) -> Option<f64> {
        self.num("Actual Rows")
    }

    pub fn loops(&self) -> Option<f64> {
        self.num("Actual Loops")
    }

    pub fn total_cost(&self) -> Option<f64> {
        self.num("Total Cost")
    }

    pub fn startup_cost(&self) -> Option<f64> {
        self.num("Startup Cost")
    }

    pub fn act_ms(&self) -> Option<f64> {
        self.num("Actual Total Time")
    }

    pub fn relationship(&self) -> Option<&str> {
        self.str("Parent Relationship")
    }

    pub fn subplan(&self) -> Option<&str> {
        self.str("Subplan Name")
    }

    pub fn never_executed(&self) -> bool {
        self.loops() == Some(0.)
    }

    pub fn misestimate(&self) -> Option<Misestimate> {
        let (e, a) = (self.est_rows()?, self.act_rows()?);
        if self.never_executed() {
            return None;
        }
        let (e, a) = (e.max(1.), a.max(1.));
        Some(Misestimate { factor: if a > e { a / e } else { e / a }, under: a > e })
    }

    pub fn buffers(&self) -> Buffers {
        let n = |k| self.num(k).unwrap_or(0.);
        Buffers {
            shared_hit: n("Shared Hit Blocks"),
            shared_read: n("Shared Read Blocks"),
            shared_dirtied: n("Shared Dirtied Blocks"),
            shared_written: n("Shared Written Blocks"),
            local_hit: n("Local Hit Blocks"),
            local_read: n("Local Read Blocks"),
            temp_read: n("Temp Read Blocks"),
            temp_written: n("Temp Written Blocks"),
        }
    }

    pub fn props(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (k, v) in &self.raw {
            match v {
                Value::Array(items) if items.iter().all(Value::is_object) && !items.is_empty() => {
                    for (i, it) in items.iter().enumerate() {
                        let o = it.as_object().expect("checked");
                        let name = match o.get("Worker Number").and_then(Value::as_i64) {
                            Some(w) => format!("Worker {w}"),
                            None => format!("{k} {}", i + 1),
                        };
                        let parts: Vec<String> =
                            o.iter().filter(|(kk, _)| kk.as_str() != "Worker Number").map(|(kk, vv)| format!("{kk} {}", value_text(vv))).collect();
                        out.push((name, parts.join(" · ")));
                    }
                }
                Value::Array(items) if items.is_empty() => {}
                _ => out.push((k.clone(), value_text(v))),
            }
        }
        out
    }

    fn mentions(&self, needle: &str, param: bool) -> bool {
        fn walk(v: &Value, f: &dyn Fn(&str) -> bool) -> bool {
            match v {
                Value::String(s) => f(s),
                Value::Array(a) => a.iter().any(|x| walk(x, f)),
                Value::Object(o) => o.values().any(|x| walk(x, f)),
                _ => false,
            }
        }
        let f = |s: &str| {
            if !param {
                return s.contains(needle);
            }
            s.match_indices(needle).any(|(i, m)| !s[i + m.len()..].starts_with(|c: char| c.is_ascii_digit()))
        };
        self.raw.iter().any(|(k, v)| k != "Subplan Name" && walk(v, &f))
    }
}

pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => {
                let s = format!("{f}");
                s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
            }
            _ => n.to_string(),
        },
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Array(a) => a.iter().map(value_text).collect::<Vec<_>>().join(", "),
        Value::Object(o) => o.iter().map(|(k, v)| format!("{k} {}", value_text(v))).collect::<Vec<_>>().join(" · "),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trigger {
    pub name: String,
    pub relation: Option<String>,
    pub ms: f64,
    pub calls: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Jit {
    pub functions: f64,
    pub total_ms: f64,
    pub phases: Vec<(String, f64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    pub triggers: Vec<Trigger>,
    pub jit: Option<Jit>,
    pub settings: Vec<(String, String)>,
}

impl Totals {
    pub fn trigger_ms(&self) -> Option<f64> {
        (!self.triggers.is_empty()).then(|| self.triggers.iter().map(|t| t.ms).sum())
    }

    pub fn setting(&self, name: &str) -> Option<&str> {
        self.settings.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub format: Format,
    pub nodes: Vec<Node>,
    pub analyzed: bool,
    pub totals: Totals,
    pub hints: Vec<Hint>,
    pub source: String,
}

impl Plan {
    pub fn parse(source: &str) -> Plan {
        let s = source.trim_start();
        if (s.starts_with('[') || s.starts_with('{'))
            && let Ok(v) = serde_json::from_str::<Value>(s)
            && let Some(p) = Plan::from_json(&v, source)
        {
            return p;
        }
        text::parse(source)
    }

    pub fn source_of(columns: &[String], rows: &[Vec<Option<String>>]) -> String {
        let col = |n: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(n));
        if let (Some(t), Some(p)) = (col("plan_type"), col("plan")) {
            let pick = rows
                .iter()
                .find(|r| r.get(t).and_then(|x| x.as_deref()).is_some_and(|x| x.contains("physical") || x.contains("Metrics")))
                .or_else(|| rows.first());
            return pick.and_then(|r| r.get(p).cloned().flatten()).unwrap_or_default();
        }
        rows.iter()
            .map(|r| r.iter().map(|c| c.as_deref().unwrap_or("")).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn from_json(v: &Value, source: &str) -> Option<Plan> {
        let top = match v {
            Value::Array(a) => a.first()?,
            Value::Object(_) => v,
            _ => return None,
        };
        let root = top.get("Plan")?.as_object()?;
        let mut nodes = Vec::new();
        walk_json(root, None, 0, &mut nodes);
        let mut totals = Totals {
            planning_ms: top.get("Planning Time").and_then(Value::as_f64),
            execution_ms: top.get("Execution Time").and_then(Value::as_f64),
            ..Default::default()
        };
        if let Some(Value::Array(ts)) = top.get("Triggers") {
            for t in ts {
                totals.triggers.push(Trigger {
                    name: t.get("Trigger Name").and_then(Value::as_str).unwrap_or("trigger").to_string(),
                    relation: t.get("Relation").and_then(Value::as_str).map(str::to_string),
                    ms: t.get("Time").and_then(Value::as_f64).unwrap_or(0.),
                    calls: t.get("Calls").and_then(Value::as_f64).unwrap_or(0.),
                });
            }
        }
        if let Some(j) = top.get("JIT") {
            let timing = j.get("Timing");
            let ms = |k: &str| {
                timing.and_then(|t| t.get(k)).and_then(|x| x.as_f64().or_else(|| x.get("Total").and_then(Value::as_f64)))
            };
            totals.jit = Some(Jit {
                functions: j.get("Functions").and_then(Value::as_f64).unwrap_or(0.),
                total_ms: ms("Total").unwrap_or(0.),
                phases: ["Generation", "Inlining", "Optimization", "Emission"]
                    .iter()
                    .filter_map(|p| ms(p).map(|m| (p.to_ascii_lowercase(), m)))
                    .collect(),
            });
        }
        if let Some(Value::Object(s)) = top.get("Settings") {
            totals.settings = s.iter().map(|(k, v)| (k.clone(), value_text(v))).collect();
        }
        let mut p = Plan { format: Format::Json, nodes, analyzed: false, totals, hints: Vec::new(), source: source.to_string() };
        p.compute();
        p.hints = hints::find(&p);
        Some(p)
    }

    pub fn end_of(&self, i: usize) -> usize {
        let d = self.nodes[i].depth;
        self.nodes[i + 1..].iter().position(|n| n.depth <= d).map_or(self.nodes.len(), |k| i + 1 + k)
    }

    pub fn is_last(&self, i: usize) -> bool {
        match self.nodes[i].parent {
            Some(p) => self.nodes[p].children.last() == Some(&i),
            None => true,
        }
    }

    pub fn total(&self) -> f64 {
        if self.analyzed { self.nodes.iter().map(|n| n.excl_ms).sum() } else { self.nodes.iter().map(|n| n.excl_cost).sum() }
    }

    pub(crate) fn compute(&mut self) {
        let n = self.nodes.len();
        if n == 0 {
            return;
        }
        self.analyzed = self.nodes.iter().any(|x| x.act_ms().is_some());
        let ends: Vec<usize> = (0..n).map(|i| self.end_of(i)).collect();
        for root in 0..n {
            if self.nodes[root].parent.is_none() {
                self.procs_from(root, None);
            }
        }
        for x in &mut self.nodes {
            x.incl_ms = match (x.act_ms(), x.loops()) {
                (Some(t), Some(l)) if l > 0. => t * l / x.procs.max(1.),
                (Some(t), None) => t,
                _ => 0.,
            };
        }
        let mut ded = vec![0f64; n];
        let mut ctes: Vec<(usize, usize, String)> = Vec::new();
        for p in 0..n {
            for &c in &self.nodes[p].children.clone() {
                let child = &self.nodes[c];
                if child.relationship() == Some("InitPlan") {
                    let name = child.subplan().unwrap_or("").to_string();
                    if let Some(cte) = name.strip_prefix("CTE ") {
                        ctes.push((p, c, cte.to_string()));
                        continue;
                    }
                    let target = self.reader_of(p, c, &name, &ends).unwrap_or(p);
                    ded[target] += child.incl_ms;
                } else {
                    ded[p] += child.incl_ms;
                }
            }
        }
        for (p, c, cte) in ctes {
            let mut scans: Vec<usize> = (p..ends[p])
                .filter(|&j| !(c..ends[c]).contains(&j))
                .filter(|&j| self.nodes[j].op == "CTE Scan" && self.nodes[j].str("CTE Name") == Some(cte.as_str()))
                .collect();
            let t = self.nodes[c].incl_ms;
            if scans.is_empty() {
                ded[p] += t;
                continue;
            }
            let avail = |j: usize, ded: &[f64]| (self.nodes[j].incl_ms - ded[j]).max(0.);
            scans.sort_by(|&a, &b| avail(b, &ded).total_cmp(&avail(a, &ded)).then(a.cmp(&b)));
            let mut left = t;
            for j in scans {
                let take = left.min(avail(j, &ded));
                ded[j] += take;
                left -= take;
            }
        }
        for (i, x) in self.nodes.iter_mut().enumerate() {
            x.excl_ms = (x.incl_ms - ded[i]).max(0.);
        }
        let mut loops = vec![1f64; n];
        for p in 0..n {
            let kids = self.nodes[p].children.clone();
            let outer_rows = kids.iter().find(|&&c| self.nodes[c].relationship() == Some("Outer")).and_then(|&c| self.nodes[c].est_rows());
            for c in kids {
                let k = match self.nodes[c].relationship() {
                    Some("Inner") if self.nodes[p].op.contains("Nested Loop") => outer_rows.unwrap_or(1.).max(1.),
                    Some("SubPlan") => self.nodes[p].est_rows().unwrap_or(1.).max(1.),
                    _ => 1.,
                };
                loops[c] = loops[p] * k;
            }
        }
        let eff: Vec<f64> = (0..n).map(|i| self.nodes[i].total_cost().unwrap_or(0.) * loops[i]).collect();
        for i in 0..n {
            let kids: f64 = self.nodes[i].children.iter().map(|&c| eff[c]).sum();
            self.nodes[i].excl_cost = (eff[i] - kids).max(0.);
        }
        let total = self.total();
        let analyzed = self.analyzed;
        for x in &mut self.nodes {
            let v = if analyzed { x.excl_ms } else { x.excl_cost };
            x.share = if total > 0. { v / total } else { 0. };
        }
    }

    fn procs_from(&mut self, i: usize, gather: Option<(f64, bool)>) {
        let x = &self.nodes[i];
        let loops = x.loops().unwrap_or(1.);
        let procs = match gather {
            None => 1.,
            Some((launched, single)) => match x.raw.get("Workers") {
                Some(Value::Array(ws)) => {
                    let wl: Vec<f64> = ws.iter().map(|w| w.get("Actual Loops").and_then(Value::as_f64).unwrap_or(1.)).collect();
                    let leader = loops - wl.iter().sum::<f64>() > 0.5;
                    (wl.iter().filter(|&&l| l > 0.).count() as f64 + if leader { 1. } else { 0. }).max(1.)
                }
                _ => (launched + if single { 0. } else { 1. }).min(loops).max(1.),
            },
        };
        let below = if x.op.starts_with("Gather") {
            let launched = x.num("Workers Launched").or(x.num("Workers Planned")).unwrap_or(0.);
            Some((launched, x.raw.get("Single Copy").and_then(Value::as_bool).unwrap_or(false)))
        } else {
            gather
        };
        self.nodes[i].procs = procs;
        for c in self.nodes[i].children.clone() {
            self.procs_from(c, below);
        }
    }

    fn reader_of(&self, p: usize, c: usize, name: &str, ends: &[usize]) -> Option<usize> {
        let base = name.split(" (").next().unwrap_or(name).trim();
        let mut needles: Vec<(String, bool)> = vec![(format!("({base})"), false)];
        if let Some(r) = name.split("returns ").nth(1) {
            needles.extend(r.trim_end_matches(')').split(',').map(|s| (s.trim().to_string(), true)).filter(|(s, _)| s.starts_with('$')));
        }
        (p..ends[p])
            .filter(|&j| !(c..ends[c]).contains(&j))
            .filter(|&j| needles.iter().any(|(s, param)| self.nodes[j].mentions(s, *param)))
            .max_by(|&a, &b| self.nodes[a].depth.cmp(&self.nodes[b].depth).then(b.cmp(&a)))
    }
}

fn walk_json(o: &Map<String, Value>, parent: Option<usize>, depth: u16, out: &mut Vec<Node>) -> usize {
    let mut raw = o.clone();
    let kids = raw.remove("Plans");
    let (op, title) = title_of(&raw);
    let id = out.len();
    out.push(Node { parent, children: Vec::new(), depth, op, title, raw, procs: 1., incl_ms: 0., excl_ms: 0., excl_cost: 0., share: 0. });
    if let Some(Value::Array(kids)) = kids {
        for k in kids {
            if let Some(ko) = k.as_object() {
                let c = walk_json(ko, Some(id), depth + 1, out);
                out[id].children.push(c);
            }
        }
    }
    id
}

pub(crate) fn title_of(o: &Map<String, Value>) -> (String, String) {
    let s = |k: &str| o.get(k).and_then(Value::as_str);
    let ty = s("Node Type").unwrap_or("?");
    let mut op = match ty {
        "Aggregate" => {
            let base = match s("Strategy") {
                Some("Hashed") => "HashAggregate",
                Some("Sorted") => "GroupAggregate",
                Some("Mixed") => "MixedAggregate",
                _ => "Aggregate",
            };
            match s("Partial Mode") {
                Some("Partial") => format!("Partial {base}"),
                Some("Finalize") => format!("Finalize {base}"),
                _ => base.to_string(),
            }
        }
        "Hash Join" | "Merge Join" | "Nested Loop" => match s("Join Type") {
            None | Some("Inner") => ty.to_string(),
            Some(j) if ty == "Nested Loop" => format!("Nested Loop {j} Join"),
            Some(j) => format!("{} {j} Join", ty.trim_end_matches(" Join")),
        },
        "SetOp" => format!("{}SetOp {}", if s("Strategy") == Some("Hashed") { "Hash" } else { "" }, s("Command").unwrap_or("")).trim_end().to_string(),
        "ModifyTable" => s("Operation").unwrap_or("ModifyTable").to_string(),
        "Custom Scan" => match s("Custom Plan Provider") {
            Some(p) => format!("Custom Scan ({p})"),
            None => ty.to_string(),
        },
        _ => ty.to_string(),
    };
    if o.get("Parallel Aware").and_then(Value::as_bool) == Some(true) {
        op = format!("Parallel {op}");
    }
    let rel = s("Relation Name");
    let alias = |name: &str| match s("Alias") {
        Some(a) if a != name => format!(" {a}"),
        _ => String::new(),
    };
    let target = match (ty, rel) {
        ("Index Scan" | "Index Only Scan", Some(r)) => format!(" using {} on {r}{}", s("Index Name").unwrap_or("?"), alias(r)),
        ("Bitmap Index Scan", _) => format!(" on {}", s("Index Name").unwrap_or("?")),
        (_, Some(r)) => format!(" on {r}{}", alias(r)),
        ("CTE Scan", _) => {
            let c = s("CTE Name").unwrap_or("?");
            format!(" on {c}{}", alias(c))
        }
        ("Function Scan", _) => {
            let f = s("Function Name").unwrap_or("?");
            format!(" on {f}{}", alias(f))
        }
        ("Subquery Scan" | "Values Scan" | "WorkTable Scan" | "Named Tuplestore Scan" | "Table Function Scan", _) => {
            s("Alias").map(|a| format!(" on {a}")).unwrap_or_default()
        }
        _ => String::new(),
    };
    let title = format!("{op}{target}");
    (op, title)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn fixture(name: &str) -> Plan {
        let s = match name {
            "parallel" => include_str!("fixtures/parallel.json"),
            "nested_loop" => include_str!("fixtures/nested_loop.json"),
            "cte_initplan" => include_str!("fixtures/cte_initplan.json"),
            "subplan" => include_str!("fixtures/subplan.json"),
            "spill" => include_str!("fixtures/spill.json"),
            "seqscan" => include_str!("fixtures/seqscan.json"),
            "misestimate" => include_str!("fixtures/misestimate.json"),
            "estimate" => include_str!("fixtures/estimate.json"),
            "insert_triggers" => include_str!("fixtures/insert_triggers.json"),
            "jit" => include_str!("fixtures/jit.json"),
            _ => panic!("no fixture {name}"),
        };
        let p = Plan::parse(s);
        assert_eq!(p.format, Format::Json, "{name}");
        p
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.002
    }

    fn by_title<'a>(p: &'a Plan, t: &str) -> &'a Node {
        p.nodes.iter().find(|n| n.title == t).unwrap_or_else(|| panic!("no node {t}: {:?}", p.nodes.iter().map(|n| &n.title).collect::<Vec<_>>()))
    }

    fn sums_to_root(p: &Plan) {
        let sum: f64 = p.nodes.iter().map(|n| n.excl_ms).sum();
        assert!(close(sum, p.nodes[0].incl_ms), "{sum} vs {}", p.nodes[0].incl_ms);
    }

    #[test]
    fn statements() {
        let pg = Target { lar: false, major: 18 };
        assert_eq!(sql("select 1;\n", false, pg), "EXPLAIN (FORMAT JSON, VERBOSE, COSTS, SETTINGS) select 1");
        assert_eq!(sql(" delete from t ;; ", true, pg), "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, VERBOSE, SETTINGS, WAL) delete from t");
        assert_eq!(sql("select 1", true, Target { lar: false, major: 12 }), "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, VERBOSE, SETTINGS) select 1");
        assert_eq!(sql("select 1", false, Target { lar: false, major: 11 }), "EXPLAIN (FORMAT JSON, VERBOSE, COSTS) select 1");
        assert_eq!(sql("select 1", true, Target { lar: true, major: 16 }), "EXPLAIN ANALYZE select 1");
        assert_eq!(sql("explain analyze select 1;", false, pg), "explain analyze select 1", "an EXPLAIN runs as written");
        assert_eq!(text_sql("select 1;", pg), "EXPLAIN (FORMAT TEXT, VERBOSE, COSTS, SETTINGS) select 1");
        assert!(is_explain("-- c\n  EXPLAIN select 1"));
        assert!(!is_explain("select 'explain'"));
        assert_eq!(inner("EXPLAIN (ANALYZE, FORMAT JSON) select 1"), Some("select 1"));
        assert_eq!(inner("explain analyse verbose\n  update t set x = 1"), Some("update t set x = 1"));
        assert_eq!(inner("select 1"), None);
        assert!(analyzes("EXPLAIN (BUFFERS, ANALYZE) select 1"));
        assert!(analyzes("explain analyze select 1"));
        assert!(analyzes("EXPLAIN (ANALYZE true) select 1"));
        assert!(!analyzes("EXPLAIN (ANALYZE false, COSTS) select 1"));
        assert!(!analyzes("EXPLAIN (FORMAT JSON) select 1"));
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_ms(0.4242), "0.42 ms");
        assert_eq!(fmt_ms(12.34), "12.3 ms");
        assert_eq!(fmt_ms(312.4), "312 ms");
        assert_eq!(fmt_ms(1250.), "1.25 s");
        assert_eq!(fmt_ms(123_000.), "2m 03s");
        assert_eq!(fmt_count(7.), "7");
        assert_eq!(fmt_count(9.96), "9.96");
        assert_eq!(fmt_count(11473.), "11k");
        assert_eq!(fmt_count(1500.), "1.5k");
        assert_eq!(fmt_count(3_100_000.), "3.1M");
        assert_eq!(fmt_count(999_999.), "1M");
        assert_eq!(fmt_kb(25.), "25 kB");
        assert_eq!(fmt_kb(5864.), "5.7 MB");
        assert_eq!(fmt_cost(48.13), "48.13");
        assert_eq!(fmt_cost(8926.23), "8926");
        assert_eq!(fmt_cost(29080.), "29k");
    }

    #[test]
    fn titles_follow_the_text_format() {
        let p = fixture("parallel");
        let t: Vec<&str> = p.nodes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(t, ["Finalize GroupAggregate", "Gather Merge", "Sort", "Partial HashAggregate", "Parallel Seq Scan on orders"]);
        let p = fixture("nested_loop");
        assert_eq!(p.nodes[3].title, "Bitmap Index Scan on customer_country");
        assert_eq!(p.nodes[4].title, "Index Scan using orders_customer on orders p");
        assert_eq!(fixture("subplan").nodes[0].title, "Index Only Scan using customer_pkey on customer c");
        assert_eq!(fixture("cte_initplan").nodes[5].title, "CTE Scan on m");
        assert_eq!(fixture("cte_initplan").nodes[6].title, "CTE Scan on m b");
        assert_eq!(fixture("insert_triggers").nodes[0].title, "Insert on orders");
        let p = fixture("cte_initplan");
        assert_eq!(p.nodes[0].children, vec![1, 4, 6, 7]);
        assert_eq!(p.end_of(1), 4);
        assert!(p.is_last(7) && !p.is_last(6));
    }

    #[test]
    fn parallel_workers_divide_loops() {
        let p = fixture("parallel");
        assert!(p.analyzed);
        let procs: Vec<f64> = p.nodes.iter().map(|n| n.procs).collect();
        assert_eq!(procs, [1., 1., 3., 3., 3.]);
        assert!(close(p.nodes[2].incl_ms, 201.763));
        assert!(close(p.nodes[1].excl_ms, 219.619 - 201.763), "the Gather's own time is its wait");
        assert!(close(p.nodes[3].excl_ms, 201.712 - 145.403));
        assert!(close(p.nodes[4].excl_ms, 145.403));
        sums_to_root(&p);
        let hot = p.nodes.iter().enumerate().max_by(|a, b| a.1.share.total_cmp(&b.1.share)).unwrap().0;
        assert_eq!(hot, 4, "the scan is the hottest node");
        assert!((p.nodes.iter().map(|n| n.share).sum::<f64>() - 1.).abs() < 1e-9);
    }

    #[test]
    fn loops_multiply_and_workers_count_from_their_list() {
        let p = fixture("nested_loop");
        assert_eq!(p.nodes[1].procs, 2.);
        assert_eq!(p.nodes[3].procs, 1.);
        assert!(close(p.nodes[3].incl_ms, 2.199));
        let inner = 0.097 * 11473. / 2.;
        assert!(close(p.nodes[4].incl_ms, inner));
        assert!(close(p.nodes[1].excl_ms, 644.014 - 70.899 - inner));
        assert!(close(p.nodes[2].excl_ms, 70.899 - 2.199));
        sums_to_root(&p);
        let mut q = p.clone();
        for n in &mut q.nodes {
            n.raw.remove("Workers");
        }
        q.compute();
        assert_eq!(q.nodes[3].procs, 1., "one loop: one process");
        assert_eq!(q.nodes[4].procs, 2.);
    }

    #[test]
    fn subplans_run_inside_their_parent() {
        let p = fixture("subplan");
        assert!(close(p.nodes[1].incl_ms, 0.026 * 499.));
        assert!(close(p.nodes[0].excl_ms, 13.243 - 0.026 * 499.));
        assert!(close(p.nodes[2].excl_ms, (0.025 - 0.001) * 499.));
        sums_to_root(&p);
    }

    #[test]
    fn ctes_go_to_their_scans_and_initplans_to_their_reader() {
        let p = fixture("cte_initplan");
        let root = &p.nodes[0];
        let b = by_title(&p, "CTE Scan on m b");
        let a = by_title(&p, "CTE Scan on m a");
        let cte = &p.nodes[1];
        let init2 = &p.nodes[4];
        assert_eq!(cte.subplan(), Some("CTE m"));
        assert!(close(root.excl_ms, 14.248 - 13.696 - 0.499));
        assert!(close(b.excl_ms, 0.));
        assert!(close(a.excl_ms, 0.478 - 0.444), "{}", a.excl_ms);
        assert!(close(init2.excl_ms, 0.444 - 0.106));
        assert!(close(cte.excl_ms, 13.723 - 12.735));
        sums_to_root(&p);
    }

    #[test]
    fn old_initplan_params_and_parent_fallback() {
        let s = r#"[{"Plan":{"Node Type":"Result","Actual Total Time":5.0,"Actual Loops":1,"Plan Rows":1,"Actual Rows":1,"Total Cost":10,
            "Plans":[
              {"Node Type":"Aggregate","Parent Relationship":"InitPlan","Subplan Name":"InitPlan 1 (returns $0)","Actual Total Time":3.0,"Actual Loops":1,"Total Cost":5},
              {"Node Type":"Seq Scan","Parent Relationship":"Outer","Relation Name":"t","Alias":"t","Filter":"(x > $0)","Actual Total Time":4.5,"Actual Loops":1,"Total Cost":4,
               "Output":["$00"]}
            ]}}]"#;
        let p = Plan::parse(s);
        assert!(close(p.nodes[2].excl_ms, 1.5), "{}", p.nodes[2].excl_ms);
        assert!(close(p.nodes[0].excl_ms, 0.5));
        let s2 = s.replace("(x > $0)", "(x > 1)");
        let p = Plan::parse(&s2);
        assert!(close(p.nodes[0].excl_ms, 0.), "5 - 3 - 4.5 clamps at 0");
        assert!(close(p.nodes[2].excl_ms, 4.5));
    }

    #[test]
    fn cost_shares_without_analyze() {
        let p = fixture("estimate");
        assert!(!p.analyzed);
        assert!(p.nodes.iter().all(|n| n.excl_ms == 0.));
        assert!(close(p.nodes[2].excl_cost, 4.51));
        assert!(close(p.nodes[1].excl_cost, 47.91 - 4.51));
        assert!(close(p.nodes[0].excl_cost, 0.));
        assert!((p.nodes.iter().map(|n| n.share).sum::<f64>() - 1.).abs() < 1e-9);
        let q = fixture("nested_loop");
        let inner_eff: f64 = 8.83 * 6708.;
        assert!(close(q.nodes[1].excl_cost, (61460.52 - 1490.53 - inner_eff).max(0.)));
    }

    #[test]
    fn totals_triggers_jit_and_settings() {
        let p = fixture("insert_triggers");
        assert_eq!(p.totals.planning_ms, Some(3.831));
        assert_eq!(p.totals.execution_ms, Some(42.057));
        assert_eq!(p.totals.trigger_ms(), Some(5.993));
        assert_eq!(p.totals.triggers[0].calls, 49.);
        assert_eq!(p.totals.triggers[0].relation.as_deref(), Some("orders"));
        let j = fixture("jit").totals.jit.unwrap();
        assert_eq!((j.functions, j.total_ms), (4., 12.141));
        assert_eq!(j.phases[0], ("generation".to_string(), 0.840));
        assert_eq!(j.phases.len(), 4);
        let s = fixture("spill");
        assert_eq!(s.totals.setting("work_mem"), Some("64kB"));
        assert_eq!(fixture("parallel").totals.jit, None);
    }

    #[test]
    fn details_list_every_property() {
        let p = fixture("spill");
        let sort = &p.nodes[1];
        let props = sort.props();
        let get = |k: &str| props.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Sort Method"), Some("external merge"));
        assert_eq!(get("Sort Key"), Some("c.email"));
        assert_eq!(get("Actual Total Time"), Some("290.665"));
        assert!(get("Worker 1").unwrap().contains("Sort Space Used 2024"));
        assert!(get("Plans").is_none());
        assert_eq!(props.first().map(|(k, _)| k.as_str()), Some("Node Type"), "in the server's order");
        let m = fixture("misestimate").nodes[0].misestimate().unwrap();
        assert_eq!((m.factor, m.under), (200., true));
        assert_eq!(fixture("parallel").nodes[4].misestimate().unwrap().factor.round(), 1.);
        assert_eq!(fixture("estimate").nodes[0].misestimate(), None, "no actual rows without ANALYZE");
    }

    #[test]
    fn big_plans_parse_fast() {
        fn node(depth: usize, i: usize) -> String {
            let kids = if depth == 0 {
                String::new()
            } else {
                format!(r#","Plans":[{},{}]"#, node(depth - 1, i * 2).replace("\"Outer\"", "\"Outer\""), node(depth - 1, i * 2 + 1).replace("\"Parent Relationship\":\"Outer\"", "\"Parent Relationship\":\"Inner\""))
            };
            format!(
                r#"{{"Node Type":"{}","Parent Relationship":"Outer","Relation Name":"t{i}","Startup Cost":0.0,"Total Cost":{}.0,"Plan Rows":100,"Plan Width":8,"Actual Startup Time":0.01,"Actual Total Time":{}.0,"Actual Rows":100.0,"Actual Loops":1,"Filter":"(x = {i})","Rows Removed by Filter":10{kids}}}"#,
                if depth == 0 { "Seq Scan" } else { "Nested Loop" },
                depth * 10 + 1,
                depth * 2 + 1,
            )
        }
        let s = format!(r#"[{{"Plan":{{"Node Type":"Append","Actual Total Time":50.0,"Actual Loops":1,"Total Cost":100.0,"Plans":[{},{}]}}}}]"#, node(9, 1), node(9, 2));
        let t0 = std::time::Instant::now();
        let p = Plan::parse(&s);
        let took = t0.elapsed();
        eprintln!("{} nodes parsed in {took:?}", p.nodes.len());
        assert_eq!(p.nodes.len(), 2047);
        assert!(took < std::time::Duration::from_millis(250), "{took:?}");
    }

    #[test]
    fn data_fusion_rows_pick_the_physical_plan() {
        let cols = vec!["plan_type".to_string(), "plan".to_string()];
        let rows = vec![
            vec![Some("logical_plan".into()), Some("Projection: t.a\n  TableScan: t".into())],
            vec![Some("physical_plan".into()), Some("ProjectionExec: expr=[a@0 as a]\n  DataSourceExec: partitions=1".into())],
        ];
        assert!(Plan::source_of(&cols, &rows).starts_with("ProjectionExec"));
        let one = vec!["QUERY PLAN".to_string()];
        let lines = vec![vec![Some("Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)".into())], vec![Some("  Filter: (x > 1)".into())]];
        assert_eq!(Plan::source_of(&one, &lines), "Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)\n  Filter: (x > 1)");
    }
}
