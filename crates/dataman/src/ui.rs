use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyView, App, Bounds, Context, Div, ElementId, FontWeight, Hsla, IntoElement, ParentElement, Pixels, Render, SharedString,
    Stateful, Styled, Window, div, prelude::*,
};
use crate::theme::px;

use crate::assets::{Icon, icon};
use crate::theme::{ActiveTheme, BANNER, Theme};

pub fn keycap(t: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .h(px(18.))
        .px(px(5.))
        .flex()
        .items_center()
        .bg(t.key)
        .border_1()
        .border_color(t.line)
        .border_b_1()
        .text_size(px(10.5))
        .line_height(px(16.))
        .text_color(t.tx2)
        .child(keys_text(text))
}

/// Key text as this platform names keys (`keymap::glyphs`).
pub fn keys_text(text: impl Into<SharedString>) -> SharedString {
    let s = text.into();
    if crate::keymap::NOT_MAC { crate::keymap::glyphs(&s).into() } else { s }
}

pub fn keycap_sm(bg: Hsla, fg: Hsla, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .h(px(17.))
        .px(px(4.))
        .flex()
        .items_center()
        .bg(bg)
        .text_size(px(10.))
        .line_height(px(14.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(keys_text(text))
}

pub fn key_hint(text: impl Into<SharedString>) -> Div {
    div().flex_none().text_size(px(10.5)).opacity(0.7).child(keys_text(text))
}

pub fn chip(color: Hsla, size: f32) -> Div {
    div()
        .flex_none()
        .size(px(size))
        .bg(color)
        .border_1()
        .border_color(gpui::hsla(0., 0., 0., 0.25))
}

pub fn find_ci(text: &str, q: &str) -> Option<std::ops::Range<usize>> {
    if q.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        return None;
    }
    let at = lower.find(q)?;
    Some(at..at + q.len())
}

pub fn marked(t: &Theme, text: String, q: &str) -> Div {
    let row = div().min_w_0().flex().flex_row().overflow_hidden().whitespace_nowrap();
    match find_ci(&text, q) {
        Some(h) => row
            .child(div().flex_none().child(text[..h.start].to_string()))
            .child(div().flex_none().bg(t.mark).text_color(t.tx).border_b_1().border_color(t.mark_b).child(text[h.clone()].to_string()))
            .child(div().min_w_0().overflow_hidden().text_ellipsis().child(text[h.end..].to_string())),
        None => row.child(div().min_w_0().overflow_hidden().text_ellipsis().child(text)),
    }
}

pub fn caps(t: &Theme, text: impl Into<SharedString>) -> Div {
    div().flex_none().text_size(px(10.5)).text_color(t.tx3).child(text.into())
}

pub fn banner(text: impl Into<SharedString>, size: f32, color: Hsla) -> Div {
    div().font_family(BANNER).text_size(px(size)).line_height(px(size * 1.15)).text_color(color).child(text.into())
}

pub fn not_reported(t: &Theme) -> Div {
    div()
        .flex_none()
        .h(px(15.))
        .px(px(4.))
        .flex()
        .items_center()
        .border_1()
        .border_dashed()
        .border_color(t.line2)
        .text_color(t.tx3)
        .text_size(px(10.))
        .child("not reported")
}

/// A result's state: glyph and label in its color, no badge behind them.
pub fn status(glyph: &str, label: impl Into<SharedString>, fg: Hsla) -> Div {
    div()
        .flex_none()
        .h(px(20.))
        .flex()
        .items_center()
        .gap(px(6.))
        .text_color(fg)
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .child(div().text_size(px(10.)).child(glyph_el(glyph, fg)))
        .child(label.into())
}

/// A state glyph. Pause is drawn: the font's ‖ runs from ascender to descender and is narrow.
pub fn glyph_el(g: &str, color: Hsla) -> gpui::AnyElement {
    if g == PAUSE { pause_mark(color).into_any_element() } else { SharedString::from(g.to_string()).into_any_element() }
}

pub const PAUSE: &str = "‖";

/// Two bars, shorter and wider than the font's ‖.
pub fn pause_mark(color: Hsla) -> Div {
    let bar = || div().w(px(2.)).h(px(7.)).bg(color);
    div().flex_none().flex().flex_row().items_center().gap(px(2.5)).child(bar()).child(bar())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

pub fn button(t: &Theme, id: impl Into<ElementId>, kind: Btn) -> Stateful<Div> {
    let id = id.into();
    let base = div()
        .children(here(&id))
        .id(id)
        .flex_none()
        .h(px(26.))
        .px(px(9.))
        .flex()
        .items_center()
        .gap(px(7.))
        .text_size(px(12.))
        .whitespace_nowrap()
        .cursor_pointer();
    match kind {
        Btn::Primary => {
            let (h, p) = (t.acc_h, t.acc_p);
            base.bg(t.acc).text_color(t.on_acc).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
        Btn::Secondary => {
            let (h, p) = (t.prs, t.line2);
            base.bg(t.hov).border_1().border_color(t.line2).text_color(t.tx).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
        Btn::Ghost => {
            let (h, p) = (t.hov, t.prs);
            base.text_color(t.tx2).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
        Btn::Danger => {
            let (h, p) = (t.err.opacity(0.22), t.err.opacity(0.30));
            base.bg(t.err_s).border_1().border_color(t.err).text_color(t.err).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
    }
}

/// "Copy SQL" and the like: the copy icon and its label, then "Copied" with a check in the same
/// place for a moment, as the value viewer's Copy. `copied` is that moment's opacity
/// ([`CopiedFlash::on`]); the button stays clickable under it.
pub fn copy_button(t: &Theme, id: impl Into<ElementId>, kind: Btn, label: impl Into<SharedString>, copied: Option<f32>) -> Stateful<Div> {
    let id = id.into();
    let done: ElementId = SharedString::from(format!("{id}-copied")).into();
    let fg = if kind == Btn::Primary { t.on_acc } else { t.tx2 };
    button(t, id, kind)
        .relative()
        .child(div().flex().flex_row().items_center().gap(px(5.)).when(copied.is_some(), |d| d.invisible()).child(icon(Icon::Copy).size(px(13.)).text_color(fg)).child(label.into()))
        .children(copied.map(|a| {
            div()
                .children(here(&done))
                .absolute()
                .inset_0()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(4.))
                .text_color(t.ok)
                .opacity(a)
                .child(icon(Icon::Check).size(px(12.)).text_color(t.ok))
                .child("Copied")
        }))
}

/// Which copy button says "Copied", and since when; `motion::copied` holds it and fades it.
#[derive(Default)]
pub struct CopiedFlash {
    at: Option<(SharedString, std::time::Instant)>,
    _redraws: Option<gpui::Task<()>>,
}

impl CopiedFlash {
    /// `id`'s moment starts now (another button's ends), redrawing `V` while it holds and fades.
    pub fn start<V: 'static>(&mut self, id: impl Into<SharedString>, cx: &mut Context<V>) {
        let start = std::time::Instant::now();
        self.at = Some((id.into(), start));
        let redraws = crate::motion::copied_redraws(cx.reduce_motion());
        self._redraws = Some(cx.spawn(async move |v, cx| {
            for at in redraws {
                cx.background_executor().timer(at.saturating_sub(start.elapsed())).await;
                if v.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        }));
        cx.notify();
    }

    /// `id`'s "Copied" opacity while it shows.
    pub fn on(&self, id: &str, cx: &App) -> Option<f32> {
        self.at.as_ref().filter(|(at, _)| at == id).and_then(|(_, since)| crate::motion::copied(*since, std::time::Instant::now(), cx.reduce_motion()))
    }
}

pub fn icon_button(t: &Theme, id: impl Into<ElementId>, i: Icon, box_size: f32, icon_size: f32) -> Stateful<Div> {
    let (hov, prs) = (t.hov, t.prs);
    icon_button_inert(t, id, i, box_size, icon_size).cursor_pointer().hover(move |s| s.bg(hov)).active(move |s| s.bg(prs))
}

/// [`icon_button`] without its hover and press tints (a disabled one: GPUI takes one hover
/// style per element, so it cannot be cleared after).
pub fn icon_button_inert(t: &Theme, id: impl Into<ElementId>, i: Icon, box_size: f32, icon_size: f32) -> Stateful<Div> {
    let id = id.into();
    div()
        .children(here(&id))
        .id(id)
        .flex_none()
        .size(px(box_size))
        .flex()
        .items_center()
        .justify_center()
        .text_color(t.tx3)
        .child(icon(i).size(px(icon_size)).text_color(t.tx3))
}

/// Not a spread `BoxShadow`: GPUI paints a shadow under the whole box, which would also tint
/// a translucent field's inside.
pub fn halo_ring(color: Hsla) -> Div {
    // Absolute insets count from inside the field's border (1 pt at any zoom, like every
    // hairline); the band itself scales with the UI.
    let o = px(-3.) - gpui::px(1.);
    div().absolute().top(o).left(o).right(o).bottom(o).border(px(3.)).border_color(color)
}

/// A text field's outline, the explorer search's: gray, darker while it has the focus. Never the
/// accent.
pub fn field_line(t: &Theme, focused: bool) -> Hsla {
    if focused { t.mark_b } else { t.field_b }
}

/// Every text field's frame: its outline, with a gray halo while it has the focus.
pub fn field_frame<E: Styled + ParentElement>(e: E, t: &Theme, focused: bool) -> E {
    let e = e.relative().border_1().border_color(field_line(t, focused));
    if focused { e.child(halo_ring(t.mark)) } else { e }
}

pub fn vsep(t: &Theme, h: f32) -> Div {
    div().flex_none().w(px(1.)).h(px(h)).bg(t.line2)
}

pub fn checkbox(t: &Theme, on: bool) -> Div {
    div()
        .flex_none()
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .when(on, |d| d.bg(t.acc).child(icon(Icon::Check).size(px(11.)).text_color(t.on_acc)))
        .when(!on, |d| d.bg(t.field).border_1().border_color(t.line2))
}

/// The on/off switch's look: the knob slides and the track fills as it turns. `id` keys the motion.
pub fn switch_track(t: &Theme, id: impl Into<SharedString>, on: bool) -> gpui::AnyElement {
    use gpui::{AnimationExt, SpringAnimation};
    let (fill, edge, knob_on, knob_off) = (t.tx2, t.field_b, t.pop, t.tx3);
    let id: SharedString = id.into();
    div()
        .flex_none()
        .relative()
        .w(px(28.))
        .h(px(16.))
        .with_spring(ElementId::Name(format!("{id}-knob").into()), SpringAnimation::new(crate::motion::TURN).to(on), move |d, k| {
            let k = k.0.clamp(0., 1.);
            let knob = |c: Hsla, a: f32| div().absolute().top(px(3.)).left(px(3. + 12. * k)).size(px(10.)).bg(c).opacity(a);
            d.child(div().absolute().inset_0().border_1().border_color(edge).opacity(1. - k))
                .child(div().absolute().inset_0().bg(fill).opacity(k))
                .child(knob(knob_off, 1. - k))
                .child(knob(knob_on, k))
        })
        .into_any_element()
}

/// An on/off switch to click. Checks find it as `id`, and as `{id}-on` while it is off and
/// `{id}-off` while it is on (what a click there does).
pub fn switch(t: &Theme, id: impl Into<SharedString>, on: bool) -> Stateful<Div> {
    let id: SharedString = id.into();
    let action: ElementId = SharedString::from(format!("{id}-{}", if on { "off" } else { "on" })).into();
    div()
        .children(here(&action))
        .children(here(&ElementId::Name(id.clone())))
        .id(ElementId::Name(id.clone()))
        .flex_none()
        .cursor_pointer()
        .child(switch_track(t, id, on))
}

pub fn segmented(t: &Theme) -> Div {
    div().flex().flex_row().h(px(28.)).p(px(2.)).bg(t.field).border_1().border_color(t.line2)
}

pub fn segment(t: &Theme, id: impl Into<ElementId>, label: impl Into<SharedString>, on: bool) -> Stateful<Div> {
    let hov = t.hov;
    let id = id.into();
    div()
        .children(here(&id))
        .id(id)
        .flex_1()
        .px(px(10.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.5))
        .cursor_pointer()
        .when(on, |d| d.bg(t.pop).text_color(t.tx).border_1().border_color(t.line2))
        .when(!on, |d| d.text_color(t.tx3).hover(move |s| s.bg(hov)))
        .child(label.into())
}

pub fn disclosure(id: impl Into<ElementId>, open: bool, size: f32, color: Hsla) -> gpui::AnyElement {
    use gpui::{AnimationExt, SpringAnimation, Transformation, radians};
    crate::assets::icon(crate::assets::Icon::ChevronRight)
        .size(px(size))
        .text_color(color)
        .with_spring(id, SpringAnimation::new(crate::motion::TURN).to(open), |svg, k| {
            svg.with_transformation(Transformation::rotate(radians(k.0 * std::f32::consts::FRAC_PI_2)))
        })
        .into_any_element()
}

/// A disclosure chevron that turns at once.
pub fn disclosure_still(open: bool, size: f32, color: Hsla) -> gpui::AnyElement {
    crate::assets::icon(if open { crate::assets::Icon::ChevronDown } else { crate::assets::Icon::ChevronRight }).size(px(size)).text_color(color).into_any_element()
}

pub fn dropdown_arrow(id: impl Into<ElementId>, open: bool, size: f32, color: Hsla) -> gpui::AnyElement {
    use gpui::{AnimationExt, SpringAnimation, Transformation, radians};
    crate::assets::icon(crate::assets::Icon::ChevronDown)
        .size(px(size))
        .text_color(color)
        .with_spring(id, SpringAnimation::new(crate::motion::TURN).to(open), |svg, k| {
            svg.with_transformation(Transformation::rotate(radians(k.0 * std::f32::consts::PI)))
        })
        .into_any_element()
}

pub fn menu_in(id: impl Into<ElementId>, menu: impl IntoElement + 'static) -> gpui::AnyElement {
    use gpui::{Animation, AnimationExt};
    div()
        .relative()
        .child(menu)
        .with_animation(id, Animation::new(crate::motion::MENU_IN).with_easing(gpui::ease_out_quint()), |d, k| {
            d.opacity(k).top(px(-4. * (1. - k)))
        })
        .into_any_element()
}

pub fn folder_icon(open: bool, fill: Option<f32>, size: f32, color: Hsla) -> gpui::AnyElement {
    match fill {
        Some(level) => flood_icon(Icon::Schema, Icon::SchemaFilled, level, size, color),
        None => icon(if open { Icon::Schema } else { Icon::SchemaFilled }).size(px(size)).text_color(color).into_any_element(),
    }
}

/// An outline icon with its filled twin rising from the bottom, `level` of the way up.
pub fn flood_icon(outline: Icon, filled: Icon, level: f32, size: f32, color: Hsla) -> gpui::AnyElement {
    let filled = div()
        .absolute()
        .left_0()
        .bottom_0()
        .w(px(size))
        .h(px(size * level))
        .overflow_hidden()
        .child(icon(filled).absolute().left_0().bottom_0().size(px(size)).text_color(color));
    div().relative().flex_none().size(px(size)).child(icon(outline).size(px(size)).text_color(color)).child(filled).into_any_element()
}

pub fn popover(t: &Theme) -> Div {
    div().bg(t.pop).shadow(t.ring()).p(px(4.)).occlude()
}

pub fn menu_item(
    t: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    key: Option<&str>,
    enabled: bool,
) -> Stateful<Div> {
    let (acc, on_acc) = (t.acc, t.on_acc);
    let id = id.into();
    div()
        .children(here(&id))
        .id(id)
        .h(px(26.))
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(10.))
        .text_size(px(12.))
        .text_color(if enabled { t.tx } else { t.tx4 })
        .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(acc).text_color(on_acc)))
        .child(div().flex_1().child(label.into()))
        .children(key.map(|k| div().text_size(px(11.)).opacity(0.7).child(k.to_string())))
}

/// A list's selected row, the explorer's: a muted blue wash and hairline while its list has the
/// focus, gray when it has not. The hairline is always there (clear unless selected), so selecting
/// a row never moves what it holds.
pub fn selectable<E: Styled>(e: E, t: &Theme, sel: bool, focused: bool) -> E {
    let e = e.border_1().border_color(if sel && focused { t.edge_acc } else { gpui::transparent_black() });
    if sel { e.bg(sel_bg(t, focused)) } else { e }
}

/// The same row with the wash alone, no blue hairline: the Agents area's lists.
pub fn selectable_flat<E: Styled>(e: E, t: &Theme, sel: bool, focused: bool) -> E {
    let e = e.border_1().border_color(gpui::transparent_black());
    if sel { e.bg(sel_bg(t, focused)) } else { e }
}

/// The selected row's wash alone, for a row whose border is taken (a separator line).
pub fn sel_bg(t: &Theme, focused: bool) -> Hsla {
    if focused { t.acc_s } else { t.prs }
}

pub fn menu_sep(t: &Theme) -> Div {
    div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(t.line)
}

/// Every tooltip in the app goes through here: it stays while the pointer is on it, so it can be
/// read through, and goes once the pointer has left the control and the tooltip both (after a
/// short grace to cross from one to the other). Right after one was shown, the next shows at once.
pub trait Tipped: StatefulInteractiveElement + ParentElement {
    fn tip(self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        let anchor: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::default();
        let at = anchor.clone();
        let warm = TIP_SEEN.with(Cell::get).is_some_and(|seen| seen.elapsed() < TIP_WARM);
        self.child(gpui::canvas(move |b, _, _| at.set(Some(b)), |_, _, _, _| {}).absolute().top_0().left_0().size_full())
            .tooltip_show_delay(if warm { Duration::ZERO } else { TIP_SHOW })
            .hoverable_tooltip(move |window, cx| {
                let inner = build(window, cx);
                let anchor = anchor.clone();
                cx.new(|_| TipHost { inner, anchor, own: Rc::default(), left: None }).into()
            })
    }
}

impl<E: StatefulInteractiveElement + ParentElement> Tipped for E {}

/// How long the pointer rests on a control before its tooltip shows, how long a tooltip waits for
/// the pointer to cross onto it, and how soon after one the next shows at once.
const TIP_SHOW: Duration = Duration::from_millis(300);
const TIP_GRACE: Duration = Duration::from_millis(150);
const TIP_WARM: Duration = Duration::from_millis(500);

thread_local! {
    /// When a tooltip was last on screen.
    static TIP_SEEN: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[cfg(feature = "checks")]
thread_local! {
    static TIP_RENDERS: Cell<u32> = const { Cell::new(0) };
    static TIP_HIDDEN: Cell<u32> = const { Cell::new(0) };
}

/// For the checks: how often a tooltip host drew, how often of those it drew nothing (the pointer
/// gone from the control and the tooltip), and how long ago a tooltip was last on screen.
#[cfg(feature = "checks")]
pub fn tip_stats() -> (u32, u32, Option<Duration>) {
    (TIP_RENDERS.with(Cell::get), TIP_HIDDEN.with(Cell::get), TIP_SEEN.with(Cell::get).map(|s| s.elapsed()))
}

/// A tooltip as gpui shows it: gpui keeps a hoverable one half a second after the pointer has
/// left, so this hides it itself once the pointer has been off the control and the tooltip both
/// for the grace.
struct TipHost {
    inner: AnyView,
    anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    own: Rc<Cell<Option<Bounds<Pixels>>>>,
    left: Option<Instant>,
}

impl Render for TipHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let at = window.mouse_position();
        let over = |b: &Cell<Option<Bounds<Pixels>>>| b.get().is_some_and(|b| b.contains(&at));
        if over(&self.anchor) || over(&self.own) {
            self.left = None;
        } else if self.left.is_none() {
            self.left = Some(Instant::now());
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(TIP_GRACE).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
        #[cfg(feature = "checks")]
        TIP_RENDERS.with(|n| n.set(n.get() + 1));
        if self.left.is_some_and(|l| l.elapsed() >= TIP_GRACE) {
            #[cfg(feature = "checks")]
            TIP_HIDDEN.with(|n| n.set(n.get() + 1));
            return div().into_any_element();
        }
        TIP_SEEN.with(|s| s.set(Some(Instant::now())));
        let own = self.own.clone();
        div()
            .relative()
            .child(gpui::canvas(move |b, _, _| own.set(Some(b)), |_, _, _, _| {}).absolute().top_0().left_0().size_full())
            .child(self.inner.clone())
            .into_any_element()
    }
}

pub struct Tooltip {
    title: SharedString,
    desc: Option<SharedString>,
    key: Option<SharedString>,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .children(here(&"tooltip".into()))
            .max_w(px(320.))
            .px(px(11.))
            .py(px(9.))
            .bg(t.pop)
            .shadow(t.ring())
            .font_family(crate::theme::mono())
            .text_size(px(11.5))
            .line_height(px(17.))
            .text_color(t.tx2)
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .text_color(t.tx)
                    .child(self.title.clone())
                    .children(self.key.clone().map(|k| keycap(&t, k))),
            )
            .children(self.desc.clone())
    }
}

