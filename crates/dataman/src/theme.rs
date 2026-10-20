use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, Ordering};

use gpui::{App, BoxShadow, Global, Hsla, Pixels, Rgba, Window, WindowAppearance, WindowBackgroundAppearance, point};
use dataman_core::{ThemePref, Translucency, UiFont};

static ZOOM: AtomicU32 = AtomicU32::new(0x3f80_0000);

pub const ZOOM_STEPS: [f32; 8] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

pub fn zoom() -> f32 {
    f32::from_bits(ZOOM.load(Ordering::Relaxed))
}

/// Window sizes, mouse positions and layout bounds from GPUI are already zoomed; [`Pt::pt`]
/// turns them back into these points.
#[inline]
pub fn px(v: f32) -> Pixels {
    gpui::px(v * zoom())
}

pub trait Pt {
    fn pt(self) -> f32;
}

impl Pt for Pixels {
    #[inline]
    fn pt(self) -> f32 {
        self.as_f32() / zoom()
    }
}

pub fn zoom_step(z: f32, dir: i32) -> f32 {
    match dir.signum() {
        0 => 1.0,
        1 => ZOOM_STEPS.iter().copied().find(|s| *s > z + 1e-3).unwrap_or(ZOOM_STEPS[ZOOM_STEPS.len() - 1]),
        _ => ZOOM_STEPS.iter().rev().copied().find(|s| *s < z - 1e-3).unwrap_or(ZOOM_STEPS[0]),
    }
}

pub fn init_zoom(z: f32) {
    ZOOM.store(dataman_core::settings::clamp_zoom(z).to_bits(), Ordering::Relaxed);
}

pub struct SaveZoom(pub Box<dyn Fn(f32)>);

impl Global for SaveZoom {}

pub fn set_zoom(z: f32, window: &mut Window, cx: &mut App) {
    init_zoom(z);
    if let Some(save) = cx.try_global::<SaveZoom>() {
        (save.0)(zoom());
    }
    let (x, y) = metrics::traffic_light_position_at(zoom());
    window.set_traffic_light_position(point(gpui::px(x), gpui::px(y)));
    cx.refresh_windows();
}

pub const PIXEL: &str = "DotGothic16";

static FONT: AtomicU8 = AtomicU8::new(0);
static REGISTERED: AtomicU8 = AtomicU8::new(0);
static SYSTEM_FONT: AtomicPtr<&'static str> = AtomicPtr::new(std::ptr::null_mut());

pub fn ui_font() -> UiFont {
    UiFont::ALL.get(FONT.load(Ordering::Relaxed) as usize).copied().unwrap_or_default()
}

pub fn system_font() -> Option<&'static str> {
    let p = SYSTEM_FONT.load(Ordering::Relaxed);
    (!p.is_null()).then(|| unsafe { *p })
}

pub fn mono() -> &'static str {
    system_font().unwrap_or_else(|| ui_font().family())
}

pub fn advance() -> f32 {
    if ui_font() == UiFont::ZedMono { 0.5 } else { 0.6 }
}

/// All OFL; the licenses are next to them in `assets/fonts`.
fn faces(f: UiFont) -> [&'static [u8]; 5] {
    match f {
        UiFont::GeistMono => [
            include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
            include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
            include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf"),
            include_bytes!("../assets/fonts/GeistMono-Bold.ttf"),
            include_bytes!("../assets/fonts/GeistMono-Italic.ttf"),
        ],
        UiFont::Lilex => [
            include_bytes!("../assets/fonts/Lilex-Regular.ttf"),
            include_bytes!("../assets/fonts/Lilex-Medium.ttf"),
            include_bytes!("../assets/fonts/Lilex-SemiBold.ttf"),
            include_bytes!("../assets/fonts/Lilex-Bold.ttf"),
            include_bytes!("../assets/fonts/Lilex-Italic.ttf"),
        ],
        UiFont::ZedMono => [
            include_bytes!("../assets/fonts/ZedMono-Regular.ttf"),
            include_bytes!("../assets/fonts/ZedMono-Medium.ttf"),
            include_bytes!("../assets/fonts/ZedMono-SemiBold.ttf"),
            include_bytes!("../assets/fonts/ZedMono-Bold.ttf"),
            include_bytes!("../assets/fonts/ZedMono-Italic.ttf"),
        ],
        UiFont::JetBrainsMono => [
            include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
            include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
            include_bytes!("../assets/fonts/JetBrainsMono-SemiBold.ttf"),
            include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
            include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf"),
        ],
    }
}

