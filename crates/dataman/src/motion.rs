//! One-shot motion driven from `render`, the way Zeron (zeronsh/zeron, MIT) moves its sidebar:
//! the view evaluates the tween every frame and asks for the next frame only while it is
//! moving, so an idle window still draws nothing. No `with_animation` wrapper: the element tree
//! keeps its shape, and a finished tween is exactly the steady state.
//!
//! - [`WidthTween`]: the explorer and the history sliding open and shut (the
//!   title bar's explorer toggle fills its left part along with the explorer).
//! - [`Flash`]: a feedback that fades in and out ("Copied", "Saved as favorite").
//! - [`Progress`] and [`RectTween`]: a panel dropped in another dock settling into its place
//!   (`dock`), and the drop preview gliding from one place to the next while it is dragged.
//! - [`SplitTween`]: the second editor group growing in from the right, or folding away
//!   (`split`).
//! - [`FindPop`]: Xcode's find indicator (AppKit's `showFindIndicatorForRange:`): a box (rounded
//!   on a grid cell, square elsewhere) with the text drawn on top pops in enlarged, springs back to the text's size and fades.
//!   Blue on a clicked grid value ([`PopStyle::Select`]), yellow on something found
//!   ([`PopStyle::Find`]: ⌘' on the error, the row an FK "Go to" lands on, the explorer
//!   filter's first match).
//!
//! "Reduce motion" (Settings ▸ Appearance, or the macOS accessibility switch) lands in GPUI's
//! `App::reduce_motion`: no slide, no pop on a click, and a find shows its box at once, without
//! the zoom, before it dissolves.

use std::time::{Duration, Instant};

use gpui::{App, Bounds, Global, Pixels, Point, point, size};
use crate::theme::px;

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing curve.
#[derive(Clone, Copy, Debug)]
pub struct CubicBezier {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

/// CSS `ease`: Zeron's dialog entrance, which its panel columns (the sidebar) share.
pub const EASE: CubicBezier = CubicBezier { x1: 0.25, y1: 0.1, x2: 0.25, y2: 1.0 };

impl CubicBezier {
    fn sample(a1: f32, a2: f32, s: f32) -> f32 {
        // B(s) for control points 0, a1, a2, 1.
        let u = 1. - s;
        3. * u * u * s * a1 + 3. * u * s * s * a2 + s * s * s
    }

    fn slope(a1: f32, a2: f32, s: f32) -> f32 {
        let u = 1. - s;
        3. * u * u * a1 + 6. * u * s * (a2 - a1) + 3. * s * s * (1. - a2)
    }

    /// Eased progress for a linear time fraction `x` in 0..=1.
    pub fn eval(&self, x: f32) -> f32 {
        let x = x.clamp(0., 1.);
        // Solve x(s) = x: Newton steps, then bisection if the slope is too flat.
        let mut s = x;
        for _ in 0..8 {
            let err = Self::sample(self.x1, self.x2, s) - x;
            if err.abs() < 1e-5 {
                return Self::sample(self.y1, self.y2, s);
            }
            let d = Self::slope(self.x1, self.x2, s);
            if d.abs() < 1e-6 {
                break;
            }
            s = (s - err / d).clamp(0., 1.);
        }
        let (mut lo, mut hi) = (0f32, 1f32);
        s = x;
        for _ in 0..30 {
            let v = Self::sample(self.x1, self.x2, s);
            if (v - x).abs() < 1e-5 {
                break;
            }
            if v < x { lo = s } else { hi = s }
            s = (lo + hi) / 2.;
        }
        Self::sample(self.y1, self.y2, s)
    }
}

/// A width moving from `from` toward a target the caller passes each frame.
#[derive(Clone, Copy, Debug)]
pub struct WidthTween {
    from: f32,
    started: Instant,
    duration: Duration,
}

impl WidthTween {
    /// Zeron's `WidthTween::panel`: 180 ms on `EASE` for a move up to `unit` (the column's
    /// usual width), growing with the square root of the distance to at most 240 ms, so a
    /// wide move travels at about the same pace instead of covering more ground in the same
    /// time.
    pub fn panel(from: f32, to: f32, unit: f32) -> WidthTween {
        let scale = ((to - from).abs() / unit.max(1.)).sqrt().clamp(1., 4. / 3.);
        WidthTween { from, started: Instant::now(), duration: Duration::from_millis(180).mul_f32(scale) }
    }