pub fn tooltip(
    title: impl Into<SharedString>,
    desc: Option<&str>,
    key: Option<&str>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let title = title.into();
    let desc: Option<SharedString> = desc.map(|d| SharedString::from(d.to_string()));
    let key: Option<SharedString> = key.map(|k| keys_text(k.to_string()));
    move |_, cx| {
        let (title, desc, key) = (title.clone(), desc.clone(), key.clone());
        cx.new(|_| Tooltip { title, desc, key }).into()
    }
}

pub const TIP_MAX_W: f32 = 360.;
pub const TIP_LINES: usize = 14;
const TIP_CHAR: f32 = 6.9;
const TIP_PAD_X: f32 = 11.;
const TIP_LINE_H: f32 = 17.;

pub fn fit_tip(text: &str) -> (String, f32) {
    let per_line = ((TIP_MAX_W - 2. * TIP_PAD_X) / TIP_CHAR).floor() as usize;
    let wraps = (per_line * 17 / 20).max(1);
    let width = |longest: usize| (longest as f32 * TIP_CHAR + 2. * TIP_PAD_X + 2.).clamp(60., TIP_MAX_W);
    let mut out = String::new();
    let (mut lines, mut longest) = (0usize, 0usize);
    for line in text.trim().lines() {
        let line = line.trim_end();
        let n = line.chars().count();
        let rows = n.div_ceil(wraps).max(1);
        if lines + rows > TIP_LINES {
            let room = (TIP_LINES - lines) * wraps;
            if room == 0 {
                out.pop();
            } else {
                let cut: String = line.chars().take(room.saturating_sub(1)).collect();
                out.push_str(cut.trim_end());
            }
            out.push('…');
            return (out, width(per_line.max(longest)));
        }
        lines += rows;
        longest = longest.max(n.min(per_line));
        out.push_str(line);
        out.push('\n');
    }
    out.pop();
    (out, width(longest))
}