pub fn register_fonts(font: UiFont, cx: &App) {
    FONT.store(UiFont::ALL.iter().position(|f| *f == font).unwrap_or(0) as u8, Ordering::Relaxed);
    let pixel: Cow<'static, [u8]> = Cow::Borrowed(include_bytes!("../assets/fonts/DotGothic16-Regular.ttf"));
    add_families(&[font], Some(pixel), cx);
}

pub fn ensure_fonts(fonts: &[UiFont], cx: &App) {
    add_families(fonts, None, cx);
}

fn add_families(fonts: &[UiFont], extra: Option<Cow<'static, [u8]>>, cx: &App) {
    let mut bytes: Vec<Cow<'static, [u8]>> = extra.into_iter().collect();
    let mut bits = 0u8;
    for (i, f) in UiFont::ALL.iter().enumerate() {
        if fonts.contains(f) && REGISTERED.load(Ordering::Relaxed) & (1 << i) == 0 {
            bits |= 1 << i;
            bytes.extend(faces(*f).into_iter().map(Cow::Borrowed));
        }
    }
    if bytes.is_empty() {
        return;
    }
    match cx.text_system().add_fonts(bytes) {
        Ok(()) => {
            REGISTERED.fetch_or(bits, Ordering::Relaxed);
        }
        Err(e) => log::warn!("could not register the embedded fonts: {e:#}"),
    }
}

pub struct SaveFont(pub Box<dyn Fn(UiFont)>);

impl Global for SaveFont {}

pub fn set_font(font: UiFont, cx: &mut App) {
    ensure_fonts(&[font], cx);
    FONT.store(UiFont::ALL.iter().position(|f| *f == font).unwrap_or(0) as u8, Ordering::Relaxed);
    SYSTEM_FONT.store(std::ptr::null_mut(), Ordering::Relaxed);
    if let Some(save) = cx.try_global::<SaveFont>() {
        (save.0)(font);
    }
    if let Some(save) = cx.try_global::<SaveSystemFont>() {
        (save.0)(None);
    }
    cx.refresh_windows();
}

pub struct SaveSystemFont(pub Box<dyn Fn(Option<String>)>);

impl Global for SaveSystemFont {}