    /// The width now, heading for `target` (the column's current goal: it may have changed
    /// since the tween started). `None` once the tween has landed: the caller drops it.
    pub fn at(&self, target: f32, now: Instant) -> Option<f32> {
        let raw = now.saturating_duration_since(self.started).as_secs_f32() / self.duration.as_secs_f32();
        if raw >= 1. {
            return None;
        }
        Some(self.from + (target - self.from) * EASE.eval(raw))
    }
}

/// An ease-out with a long, soft landing (`easeOutQuint`): the result zoom starts quick and
/// settles gently, so it feels immediate without snapping.
pub const EASE_OUT: CubicBezier = CubicBezier { x1: 0.22, y1: 1.0, x2: 0.36, y2: 1.0 };

/// The result zoom's length, either way.
pub const ZOOM: Duration = Duration::from_millis(250);

/// The result panel's zoom (0 = in its place, 1 = over the editor and the side panel), moving
/// from where it was toward the tab's current goal.
#[derive(Clone, Copy, Debug)]
pub struct ZoomTween {
    from: f32,
    started: Instant,
}

impl ZoomTween {
    pub fn new(from: f32) -> ZoomTween {
        ZoomTween { from, started: Instant::now() }
    }

    /// The zoom now, heading for `target` (0 or 1); `None` once it has landed. A turn mid-way
    /// takes the time that is left of the way back.
    pub fn at(&self, target: f32, now: Instant) -> Option<f32> {
        if let Some(p) = zoom_freeze() {
            return Some(p);
        }
        let span = ZOOM.as_secs_f32() * (target - self.from).abs().max(0.25);
        let raw = now.saturating_duration_since(self.started).as_secs_f32() / span;
        if raw >= 1. {
            return None;
        }
        Some(self.from + (target - self.from) * EASE_OUT.eval(raw))
    }
}

/// A panel moving to another dock (or another place in its own): the place it leaves folds
/// shut while the new one opens, the other panels and the work surface sliding with them.
pub const DOCK_MOVE: Duration = Duration::from_millis(240);
/// The drop preview moving from one place to the next while a panel is dragged.
pub const DROP_PREVIEW: Duration = Duration::from_millis(120);

/// An eased progress 0 → 1 over a fixed time (`EASE`): the caller mixes its own start and end.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    started: Instant,
    duration: Duration,
}

impl Progress {
    pub fn new(duration: Duration) -> Progress {
        Progress { started: Instant::now(), duration }
    }

    /// How far along it is now; `None` once it has landed (the caller drops it).
    pub fn at(&self, now: Instant) -> Option<f32> {
        if let Some(p) = dock_freeze() {
            return Some(p);
        }
        let raw = now.saturating_duration_since(self.started).as_secs_f32() / self.duration.as_secs_f32().max(1e-3);
        (raw < 1.).then(|| EASE.eval(raw))
    }
}

/// A rectangle (x, y, w, h in points) moving from where it was toward a target the caller
/// passes each frame, over [`DROP_PREVIEW`].
#[derive(Clone, Copy, Debug)]
pub struct RectTween {
    from: [f32; 4],
    started: Instant,
}

impl RectTween {
    pub fn new(from: [f32; 4]) -> RectTween {
        RectTween { from, started: Instant::now() }
    }

    /// The rectangle now, heading for `target`; `None` once it has landed.
    pub fn at(&self, target: [f32; 4], now: Instant) -> Option<[f32; 4]> {
        let raw = now.saturating_duration_since(self.started).as_secs_f32() / DROP_PREVIEW.as_secs_f32();
        if raw >= 1. {
            return None;
        }
        let k = EASE_OUT.eval(raw);
        Some(std::array::from_fn(|i| self.from[i] + (target[i] - self.from[i]) * k))
    }
}

/// The second editor group growing in from the right as the first narrows (Open to the Side),
/// or folding away as the other takes its room back.
pub const SPLIT: Duration = Duration::from_millis(220);

/// How far the split is open (0 one group … 1 two), moving from where it was toward the
/// workspace's current goal over [`SPLIT`] on `EASE` (the panels' curve).
#[derive(Clone, Copy, Debug)]
pub struct SplitTween {
    from: f32,
    started: Instant,
}

impl SplitTween {
    pub fn new(from: f32) -> SplitTween {
        SplitTween { from, started: Instant::now() }
    }

