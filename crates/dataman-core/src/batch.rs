use std::collections::HashMap;
use std::sync::Arc;

use crate::result::{Phase, ResultSet};
use crate::sql::{first_table, statement_verb};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Key {
    Stmt(usize),
    Messages,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Paused,
    Done,
    Partial,
    Cancelled,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub state: State,
    pub cols: Option<bool>,
    pub affected: Option<u64>,
    pub messages: bool,
}

impl Outcome {
    pub fn of(rs: &ResultSet) -> Outcome {
        let st = rs.status();
        let state = match st.phase.unwrap_or(Phase::Running) {
            Phase::Running => State::Running,
            Phase::Paused => State::Paused,
            Phase::Done if st.partial => State::Partial,
            Phase::Done => State::Done,
            Phase::Cancelled => State::Cancelled,
            Phase::Failed => State::Error,
        };
        Outcome { state, cols: rs.columns().map(|c| !c.is_empty()), affected: st.affected, messages: rs.has_messages() }
    }

    pub fn shows_messages(&self) -> bool {
        self.messages && self.cols != Some(true) && self.state != State::Error
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Rows,
    Affected,
    Error,
    Cancelled,
    Command,
}

const DML: &[&str] = &["INSERT", "UPDATE", "DELETE", "MERGE", "COPY"];

const COMMANDS: &[&str] = &[
    "CREATE", "DROP", "ALTER", "TRUNCATE", "SET", "RESET", "BEGIN", "START", "COMMIT", "END", "ROLLBACK", "ABORT", "SAVEPOINT",
    "RELEASE", "PREPARE", "DEALLOCATE", "GRANT", "REVOKE", "COMMENT", "VACUUM", "ANALYZE", "CLUSTER", "REINDEX", "LOCK",
    "DISCARD", "LISTEN", "UNLISTEN", "NOTIFY", "DO", "REFRESH", "SECURITY", "DECLARE", "CLOSE", "MOVE", "CHECKPOINT", "LOAD",
    "IMPORT", "REASSIGN",
];

fn first_word(verb: &str) -> &str {
    verb.split_whitespace().next().unwrap_or("")
}

pub fn kind(sql: &str, o: &Outcome, multi: bool) -> Kind {
    match o.state {
        State::Error => return Kind::Error,
        State::Cancelled => return Kind::Cancelled,
        _ => {}
    }
    if o.cols == Some(true) {
        return Kind::Rows;
    }
    let verb = statement_verb(sql);
    let w = first_word(&verb);
    if DML.contains(&w) {
        Kind::Affected
    } else if multi && (o.cols == Some(false) || COMMANDS.contains(&w)) {
        Kind::Command
    } else if o.cols == Some(false) {
        Kind::Affected
    } else {
        Kind::Rows
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub key: Key,
    pub number: Option<usize>,
    pub label: String,
    pub state: State,
    pub kind: Kind,
    pub folded: usize,
}

const LABEL_MAX: usize = 22;

fn short(s: String) -> String {
    if s.chars().count() <= LABEL_MAX {
        return s;
    }
    let mut out: String = s.chars().take(LABEL_MAX - 1).collect();
    out.push('…');
    out
}

pub fn label(sql: &str, o: &Outcome, kind: Kind) -> String {
    short(match kind {
        Kind::Affected | Kind::Command => tag(sql, o.affected),
        _ => first_table(sql).unwrap_or_else(|| statement_verb(sql)),
    })
}

pub fn tag(sql: &str, affected: Option<u64>) -> String {
    let verb = statement_verb(sql);
    match affected {
        Some(n) if DML.contains(&first_word(&verb)) => format!("{verb} {n}"),
        _ => verb,
    }
}

pub fn tabs(stmts: &[(&str, Option<Outcome>)]) -> Vec<Tab> {
    let multi = stmts.len() > 1;
    let mut out = Vec::new();
    let mut folded = 0;
    let mut folded_running = false;
    let mut printed = false;
    for (i, (sql, o)) in stmts.iter().enumerate() {
        let Some(o) = o else { continue };
        let k = kind(sql, o, multi);
        if k == Kind::Command {
            folded += 1;
            folded_running |= o.state == State::Running;
            continue;
        }
        printed |= o.messages && !o.shows_messages();
        out.push(Tab { key: Key::Stmt(i), number: Some(i + 1), label: label(sql, o, k), state: o.state, kind: k, folded: 0 });
    }
    if folded > 0 || printed {
        let state = if folded_running { State::Running } else { State::Done };
        out.push(Tab { key: Key::Messages, number: None, label: "messages".into(), state, kind: Kind::Command, folded });
    }
    out
}

pub fn default_active(tabs: &[Tab]) -> Option<Key> {
    if let Some(t) = tabs.iter().find(|t| t.kind == Kind::Error) {
        return Some(t.key);
    }
    tabs.iter()
        .rev()
        .find(|t| matches!(t.key, Key::Stmt(_)))
        .or_else(|| tabs.iter().find(|t| t.key == Key::Messages))
        .or_else(|| tabs.first())
        .map(|t| t.key)
}

pub fn active(tabs: &[Tab], pick: Option<Key>) -> Option<Key> {
    pick.filter(|p| tabs.iter().any(|t| t.key == *p)).or_else(|| default_active(tabs))
}

pub fn cycle(tabs: &[Tab], from: Option<Key>, forward: bool) -> Option<Key> {
    let n = tabs.len();
    if n == 0 {
        return None;
    }
    let i = from.and_then(|k| tabs.iter().position(|t| t.key == k));
    let j = match (i, forward) {
        (Some(i), true) => (i + 1) % n,
        (Some(i), false) => (i + n - 1) % n,
        (None, true) => 0,
        (None, false) => n - 1,
    };
    Some(tabs[j].key)
}

pub struct Views<V> {
    map: HashMap<Key, (Arc<ResultSet>, V)>,
}

impl<V> Default for Views<V> {
    fn default() -> Self {
        Views { map: HashMap::new() }
    }
}

impl<V> Views<V> {
    pub fn put(&mut self, key: Key, rs: Arc<ResultSet>, v: V) {
        self.map.insert(key, (rs, v));
    }

    pub fn take(&mut self, key: Key, rs: &Arc<ResultSet>) -> Option<V> {
        match self.map.remove(&key) {
            Some((r, v)) if Arc::ptr_eq(&r, rs) => Some(v),
            _ => None,
        }
    }

    pub fn of_result(&mut self, rs: &Arc<ResultSet>) -> Option<&mut V> {
        self.map.values_mut().find(|(r, _)| Arc::ptr_eq(r, rs)).map(|(_, v)| v)
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn remove(&mut self, key: Key) {
        self.map.remove(&key);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(state: State, cols: Option<bool>, affected: Option<u64>) -> Option<Outcome> {
        Some(Outcome { state, cols, affected, messages: false })
    }

    fn printed(state: State, cols: Option<bool>) -> Option<Outcome> {
        Some(Outcome { state, cols, affected: None, messages: true })
    }

    fn labels(t: &[Tab]) -> Vec<String> {
        t.iter().map(|t| format!("{}{}", t.number.map(|n| format!("{n} · ")).unwrap_or_default(), t.label)).collect()
    }

    #[test]
    fn rows_counts_errors_and_not_run() {
        let stmts = [
            ("SELECT * FROM public.orders o", o(State::Done, Some(true), None)),
            ("UPDATE orders SET x = 1 WHERE id < 4", o(State::Done, Some(false), Some(3))),
            ("SELECT * FROM nope", o(State::Error, None, None)),
            ("SELECT 1", None),
        ];
        let t = tabs(&stmts);
        assert_eq!(labels(&t), ["1 · orders", "2 · UPDATE 3", "3 · nope"]);
        assert_eq!(t.iter().map(|t| t.kind).collect::<Vec<_>>(), [Kind::Rows, Kind::Affected, Kind::Error]);
        assert_eq!(t[2].state, State::Error);
        assert_eq!(default_active(&t), Some(Key::Stmt(2)));
    }

    #[test]
    fn commands_fold_into_messages() {
        let stmts = [
            ("BEGIN", o(State::Done, Some(false), None)),
            ("CREATE TEMP TABLE t AS SELECT 1 AS x", o(State::Done, Some(false), Some(1))),
            ("SET work_mem = '64MB'", o(State::Done, None, None)),
            ("SELECT x FROM t", o(State::Done, Some(true), None)),
            ("COMMIT", o(State::Running, None, None)),
        ];
        let t = tabs(&stmts);
        assert_eq!(labels(&t), ["4 · t", "messages"]);
        assert_eq!(t[1].folded, 4);
        assert_eq!(t[1].state, State::Running, "a folded command is running");
        assert_eq!(default_active(&t), Some(Key::Stmt(3)));
        let only = tabs(&stmts[..3]);
        assert_eq!(labels(&only), ["messages"]);
        assert_eq!(default_active(&only), Some(Key::Messages));
    }

    #[test]
    fn printed_messages() {
        let t = tabs(&[("PRINT 'x'", printed(State::Done, None))]);
        assert_eq!(labels(&t), ["1 · PRINT"]);
        let t = tabs(&[("WHILE 1 = 1 PRINT 'x'", printed(State::Running, None))]);
        assert_eq!(labels(&t), ["1 · WHILE"]);
        let t = tabs(&[("PRINT 'a'; SELECT * FROM t", printed(State::Done, Some(true)))]);
        assert_eq!(labels(&t), ["1 · t", "messages"]);
        assert_eq!((t[1].folded, t[1].state, default_active(&t)), (0, State::Done, Some(Key::Stmt(0))));
        let t = tabs(&[("PRINT 'a'; SELECT 1 / 0", printed(State::Error, None))]);
        assert_eq!(labels(&t)[1], "messages");
        assert_eq!(default_active(&t), Some(Key::Stmt(0)), "the error first");
    }

    #[test]
    fn a_single_statement_never_folds() {
        let t = tabs(&[("CREATE TABLE t (x int)", o(State::Done, Some(false), None))]);
        assert_eq!(labels(&t), ["1 · CREATE TABLE"]);
        let t = tabs(&[("SET x = 1", o(State::Done, Some(false), Some(0)))]);
        assert_eq!(labels(&t), ["1 · SET"], "no count for a command");
        assert_eq!(t[0].kind, Kind::Affected);
    }

    #[test]
    fn running_paused_partial_and_cancelled() {
        let stmts = [
            ("SELECT * FROM big", o(State::Partial, Some(true), Some(1_000_000))),
            ("CREATE INDEX ON big (x)", o(State::Cancelled, Some(false), None)),
            ("SELECT * FROM big2", o(State::Paused, Some(true), None)),
            ("SELECT * FROM big3", o(State::Running, None, None)),
        ];
        let t = tabs(&stmts);
        assert_eq!(labels(&t), ["1 · big", "2 · CREATE INDEX", "3 · big2", "4 · big3"]);
        assert_eq!(t.iter().map(|t| t.state).collect::<Vec<_>>(), [State::Partial, State::Cancelled, State::Paused, State::Running]);
        assert_eq!(t[1].kind, Kind::Cancelled, "a cancelled command keeps a tab");
        assert_eq!(default_active(&t), Some(Key::Stmt(3)), "follows the running statement");
    }

    #[test]
    fn picking_and_cycling() {
        let stmts = [
            ("SELECT * FROM a", o(State::Done, Some(true), None)),
            ("SET x = 1", o(State::Done, Some(false), None)),
            ("SELECT * FROM b", o(State::Done, Some(true), None)),
        ];
        let t = tabs(&stmts);
        assert_eq!(active(&t, None), Some(Key::Stmt(2)));
        assert_eq!(active(&t, Some(Key::Stmt(0))), Some(Key::Stmt(0)));
        assert_eq!(active(&t, Some(Key::Stmt(1))), Some(Key::Stmt(2)), "a pick that has no tab falls back");
        assert_eq!(cycle(&t, Some(Key::Stmt(2)), true), Some(Key::Messages));
        assert_eq!(cycle(&t, Some(Key::Messages), true), Some(Key::Stmt(0)), "wraps");
        assert_eq!(cycle(&t, Some(Key::Stmt(0)), false), Some(Key::Messages));
        assert_eq!(cycle(&t, None, true), Some(Key::Stmt(0)));
        assert_eq!(cycle(&[], None, true), None);
    }

    #[test]
    fn long_labels_are_cut() {
        let t = tabs(&[("SELECT * FROM a_really_long_table_name_for_reports", o(State::Done, Some(true), None))]);
        assert_eq!(t[0].label.chars().count(), LABEL_MAX);
        assert!(t[0].label.ends_with('…'));
    }

    #[test]
    fn views_stay_with_their_result() {
        let (a, b, c) = (ResultSet::new("a"), ResultSet::new("b"), ResultSet::new("c"));
        let mut v: Views<&str> = Views::default();
        v.put(Key::Stmt(0), a.clone(), "sel a");
        v.put(Key::Stmt(1), b.clone(), "sel b");
        assert_eq!(v.take(Key::Stmt(1), &c), None, "another result under the same key starts fresh");
        assert_eq!(v.take(Key::Stmt(0), &a), Some("sel a"));
        assert_eq!(v.take(Key::Stmt(0), &a), None, "taken");
        v.put(Key::Stmt(0), a.clone(), "sel a2");
        v.put(Key::Stmt(2), c.clone(), "sel c");
        *v.of_result(&c).unwrap() = "late fk";
        assert_eq!(v.len(), 2);
        v.remove(Key::Stmt(2));
        assert_eq!(v.len(), 1);
        v.clear();
        assert!(v.is_empty());
    }
}