fn leak_font_name(name: &str) -> *mut &'static str {
    Box::into_raw(Box::new(Box::leak(name.to_string().into_boxed_str()) as &'static str))
}

pub fn set_system_font(name: Option<String>, cx: &mut App) {
    let p = name.as_deref().map(leak_font_name).unwrap_or(std::ptr::null_mut());
    SYSTEM_FONT.store(p, Ordering::Relaxed);
    if let Some(save) = cx.try_global::<SaveSystemFont>() {
        (save.0)(name);
    }
    cx.refresh_windows();
}

pub fn activate_system_font(name: &str, cx: &App) {
    if cx.text_system().all_font_names().iter().any(|f| f == name) {
        SYSTEM_FONT.store(leak_font_name(name), Ordering::Relaxed);
    }
}

pub mod metrics {
    pub const TITLE_H: f32 = 40.;
    pub const TAB_H: f32 = 28.;
    pub const TOOLBAR_H: f32 = 40.;
    pub const RESULT_BAR_H: f32 = 34.;
    pub const STRIP_H: f32 = 30.;
    pub const STATUS_H: f32 = 28.;
    pub const RAIL_W: f32 = 36.;
    pub const EXPLORER_W: f32 = 260. + RAIL_W;
    pub const EXPLORER_MIN: f32 = 200. + RAIL_W;
    pub const EXPLORER_MAX: f32 = 480. + RAIL_W;
    pub const EXPLORER_COMPACT: f32 = 220. + RAIL_W;
    pub const SIDE_W: f32 = 360.;
    pub const SIDE_MIN: f32 = 280.;
    pub const SIDE_MAX: f32 = 520.;
    pub const EDITOR_H: f32 = 300.;
    pub const EDITOR_COMPACT: f32 = 240.;
    pub const EDITOR_MIN: f32 = 80.;
    pub const GRID_MIN: f32 = 120.;
    pub const DIVIDER: f32 = 7.;
    pub const DIVIDER_HIT: f32 = 11.;
    pub const GRID_ROW: f32 = 22.;
    pub const EDITOR_LINE: f32 = 20.;
    pub const MIN_WINDOW: (f32, f32) = (960., 600.);

    pub const LIGHTS_AT: (f32, f32) = (16., 14.);
    pub const LIGHT_D: f32 = 12.;
    pub const LIGHT_STEP: f32 = 20.;
    /// AppKit's standard window button is a 14 x 16 frame with its 12 pt circle centred in it
    /// (1 pt in from the left, 2 pt down from the top; measured on macOS 15), and GPUI's
    /// `traffic_light_position` places that frame's origin: it sizes the title bar container
    /// to the button's height plus twice the y, so the y is the frame's distance from the
    /// window's top whichever way the container is flipped.
    pub const LIGHT_FRAME: (f32, f32) = (14., 16.);

    pub fn traffic_light_position() -> (f32, f32) {
        (LIGHTS_AT.0 - (LIGHT_FRAME.0 - LIGHT_D) / 2., LIGHTS_AT.1 - (LIGHT_FRAME.1 - LIGHT_D) / 2.)
    }

    /// The same at a UI zoom: AppKit's lights keep their size and x; their centre stays on the
    /// zoomed title bar's middle line.
    pub fn traffic_light_position_at(zoom: f32) -> (f32, f32) {
        let (x, _) = traffic_light_position();
        (x, (TITLE_H * zoom - LIGHT_FRAME.1) / 2.)
    }

    pub fn lights_right_at(zoom: f32) -> f32 {
        LIGHTS_RIGHT / zoom
    }

    pub const LIGHTS_RIGHT: f32 = LIGHTS_AT.0 + 2. * LIGHT_STEP + LIGHT_D;

    #[cfg(test)]
    mod tests {
        use super::*;

        fn light_centre(i: usize) -> (f32, f32) {
            let (x, y) = traffic_light_position();
            (x + i as f32 * LIGHT_STEP + LIGHT_FRAME.0 / 2., y + LIGHT_FRAME.1 / 2.)
        }

        #[test]
        fn the_title_bar_centres_on_the_traffic_lights() {
            assert_eq!(traffic_light_position(), (15., 12.));
            let mid = TITLE_H / 2.;
            for i in 0..3 {
                assert_eq!(light_centre(i).1, mid);
            }
            assert_eq!(light_centre(0).0, LIGHTS_AT.0 + LIGHT_D / 2.);
            assert_eq!(LIGHTS_RIGHT, 68.);
            assert_eq!((TITLE_H - TAB_H) / 2. + TAB_H / 2., mid);
            assert_eq!((TITLE_H - 26.) / 2. + 13., mid);
            assert_eq!(traffic_light_position_at(1.), traffic_light_position());
            for z in [0.8, 1.25, 2.] {
                let (_, y) = traffic_light_position_at(z);
                assert!((y + LIGHT_FRAME.1 / 2. - TITLE_H * z / 2.).abs() < 1e-4);
            }
        }
    }
}

pub mod text {
    pub const CODE: f32 = 13.;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

/// Stored in the profile by name: do not rename.
pub const SWATCHES: [&str; 8] = ["rose", "coral", "lime", "mint", "sky", "cobalt", "sand", "slate"];

#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: Mode,
    pub glass: Hsla,
    pub bg: Hsla,
    /// Painted ONCE: GPUI blends color `src·a + dst·(1−a)` but adds alpha (`a + dst_a`), so a
    /// second coat would darken the overlap. Dark: `bg` at 7 % over the 0.86 glass lands at alpha
    /// 0.93 (more would reach alpha 1 and hide the blur). Light: solid white, since with this
    /// blending any translucent white panel ends up darker than the glass over a light desktop.
    pub panel: Hsla,
    pub card: Hsla,
    /// Opaque: our GPUI has no backdrop blur, and translucent without it the text under them
    /// would show through.
    pub pop: Hsla,
    pub field: Hsla,
    /// Opaque, so it holds over any desktop (a translucent white outline on the dark glass
    /// cancels out over a white one).
    pub field_b: Hsla,
    pub hov: Hsla,
    pub prs: Hsla,
    pub cur_line: Hsla,
    // Lines. Opaque: a translucent hairline over the glass or the translucent surface would let
    // the desktop through and lose its contrast.
    pub line: Hsla,
    pub line2: Hsla,
    pub grid_line: Hsla,
    /// Translucent on purpose: GPUI adds alpha, so an opaque color on the antialiased pixels of a
    /// curve saturates them to alpha 1 — a dark fringe over a light desktop. Kept at ≤ 0.2 alpha.
    pub edge: Hsla,
    pub edge_acc: Hsla,
    pub tx: Hsla,
    pub tx2: Hsla,
    pub tx3: Hsla,
    pub tx4: Hsla,
    pub acc: Hsla,
    pub acc_s: Hsla,
    pub acc_t: Hsla,
    pub acc_h: Hsla,
    pub acc_p: Hsla,
    pub on_acc: Hsla,
    pub lar: Hsla,
    pub lar_s: Hsla,
    pub lar_r: Hsla,
    pub lar_t: Hsla,
    pub lar_h: Hsla,
    pub on_lar: Hsla,
    pub ok: Hsla,
    pub ok_s: Hsla,
    pub warn: Hsla,
    pub warn_s: Hsla,
    pub err: Hsla,
    pub err_s: Hsla,
    pub err_l: Hsla,
    pub err_line: Hsla,
    pub on_err: Hsla,
    pub info: Hsla,
    pub info_s: Hsla,
    pub null: Hsla,
    pub null_b: Hsla,
    pub sel: Hsla,
    pub sel_hi: Hsla,
    pub find: Hsla,
    pub find_tx: Hsla,
    pub find_s: Hsla,
    pub mark: Hsla,
    pub mark_b: Hsla,
    pub focus: Hsla,
    pub focus_s: Hsla,
    pub alt: Hsla,
    /// GPUI adds a wash's alpha, so a lone white stripe also hides that much more of the desktop
    /// and, over a white one, the two cancel: every row gets the same alpha (dark here, light on
    /// `alt`).
    pub alt_base: Hsla,
    pub key: Hsla,
    pub kw: Hsla,
    pub func: Hsla,
    pub str: Hsla,
    pub num: Hsla,
    pub typ: Hsla,
    pub cm: Hsla,
    pub op: Hsla,
    pub lkw: Hsla,
    pub param: Hsla,
    pub kw_s: Hsla,
    pub func_s: Hsla,
    pub typ_s: Hsla,
    pub view_s: Hsla,
    pub scrim: Hsla,
    pub swatches: [Hsla; 8],
}

pub const GLASS_ALPHA: f32 = 0.86;
pub const PANEL_ALPHA: f32 = 0.07;
pub const CARD_ALPHA: f32 = 0.40;
const ALT_GLASS: (f32, f32) = (0.022, 0.032);
const ALT_OPAQUE: (f32, f32) = (0.016, 0.02);

fn c(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
}

fn ca(hex: u32, a: f32) -> Hsla {
    let mut x: Rgba = gpui::rgb(hex);
    x.a = a;
    x.into()
}

pub fn mix(a: Hsla, b: Hsla, k: f32) -> Hsla {
    let (a, b): (Rgba, Rgba) = (a.into(), b.into());
    let k = k.clamp(0., 1.);
    let l = |x: f32, y: f32| x + (y - x) * k;
    Rgba { r: l(a.r, b.r), g: l(a.g, b.g), b: l(a.b, b.b), a: l(a.a, b.a) }.into()
}

impl Theme {
    pub fn dark() -> Theme {
        const W: u32 = 0xffffff;
        Theme {
            mode: Mode::Dark,
            glass: ca(0x0d0d0d, GLASS_ALPHA),
            bg: c(0x060606),
            panel: ca(0x060606, PANEL_ALPHA),
            card: ca(0x0d0d0d, CARD_ALPHA),
            pop: c(0x161616),
            field: c(0x0e0e0e),
            field_b: c(0x5e5e5e),
            hov: ca(W, 0.045),
            prs: ca(W, 0.08),
            cur_line: ca(W, 0.035),
            line: c(0x262626),
            line2: c(0x363636),
            grid_line: c(0x1d1d1d),
            edge: ca(W, 0.14),
            edge_acc: ca(0x4c7dff, 0.2),
            tx: c(0xe8eaf4),
            tx2: c(0xa7acc2),
            tx3: c(0x8a8fa7),
            tx4: c(0x4b5066),
            acc: c(0x4c7dff),
            acc_s: ca(0x4c7dff, 0.16),
            acc_t: c(0xa3baff),
            acc_h: c(0x5f8bff),
            acc_p: c(0x3d6ae6),
            on_acc: c(0xffffff),
            lar: c(0xa47bff),
            lar_s: ca(0xa47bff, 0.15),
            lar_r: ca(0xa47bff, 0.35),
            lar_t: c(0xcdb7ff),
            lar_h: c(0xb28eff),
            on_lar: c(0x0b0716),
            ok: c(0x5be49b),
            ok_s: ca(0x5be49b, 0.12),
            warn: c(0xff9f43),
            warn_s: ca(0xff9f43, 0.13),
            err: c(0xff5d73),
            err_s: ca(0xff5d73, 0.13),
            err_l: ca(0xff5d73, 0.16),
            err_line: ca(0xff5d73, 0.10),
            on_err: c(0x1a0509),
            info: c(0x56d4f5),
            info_s: ca(0x56d4f5, 0.12),
            null: c(0x62677f),
            null_b: ca(W, 0.025),
            sel: ca(0x4c7dff, 0.20),
            sel_hi: ca(0x4c7dff, 0.42),
            find: c(0xf5d33f),
            find_tx: c(0x1c1600),
            find_s: ca(0xf5d33f, 0.16),
            mark: ca(W, 0.12),
            mark_b: c(0x8c8c8c),
            focus: c(0x6e6e6e),
            focus_s: ca(W, 0.05),
            alt: ca(W, ALT_GLASS.0),
            alt_base: ca(0x000000, ALT_GLASS.0),
            key: ca(W, 0.06),
            kw: c(0x7aa2ff),
            func: c(0xff8fd0),
            str: c(0x9be37b),
            num: c(0xffb86b),
            typ: c(0x56d4f5),
            cm: c(0x666c8a),
            op: c(0xa7acc2),
            lkw: c(0xb892ff),
            param: c(0xffb86b),
            kw_s: ca(0x7aa2ff, 0.16),
            func_s: ca(0xff8fd0, 0.14),
            typ_s: ca(0x56d4f5, 0.14),
            view_s: ca(0x5be49b, 0.14),
            scrim: ca(0x04050a, 0.58),
            swatches: [
                c(0xf06ccf),
                c(0xff8a5b),
                c(0xc6f05a),
                c(0x5be49b),
                c(0x56d4f5),
                c(0x4c7dff),
                c(0xe2c99a),
                c(0xa7acc2),
            ],
        }
    }

    pub fn light() -> Theme {
        const K: u32 = 0x0c1020;
        Theme {
            mode: Mode::Light,
            glass: ca(0xf5f5f5, GLASS_ALPHA),
            bg: c(0xffffff),
            panel: c(0xffffff),
            card: ca(0xf5f5f5, CARD_ALPHA),
            pop: c(0xffffff),
            field: c(0xffffff),
            field_b: c(0xadadad),
            hov: ca(K, 0.045),
            prs: ca(K, 0.08),
            cur_line: ca(K, 0.035),
            line: c(0xe0e0e0),
            line2: c(0xcccccc),
            grid_line: c(0xebebeb),
            edge: ca(K, 0.14),
            edge_acc: ca(0x2f5bff, 0.2),
            tx: c(0x11131b),
            tx2: c(0x4a5068),
            tx3: c(0x585e76),
            tx4: c(0xa5aabd),
            acc: c(0x2f5bff),
            acc_s: ca(0x2f5bff, 0.11),
            acc_t: c(0x2346d6),
            acc_h: c(0x4570ff),
            acc_p: c(0x2449e0),
            on_acc: c(0xffffff),
            lar: c(0x7a4dff),
            lar_s: ca(0x7a4dff, 0.10),
            lar_r: ca(0x7a4dff, 0.30),
            lar_t: c(0x5b30dd),
            lar_h: c(0x8a62ff),
            on_lar: c(0xffffff),
            ok: c(0x0e8f50),
            ok_s: ca(0x0e8f50, 0.10),
            warn: c(0xb45a00),
            warn_s: ca(0xb45a00, 0.10),
            err: c(0xd4213f),
            err_s: ca(0xd4213f, 0.09),
            err_l: ca(0xd4213f, 0.10),
            err_line: ca(0xd4213f, 0.06),
            on_err: c(0xffffff),
            info: c(0x087fa3),
            info_s: ca(0x087fa3, 0.09),
            null: c(0x9ea3b6),
            null_b: ca(K, 0.03),
            sel: ca(0x2f5bff, 0.12),
            sel_hi: ca(0x2f5bff, 0.26),
            find: c(0xffe14a),
            find_tx: c(0x1c1600),
            find_s: ca(0xf2c200, 0.24),
            mark: ca(0x000000, 0.08),
            mark_b: c(0x7c7c7c),
            focus: c(0x8a8a8a),
            focus_s: ca(0x000000, 0.04),
            alt: ca(K, ALT_GLASS.1),
            alt_base: gpui::transparent_black(),
            key: ca(K, 0.04),
            kw: c(0x2f5bff),
            func: c(0xb0287a),
            str: c(0x2c7a2e),
            num: c(0xb05a00),
            typ: c(0x087fa3),
            cm: c(0x8e94a8),
            op: c(0x4a5068),
            lkw: c(0x6b3cf0),
            param: c(0xb05a00),
            kw_s: ca(0x2f5bff, 0.10),
            func_s: ca(0xb0287a, 0.10),
            typ_s: ca(0x087fa3, 0.10),
            view_s: ca(0x0e8f50, 0.10),
            scrim: ca(0xdce0ec, 0.55),
            swatches: [
                c(0xd64aab),
                c(0xe2662f),
                c(0x7fa315),
                c(0x14a866),
                c(0x1497c2),
                c(0x2f5bff),
                c(0xb08f55),
                c(0x8e94a8),
            ],
        }
    }

    pub fn opaque(mut self) -> Theme {
        self.glass = self.glass.alpha(1.);
        self.panel = self.panel.alpha(1.);
        self.card = self.card.alpha(1.);
        self.alt = self.alt.alpha(if self.is_dark() { ALT_OPAQUE.0 } else { ALT_OPAQUE.1 });
        self.alt_base = gpui::transparent_black();
        self
    }

    pub fn at(self, level: Translucency) -> Theme {
        if level.is_on() { self } else { self.opaque() }
    }

    pub fn is_dark(&self) -> bool {
        self.mode == Mode::Dark
    }

    pub fn profile_color(&self, p: &dataman_core::ConnectionProfile) -> Hsla {
        if p.env.is_production() { self.err } else { self.conn_color(p.color.as_deref(), &p.id) }
    }

    pub fn conn_color(&self, swatch: Option<&str>, id: &str) -> Hsla {
        let ix = swatch
            .and_then(|s| SWATCHES.iter().position(|w| *w == s))
            .unwrap_or_else(|| id.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)) % 5);
        self.swatches[ix]
    }

    pub fn ring(&self) -> Vec<BoxShadow> {
        // A hairline like the borders: 1 pt at any zoom.
        vec![BoxShadow { color: self.line2, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: gpui::px(1.), inset: false }]
    }

    /// GPUI paints a shadow under the whole box, so a translucent field takes `ui::halo_ring`.
    pub fn halo(&self) -> Vec<BoxShadow> {
        vec![BoxShadow { color: self.acc_s, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(3.), inset: false }]
    }
}