pub struct TextTip {
    text: SharedString,
    width: f32,
}

impl Render for TextTip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .children(here(&"tooltip".into()))
            .w(px(self.width))
            .max_h(px(TIP_LINES as f32 * TIP_LINE_H + 20.))
            .overflow_hidden()
            .px(px(TIP_PAD_X))
            .py(px(9.))
            .bg(t.pop)
            .shadow(t.ring())
            .font_family(crate::theme::mono())
            .text_size(px(11.5))
            .line_height(px(TIP_LINE_H))
            .text_color(t.tx)
            .whitespace_normal()
            .child(div().w_full().line_clamp(TIP_LINES).text_ellipsis().child(self.text.clone()))
    }
}

pub fn text_tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let (text, width) = fit_tip(&text.into());
    let text = SharedString::from(text);
    move |_, cx| {
        let text = text.clone();
        cx.new(|_| TextTip { text, width }).into()
    }
}

pub fn modal(t: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .max_w(gpui::relative(0.96))
        .max_h(gpui::relative(0.94))
        .flex()
        .flex_col()
        .bg(t.pop)
        .shadow(t.ring())
        .text_size(px(12.))
        .text_color(t.tx)
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

pub fn modal_header(t: &Theme, leading: Option<gpui::AnyElement>, title: impl Into<SharedString>, close: impl IntoElement) -> Div {
    modal_header_with(t, leading, title, None, close)
}

pub fn modal_header_with(
    t: &Theme,
    leading: Option<gpui::AnyElement>,
    title: impl Into<SharedString>,
    extra: Option<gpui::AnyElement>,
    close: impl IntoElement,
) -> Div {
    div()
        .h(px(52.))
        .flex_none()
        .pl(px(20.))
        .pr(px(16.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .border_b_1()
        .border_color(t.line)
        .children(leading)
        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).child(title.into()))
        .children(extra)
        .child(close)
}

pub fn modal_close(t: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    icon_button(t, id, Icon::Close, 26., 14.)
}

/// A modal layer laid out this frame: what Esc does to it.
struct EscLayer {
    window: gpui::WindowId,
    /// The layer's key context: with the focus under it, its own Esc binding answers.
    keys: Option<&'static str>,
    /// None while the layer listens for a key (a shortcut being recorded): Esc is its own.
    close: Option<std::rc::Rc<dyn Fn(&mut Window, &mut App)>>,
}

thread_local! {
    static ESC_LAYERS: std::cell::RefCell<Vec<EscLayer>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Called as the window renders: only the modals this frame lays out answer Esc.
pub fn esc_frame(window: &Window) {
    let id = window.window_handle().window_id();
    ESC_LAYERS.with(|l| l.borrow_mut().retain(|e| e.window != id));
}

fn esc_layer(keys: Option<&'static str>, close: Option<std::rc::Rc<dyn Fn(&mut Window, &mut App)>>) -> gpui::AnyElement {
    gpui::canvas(move |_, window, _| ESC_LAYERS.with(|l| l.borrow_mut().push(EscLayer { window: window.window_handle().window_id(), keys, close })), |_, _, _, _| {})
        .absolute()
        .size_0()
        .into_any_element()
}

/// A modal layer's child: Esc does what `close` does (the layer's Cancel), wherever the keys
/// are. Layers register as they lay out, so the last is the topmost. With the focus under `keys`
/// (the layer's key context) its own Esc binding answers instead, so a menu in it shuts first.
pub fn esc_closes<E: Default + 'static>(keys: Option<&'static str>, close: impl Fn(&E, &mut Window, &mut App) + 'static) -> gpui::AnyElement {
    esc_layer(keys, Some(std::rc::Rc::new(move |window: &mut Window, cx: &mut App| close(&E::default(), window, cx))))
}

/// A modal layer listening for a key: Esc goes to it, and closes nothing.
pub fn esc_kept() -> gpui::AnyElement {
    esc_layer(None, None)
}

/// A modal fading out: drawn, but no longer answering Esc.
pub fn esc_muted(el: gpui::AnyElement) -> gpui::AnyElement {
    let from = std::rc::Rc::new(std::cell::Cell::new(0));
    let to = from.clone();
    let mark = gpui::canvas(move |_, _, _| from.set(ESC_LAYERS.with(|l| l.borrow().len())), |_, _, _, _| {}).absolute().size_0();
    let cut = gpui::canvas(move |_, _, _| ESC_LAYERS.with(|l| l.borrow_mut().truncate(to.get())), |_, _, _, _| {}).absolute().size_0();
    div().absolute().top_0().left_0().right_0().bottom_0().child(mark).child(el).child(cut).into_any_element()
}

/// Esc closes the topmost modal. An interceptor runs before any binding and key handler, so
/// nothing under a modal hears it; registered before the drag's (`dnd`), which goes first.
pub fn close_modals_on_escape(cx: &mut App) {
    cx.intercept_keystrokes(|ev, window, cx| {
        if ev.keystroke.key != "escape" || ev.keystroke.modifiers.modified() || cx.has_active_drag() {
            return;
        }
        let id = window.window_handle().window_id();
        let Some((keys, close)) = ESC_LAYERS.with(|l| l.borrow().iter().rev().find(|e| e.window == id).map(|e| (e.keys, e.close.clone()))) else { return };
        let Some(close) = close else { return };
        if keys.is_some_and(|k| ev.context_stack.iter().any(|c| c.contains(k))) {
            return;
        }
        cx.stop_propagation();
        close(window, cx);
    })
    .detach();
}

pub fn modal_footer(t: &Theme) -> Div {
    div()
        .h(px(56.))
        .flex_none()
        .pl(px(20.))
        .pr(px(16.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .border_t_1()
        .border_color(t.line)
}

pub fn empty_state(t: &Theme, tag: &str, size: f32, msg: Option<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(10.))
        .text_center()
        .child(banner(SharedString::from(tag.to_string()), size, t.tx4))
        .children(msg.map(|m| div().text_color(t.tx2).line_height(px(18.)).child(m)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_tips_fit() {
        let (t, w) = fit_tip("connection refused");
        assert_eq!(t, "connection refused");
        assert!(w < 200. && w >= 60., "{w}");
        let huge = format!("FATAL: {}\nDETAIL: {}\n{}", "x".repeat(900), "y ".repeat(300), "z\n".repeat(50));
        let (t, w) = fit_tip(&huge);
        assert!(w > 340. && w <= TIP_MAX_W, "{w}");
        assert!(t.ends_with('…') && t.chars().count() < 900, "{}", t.len());
        let (t, _) = fit_tip(&"line\n".repeat(40));
        assert_eq!(t.lines().count(), TIP_LINES);
        assert!(t.ends_with('…'));
    }

    /// A control's tooltip goes through `Tipped::tip`, never gpui's own, which vanishes as the
    /// pointer moves onto it.
    #[test]
    fn every_tooltip_can_be_hovered() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut plain = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            for (n, line) in text.lines().enumerate() {
                if line.contains(concat!(".tool", "tip(")) && !line.trim_start().starts_with("//") {
                    plain.push(format!("{}:{}", e.file_name().to_string_lossy(), n + 1));
                }
            }
        }
        assert!(plain.is_empty(), "use .tip(…) (ui::Tipped): {plain:?}");
    }

    /// Every modal layer (a scrim) answers Esc through `esc_closes`, as many as it has scrims.
    /// app.rs's param sheet holds the prompt (params_ui), which answers for it.
    #[test]
    fn every_modal_answers_esc() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut deaf = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            let scrims = text.matches(concat!(".bg(t.", "scrim)")).count();
            let answers = text.matches(concat!("esc_", "closes(")).count() + usize::from(name == "params_ui.rs");
            // data_import.rs: its guard is drawn inside the sheet's own layer.
            let layers = scrims - usize::from(name == "data_import.rs" && scrims > 0) - usize::from(name == "app.rs" && scrims > 0);
            if answers < layers {
                deaf.push(format!("{name}: {layers} layers, {answers} answer Esc"));
            }
        }
        assert!(deaf.is_empty(), "a modal Esc cannot close (ui::esc_closes): {deaf:?}");
    }
}