    /// The opening now, heading for `target` (0 or 1); `None` once it has landed. A turn
    /// mid-way takes the time that is left of the way back.
    pub fn at(&self, target: f32, now: Instant) -> Option<f32> {
        if let Some(p) = split_freeze() {
            return Some(p);
        }
        let span = SPLIT.as_secs_f32() * (target - self.from).abs().max(0.25);
        let raw = now.saturating_duration_since(self.started).as_secs_f32() / span;
        if raw >= 1. {
            return None;
        }
        Some(self.from + (target - self.from) * EASE.eval(raw))
    }
}

/// Screenshots (`--features screenshots`): `DATAMAN_SPLIT_FREEZE=0.5` holds the split half-way
/// open (or shut).
fn split_freeze() -> Option<f32> {
    #[cfg(feature = "screenshots")]
    {
        static FREEZE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
        *FREEZE.get_or_init(|| std::env::var("DATAMAN_SPLIT_FREEZE").ok().and_then(|v| v.parse().ok()))
    }
    #[cfg(not(feature = "screenshots"))]
    None
}

/// Screenshots (`--features screenshots`): `DATAMAN_DOCK_FREEZE=0.5` holds a panel's move
/// half-way.
fn dock_freeze() -> Option<f32> {
    #[cfg(feature = "screenshots")]
    {
        static FREEZE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
        *FREEZE.get_or_init(|| std::env::var("DATAMAN_DOCK_FREEZE").ok().and_then(|v| v.parse().ok()))
    }
    #[cfg(not(feature = "screenshots"))]
    None
}

/// Screenshots (`--features screenshots`): `DATAMAN_ZOOM_FREEZE=0.5` holds a zoom half-way.
fn zoom_freeze() -> Option<f32> {
    #[cfg(feature = "screenshots")]
    {
        static FREEZE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
        *FREEZE.get_or_init(|| std::env::var("DATAMAN_ZOOM_FREEZE").ok().and_then(|v| v.parse().ok()))
    }
    #[cfg(not(feature = "screenshots"))]
    None
}

/// What a pop looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopStyle {
    /// A value clicked in the grid: a stronger selection blue (`sel_hi`) that settles into the
    /// rounded selection, which stays.
    Select,
    /// Something found: Xcode's yellow box (`find`, dark text `find_tx` on it), gone at the end.
    Find,
}

/// The box starts this much larger than its target (AppKit's indicator: ~1.15–1.2) ...
pub const POP_SCALE: f32 = 1.15;
/// ... and springs back to 1 in this long, dipping a little under it on the way (≥ 0.98).
pub const POP_SPRING: Duration = Duration::from_millis(200);

impl PopStyle {
    /// When the pop tint starts and stops fading, in ms from the start. A click's blue fades
    /// early and quick (it is on every click: it must not shout); a find's yellow holds until
    /// the spring has landed, then dissolves.
    fn fade_ms(self) -> (f32, f32) {
        match self {
            PopStyle::Select => (110., 280.),
            PopStyle::Find => (190., 350.),
        }
    }

    /// The whole pop: 280 ms for a click, 350 ms for a find.
    pub fn duration(self) -> Duration {
        Duration::from_millis(self.fade_ms().1 as u64)
    }
}

/// One frame of a pop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopFrame {
    /// The box and its text, scaled about the target's centre: 1.15 → 1.
    pub scale: f32,
    /// How much pop tint is left: 1 = full (`sel_hi`, opaque `find`), 0 = resting (the plain
    /// selection for a click, nothing for a find).
    pub strength: f32,
}

impl PopFrame {
    /// The frame `ms` after the start; `None` once it is over. `reduced` (reduce motion): no
    /// zoom, the box only dissolves.
    pub fn at(style: PopStyle, ms: f32, reduced: bool) -> Option<PopFrame> {
        let (a, b) = style.fade_ms();
        if ms >= b {
            return None;
        }
        let scale = if reduced { 1. } else { pop_scale(ms) };
        let strength = if ms <= a { 1. } else { 1. - EASE.eval((ms - a) / (b - a)) };
        Some(PopFrame { scale, strength })
    }
}