pub struct WindowGlass {
    pub level: Translucency,
}

impl Global for WindowGlass {}

pub struct SavePref(pub Box<dyn Fn(ThemePref)>);

impl Global for SavePref {}

pub struct ThemeState {
    pub pref: ThemePref,
    pub theme: Arc<Theme>,
}

impl Global for ThemeState {}

pub trait ActiveTheme {
    fn theme(&self) -> Arc<Theme>;
}

impl ActiveTheme for App {
    fn theme(&self) -> Arc<Theme> {
        match self.try_global::<ThemeState>() {
            Some(s) => s.theme.clone(),
            None => Arc::new(Theme::dark()),
        }
    }
}

fn resolve(pref: ThemePref, appearance: WindowAppearance) -> Mode {
    match pref {
        ThemePref::Dark => Mode::Dark,
        ThemePref::Light => Mode::Light,
        ThemePref::System => match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Mode::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Mode::Dark,
        },
    }
}

pub fn init(pref: ThemePref, cx: &mut App) {
    let mode = resolve(pref, cx.window_appearance());
    let theme = Arc::new(theme_for(mode, cx));
    cx.set_global(ThemeState { pref, theme });
    apply_window_appearance(pref, mode, cx);
    crate::dock_icon::follow(mode == Mode::Light);
}

