//! One-shot motion driven from `render`, the way Zeron (zeronsh/zeron, MIT) moves its sidebar.

use std::time::{Duration, Instant};

use gpui::{App, Bounds, Global, Pixels, Point, point, size};
use crate::theme::px;

#[derive(Clone, Copy, Debug)]
pub struct CubicBezier {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

pub const EASE: CubicBezier = CubicBezier { x1: 0.25, y1: 0.1, x2: 0.25, y2: 1.0 };

impl CubicBezier {
    fn sample(a1: f32, a2: f32, s: f32) -> f32 {
        let u = 1. - s;
        3. * u * u * s * a1 + 3. * u * s * s * a2 + s * s * s
    }

    fn slope(a1: f32, a2: f32, s: f32) -> f32 {
        let u = 1. - s;
        3. * u * u * a1 + 6. * u * s * (a2 - a1) + 3. * s * s * (1. - a2)
    }

    pub fn eval(&self, x: f32) -> f32 {
        let x = x.clamp(0., 1.);
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

#[derive(Clone, Copy, Debug)]
pub struct WidthTween {
    from: f32,
    started: Instant,
    duration: Duration,
}

impl WidthTween {
    pub fn panel(from: f32, to: f32, unit: f32) -> WidthTween {
        let scale = ((to - from).abs() / unit.max(1.)).sqrt().clamp(1., 4. / 3.);
        WidthTween { from, started: Instant::now(), duration: Duration::from_millis(180).mul_f32(scale) }
    }

    pub fn at(&self, target: f32, now: Instant) -> Option<f32> {
        let raw = now.saturating_duration_since(self.started).as_secs_f32() / self.duration.as_secs_f32();
        if raw >= 1. {
            return None;
        }
        Some(self.from + (target - self.from) * EASE.eval(raw))
    }
}

pub const EASE_OUT: CubicBezier = CubicBezier { x1: 0.22, y1: 1.0, x2: 0.36, y2: 1.0 };

pub const ZOOM: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
pub struct ZoomTween {
    from: f32,
    started: Instant,
}

impl ZoomTween {
    pub fn new(from: f32) -> ZoomTween {
        ZoomTween { from, started: Instant::now() }
    }

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

pub const DOCK_MOVE: Duration = Duration::from_millis(240);
pub const DROP_PREVIEW: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, Debug)]
pub struct Progress {
    started: Instant,
    duration: Duration,
}

impl Progress {
    pub fn new(duration: Duration) -> Progress {
        Progress { started: Instant::now(), duration }
    }