/// The spring: an ease-out from `POP_SCALE` with one small dip under 1, damped to exactly 1 (and
/// at rest) when `POP_SPRING` ends: `1 + 0.15 · (1 − p)² · cos(1.25 π p)`.
pub fn pop_scale(ms: f32) -> f32 {
    let p = (ms / (POP_SPRING.as_secs_f32() * 1e3)).clamp(0., 1.);
    1. + (POP_SCALE - 1.) * (1. - p).powi(2) * (1.25 * std::f32::consts::PI * p).cos()
}

/// A find indicator on `target` (a grid cell, an editor range, an explorer row), evaluated in
/// `render`/`prepaint` every frame: the owner asks for another frame while [`FindPop::frame`] is
/// `Some` and drops the pop when it is `None`. A new pop replaces the old one (a click mid-pop
/// restarts it on the new cell). Targets are content positions, so the box scrolls with the
/// content.
#[derive(Clone, Debug)]
pub struct FindPop<T> {
    pub target: T,
    pub style: PopStyle,
    started: Instant,
    reduced: bool,
}

impl<T> FindPop<T> {
    /// Starts a pop now; `None` when there is nothing to show (a click's pop under reduce
    /// motion: the selection simply appears).
    pub fn start(target: T, style: PopStyle, cx: &App) -> Option<FindPop<T>> {
        let reduced = cx.reduce_motion();
        (style == PopStyle::Find || !reduced).then(|| FindPop { target, style, started: Instant::now(), reduced })
    }

    /// This frame of the pop, or `None` once it is over.
    pub fn frame(&self, now: Instant) -> Option<PopFrame> {
        let ms = freeze_ms().unwrap_or_else(|| now.saturating_duration_since(self.started).as_secs_f32() * 1e3);
        PopFrame::at(self.style, ms, self.reduced)
    }
}

/// Screenshots (`--features screenshots`): `DATAMAN_POP_FREEZE_MS=60` holds every pop at that
/// moment, so a capture shows it mid-flight.
fn freeze_ms() -> Option<f32> {
    #[cfg(feature = "screenshots")]
    {
        static FREEZE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
        *FREEZE.get_or_init(|| std::env::var("DATAMAN_POP_FREEZE_MS").ok().and_then(|v| v.parse().ok()))
    }
    #[cfg(not(feature = "screenshots"))]
    None
}

/// `b` scaled by `s` about its centre.
pub fn scale_bounds(b: Bounds<Pixels>, s: f32) -> Bounds<Pixels> {
    let c = b.center();
    let sz = size(b.size.width * s, b.size.height * s);
    Bounds::new(point(c.x - sz.width / 2., c.y - sz.height / 2.), sz)
}

/// Where `p` goes when its frame is scaled by `s` about `c`.
pub fn scale_point(p: Point<Pixels>, c: Point<Pixels>, s: f32) -> Point<Pixels> {
    point(c.x + (p.x - c.x) * s, c.y + (p.y - c.y) * s)
}

/// Paints a pop's box (`rest` is its size at rest) and, on top, its text: a line already shaped
/// at the scaled font size, whose resting origin and line height are given.
pub fn paint_pop(
    window: &mut gpui::Window,
    cx: &mut App,
    rest: Bounds<Pixels>,
    radius: f32,
    fill: gpui::Hsla,
    scale: f32,
    text: Option<(&gpui::ShapedLine, Point<Pixels>, Pixels)>,
) {
    window.paint_quad(gpui::quad(
        scale_bounds(rest, scale),
        px(radius * scale),
        fill,
        px(0.),
        gpui::transparent_black(),
        gpui::BorderStyle::Solid,
    ));
    if let Some((line, origin, lh)) = text {
        let at = scale_point(origin, rest.center(), scale);
        let _ = line.paint(at, lh * scale, gpui::TextAlign::Left, None, window, cx);
    }
}

/// How long the result bar's Copy button says "Copied" after a copy.
pub const COPIED_FOR: Duration = Duration::from_millis(2000);
/// When "Copied" starts to fade back into the Copy button.
pub const COPIED_FADE_AT: Duration = Duration::from_millis(1750);
/// Its fade in.
pub const COPIED_IN: Duration = Duration::from_millis(120);

/// How long the toolbar's "Saved as favorite" (⌘S) stays next to the History button.
pub const SAVED_FOR: Duration = Duration::from_millis(2500);
/// When it starts to fade out.
pub const SAVED_FADE_AT: Duration = Duration::from_millis(2150);

