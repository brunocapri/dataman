//! The cell viewer (⇧Space, the result bar's button): each editor group has its own, at the
//! group's right side, showing the cell selected in that group's grid — JSON pretty-printed,
//! the full text, bytes as hex (`Workspace::render_viewer`). It is part of its group, not a dock
//! panel: it never moves elsewhere. It slides open and shut like the explorer
//! (`motion::WidthTween`; with Reduce Motion it is simply there or gone) and its left edge
//! resizes it, leaving its group's editor and results at least [`MAIN_MIN`].
//!
//! Its state is the group's: when the split closes, the group that stays keeps its viewer.

use std::time::Instant;

use dataman_core::groups::Pane;
use gpui::{AnyElement, Context, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled, Window, div, prelude::FluentBuilder};

use crate::app::{Drag, Workspace};
use crate::motion::WidthTween;
use crate::tab::QueryTab;
use crate::theme::{ActiveTheme, Pt, metrics, px};

/// The least width a group keeps for its editor and results beside its viewer.
pub const MAIN_MIN: f32 = 220.;
/// The grabber along the viewer's left edge (centred on its hairline).
const EDGE_HIT: f32 = 8.;

/// One group's viewer: open or not, the width it opens to, and its slide.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CellViewer {
    pub open: bool,
    pub w: f32,
    tween: Option<WidthTween>,
}

impl Default for CellViewer {
    fn default() -> CellViewer {
        CellViewer { open: false, w: metrics::SIDE_W, tween: None }
    }
}

impl CellViewer {
    fn target(&self) -> f32 {
        if self.open { self.w } else { 0. }
    }

    /// Its width now (0 shut), sliding or not.
    pub fn now(&self) -> f32 {
        self.tween.and_then(|t| t.at(self.target(), Instant::now())).unwrap_or(self.target())
    }

    pub fn sliding(&self) -> bool {
        self.tween.is_some_and(|t| t.at(self.target(), Instant::now()).is_some())
    }
}

impl Workspace {
    /// Opens or shuts group `pane`'s viewer, sliding from wherever it is now.
    pub(crate) fn toggle_viewer(&mut self, pane: Pane, cx: &mut Context<Self>) {
        let reduced = cx.reduce_motion();
        let v = &mut self.viewers[pane.index()];
        let from = v.now();
        v.open = !v.open;
        v.tween = (!reduced).then(|| WidthTween::panel(from, v.target(), metrics::SIDE_W));
        cx.notify();
    }

    pub(crate) fn viewer_open(&self, pane: Pane) -> bool {
        self.viewers[pane.index()].open
    }

    /// The viewer's edge dragged: its width, within the side panels' bounds.
    pub(crate) fn resize_viewer(&mut self, pane: Pane, w: f32) {
        self.viewers[pane.index()].w = w.clamp(metrics::SIDE_MIN, metrics::SIDE_MAX);
    }

    /// The viewers' state follows the group that stays when the split closes: from now on it is
    /// the only (left) group.
    pub(crate) fn viewer_survives(&mut self, survivor: Pane) {
        if survivor == Pane::Right {
            self.viewers[0] = self.viewers[1];
        }
        self.viewers[1] = CellViewer::default();
    }

    /// The width group `pane`'s viewer takes this frame in a group `group_w` points wide (it
    /// leaves the group [`MAIN_MIN`]); asks for the next frame while it slides. 0: shut.
    pub(crate) fn viewer_width(&mut self, pane: Pane, group_w: f32, window: &mut Window) -> (f32, f32) {
        let v = &mut self.viewers[pane.index()];
        let room = (group_w - MAIN_MIN).max(0.);
        let now = match v.tween.map(|t| t.at(v.target(), Instant::now())) {
            Some(Some(w)) => {
                window.request_animation_frame();
                w
            }
            Some(None) => {
                v.tween = None;
                v.target()
            }
            None => v.target(),
        };
        (now.min(room), v.w.min(room))
    }

    /// Group `pane`'s viewer for `tab` (its tab in front), `w` wide this frame and `full` once
    /// open: the content keeps its full width while it slides, clipped, and the left edge
    /// resizes it.
    pub(crate) fn render_group_viewer(&self, tab: &Entity<QueryTab>, pane: Pane, w: f32, full: f32, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        div()
            .id(("cell-viewer", pane.index()))
            .relative()
            .flex_none()
            .h_full()
            .w(px(w))
            .border_l_1()
            .border_color(t.line)
            .child(div().absolute().top_0().bottom_0().left_0().w(px(full)).overflow_hidden().child(self.render_viewer(tab, &t, cx)))
            .when(w >= full - 0.5, |d| {
                d.child(
                    div()
                        .id(("cell-viewer-edge", pane.index()))
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-EDGE_HIT / 2.))
                        .w(px(EDGE_HIT))
                        .cursor_col_resize()
                        .on_mouse_down(MouseButton::Left, cx.listener(move |w, ev: &gpui::MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            w.drag = Some(Drag::Viewer { pane, x0: ev.position.x.pt(), w0: full });
                        })),
                )
            })
            .into_any_element()
    }
}