thread_local! {
    static MENU_SHUT: std::cell::RefCell<Option<(String, std::time::Instant)>> = const { std::cell::RefCell::new(None) };
}

/// A dropdown closed by a press outside it: when that press was on the dropdown's own button,
/// the click that follows must not open it again (one click on the button closes it).
pub fn menu_shut(id: impl Into<String>) {
    MENU_SHUT.with(|s| *s.borrow_mut() = Some((id.into(), std::time::Instant::now())));
}

/// True once, right after `menu_shut(id)`: the button's click then leaves the menu closed.
pub fn menu_just_shut(id: &str) -> bool {
    MENU_SHUT.with(|s| {
        let mut s = s.borrow_mut();
        let hit = s.as_ref().is_some_and(|(i, at)| i == id && at.elapsed() < std::time::Duration::from_millis(400));
        if hit {
            *s = None;
        }
        hit
    })
}

/// Test probes: in `--demo` runs, a wrapped control records where it was laid out, under a
/// name, so a check can click it with real mouse events. Off otherwise (no wrapper at all).
#[cfg(feature = "checks")]
mod probes {
    use super::*;

    static PROBING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    thread_local! {
        static PROBES: std::cell::RefCell<std::collections::HashMap<SharedString, gpui::Bounds<gpui::Pixels>>> = Default::default();
    }