/// A short feedback that comes and goes ("Copied", "Saved as favorite"): how long it shows and
/// when it starts to fade out; it fades in over [`COPIED_IN`].
#[derive(Clone, Copy, Debug)]
pub struct Flash {
    pub total: Duration,
    pub fade_at: Duration,
}

pub const COPIED: Flash = Flash { total: COPIED_FOR, fade_at: COPIED_FADE_AT };
pub const SAVED: Flash = Flash { total: SAVED_FOR, fade_at: SAVED_FADE_AT };

impl Flash {
    /// Its opacity `since` it started: fading in, held, fading out (always 1 with "Reduce
    /// motion": it is simply there, then gone); `None` once it is over.
    pub fn at(self, since: Instant, now: Instant, reduced: bool) -> Option<f32> {
        let e = now.saturating_duration_since(since);
        if e >= self.total {
            return None;
        }
        if reduced {
            return Some(1.);
        }
        let fade_in = (e.as_secs_f32() / COPIED_IN.as_secs_f32()).min(1.);
        let fade_out = if e >= self.fade_at { (self.total - e).as_secs_f32() / (self.total - self.fade_at).as_secs_f32() } else { 1. };
        Some(EASE.eval(fade_in.min(fade_out)))
    }

    /// When it must be drawn again after it started: every frame of its fades, and its end
    /// (with "Reduce motion" only the end).
    pub fn redraws(self, reduced: bool) -> Vec<Duration> {
        let frame = Duration::from_millis(16);
        let mut at = Vec::new();
        let fade = |from: Duration, to: Duration, at: &mut Vec<Duration>| {
            let mut t = from;
            while t < to {
                at.push(t);
                t += frame;
            }
        };
        if !reduced {
            fade(frame, COPIED_IN + frame, &mut at);
            fade(self.fade_at, self.total, &mut at);
        }
        at.push(self.total);
        at
    }
}

/// The "Copied" state's opacity `since` a copy ([`Flash::at`]).
pub fn copied(since: Instant, now: Instant, reduced: bool) -> Option<f32> {
    COPIED.at(since, now, reduced)
}

/// When the "Copied" state must be drawn again after a copy ([`Flash::redraws`]).
pub fn copied_redraws(reduced: bool) -> Vec<Duration> {
    COPIED.redraws(reduced)
}

/// The "Reduce motion" setting (Settings ▸ Appearance), kept so the system's switch can be
/// combined with it again later.
pub struct ReduceMotionPref(pub bool);

impl Global for ReduceMotionPref {}

/// Changes the setting (already saved by the caller) and applies it.
pub fn set_reduce_motion_pref(on: bool, cx: &mut App) {
    cx.set_global(ReduceMotionPref(on));
    apply_reduce_motion(cx);
}

/// GPUI's `reduce_motion` = the setting or the macOS accessibility switch. GPUI keeps the flag
/// but never reads the system's, so this does: at startup and whenever the window becomes
/// active (the switch is flipped in System Settings, then the user comes back).
pub fn apply_reduce_motion(cx: &mut App) {
    let pref = cx.try_global::<ReduceMotionPref>().is_some_and(|p| p.0);
    let on = pref || system_reduce_motion();
    if cx.reduce_motion() != on {
        cx.set_reduce_motion(on);
    }
}

/// System Settings ▸ Accessibility ▸ Display ▸ Reduce motion
/// (`NSWorkspace.accessibilityDisplayShouldReduceMotion`).
#[cfg(target_os = "macos")]
pub fn system_reduce_motion() -> bool {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    // SAFETY: class methods/properties of AppKit's shared workspace, available since 10.12 and
    // callable from any thread; a nil workspace reads as "not reduced".
    unsafe {
        let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if ws.is_null() {
            return false;
        }
        msg_send![ws, accessibilityDisplayShouldReduceMotion]
    }
}

