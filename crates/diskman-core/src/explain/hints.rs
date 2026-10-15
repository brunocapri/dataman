//! Findings computed from an analyzed JSON plan: terse, few, and only where the numbers are
//! clear. Each names its node, a chip for the node's row and a line for the list.

use super::{Plan, fmt_count, fmt_kb};

/// What a hint is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// A sequential scan whose filter throws away almost everything it reads.
    SeqScanFilter,
    /// A sort or a hash aggregate that went to disk.
    Spill,
    /// A hash join's table did not fit in memory: several batches.
    HashBatches,
    /// A nested loop that ran its inner side very many times.
    NestedLoops,
    /// A scan whose row estimate was far off.
    Misestimate,
    /// A Gather got fewer workers than planned.
    Workers,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hint {
    pub node: usize,
    pub rule: Rule,
    /// On the node's row (`filter 99%`); empty when the row says it already.
    pub chip: String,
    /// `Seq Scan on orders · 3.1M rows · filter removes 99%`.
    pub text: String,
    /// `index on orders (status)`, `raise work_mem`.
    pub fix: Option<String>,
}

/// Rows read before a filter's share means anything.
pub const SCAN_MIN_ROWS: f64 = 10_000.;
/// Share of the rows read that the filter removes.
pub const FILTER_MIN: f64 = 0.95;
/// Inner-side runs of a nested loop worth a mention.
pub const LOOPS_MIN: f64 = 10_000.;
/// Estimate off by this much (either way).
pub const MISESTIMATE_MIN: f64 = 100.;
/// …on a scan that returned (or was expected to return) at least this many rows in all.
pub const MISESTIMATE_ROWS: f64 = 1_000.;

/// The findings of a plan, the costliest node first.
pub fn find(p: &Plan) -> Vec<Hint> {
    let mut out = Vec::new();
    if !p.analyzed {
        return out;
    }
    let work_mem = p.totals.setting("work_mem").map(|w| format!("raise work_mem · now {w}")).unwrap_or_else(|| "raise work_mem".into());
    for (i, n) in p.nodes.iter().enumerate() {
        if n.never_executed() {
            continue;
        }
        let loops = n.loops().unwrap_or(1.);
        let rel = n.str("Relation Name");
        // A seq scan reading many rows for few.
        if n.op.ends_with("Seq Scan")
            && let (Some(rows), Some(removed)) = (n.act_rows(), n.num("Rows Removed by Filter"))
        {
            let (kept, removed) = (rows * loops, removed * loops);
            let read = kept + removed;
            if read >= SCAN_MIN_ROWS && removed / read >= FILTER_MIN {
                let pct = if kept == 0. { 100. } else { ((removed / read) * 100.).floor().min(99.) };
                let cols = n.str("Filter").map(filter_columns).unwrap_or_default();
                let target = rel.unwrap_or("the table");
                let fix = if cols.is_empty() { format!("index for the filter on {target}") } else { format!("index on {target} ({})", cols.join(", ")) };
                out.push(Hint {
                    node: i,
                    rule: Rule::SeqScanFilter,
                    chip: format!("filter {pct:.0}%"),
                    text: format!("{} · {} rows · filter removes {pct:.0}%", n.title, fmt_count(read)),
                    fix: Some(fix),
                });
            }
        }
        // Sorts and hash aggregates on disk (the leader's numbers and each worker's).
        let spilled = spill_kb(n);
        if spilled > 0. {
            let what = if n.op.contains("Sort") { "Sort" } else { n.op.as_str() };
            out.push(Hint {
                node: i,
                rule: Rule::Spill,
                chip: format!("spill {}", fmt_kb(spilled)),
                text: format!("{what} spills to disk · {}", fmt_kb(spilled)),
                fix: Some(work_mem.clone()),
            });
        }
        if n.op.ends_with("Hash")
            && let Some(b) = n.num("Hash Batches")
            && b > 1.
        {
            out.push(Hint { node: i, rule: Rule::HashBatches, chip: format!("{b:.0} batches"), text: format!("Hash batches {b:.0}"), fix: Some(work_mem.clone()) });
        }
        if n.op.contains("Nested Loop")
            && let Some(&inner) = n.children.iter().find(|&&c| p.nodes[c].relationship() == Some("Inner"))
            && let Some(l) = p.nodes[inner].loops()
            && l >= LOOPS_MIN
        {
            let outer = n.children.iter().find(|&&c| p.nodes[c].relationship() == Some("Outer")).and_then(|&c| p.nodes[c].misestimate());
            let fix = outer.filter(|m| m.factor >= 10.).map(|m| format!("outer rows misestimated ×{}", fmt_count(m.factor.round())));
            out.push(Hint { node: i, rule: Rule::NestedLoops, chip: format!("{} loops", fmt_count(l)), text: format!("Nested Loop with {} loops", fmt_count(l)), fix });
        }
        if let Some(r) = rel
            && let Some(m) = n.misestimate()
            && m.factor >= MISESTIMATE_MIN
            && n.est_rows().unwrap_or(0.).max(n.act_rows().unwrap_or(0.)) * loops >= MISESTIMATE_ROWS
            && !n.op.starts_with("Insert")
            && !matches!(n.op.as_str(), "Update" | "Delete" | "Merge")
        {
            out.push(Hint {
                node: i,
                rule: Rule::Misestimate,
                chip: String::new(),
                text: format!("rows misestimated ×{} · {}", fmt_count(m.factor.round()), n.title),
                fix: Some(format!("ANALYZE {r}")),
            });
        }
        if n.op.starts_with("Gather")
            && let (Some(planned), Some(launched)) = (n.num("Workers Planned"), n.num("Workers Launched"))
            && launched < planned
        {
            out.push(Hint {
                node: i,
                rule: Rule::Workers,
                chip: format!("{launched:.0}/{planned:.0} workers"),
                text: format!("{} · {launched:.0} of {planned:.0} workers launched", n.title),
                fix: Some("max_parallel_workers".into()),
            });
        }
    }
    out.sort_by(|a, b| p.nodes[b.node].excl_ms.total_cmp(&p.nodes[a.node].excl_ms).then(a.rule.cmp(&b.rule)).then(a.node.cmp(&b.node)));
    out
}

