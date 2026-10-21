//! The policy runs nothing the guard calls destructive, outside YOLO and the allowlist; what an
//! allowlist lets through is exactly its entries, and the rest is judged by the tier.

#[path = "../../dataman-core/tests/sql_corpus/mod.rs"]
mod sql_corpus;

use dataman_core::guard::{self, WriteClass};
use dataman_core::sql::Dialect;
use dataman_core::{Environment, McpAccess};
use dataman_mcp::policy::{By, Target, Verdict, judge};
use sql_corpus::*;

fn targets(d: Dialect) -> Vec<Target> {
    let mut out = Vec::new();
    for access in [McpAccess::Read, McpAccess::Write, McpAccess::Full] {
        for env in [Environment::Dev, Environment::Production] {
            out.push(Target { access, env, read_only: false, dialect: d, yolo: false });
        }
    }
    out
}

/// The allowlist entries a user could have saved from the corpus: each write, its values `?`.
fn entries(d: Dialect) -> Vec<String> {
    corpus(d).iter().filter(|s| guard::agent_write_class(s, d).is_write()).filter_map(|s| guard::allowlist_entry(&entry_of(s, d), d).ok()).collect()
}

fn texts_of(d: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    for t in texts(d) {
        out.extend(variations(&t, d));
        for e in entries(d) {
            out.extend(impostors(&e, d));
        }
        out.push(t);
    }
    for (i, a) in corpus(d).iter().enumerate() {
        for b in corpus(d).iter().skip(i).step_by(7) {
            out.extend(joins(a, b, d));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn judge_runs_nothing_destructive_outside_the_allowlist() {
    let mut fails = Vec::new();
    for d in DIALECTS {
        let allowed = entries(d);
        for sql in texts_of(d) {
            let class = guard::agent_write_class(&sql, d);
            for t in targets(d) {
                let j = judge(true, t, &sql, &[]);
                if j.verdict == Verdict::Run && (class.is_destructive() || t.access == McpAccess::Read && class.is_write()) {
                    fails.push(format!("[{d:?} {:?} {:?}] {sql:?}: Run, but {class:?}", t.access, t.env));
                }
                if j.verdict == Verdict::Run && t.env.is_production() && class.is_write() {
                    fails.push(format!("[{d:?} {:?} production] {sql:?}: Run without asking, {class:?}", t.access));
                }
                // With the allowlist: what runs unasked is the entries and what the tier runs anyway.
                let j = judge(true, t, &sql, &allowed);
                if j.verdict != Verdict::Run {
                    continue;
                }
                let pieces = guard::agent_pieces(&sql, d);
                let rest: Vec<&String> = pieces.iter().filter(|p| !allowed.iter().any(|e| guard::allowlist_match(e, p, d))).collect();
                let rest_class = guard::pieces_class(&rest, d);
                let tier_runs = match rest_class {
                    WriteClass::Read => true,
                    WriteClass::SafeWrite => t.access != McpAccess::Read && !t.env.is_production(),
                    WriteClass::Destructive { .. } => false,
                };
                if !tier_runs {
                    fails.push(format!("[{d:?} {:?} {:?}] {sql:?}: Run by {:?}, but its other statements are {rest_class:?}", t.access, t.env, j.by));
                }
                if let Some(By::Allowlist(used)) = &j.by {
                    for e in used {
                        if !pieces.iter().any(|p| guard::allowlist_match(e, p, d)) {
                            fails.push(format!("[{d:?}] {sql:?}: allowed by {e:?}, which takes none of its statements"));
                        }
                    }
                }
            }
        }
    }
    fails.dedup();
    assert!(fails.is_empty(), "{} divergences:\n{}", fails.len(), fails.iter().take(60).cloned().collect::<Vec<_>>().join("\n"));
}
