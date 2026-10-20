use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use dataman_core::batch::Key;
use gpui::{AnyElement, App, EntityId, Hsla, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder};

use crate::motion::{self, Fold};
use dataman_core::groups::Pane;
use crate::theme::{Theme, metrics, px};
use crate::ui;

pub(crate) struct Gone {
    pub pane: Pane,
    pub after: Option<EntityId>,
    pub width: f32,
    pub title: SharedString,
    pub color: Hsla,
    pub front: bool,
}

pub(crate) struct GoneResult {
    pub tab: EntityId,
    pub after: Option<Key>,
    pub width: f32,
    pub label: SharedString,
    pub front: bool,
}

#[derive(Default)]
pub(crate) struct TabMotion {
    pub live: bool,
    born: HashMap<EntityId, Fold>,
    natural: Rc<RefCell<HashMap<EntityId, f32>>>,
    gone: Vec<(Gone, Fold)>,
    results_gone: Vec<(GoneResult, Fold)>,
    gaps: Vec<(Pane, usize, Fold)>,
    lifted: Option<(EntityId, f32, Fold)>,
}

impl TabMotion {
    pub fn opened(&mut self, id: EntityId, cx: &App) {
        if self.live && !cx.reduce_motion() {
            self.born.insert(id, Fold::new(0., 1., motion::TAB_OPEN));
        }
    }

    pub fn closed(&mut self, id: EntityId, g: Gone, cx: &App) {
        self.born.remove(&id);
        if self.live && !cx.reduce_motion() && g.width > 0. {
            self.gone.push((g, Fold::new(1., 0., motion::TAB_CLOSE)));
        }
    }

    pub fn result_closed(&mut self, g: GoneResult, cx: &App) {
        if self.live && !cx.reduce_motion() && g.width > 0. {
            self.results_gone.push((g, Fold::new(1., 0., motion::TAB_CLOSE)));
        }
    }

    pub fn follow_drop(&mut self, dragged: EntityId, width: impl FnOnce() -> f32, at: Option<(Pane, usize)>, cx: &App) {
        let now = Instant::now();
        let time = motion::timed(motion::TAB_GAP, cx);
        for (p, s, f) in &mut self.gaps {
            f.turn(if Some((*p, *s)) == at { 1. } else { 0. }, now);
        }
        if let Some((p, s)) = at
            && !self.gaps.iter().any(|g| (g.0, g.1) == (p, s))
        {
            self.gaps.push((p, s, Fold::new(0., 1., time)));
        }
        if self.lifted.as_ref().is_none_or(|l| l.0 != dragged) {
            self.lifted = Some((dragged, width(), Fold::new(1., 1., time)));
        }
        if let Some((_, _, f)) = &mut self.lifted {
            f.turn(if at.is_some() { 0. } else { 1. }, now);
        }
    }

    pub fn landed(&mut self) {
        self.gaps.clear();
        self.lifted = None;
    }

    pub fn drag_ended(&mut self) {
        if self.gaps.is_empty() && self.lifted.is_none() {
            return;
        }
        let now = Instant::now();
        for (_, _, f) in &mut self.gaps {
            f.turn(0., now);
        }
        if let Some((_, _, f)) = &mut self.lifted {
            f.turn(1., now);
        }
    }

    pub fn prune(&mut self, now: Instant) -> bool {
        self.born.retain(|_, f| !f.done(now));
        self.natural.borrow_mut().retain(|id, _| self.born.contains_key(id));
        self.gone.retain(|g| !g.1.done(now));
        self.results_gone.retain(|g| !g.1.done(now));
        self.gaps.retain(|(_, _, f)| !(f.done(now) && f.target() == 0.));
        if self.lifted.as_ref().is_some_and(|l| l.2.done(now) && l.2.target() == 1.) && self.gaps.is_empty() {
            self.lifted = None;
        }
        !self.born.is_empty()
            || !self.gone.is_empty()
            || !self.results_gone.is_empty()
            || self.gaps.iter().any(|g| !g.2.done(now))
            || self.lifted.as_ref().is_some_and(|l| !l.2.done(now))
    }

