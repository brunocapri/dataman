use std::cell::RefCell;

use gpui::{EntityId, FollowMode, ListAlignment, ListState};
use dataman_core::ResultSet;
use dataman_core::result::Phase;

use crate::tab::BatchItem;
use crate::theme::px;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    Stmt(usize),
    Line(usize, usize),
    More(usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Of {
    Tab(EntityId),
    Log(usize),
}

impl Of {
    pub(crate) fn log(rs: &ResultSet) -> Of {
        Of::Log(rs as *const ResultSet as usize)
    }
}

pub(crate) struct Messages {
    pub(crate) list: ListState,
    rows: RefCell<(Option<Of>, Vec<Row>)>,
}

impl Messages {
    pub(crate) fn new() -> Messages {
        Messages { list: ListState::new(0, ListAlignment::Top, px(600.)), rows: RefCell::new((None, Vec::new())) }
    }

    pub(crate) fn sync(&self, of: Of, rows: Vec<Row>, running: bool) {
        let mut g = self.rows.borrow_mut();
        if g.0 != Some(of) {
            self.list.reset(rows.len());
            self.list.set_follow_mode(if running { FollowMode::Tail } else { FollowMode::Normal });
        } else {
            let same = g.1.iter().zip(&rows).take_while(|(a, b)| a == b).count();
            if same < g.1.len() || same < rows.len() {
                self.list.splice(same..g.1.len(), rows.len() - same);
            }
        }
        *g = (Some(of), rows);
    }

    pub(crate) fn row(&self, ix: usize) -> Option<Row> {
        self.rows.borrow().1.get(ix).copied()
    }

    pub(crate) fn of(&self) -> Option<Of> {
        self.rows.borrow().0
    }
}

fn lines(i: usize, rs: &ResultSet, out: &mut Vec<Row>) {
    let (m, more) = rs.messages();
    out.extend((0..m.len()).map(|j| Row::Line(i, j)));
    if more > 0 {
        out.push(Row::More(i, more));
    }
}

pub(crate) fn tab_rows(batch: &[BatchItem]) -> Vec<Row> {
    let mut out = Vec::with_capacity(batch.len());
    for (i, b) in batch.iter().enumerate() {
        out.push(Row::Stmt(i));
        if let Some(rs) = &b.rs {
            lines(i, rs, &mut out);
        }
    }
    out
}

pub(crate) fn log_rows(i: usize, rs: &ResultSet) -> Vec<Row> {
    let mut out = Vec::new();
    lines(i, rs, &mut out);
    out
}

pub(crate) fn shows_log(rs: &ResultSet) -> bool {
    rs.has_messages() && !rs.columns().is_some_and(|c| !c.is_empty()) && rs.phase() != Phase::Failed
}

pub(crate) fn text<'a>(results: impl Iterator<Item = &'a ResultSet>) -> String {
    let mut out = String::new();
    for rs in results {
        for m in rs.messages().0.iter() {
            out.push_str(m);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_follow_the_run() {
        let a = ResultSet::new("PRINT 'a'; SELECT 1");
        a.push_message("a".into());
        a.push_message("b".into());
        let item = |rs: Option<std::sync::Arc<ResultSet>>| BatchItem {
            at: 0,
            sql: "x".into(),
            state: crate::tab::ItemState::Ok,
            elapsed: None,
            rs,
            params: Vec::new(),
            rollback: false,
            label: None,
        };
        let batch = [item(Some(a.clone())), item(None)];
        assert_eq!(tab_rows(&batch), [Row::Stmt(0), Row::Line(0, 0), Row::Line(0, 1), Row::Stmt(1)]);
        assert_eq!(log_rows(3, &a), [Row::Line(3, 0), Row::Line(3, 1)]);
        assert!(shows_log(&a), "no columns yet");
        assert_eq!(text([&*a].into_iter()), "a\nb\n");
        assert!(!shows_log(&ResultSet::new("SELECT 1")));
    }
}