    pub fn at(&self, now: Instant) -> Option<f32> {
        if let Some(p) = dock_freeze() {
            return Some(p);
        }
        let raw = now.saturating_duration_since(self.started).as_secs_f32() / self.duration.as_secs_f32().max(1e-3);
        (raw < 1.).then(|| EASE.eval(raw))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RectTween {
    from: [f32; 4],
    started: Instant,
}

impl RectTween {
    pub fn new(from: [f32; 4]) -> RectTween {
        RectTween { from, started: Instant::now() }
    }

    pub fn at(&self, target: [f32; 4], now: Instant) -> Option<[f32; 4]> {
        let raw = now.saturating_duration_since(self.started).as_secs_f32() / DROP_PREVIEW.as_secs_f32();
        if raw >= 1. {
            return None;
        }
        let k = EASE_OUT.eval(raw);
        Some(std::array::from_fn(|i| self.from[i] + (target[i] - self.from[i]) * k))
    }
}

pub const SPLIT: Duration = Duration::from_millis(220);

#[derive(Clone, Copy, Debug)]
pub struct SplitTween {
    from: f32,
    started: Instant,
}

impl SplitTween {
    pub fn new(from: f32) -> SplitTween {
        SplitTween { from, started: Instant::now() }
    }

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

fn split_freeze() -> Option<f32> {
    #[cfg(feature = "screenshots")]
    {
        static FREEZE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
        *FREEZE.get_or_init(|| std::env::var("DATAMAN_SPLIT_FREEZE").ok().and_then(|v| v.parse().ok()))
    }
    #[cfg(not(feature = "screenshots"))]
    None
}

fn dock_freeze() -> Option<f32> {
    #[cfg(feature = "screenshots")]
    {
        static FREEZE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
        *FREEZE.get_or_init(|| std::env::var("DATAMAN_DOCK_FREEZE").ok().and_then(|v| v.parse().ok()))
    }
    #[cfg(not(feature = "screenshots"))]
    None
}

fn zoom_freeze() -> Option<f32> {
    #[cfg(feature = "screenshots")]
    {
        static FREEZE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
        *FREEZE.get_or_init(|| std::env::var("DATAMAN_ZOOM_FREEZE").ok().and_then(|v| v.parse().ok()))
    }
    #[cfg(not(feature = "screenshots"))]
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopStyle {
    Select,
    Find,
}

/// AppKit's find indicator starts ~1.15–1.2 larger.
pub const POP_SCALE: f32 = 1.15;
pub const POP_SPRING: Duration = Duration::from_millis(200);

impl PopStyle {
    fn fade_ms(self) -> (f32, f32) {
        match self {
            PopStyle::Select => (110., 280.),
            PopStyle::Find => (190., 350.),
        }
    }

    #[cfg(any(test, feature = "checks"))]
    pub fn duration(self) -> Duration {
        Duration::from_millis(self.fade_ms().1 as u64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopFrame {
    pub scale: f32,
    pub strength: f32,
}

impl PopFrame {
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

pub fn pop_scale(ms: f32) -> f32 {
    let p = (ms / (POP_SPRING.as_secs_f32() * 1e3)).clamp(0., 1.);
    1. + (POP_SCALE - 1.) * (1. - p).powi(2) * (1.25 * std::f32::consts::PI * p).cos()
}

#[derive(Clone, Debug)]
pub struct FindPop<T> {
    pub target: T,
    pub style: PopStyle,
    started: Instant,
    reduced: bool,
}

impl<T> FindPop<T> {
    pub fn start(target: T, style: PopStyle, cx: &App) -> Option<FindPop<T>> {
        let reduced = cx.reduce_motion();
        (style == PopStyle::Find || !reduced).then(|| FindPop { target, style, started: Instant::now(), reduced })
    }

    pub fn frame(&self, now: Instant) -> Option<PopFrame> {
        let ms = freeze_ms().unwrap_or_else(|| now.saturating_duration_since(self.started).as_secs_f32() * 1e3);
        PopFrame::at(self.style, ms, self.reduced)
    }
}

fn freeze_ms() -> Option<f32> {
    #[cfg(feature = "screenshots")]
    {
        static FREEZE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
        *FREEZE.get_or_init(|| std::env::var("DATAMAN_POP_FREEZE_MS").ok().and_then(|v| v.parse().ok()))
    }
    #[cfg(not(feature = "screenshots"))]
    None
}

pub fn scale_bounds(b: Bounds<Pixels>, s: f32) -> Bounds<Pixels> {
    let c = b.center();
    let sz = size(b.size.width * s, b.size.height * s);
    Bounds::new(point(c.x - sz.width / 2., c.y - sz.height / 2.), sz)
}

pub fn scale_point(p: Point<Pixels>, c: Point<Pixels>, s: f32) -> Point<Pixels> {
    point(c.x + (p.x - c.x) * s, c.y + (p.y - c.y) * s)
}

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

pub const TAB_OPEN: Duration = Duration::from_millis(180);
pub const TAB_CLOSE: Duration = Duration::from_millis(150);
pub const TAB_GAP: Duration = Duration::from_millis(140);
pub const FOLDER_ROW: Duration = Duration::from_millis(190);
pub const FOLDER_STAGGER: Duration = Duration::from_millis(28);
pub const FLOOD: Duration = Duration::from_millis(300);
pub fn folder_fill(at: Instant, opening: bool) -> Option<f32> {
    let p = at.elapsed().as_secs_f32() / FLOOD.as_secs_f32();
    (p < 1.).then(|| if opening { 1. - EASE.eval(p) } else { EASE.eval(p) })
}

pub const FOLDER_SHUT: Duration = Duration::from_millis(150);
pub const FOLDER_SHUT_STAGGER: Duration = Duration::from_millis(18);
pub const TURN: gpui::SpringConfig = gpui::SpringConfig::new(520., 42., 1.);
pub const MENU_IN: Duration = Duration::from_millis(110);

#[derive(Clone, Copy, Debug)]
pub struct Fold {
    from: f32,
    to: f32,
    started: Instant,
    full: Duration,
}

impl Fold {
    pub fn new(from: f32, to: f32, full: Duration) -> Fold {
        Fold { from, to, started: Instant::now(), full }
    }

    fn span(&self) -> f32 {
        self.full.as_secs_f32() * (self.to - self.from).abs()
    }

    pub fn at(&self, now: Instant) -> f32 {
        let span = self.span();
        let raw = if span <= 0. { 1. } else { now.saturating_duration_since(self.started).as_secs_f32() / span };
        if raw >= 1. {
            return self.to;
        }
        self.from + (self.to - self.from) * EASE.eval(raw)
    }

    pub fn target(&self) -> f32 {
        self.to
    }

    pub fn done(&self, now: Instant) -> bool {
        self.at(now) == self.to
    }

    pub fn turn(&mut self, to: f32, now: Instant) {
        if to != self.to {
            *self = Fold { from: self.at(now), to, started: now, full: self.full };
        }
    }
}

pub fn timed(d: Duration, cx: &App) -> Duration {
    if cx.reduce_motion() { Duration::ZERO } else { d }
}

pub const COPIED_FOR: Duration = Duration::from_millis(2000);
pub const COPIED_FADE_AT: Duration = Duration::from_millis(1750);
pub const COPIED_IN: Duration = Duration::from_millis(120);

pub const SAVED_FOR: Duration = Duration::from_millis(2500);
pub const SAVED_FADE_AT: Duration = Duration::from_millis(2150);

#[derive(Clone, Copy, Debug)]
pub struct Flash {
    pub total: Duration,
    pub fade_at: Duration,
}

pub const COPIED: Flash = Flash { total: COPIED_FOR, fade_at: COPIED_FADE_AT };
pub const SAVED: Flash = Flash { total: SAVED_FOR, fade_at: SAVED_FADE_AT };

impl Flash {
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

pub fn copied(since: Instant, now: Instant, reduced: bool) -> Option<f32> {
    COPIED.at(since, now, reduced)
}

pub fn copied_redraws(reduced: bool) -> Vec<Duration> {
    COPIED.redraws(reduced)
}

/// No motion that moves or covers things: reduce motion is on, or a check stage is clicking
/// and drawing right after each change.
pub fn still(cx: &gpui::App) -> bool {
    cx.reduce_motion() || (cfg!(feature = "checks") && std::env::var_os("DATAMAN_SHOT_STAGE").is_some())
}

pub struct ReduceMotionPref(pub bool);

impl Global for ReduceMotionPref {}

pub fn set_reduce_motion_pref(on: bool, cx: &mut App) {
    cx.set_global(ReduceMotionPref(on));
    apply_reduce_motion(cx);
}

/// GPUI keeps the flag but never reads the system's, so this does: at startup and on every activation.
pub fn apply_reduce_motion(cx: &mut App) {
    let pref = cx.try_global::<ReduceMotionPref>().is_some_and(|p| p.0);
    let on = pref || system_reduce_motion();
    if cx.reduce_motion() != on {
        cx.set_reduce_motion(on);
    }
}

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

/// GNOME's "Animations" switch (`enable-animations`), else KDE's animation speed set to
/// instant; read again at most every few seconds (the settings view asks every frame).
#[cfg(target_os = "linux")]
pub fn system_reduce_motion() -> bool {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static LAST: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else { return false };
    if let Some((at, on)) = *last
        && at.elapsed() < Duration::from_secs(5)
    {
        return on;
    }
    let gnome = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "false");
    let on = gnome.unwrap_or_else(|| {
        let kde = dataman_core::os::config_home().and_then(|c| std::fs::read_to_string(c.join("kdeglobals")).ok());
        kde.is_some_and(|t| kde_instant(&t))
    });
    *last = Some((Instant::now(), on));
    on
}

#[cfg(any(target_os = "linux", test))]
fn kde_instant(kdeglobals: &str) -> bool {
    let mut kde = false;
    for line in kdeglobals.lines().map(str::trim) {
        if line.starts_with('[') {
            kde = line == "[KDE]";
        } else if kde && let Some(v) = line.strip_prefix("AnimationDurationFactor=") {
            return v.trim().parse::<f32>().is_ok_and(|f| f == 0.);
        }
    }
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn system_reduce_motion() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kde_animation_speed() {
        assert!(kde_instant("[General]\nfoo=1\n[KDE]\nAnimationDurationFactor=0\n"));
        assert!(!kde_instant("[KDE]\nAnimationDurationFactor=0.5\n"));
        assert!(!kde_instant("[General]\nAnimationDurationFactor=0\n"));
        assert!(!kde_instant(""));
    }

    #[test]
    fn fold_turns_around() {
        let f = Fold::new(0., 1., Duration::from_millis(200));
        let t0 = f.started;
        let mid = f.at(t0 + Duration::from_millis(100));
        assert!(mid > 0.5 && mid < 1., "{mid}");
        assert!(f.done(t0 + Duration::from_millis(200)) && f.at(t0 + Duration::from_millis(500)) == 1.);
        let mut back = Fold::new(0., 1., Duration::from_millis(200));
        let half = back.started + Duration::from_millis(60);
        back.turn(0., half);
        let from = back.at(half);
        assert!(from > 0. && from < 1. && back.target() == 0.);
        assert!(back.done(half + Duration::from_millis((from * 200.) as u64 + 1)));
        let now = Fold::new(1., 0., Duration::ZERO);
        assert!(now.done(now.started) && now.at(now.started) == 0.);
    }

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
        let c = WidthTween::panel(260., 0., 260.);
        assert!(c.at(0., c.started + Duration::from_millis(60)).unwrap() < 260.);
    }

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
        let f = PopFrame::at(PopStyle::Find, 0., false).unwrap();
        assert_eq!(f, PopFrame { scale: POP_SCALE, strength: 1. });
        assert!(pop_scale(40.) < 1.08, "{}", pop_scale(40.));
        let low = (0..=200).map(|ms| pop_scale(ms as f32)).fold(f32::MAX, f32::min);
        assert!(low < 1. && low > 0.98, "dip {low}");
        let mid = PopFrame::at(PopStyle::Find, 100., false).unwrap();
        assert!((mid.scale - 1.).abs() < 0.03 && mid.strength == 1., "{mid:?}");
        assert!((pop_scale(200.) - 1.).abs() < 1e-6);
        assert!((pop_scale(199.) - 1.).abs() < 1e-4);
        assert_eq!(pop_scale(260.), 1.);
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
            assert_eq!(PopFrame::at(style, end, false), None);
            assert_eq!(PopFrame::at(style, end + 500., false), None);
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
        let f = PopFrame::at(PopStyle::Find, 0., true).unwrap();
        assert_eq!(f, PopFrame { scale: 1., strength: 1. });
        assert!(PopFrame::at(PopStyle::Find, 300., true).unwrap().strength < 0.5);
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
        let o = scale_point(b.origin, b.center(), 1.2);
        assert!((o.x.as_f32() - 0.).abs() < 1e-4 && (o.y.as_f32() - 17.8).abs() < 1e-4, "{o:?}");
    }
}