#[cfg(not(target_os = "macos"))]
pub fn system_reduce_motion() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Copied": in, held, out, gone after 2 s; without motion simply there, then gone, drawn
    /// again only at its end.
    #[test]
    fn copied_fades() {
        let t0 = Instant::now();
        let at = |ms: u64, reduced: bool| copied(t0, t0 + Duration::from_millis(ms), reduced);
        assert!(at(0, false).is_some_and(|a| a < 0.05));
        assert_eq!(at(500, false), Some(1.));
        assert!(at(1900, false).is_some_and(|a| a > 0. && a < 1.));
        assert_eq!((at(2000, false), at(2000, true)), (None, None));
        assert_eq!((at(0, true), at(1900, true)), (Some(1.), Some(1.)));
        assert_eq!(copied_redraws(true), vec![COPIED_FOR]);
        let r = copied_redraws(false);
        assert!(r.len() > 20 && r.last() == Some(&COPIED_FOR) && r.windows(2).all(|w| w[0] < w[1]));
        assert!(!r.iter().any(|d| *d > COPIED_IN + Duration::from_millis(16) && *d < COPIED_FADE_AT), "no frames while it holds");
    }

    /// "Saved as favorite": the same fades, held for 2.5 s.
    #[test]
    fn saved_fades() {
        let t0 = Instant::now();
        let at = |ms: u64, reduced: bool| SAVED.at(t0, t0 + Duration::from_millis(ms), reduced);
        assert_eq!(at(1000, false), Some(1.));
        assert!(at(2400, false).is_some_and(|a| a > 0. && a < 1.));
        assert_eq!((at(2500, false), at(2500, true), at(2400, true)), (None, None, Some(1.)));
        assert_eq!(SAVED.redraws(true), vec![SAVED_FOR]);
        assert_eq!(SAVED.redraws(false).last(), Some(&SAVED_FOR));
    }

    #[test]
    fn ease_matches_css() {
        assert!((EASE.eval(0.) - 0.).abs() < 1e-4);
        assert!((EASE.eval(1.) - 1.).abs() < 1e-4);
        // CSS `ease` at 50 % of the time is ~80 % of the way.
        assert!((EASE.eval(0.5) - 0.8024).abs() < 2e-3, "{}", EASE.eval(0.5));
        let mut last = 0.;
        for i in 0..=100 {
            let v = EASE.eval(i as f32 / 100.);
            assert!(v + 1e-4 >= last, "monotonic at {i}");
            last = v;
        }
    }

    #[test]
    fn panel_tween_lands_and_scales_with_distance() {
        let t = WidthTween::panel(0., 260., 260.);
        assert_eq!(t.duration, Duration::from_millis(180));
        assert!((WidthTween::panel(0., 480., 260.).duration.as_secs_f32() - 0.240).abs() < 1e-3);
        let mid = t.at(260., t.started + Duration::from_millis(90)).unwrap();
        assert!(mid > 130. && mid < 260., "{mid}");
        assert_eq!(t.at(260., t.started + Duration::from_millis(181)), None);
        // Closing: from the width down to zero.
        let c = WidthTween::panel(260., 0., 260.);
        assert!(c.at(0., c.started + Duration::from_millis(60)).unwrap() < 260.);
    }

    /// The zoom: quick out of the gate, landing exactly on its goal in 250 ms; a turn half-way
    /// back takes half the time.
    #[test]
    fn zoom_lands() {
        let z = ZoomTween::new(0.);
        let t0 = z.started;
        let early = z.at(1., t0 + Duration::from_millis(60)).unwrap();
        assert!(early > 0.5 && early < 1., "{early}");
        assert!(z.at(1., t0 + Duration::from_millis(249)).unwrap() > 0.99);
        assert_eq!(z.at(1., t0 + ZOOM), None);
        let back = ZoomTween::new(0.5);
        assert!(back.at(0., back.started + Duration::from_millis(100)).is_some());
        assert_eq!(back.at(0., back.started + Duration::from_millis(126)), None);
        let mut last = 0.;
        for ms in 0..250 {
            let v = z.at(1., t0 + Duration::from_millis(ms)).unwrap();
            assert!(v + 1e-4 >= last);
            last = v;
        }
    }

    /// A dock move lands on 1 after `DOCK_MOVE`, eased; the preview glides to its target and
    /// stops there.
    #[test]
    fn dock_move_and_preview_land() {
        let p = Progress::new(DOCK_MOVE);
        let t0 = p.started;
        let mid = p.at(t0 + DOCK_MOVE / 2).unwrap();
        assert!(mid > 0.5 && mid < 1., "{mid}");
        assert!(p.at(t0 + Duration::from_millis(10)).unwrap() < mid);
        assert_eq!(p.at(t0 + DOCK_MOVE), None);
        let r = RectTween::new([0., 0., 100., 100.]);
        let target = [200., 40., 300., 500.];
        let half = r.at(target, r.started + DROP_PREVIEW / 2).unwrap();
        assert!(half.iter().zip(target).all(|(v, t)| *v <= t && *v >= 0.), "{half:?}");
        assert!(half[0] > 100., "quick out of the gate: {half:?}");
        assert_eq!(r.at(target, r.started + DROP_PREVIEW), None);
    }

    fn pop(style: PopStyle, reduced: bool) -> FindPop<()> {
        FindPop { target: (), style, started: Instant::now(), reduced }
    }

    #[test]
    fn pop_springs_from_enlarged_to_exact_size() {
        // t = 0: enlarged, full tint.
        let f = PopFrame::at(PopStyle::Find, 0., false).unwrap();
        assert_eq!(f, PopFrame { scale: POP_SCALE, strength: 1. });
        // Ease-out: most of the way back in the first third of the spring.
        assert!(pop_scale(40.) < 1.08, "{}", pop_scale(40.));
        // Mid-spring it dips a little under 1 (the overshoot), never far.
        let low = (0..=200).map(|ms| pop_scale(ms as f32)).fold(f32::MAX, f32::min);
        assert!(low < 1. && low > 0.98, "dip {low}");
        let mid = PopFrame::at(PopStyle::Find, 100., false).unwrap();
        assert!((mid.scale - 1.).abs() < 0.03 && mid.strength == 1., "{mid:?}");
        // The spring lands exactly on 1, at rest (no jump into the steady state).
        assert!((pop_scale(200.) - 1.).abs() < 1e-6);
        assert!((pop_scale(199.) - 1.).abs() < 1e-4);
        assert_eq!(pop_scale(260.), 1.);
        // Never bigger than where it starts.
        assert!((0..=200).all(|ms| pop_scale(ms as f32) <= POP_SCALE + 1e-6));
    }

    #[test]
    fn pop_fades_then_ends() {
        for (style, hold, end) in [(PopStyle::Select, 110., 280.), (PopStyle::Find, 190., 350.)] {
            assert_eq!(style.duration(), Duration::from_millis(end as u64));
            assert_eq!(PopFrame::at(style, hold, false).unwrap().strength, 1., "{style:?} holds");
            let half = PopFrame::at(style, (hold + end) / 2., false).unwrap();
            assert!(half.strength > 0.05 && half.strength < 0.6, "{style:?} {half:?}");
            assert!((half.scale - 1.).abs() < 1e-3, "{style:?}: the fade happens at (about) rest size");
            let last = PopFrame::at(style, end - 1., false).unwrap();
            assert!(last.strength < 0.02, "{style:?} {last:?}");
            // Done: the owner drops it and stops asking for frames.
            assert_eq!(PopFrame::at(style, end, false), None);
            assert_eq!(PopFrame::at(style, end + 500., false), None);
            // Strength only goes down.
            let mut prev = 1.;
            for ms in 0..end as u32 {
                let s = PopFrame::at(style, ms as f32, false).unwrap().strength;
                assert!(s <= prev + 1e-5, "{style:?} at {ms}");
                prev = s;
            }
        }
    }

    #[test]
    fn reduced_pop_has_no_zoom_and_a_real_pop_ends_in_time() {
        // Reduce motion: the box is at its size from the first frame, and still dissolves.
        let f = PopFrame::at(PopStyle::Find, 0., true).unwrap();
        assert_eq!(f, PopFrame { scale: 1., strength: 1. });
        assert!(PopFrame::at(PopStyle::Find, 300., true).unwrap().strength < 0.5);
        // A live pop: running now, over after its duration.
        let p = pop(PopStyle::Select, false);
        assert!(p.frame(p.started).is_some());
        assert!(p.frame(p.started + Duration::from_millis(150)).is_some());
        assert_eq!(p.frame(p.started + PopStyle::Select.duration()), None);
    }

    #[test]
    fn scaling_about_the_centre() {
        let b = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(22.)));
        let s = scale_bounds(b, 1.2);
        assert_eq!(s.center(), b.center());
        assert!((s.size.width.as_f32() - 120.).abs() < 1e-4 && (s.size.height.as_f32() - 26.4).abs() < 1e-4);
        // The text's origin moves out with the box.
        let o = scale_point(b.origin, b.center(), 1.2);
        assert!((o.x.as_f32() - 0.).abs() < 1e-4 && (o.y.as_f32() - 17.8).abs() < 1e-4, "{o:?}");
    }
}