fn theme_for(mode: Mode, cx: &App) -> Theme {
    let t = match mode {
        Mode::Dark => Theme::dark(),
        Mode::Light => Theme::light(),
    };
    t.at(glass_level(cx))
}

pub fn glass_level(cx: &App) -> Translucency {
    cx.try_global::<WindowGlass>().map_or(Translucency::On, |g| g.level)
}

pub fn effective_level(level: Translucency) -> Translucency {
    if std::env::var_os("DATAMAN_OPAQUE").is_some() { Translucency::Off } else { level }
}

/// Where GPUI's blur does not work, transparent over Dataman's own blur view (`glass`).
pub fn background_for(level: Translucency) -> WindowBackgroundAppearance {
    crate::glass::background_for(level)
}

pub fn set_translucency(level: Translucency, window: &mut Window, cx: &mut App) {
    if let Some(save) = cx.try_global::<SaveTranslucency>() {
        (save.0)(level);
    }
    let level = effective_level(level);
    crate::glass::apply(level, window);
    cx.set_global(WindowGlass { level });
    if let Some(state) = cx.try_global::<ThemeState>() {
        let (pref, mode) = (state.pref, state.theme.mode);
        let theme = Arc::new(theme_for(mode, cx));
        cx.set_global(ThemeState { pref, theme });
    }
    cx.refresh_windows();
}