    pub fn enable_probes() {
        PROBING.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Called as the window renders: only what this frame lays out stays clickable.
    pub fn clear_probes() {
        if PROBING.load(std::sync::atomic::Ordering::Relaxed) {
            PROBES.with(|p| p.borrow_mut().clear());
        }
    }

    pub fn probed(id: &str) -> Option<gpui::Bounds<gpui::Pixels>> {
        PROBES.with(|p| p.borrow().get(id).copied())
    }

    /// For a control with an element id: an absolutely placed, empty canvas that records the
    /// control's bounds under that id (`segment-2` for `("segment", 2)`).
    pub fn here(id: &ElementId) -> Option<gpui::AnyElement> {
        if !PROBING.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let id: SharedString = id.to_string().into();
        Some(
            gpui::canvas(move |bounds, _, _| PROBES.with(|p| p.borrow_mut().insert(id, bounds)), |_, _, _, _| {})
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .into_any_element(),
        )
    }

    pub fn probe(id: impl Into<SharedString>, el: impl IntoElement) -> gpui::AnyElement {
        if !PROBING.load(std::sync::atomic::Ordering::Relaxed) {
            return el.into_any_element();
        }
        Probe { id: id.into(), child: el.into_any_element() }.into_any_element()
    }

    struct Probe {
        id: SharedString,
        child: gpui::AnyElement,
    }

    impl IntoElement for Probe {
        type Element = Self;
        fn into_element(self) -> Self {
            self
        }
    }

    impl gpui::Element for Probe {
        type RequestLayoutState = ();
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
            None
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(&mut self, _: Option<&gpui::GlobalElementId>, _: Option<&gpui::InspectorElementId>, window: &mut Window, cx: &mut App) -> (gpui::LayoutId, ()) {
            (self.child.request_layout(window, cx), ())
        }

        fn prepaint(&mut self, _: Option<&gpui::GlobalElementId>, _: Option<&gpui::InspectorElementId>, bounds: gpui::Bounds<gpui::Pixels>, _: &mut (), window: &mut Window, cx: &mut App) {
            PROBES.with(|p| p.borrow_mut().insert(self.id.clone(), bounds));
            self.child.prepaint(window, cx);
        }

        fn paint(&mut self, _: Option<&gpui::GlobalElementId>, _: Option<&gpui::InspectorElementId>, _: gpui::Bounds<gpui::Pixels>, _: &mut (), _: &mut (), window: &mut Window, cx: &mut App) {
            self.child.paint(window, cx);
        }
    }
}

#[cfg(feature = "checks")]
pub use probes::*;

/// Without the checks: nothing recorded, nothing wrapped.
#[cfg(not(feature = "checks"))]
pub fn clear_probes() {}

#[cfg(not(feature = "checks"))]
pub fn here(_: &ElementId) -> Option<gpui::AnyElement> {
    None
}

#[cfg(not(feature = "checks"))]
pub fn probe(_: impl Into<SharedString>, el: impl IntoElement) -> gpui::AnyElement {
    el.into_any_element()
}