    pub fn gap_target(&self) -> Option<(Pane, usize)> {
        self.gaps.iter().find(|g| g.2.target() == 1.).map(|g| (g.0, g.1))
    }

    pub fn growing(&self, id: EntityId) -> bool {
        self.born.contains_key(&id)
    }

    pub fn tab(&self, id: EntityId, el: AnyElement, now: Instant) -> AnyElement {
        let (k, width) = match (&self.lifted, self.born.get(&id)) {
            (Some((l, w, f)), _) if *l == id => (f.at(now), Some(*w)),
            (_, Some(f)) => (f.at(now), None),
            _ => return el,
        };
        if k >= 1. {
            return el;
        }
        let natural = self.natural.clone();
        let w = width.or_else(|| natural.borrow().get(&id).copied()).unwrap_or(0.);
        div()
            .flex_none()
            .flex()
            .flex_row()
            .w(px(w * k))
            .overflow_hidden()
            .opacity(k)
            .on_children_prepainted(move |b, _, _| {
                if let Some(b) = b.first() {
                    natural.borrow_mut().insert(id, b.size.width.as_f32() / crate::theme::zoom());
                }
            })
            .child(div().flex_none().flex().child(el))
            .into_any_element()
    }

    pub fn ghosts(&self, pane: Pane, after: Option<EntityId>, t: &Theme, now: Instant) -> impl Iterator<Item = AnyElement> + '_ {
        let t = t.clone();
        self.gone.iter().filter(move |(g, _)| g.pane == pane && g.after == after).map(move |(g, f)| ghost(g, f.at(now), &t))
    }

    pub fn stray_ghosts(&self, pane: Pane, ids: &[EntityId], t: &Theme, now: Instant) -> Vec<AnyElement> {
        self.gone.iter().filter(|(g, _)| g.pane == pane && g.after.is_some_and(|a| !ids.contains(&a))).map(|(g, f)| ghost(g, f.at(now), t)).collect()
    }

    pub fn gap(&self, pane: Pane, slot: usize, t: &Theme, now: Instant) -> Option<AnyElement> {
        let (_, _, f) = self.gaps.iter().find(|g| (g.0, g.1) == (pane, slot))?;
        let k = f.at(now);
        let w = self.lifted.as_ref().map_or(120., |l| l.1);
        (k > 0.).then(|| {
            div()
                .flex_none()
                .h(px(metrics::TAB_H))
                .w(px(w * k))
                .opacity(k)
                .child(div().size_full().bg(t.acc_s).border_1().border_color(t.acc.opacity(0.45)))
                .into_any_element()
        })
    }

    pub fn result_ghosts(&self, tab: EntityId, after: Option<Key>, t: &Theme, now: Instant) -> Vec<AnyElement> {
        self.results_gone
            .iter()
            .filter(|(g, _)| g.tab == tab && g.after == after)
            .map(|(g, f)| {
                let k = f.at(now);
                div()
                    .flex_none()
                    .h(px(22.))
                    .w(px(g.width * k))
                    .overflow_hidden()
                    .opacity(k)
                    .child(
                        div()
                            .w(px(g.width))
                            .h_full()
                            .pl(px(8.))
                            .flex()
                            .items_center()
                            .text_color(t.tx2)
                            .when(g.front, |d| d.bg(t.prs).text_color(t.tx))
                            .child(g.label.clone()),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

fn ghost(g: &Gone, k: f32, t: &Theme) -> AnyElement {
    div()
        .flex_none()
        .h(px(metrics::TAB_H))
        .w(px(g.width * k))
        .overflow_hidden()
        .opacity(k)
        .child(
            div()
                .w(px(g.width))
                .h_full()
                .pl(px(10.))
                .pr(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.))
                .text_color(t.tx3)
                .when(g.front, |d| d.bg(t.prs).text_color(t.tx))
                .child(ui::chip(g.color, 8.))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(g.title.clone())),
        )
        .into_any_element()
}