pub struct SaveTranslucency(pub Box<dyn Fn(Translucency)>);

impl Global for SaveTranslucency {}

fn apply_window_appearance(pref: ThemePref, mode: Mode, cx: &App) {
    let forced = match (pref, mode) {
        (ThemePref::System, _) => None,
        (_, Mode::Dark) => Some(WindowAppearance::Dark),
        (_, Mode::Light) => Some(WindowAppearance::Light),
    };
    cx.set_window_appearance(forced);
}

pub fn set_pref(pref: ThemePref, cx: &mut App) {
    let mode = resolve(pref, cx.window_appearance());
    let theme = Arc::new(theme_for(mode, cx));
    cx.set_global(ThemeState { pref, theme });
    apply_window_appearance(pref, mode, cx);
    crate::dock_icon::follow(mode == Mode::Light);
    if let Some(save) = cx.try_global::<SavePref>() {
        (save.0)(pref);
    }
    cx.refresh_windows();
}

pub fn next_pref(pref: ThemePref) -> ThemePref {
    match pref {
        ThemePref::Dark => ThemePref::Light,
        ThemePref::Light => ThemePref::System,
        ThemePref::System => ThemePref::Dark,
    }
}

pub fn on_system_appearance(window: &Window, cx: &mut App) {
    let Some(state) = cx.try_global::<ThemeState>() else { return };
    if state.pref != ThemePref::System {
        return;
    }
    let mode = resolve(ThemePref::System, window.appearance());
    if state.theme.mode != mode {
        let theme = Arc::new(theme_for(mode, cx));
        cx.set_global(ThemeState { pref: ThemePref::System, theme });
        crate::dock_icon::follow(mode == Mode::Light);
        cx.refresh_windows();
    }
}

