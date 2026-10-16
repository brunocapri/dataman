//! Text plans: PostgreSQL's `FORMAT TEXT` (`->` children, `(cost=…)`, detail lines) and plain
//! indented trees (DataFusion, LAR), parsed into the same nodes; and the text a JSON plan prints
//! as ("Copy as text" after ANALYZE, which cannot run again).

use serde_json::{Map, Number, Value};

use super::{Format, Node, Plan, Totals, Trigger, value_text};

pub(super) fn parse(source: &str) -> Plan {
    let lines: Vec<&str> = source.lines().map(|l| l.trim_end()).filter(|l| !l.trim().is_empty()).collect();
    let mut p = Plan { format: Format::Raw, nodes: Vec::new(), analyzed: false, totals: Totals::default(), hints: Vec::new(), source: source.to_string() };
    if lines.is_empty() {
        return p;
    }
    let pg = lines.iter().any(|l| l.contains("(cost=") || l.contains("(actual ") || l.trim_start().starts_with("->"));
    if pg { parse_pg(&lines, &mut p) } else { parse_indented(&lines, &mut p) }
    if !p.nodes.is_empty() {
        p.format = Format::Text;
        p.compute();
    }
    p
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

fn num(s: &str) -> Option<Value> {
    let f: f64 = s.trim().parse().ok()?;
    Number::from_f64(f).map(Value::Number)
}

/// `Seq Scan on public.orders p` → `Seq Scan`.
fn op_of(title: &str) -> String {
    let cut = [" on ", " using "].iter().filter_map(|s| title.find(s)).min().unwrap_or(title.len());
    title[..cut].to_string()
}

fn push(p: &mut Plan, parent: Option<usize>, title: &str, mut raw: Map<String, Value>) -> usize {
    let op = op_of(title);
    raw.insert("Node Type".into(), Value::String(op.trim_start_matches("Parallel ").to_string()));
    if op == "CTE Scan"
        && let Some(name) = title.split(" on ").nth(1).and_then(|r| r.split_whitespace().next())
    {
        raw.insert("CTE Name".into(), Value::String(name.to_string()));
    }
    let depth = parent.map_or(0, |q| p.nodes[q].depth + 1);
    let id = p.nodes.len();
    p.nodes.push(Node {
        parent,
        children: Vec::new(),
        depth,
        op,
        title: title.to_string(),
        raw,
        procs: 1.,
        incl_ms: 0.,
        excl_ms: 0.,
        excl_cost: 0.,
        share: 0.,
    });
    if let Some(q) = parent {
        p.nodes[q].children.push(id);
    }
    id
}

/// The label and measures of a node line: `Hash Join  (cost=1.00..2.00 rows=9 width=4) (actual
/// time=0.1..0.2 rows=9 loops=1)`.
fn node_line(text: &str) -> (String, Map<String, Value>) {
    let mut raw = Map::new();
    let cut = ["  (cost=", " (cost=", "  (actual", " (actual", " (never executed)"].iter().filter_map(|s| text.find(s)).min().unwrap_or(text.len());
    let label = text[..cut].trim().to_string();
    let rest = &text[cut..];
    let field = |s: &str, k: &str| -> Option<String> {
        let i = s.find(k)? + k.len();
        Some(s[i..].split(|c: char| c == ' ' || c == ')').next().unwrap_or("").to_string())
    };
    if let Some(i) = rest.find("(cost=") {
        let s = &rest[i..];
        if let Some((a, b)) = field(s, "cost=").as_deref().and_then(|c| c.split_once("..")) {
            raw.extend(num(a).map(|v| ("Startup Cost".to_string(), v)));
            raw.extend(num(b).map(|v| ("Total Cost".to_string(), v)));
        }
        raw.extend(field(s, "rows=").and_then(|r| num(&r)).map(|v| ("Plan Rows".to_string(), v)));
        raw.extend(field(s, "width=").and_then(|r| num(&r)).map(|v| ("Plan Width".to_string(), v)));
    }
    if let Some(i) = rest.find("(actual") {
        let s = &rest[i..];
        if let Some((a, b)) = field(s, "time=").as_deref().and_then(|c| c.split_once("..")) {
            raw.extend(num(a).map(|v| ("Actual Startup Time".to_string(), v)));
            raw.extend(num(b).map(|v| ("Actual Total Time".to_string(), v)));
        }
        raw.extend(field(s, "rows=").and_then(|r| num(&r)).map(|v| ("Actual Rows".to_string(), v)));
        raw.extend(field(s, "loops=").and_then(|r| num(&r)).map(|v| ("Actual Loops".to_string(), v)));
    }
    if rest.contains("(never executed)") {
        raw.insert("Actual Loops".into(), Value::from(0));
    }
    (label, raw)
}

fn parse_pg(lines: &[&str], p: &mut Plan) {
    // (column of the node's line, node).
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut pending: Option<String> = None;
    let mut in_jit = false;
    for line in lines {
        let col = indent(line);
        let t = line.trim_start();
        let arrow = t.strip_prefix("->");
        if arrow.is_some() || p.nodes.is_empty() {
            let (label, mut raw) = node_line(arrow.map_or(t, str::trim_start));
            while stack.last().is_some_and(|&(c, _)| c >= col) {
                stack.pop();
            }
            if let Some(name) = pending.take() {
                let rel = if name.starts_with("SubPlan") { "SubPlan" } else { "InitPlan" };
                raw.insert("Parent Relationship".into(), Value::String(rel.into()));
                raw.insert("Subplan Name".into(), Value::String(name));
            }
            let id = push(p, stack.last().map(|&(_, n)| n), &label, raw);
            stack.push((col, id));
            in_jit = false;
            continue;
        }
        if col > 0 && (t.starts_with("InitPlan ") || t.starts_with("SubPlan ") || t.starts_with("CTE ")) && !t.contains(':') {
            pending = Some(t.to_string());
            continue;
        }
        let (k, v) = t.split_once(':').map_or((t, ""), |(k, v)| (k.trim(), v.trim()));
        if col == 0 || in_jit {
            in_jit |= k == "JIT";
            let ms = || v.trim_end_matches(" ms").trim().parse::<f64>().ok();
            match k {
                "Planning Time" => p.totals.planning_ms = ms(),
                "Execution Time" => p.totals.execution_ms = ms(),
                "Settings" => {
                    p.totals.settings = v
                        .split(", ")
                        .filter_map(|kv| kv.split_once(" = ").map(|(a, b)| (a.trim().to_string(), b.trim().trim_matches('\'').to_string())))
                        .collect()
                }
                _ if k.starts_with("Trigger ") => {
                    let f = |key: &str| v.split_whitespace().find_map(|w| w.strip_prefix(key)).and_then(|x| x.parse().ok()).unwrap_or(0.);
                    p.totals.triggers.push(Trigger { name: k.trim_start_matches("Trigger ").to_string(), relation: None, ms: f("time="), calls: f("calls=") });
                }
                _ => {}
            }
            continue;
        }
        // A detail of the node above.
        if let Some(&(_, id)) = stack.last() {
            let value = num(v).unwrap_or_else(|| Value::String(v.to_string()));
            p.nodes[id].raw.insert(k.to_string(), value);
        }
    }
}

/// One node per line, children indented under their parent (DataFusion, LAR).
fn parse_indented(lines: &[&str], p: &mut Plan) {
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for line in lines {
        let col = indent(line);
        let t = line.trim();
        while stack.last().is_some_and(|&(c, _)| c >= col) {
            stack.pop();
        }
        let mut raw = Map::new();
        let (op, rest) = t.split_once(':').map_or((t, ""), |(a, b)| (a.trim(), b.trim()));
        if !rest.is_empty() {
            raw.insert("Details".into(), Value::String(rest.to_string()));
        }
        let id = push(p, stack.last().map(|&(_, n)| n), t, raw);
        p.nodes[id].op = op.to_string();
        stack.push((col, id));
    }
}

// ---------------------------------------------------------------------------------------------
// JSON → text.

/// Keys shown in a node's first line, or not at all.
const HEADER: &[&str] = &[
    "Node Type", "Parent Relationship", "Subplan Name", "Parallel Aware", "Async Capable", "Disabled", "Startup Cost", "Total Cost",
    "Plan Rows", "Plan Width", "Actual Startup Time", "Actual Total Time", "Actual Rows", "Actual Loops", "Relation Name", "Schema",
    "Alias", "Index Name", "CTE Name", "Function Name", "Strategy", "Partial Mode", "Operation", "Join Type", "Scan Direction",
    "Custom Plan Provider", "Command", "Workers",
];

fn n2(v: Option<f64>) -> String {
    format!("{:.2}", v.unwrap_or(0.))
}

/// Rows as PostgreSQL prints them: whole estimates, actual rows with decimals only when needed.
fn rows(v: Option<f64>) -> String {
    let v = v.unwrap_or(0.);
    if v.fract() == 0. { format!("{v:.0}") } else { format!("{v:.2}") }
}

fn buffers_line(o: &Map<String, Value>) -> Option<String> {
    let n = |k: &str| o.get(k).and_then(Value::as_f64).unwrap_or(0.);
    let part = |label: &str, keys: [(&str, &str); 4]| {
        let xs: Vec<String> = keys.iter().filter(|(_, k)| n(k) > 0.).map(|(w, k)| format!("{w}={}", n(k))).collect();
        (!xs.is_empty()).then(|| format!("{label} {}", xs.join(" ")))
    };
    let parts: Vec<String> = [
        part("shared", [("hit", "Shared Hit Blocks"), ("read", "Shared Read Blocks"), ("dirtied", "Shared Dirtied Blocks"), ("written", "Shared Written Blocks")]),
        part("local", [("hit", "Local Hit Blocks"), ("read", "Local Read Blocks"), ("dirtied", "Local Dirtied Blocks"), ("written", "Local Written Blocks")]),
        part("temp", [("read", "Temp Read Blocks"), ("written", "Temp Written Blocks"), ("", ""), ("", "")]),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| format!("Buffers: {}", parts.join(", ")))
}

fn wal_line(o: &Map<String, Value>) -> Option<String> {
    let n = |k: &str| o.get(k).and_then(Value::as_f64).unwrap_or(0.);
    let xs: Vec<String> =
        [("records", "WAL Records"), ("fpi", "WAL FPI"), ("bytes", "WAL Bytes"), ("buffers full", "WAL Buffers Full")].iter().filter(|(_, k)| n(k) > 0.).map(|(w, k)| format!("{w}={}", n(k))).collect();
    (!xs.is_empty()).then(|| format!("WAL: {}", xs.join(" ")))
}

fn skip_key(k: &str) -> bool {
    HEADER.contains(&k) || k.ends_with(" Blocks") || k.starts_with("WAL ")
}

/// The plan as PostgreSQL's text format prints it (a text plan is returned as it came).
pub fn render_text(p: &Plan) -> String {
    if p.format != Format::Json {
        return p.source.clone();
    }
    let mut out = String::new();
    for n in &p.nodes {
        let d = n.depth as usize;
        let arrow_col = if d == 0 { 0 } else { 2 + 6 * (d - 1) };
        let detail = " ".repeat(2 + 6 * d);
        if let Some(s) = n.subplan() {
            out.push_str(&format!("{}{s}\n", " ".repeat(arrow_col)));
        }
        if d > 0 {
            out.push_str(&format!("{}->  ", " ".repeat(arrow_col)));
        }
        out.push_str(&n.title);
        if n.total_cost().is_some() {
            out.push_str(&format!("  (cost={}..{} rows={} width={})", n2(n.startup_cost()), n2(n.total_cost()), rows(n.est_rows()), rows(n.num("Plan Width"))));
        }
        if n.never_executed() {
            out.push_str(" (never executed)");
        } else if let Some(t) = n.act_ms() {
            out.push_str(&format!(" (actual time={:.3}..{t:.3} rows={} loops={})", n.num("Actual Startup Time").unwrap_or(0.), rows(n.act_rows()), rows(n.loops())));
        }
        out.push('\n');
        for (k, v) in &n.raw {
            if skip_key(k) || matches!(v, Value::Array(a) if a.is_empty()) {
                continue;
            }
            out.push_str(&format!("{detail}{k}: {}\n", value_text(v)));
        }
        for line in [buffers_line(&n.raw), wal_line(&n.raw)].into_iter().flatten() {
            out.push_str(&format!("{detail}{line}\n"));
        }
        if let Some(Value::Array(ws)) = n.raw.get("Workers") {
            for w in ws.iter().filter_map(Value::as_object) {
                let num = w.get("Worker Number").and_then(Value::as_i64).unwrap_or(0);
                let f = |k: &str| w.get(k).and_then(Value::as_f64);
                let mut line = format!("Worker {num}: ");
                if let Some(t) = f("Actual Total Time") {
                    line.push_str(&format!(" actual time={:.3}..{t:.3} rows={} loops={}", f("Actual Startup Time").unwrap_or(0.), rows(f("Actual Rows")), rows(f("Actual Loops"))));
                }
                out.push_str(&format!("{detail}{}\n", line.trim_end()));
            }
        }
    }
    let t = &p.totals;
    if !t.settings.is_empty() {
        let s: Vec<String> = t.settings.iter().map(|(k, v)| format!("{k} = '{v}'")).collect();
        out.push_str(&format!("Settings: {}\n", s.join(", ")));
    }
    if let Some(ms) = t.planning_ms {
        out.push_str(&format!("Planning Time: {ms:.3} ms\n"));
    }
    for tr in &t.triggers {
        out.push_str(&format!("Trigger {}: time={:.3} calls={}\n", tr.name, tr.ms, tr.calls));
    }
    if let Some(j) = &t.jit {
        out.push_str(&format!("JIT:\n  Functions: {}\n", j.functions));
        let phases: Vec<String> = j.phases.iter().map(|(k, v)| format!("{}{} {v:.3} ms", k[..1].to_uppercase(), &k[1..])).collect();
        out.push_str(&format!("  Timing: {}, Total {:.3} ms\n", phases.join(", "), j.total_ms));
    }
    if let Some(ms) = t.execution_ms {
        out.push_str(&format!("Execution Time: {ms:.3} ms\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn postgres_text_plans() {
        let p = Plan::parse(include_str!("fixtures/text_plan.txt"));
        assert_eq!(p.format, Format::Text);
        let t: Vec<(&str, u16)> = p.nodes.iter().map(|n| (n.title.as_str(), n.depth)).collect();
        assert_eq!(
            t,
            [
                ("Hash Join", 0),
                ("Seq Scan on public.orders p", 1),
                ("Hash", 1),
                ("Bitmap Heap Scan on public.customer c", 2),
                ("Bitmap Index Scan on customer_country", 3)
            ]
        );
        assert_eq!(p.nodes[0].children, vec![1, 2]);
        assert_eq!(p.nodes[1].op, "Seq Scan");
        assert_eq!(p.nodes[1].total_cost(), Some(31830.46));
        assert_eq!(p.nodes[1].est_rows(), Some(1000046.));
        assert_eq!(p.nodes[0].str("Hash Cond"), Some("(p.customer = c.id)"));
        assert_eq!(p.nodes[3].str("Recheck Cond"), Some("(c.country = 'DE'::text)"));
        assert_eq!(p.totals.setting("work_mem"), Some("64MB"));
        assert!(!p.analyzed);
        assert!(p.hints.is_empty(), "no hints from text");
        assert!(p.nodes[1].share > 0.8, "the seq scan holds most of the cost");
    }

    #[test]
    fn analyzed_text_with_initplans() {
        let s = "Result  (cost=1.00..1.01 rows=1 width=4) (actual time=0.010..5.000 rows=1 loops=1)
  InitPlan 1
    ->  Aggregate  (cost=1.00..1.01 rows=1 width=4) (actual time=3.000..3.000 rows=1 loops=1)
          ->  Seq Scan on t  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..2.000 rows=10 loops=1)
  ->  Seq Scan on u  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..4.500 rows=10 loops=1)
        Filter: (x > (InitPlan 1).col1)
        Rows Removed by Filter: 5
  ->  Seq Scan on v  (never executed)
Planning Time: 0.100 ms
Trigger t_audit: time=1.500 calls=10
Execution Time: 6.500 ms";
        let p = Plan::parse(s);
        assert!(p.analyzed);
        assert_eq!(p.nodes.len(), 5);
        assert_eq!(p.nodes[1].subplan(), Some("InitPlan 1"));
        assert_eq!(p.nodes[1].relationship(), Some("InitPlan"));
        assert_eq!(p.nodes[3].parent, Some(0));
        assert_eq!(p.nodes[3].num("Rows Removed by Filter"), Some(5.));
        assert!(p.nodes[4].never_executed());
        // The InitPlan is read by u's filter: u pays for it.
        assert!((p.nodes[3].excl_ms - 1.5).abs() < 1e-9);
        assert!((p.nodes[0].excl_ms - 0.5).abs() < 1e-9);
        assert_eq!(p.totals.execution_ms, Some(6.5));
        assert_eq!(p.totals.trigger_ms(), Some(1.5));
    }

    #[test]
    fn indented_plans_and_raw_text() {
        // DataFusion's shape (what LAR would answer): one operator per line, two-space indent.
        let s = "ProjectionExec: expr=[id@0 as id, name@1 as name]\n  CoalesceBatchesExec: target_batch_size=8192\n    FilterExec: country@2 = DE\n      DataSourceExec: partitions=1\n  SortExec: expr=[id@0 ASC]";
        let p = Plan::parse(s);
        assert_eq!(p.format, Format::Text);
        let t: Vec<(&str, u16)> = p.nodes.iter().map(|n| (n.op.as_str(), n.depth)).collect();
        assert_eq!(t, [("ProjectionExec", 0), ("CoalesceBatchesExec", 1), ("FilterExec", 2), ("DataSourceExec", 3), ("SortExec", 1)]);
        assert_eq!(p.nodes[2].str("Details"), Some("country@2 = DE"));
        assert!(p.nodes.iter().all(|n| n.share == 0.), "no costs: no bars");
        assert!(p.hints.is_empty());
        let p = Plan::parse("   \n");
        assert_eq!(p.format, Format::Raw);
        assert!(p.nodes.is_empty());
    }

    #[test]
    fn json_prints_as_text_and_parses_back() {
        for name in ["cte_initplan", "nested_loop", "spill", "jit", "insert_triggers", "estimate"] {
            let p = fixture(name);
            let text = render_text(&p);
            let back = Plan::parse(&text);
            assert_eq!(back.format, Format::Text, "{name}");
            let a: Vec<&str> = p.nodes.iter().map(|n| n.title.as_str()).collect();
            let b: Vec<&str> = back.nodes.iter().map(|n| n.title.as_str()).collect();
            assert_eq!(a, b, "{name}:\n{text}");
            let da: Vec<u16> = p.nodes.iter().map(|n| n.depth).collect();
            let db: Vec<u16> = back.nodes.iter().map(|n| n.depth).collect();
            assert_eq!(da, db, "{name}:\n{text}");
            assert_eq!(back.totals.execution_ms, p.totals.execution_ms, "{name}");
        }
        let text = render_text(&fixture("spill"));
        assert!(text.contains("Sort Method: external merge"), "{text}");
        assert!(text.contains("Buffers: shared hit=23080, temp read=3526 written=4138"), "{text}");
        assert!(text.contains("Worker 1:  actual time=262.481..300.371 rows=34566 loops=1"), "{text}");
        assert!(text.contains("  ->  Sort  (cost=38078.90..38181.56 rows=41065 width=45) (actual time=263.064..290.665 rows=33333 loops=3)"), "{text}");
        let text = render_text(&fixture("jit"));
        assert!(text.contains("JIT:\n  Functions: 4\n  Timing: Generation 0.840 ms"), "{text}");
        let raw = Plan::parse("not a plan at all");
        assert_eq!(render_text(&raw), "not a plan at all");
    }
}