/// Kilobytes a sort or hash aggregate wrote to disk, over the leader and its workers.
fn spill_kb(n: &super::Node) -> f64 {
    let on_disk = |o: &serde_json::Map<String, serde_json::Value>| {
        let f = |k: &str| o.get(k).and_then(serde_json::Value::as_f64).unwrap_or(0.);
        let sort = if o.get("Sort Space Type").and_then(serde_json::Value::as_str) == Some("Disk") { f("Sort Space Used") } else { 0. };
        sort + f("Disk Usage")
    };
    let mut kb = on_disk(&n.raw);
    if let Some(serde_json::Value::Array(ws)) = n.raw.get("Workers") {
        kb += ws.iter().filter_map(serde_json::Value::as_object).map(on_disk).sum::<f64>();
    }
    kb
}

/// The columns a filter compares directly (`(customer.city = 'x'::text)` → `city`), when an
/// index could serve it: none for an `OR`, nor for expressions.
pub fn filter_columns(filter: &str) -> Vec<String> {
    if filter.contains(" OR ") {
        return Vec::new();
    }
    let mut cols: Vec<String> = Vec::new();
    for part in filter.split(" AND ") {
        let p = part.trim().trim_start_matches('(').trim_start();
        let name: String = p.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.' || *c == '"').collect();
        let rest = p[name.len()..].trim_start();
        let compares = ["=", "<", ">", "~~", "!~~", "IS ", "<>", ">=", "<="].iter().any(|op| rest.starts_with(op));
        if name.is_empty() || !compares || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let col = name.rsplit('.').next().unwrap_or(&name).trim_matches('"').to_string();
        if !cols.contains(&col) {
            cols.push(col);
        }
    }
    cols.truncate(3);
    cols
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    fn rules(p: &Plan) -> Vec<Rule> {
        p.hints.iter().map(|h| h.rule).collect()
    }

    #[test]
    fn seq_scan_filter() {
        let p = fixture("seqscan");
        let h = p.hints.iter().find(|h| h.rule == Rule::SeqScanFilter).expect("hint");
        assert_eq!(h.text, "Seq Scan on customer · 100k rows · filter removes 100%");
        assert_eq!(h.chip, "filter 100%");
        assert_eq!(h.fix.as_deref(), Some("index for the filter on customer"), "an OR: no column to name");
        // 90 % removed (1M rows read for 100k) is not enough.
        assert!(!rules(&fixture("spill")).contains(&Rule::SeqScanFilter));
        // 85,715 of 100,000 removed: no.
        assert!(!rules(&fixture("jit")).contains(&Rule::SeqScanFilter));
        assert_eq!(filter_columns("(customer.city = 'Berlin'::text)"), ["city"]);
        assert_eq!(filter_columns("((p.status = 'paid'::text) AND (p.total > 100))"), ["status", "total"]);
        assert!(filter_columns("((customer.id % '7'::bigint) = 0)").is_empty());
        assert!(filter_columns("((a = 1) OR (b = 2))").is_empty());
    }

    #[test]
    fn seq_scan_filter_names_the_column() {
        let s = include_str!("fixtures/seqscan.json").replace(
            "((customer.city = 'Berlin'::text) OR ((customer.name || ''::text) = 'x'::text))",
            "(customer.city = 'Berlin'::text)",
        );
        let p = Plan::parse(&s);
        assert_eq!(p.hints[0].fix.as_deref(), Some("index on customer (city)"));
    }

    #[test]
    fn spills() {
        let p = fixture("spill");
        let h = p.hints.iter().find(|h| h.rule == Rule::Spill).expect("hint");
        // 1,944 + 1,896 + 2,024 kB over the leader and both workers.
        assert_eq!(h.text, "Sort spills to disk · 5.7 MB");
        assert_eq!(h.chip, "spill 5.7 MB");
        assert_eq!(h.fix.as_deref(), Some("raise work_mem · now 64kB"));
        assert_eq!(p.nodes[h.node].op, "Sort");
        assert!(!rules(&fixture("parallel")).contains(&Rule::Spill), "quicksort in memory");
    }

    #[test]
    fn hash_batches() {
        let p = fixture("spill");
        let h = p.hints.iter().find(|h| h.rule == Rule::HashBatches).expect("hint");
        assert_eq!(h.text, "Hash batches 32");
        assert_eq!(h.chip, "32 batches");
        assert!(!rules(&fixture("cte_initplan")).contains(&Rule::HashBatches), "one batch");
    }

    #[test]
    fn nested_loops() {
        let p = fixture("nested_loop");
        let h = p.hints.iter().find(|h| h.rule == Rule::NestedLoops).expect("hint");
        assert_eq!(h.text, "Nested Loop with 11k loops");
        assert_eq!(h.chip, "11k loops");
        assert_eq!(h.fix, None, "the outer rows were well estimated");
        let few = include_str!("fixtures/nested_loop.json").replace("\"Actual Loops\":11473", "\"Actual Loops\":900");
        assert!(!rules(&Plan::parse(&few)).contains(&Rule::NestedLoops));
    }

    #[test]
    fn misestimates() {
        let p = fixture("misestimate");
        assert_eq!(rules(&p), [Rule::Misestimate]);
        assert_eq!(p.hints[0].text, "rows misestimated ×200 · Seq Scan on customer");
        assert_eq!(p.hints[0].fix.as_deref(), Some("ANALYZE customer"));
        assert!(p.hints[0].chip.is_empty(), "the row's ×200 says it");
        assert!(!rules(&fixture("parallel")).contains(&Rule::Misestimate));
        // Far off but small: 2 estimated, 300 returned.
        let tiny = include_str!("fixtures/misestimate.json").replace("\"Plan Rows\":500", "\"Plan Rows\":2").replace("\"Actual Rows\":100000.00", "\"Actual Rows\":300.00");
        assert!(!rules(&Plan::parse(&tiny)).contains(&Rule::Misestimate));
    }

    #[test]
    fn workers_short() {
        let short = include_str!("fixtures/parallel.json").replace("\"Workers Launched\":2", "\"Workers Launched\":0");
        let p = Plan::parse(&short);
        let h = p.hints.iter().find(|h| h.rule == Rule::Workers).expect("hint");
        assert_eq!(h.text, "Gather Merge · 0 of 2 workers launched");
        assert_eq!(h.chip, "0/2 workers");
        assert!(!rules(&fixture("parallel")).contains(&Rule::Workers));
    }

    #[test]
    fn nothing_without_analyze_and_costliest_first() {
        assert!(fixture("estimate").hints.is_empty());
        let p = fixture("spill");
        let ex: Vec<f64> = p.hints.iter().map(|h| p.nodes[h.node].excl_ms).collect();
        assert!(ex.windows(2).all(|w| w[0] >= w[1]), "{ex:?}");
        assert!(fixture("subplan").hints.is_empty());
    }
}