pub fn pref(cx: &App) -> ThemePref {
    cx.try_global::<ThemeState>().map(|s| s.pref).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(c: Hsla) -> [f32; 3] {
        let x: Rgba = c.into();
        [x.r, x.g, x.b]
    }

    fn over(fg: Hsla, bg: [f32; 3]) -> [f32; 3] {
        let (f, a) = (rgb(fg), fg.a);
        [0, 1, 2].map(|i| f[i] * a + bg[i] * (1. - a))
    }

    fn contrast(a: [f32; 3], b: [f32; 3]) -> f32 {
        let lin = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        let l = |c: [f32; 3]| 0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2]);
        let (la, lb) = (l(a), l(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn swatches_stay_clear_of_production_red() {
        for t in [Theme::dark(), Theme::light()] {
            for (name, s) in SWATCHES.iter().zip(t.swatches) {
                if s.s < 0.3 {
                    continue;
                }
                let d = (s.h - t.err.h).abs() * 360.;
                let d = d.min(360. - d);
                assert!(d >= 20., "{name} is {d:.0}° from err ({:?})", t.mode);
            }
            let rose = t.swatches[0];
            assert!(rose.s <= t.err.s + 0.05 && (rose.l - t.err.l).abs() < 0.1, "rose at err's weight ({:?})", t.mode);
        }
    }

    fn gpui_stack(layers: &[Hsla]) -> ([f32; 3], f32) {
        let (mut c, mut a) = ([0f32; 3], 0f32);
        for l in layers {
            let s = rgb(*l);
            c = [0, 1, 2].map(|i| s[i] * l.a + c[i] * (1. - l.a));
            a = (a + l.a).min(1.);
        }
        (c, a)
    }

    #[test]
    fn muted_text_reads_on_the_translucent_surface() {
        for (t, adverse) in [(Theme::dark(), [1.; 3]), (Theme::light(), [0.; 3])] {
            let (stack, alpha) = gpui_stack(&[t.glass, t.panel]);
            let surface = [0, 1, 2].map(|i| stack[i] + adverse[i] * (1. - alpha));
            let card = over(t.card, stack);
            match t.mode {
                Mode::Dark => assert!((alpha - (GLASS_ALPHA + PANEL_ALPHA)).abs() < 1e-4, "dark surface alpha {alpha}"),
                Mode::Light => assert_eq!(alpha, 1., "the light surface is solid white"),
            }
            for (name, bg) in [("surface", surface), ("card", card), ("opaque surface", rgb(t.bg))] {
                let r = contrast(rgb(t.tx3), bg);
                assert!(r >= 4.5, "{:?} tx3 on the {name}: {r:.2}:1", t.mode);
            }
            assert!(contrast(rgb(t.tx), surface) >= 7., "{:?} tx on the surface", t.mode);
            let o = t.clone().opaque();
            assert_eq!((o.glass.a, o.panel.a, o.card.a), (1., 1., 1.));
        }
    }

    #[test]
    fn find_yellow_reads_and_stands_out() {
        for t in [Theme::dark(), Theme::light()] {
            assert_eq!(t.find.a, 1., "{:?} find is opaque (it hides the text under it)", t.mode);
            let r = contrast(rgb(t.find_tx), rgb(t.find));
            assert!(r >= 10., "{:?} find_tx on find: {r:.2}:1", t.mode);
            let surface = if t.is_dark() { rgb(t.bg) } else { [1.; 3] };
            let lift = contrast(rgb(t.find), surface);
            assert!(lift >= if t.is_dark() { 10. } else { 1.2 }, "{:?} find on the surface: {lift:.2}:1", t.mode);
            assert!(t.sel_hi.a > t.sel.a && rgb(t.sel_hi) == rgb(t.sel), "{:?} sel_hi is a stronger sel", t.mode);
        }
    }

    #[test]
    fn zoom_steps_like_a_browser() {
        assert_eq!(zoom_step(1.0, 1), 1.1);
        assert_eq!(zoom_step(1.1, 1), 1.25);
        assert_eq!(zoom_step(1.0, -1), 0.9);
        assert_eq!(zoom_step(0.9, -1), 0.8);
        assert_eq!(zoom_step(2.0, 1), 2.0);
        assert_eq!(zoom_step(0.8, -1), 0.8);
        assert_eq!(zoom_step(1.6, 0), 1.0);
        assert_eq!((zoom_step(1.3, 1), zoom_step(1.3, -1)), (1.5, 1.25));
        let mut z = 0.8;
        let mut seen = vec![z];
        while zoom_step(z, 1) != z {
            z = zoom_step(z, 1);
            seen.push(z);
        }
        assert_eq!(seen, ZOOM_STEPS);
    }

    #[test]
    fn mix_goes_from_one_color_to_the_other() {
        let t = Theme::dark();
        let m0: Rgba = mix(t.sel, t.sel_hi, 0.).into();
        let m1: Rgba = mix(t.sel, t.sel_hi, 1.).into();
        let half: Rgba = mix(t.sel, t.sel_hi, 0.5).into();
        let (a, b): (Rgba, Rgba) = (t.sel.into(), t.sel_hi.into());
        assert!((m0.a - a.a).abs() < 1e-4 && (m1.a - b.a).abs() < 1e-4);
        assert!((half.a - (a.a + b.a) / 2.).abs() < 1e-4);
    }

    #[test]
    fn zebra_rows_read_over_any_desktop() {
        for t in [Theme::dark(), Theme::light(), Theme::dark().opaque(), Theme::light().opaque()] {
            for desk in [0f32, 1.] {
                let show = |row: Hsla| {
                    let (c, a) = gpui_stack(&[t.glass, t.panel, row]);
                    (c[1] + desk * (1. - a)) * 255.
                };
                let d = (show(t.alt) - show(t.alt_base)).abs();
                assert!((3.5..=9.).contains(&d), "{:?} (opaque {}) over desk {desk}: stripes {d:.1} levels apart", t.mode, t.glass.a == 1.);
            }
        }
    }

    #[test]
    fn translucency_and_opaque_separators() {
        assert_eq!((GLASS_ALPHA, PANEL_ALPHA), (0.86, 0.07));
        for t in [Theme::dark(), Theme::light()] {
            let on = t.clone().at(Translucency::On);
            assert_eq!((on.glass.a, on.panel.a, on.alt.a), (t.glass.a, t.panel.a, t.alt.a), "{:?} on is the default", t.mode);
            let (_, alpha) = gpui_stack(&[on.glass, on.panel]);
            let want = if t.mode == Mode::Dark { 0.93 } else { 1. };
            assert!((alpha - want).abs() < 1e-4, "{:?}: surface alpha {alpha}", t.mode);
            let off = t.clone().at(Translucency::Off);
            assert_eq!((off.glass.a, off.panel.a, off.card.a), (1., 1., 1.));
            assert!(on.alt.a > off.alt.a, "{:?} the zebra rows read more on the glass", t.mode);
            assert_eq!((t.line.a, t.line2.a, t.grid_line.a, t.field_b.a), (1., 1., 1., 1.), "{:?} separators are opaque", t.mode);
        }
    }
}
